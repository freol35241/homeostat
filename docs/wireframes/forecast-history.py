#!/usr/bin/env python3
"""Generates forecast-history.svg: how to plot the forecasts a store keeps.

Stored forecasts are not a series. They are a 2-D field, `(issued, valid)
-> value`, because the house says something about the same future instant
many times over. Any plot must collapse one axis, and there are exactly
three ways: fix the issue time and you have a ROW, fix the valid time a
COLUMN, fix the lead time a DIAGONAL.

The sheet's argument is that these are not rival charts. Draw every row
at once — the bundle — and the row and the diagonal are already inside
it, as a line and as a locus; they want highlighting, not a chart each.
Only the column needs its own, because its x axis is issue time, and that
is a different domain rather than a different style.

Run from anywhere: python3 forecast-history.py — output lands next to the
script.

Colour discipline (docs/brand/README.md, and the dataviz form heuristic):
the outcome is the point and the highlighted forecast is what is being
examined, so those two are a validated two-hue categorical pair,
direct-labelled, never a legend box. The bundle behind them is context
and recedes. Issue time is ORDERED, so were individual issues drawn they
would be a sequential ramp on age — never categorical hues: twenty of
those would be unreadable and would fail every colourblind check.
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
FCAST = "#c7761a"    # the forecast under examination
BAND = "#e8c9a4"     # every issue at once — context, and it recedes
BANDINK = "#a5741f"  # a label for the band, dark enough to read
SANS = "DejaVu Sans, sans-serif"

W, H = 900, 1150
PAD = 28

# ---- the synthetic field ---------------------------------------------------
ISSUE_EVERY = 3.0     # a new forecast every three hours
HORIZON = 24.0        # each one reaches a day ahead
SPAN = 72.0           # three days on the sheet
LEAD_C = 4.0          # the lead time panel C follows
# Only the stretch where every instant is covered by a full set of issues;
# before it the bundle would thin out for reasons that are the sheet's own
# arithmetic rather than anything about forecasting.
T0, T1 = HORIZON, SPAN


def truth(t: float) -> float:
    """What actually happened, in hours since the sheet's origin."""
    return 1.0 + 0.55 * math.sin(2 * math.pi * (t - 6) / 24) + 0.22 * math.sin(2 * math.pi * t / 8)


def forecast(issued: float, valid: float) -> float:
    """What the house said at `issued` about `valid`.

    Two error terms, because one would misdraw the picture. A bias that
    grows with lead time and differs per issue is what makes successive
    issues disagree — the reason the field has two axes at all. A small
    term that does NOT vanish at zero lead keeps the bundle straddling
    the outcome: without it every issue is exactly right about the moment
    it was made, so the band hugs the actual on one edge and fans only to
    the other, which is an artifact of the toy rather than anything true
    about forecasts.
    """
    lead = max(valid - issued, 0.0)
    bias = 0.62 * math.sin(issued / 6.5 + 0.4)
    jitter = 0.05 * math.sin(valid * 2.3 + issued * 0.7)
    return truth(valid) + bias * (lead / 24.0) ** 1.15 + jitter


def issues_covering(t: float) -> list:
    """Every issue that said something about `t` — one column of the field."""
    first = max(math.ceil((t - HORIZON) / ISSUE_EVERY) * ISSUE_EVERY, 0.0)
    out, i = [], first
    while i <= t:
        out.append(i)
        i += ISSUE_EVERY
    return out or [t]


def grid(step=0.5, lo=T0, hi=T1) -> list:
    return [lo + k * step for k in range(int((hi - lo) / step) + 1)]


# ---- svg helpers -----------------------------------------------------------
def esc(text: str) -> str:
    return text.replace("&", "&amp;").replace("<", "&lt;").replace(">", "&gt;")


def text(x, y, s, size=12, fill=SUB, weight="normal", anchor="start"):
    return (
        f'<text x="{x:.1f}" y="{y:.1f}" font-family="{SANS}" font-size="{size}" '
        f'fill="{fill}" font-weight="{weight}" text-anchor="{anchor}">{esc(s)}</text>'
    )


def panel(x, y, w, h, title, subtitle=None, size=14):
    out = [
        f'<rect x="{x}" y="{y}" width="{w}" height="{h}" rx="10" fill="{SURFACE}" stroke="{BORDER}"/>',
        text(x + 18, y + 26, title, size, INK, "bold"),
    ]
    if subtitle:
        out.append(text(x + 18, y + 46, subtitle, 12, SUB))
    return out


def path(points) -> str:
    return "M " + " L ".join(f"{px:.1f} {py:.1f}" for px, py in points)


def series(points, colour, width=2.0, dash=None, opacity=1.0):
    d = f' stroke-dasharray="{dash}"' if dash else ""
    return (f'<path d="{path(points)}" fill="none" stroke="{colour}" stroke-width="{width}" '
            f'stroke-linecap="round" stroke-linejoin="round" opacity="{opacity}"{d}/>')


def plot_frame(x, y, w, h, ylo, yhi, xlo, xhi):
    out = [
        f'<line x1="{x}" y1="{y + i * (h / 2.0):.1f}" x2="{x + w}" y2="{y + i * (h / 2.0):.1f}" '
        f'stroke="{GRID}" stroke-width="1"/>' for i in range(3)
    ]

    def to_px(t, v):
        return (x + (t - xlo) / (xhi - xlo) * w,
                y + (1 - (v - ylo) / (yhi - ylo)) * h)

    return out, to_px


YLO, YHI = 0.0, 2.15


def bundle_band(to, ts) -> str:
    """The spread of every issue's opinion about each instant, as one
    filled shape: a band rather than a line per issue, because twenty
    strokes is ink without a reading. What it costs is knowing WHICH
    issue said what — the inspection mode, not the default."""
    hi = [to(t, max(forecast(i, t) for i in issues_covering(t))) for t in ts]
    lo = [to(t, min(forecast(i, t) for i in issues_covering(t))) for t in ts]
    d = path(hi) + " L " + " L ".join(f"{px:.1f} {py:.1f}" for px, py in reversed(lo)) + " Z"
    return f'<path d="{d}" fill="{BAND}" opacity=".6" stroke="none"/>'


# ---- panel 1: the field ----------------------------------------------------
def field_panel(x, y, w, h):
    out = panel(x, y, w, h, "The data is a field, not a series",
                "Every cell is one forecast: what was said at an issue time about a valid time.")
    gx, gy = x + 150, y + 78
    cols, rows = 16, 8
    cw, ch = 24, 20
    for r in range(rows):
        for c in range(cols):
            # A forecast only speaks about the future, so the field is
            # upper-triangular: nothing is said about the past.
            fill = "#f4f6f5" if c >= r else "#fbfbfc"
            out.append(
                f'<rect x="{gx + c * cw}" y="{gy + r * ch}" width="{cw - 2}" height="{ch - 2}" '
                f'rx="2" fill="{fill}" stroke="{GRID}"/>'
            )
    out.append(f'<rect x="{gx + 3 * cw}" y="{gy + 3 * ch}" width="{(cols - 3) * cw - 2}" '
               f'height="{ch - 2}" rx="2" fill="{FCAST}" opacity=".85"/>')
    col = 11
    for r in range(rows):
        out.append(f'<rect x="{gx + col * cw}" y="{gy + r * ch}" width="{cw - 2}" '
                   f'height="{ch - 2}" rx="2" fill="{ACTUAL}" opacity=".85"/>')
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
        text(right, gy + 3 * ch + 14, "A  a row", 11, FCAST, "bold"),
        text(right, gy + 3 * ch + 30, "one issue’s horizon", 11, SUB),
        text(right, gy + diag_last * ch + 6, "C  a diagonal", 11, INK, "bold"),
        text(right, gy + diag_last * ch + 22, "one lead time", 11, SUB),
        text(gx + col * cw + cw / 2, gy + rows * ch + 16, "B", 11, ACTUAL, "bold", "middle"),
        text(gx + col * cw + cw / 2 + 12, gy + rows * ch + 16,
             "a column — one instant", 11, SUB),
        text(gx, gy + rows * ch + 44,
             "D — every row at once is the bundle below, and A and C are already inside it.",
             12, INK, "bold"),
    ]
    return out


# ---- panel 2: D, the bundle ------------------------------------------------
def bundle_panel(x, y, w, h):
    out = panel(x, y, w, h, "D — every issue at once  (the field, drawn)",
                "The width is how much successive forecasts disagreed about each moment — "
                "and it grows with lead time.")
    px, py, pw, ph = x + 56, y + 74, w - 150, h - 126
    frame, to = plot_frame(px, py, pw, ph, YLO, YHI, T0, T1)
    out += frame
    ts = grid()
    out.append(bundle_band(to, ts))
    out.append(series([to(t, truth(t)) for t in ts], ACTUAL))
    at = T0 + 15
    bx, by = to(at, max(forecast(i, at) for i in issues_covering(at)))
    ax, ay = to(T0 + 2, truth(T0 + 2))
    out += [
        text(bx, by - 10, "every issue", 11, BANDINK, "bold"),
        text(ax, ay - 14, "actual", 11, ACTUAL, "bold"),
        text(px, py + ph + 22, "valid time →", 11, MUT),
        text(x + w - 18, y + h - 16,
             "one query — ?valid_from=..;valid_to=..  — and A, B and C all fall out of it",
             11, MUT, "normal", "end"),
    ]
    return out


# ---- panels 3 & 4: A and C as highlights on D ------------------------------
def highlight_panel(x, y, w, h, which):
    if which == "A":
        title, sub = "A  —  highlight one row", "“What did we think then?”"
        foot = "?at=<ts>  — or just filter the bundle"
    else:
        title, sub = "C  —  highlight one diagonal", "“The 4 h-ahead view a controller uses.”"
        foot = "free from the bundle over a short window"
    out = panel(x, y, w, h, title, sub, 13)
    px, py, pw, ph = x + 34, y + 68, w - 58, h - 116
    frame, to = plot_frame(px, py, pw, ph, YLO, YHI, T0, T1)
    out += frame
    ts = grid()
    out.append(bundle_band(to, ts))
    out.append(series([to(t, truth(t)) for t in ts], ACTUAL, 1.6, None, 0.5))
    if which == "A":
        candidates = [k * ISSUE_EVERY for k in range(int(T1 / ISSUE_EVERY) + 1)]
        candidates = [i for i in candidates if T0 <= i <= T1 - HORIZON]
        issued = max(candidates,
                     key=lambda i: abs(forecast(i, i + HORIZON) - truth(i + HORIZON)))
        out.append(series([to(t, forecast(issued, t)) for t in grid(0.5, issued, issued + HORIZON)],
                          FCAST, 2.2))
        ix, _ = to(issued, 0)
        out.append(f'<line x1="{ix:.1f}" y1="{py}" x2="{ix:.1f}" y2="{py + ph}" '
                   f'stroke="{MUT}" stroke-width="1" stroke-dasharray="2 3"/>')
        out.append(text(ix + 4, py - 6, "issued", 10, MUT))
    else:
        out.append(series([to(t, forecast(t - LEAD_C, t)) for t in ts if t >= T0 + LEAD_C],
                          FCAST, 2.2))
    out.append(text(px, py + ph + 20, "valid time →", 11, MUT))
    out.append(text(x + w - 16, y + h - 14, foot, 10, MUT, "normal", "end"))
    return out


# ---- panel 5: B, the one that needs its own axis ---------------------------
def column_panel(x, y, w, h):
    out = panel(x, y, w, h, "B — fix the valid time  (a column)",
                "“How did our view of one moment settle as it approached?”")
    # A second short line rather than one that runs off the panel.
    out.append(text(x + 18, y + 64,
                    "The x axis is ISSUE time — a different domain, so this one needs its "
                    "own chart.", 11, MUT))
    px, py, pw, ph = x + 56, y + 90, w - 150, h - 142
    valid = 54.0
    issues = issues_covering(valid)
    # Its own y range: on the shared scale a day of opinions about one
    # instant is a flat line, and the disagreement is the whole subject.
    vals = [forecast(i, valid) for i in issues] + [truth(valid)]
    lo, hi = min(vals), max(vals)
    margin = (hi - lo) * 0.4 or 0.1
    frame, to = plot_frame(px, py, pw, ph, lo - margin, hi + margin, issues[0], valid)
    out += frame
    out.append(series([to(i, forecast(i, valid)) for i in issues], FCAST, 2.0))
    for i in issues:
        cx, cy = to(i, forecast(i, valid))
        out.append(f'<circle cx="{cx:.1f}" cy="{cy:.1f}" r="3.5" fill="{FCAST}" '
                   f'stroke="{SURFACE}" stroke-width="2"/>')
    ty = to(valid, truth(valid))[1]
    out.append(f'<line x1="{px}" y1="{ty:.1f}" x2="{px + pw}" y2="{ty:.1f}" '
               f'stroke="{ACTUAL}" stroke-width="2"/>')
    fx, fy = to(issues[1], forecast(issues[1], valid))
    out += [
        text(px + pw + 8, ty + 4, "actual", 11, ACTUAL, "bold"),
        text(fx + 10, fy + 22, "successive forecasts", 11, FCAST, "bold"),
        text(px, py + ph + 22, "issue time →   (24 h before … the moment itself)", 11, MUT),
        text(x + w - 18, y + h - 16,
             "?valid_from=..;valid_to=..  over a narrow window", 11, MUT, "normal", "end"),
    ]
    return out


def main() -> None:
    half = (W - 2 * PAD - 16) / 2
    parts = [
        f'<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 {W} {H}" width="{W}" height="{H}">',
        f'<rect width="{W}" height="{H}" fill="{CANVAS}"/>',
        text(PAD, 40, "Plotting stored forecasts", 19, INK, "bold"),
        text(PAD, 62,
             "One bundle carries most of it. Same synthetic data throughout — a new forecast "
             "every 3 h, each reaching 24 h ahead.", 12, SUB),
    ]
    parts += field_panel(PAD, 80, W - 2 * PAD, 296)
    parts += bundle_panel(PAD, 392, W - 2 * PAD, 226)
    parts += highlight_panel(PAD, 634, half, 220, "A")
    parts += highlight_panel(PAD + half + 16, 634, half, 220, "C")
    parts += column_panel(PAD, 870, W - 2 * PAD, 226)
    parts.append("</svg>")
    out = "\n".join(parts) + "\n"
    target = __file__.rsplit("/", 1)[0] + "/forecast-history.svg"
    with open(target, "w") as handle:
        handle.write(out)
    print(f"wrote {target} ({len(out)} bytes)")


if __name__ == "__main__":
    main()
