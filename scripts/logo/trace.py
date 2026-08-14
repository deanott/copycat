#!/usr/bin/env python3
"""Trace the pen sketch to a centreline path.

    python3 scripts/logo/trace.py sketch.png

potrace alone is the wrong tool here. It traces *filled regions*, so on line art
it returns a closed loop around each stroke -- an outline of the ink, two
contours where the drawing has one line. What we want is the line the pen
actually travelled, so the pipeline is:

  1. isolate the ink by colour, not luminance. The photo has a strong lighting
     gradient across the paper and a green background darker than the paper, so
     any global threshold either eats the ink or keeps the background. Purple
     ink is the one thing in frame where (R+B)/2 clearly exceeds G.
  2. split the components: one long thin outline, plus the eyes and nose, which
     are blobs and must not be thinned into sticks.
  3. Zhang-Suen thinning of the outline to a 1px skeleton.
  4. walk the skeleton from an endpoint into an ordered polyline, pruning the
     short spurs thinning always leaves at corners and stroke ends.
  5. Douglas-Peucker to the vertices the drawing actually has.

The blobs stay blobs: their centroid and equivalent radius become the eyes, and
the nose keeps its own outline.
"""
import sys
sys.setrecursionlimit(10000)
from pathlib import Path

import numpy as np
from PIL import Image
from scipy import ndimage


def ink_mask(path, purple=8, dark=95, crop=0.75):
    """Ink as a boolean array. See (1) above for why this is a colour test.

    Neither half of the test works alone, which is the whole difficulty here:

      purple = (R+B)/2 - G     ink ~+16, paper ~0, green felt ~-35
      luminance                ink ~30,  paper ~130, green felt ~50

    Purpleness alone keeps the reddish threads in the bedding at the bottom of
    the frame; darkness alone keeps the green felt and the shadow under the
    paper. Requiring both leaves only the pen. `crop` drops the bottom of the
    frame outright, since nothing below the paper is ever wanted.
    """
    im = np.asarray(Image.open(path).convert("RGB")).astype(np.int16)
    r, g, b = im[..., 0], im[..., 1], im[..., 2]
    m = (((r + b) / 2 - g) > purple) & (im.mean(2) < dark)
    m[int(m.shape[0] * crop):] = False
    return m


def zhang_suen(img):
    """Thin a boolean image to a 1px skeleton. Vectorised, so the iteration
    count is what costs, not the pixel count."""
    img = img.copy().astype(np.uint8)

    def nbrs(a):
        # P2..P9 clockwise from north, as the algorithm names them.
        return [np.roll(np.roll(a, dy, 0), dx, 1) for dy, dx in
                ((1, 0), (1, -1), (0, -1), (-1, -1), (-1, 0), (-1, 1), (0, 1), (1, 1))]

    while True:
        changed = False
        for step in (0, 1):
            p = nbrs(img)
            B = sum(p)
            # A = number of 0->1 transitions going round the ring.
            ring = p + [p[0]]
            A = sum(((ring[i] == 0) & (ring[i + 1] == 1)).astype(np.uint8)
                    for i in range(8))
            if step == 0:
                c1, c2 = p[0] * p[2] * p[4], p[2] * p[4] * p[6]
            else:
                c1, c2 = p[0] * p[2] * p[6], p[0] * p[4] * p[6]
            kill = (img == 1) & (B >= 2) & (B <= 6) & (A == 1) & (c1 == 0) & (c2 == 0)
            if kill.any():
                img[kill] = 0
                changed = True
        if not changed:
            return img.astype(bool)


NB8 = [(-1, -1), (-1, 0), (-1, 1), (0, -1), (0, 1), (1, -1), (1, 0), (1, 1)]


def walk(skel):
    """Order a skeleton into its longest path, which is the pen stroke.

    Double BFS: farthest pixel from any start, then farthest from *that*, then
    backtrack the parent chain. Linear in skeleton size, and correct because a
    thinned open stroke is a tree -- the spurs thinning leaves at corners are
    just short branches, and the longest path cannot run through one. An
    exhaustive DFS gets the same answer and takes exponential time doing it.
    """
    pts = {(int(y), int(x)) for y, x in zip(*np.nonzero(skel))}

    def nb(p):
        return [(p[0] + dy, p[1] + dx) for dy, dx in NB8 if (p[0] + dy, p[1] + dx) in pts]

    def bfs(start):
        parent = {start: None}
        frontier, last = [start], start
        while frontier:
            nxt = []
            for p in frontier:
                for q in nb(p):
                    if q not in parent:
                        parent[q] = p
                        nxt.append(q)
            if nxt:
                last = nxt[-1]
            frontier = nxt
        return last, parent

    a, _ = bfs(next(iter(pts)))
    b, parent = bfs(a)
    path, p = [], b
    while p is not None:
        path.append(p)
        p = parent[p]
    return path[::-1]


def dp(pts, eps):
    """Douglas-Peucker. Returns the vertices the polyline really has."""
    if len(pts) < 3:
        return pts
    a, b = np.array(pts[0], float), np.array(pts[-1], float)
    ab = b - a
    n = np.hypot(*ab) or 1.0
    d = [abs(np.cross(ab, np.array(p, float) - a)) / n for p in pts]
    i = int(np.argmax(d))
    if d[i] <= eps:
        return [pts[0], pts[-1]]
    return dp(pts[:i + 1], eps)[:-1] + dp(pts[i:], eps)


def main():
    src = Path(sys.argv[1])
    mask = ink_mask(src)
    lab, n = ndimage.label(mask, structure=np.ones((3, 3)))
    sizes = ndimage.sum(mask, lab, range(1, n + 1))
    order = np.argsort(sizes)[::-1]
    print(f"{n} ink components; largest {sizes[order[0]]:.0f} px")

    outline = lab == (order[0] + 1)
    blobs = [(lab == (i + 1)) for i in order[1:] if sizes[i] > 300]

    ys, xs = np.nonzero(mask)
    y0, y1, x0, x1 = ys.min(), ys.max(), xs.min(), xs.max()
    print(f"ink bbox: x {x0}..{x1} ({x1 - x0}), y {y0}..{y1} ({y1 - y0}), "
          f"aspect {(x1 - x0) / (y1 - y0):.2f}")

    skel = zhang_suen(outline)
    print(f"skeleton {skel.sum()} px")
    path = walk(skel)
    print(f"walked {len(path)} px")
    simp = dp(path, 4.0)
    print(f"simplified to {len(simp)} vertices")

    # Normalise into the 0..100 box the variants use: one uniform scale, the
    # mark centred, 2 units of margin -- identical to the hand transcription so
    # the two are directly comparable.
    span = max(x1 - x0, y1 - y0)
    s = 96.0 / span
    ox = 2 + (96 - (x1 - x0) * s) / 2
    oy = 2 + (96 - (y1 - y0) * s) / 2
    conv = lambda p: ((p[1] - x0) * s + ox, (p[0] - y0) * s + oy)

    d = "M" + " L".join(f"{x:.2f},{y:.2f}" for x, y in map(conv, simp))
    print("\n--- traced centreline, 0..100 box ---")
    print(d)

    print("\n--- blobs (eyes / nose) ---")
    for b in blobs:
        by, bx = np.nonzero(b)
        cy, cx = by.mean(), bx.mean()
        r = np.sqrt(b.sum() / np.pi)
        x, y = conv((cy, cx))
        print(f'<circle cx="{x:.2f}" cy="{y:.2f}" r="{r * s:.2f}"/>   ({b.sum():.0f} px)')

    Image.fromarray((skel * 255).astype(np.uint8)).save(src.with_name("skeleton.png"))
    print(f"\nwrote {src.with_name('skeleton.png')}")


if __name__ == "__main__":
    main()
