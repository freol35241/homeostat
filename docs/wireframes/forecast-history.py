#!/usr/bin/env python3
"""Generates forecast-history.svg: the three ways to plot stored forecasts.

A forecast series is not a series — it is a 2-D field, `(issued, valid) ->
value`, because the house says something about the same future instant
many times over. Any plot of it must collapse one axis, and there are
exactly three ways to do that. The sheet draws the field once, then the
three slices, on one shared synthetic dataset so they can be compared as
answers to different questions rather than as rival styles.

Run from anywhere: python3 forecast-history.py — output lands next to the
script.

Colour discipline (docs/brand/README.md, and the dataviz form heuristic):
the OUTCOME is the point and the forecast is what is being examined, so
the two are a validated two-hue categorical pair, direct-labelled, never
a legend box. Where many issues are drawn at once they are a sequential
ramp on issue age — older paler — because issue time is ordered, not an
identity: a rainbow of twenty issues would be unreadable and would fail
every colourblind check.
"""

import math

# ---- palette ---------------------------------------------------------------
# The two series hues are brand-family and validated together (chroma
# floor, CVD separation, contrast vs surface all pass); the literal brand
# green #1F5E4A reads gray as a categorical hue, so it is lifted here.
INK = "#18181b"
SUB = "#52525b"
MUT = "#a1a1aa"
BORDER = "#e4e4e7"
GRID = "#ececee"
SURFACE = "#ffffff"
CANVAS = "#fafafa"
ACTUAL = "#1f8a66"   # what happened
FCAST = "#c7761a"    # what was predicted
SANS = "DejaVu Sans, sans-serif"

W, H = 900, 1180
PAD = 28


# ---- the synthetic field ---------------------------------------------------
def truth(t: float) -> float:
    """What actually happened, in hours since the sheet's origin."""
    return 1.0 + 0.55 * math.sin(2 * math.pi * (t - 6) / 24) + 0.22 * math.sin(2 * math.pi * t / 8)


def forecast(issued: float, valid: float) -> float:
    """What the house said at `issued` about `valid`. Error grows with lead
    time and vanishes at zero lead, and each issue carries its own bias —
    which is what makes successive issues disagree, and the whole reason
    the field has two axes."""
    lead = max(valid - issued, 0.0)
    bias = 0.62 * math.sin(issued / 6.5 + 0.4)
    return truth(valid) + bias * (lead / 24.0) ** 1.15


ISSUE_EVERY = 3.0     # a new forecast every three hours
HORIZON = 24.0        # each one reaches a day ahead
SPAN = 72.0           # three days of history on the sheet


def esc(text: str) -> str:
    return text.replace("&", "&amp;").replace("<", "&lt;").replace(">", "&gt;")


def text(x, y, s, size=12, fill=SUB, weight="normal", anchor="start"):
    return (
        f'<text x="{x:.1f}" y="{y:.1f}" font-family="{SANS}" font-size="{size}" '
        f'fill="{fill}" font-weight="{weight}" text-anchor="{anchor}">{esc(s)}</text>'
    )


def panel(x, y, w, h, title, subtitle):
    out = [
        f'<rect x="{x}" y="{y}" width="{w}" height="{h}" rx="10" fill="{SURFACE}" stroke="{BORDER}"/>',
        text(x + 18, y + 26, title, 14, INK, "bold"),
        text(x + 18, y + 46, subtitle, 12, SUB),
    ]
    return out


def path(points) -> str:
    return "M " + " L ".join(f"{px:.1f} {py:.1f}" for px, py in points)


# ---- panel 1: the field, and the three cuts through it ---------------------
def field_panel(x, y, w, h):
    out = panel(x, y, w, h, "The data is a field, not a series",
                "Every cell is one forecast: what was said at an issue time about a valid time. "
                "Each plot below cuts it a different way.")
    gx, gy = x + 150, y + 74
    cols, rows = 16, 8
    cw, ch = 24, 20
    # the grid of issues (rows) x valid times (columns)
    for r in range(rows):
        for c in range(cols):
            # a forecast only speaks about the future, so the field is
            # upper-triangular: nothing is said about the past.
            live = c >= r
            fill = "#f4f6f5" if live else "#fbfbfc"
            out.append(
                f'<rect x="{gx + c * cw}" y="{gy + r * ch}" width="{cw - 2}" height="{ch - 2}" '
                f'rx="2" fill="{fill}" stroke="{GRID}"/>'
            )
    # A — one row: a single issue's whole horizon
    out.append(f'<rect x="{gx + 3 * cw}" y="{gy + 3 * ch}" width="{(cols - 3) * cw - 2}" '
               f'height="{ch - 2}" rx="2" fill="{FCAST}" opacity=".85"/>')
    # B — one column: every issue's view of one instant. It spans the
    # grid and stops there; an earlier version ran a fixed 12 rows deep
    # and spilled out of the panel.
    col = 11
    for r in range(rows):
        out.append(f'<rect x="{gx + col * cw}" y="{gy + r * ch}" width="{cw - 2}" '
                   f'height="{ch - 2}" rx="2" fill="{ACTUAL}" opacity=".85"/>')
    # C — the diagonal: a fixed distance ahead
    diag_last = 0
    for r in range(rows):
        c = r + 4
        if c < cols:
            diag_last = r
            out.append(f'<rect x="{gx + c * cw}" y="{gy + r * ch}" width="{cw - 2}" '
                       f'height="{ch - 2}" rx="2" fill="{INK}" opacity=".62"/>')
    right = gx + cols * cw + 16
    out += [
        text(gx, gy - 10, "valid time  →", 11, MUT),
        text(x + 18, gy + 14, "issue", 11, MUT),
        text(x + 18, gy + 28, "time", 11, MUT),
        text(x + 18, gy + 42, "↓", 11, MUT),
        # Each label sits on the feature it names.
        text(right, gy + 3 * ch + 14, "A  one issue’s horizon", 11, FCAST, "bold"),
        text(gx + col * cw + cw / 2, gy + rows * ch + 8, "B", 11, ACTUAL, "bold", "middle"),
        text(gx + col * cw + cw / 2 + 46, gy + rows * ch + 8, "one instant", 10, ACTUAL),
        text(right, gy + diag_last * ch + 14, "C  one lead time", 11, INK, "bold"),
        text(gx, gy + rows * ch + 24,
             "Pale cells are empty by construction: a forecast never speaks about the past.", 11, MUT),
    ]
    return out


# ---- the three slice plots -------------------------------------------------
def plot_frame(x, y, w, h, ylo, yhi, xlo, xhi):
    """Returns (svg, to_px). Gridlines and a baseline, nothing else — the
    axes stay recessive so the marks carry the reading."""
    out = []
    for i in range(3):
        gy = y + i * (h / 2.0)
        out.append(f'<line x1="{x}" y1="{gy:.1f}" x2="{x + w}" y2="{gy:.1f}" '
                   f'stroke="{GRID}" stroke-width="1"/>')

    def to_px(t, v):
        return (x + (t - xlo) / (xhi - xlo) * w,
                y + (1 - (v - ylo) / (yhi - ylo)) * h)

    return out, to_px


def series(points, colour, width=2.0, dash=None, opacity=1.0):
    d = f' stroke-dasharray="{dash}"' if dash else ""
    return (f'<path d="{path(points)}" fill="none" stroke="{colour}" stroke-width="{width}" '
            f'stroke-linecap="round" stroke-linejoin="round" opacity="{opacity}"{d}/>')


def slice_a(x, y, w, h):
    out = panel(x, y, w, h, "A — fix the issue time  (a row)",
                "“What did we think then?”  One issue’s whole horizon, "
                "drawn against what actually happened.")
    px, py, pw, ph = x + 60, y + 70, w - 130, h - 120
    frame, to = plot_frame(px, py, pw, ph, 0.0, 2.1, 0.0, SPAN)
    out += frame
    issues = [i * ISSUE_EVERY for i in range(int((SPAN - HORIZON) / ISSUE_EVERY))]
    issued = max(issues, key=lambda i: abs(forecast(i, i + HORIZON) - truth(i + HORIZON)))
    out.append(series([to(t, truth(t)) for t in [i * 0.5 for i in range(int(SPAN * 2) + 1)]], ACTUAL))
    horizon = [i * 0.5 for i in range(int(HORIZON * 2) + 1)]
    out.append(series([to(issued + o, forecast(issued, issued + o)) for o in horizon],
                      FCAST, 2.0, "5 4"))
    ix, _ = to(issued, 0)
    out.append(f'<line x1="{ix:.1f}" y1="{py}" x2="{ix:.1f}" y2="{py + ph}" '
               f'stroke="{MUT}" stroke-width="1" stroke-dasharray="2 3"/>')
    lx, ly = to(issued + HORIZON, forecast(issued, issued + HORIZON))
    out.append(text(lx + 10, ly + 4, "forecast", 11, FCAST, "bold"))
    ax, ay = to(SPAN * 0.22, truth(SPAN * 0.22))
    out.append(text(ax, ay - 12, "actual", 11, ACTUAL, "bold", "middle"))
    out.append(text(ix + 4, py - 6, "issued", 10, MUT))
    out.append(text(px, py + ph + 20, "valid time →", 11, MUT))
    out.append(text(x + w - 18, y + h - 18,
                    "served today by  ?at=<ts>", 11, MUT, "normal", "end"))
    return out


def slice_b(x, y, w, h):
    out = panel(x, y, w, h, "B — fix the valid time  (a column)",
                "“How did our view of one moment settle as it approached?”  "
                "The x axis is issue time, not valid time.")
    px, py, pw, ph = x + 60, y + 70, w - 130, h - 120
    valid = 54.0
    issues = [valid - HORIZON + i * ISSUE_EVERY for i in range(int(HORIZON / ISSUE_EVERY) + 1)]
    # This panel gets its own y range. On the shared scale a day of
    # opinions about one instant is a flat line, and the disagreement —
    # which is the entire subject here — disappears.
    vals = [forecast(i, valid) for i in issues] + [truth(valid)]
    lo, hi = min(vals), max(vals)
    margin = (hi - lo) * 0.35 or 0.1
    frame, to = plot_frame(px, py, pw, ph, lo - margin, hi + margin, valid - HORIZON, valid)
    out += frame
    out.append(series([to(i, forecast(i, valid)) for i in issues], FCAST, 2.0))
    for i in issues:
        cx, cy = to(i, forecast(i, valid))
        out.append(f'<circle cx="{cx:.1f}" cy="{cy:.1f}" r="3.5" fill="{FCAST}" '
                   f'stroke="{SURFACE}" stroke-width="2"/>')
    ty = to(valid, truth(valid))[1]
    out.append(f'<line x1="{px}" y1="{ty:.1f}" x2="{px + pw}" y2="{ty:.1f}" '
               f'stroke="{ACTUAL}" stroke-width="2"/>')
    out.append(text(px + pw + 8, ty + 4, "actual", 11, ACTUAL, "bold"))
    fx, fy = to(issues[1], forecast(issues[1], valid))
    out.append(text(fx + 10, fy + 22, "successive forecasts", 11, FCAST, "bold"))
    out.append(text(px, py + ph + 20, "issue time →   (24 h before … the moment itself)", 11, MUT))
    out.append(text(x + w - 18, y + h - 18,
                    "served today by  ?valid_from=..;valid_to=..  over a narrow window", 11, MUT,
                    "normal", "end"))
    return out


def slice_c(x, y, w, h):
    out = panel(x, y, w, h, "C — fix the lead time  (a diagonal)",
                "“How good is the four-hours-ahead view a controller actually consumes?”")
    px, py, pw, ph = x + 60, y + 70, w - 130, h - 120
    frame, to = plot_frame(px, py, pw, ph, 0.0, 2.1, 0.0, SPAN)
    out += frame
    lead = 4.0
    ts = [lead + i * 0.5 for i in range(int((SPAN - lead) * 2) + 1)]
    out.append(series([to(t, truth(t)) for t in ts], ACTUAL))
    out.append(series([to(t, forecast(t - lead, t)) for t in ts], FCAST, 2.0, "5 4"))
    lx, ly = to(SPAN, forecast(SPAN - lead, SPAN))
    out.append(text(lx - 6, ly + 16, "forecast, 4 h ahead", 11, FCAST, "bold", "end"))
    ax, ay = to(SPAN * 0.30, truth(SPAN * 0.30))
    out.append(text(ax, ay - 22, "actual", 11, ACTUAL, "bold", "middle"))
    out.append(text(px, py + ph + 20, "valid time →", 11, MUT))
    out.append(text(x + w - 18, y + h - 18,
                    "NOT served well today — would want a  ?lead=<seconds>  shape", 11, FCAST,
                    "normal", "end"))
    return out


def main() -> None:
    parts = [
        f'<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 {W} {H}" width="{W}" height="{H}">',
        f'<rect width="{W}" height="{H}" fill="{CANVAS}"/>',
        text(PAD, 40, "Plotting stored forecasts: three slices of one field", 19, INK, "bold"),
        text(PAD, 62,
             "Same synthetic data throughout — a new forecast every 3 h, each reaching 24 h "
             "ahead, over three days.", 12, SUB),
    ]
    parts += field_panel(PAD, 80, W - 2 * PAD, 280)
    parts += slice_a(PAD, 376, W - 2 * PAD, 256)
    parts += slice_b(PAD, 648, W - 2 * PAD, 256)
    parts += slice_c(PAD, 920, W - 2 * PAD, 240)
    parts.append("</svg>")
    out = "\n".join(parts) + "\n"
    target = __file__.rsplit("/", 1)[0] + "/forecast-history.svg"
    with open(target, "w") as handle:
        handle.write(out)
    print(f"wrote {target} ({len(out)} bytes)")


if __name__ == "__main__":
    main()
