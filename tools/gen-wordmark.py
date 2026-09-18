#!/usr/bin/env python3
"""Render 'mnemosyne' as tonal ASCII art.

Not an outline font and not solid blocks: the text is rasterised with
anti-aliasing, then each character cell is averaged and mapped onto a density
ramp, so stroke centres come out dense and edges fall away through mid-tones.
That edge falloff is what makes it read as art rather than a stencil.

Terminal cells are about twice as tall as they are wide, so cells are sampled
at 1:2 to keep the proportions right.
"""
from PIL import Image, ImageDraw, ImageFont
import sys

RAMPS = {
    "classic": " .:-=+*#%@",
    "soft":    " .,:;i1tfLCG08@",
    "sparse":  "  ..::--==++**##%%@@",
    "dots":    " ·∶∷⁘⁙▪▫◦●◼",
}

def render(text, font_path, cols, ramp=" .:-=+*#%@", weight=1.0, thresh=0.0):
    # rasterise big, then average down
    cell_w = 8
    cell_h = cell_w * 2
    px_w = cols * cell_w
    # find a font size whose rendered width lands near px_w
    size = 10
    for _ in range(80):
        f = ImageFont.truetype(font_path, size)
        bbox = f.getbbox(text)
        if bbox[2] - bbox[0] >= px_w * 0.98:
            break
        size += 2
    f = ImageFont.truetype(font_path, size)
    bbox = f.getbbox(text)
    tw, th = bbox[2] - bbox[0], bbox[3] - bbox[1]
    img = Image.new("L", (tw + 4, th + 4), 0)
    ImageDraw.Draw(img).text((2 - bbox[0], 2 - bbox[1]), text, font=f, fill=255)

    rows = max(1, round(img.height / (img.width / cols) / 2))
    small = img.resize((cols, rows), Image.LANCZOS)

    out = []
    for y in range(rows):
        line = ""
        for x in range(cols):
            v = small.getpixel((x, y)) / 255.0
            v = min(1.0, v * weight)
            line += " " if v <= thresh else ramp[min(len(ramp) - 1, int(v * (len(ramp) - 1) + 0.5))]
        out.append(line.rstrip())
    return [l for l in out if l.strip()]

if __name__ == "__main__":
    FONT = "/usr/share/fonts/dejavu/DejaVuSans-Bold.ttf"
    for name, cols in (("classic", 78),):
        art = render("mnemosyne", FONT, cols, RAMPS[name])
        print(f"=== ramp={name} cols={cols} rows={len(art)} ===")
        for l in art: print("  " + l)
        print()
