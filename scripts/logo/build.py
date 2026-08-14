#!/usr/bin/env python3
"""Regenerate every shipped logo asset from `mark.py`.

    python3 scripts/logo/build.py            # write all sinks
    python3 scripts/logo/build.py --check    # verify they match; write nothing

There are six places a copycat logo lives, and before this they were separate
copies that had already drifted -- dist/copycat.svg still shipped a
clipboard-and-paw drawing long after the header had switched to a cat. Each is
now derived, so "change the logo" is one command:

  1. crates/copycat-ui/dist/copycat.svg   Linux hicolor scalable icon
  2. crates/copycat-ui/dist/copycat.ico   Windows; embedded in the exe by build.rs
  3. dist/macos/copycat-icon-1024.png     master that build-app.sh feeds iconutil
  4. crates/copycat-ui/src/cat_logo.rs    CAT (codegen), ASPECT and INK, which
                                          the tray icon and the window icon
                                          render from
  5. docs/assets/mark-dark.svg            the README mark, GitHub dark theme
  6. docs/assets/mark-light.svg           the README mark, GitHub light theme

The README pair is the one place --color does not reach, and deliberately: a
README is read on both GitHub themes and has to stay legible on each, so the
two are pinned to colours chosen against those backgrounds. Everything else
takes a single ink, because everything else lands on one known surface.

Sinks 2 and 3 are peers, not a chain: every ICO frame and the 1024 master are
rendered independently at native size from the SVG, rather than one being
downscaled from the other.

Sink 4 is codegen. The outline becomes a run of `Prim::Stroke` -- a straight
segment is a cubic with its controls on the chord, so `bezier_at` needs no
special case -- and the face becomes `Prim::Circle` and `Prim::Tri`. The tray
and window icons therefore keep rendering from Rust with no image decoder.

The nose is the one lossy step. `Prim` has no filleted polygon, so the Rust mark
takes the sharp triangle. That is a real difference, not a rounding error: at
the 64 px both `rasterize` callers use, `NOSE_R` is about 1.3 px and the
filleted nose carries roughly 9% less ink. It is accepted because a filleted
`Prim` variant is not worth carrying for a shape this small, and nothing in the
Rust path rounds it -- `paint` draws `Prim::Tri` with `PathStroke::NONE`. The
SVG, and therefore every raster sink, keeps the true shape.

INK: the mark ships as a single white line on transparent. That is right for the
tray, the window icon and dark taskbars, and it is wrong in Explorer's light
file lists and on light Linux panels, where a white line on transparent is
invisible. --color exists for that reason; changing the default is a design
decision, not a bug fix, so it is left to a human. The window *header* is not
covered by this: it sits on a surface the app itself themes, so `app.rs` picks
its own colour rather than always taking INK.

The SVG gets that same concrete colour rather than `currentColor`. It looks like
a themeable file but is never consumed as one: it is installed to
hicolor/scalable/apps and rendered standalone by a panel or file manager, where
nothing supplies an inherited colour and librsvg resolves `currentColor` to
black. The app header does not read this file at all -- it draws from the
generated CAT const -- so there is no themeable consumer to serve, and shipping
`currentColor` only meant Linux quietly got a black cat.
"""
import argparse
import re
import subprocess
import sys
import tempfile
from pathlib import Path

sys.path.insert(0, str(Path(__file__).parent))
import mark  # noqa: E402

REPO = Path(__file__).resolve().parents[2]
ICO_SIZES = [(n, n) for n in (16, 32, 48, 64, 128, 256)]
WHITE = "#FFFFFF"

#: README marks. GitHub swaps <picture> sources on its own theme, so these are
#: pinned rather than following --color: white would be invisible on the light
#: theme and near-black invisible on the dark one.
README_DARK_INK = "#F0F6FC"
README_LIGHT_INK = "#1F2328"

SVG_SINK = REPO / "crates/copycat-ui/dist/copycat.svg"
ICO_SINK = REPO / "crates/copycat-ui/dist/copycat.ico"
MAC_SINK = REPO / "dist/macos/copycat-icon-1024.png"
RS_SINK = REPO / "crates/copycat-ui/src/cat_logo.rs"
MD_DARK_SINK = REPO / "docs/assets/mark-dark.svg"
MD_LIGHT_SINK = REPO / "docs/assets/mark-light.svg"


def ink_box(steps=64):
    """Bounding box of the drawn ink in master units, stroke included.

    Beziers are flattened rather than bounded by their control points: a control
    point can sit well outside the curve it steers, and here that would report
    the mark wider than it is and leave a visible margin on one side only.
    """
    xs, ys = [], []
    for seg in mark.parse_path(mark.OUTLINE):
        if seg[0] == "line":
            pts = [seg[1], seg[2]]
        else:
            p0, c1, c2, p1 = seg[1:]
            pts = []
            for i in range(steps + 1):
                t = i / steps
                u = 1 - t
                a, b, c, d = u**3, 3 * u * u * t, 3 * u * t * t, t**3
                pts.append((a * p0[0] + b * c1[0] + c * c2[0] + d * p1[0],
                            a * p0[1] + b * c1[1] + c * c2[1] + d * p1[1]))
        for p in pts:
            xs.append(p[0]); ys.append(p[1])
    half = mark.STROKE_W / 2
    return (min(xs) - half, min(ys) - half, max(xs) + half, max(ys) + half)


def normaliser():
    """-> (point mapper into the 0..1 box, the scale divisor).

    Uniform scale on both axes, ink fitted to the larger dimension and centred
    in the other. Two constraints force this:

      * `Prim::covers` measures a point's distance to a stroke centre-line, so
        x and y must share a scale or the stroke width means different things
        horizontally and vertically and the rasterised mark comes out lumpy;
      * `paint` maps the whole 0..1 box onto its rect and sizes stroke widths
        and circle radii off the rect's *height* alone, so ASPECT has to stay
        1.0 and the rect stays square.

    Which is why fitting happens here instead: the mark is made to fill a square
    box, rather than the box being stretched to fit the mark. Emitting the
    master's own coordinates left the ink occupying 46% of the box's width and
    76% of its height, so the header drew it squashed and every icon wasted half
    its pixels on margin.
    """
    x0, y0, x1, y1 = ink_box()
    span = max(x1 - x0, y1 - y0)
    ox = (span - (x1 - x0)) / 2
    oy = (span - (y1 - y0)) / 2
    return lambda p: (round((p[0] - x0 + ox) / span, 4),
                      round((p[1] - y0 + oy) / span, 4)), span


def emit_rust():
    """Codegen the CAT prim list, in cat_logo.rs's 0..1 box."""
    n, span = normaliser()
    s = lambda v: round(v / span, 4)
    lines = []
    for seg in mark.parse_path(mark.OUTLINE):
        if seg[0] == "line":
            p0, p1 = seg[1], seg[2]
            c1 = (p0[0] + (p1[0] - p0[0]) / 3, p0[1] + (p1[1] - p0[1]) / 3)
            c2 = (p0[0] + 2 * (p1[0] - p0[0]) / 3, p0[1] + 2 * (p1[1] - p0[1]) / 3)
        else:
            p0, c1, c2, p1 = seg[1], seg[2], seg[3], seg[4]
        pts = ", ".join(f"({n(p)[0]}, {n(p)[1]})" for p in (p0, c1, c2, p1))
        lines.append(f"    Prim::Stroke {{\n        p: [{pts}],\n        w: {s(mark.STROKE_W)},\n    }},")

    for cx, cy, r in mark.EYES:
        c = n((cx, cy))
        lines.append(f"    Prim::Circle {{\n        c: ({c[0]}, {c[1]}),\n        r: {s(r)},\n    }},")
    tri = ", ".join(f"({n(p)[0]}, {n(p)[1]})" for p in mark.NOSE)
    lines.append(f"    Prim::Tri([{tri}]),")

    body = "\n".join(lines)
    return (
        "/// The copycat mark: a cat head drawn as one closed line, plus a face.\n"
        "///\n"
        "/// GENERATED by `scripts/logo/build.py` from `scripts/logo/mark.py`.\n"
        "/// Edit the geometry there, not here, or the SVG, ICO and macOS assets\n"
        "/// drift out of sync with what the tray and window icons draw.\n"
        "///\n"
        "/// The nose is the plain triangle: `Prim` has no filleted polygon. That\n"
        "/// is a visible difference rather than a rounding error - about 1.3 px\n"
        "/// at the 64 px [`rasterize`]'s callers use, and roughly 9% of the\n"
        "/// nose's ink - and nothing here rounds it, since [`paint`] draws\n"
        "/// `Prim::Tri` with `PathStroke::NONE`. It is accepted rather than\n"
        "/// fixed: a filleted variant is not worth carrying for a shape this\n"
        "/// small. The SVG, and so every raster asset, keeps the true shape.\n"
        f"pub const CAT: &[Prim] = &[\n{body}\n];\n"
    )


def edition():
    """The workspace edition, so rustfmt is invoked the way cargo would.

    Read rather than hardcoded: a mismatch here formats the generated const to a
    different standard than `cargo fmt` does, which surfaces as --check calling
    the file stale with no geometry change behind it.
    """
    m = re.search(r'^edition\s*=\s*"(\d+)"', (REPO / "Cargo.toml").read_text(), re.M)
    return m.group(1) if m else "2021"


def rustfmt(text):
    """Normalise Rust source through rustfmt, reading stdin.

    Required, not cosmetic. rustfmt reflows the generated const -- it decides
    for itself where a `p: [...]` array wraps -- so text straight out of
    `emit_rust` never equals what ends up committed after `cargo fmt`.

    A missing or failing rustfmt is fatal. Returning the input unformatted
    instead looks forgiving and is worse: --check would report the file stale on
    every run, and the write path would silently leave a tree that fails
    `cargo fmt --all --check` while printing a success line.
    """
    try:
        p = subprocess.run(["rustfmt", "--edition", edition(), "--emit", "stdout"],
                           input=text, capture_output=True, text=True)
    except FileNotFoundError:
        sys.exit("rustfmt is required by this script: rustup component add rustfmt")
    if p.returncode != 0:
        sys.exit(f"rustfmt failed on the generated source:\n{p.stderr}")
    # rustfmt prefixes stdout output with a `//stdin` banner line.
    return re.sub(r"\A//stdin\n*", "", p.stdout)


def patch_rust(color, check):
    """Rewrite the generated parts of cat_logo.rs: CAT, ASPECT and INK."""
    src = RS_SINK.read_text()
    pat = re.compile(r"(?:^///[^\n]*\n)*^pub const CAT: &\[Prim\] = &\[.*?^\];\n",
                     re.S | re.M)
    if not pat.search(src):
        sys.exit(f"could not locate the CAT const in {RS_SINK}")
    out = pat.sub(emit_rust(), src)
    # ASPECT is 1.0 by construction and deliberately not derived from the
    # geometry: the box is square (see `normaliser`) and `paint` maps that box
    # onto its rect, so anything else stretches the mark. It was briefly
    # generated from the ink's own aspect, which drew the header cat squashed to
    # 60% of its width.
    out = re.sub(r"pub const ASPECT: f32 = [\d.]+;",
                 "pub const ASPECT: f32 = 1.0;", out)
    # INK is substituted for the same reason CAT is: the Rust rasterisers are a
    # sink like any other. Left hand-written, --color repainted the SVG, the
    # .ico and the macOS master while the tray and window icons stayed white,
    # and --check could not see the divergence, because it only ever compares
    # each sink against its own re-derivation.
    r, g, b = (int(color.lstrip("#")[i:i + 2], 16) for i in (0, 2, 4))
    out = re.sub(r"pub const INK: \[u8; 3\] = \[[^\]]*\];",
                 f"pub const INK: [u8; 3] = [0x{r:02X}, 0x{g:02X}, 0x{b:02X}];", out)
    return write(RS_SINK, rustfmt(out), check)


def write(path, text, check):
    if check:
        if (path.read_text() if path.exists() else None) != text:
            print(f"STALE {path.relative_to(REPO)}")
            return False
        return True
    path.write_text(text)
    print(f"wrote {path.relative_to(REPO)}")
    return True


def write_bytes(path, data, check):
    """Write a binary sink, or under --check compare it by *pixels*.

    Not by bytes: these come out of librsvg and Pillow, whose encoders are free
    to differ between versions and platforms without a single rendered pixel
    changing. A byte comparison would turn "the CI runner has a newer library"
    into "the assets are stale", and a check that cries wolf gets deleted.
    Comparing decoded frames still catches the failure that matters -- geometry
    changed and nobody re-ran this.
    """
    if not check:
        path.write_bytes(data)
        print(f"wrote {path.relative_to(REPO)}")
        return True
    if not path.exists():
        print(f"MISSING {path.relative_to(REPO)}")
        return False
    if not pixels_match(path.read_bytes(), data):
        print(f"STALE {path.relative_to(REPO)}")
        return False
    return True


def frames(data):
    """Every frame of an encoded image, keyed by size.

    An .ico needs its own path. Pillow's ICO plugin implements no `seek`, so
    `ImageSequence.Iterator` yields the single largest frame and stops -- which
    silently left five of the six .ico frames unverified, the 16 px one
    included, on the sink with the least review coverage of any here.
    """
    from io import BytesIO

    from PIL import Image, ImageSequence

    im = Image.open(BytesIO(data))
    if im.format == "ICO":
        return {sz: im.ico.getimage(sz).convert("RGBA") for sz in im.ico.sizes()}
    return {f.size: f.convert("RGBA").copy() for f in ImageSequence.Iterator(im)}


def pixels_match(a, b, tol=2.0):
    """Do two encoded images decode to the same frames, within `tol` per channel?"""
    from PIL import ImageChops

    fa, fb = frames(a), frames(b)
    if sorted(fa) != sorted(fb):
        return False
    for sz, ia in fa.items():
        diff = ImageChops.difference(ia, fb[sz])
        if max(diff.getextrema(), key=lambda mm: mm[1])[1] > tol:
            return False
    return True


def build_ico(color, tmp):
    """Render each .ico frame at its native size and pack them, PNG-compressed.

    Every size is rendered from the SVG rather than downscaled from one master,
    so the 16 px frame is rasterised as 16 px art instead of a shrunk 256.

    Pillow writes the directory. ImageMagick's `convert` emitted every entry as
    a raw 32bpp DIB: 370 KB against 15 KB for the same pixels PNG-compressed,
    and build.rs links the whole thing into copycat-ui.exe.
    """
    from PIL import Image

    imgs = [Image.open(mark.render(color, n, tmp / f"{n}.png")).convert("RGBA")
            for n, _ in ICO_SIZES]
    out = tmp / "copycat.ico"
    imgs[-1].save(out, format="ICO", sizes=ICO_SIZES, append_images=imgs[:-1])
    return out.read_bytes()


def main():
    ap = argparse.ArgumentParser(description=__doc__.split("\n")[0])
    ap.add_argument("--color", default=WHITE,
                    help="ink for every sink: the SVG, the .ico, the macOS "
                         "master and cat_logo.rs's INK const")
    ap.add_argument("--check", action="store_true",
                    help="verify the committed assets match; write nothing")
    a = ap.parse_args()
    ok = True

    ok &= write(SVG_SINK, mark.svg(a.color, 64), a.check)
    ok &= patch_rust(a.color, a.check)
    MD_DARK_SINK.parent.mkdir(parents=True, exist_ok=True)
    ok &= write(MD_DARK_SINK, mark.svg(README_DARK_INK, 32), a.check)
    ok &= write(MD_LIGHT_SINK, mark.svg(README_LIGHT_INK, 32), a.check)

    with tempfile.TemporaryDirectory() as td:
        tmp = Path(td)
        ok &= write_bytes(ICO_SINK, build_ico(a.color, tmp), a.check)
        ok &= write_bytes(MAC_SINK,
                          mark.render(a.color, 1024, tmp / "mac.png").read_bytes(),
                          a.check)

    if a.check and not ok:
        print("\nassets are stale: run scripts/logo/build.py")
    sys.exit(0 if ok else 1)


if __name__ == "__main__":
    main()
