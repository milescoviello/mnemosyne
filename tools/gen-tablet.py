#!/usr/bin/env python3
"""Generate the gold tablet: the pixel maps baked into src/art.rs, and the
SVG logo in docs/.

    python3 tools/gen-tablet.py rust            # consts to paste into art.rs
    python3 tools/gen-tablet.py svg docs/logo.svg
    python3 tools/gen-tablet.py mark docs/mark.svg
    python3 tools/gen-tablet.py preview out.png # authoring aid, needs Pillow

The Orphic gold tablets were thin leaves of gold buried with the dead,
telling them to drink from the spring of Mnemosyne rather than Lethe's. So
the name is cut into a leaf of gold, in Greek capitals: ΜΝΗΜΟΣΥΝΗ.

The letters are drawn by hand on a pixel grid rather than rasterised from a
font. A terminal cell is about twice as tall as it is wide, so a half block
(▀ ▄) is close to a square pixel, and a letter eight pixels tall is four rows.
At that size a rasteriser smears every diagonal; a hand-set pixel does not.

Everything here is geometry. The colour -- the lighting across the leaf, the
glint that sweeps it, the incision as it is cut -- is worked out at runtime
in art.rs, because it moves. The SVG gets the same lighting, frozen.
"""
import sys

# Eight pixels tall, seven wide. Stoichedon: the Greek habit of cutting
# inscriptions on a strict grid, every letter in its own cell, which is what
# a terminal does anyway.
BIG = {
    "M": ["#.....#", "##...##", "#.#.#.#", "#..#..#", "#.....#", "#.....#", "#.....#", "#.....#"],
    "N": ["#.....#", "##....#", "#.#...#", "#..#..#", "#...#.#", "#....##", "#.....#", "#.....#"],
    "H": ["#.....#", "#.....#", "#.....#", "#######", "#.....#", "#.....#", "#.....#", "#.....#"],
    "O": ["..###..", ".#...#.", "#.....#", "#.....#", "#.....#", "#.....#", ".#...#.", "..###.."],
    "S": ["#######", ".#....#", "..#....", "...#...", "...#...", "..#....", ".#....#", "#######"],
    "Y": ["#.....#", ".#...#.", "..#.#..", "...#...", "...#...", "...#...", "...#...", "...#..."],
}
# Six tall, five wide, for terminals too narrow for the big one.
SMALL = {
    "M": ["#...#", "##.##", "#.#.#", "#...#", "#...#", "#...#"],
    "N": ["#...#", "##..#", "#.#.#", "#..##", "#...#", "#...#"],
    "H": ["#...#", "#...#", "#####", "#...#", "#...#", "#...#"],
    "O": [".###.", "#...#", "#...#", "#...#", "#...#", ".###."],
    "S": ["#####", ".#..#", "..#..", "..#..", ".#..#", "#####"],
    "Y": ["#...#", ".#.#.", "..#..", "..#..", "..#..", "..#.."],
}
WORD = "MNHMOSYNH"  # Μ Ν Η Μ Ο Σ Υ Ν Η, spelled in the Latin lookalikes

# The first words of the gold tablet from Hipponion, c. 400 BC: "this is the
# work of Memory". Cut in the terminal's own type rather than in pixels, so
# it stays legible at one row.
LINE = "ΜΝΑΜΟΣΥΝΑΣ ΤΟΔΕ ΕΡΓΟΝ"

# name, font, letter gap, margin x, margin top, margin bottom, second line
SIZES = [
    ("TABLET_WIDE", BIG, 2, 5, 3, 2, LINE),
    ("TABLET_MED", BIG, 1, 4, 3, 2, LINE),
    ("TABLET_SMALL", SMALL, 1, 3, 2, 2, LINE),
]


def tablet(font, gap, mx, mt, mb, line=None):
    """The leaf as rows of classes: ' ' bare, 'g' gold, 'f' fold, '#' cut,
    't' where the second line of text goes."""
    lh = len(font["M"])
    ww = sum(len(font[c][0]) for c in WORD) + gap * (len(WORD) - 1)
    W = ww + 2 * mx
    y = mt + lh
    text_y = None
    if line:
        # a pixel of air, then a whole cell row: text cannot sit on a half
        y += 1 + (mt + lh + 1) % 2
        text_y = y
        y += 2
    H = y + mb + (y + mb) % 2
    assert H % 2 == 0, "half blocks pair rows, so the leaf must be an even height"
    g = [["g"] * W for _ in range(H)]
    if line:
        tx = (W - len(line)) // 2
        for yy in (text_y, text_y + 1):
            for i in range(len(line)):
                g[yy][tx + i] = "t"
    x = mx
    for c in WORD:
        for r, row in enumerate(font[c]):
            for i, ch in enumerate(row):
                if ch == "#":
                    g[mt + r][x + i] = "#"
        x += len(font[c][0]) + gap

    def bare(x, y):
        if 0 <= x < W and 0 <= y < H and g[y][x] != "#":
            g[y][x] = " "

    # Rounded where it was cut, torn where it was not. The real leaves are
    # rarely square: a corner is missing, an edge has come away.
    for x, y in [(0, 0), (W - 1, 0), (0, H - 1), (W - 1, H - 1)]:
        bare(x, y)
    # the top right corner broken off, on a slant
    for i in range(3):
        for j in range(3 - i):
            bare(W - 1 - j, i)
    # a torn right edge: ragged, not regular, or it reads as a comb
    tear = [0, 0, 1, 2, 1, 1, 0, 0, 1, 0, 0, 2, 1, 0, 1, 1, 0, 0, 1, 2]
    for y in range(H):
        for d in range(tear[y % len(tear)]):
            bare(W - 1 - d, y)
    # a nick out of the bottom edge, and one from the top
    for x in (int(W * 0.63), int(W * 0.63) + 1, int(W * 0.21)):
        bare(x, H - 1)
    bare(int(W * 0.81), 0)

    # Folded in three to be carried, as most of them were, and flattened
    # out again: two creases, running top to bottom.
    # A crease crosses whatever is in its way, letters included, the way
    # it does on the leaves themselves.
    for fx in (W // 3, 2 * W // 3):
        for y in range(H):
            if g[y][fx] == "g":
                g[y][fx] = "f"
    return ["".join(r) for r in g]


# ---------------------------------------------------------------- lighting
# A static copy of what art.rs does at runtime, for the SVG and the preview.

RAMP = [(0x2B, 0x21, 0x17), (0x5F, 0x45, 0x24), (0x9A, 0x74, 0x38),
        (0xD4, 0xA9, 0x4F), (0xEF, 0xCF, 0x7A), (0xFB, 0xEF, 0xC4)]


def ramp(p):
    p = max(0.0, min(1.0, p))
    s = p * (len(RAMP) - 1)
    i = int(s)
    if i >= len(RAMP) - 1:
        return RAMP[-1]
    a, b, t = RAMP[i], RAMP[i + 1], s - i
    return tuple(round(a[k] + (b[k] - a[k]) * t) for k in range(3))


def lift(c, t):
    return tuple(round(c[k] + (255 - c[k]) * t) for k in range(3))


def darken(c, t):
    return tuple(round(c[k] * (1 - t)) for k in range(3))


def colour(g, x, y):
    H, W = len(g), len(g[0])
    c = g[y][x]
    if c == " ":
        return None
    if c == "#":
        return ramp(0.2)
    if c == "t":
        c = "g"
    # light from the upper left, and a broad sheen across it: metal reads
    # as metal by its highlights, not by its hue
    u = x / W * 0.7 + y / H * 0.3
    sheen = max(0.0, 1 - abs(u - 0.3) / 0.22) ** 2
    grain = ((x * 73856093 ^ y * 19349663) % 1000) / 1000.0 - 0.5
    col = ramp(0.52 + (1 - u) * 0.3 + grain * 0.04)
    col = lift(col, sheen * 0.22)
    if c == "f":
        col = darken(col, 0.16)
    elif x > 0 and g[y][x - 1] == "f":
        col = lift(col, 0.10)

    def bare(dx, dy):
        xx, yy = x + dx, y + dy
        return not (0 <= xx < W and 0 <= yy < H) or g[yy][xx] == " "

    if bare(1, 0) or bare(-1, 0) or bare(0, 1) or bare(0, -1):
        col = darken(col, 0.2)
    if y > 0 and g[y - 1][x] == "#":
        col = lift(col, 0.16)
    return col


def hexc(c):
    return "#%02x%02x%02x" % c


def svg(g, px=6, pad=0):
    """Pixels as rects, a unit per pixel, scaled by the viewBox."""
    H, W = len(g), len(g[0])
    out = [f'<svg xmlns="http://www.w3.org/2000/svg" viewBox="{-pad} {-pad} {W + 2 * pad} {H + 2 * pad}" '
           f'width="{(W + 2 * pad) * px}" height="{(H + 2 * pad) * px}" shape-rendering="crispEdges">',
           "<title>ΜΝΗΜΟΣΥΝΗ</title>"]
    for y in range(H):
        x = 0
        while x < W:
            c = colour(g, x, y)
            if c is None:
                x += 1
                continue
            # merge runs of one colour, which the cut letters mostly are
            run = 1
            while x + run < W and colour(g, x + run, y) == c:
                run += 1
            out.append(f'<rect x="{x}" y="{y}" width="{run}" height="1" fill="{hexc(c)}"/>')
            x += run
    out.append("</svg>")
    return "\n".join(out) + "\n"


def mark():
    """The square mark: the first letter alone on its leaf."""
    font = BIG
    m = font["M"]
    W, H = len(m[0]) + 8, len(m) + 8
    g = [["g"] * W for _ in range(H)]
    for r, row in enumerate(m):
        for i, ch in enumerate(row):
            if ch == "#":
                g[4 + r][4 + i] = "#"
    for x, y in [(0, 0), (W - 1, 0), (0, H - 1), (W - 1, H - 1), (W - 2, 0), (W - 1, 1)]:
        g[y][x] = " "
    for y in range(H):
        if g[y][W // 2 + 1] == "g":
            g[y][W // 2 + 1] = "f"
    return ["".join(r) for r in g]


def preview(path, cw=12, ch=24):
    from PIL import Image, ImageDraw, ImageFont
    font = ImageFont.truetype("/usr/share/fonts/jetbrains-mono/JetBrainsMono-Bold.ttf", 20)
    boards = [tablet(*s[1:]) for s in SIZES] + [mark()]
    Wmax = max(len(b[0]) for b in boards)
    Htot = sum(len(b) // 2 + 2 for b in boards)
    img = Image.new("RGB", ((Wmax + 8) * cw, (Htot + 2) * ch), (11, 15, 20))
    d = ImageDraw.Draw(img)
    oy = ch
    for g in boards:
        for y in range(len(g)):
            for x in range(len(g[0])):
                c = colour(g, x, y)
                if c:
                    X, Y = 4 * cw + x * cw, oy + y * ch // 2
                    d.rectangle([X, Y, X + cw - 1, Y + ch // 2 - 1], fill=c)
        for y in range(0, len(g), 2):
            row = g[y]
            if "t" in row:
                x0 = row.index("t")
                for i, chr_ in enumerate(LINE):
                    d.text((4 * cw + (x0 + i) * cw, oy + y * ch // 2 + 1), chr_, font=font, fill=ramp(0.2))
        oy += (len(g) // 2 + 2) * ch
    img.save(path)


def rust():
    print(f'pub const INSCRIPTION: &str = "{LINE}";\n')
    for name, *geometry in SIZES:
        g = tablet(*geometry)
        bad = set("".join(g)) - set(" gf#t")
        assert not bad, bad
        print(f"/// {len(g[0])} columns by {len(g) // 2} rows.")
        print(f"pub const {name}: [&str; {len(g)}] = [")
        for row in g:
            print(f'    "{row}",')
        print("];\n")


if __name__ == "__main__":
    what = sys.argv[1] if len(sys.argv) > 1 else "rust"
    if what == "rust":
        rust()
    elif what == "svg":
        open(sys.argv[2], "w").write(svg(tablet(*SIZES[0][1:])))
    elif what == "mark":
        open(sys.argv[2], "w").write(svg(mark(), px=16))
    elif what == "preview":
        preview(sys.argv[2])
    else:
        sys.exit(__doc__)
