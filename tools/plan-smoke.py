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
import subprocess
import sys
import fcntl
import termios
import time

HERE = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.dirname(HERE)


def run(binary, home, keys, rows=24, cols=130, timeout=25):
    """Drive the TUI, returning what it printed on stdout."""
    main, worker = pty.openpty()
    fcntl.ioctl(worker, termios.TIOCSWINSZ, struct.pack("HHHH", rows, cols, 0, 0))
    env = dict(os.environ, HOME=home)
    env.pop("MNEMOSYNE_NO_SPLASH", None)
    proc = subprocess.Popen(
        [binary, "--no-splash", "--no-update"],
        stdin=worker, stderr=worker, stdout=subprocess.PIPE,
        close_fds=True, env=env,
    )
    os.close(worker)
    time.sleep(1.2)
    for k in keys:
        os.write(main, k.encode())
        time.sleep(0.4)
    try:
        out, _ = proc.communicate(timeout=timeout)
    except subprocess.TimeoutExpired:
        proc.kill()
        out, _ = proc.communicate()
        os.close(main)
        raise SystemExit("the interface never exited")
    os.close(main)
    return out.decode(errors="replace")


def main():
    binary = sys.argv[1] if len(sys.argv) > 1 else os.path.join(ROOT, "target/release/mnemosyne")
    if not os.path.exists(binary):
        raise SystemExit(f"no binary at {binary} — cargo build --release first")

    home = "/tmp/mnemosyne-plan-smoke"
    subprocess.run([sys.executable, os.path.join(HERE, "demo-corpus.py"), home],
                   check=True, stdout=subprocess.DEVNULL)

    failures = []

    def check(what, ok, detail=""):
        print(f"  {'ok  ' if ok else 'FAIL'} {what}")
        if not ok:
            failures.append(f"{what}: {detail}")

    # enter resumes the session under the cursor, here in this terminal
    plan = run(binary, home, ["\r"])
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
    plan = run(binary, home, ["\x14", "eft work!", "\r"])
    check("naming a tmux session keeps stdout clean", "\x1b" not in plan, repr(plan[:120]))
    fields = plan.rstrip("\n").split("\t")
    check("the mode is tmux", fields[0] == "tmux" if fields else False, repr(fields[:1]))
    check("the chosen name comes through, made safe for tmux",
          fields[-1] == "eft-work" if fields else False, repr(fields[-1:]))

    # an empty answer means the generated name
    plan = run(binary, home, ["\x14", "\r"])
    fields = plan.rstrip("\n").split("\t")
    check("an empty name is left empty for the shell to fill in",
          fields[-1] == "" if fields else False, repr(fields[-1:]))

    # quitting says nothing at all
    plan = run(binary, home, ["q"])
    check("quitting prints nothing", plan.strip() == "", repr(plan[:120]))

    print()
    if failures:
        print(f"{len(failures)} failed")
        for f in failures:
            print(f"  {f}")
        return 1
    print("plan output is clean")
    return 0


if __name__ == "__main__":
    sys.exit(main())
