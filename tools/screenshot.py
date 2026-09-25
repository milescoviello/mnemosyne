#!/usr/bin/env python3
"""Render the interface to a PNG, exactly as a terminal would draw it.

    python3 tools/screenshot.py docs/list.png 150 26 --keys '[]'

Runs the binary in a pseudo-terminal, parses the escape codes into a grid of
coloured cells, and draws that with a monospace font. Reproducible, so the
images in the README can be regenerated rather than re-grabbed by hand, and
it needs no desktop — which also means it works over ssh and in CI.

Needs Pillow at authoring time only; nothing here ships in the binary.
"""
import argparse, fcntl, json, os, pty, re, select, struct, subprocess, sys, termios, time
from PIL import Image, ImageDraw, ImageFont

# A terminal falls back to another font for a glyph its primary lacks, so the
# renderer has to as well — otherwise the images show boxes where a real
# terminal would show the character, or vice versa.
FONTS = [
    "/usr/share/fonts/jetbrains-mono/JetBrainsMono-Regular.ttf",
    "/usr/share/fonts/dejavu/DejaVuSansMono.ttf",
    "/usr/share/fonts/noto/NotoSansMono-Regular.ttf",
]
FONTS_BOLD = [
    "/usr/share/fonts/jetbrains-mono/JetBrainsMono-Bold.ttf",
    "/usr/share/fonts/dejavu/DejaVuSansMono-Bold.ttf",
    "/usr/share/fonts/noto/NotoSansMono-Bold.ttf",
]
FONT = FONTS[0]
FONT_BOLD = FONTS_BOLD[0]
BG = (11, 15, 20)
FG = (200, 205, 212)

# xterm's first sixteen, which is what indexed colours resolve to.
BASE16 = [
    (0, 0, 0), (205, 49, 49), (13, 188, 121), (229, 229, 16),
    (36, 114, 200), (188, 63, 188), (17, 168, 205), (229, 229, 229),
    (102, 102, 102), (241, 76, 76), (35, 209, 139), (245, 245, 67),
    (59, 142, 234), (214, 112, 214), (41, 184, 219), (255, 255, 255),
]


def xterm256(n):
    if n < 16:
        return BASE16[n]
    if n < 232:
        n -= 16
        lv = [0, 95, 135, 175, 215, 255]
        return (lv[n // 36], lv[(n // 6) % 6], lv[n % 6])
    g = 8 + (n - 232) * 10
    return (g, g, g)


class Cell:
    __slots__ = ("ch", "fg", "bg", "bold", "dim")

    def __init__(self):
        self.ch, self.fg, self.bg, self.bold, self.dim = " ", None, None, False, False


class Screen:
    """Just enough terminal to reproduce what was drawn."""

    def __init__(self, rows, cols):
        self.rows, self.cols = rows, cols
        self.g = [[Cell() for _ in range(cols)] for _ in range(rows)]
        self.r = self.c = 0
        self.fg = self.bg = None
        self.bold = self.dim = False
        self.pending = ""

    def sgr(self, params):
        nums = [int(x) for x in params.split(";") if x != ""] or [0]
        i = 0
        while i < len(nums):
            n = nums[i]
            if n == 0:
                self.fg = self.bg = None
                self.bold = self.dim = False
            elif n == 1:
                self.bold = True
            elif n == 2:
                self.dim = True
            elif n == 22:
                self.bold = self.dim = False
            elif 30 <= n <= 37:
                self.fg = BASE16[n - 30]
            elif 90 <= n <= 97:
                self.fg = BASE16[n - 90 + 8]
            elif 40 <= n <= 47:
                self.bg = BASE16[n - 40]
            elif 100 <= n <= 107:
                self.bg = BASE16[n - 100 + 8]
            elif n == 39:
                self.fg = None
            elif n == 49:
                self.bg = None
            elif n in (38, 48):
                target = "fg" if n == 38 else "bg"
                if i + 1 < len(nums) and nums[i + 1] == 2:
                    setattr(self, target, tuple(nums[i + 2:i + 5]))
                    i += 4
                elif i + 1 < len(nums) and nums[i + 1] == 5:
                    setattr(self, target, xterm256(nums[i + 2]))
                    i += 2
            i += 1

    def put(self, ch):
        if self.r < self.rows and self.c < self.cols:
            cell = self.g[self.r][self.c]
            cell.ch, cell.fg, cell.bg = ch, self.fg, self.bg
            cell.bold, cell.dim = self.bold, self.dim
        self.c = min(self.c + 1, self.cols - 1)

    def feed(self, s):
        s, self.pending = self.pending + s, ""
        i, n = 0, len(s)
        while i < n:
            ch = s[i]
            if ch == "\x1b":
                # CSI may carry a private prefix: `\x1b[>1u` pushes the
                # keyboard flags, and without the prefix it was drawn as text
                if re.fullmatch(r"\x1b\[?[<=>?]?[0-9;?]*", s[i:]):
                    self.pending = s[i:]
                    return
                m = re.match(r"\x1b\[([<=>?]?[0-9;?]*)([a-zA-Z])", s[i:])
                if m and m.group(1)[:1] in ("<", "=", ">"):
                    i += m.end(); continue
                if m:
                    p, f = m.group(1), m.group(2)
                    nums = [int(x) for x in p.split(";") if x.isdigit()]
                    if f in "Hf":
                        self.r = max(0, min((nums[0] - 1) if nums else 0, self.rows - 1))
                        self.c = max(0, min((nums[1] - 1) if len(nums) > 1 else 0, self.cols - 1))
                    elif f == "m":
                        self.sgr(p)
                    elif f == "K":
                        k = nums[0] if nums else 0
                        rng = (range(self.c, self.cols) if k == 0
                               else range(0, self.c + 1) if k == 1 else range(0, self.cols))
                        for x in rng:
                            self.g[self.r][x] = Cell()
                    elif f == "J":
                        self.g = [[Cell() for _ in range(self.cols)] for _ in range(self.rows)]
                        self.r = self.c = 0
                    elif f == "A": self.r = max(0, self.r - (nums[0] if nums else 1))
                    elif f == "B": self.r = min(self.rows - 1, self.r + (nums[0] if nums else 1))
                    elif f == "C": self.c = min(self.cols - 1, self.c + (nums[0] if nums else 1))
                    elif f == "D": self.c = max(0, self.c - (nums[0] if nums else 1))
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
            elif ord(ch) >= 32: self.put(ch)
            i += 1


NAMED = {"DOWN": "\x1b[B", "UP": "\x1b[A", "RIGHT": "\x1b[C", "LEFT": "\x1b[D",
         "ENTER": "\r", "ESC": "\x1b", "TAB": "\t", "SPACE": " "}


def capture(argv, keys, rows, cols, settle, after):
    master, slave = pty.openpty()
    fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", rows, cols, 0, 0))
    env = dict(os.environ, TERM="xterm-256color", COLORTERM="truecolor")
    p = subprocess.Popen(argv, stdin=slave, stdout=slave, stderr=slave, close_fds=True, env=env)
    os.close(slave)
    sc = Screen(rows, cols)

    def pump(seconds):
        end = time.time() + seconds
        while time.time() < end:
            r, _, _ = select.select([master], [], [], 0.02)
            if master in r:
                try:
                    d = os.read(master, 1 << 16)
                except OSError:
                    return
                if not d:
                    return
                sc.feed(d.decode("utf8", "replace"))

    pump(settle)
    for k in keys:
        os.write(master, NAMED.get(k, k).encode())
        pump(0.35)
    pump(after)
    os.write(master, b"q")
    time.sleep(0.2)
    try:
        p.wait(timeout=4)
    except subprocess.TimeoutExpired:
        p.kill()
    os.close(master)
    return sc


def load_chain(paths, size):
    out = []
    for p in paths:
        try:
            out.append(ImageFont.truetype(p, size))
        except Exception:
            pass
    return out


def render(sc, path, scale=2, pad=14):
    size = 15 * scale
    chain = load_chain(FONTS, size)
    chain_bold = load_chain(FONTS_BOLD, size)
    font, bold = chain[0], chain_bold[0]
    tofu = font.getmask("\ue123")

    def pick(fonts, ch):
        """First font in the chain that really has this glyph."""
        for f in fonts:
            m = f.getmask(ch)
            if m.size != tofu.size or bytes(m) != bytes(tofu):
                return f
        return fonts[0]
    cw = round(font.getlength("M"))
    chh = round(size * 1.34)
    img = Image.new("RGB", (sc.cols * cw + pad * 2, sc.rows * chh + pad * 2), BG)
    d = ImageDraw.Draw(img)
    for y, row in enumerate(sc.g):
        for x, cell in enumerate(row):
            px, py = pad + x * cw, pad + y * chh
            if cell.bg:
                d.rectangle([px, py, px + cw, py + chh], fill=cell.bg)
            if cell.ch == " ":
                continue
            fg = cell.fg or FG
            if cell.dim:
                fg = tuple(int(c * 0.6) for c in fg)
            use = chain_bold if cell.bold else chain
            d.text((px, py), cell.ch, font=pick(use, cell.ch), fill=fg)
    img.save(path)
    return img.size


if __name__ == "__main__":
    ap = argparse.ArgumentParser()
    ap.add_argument("out")
    ap.add_argument("cols", type=int)
    ap.add_argument("rows", type=int)
    ap.add_argument("--keys", default="[]")
    ap.add_argument("--settle", type=float, default=1.6)
    ap.add_argument("--after", type=float, default=0.4)
    ap.add_argument("--cmd", default="./target/release/mnemosyne --no-splash")
    a = ap.parse_args()
    sc = capture(a.cmd.split(), json.loads(a.keys), a.rows, a.cols, a.settle, a.after)
    os.makedirs(os.path.dirname(a.out) or ".", exist_ok=True)
    print(f"{a.out}  {render(sc, a.out)}")
