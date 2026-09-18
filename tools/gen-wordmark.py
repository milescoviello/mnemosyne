#!/usr/bin/env python3
"""Generate the tonal ASCII wordmark baked into src/art.rs.

Run: python3 tools/gen-wordmark.py > /tmp/art.rs   (then paste the consts in)

Needs Pillow at authoring time only. The output is static data, so the binary
has no image dependency and rasterises nothing at runtime.

Two things decide whether the result is readable:

* Vertical resolution. Lowercase "mnemosyne" is about 16:1, so even 96 columns
  buys only four rows of x-height -- not enough cells to draw a letter with.
  Uppercase is ~13:1 and every row is cap height, so nothing is spent on
  ascenders or descenders. That change alone did most of the work.
* Contrast. Mid-tones scattered through the inside of a stroke read as noise.
  An S-curve pushes stroke interiors to solid and leaves only the true edges
  soft, which is where the tonal falloff actually belongs.
"""
from PIL import Image, ImageDraw, ImageFont

WORD = "MNEMOSYNE"
FONT = "/usr/share/fonts/dejavu/DejaVuSans-Bold.ttf"
RAMP = " .:-+*#@"
ASPECT = 1.45      # terminal cell height : width, tuned for row count
CONTRAST = 2.4
FLOOR = 0.06
SIZES = [("ART_WIDE", 96), ("ART_MED", 84), ("ART_SMALL", 68)]


def scurve(v, k):
    """Push values away from the middle, so strokes solidify and edges stay soft."""
    if k <= 0:
        return v
    v = min(1.0, max(0.0, v))
    return v ** (1.0 / (1.0 + k)) if v > 0.5 else 1.0 - (1.0 - v) ** (1.0 / (1.0 + k))


def render(text, cols):
    size, cell = 10, 8
    px_w = cols * cell
    while size < 400:
        f = ImageFont.truetype(FONT, size)
        b = f.getbbox(text)
        if b[2] - b[0] >= px_w * 0.985:
            break
        size += 2
    f = ImageFont.truetype(FONT, size)
    b = f.getbbox(text)
    img = Image.new("L", (b[2] - b[0] + 4, b[3] - b[1] + 4), 0)
    ImageDraw.Draw(img).text((2 - b[0], 2 - b[1]), text, font=f, fill=255)
    rows = max(1, round(img.height / (img.width / cols) / ASPECT))
    small = img.resize((cols, rows), Image.LANCZOS)
    out = []
    for y in range(rows):
        line = ""
        for x in range(cols):
            v = scurve(small.getpixel((x, y)) / 255.0, CONTRAST)
            line += " " if v <= FLOOR else RAMP[min(len(RAMP) - 1, int(v * (len(RAMP) - 1) + 0.5))]
        out.append(line.rstrip())
    return [l for l in out if l.strip()]


if __name__ == "__main__":
    for name, cols in SIZES:
        art = render(WORD, cols)
        w = max(len(l) for l in art)
        art = [l.ljust(w) for l in art]
        bad = set("".join(art)) & set('"\\')
        assert not bad, f"characters unsafe in a Rust literal: {bad}"
        print(f"/// Tonal ASCII art, {w} columns by {len(art)} rows.")
        print(f"pub const {name}: [&str; {len(art)}] = [")
        for l in art:
            print(f'    "{l}",')
        print("];\n")
