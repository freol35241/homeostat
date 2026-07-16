#!/usr/bin/env python3
"""Generates the dashboard wireframe SVGs in this directory.

Sheets:
  A — concept: spatial-first room grid (exploration)
  B — concept: regulator view, signals + setpoints first (exploration)
  V — widget vocabulary: the manifest -> widget generation function
  H — the direction of record: deviation-first "Now" (embedded in the
      README; map and person entities are settled design, not yet built)

Run from anywhere: python3 generate.py — output lands next to the script.
Wireframe discipline: grayscale UI, blue reserved for annotations only,
health states carried by shape + word (never color alone).
"""

import math

# ---- palette (wireframe) ----------------------------------------------------
INK = "#18181b"
SUB = "#52525b"
MUT = "#a1a1aa"
BORDER = "#d4d4d8"
FILL = "#f4f4f5"
SURFACE = "#ffffff"
CANVAS = "#fafafa"
ANNOT = "#2563eb"
MONO = "DejaVu Sans Mono, monospace"
SANS = "DejaVu Sans, sans-serif"


def esc(s):
    s = s.replace("&", "&amp;").replace("<", "&lt;").replace(">", "&gt;")
    # preserve leading-space indentation (SVG collapses whitespace)
    stripped = s.lstrip(" ")
    return " " * (len(s) - len(stripped)) + stripped


class SVG:
    def __init__(self, w, h):
        self.w, self.h = w, h
        self.parts = [
            f'<svg xmlns="http://www.w3.org/2000/svg" width="{w}" height="{h}" '
            f'viewBox="0 0 {w} {h}" font-family="{SANS}">',
            f'<rect width="{w}" height="{h}" fill="{CANVAS}"/>',
        ]

    def rect(self, x, y, w, h, fill=SURFACE, stroke=BORDER, rx=0, sw=1, dash=None):
        d = f' stroke-dasharray="{dash}"' if dash else ""
        s = f' stroke="{stroke}" stroke-width="{sw}"' if stroke else ""
        self.parts.append(
            f'<rect x="{x:.1f}" y="{y:.1f}" width="{w:.1f}" height="{h:.1f}" '
            f'rx="{rx}" fill="{fill}"{s}{d}/>'
        )

    def text(self, x, y, s, size=12, fill=INK, weight="normal", anchor="start",
             family=SANS, style=None, spacing=None):
        extra = ""
        if style:
            extra += f' font-style="{style}"'
        if spacing:
            extra += f' letter-spacing="{spacing}"'
        self.parts.append(
            f'<text x="{x:.1f}" y="{y:.1f}" font-size="{size}" fill="{fill}" '
            f'font-weight="{weight}" text-anchor="{anchor}" font-family="{family}"{extra}>'
            f"{esc(s)}</text>"
        )

    def line(self, x1, y1, x2, y2, stroke=BORDER, sw=1, dash=None):
        d = f' stroke-dasharray="{dash}"' if dash else ""
        self.parts.append(
            f'<line x1="{x1:.1f}" y1="{y1:.1f}" x2="{x2:.1f}" y2="{y2:.1f}" '
            f'stroke="{stroke}" stroke-width="{sw}"{d}/>'
        )

    def circle(self, cx, cy, r, fill=INK, stroke=None, sw=1):
        s = f' stroke="{stroke}" stroke-width="{sw}"' if stroke else ""
        self.parts.append(f'<circle cx="{cx:.1f}" cy="{cy:.1f}" r="{r}" fill="{fill}"{s}/>')

    def path(self, d, stroke=INK, sw=2, fill="none", dash=None):
        dd = f' stroke-dasharray="{dash}"' if dash else ""
        self.parts.append(
            f'<path d="{d}" stroke="{stroke}" stroke-width="{sw}" fill="{fill}" '
            f'stroke-linecap="round" stroke-linejoin="round"{dd}/>'
        )

    def save(self, path):
        self.parts.append("</svg>")
        with open(path, "w") as f:
            f.write("\n".join(self.parts))


# ---- shared components -------------------------------------------------------

def toggle(s, x, y, on=True):
    """Toggle pill, right edge at x. 34x18."""
    s.rect(x - 34, y, 34, 18, fill=INK if on else FILL,
           stroke=None if on else BORDER, rx=9)
    kx = x - 10 if on else x - 34 + 10
    s.circle(kx, y + 9, 6.5, fill=SURFACE if on else MUT)


def slider(s, x, y, w, frac, label, value):
    """Labelled slider row."""
    s.text(x, y + 4, label, size=10, fill=SUB)
    tx = x + 92
    tw = w - 92 - 44
    s.line(tx, y, tx + tw, y, stroke=BORDER, sw=3)
    s.line(tx, y, tx + tw * frac, y, stroke=SUB, sw=3)
    s.circle(tx + tw * frac, y, 6, fill=SURFACE, stroke=INK, sw=1.5)
    s.text(x + w, y + 4, value, size=10, fill=SUB, anchor="end")


def sparkline(s, x, y, w, h, seed=1, sw=1.5):
    pts = []
    n = 28
    for i in range(n):
        t = i / (n - 1)
        v = (math.sin(t * 5.1 + seed) * 0.5 + math.sin(t * 11.3 + seed * 2) * 0.28
             + math.sin(t * 2.2 + seed * 3) * 0.32)
        pts.append((x + t * w, y + h / 2 - v * h / 2.6))
    d = "M " + " L ".join(f"{px:.1f} {py:.1f}" for px, py in pts)
    s.path(d, stroke=SUB, sw=sw)
    s.circle(pts[-1][0], pts[-1][1], 2.5, fill=INK)


def stepper(s, x, y, value, w=86):
    """[ - ] value [ + ] control, right edge at x."""
    s.rect(x - w, y, w, 22, fill=SURFACE, stroke=BORDER, rx=5)
    s.line(x - w + 24, y, x - w + 24, y + 22, stroke=BORDER)
    s.line(x - 24, y, x - 24, y + 22, stroke=BORDER)
    s.text(x - w + 12, y + 15, "–", size=12, fill=SUB, anchor="middle")
    s.text(x - 12, y + 15, "+", size=12, fill=SUB, anchor="middle")
    s.text(x - w / 2 - 0, y + 15, value, size=11, fill=INK, anchor="middle", weight="bold")


def health_chip(s, x, y, name, state="ok", detail=None):
    """Health chip; shape + label carry state. Returns width."""
    label = {"ok": "ok", "restart": "omstart", "open": "brytare öppen",
             "start": "startar"}[state]
    txt = f"{name} · {label}" + (f" {detail}" if detail else "")
    w = 30 + len(txt) * 5.6
    s.rect(x, y, w, 20, fill=SURFACE if state == "ok" else FILL,
           stroke=BORDER, rx=10)
    cx, cy = x + 13, y + 10
    if state == "ok":
        s.circle(cx, cy, 3.5, fill=SUB)
    elif state == "restart":
        s.path(f"M {cx} {cy-4.5} L {cx+4.5} {cy+3.5} L {cx-4.5} {cy+3.5} Z",
               stroke=INK, sw=1.2, fill=INK)
    elif state == "open":
        s.path(f"M {cx-3.5} {cy-3.5} L {cx+3.5} {cy+3.5} M {cx+3.5} {cy-3.5} L {cx-3.5} {cy+3.5}",
               stroke=INK, sw=1.6)
    else:
        s.circle(cx, cy, 3.5, fill="none", stroke=SUB, sw=1.4)
    s.text(x + 22, y + 14, txt, size=10, fill=SUB)
    return w


def chip(s, x, y, label, active=False, size=11, h=24):
    w = 20 + len(label) * (size * 0.56)
    s.rect(x, y, w, h, fill=INK if active else SURFACE,
           stroke=None if active else BORDER, rx=h / 2)
    s.text(x + w / 2, y + h / 2 + size * 0.36, label, size=size,
           fill=SURFACE if active else SUB, anchor="middle",
           weight="bold" if active else "normal")
    return w


def annot(s, x, y, lines, tx=None, ty=None, anchor="start", width_hint=None):
    """Blue annotation text with optional dashed leader to (tx, ty)."""
    for i, ln in enumerate(lines):
        s.text(x, y + i * 15, ln, size=11.5, fill=ANNOT, style="italic", anchor=anchor)
    if tx is not None:
        if anchor == "start":
            lx = x - 6
        else:
            lx = x + 6
        s.line(lx, y - 4, tx, ty, stroke=ANNOT, sw=1, dash="3 3")
        s.circle(tx, ty, 2.4, fill=ANNOT)


def browser_frame(s, x, y, w, h, url):
    s.rect(x, y, w, h, fill=SURFACE, stroke=SUB, rx=8, sw=1.2)
    s.rect(x, y, w, 28, fill=FILL, stroke=None, rx=8)
    s.rect(x, y + 20, w, 8, fill=FILL, stroke=None)
    s.line(x, y + 28, x + w, y + 28, stroke=BORDER)
    for i, cx in enumerate((16, 32, 48)):
        s.circle(x + cx, y + 14, 4.5, fill="none", stroke=MUT, sw=1.2)
    s.rect(x + 68, y + 6, 240, 16, fill=SURFACE, stroke=BORDER, rx=8)
    s.text(x + 78, y + 17.5, url, size=9.5, fill=MUT)


def phone_frame(s, x, y, w, h):
    s.rect(x, y, w, h, fill=SURFACE, stroke=SUB, rx=22, sw=1.2)
    s.rect(x + w / 2 - 34, y + 8, 68, 12, fill=FILL, rx=6)
    s.text(x + 16, y + 18, "12:44", size=9, fill=MUT)
    s.text(x + w - 16, y + 18, "▮▮ WG", size=8, fill=MUT, anchor="end")


def section_label(s, x, y, txt):
    s.text(x, y, txt.upper(), size=10, fill=MUT, weight="bold", spacing="1.2")


def sheet_title(s, x, y, kicker, title, thesis):
    s.text(x, y, kicker.upper(), size=11, fill=MUT, weight="bold", spacing="1.5")
    s.text(x, y + 28, title, size=22, fill=INK, weight="bold")
    s.text(x, y + 50, thesis, size=13, fill=SUB)


def entity_row(s, x, y, w, name, kind="toggle", on=True, value=None, seed=1):
    """One entity row inside a room card. Returns height used."""
    if kind == "toggle":
        s.text(x, y + 14, name, size=11.5, fill=INK)
        toggle(s, x + w, y + 2, on)
        return 30
    if kind == "sensor":
        s.text(x, y + 14, name, size=11.5, fill=INK)
        s.text(x + w, y + 14, value, size=12, fill=INK, anchor="end", weight="bold")
        sparkline(s, x + w - 150, y + 4, 92, 16, seed=seed)
        return 30
    if kind == "presence":
        s.text(x, y + 14, name, size=11.5, fill=INK)
        s.circle(x + w - 84, y + 10, 3.5, fill=SUB)
        s.text(x + w - 76, y + 14, "3 min sedan", size=10, fill=SUB)
        return 30
    raise ValueError(kind)


# ==============================================================================
# Sheet A — room grid
# ==============================================================================

def sheet_a(path):
    s = SVG(1500, 880)
    sheet_title(s, 40, 40, "homeostat dashboard · wireframe 1/2",
                "Concept A — The rooms",
                "Spatial-first: the house as a grid of room cards, automations as their own cards. "
                "Everything below is generated from manifests — no layout state exists.")

    fx, fy, fw, fh = 40, 128, 1000, 520
    browser_frame(s, fx, fy, fw, fh, "http://homeostat.lan  (LAN / WireGuard)")

    # app header
    hy = fy + 28
    s.text(fx + 20, hy + 30, "homeostat", size=15, fill=INK, weight="bold")
    cx = fx + 150
    for lbl, act in (("Hela huset", False), ("Nere", True), ("Uppe", False), ("Ute", False)):
        cx += chip(s, cx, hy + 12, lbl, active=act) + 8
    hw = health_chip(s, fx + fw - 190, hy + 12, "1 enhet", "restart")
    s.line(fx, hy + 46, fx + fw, hy + 46, stroke=BORDER)

    # health strip
    sy = hy + 56
    hx = fx + 20
    for name, st, det in (("zigbee", "ok", None), ("clock", "ok", None),
                          ("recorder", "ok", None), ("mcp", "ok", None),
                          ("kvällsbelysning", "restart", "3/5")):
        hx += health_chip(s, hx, sy, name, st, det) + 8
    s.line(fx, sy + 30, fx + fw, sy + 30, stroke=BORDER)

    # room cards, zone "Nere"
    top = sy + 44
    colw = (fw - 16 * 4) / 3
    x1, x2, x3 = fx + 16, fx + 16 * 2 + colw, fx + 16 * 3 + colw * 2

    # -- Vardagsrum
    ch1 = 216
    s.rect(x1, top, colw, ch1, fill=SURFACE, stroke=BORDER, rx=8)
    section_label(s, x1 + 14, top + 24, "Vardagsrum")
    s.text(x1 + colw - 14, top + 24, "21,4°", size=11, fill=SUB, anchor="end")
    ry = top + 38
    ry += entity_row(s, x1 + 14, ry, colw - 28, "Lampan i vardagsrummet", "toggle", on=True)
    slider(s, x1 + 14, ry + 6, colw - 28, 0.64, "ljusstyrka", "64 %")
    ry += 24
    ry += entity_row(s, x1 + 14, ry, colw - 28, "Golvlampan", "toggle", on=False)
    ry += entity_row(s, x1 + 14, ry, colw - 28, "Temperaturen", "sensor", value="21,4°", seed=1.3)
    # arbitrated heat pump
    s.line(x1 + 14, ry + 2, x1 + colw - 14, ry + 2, stroke=BORDER)
    s.text(x1 + 14, ry + 22, "Värmepumpen", size=11.5, fill=INK)
    s.rect(x1 + 100, ry + 12, 74, 14, fill=FILL, stroke=BORDER, rx=7)
    s.text(x1 + 137, ry + 22.5, "ARBITRERAD", size=8, fill=SUB, anchor="middle", spacing="0.5")
    stepper(s, x1 + colw - 14, ry + 32, "21,0°")
    s.text(x1 + 14, ry + 46, "börvärde · 18–24°", size=9.5, fill=MUT)

    # -- Hall
    ch2 = 108
    s.rect(x2, top, colw, ch2, fill=SURFACE, stroke=BORDER, rx=8)
    section_label(s, x2 + 14, top + 24, "Hall")
    ry = top + 38
    ry += entity_row(s, x2 + 14, ry, colw - 28, "Taklampan i hallen", "toggle", on=True)
    ry += entity_row(s, x2 + 14, ry, colw - 28, "Rörelsesensorn i hallen", "presence")

    # -- Kök
    ch3 = 176
    s.rect(x3, top, colw, ch3, fill=SURFACE, stroke=BORDER, rx=8)
    section_label(s, x3 + 14, top + 24, "Kök")
    s.text(x3 + colw - 14, top + 24, "22,1°", size=11, fill=SUB, anchor="end")
    ry = top + 38
    ry += entity_row(s, x3 + 14, ry, colw - 28, "Taklampan i köket", "toggle", on=True)
    slider(s, x3 + 14, ry + 6, colw - 28, 0.8, "ljusstyrka", "80 %")
    ry += 24
    slider(s, x3 + 14, ry + 6, colw - 28, 0.35, "färgtemp.", "2 900 K")
    ry += 24
    ry += entity_row(s, x3 + 14, ry, colw - 28, "Temperaturen", "sensor", value="22,1°", seed=2.6)

    # automations band
    ay = top + ch1 + 26
    section_label(s, fx + 16, ay, "Automationer — Nere")
    aw = colw * 2 + 16
    s.rect(fx + 16, ay + 10, aw, 96, fill=SURFACE, stroke=BORDER, rx=8)
    s.text(fx + 30, ay + 34, "Kvällsbelysning", size=12.5, fill=INK, weight="bold")
    s.text(fx + 30, ay + 52, "Släcker nere efter släcktid när ingen är hemma", size=10.5, fill=SUB)
    s.text(fx + 30, ay + 82, "släcktid", size=11, fill=INK)
    stepper(s, fx + 16 + aw - 160, ay + 66, "22:00", w=96)
    s.text(fx + 16 + aw - 14, ay + 82, "20:00–02:00", size=9.5, fill=MUT, anchor="end")

    # ghost card hinting at more zones
    s.rect(fx + 16 + aw + 16, ay + 10, colw, 96, fill=FILL, stroke=BORDER, rx=8, dash="4 4")
    s.text(fx + 16 + aw + 16 + colw / 2, ay + 62, "+ Uppe, Ute …", size=11, fill=MUT, anchor="middle")

    # ---- phone frame ----
    px, py, pw, ph = 1120, 128, 300, 620
    phone_frame(s, px, py, pw, ph)
    iy = py + 30
    s.text(px + 18, iy + 20, "homeostat", size=13, fill=INK, weight="bold")
    health_chip(s, px + pw - 100, iy + 6, "1", "restart")
    ccx = px + 18
    for lbl, act in (("Hela huset", False), ("Nere", True), ("Uppe", False), ("Ute", False)):
        ccx += chip(s, ccx, iy + 34, lbl, active=act, size=10, h=22) + 6
    iy += 66

    cw = pw - 32
    s.rect(px + 16, iy, cw, 168, fill=SURFACE, stroke=BORDER, rx=8)
    section_label(s, px + 30, iy + 22, "Vardagsrum")
    s.text(px + pw - 30, iy + 22, "21,4°", size=10, fill=SUB, anchor="end")
    ry = iy + 34
    ry += entity_row(s, px + 30, ry, cw - 28, "Lampan i vardagsrummet", "toggle", on=True)
    slider(s, px + 30, ry + 6, cw - 28, 0.64, "ljusstyrka", "64 %")
    ry += 24
    ry += entity_row(s, px + 30, ry, cw - 28, "Golvlampan", "toggle", on=False)
    ry += entity_row(s, px + 30, ry, cw - 28, "Temperaturen", "sensor", value="21,4°", seed=1.3)
    iy += 168 + 10

    # collapsed rooms
    for name, summary in (("Hall", "1 tänd · rörelse 3 min"),
                          ("Kök", "1 tänd · 22,1°")):
        s.rect(px + 16, iy, cw, 40, fill=SURFACE, stroke=BORDER, rx=8)
        section_label(s, px + 30, iy + 25, name)
        s.text(px + pw - 46, iy + 25, summary, size=9.5, fill=SUB, anchor="end")
        s.text(px + pw - 30, iy + 25, "▸", size=11, fill=MUT, anchor="end")
        iy += 48

    section_label(s, px + 30, iy + 16, "Automationer")
    iy += 24
    s.rect(px + 16, iy, cw, 62, fill=SURFACE, stroke=BORDER, rx=8)
    s.text(px + 30, iy + 22, "Kvällsbelysning", size=11.5, fill=INK, weight="bold")
    s.text(px + 30, iy + 40, "släcktid · 20:00–02:00", size=9.5, fill=MUT)
    stepper(s, px + pw - 30, iy + 20, "22:00", w=86)
    # scroll fade
    s.rect(px + 4, py + ph - 26, pw - 8, 22, fill=CANVAS, stroke=None, rx=18)
    s.text(px + pw / 2, py + ph - 10, "⌄ scroll", size=9, fill=MUT, anchor="middle")

    # ---- annotations ----
    annot(s, fx + 260, fy - 8,
          ["zones.toml drives the tabs; rooms come from entity room fields"],
          tx=fx + 235, ty=fy + 52)
    s.text(fx + 656, sy + 14, "← home/health/**: state by shape + word, never color alone",
           size=11, fill=ANNOT, style="italic")
    ann_y = fy + fh + 52
    annot(s, 40, ann_y,
          ["features: [\"brightness\"] ⇒ slider;", "commands leave at the manual band"],
          tx=x1 + 180, ty=top + 96)
    annot(s, 320, ann_y,
          ["sparkline: recorder over", "home/history/**, last 24 h"],
          tx=x1 + 224, ty=top + 128)
    annot(s, 560, ann_y,
          ["arbitrated entity: badge; manual wins,", "preemption events surface here"],
          tx=x1 + 137, ty=top + 172)
    annot(s, 830, ann_y,
          ["editable_by = \"family\" params only;", "constraint rendered and enforced on publish"],
          tx=fx + 560, ty=ay + 78)
    annot(s, px + 16, py + ph + 28,
          ["same generated structure, one column;", "collapsed rooms show a summary line"],
          tx=px + pw - 36, ty=py + 330)

    s.text(40, 856, "Wireframe — grayscale is deliberate; visual design is a later pass. "
                    "All labels from [naming].sv.", size=11, fill=MUT)
    s.save(path)


# ==============================================================================
# Sheet B — regulator view
# ==============================================================================

def sheet_b(path):
    s = SVG(1500, 880)
    sheet_title(s, 40, 40, "homeostat dashboard · wireframe 2/2",
                "Concept B — The regulator",
                "State-first: the house as a control loop — signals and setpoints front and center. "
                "Rooms are demoted to navigation; a room detail view reuses Concept A's card.")

    fx, fy, fw, fh = 40, 128, 1000, 560
    browser_frame(s, fx, fy, fw, fh, "http://homeostat.lan  (LAN / WireGuard)")

    # left rail
    rw = 190
    top = fy + 28
    s.rect(fx, top, rw, fh - 28, fill=FILL, stroke=None)
    s.line(fx + rw, top, fx + rw, fy + fh, stroke=BORDER)
    s.text(fx + 20, top + 34, "homeostat", size=15, fill=INK, weight="bold")
    ny = top + 64
    for lbl, act in (("Läget", True), ("Börvärden", False), ("Rum", False)):
        if act:
            s.rect(fx + 10, ny - 16, rw - 20, 26, fill=SURFACE, stroke=BORDER, rx=6)
        s.text(fx + 22, ny + 2, lbl, size=12.5, fill=INK if act else SUB,
               weight="bold" if act else "normal")
        ny += 34
    for room in ("Nere · Vardagsrum", "Nere · Hall", "Nere · Kök", "Uppe · Sovrum", "Ute"):
        s.text(fx + 34, ny, room, size=10.5, fill=MUT)
        ny += 22
    # health block pinned at rail bottom
    hb = fy + fh - 118
    s.line(fx, hb, fx + rw, hb, stroke=BORDER)
    section_label(s, fx + 20, hb + 22, "Hälsa")
    s.path(f"M {fx+26} {hb+38} L {fx+31} {hb+47} L {fx+21} {hb+47} Z", stroke=INK, sw=1.2, fill=INK)
    s.text(fx + 38, hb + 47, "kvällsbelysning · omstart 3/5", size=9.5, fill=SUB)
    s.circle(fx + 26, hb + 66, 3.5, fill=SUB)
    s.text(fx + 38, hb + 70, "4 enheter · ok", size=9.5, fill=SUB)
    s.text(fx + 20, hb + 92, "visa allt ▸", size=9.5, fill=MUT)

    # main pane
    mx = fx + rw + 24
    mw = fw - rw - 48
    s.text(mx, top + 36, "Läget nu", size=16, fill=INK, weight="bold")
    s.text(mx + mw, top + 36, "Nere ▾   12:44", size=11, fill=SUB, anchor="end")

    # signal tiles
    ty = top + 54
    tw = (mw - 2 * 16) / 3
    tiles = (("Temperatur inne", "21,4°", "20,8–22,3° idag", 1.3),
             ("Temperatur ute", "14,2°", "9,6–15,1° idag", 4.1),
             ("Närvaro", None, None, None))
    for i, (lbl, val, rng, seed) in enumerate(tiles):
        x = mx + i * (tw + 16)
        s.rect(x, ty, tw, 108, fill=SURFACE, stroke=BORDER, rx=8)
        section_label(s, x + 14, ty + 22, lbl)
        if val:
            s.text(x + 14, ty + 52, val, size=22, fill=INK, weight="bold")
            sparkline(s, x + 14, ty + 62, tw - 28, 22, seed=seed)
            s.text(x + 14, ty + 98, rng, size=9.5, fill=MUT)
        else:
            s.circle(x + 20, ty + 46, 3.5, fill=SUB)
            s.text(x + 30, ty + 50, "Hallen · 3 min sedan", size=10.5, fill=INK)
            s.circle(x + 20, ty + 68, 3.5, fill="none", stroke=MUT, sw=1.2)
            s.text(x + 30, ty + 72, "Vardagsrummet · 41 min", size=10.5, fill=SUB)
            s.circle(x + 20, ty + 90, 3.5, fill="none", stroke=MUT, sw=1.2)
            s.text(x + 30, ty + 94, "Uppe · 2 tim", size=10.5, fill=SUB)

    # setpoints table
    by = ty + 132
    section_label(s, mx, by, "Börvärden — hela huset")
    rows = (("Kvällsbelysning", "släcktid", "time", "22:00", "20:00–02:00", None),
            ("Värmepumpen", "börvärde", "slider", "21,0°", "18–24°", "ARBITRERAD"),
            ("Morgonvärmen", "starttid", "time", "06:30", "05:00–08:00", None))
    ry = by + 12
    s.rect(mx, ry, mw, len(rows) * 46 + 8, fill=SURFACE, stroke=BORDER, rx=8)
    ry += 4
    for i, (unit, param, kind, val, constraint, badge) in enumerate(rows):
        yy = ry + i * 46
        if i:
            s.line(mx + 14, yy, mx + mw - 14, yy, stroke=BORDER)
        s.text(mx + 16, yy + 20, unit, size=11.5, fill=INK, weight="bold")
        s.text(mx + 16, yy + 36, param, size=9.5, fill=MUT)
        if badge:
            s.rect(mx + 130, yy + 10, 74, 14, fill=FILL, stroke=BORDER, rx=7)
            s.text(mx + 167, yy + 20.5, badge, size=8, fill=SUB, anchor="middle", spacing="0.5")
        if kind == "time":
            stepper(s, mx + mw - 130, yy + 12, val, w=96)
        else:
            tx0 = mx + 330
            s.line(tx0, yy + 23, tx0 + 160, yy + 23, stroke=BORDER, sw=3)
            s.line(tx0, yy + 23, tx0 + 160 * 0.5, yy + 23, stroke=SUB, sw=3)
            s.circle(tx0 + 80, yy + 23, 6, fill=SURFACE, stroke=INK, sw=1.5)
            s.text(tx0 + 180, yy + 27, val, size=11, fill=INK, weight="bold")
        s.text(mx + mw - 16, yy + 27, constraint, size=9.5, fill=MUT, anchor="end")

    # quick rooms column
    qy = ry + len(rows) * 46 + 26
    section_label(s, mx, qy, "Rum — snabbt")
    qw = mw
    s.rect(mx, qy + 12, qw, 118, fill=SURFACE, stroke=BORDER, rx=8)
    qrows = (("Vardagsrum", "2 av 3 tända · 21,4°"),
             ("Hall", "1 tänd · rörelse 3 min"),
             ("Kök", "1 tänd · 22,1°"))
    for i, (room, summary) in enumerate(qrows):
        yy = qy + 12 + 8 + i * 34
        if i:
            s.line(mx + 14, yy - 8, mx + qw - 14, yy - 8, stroke=BORDER)
        s.text(mx + 16, yy + 14, room, size=11.5, fill=INK)
        s.text(mx + qw - 40, yy + 14, summary, size=10, fill=SUB, anchor="end")
        s.text(mx + qw - 18, yy + 14, "▸", size=11, fill=MUT, anchor="end")

    # ---- phone frame: Börvärden tab ----
    px, py, pw, ph = 1120, 128, 300, 620
    phone_frame(s, px, py, pw, ph)
    iy = py + 34
    s.text(px + 18, iy + 18, "Börvärden", size=15, fill=INK, weight="bold")
    iy += 34
    cw = pw - 32
    for unit, param, val, constraint in (
            ("Kvällsbelysning", "släcktid", "22:00", "20:00–02:00"),
            ("Värmepumpen", "börvärde", "21,0°", "18–24° · arbitrerad"),
            ("Morgonvärmen", "starttid", "06:30", "05:00–08:00")):
        s.rect(px + 16, iy, cw, 74, fill=SURFACE, stroke=BORDER, rx=8)
        s.text(px + 30, iy + 22, unit, size=12, fill=INK, weight="bold")
        s.text(px + 30, iy + 40, f"{param} · {constraint}", size=9.5, fill=MUT)
        stepper(s, px + pw - 30, iy + 44, val, w=110)
        s.text(px + 30, iy + 60, "live på bussen", size=8.5, fill=MUT)
        iy += 82
    # bottom tab bar
    tb = py + ph - 54
    s.line(px + 2, tb, px + pw - 2, tb, stroke=BORDER)
    tabs = (("Läget", False), ("Börvärden", True), ("Rum", False), ("Hälsa", False))
    tw2 = pw / 4
    for i, (lbl, act) in enumerate(tabs):
        cx = px + tw2 * i + tw2 / 2
        s.rect(cx - 11, tb + 10, 22, 16, fill=INK if act else FILL,
               stroke=None if act else BORDER, rx=4)
        s.text(cx, tb + 42, lbl, size=8.5, fill=INK if act else MUT, anchor="middle",
               weight="bold" if act else "normal")

    # ---- annotations ----
    annot(s, fx + rw + 250, fy - 8,
          ["sensors first: the regulator's inputs, with today's range"],
          tx=mx + 120, ty=ty + 16)
    ann_y = fy + fh + 52
    annot(s, 40, ann_y,
          ["spatial view demoted to nav;", "room detail reuses Concept A's card"],
          tx=fx + 80, ty=top + 150)
    annot(s, 330, ann_y,
          ["health pinned in the rail,", "not a strip across the top"],
          tx=fx + 90, ty=hb + 50)
    annot(s, 620, ann_y,
          ["every editable_by=\"family\" param in the", "house — one flat list, the family's levers"],
          tx=mx + 400, ty=by + 46)
    annot(s, 920, ann_y,
          ["room summary rows navigate;", "no aggregate \"all off\" action invented"],
          tx=mx + 300, ty=qy + 72)
    annot(s, px + 16, py + ph + 28,
          ["phone leads with Börvärden — the most", "common family act is nudging a setpoint"],
          tx=px + 112, ty=py + ph - 42)

    s.text(40, 856, "Wireframe — grayscale is deliberate; visual design is a later pass. "
                    "All labels from [naming].sv.", size=11, fill=MUT)
    s.save(path)


# ==============================================================================
# Sheet V — widget vocabulary
# ==============================================================================

def sheet_v(path):
    s = SVG(1500, 1120)
    sheet_title(s, 40, 40, "homeostat dashboard · shared",
                "Widget vocabulary — the generation function",
                "Both concepts assemble from these derived widgets. Manifest in, widget out — "
                "this table is what actually gets implemented.")

    x0, y0 = 40, 120
    col1, col2, col3 = 430, 560, 420   # manifest | widget | bus surface
    xw = x0 + col1 + 20
    xb = xw + col2 + 20

    s.text(x0, y0, "MANIFEST SAYS", size=10, fill=MUT, weight="bold", spacing="1.2")
    s.text(xw, y0, "DASHBOARD RENDERS", size=10, fill=MUT, weight="bold", spacing="1.2")
    s.text(xb, y0, "BUS SURFACE", size=10, fill=MUT, weight="bold", spacing="1.2")

    rows_y = y0 + 14
    rh = 96

    def row(i, manifest_lines, bus_lines, draw):
        y = rows_y + i * rh
        s.line(x0, y, x0 + col1 + col2 + col3 + 40, y, stroke=BORDER)
        for j, ln in enumerate(manifest_lines):
            s.text(x0, y + 24 + j * 15, ln, size=10.5, fill=SUB, family=MONO)
        for j, ln in enumerate(bus_lines):
            s.text(xb, y + 24 + j * 15, ln, size=10, fill=SUB, family=MONO)
        draw(xw, y + 14)

    def d_toggle(x, y):
        s.text(x, y + 14, "Golvlampan", size=11.5, fill=INK)
        toggle(s, x + 300, y + 2, True)

    def d_bright(x, y):
        s.text(x, y + 14, "Lampan i vardagsrummet", size=11.5, fill=INK)
        toggle(s, x + 300, y + 2, True)
        slider(s, x, y + 36, 300, 0.64, "ljusstyrka", "64 %")

    def d_ct(x, y):
        s.text(x, y + 14, "Taklampan i köket", size=11.5, fill=INK)
        toggle(s, x + 300, y + 2, True)
        slider(s, x, y + 34, 300, 0.8, "ljusstyrka", "80 %")
        slider(s, x, y + 56, 300, 0.35, "färgtemp.", "2 900 K")

    def d_presence(x, y):
        s.text(x, y + 14, "Rörelsesensorn i hallen", size=11.5, fill=INK)
        s.circle(x + 224, y + 10, 3.5, fill=SUB)
        s.text(x + 234, y + 14, "3 min sedan", size=10, fill=SUB)

    def d_sensor(x, y):
        s.text(x, y + 14, "Temperaturen", size=11.5, fill=INK)
        s.text(x + 300, y + 14, "21,4°", size=12, fill=INK, anchor="end", weight="bold")
        sparkline(s, x + 120, y + 4, 120, 16, seed=1.3)
        s.text(x, y + 36, "tap ⇒ full history view (range, zoom)", size=9.5, fill=MUT)

    def d_time(x, y):
        s.text(x, y + 14, "släcktid", size=11.5, fill=INK)
        stepper(s, x + 300, y + 2, "22:00", w=96)
        s.text(x, y + 36, "tillåtet 20:00–02:00 · out-of-range rejected on publish", size=9.5, fill=MUT)

    def d_number(x, y):
        s.text(x, y + 14, "börvärde", size=11.5, fill=INK)
        s.line(x + 90, y + 10, x + 260, y + 10, stroke=BORDER, sw=3)
        s.line(x + 90, y + 10, x + 175, y + 10, stroke=SUB, sw=3)
        s.circle(x + 175, y + 10, 6, fill=SURFACE, stroke=INK, sw=1.5)
        s.text(x + 300, y + 14, "21,0°", size=11, fill=INK, weight="bold", anchor="end")
        for fr in (0, 0.5, 1):
            s.line(x + 90 + 170 * fr, y + 16, x + 90 + 170 * fr, y + 20, stroke=MUT)
        s.text(x + 90, y + 32, "18°", size=8.5, fill=MUT)
        s.text(x + 260, y + 32, "24°", size=8.5, fill=MUT, anchor="end")

    def d_enum(x, y):
        s.text(x, y + 14, "läge", size=11.5, fill=INK)
        opts = ("av", "eco", "komfort")
        ox = x + 90
        for i, o in enumerate(opts):
            w = 64
            s.rect(ox, y, w, 22, fill=INK if i == 1 else SURFACE,
                   stroke=None if i == 1 else BORDER,
                   rx=11 if i in (0, 2) else 0)
            s.text(ox + w / 2, y + 15, o, size=10, fill=SURFACE if i == 1 else SUB,
                   anchor="middle")
            ox += w

    def d_arb(x, y):
        s.text(x, y + 14, "Värmepumpen", size=11.5, fill=INK)
        s.rect(x + 110, y + 3, 74, 14, fill=FILL, stroke=BORDER, rx=7)
        s.text(x + 147, y + 13.5, "ARBITRERAD", size=8, fill=SUB, anchor="middle", spacing="0.5")
        s.rect(x, y + 28, 330, 26, fill=FILL, stroke=BORDER, rx=6)
        s.text(x + 10, y + 45, "⚑ manuellt övertag — automation förträngd", size=9.5, fill=SUB)

    def d_health(x, y):
        hx = x
        hx += health_chip(s, hx, y, "zigbee", "ok") + 8
        hx += health_chip(s, hx, y, "kvällsbel.", "restart", "3/5") + 8
        hx = x
        y2 = y + 28
        hx += health_chip(s, hx, y2, "recorder", "open") + 8
        hx += health_chip(s, hx, y2, "clock", "start") + 8
        s.text(x, y + 66, "state by shape + word: ● ok  ▲ omstart  ✕ brytare öppen  ○ startar", size=9.5, fill=MUT)

    rows = [
        (['capability = "light"', 'features = []'],
         ["home/state/…/on", "home/cmd/…/on   (manual)"], d_toggle),
        (['capability = "light"', 'features = ["brightness"]'],
         ["…/on, …/brightness", "cmd at manual band"], d_bright),
        (['features = ["brightness",', '            "color_temp"]'],
         ["…/on, …/brightness,", "…/color_temp"], d_ct),
        (['capability = "presence"'],
         ["home/state/…/occupancy", "(read-only: no cmd)"], d_presence),
        (['capability = "sensor"  # bare'],
         ["home/state/…/temperature", "home/history/** for the line"], d_sensor),
        (['[params.off_time]', 'type = "time"', 'constraint = {after, before}'],
         ["home/config/{unit}/{param}", "validated by core on publish"], d_time),
        (['type = "number"', 'constraint = {min = 18, max = 24}'],
         ["home/config/{unit}/{param}"], d_number),
        (['type = "enum"', 'values = ["av","eco","komfort"]'],
         ["home/config/{unit}/{param}"], d_enum),
        (['[write_policy]', 'mode = "arbitrated"'],
         ["cmd via arbiter's output key;", "preemption events published"], d_arb),
        (["(no manifest — derived from", " liveliness + supervision)"],
         ["home/health/{unit}"], d_health),
    ]
    for i, (m, b, d) in enumerate(rows):
        row(i, m, b, d)
    s.line(x0, rows_y + len(rows) * rh, x0 + col1 + col2 + col3 + 40, rows_y + len(rows) * rh, stroke=BORDER)

    annot(s, 1030, 46, ["editable_by = \"family\" gates whether a param", "appears at all — owner params never render"])
    s.save(path)




# ==============================================================================
# Sheet H — hybrid: B's shell, deviation-first "Now"
# ==============================================================================

def sheet_h(path):
    s = SVG(1500, 880)
    sheet_title(s, 40, 40, "homeostat dashboard · design record",
                "A deviation-first “Now”",
                "Signals and setpoints first; rooms are a view. “Now” shows the error signal — "
                "what deviates from equilibrium — and stays calm otherwise. "
                "The map and person entities are settled design, not yet built.")

    fx, fy, fw, fh = 40, 128, 1000, 500
    browser_frame(s, fx, fy, fw, fh, "http://homeostat.lan  (LAN / WireGuard)")

    # left rail
    rw = 190
    top = fy + 28
    s.rect(fx, top, rw, fh - 28, fill=FILL, stroke=None)
    s.line(fx + rw, top, fx + rw, fy + fh, stroke=BORDER)
    s.text(fx + 20, top + 34, "homeostat", size=15, fill=INK, weight="bold")
    ny = top + 64
    for lbl, act in (("Now", True), ("Setpoints", False), ("Rooms", False), ("Health", False)):
        if act:
            s.rect(fx + 10, ny - 16, rw - 20, 26, fill=SURFACE, stroke=BORDER, rx=6)
            s.rect(fx + 10, ny - 16, 3, 26, fill=INK, stroke=None, rx=1.5)
        s.text(fx + 24, ny + 2, lbl, size=12.5, fill=INK if act else SUB,
               weight="bold" if act else "normal")
        ny += 34
    hb = fy + fh - 108
    s.line(fx, hb, fx + rw, hb, stroke=BORDER)
    section_label(s, fx + 20, hb + 22, "Health")
    s.path(f"M {fx+26} {hb+38} L {fx+31} {hb+47} L {fx+21} {hb+47} Z",
           stroke=INK, sw=1.2, fill=INK)
    s.text(fx + 38, hb + 47, "evening lights · restart 3/5", size=9.5, fill=SUB)
    s.circle(fx + 26, hb + 66, 3.5, fill=SUB)
    s.text(fx + 38, hb + 70, "4 units · ok", size=9.5, fill=SUB)

    # main pane
    mx = fx + rw + 24
    mw = fw - rw - 48
    s.text(mx, top + 36, "Now", size=16, fill=INK, weight="bold")
    s.text(mx + mw, top + 36, "Whole house · 12:44", size=11, fill=SUB, anchor="end")

    # signal tiles
    ty = top + 54
    tw = (mw - 2 * 16) / 3
    for i, (lbl, val, rng, seed) in enumerate(
            (("Inside", "21.4°", "20.8–22.3° today", 1.3),
             ("Outside", "14.2°", "9.6–15.1° today", 4.1),
             ("People", None, None, None))):
        x = mx + i * (tw + 16)
        s.rect(x, ty, tw, 100, fill=SURFACE, stroke=BORDER, rx=8)
        section_label(s, x + 14, ty + 22, lbl)
        if val:
            s.text(x + 14, ty + 50, val, size=21, fill=INK, weight="bold")
            sparkline(s, x + 14, ty + 58, tw - 28, 20, seed=seed)
            s.text(x + 14, ty + 92, rng, size=9.5, fill=MUT)
        else:
            s.circle(x + 20, ty + 44, 3.5, fill=SUB)
            s.text(x + 30, ty + 48, "Anna · home — Hallway, 3 min", size=10.5, fill=INK)
            s.circle(x + 20, ty + 68, 3.5, fill="none", stroke=MUT, sw=1.2)
            s.text(x + 30, ty + 72, "Erik · away — 2.1 km", size=10.5, fill=SUB)

    # map card + deviations feed
    ry = ty + 116
    mh = 240
    mwid = 300
    s.rect(mx, ry, mwid, mh, fill=SURFACE, stroke=BORDER, rx=8)
    section_label(s, mx + 14, ry + 22, "Map")
    s.rect(mx + mwid - 76, ry + 10, 62, 15, fill=FILL, stroke=BORDER, rx=7.5)
    s.text(mx + mwid - 45, ry + 21, "PLANNED", size=8, fill=SUB, anchor="middle", spacing="0.5")
    # abstract map: streets, house, one away-pin
    s.rect(mx + 14, ry + 32, mwid - 28, mh - 66, fill=FILL, stroke=BORDER)
    s.path(f"M {mx+14} {ry+90} L {mx+120} {ry+80} L {mx+mwid-14} {ry+110}",
           stroke=SURFACE, sw=5)
    s.path(f"M {mx+90} {ry+32} L {mx+110} {ry+140} L {mx+100} {ry+mh-34}",
           stroke=SURFACE, sw=4)
    s.path(f"M {mx+14} {ry+160} L {mx+mwid-14} {ry+150}", stroke=SURFACE, sw=3)
    s.rect(mx + 140, ry + 96, 12, 12, fill=INK)
    s.text(mx + 158, ry + 106, "home", size=9, fill=SUB)
    s.circle(mx + 236, ry + 68, 6, fill=INK)
    s.path(f"M {mx+236} {ry+74} L {mx+236} {ry+84}", stroke=INK, sw=1.5)
    s.text(mx + 236, ry + 58, "Erik", size=9, fill=SUB, anchor="middle")
    s.text(mx + 14, ry + mh - 12, "self-hosted tiles (PMTiles) — served by the unit",
           size=8.5, fill=MUT)

    dx = mx + mwid + 16
    dw = mw - mwid - 16
    s.rect(dx, ry, dw, mh, fill=SURFACE, stroke=BORDER, rx=8)
    section_label(s, dx + 14, ry + 22, "Out of the ordinary — 3")
    rows = (
        ("tri", "evening lights", "backoff, restart 3 of 5", "supervision"),
        ("dot", "3 lights on", "Livingroom, Kitchen", "state"),
        ("ring", "off time", "23:00 — differs from default 22:00", "setpoint"),
    )
    yy = ry + 40
    for icon, title, detail, src in rows:
        cx, cy = dx + 22, yy + 8
        if icon == "tri":
            s.path(f"M {cx} {cy-5} L {cx+5} {cy+4} L {cx-5} {cy+4} Z",
                   stroke=INK, sw=1.2, fill=INK)
        elif icon == "flag":
            s.text(cx - 5, cy + 5, "⚑", size=11, fill=INK)
        elif icon == "dot":
            s.circle(cx, cy, 4, fill=SUB)
        else:
            s.circle(cx, cy, 4, fill="none", stroke=SUB, sw=1.4)
        s.text(dx + 38, yy + 6, title, size=11.5, fill=INK, weight="bold")
        s.text(dx + 38, yy + 22, detail, size=10, fill=SUB)
        s.text(dx + dw - 32, yy + 6, src, size=9, fill=MUT, anchor="end")
        s.text(dx + dw - 14, yy + 8, "›", size=13, fill=MUT, anchor="end")
        yy += 52
    s.text(dx + 14, ry + mh - 12, "empty when the house is in equilibrium — a calm page",
           size=8.5, fill=MUT)

    # ---- phone: Now tab ----
    px, py, pw, ph = 1120, 128, 300, 620
    phone_frame(s, px, py, pw, ph)
    iy = py + 34
    s.text(px + 18, iy + 18, "Now", size=15, fill=INK, weight="bold")
    s.text(px + pw - 18, iy + 18, "12:44", size=10, fill=MUT, anchor="end")
    iy += 32
    cw = pw - 32
    # people
    s.rect(px + 16, iy, cw, 62, fill=SURFACE, stroke=BORDER, rx=8)
    s.circle(px + 32, iy + 22, 3.5, fill=SUB)
    s.text(px + 42, iy + 26, "Anna · home — Hallway", size=10.5, fill=INK)
    s.circle(px + 32, iy + 44, 3.5, fill="none", stroke=MUT, sw=1.2)
    s.text(px + 42, iy + 48, "Erik · away — 2.1 km", size=10.5, fill=SUB)
    iy += 70
    # deviations
    s.rect(px + 16, iy, cw, 172, fill=SURFACE, stroke=BORDER, rx=8)
    section_label(s, px + 30, iy + 22, "Out of the ordinary")
    yy = iy + 42
    for t, d in (("evening lights", "backoff 3/5"),
                 ("3 lights on", "Livingroom, Kitchen"),
                 ("off time", "23:00 · default 22:00")):
        s.text(px + 30, yy + 4, t, size=10.5, fill=INK, weight="bold")
        s.text(px + pw - 44, yy + 4, d, size=9.5, fill=SUB, anchor="end")
        s.text(px + pw - 30, yy + 5, "›", size=11, fill=MUT, anchor="end")
        yy += 36
    iy += 180
    # mini map
    s.rect(px + 16, iy, cw, 108, fill=SURFACE, stroke=BORDER, rx=8)
    s.rect(px + 26, iy + 10, cw - 20, 88, fill=FILL, stroke=BORDER)
    s.path(f"M {px+26} {iy+50} L {px+140} {iy+42} L {px+pw-26} {iy+60}",
           stroke=SURFACE, sw=4)
    s.rect(px + 120, iy + 48, 10, 10, fill=INK)
    s.circle(px + 210, iy + 34, 5, fill=INK)
    # tab bar
    tb = py + ph - 54
    s.line(px + 2, tb, px + pw - 2, tb, stroke=BORDER)
    tw2 = pw / 4
    for i, (lbl, act) in enumerate((("Now", True), ("Setpoints", False),
                                    ("Rooms", False), ("Health", False))):
        cx = px + tw2 * i + tw2 / 2
        s.rect(cx - 11, tb + 10, 22, 16, fill=INK if act else FILL,
               stroke=None if act else BORDER, rx=4)
        s.text(cx, tb + 42, lbl, size=8.5, fill=INK if act else MUT, anchor="middle",
               weight="bold" if act else "normal")

    # ---- annotations ----
    annot(s, fx + rw + 240, fy - 8,
          ["“Now” = the error signal: deviations from equilibrium, not an inventory"],
          tx=dx + 200, ty=ry + 20)
    ann_y = fy + fh + 52
    annot(s, 40, ann_y,
          ["map: a view over every entity with a", "location aspect — not a per-entity widget;",
           "tiles self-hosted, positions never leave the LAN"],
          tx=mx + 150, ty=ry + mh - 30)
    annot(s, 420, ann_y,
          ["three sources today: supervision, notable state,", "setpoints off default (arbiter preemptions later);",
           "rows click through to their source's detail panel"],
          tx=dx + 50, ty=ry + mh - 25)
    annot(s, 800, ann_y,
          ["people via an OwnTracks adapter → person", "entities; where movers live in the room-keyed",
           "key space is a design-doc decision to settle"],
          tx=mx + 2 * (tw + 16) + 60, ty=ty + 104)
    annot(s, px + 16, py + ph + 28,
          ["phone “Now”: people + deviations first,", "map below; rooms and setpoints one tab away"],
          tx=px + 38, ty=py + ph - 42)

    s.text(40, 856, "Grayscale wireframe of the shipped dashboard. Labels from [naming].en; "
                    "the map and person entities are settled design, not yet built.", size=11, fill=MUT)
    s.save(path)


if __name__ == "__main__":
    import os
    out = os.path.dirname(os.path.abspath(__file__))
    sheet_a(os.path.join(out, "wireframe-a-rooms.svg"))
    sheet_b(os.path.join(out, "wireframe-b-regulator.svg"))
    sheet_v(os.path.join(out, "wireframe-widgets.svg"))
    sheet_h(os.path.join(out, "wireframe-hybrid-now.svg"))
    print("done")
