#!/usr/bin/env python3
"""Drive the TUI in a pseudo-terminal and reconstruct what it drew.

    python3 tools/tui-drive.py '["./target/release/mnemosyne","--no-splash"]' '["DOWN","v"]' 30 140

Keys are names (DOWN, ENTER, ESC, CTRLT, ...), raw text, or synthetic mouse
events: CLICK:col,row  RCLICK:col,row  WHEELUP  WHEELDOWN  CLICKFIND:<glyph>.

CLICKFIND locates a glyph on the screen as it stands *right now* and clicks it.
Rendering once to find coordinates and clicking in a second run races against a
transcript corpus that is being written to continuously -- the list reorders
between runs, and the click lands on the wrong row.

Includes just enough terminal emulation (cursor positioning, erase, SGR) to
rebuild the character grid. Escape sequences can be split across pty reads, so
an incomplete tail is carried over rather than printed as text.
"""
import os, pty, subprocess, time, select, sys, fcntl, termios, struct, re, json

NAMED = {
    "DOWN": "\x1b[B", "UP": "\x1b[A", "RIGHT": "\x1b[C", "LEFT": "\x1b[D",
    "ENTER": "\r", "ESC": "\x1b", "TAB": "\t", "BS": "\x7f", "SPACE": " ",
    "CTRLN": "\x0e", "CTRLT": "\x14", "CTRLU": "\x15", "CTRLD": "\x04",
    "PGUP": "\x1b[5~", "PGDN": "\x1b[6~", "HOME": "\x1b[H", "END": "\x1b[F",
}
BUTTONS = {"CLICK": 0, "RCLICK": 2, "WHEELUP": 64, "WHEELDOWN": 65}


def encode_key(k):
    kind = k.split(":")[0]
    if kind in BUTTONS:
        btn = BUTTONS[kind]
        _, _, arg = k.partition(":")
        col, row = (int(v) for v in arg.split(",")) if arg else (1, 1)
        press = f"\x1b[<{btn};{col};{row}M"          # SGR 1006
        return press if btn >= 64 else press + f"\x1b[<{btn};{col};{row}m"
    return NAMED.get(k, k)


class Screen:
    def __init__(self, rows, cols):
        self.rows, self.cols = rows, cols
        self.g = [[" "] * cols for _ in range(rows)]
        self.r = self.c = 0
        self.pending = ""

    def put(self, ch):
        if self.r < self.rows and self.c < self.cols:
            self.g[self.r][self.c] = ch
        self.c = min(self.c + 1, self.cols - 1)

    def feed(self, s):
        s, self.pending = self.pending + s, ""
        i, n = 0, len(s)
        while i < n:
            ch = s[i]
            if ch == "\x1b":
                # an incomplete escape at the end of a read: keep it for later
                if re.fullmatch(r"\x1b\[?[0-9;?]*", s[i:]):
                    self.pending = s[i:]
                    return
                if s[i:i + 2] == "\x1b]" and not re.search(r"(\x07|\x1b\\)", s[i:]):
                    self.pending = s[i:]
                    return
                m = re.match(r"\x1b\[([0-9;?]*)([a-zA-Z])", s[i:])
                if m:
                    p, fn = m.group(1), m.group(2)
                    nums = [int(x) for x in p.split(";") if x.isdigit()]
                    if fn in "Hf":
                        self.r = max(0, min((nums[0] - 1) if nums else 0, self.rows - 1))
                        self.c = max(0, min((nums[1] - 1) if len(nums) > 1 else 0, self.cols - 1))
                    elif fn == "K":
                        k = nums[0] if nums else 0
                        rng = range(self.c, self.cols) if k == 0 else \
                              range(0, self.c + 1) if k == 1 else range(0, self.cols)
                        for x in rng:
                            self.g[self.r][x] = " "
                    elif fn == "J":
                        self.g = [[" "] * self.cols for _ in range(self.rows)]
                        self.r = self.c = 0
                    elif fn == "A": self.r = max(0, self.r - (nums[0] if nums else 1))
                    elif fn == "B": self.r = min(self.rows - 1, self.r + (nums[0] if nums else 1))
                    elif fn == "C": self.c = min(self.cols - 1, self.c + (nums[0] if nums else 1))
                    elif fn == "D": self.c = max(0, self.c - (nums[0] if nums else 1))
                    i += m.end(); continue
                for pat in (r"\x1b\][^\x07\x1b]*(\x07|\x1b\\)", r"\x1b[()][A-Z0-9]", r"\x1b[=>78Mc]"):
                    m = re.match(pat, s[i:])
                    if m:
                        i += m.end(); break
                else:
                    i += 1
                continue
            if ch == "\n": self.r = min(self.rows - 1, self.r + 1)
            elif ch == "\r": self.c = 0
            elif ch == "\x08": self.c = max(0, self.c - 1)
            elif ord(ch) >= 32: self.put(ch)
            i += 1

    def text(self):
        return "\n".join("".join(r).rstrip() for r in self.g)

    def find(self, glyph):
        """1-based (col, row) of a glyph, or None."""
        for y, row in enumerate(self.g):
            x = "".join(row).find(glyph)
            if x >= 0:
                return x + 1, y + 1
        return None


def run(argv, keys, rows=40, cols=160, settle=1.4, step=0.35):
    master, slave = pty.openpty()
    fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", rows, cols, 0, 0))
    env = dict(os.environ, TERM="xterm-256color", COLORTERM="truecolor")
    p = subprocess.Popen(argv, stdin=slave, stdout=slave, stderr=slave,
                         close_fds=True, env=env)
    os.close(slave)
    sc = Screen(rows, cols)

    def pump(seconds):
        end = time.time() + seconds
        while time.time() < end:
            r, _, _ = select.select([master], [], [], 0.05)
            if master in r:
                try:
                    data = os.read(master, 1 << 16)
                except OSError:
                    return
                if not data:
                    return
                sc.feed(data.decode("utf8", "replace"))

    pump(settle)
    for k in keys:
        if k.startswith("CLICKFIND:"):
            spot = sc.find(k.split(":", 1)[1])
            if spot is None:
                continue
            k = f"CLICK:{spot[0]},{spot[1]}"
        os.write(master, encode_key(k).encode())
        pump(step)
    pump(0.5)
    try:
        p.wait(timeout=4)
    except subprocess.TimeoutExpired:
        os.write(master, b"\x03")
        try:
            p.wait(timeout=3)
        except subprocess.TimeoutExpired:
            p.kill()
    os.close(master)
    return sc


if __name__ == "__main__":
    argv = json.loads(sys.argv[1])
    keys = json.loads(sys.argv[2]) if len(sys.argv) > 2 else []
    rows = int(sys.argv[3]) if len(sys.argv) > 3 else 40
    cols = int(sys.argv[4]) if len(sys.argv) > 4 else 160
    print(run(argv, keys, rows=rows, cols=cols).text())
