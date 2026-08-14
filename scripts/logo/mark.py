#!/usr/bin/env python3
"""copycat's mark: one closed stroked path, plus a face.

    python3 scripts/logo/mark.py            # write the SVG to out/
    python3 scripts/logo/mark.py preview    # review sheet at real icon sizes

This is the single source the mark comes from. `build.py` derives every shipped
asset from it, so nothing here should be duplicated into an icon file by hand.

The geometry is a cat head drawn as one continuous line: up the left side, into
the left ear, across the head, up to the right ear, down the right side and
round the chin. It was arrived at from two pen sketches, the second of which is
traced by `trace.py`; the drawing's own proportions are recorded there rather
than here, because this shape is the tidied outcome and not the transcription.

Two shaping rules earn their place:

  * round caps and joins throughout, so the outline reads as drawn rather than
    plotted, and
  * the nose is filleted (`soft_poly`). A filled triangle has three needle
    points and against this soft outline they were the only hard thing left.
"""
import math
import re
import sys
from pathlib import Path

OUT = Path(__file__).parent / "out"

# --- geometry ----------------------------------------------------------------

#: The head, in a 0..100 box, x right and y down. One closed subpath.
OUTLINE = (
    "M27,70 L27.2,9.5 L35,29.5 L57,27.8 L64,7.5 "
    "C69.5,18 72.5,40 72.8,70 "
    "C72.8,79 61,83.5 50,83.5 C39,83.5 27,79 27,70 Z"
)

#: Stroke width of the outline, in the same units.
STROKE_W = 4.2

#: Eyes. Deliberately not level: the right sits 1.5 higher, which is the one
#: asymmetry kept from the sketches and the only thing stopping the face
#: reading as a rendered emoji.
EYES = [(40.0, 50.0, 2.9), (58.0, 48.5, 2.9)]

#: Nose, before filleting.
NOSE = [(46.4, 52.0), (52.6, 52.0), (49.5, 58.5)]

#: Corner radius applied to the nose.
NOSE_R = 1.8

#: Hairline stroke on the face group. Blunts the shapes very slightly and keeps
#: the eyes from looking pinned-on next to a 4.2-wide outline.
FACE_W = 1.1


def parse_path(d):
    """-> list of ('line', p0, p1) | ('cubic', p0, c1, c2, p1).

    Handles the M/L/C/Z subset this module emits. `build.py` uses it to turn
    the outline into the Rust `Prim` list.

    Every letter is tokenised, not just the supported ones, so an unsupported
    command reaches the `raise` below. Matching only `MLCZ` made `H V S Q T A`
    match neither branch: they vanished before the guard could fire and their
    coordinates were consumed by whichever op was still in effect, quietly
    turning a curve into a straight line in the Rust sink alone -- and --check
    cannot see that, since each sink is only ever compared against its own
    re-derivation.
    """
    flat = [(a or b) for a, b in re.findall(r"([A-Za-z])|(-?\d*\.?\d+)", d)]
    cur = start = op = None
    out, i = [], 0
    while i < len(flat):
        if flat[i].isalpha():
            op = flat[i]
            i += 1
            if op == "Z":
                if cur != start:
                    out.append(("line", cur, start))
                cur = start
                continue
        # Validated before the operands are read, not after: `H`/`V` take fewer
        # coordinates than anything here does, so reading first turned an
        # unsupported command into an IndexError instead of the real message.
        if op not in ("M", "L", "C"):
            raise ValueError(f"unsupported path op {op!r}")
        n = [float(flat[i + j]) for j in range(6 if op == "C" else 2)]
        if op == "M":
            cur = start = (n[0], n[1]); i += 2; op = "L"
        elif op == "L":
            p = (n[0], n[1]); out.append(("line", cur, p)); cur = p; i += 2
        else:
            c1, c2, p = (n[0], n[1]), (n[2], n[3]), (n[4], n[5])
            out.append(("cubic", cur, c1, c2, p)); cur = p; i += 6
    return out


def soft_poly(pts, r):
    """Fillet the corners of a closed polygon. -> a closed path `d`.

    Each corner is cut back by `r` along both edges and bridged with a cubic
    whose controls sit 55% of the way to the original vertex, the standard
    circular-arc approximation. `r` is clamped to just under half the shorter
    edge so a short edge between two corners cannot be consumed twice and
    invert the shape.
    """
    def lerp(a, b, t):
        return (a[0] + (b[0] - a[0]) * t, a[1] + (b[1] - a[1]) * t)

    n, out = len(pts), []
    for i in range(n):
        a, v, b = pts[i - 1], pts[i], pts[(i + 1) % n]
        la = math.hypot(v[0] - a[0], v[1] - a[1])
        lb = math.hypot(b[0] - v[0], b[1] - v[1])
        rr = min(r, la * 0.49, lb * 0.49)
        p1, p2 = lerp(v, a, rr / la), lerp(v, b, rr / lb)
        c1, c2 = lerp(p1, v, 0.55), lerp(p2, v, 0.55)
        out.append(f"{'M' if i == 0 else 'L'}{p1[0]:.2f},{p1[1]:.2f}")
        out.append(f"C{c1[0]:.2f},{c1[1]:.2f} {c2[0]:.2f},{c2[1]:.2f} "
                   f"{p2[0]:.2f},{p2[1]:.2f}")
    return " ".join(out) + " Z"


def face_svg():
    """The eyes and nose as SVG shapes."""
    eyes = "".join(f'<circle cx="{x}" cy="{y}" r="{r}"/>' for x, y, r in EYES)
    return eyes + f'<path d="{soft_poly(NOSE, NOSE_R)}"/>'


SVG = '''<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 100 100" width="{px}" height="{px}" role="img" aria-label="copycat">
  <g fill="none" stroke="{color}" stroke-width="{w}" stroke-linecap="round" stroke-linejoin="round">
    <path d="{d}"/>
  </g>
  <g fill="{color}" stroke="{color}" stroke-width="{fw}" stroke-linejoin="round">{face}</g>
</svg>
'''


def svg(color="#FFFFFF", px=256):
    """The mark as an SVG document.

    `color` is a concrete value, not `currentColor`. Every consumer of this
    output renders it standalone -- a panel or file manager reading the
    installed hicolor icon, or rsvg feeding the .ico and macOS masters -- and in
    that context nothing supplies an inherited colour, so librsvg resolves
    `currentColor` to black. The app header does not read the SVG at all; it
    draws from the generated `CAT` const. There is no themeable consumer to
    serve, so shipping a themeable colour only produced a black cat on Linux.
    """
    return SVG.format(d=OUTLINE, face=face_svg(), w=STROKE_W, fw=FACE_W,
                      color=color, px=px)


def render(color, px, path):
    """Rasterise to PNG at `px` via rsvg-convert."""
    import subprocess
    src = OUT / "_tmp.svg"
    OUT.mkdir(parents=True, exist_ok=True)
    src.write_text(svg(color, px))
    subprocess.run(["rsvg-convert", "-w", str(px), "-h", str(px),
                    "-o", str(path), str(src)], check=True)
    src.unlink()
    return path


def preview():
    """Review sheet: white on dark and, below, the same on light.

    Both grounds are shown because the mark ships as a single white line: the
    tray and header sit on dark, but the Windows .ico also lands in Explorer
    lists and light taskbars, where a white line is the failure case worth
    looking at rather than assuming.
    """
    from PIL import Image, ImageDraw
    OUT.mkdir(parents=True, exist_ok=True)
    BIG, SMALL, SIZES = 240, 104, (16, 22, 32, 48)
    W = 24 + BIG + 30 + len(SIZES) * (SMALL + 12) + 24
    sheet = Image.new("RGBA", (W, BIG + 200), (22, 24, 28, 255))
    d = ImageDraw.Draw(sheet)
    d.text((24, 10), "copycat mark   |   256px      16px    22px    32px    48px",
           fill=(150, 156, 164, 255))
    sheet.alpha_composite(Image.open(render("#FFFFFF", BIG, OUT / "pv-big.png")), (24, 30))
    for j, n in enumerate(SIZES):
        s = Image.open(render("#FFFFFF", n, OUT / f"pv-{n}.png"))
        sheet.alpha_composite(s.resize((SMALL, SMALL), Image.NEAREST),
                              (24 + BIG + 30 + j * (SMALL + 12), 30 + (BIG - SMALL) // 2))
    light = Image.new("RGBA", (W - 48, 140), (242, 244, 247, 255))
    for j, n in enumerate((256, 48, 32, 22, 16)):
        im = Image.open(render("#111111", n, OUT / f"pvb-{n}.png"))
        light.alpha_composite(im.resize((112, 112),
                              Image.NEAREST if n < 64 else Image.LANCZOS), (14 + j * 120, 14))
    sheet.alpha_composite(light, (24, BIG + 46))
    sheet.save(OUT / "mark-preview.png")
    print(OUT / "mark-preview.png")


if __name__ == "__main__":
    OUT.mkdir(parents=True, exist_ok=True)
    if len(sys.argv) > 1 and sys.argv[1] == "preview":
        preview()
    else:
        p = OUT / "copycat-cat.svg"
        p.write_text(svg())
        print(p)
