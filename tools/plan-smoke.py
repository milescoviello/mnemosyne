#!/usr/bin/env python3
"""Check that the plan printed on stdout is clean enough for a shell to parse.

    python3 tools/plan-smoke.py [path-to-binary]

The whole design rests on one split: the interface is drawn on stderr and the
chosen session is printed on stdout, so a shell function can capture the
decision with a command substitution while the TUI still owns the terminal.
Anything that writes to stdout by accident breaks that, and it breaks it
invisibly -- the picker looks perfect and the resume fails with a nonsense
error.

That is not hypothetical. Asking the terminal whether it supports the kitty
keyboard protocol does its probe on *stdout*, so the plan came out as
`^[[?u^[[ctmux<tab>/home/...` and the shell tried to resume a session called
`^[[?u^[[ctmux`. No Rust test could see it: it only happens against a real
terminal.

So this runs the real binary on a real pty with a corpus of invented
sessions, drives it, and checks what lands on stdout.
"""

import os
import pty
import struct
import select
import subprocess
import sys
import fcntl
import termios
import threading
import time

HERE = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.dirname(HERE)


def run(binary, home, keys, rows=24, cols=130, timeout=40, extra_env=None):
    """Drive the TUI, returning (stdout, what it drew, exit code).

    Waiting a fixed second and hoping was enough on a laptop and was not on
    a CI runner, where the first index build is slower: the keys arrived
    before anything could read them, the plan came back empty, and there was
    no clue as to why. So this waits for the interface to actually appear,
    and keeps what it drew so a failure can say what happened.

    The pty is drained by a thread throughout. Reading only between
    keystrokes deadlocks as soon as the interface draws more than a pipe
    buffer's worth: it blocks writing, so it never exits, so nothing reads.
    """
    main_fd, worker = pty.openpty()
    fcntl.ioctl(worker, termios.TIOCSWINSZ, struct.pack("HHHH", rows, cols, 0, 0))
    env = dict(os.environ, HOME=home, TERM="xterm-256color")
    if extra_env:
        env.update(extra_env)
    proc = subprocess.Popen(
        [binary, "--no-splash", "--no-update"],
        stdin=worker, stderr=worker, stdout=subprocess.PIPE,
        close_fds=True, env=env,
    )
    os.close(worker)

    drew = bytearray()
    done = threading.Event()

    def drain():
        while not done.is_set():
            r, _, _ = select.select([main_fd], [], [], 0.1)
            if not r:
                continue
            try:
                chunk = os.read(main_fd, 65536)
            except OSError:
                return
            if not chunk:
                return
            drew.extend(chunk)

    reader = threading.Thread(target=drain, daemon=True)
    reader.start()

    # Wait for a full frame. The wordmark is no use as the signal: every
    # letter is its own colour span, so the bytes "mnemosyne" never appear
    # together. The column headings and the footer are drawn whole.
    def drawn():
        raw = bytes(drew)
        return b"FOLDER" in raw or b"resume" in raw

    deadline = time.time() + timeout
    while time.time() < deadline and not drawn():
        time.sleep(0.1)
    appeared = drawn()

    for k in keys:
        os.write(main_fd, k.encode())
        time.sleep(0.5)

    try:
        out, _ = proc.communicate(timeout=timeout)
        code = proc.returncode
    except subprocess.TimeoutExpired:
        proc.kill()
        out, _ = proc.communicate()
        code = -1
    done.set()
    reader.join(timeout=2)
    os.close(main_fd)

    screen = bytes(drew).decode(errors="replace")
    if not appeared:
        screen = "[the interface never drew a frame]\n" + screen
    return out.decode(errors="replace"), screen, code


def main():
    binary = sys.argv[1] if len(sys.argv) > 1 else os.path.join(ROOT, "target/release/mnemosyne")
    if not os.path.exists(binary):
        raise SystemExit(f"no binary at {binary} — cargo build --release first")

    home = "/tmp/mnemosyne-plan-smoke"
    subprocess.run([sys.executable, os.path.join(HERE, "demo-corpus.py"), home],
                   check=True, stdout=subprocess.DEVNULL)

    failures = []
    context = {"screen": "", "code": 0}

    def check(what, ok, detail=""):
        print(f"  {'ok  ' if ok else 'FAIL'} {what}")
        if not ok:
            failures.append(f"{what}: {detail}")

    def drive(keys):
        plan, screen, code = run(binary, home, keys)
        context["screen"], context["code"] = screen, code
        check(f"it exited cleanly after {keys!r}", code == 0, f"exit {code}")
        return plan

    # enter resumes the session under the cursor, here in this terminal
    plan = drive(["\r"])
    check("stdout carries no escape sequences", "\x1b" not in plan, repr(plan[:120]))
    check("one line for one session", len(plan.rstrip("\n").splitlines()) == 1, repr(plan))
    fields = plan.rstrip("\n").split("\t")
    check("seven tab-separated fields", len(fields) == 7, f"{len(fields)}: {fields}")
    check("it starts with a mode the shell knows",
          fields[0] in ("here", "window", "tmux", "wintmux"), fields[0] if fields else "")
    check("the folder is a real path", fields[1].startswith("/") if len(fields) > 1 else False,
          fields[1] if len(fields) > 1 else "")
    check("the session id looks like one",
          len(fields[2]) == 36 if len(fields) > 2 else False,
          fields[2] if len(fields) > 2 else "")

    # ctrl+t asks what to call the tmux session; the answer is the last field
    plan = drive(["\x14", "eft work!", "\r"])
    check("naming a tmux session keeps stdout clean", "\x1b" not in plan, repr(plan[:120]))
    fields = plan.rstrip("\n").split("\t")
    check("the mode is tmux", fields[0] == "tmux" if fields else False, repr(fields[:1]))
    check("the chosen name comes through, made safe for tmux",
          fields[-1] == "eft-work" if fields else False, repr(fields[-1:]))

    # an empty answer means the generated name
    plan = drive(["\x14", "\r"])
    fields = plan.rstrip("\n").split("\t")
    check("an empty name is left empty for the shell to fill in",
          fields[-1] == "" if fields else False, repr(fields[-1:]))

    # quitting says nothing at all
    plan = drive(["q"])
    check("quitting prints nothing", plan.strip() == "", repr(plan[:120]))

    # a panic must hand the terminal back. Without it you are left in raw
    # mode, on the alternate screen, with the keyboard in a protocol your
    # shell does not speak -- and the message saying so painted on a screen
    # you can no longer see.
    _, screen, code = run(binary, home, [], timeout=30,
                          extra_env={"MNEMOSYNE_PANIC_TEST": "1"})
    check("a panic exits, rather than hanging", code == 101, f"exit {code}")
    for what, seq in (("leaves the alternate screen", "\x1b[?1049l"),
                      ("pops the keyboard protocol", "\x1b[<1u"),
                      ("turns mouse reporting off", "\x1b[?1006l")):
        check(f"a panic {what}", seq in screen, "not found in the output")
    check("a panic still says what happened", "deliberate panic" in screen,
          repr(screen[-200:]))

    print()
    if failures:
        print(f"{len(failures)} failed")
        for f in failures:
            print(f"  {f}")
        # what it actually drew, so a failure on a machine you cannot see
        # says something more useful than "empty"
        tail = context["screen"][-1500:]
        print("\n--- last thing the interface drew ---")
        print(tail)
        return 1
    print("plan output is clean")
    return 0


if __name__ == "__main__":
    sys.exit(main())
