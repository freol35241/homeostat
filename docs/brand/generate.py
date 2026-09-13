"""The Homeostat mark: a pine house, the step response cut into it, the amber set point.

One geometry, three SVGs. The monochrome mark is a real cutout — the
response curve's stroke outline as a polygon, even-odd filled — so it is one
path, which is what favicons, themed icons and the app's notification glyph
need (the app generates its vector drawables from the same numbers).

    python3 docs/brand/generate.py docs/brand
"""
import math

HOUSE_PTS = [(54, 12), (96, 50), (96, 96), (12, 96), (12, 50)]
RADIUS = 4
CURVE = [(24, 84), (36, 84), (36, 44), (47, 44), (58, 44), (58, 66), (66, 68), (72, 69.5), (76, 64), (84, 64)]
CURVE_W = 7
LINE = ((24, 64), (84, 64))
LINE_W = 5

PINE, PINE_DARK, TRACE, AMBER = "#1F5E4A", "#2A7A61", "#F2EFE6", "#E8A33D"


def house_path():
    d = []
    n = len(HOUSE_PTS)
    for i, p in enumerate(HOUSE_PTS):
        a, b = HOUSE_PTS[i - 1], HOUSE_PTS[(i + 1) % n]
        def toward(q):
            dx, dy = q[0] - p[0], q[1] - p[1]
            L = math.hypot(dx, dy)
            return (p[0] + dx / L * RADIUS, p[1] + dy / L * RADIUS)
        s1, s2 = toward(a), toward(b)
        d.append(("M" if i == 0 else "L") + f"{s1[0]:.2f},{s1[1]:.2f}")
        d.append(f"Q{p[0]},{p[1]} {s2[0]:.2f},{s2[1]:.2f}")
    return " ".join(d) + " Z"


def curve_d():
    p = CURVE
    return f"M{p[0][0]},{p[0][1]} " + " ".join(
        f"C{p[i][0]},{p[i][1]} {p[i+1][0]},{p[i+1][1]} {p[i+2][0]},{p[i+2][1]}" for i in range(1, len(p) - 2, 3)
    )


def bezier(p0, p1, p2, p3, t):
    u = 1 - t
    return (u**3*p0[0] + 3*u*u*t*p1[0] + 3*u*t*t*p2[0] + t**3*p3[0],
            u**3*p0[1] + 3*u*u*t*p1[1] + 3*u*t*t*p2[1] + t**3*p3[1])


def sample_curve(n=24):
    pts = []
    segs = [(CURVE[i], CURVE[i+1], CURVE[i+2], CURVE[i+3]) for i in range(0, len(CURVE) - 3, 3)]
    for s in segs:
        for k in range(n):
            pts.append(bezier(*s, k / n))
    pts.append(CURVE[-1])
    return pts


def outline(pts, w):
    """Polygon of a round-capped stroke of width w along pts."""
    r = w / 2
    left, right = [], []
    for i, p in enumerate(pts):
        a = pts[max(i - 1, 0)]; b = pts[min(i + 1, len(pts) - 1)]
        dx, dy = b[0] - a[0], b[1] - a[1]
        L = math.hypot(dx, dy) or 1
        nx, ny = -dy / L * r, dx / L * r
        left.append((p[0] + nx, p[1] + ny)); right.append((p[0] - nx, p[1] - ny))
    def cap(c, tangent, steps=8):
        # A semicircle around c, bulging along the tangent: from the left
        # offset, through c + tangent * r, to the right offset.
        base = math.atan2(tangent[1], tangent[0])
        return [(c[0] + r * math.cos(base - math.pi / 2 + math.pi * k / steps),
                 c[1] + r * math.sin(base - math.pi / 2 + math.pi * k / steps)) for k in range(1, steps)]
    def tangent(a, b):
        dx, dy = b[0] - a[0], b[1] - a[1]; L = math.hypot(dx, dy) or 1
        return (dx / L, dy / L)
    t_end = tangent(pts[-2], pts[-1]); t_start = tangent(pts[1], pts[0])
    # left is p + n where n = (-dy, dx): the cap runs left -> tangent -> right.
    poly = left + cap(pts[-1], t_end)[::-1] + right[::-1] + cap(pts[0], t_start)[::-1]
    return "M" + " L".join(f"{x:.1f},{y:.1f}" for x, y in poly) + " Z"


def capsule():
    (x0, y), (x1, _) = LINE
    return outline([(x0, y), (x1, y)], LINE_W)


HOUSE = house_path()
CURVE_POLY = outline(sample_curve(), CURVE_W)
LINE_POLY = capsule()
CUTOUT = f"{HOUSE} {CURVE_POLY} {LINE_POLY}"


def svg_colour(house_fill):
    return f'''<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 108 108" width="108" height="108">
  <path d="{HOUSE}" fill="{house_fill}"/>
  <path d="M{LINE[0][0]},{LINE[0][1]} H{LINE[1][0]}" stroke="{AMBER}" stroke-width="{LINE_W}" stroke-linecap="round"/>
  <path d="{curve_d()}" fill="none" stroke="{TRACE}" stroke-width="{CURVE_W}" stroke-linecap="round" stroke-linejoin="round"/>
</svg>
'''


def svg_mono(fill="currentColor"):
    return f'''<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 108 108" width="108" height="108">
  <path d="{CUTOUT}" fill="{fill}" fill-rule="evenodd"/>
</svg>
'''


if __name__ == "__main__":
    import sys
    out = sys.argv[1]
    open(f"{out}/homeostat-mark.svg", "w").write(svg_colour(PINE))
    open(f"{out}/homeostat-mark-dark.svg", "w").write(svg_colour(PINE_DARK))
    open(f"{out}/homeostat-mark-mono.svg", "w").write(svg_mono())
    print("ok")
