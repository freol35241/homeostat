#!/usr/bin/env python3
"""Generates the README schematics in this directory as draw.io-editable SVGs.

Sheets:
  concept      — the hypothesis: home automation is regulation (two loops,
                 Ashby's ultrastability mapped onto homeostat vocabulary)
  architecture — the software architecture: repo -> core -> bus -> units

Each diagram is defined once (nodes, edges, texts, absolute geometry) and
rendered twice from that single model: an <svg> body for GitHub, and an
uncompressed mxGraphModel embedded in the SVG root's `content` attribute —
the draw.io "editable SVG" format, so the same file opens in
draw.io / diagrams.net for editing.

Run from anywhere: python3 generate.py — output lands next to the script.
Same discipline as docs/wireframes: grayscale, blue for annotations only.
"""

import os

# ---- palette (shared with docs/wireframes) -----------------------------------
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
    return s.replace("&", "&amp;").replace("<", "&lt;").replace(">", "&gt;")


def attr(s):
    return esc(s).replace('"', "&quot;")


class Diagram:
    """One diagram: a node/edge/text model rendered as SVG + embedded mxfile."""

    def __init__(self, name, w, h):
        self.name, self.w, self.h = name, w, h
        self.nodes = {}  # id -> dict
        self.order = []  # node ids in insertion order (groups first by caller)
        self.edges = []
        self.texts = []

    # -- model -----------------------------------------------------------------

    def node(self, nid, x, y, w, h, title, subs=(), kind="box", mono_x=None):
        self.nodes[nid] = dict(x=x, y=y, w=w, h=h, title=title,
                               subs=list(subs), kind=kind, mono_x=mono_x)
        self.order.append(nid)

    def edge(self, src, dst, p1, p2, points=(), label=None, label_pos=None,
             label_anchor="middle", dashed=False, both=False):
        self.edges.append(dict(src=src, dst=dst, p1=p1, p2=p2,
                               points=list(points), label=label,
                               label_pos=label_pos, label_anchor=label_anchor,
                               dashed=dashed, both=both))

    def text(self, x, y, s, size=12, fill=INK, weight="normal", anchor="start",
             family=SANS, style=None, spacing=None):
        self.texts.append(dict(x=x, y=y, s=s, size=size, fill=fill,
                               weight=weight, anchor=anchor, family=family,
                               style=style, spacing=spacing))

    def header(self, title, subtitle):
        self.text(40, 36, "HOMEOSTAT · DESIGN RECORD", size=11, fill=MUT,
                  weight="bold", spacing=1.5)
        self.text(40, 66, title, size=22, weight="bold")
        self.text(40, 88, subtitle, size=13, fill=SUB)

    def annot(self, x, y, s):
        self.text(x, y, s, size=10, fill=ANNOT, style="italic")

    # -- SVG rendering -----------------------------------------------------------

    def _svg_node(self, out, n):
        x, y, w, h = n["x"], n["y"], n["w"], n["h"]
        cx, cy = x + w / 2, y + h / 2
        kind = n["kind"]
        if kind == "group":
            out.append(f'<rect x="{x}" y="{y}" width="{w}" height="{h}" rx="8" '
                       f'fill="none" stroke="{MUT}" stroke-width="1" '
                       f'stroke-dasharray="5 4"/>')
            out.append(f'<text x="{x + 12}" y="{y + 17}" font-size="9.5" '
                       f'fill="{SUB}" font-weight="bold" letter-spacing="1.2">'
                       f'{esc(n["title"])}</text>')
            return
        if kind == "band":
            out.append(f'<rect x="{x}" y="{y}" width="{w}" height="{h}" rx="6" '
                       f'fill="{INK}"/>')
            out.append(f'<text x="{x + 24}" y="{cy + 4.5}" font-size="12.5" '
                       f'fill="{SURFACE}" font-weight="bold">{esc(n["title"])}</text>')
            if n["subs"]:
                out.append(f'<text x="{n["mono_x"]}" y="{cy + 4.5}" font-size="11" '
                           f'fill="{BORDER}" font-family="{MONO}">'
                           f'{esc(n["subs"][0])}</text>')
            return
        if kind == "circle":
            out.append(f'<circle cx="{cx}" cy="{cy}" r="{w / 2}" fill="{SURFACE}" '
                       f'stroke="{INK}" stroke-width="1.5"/>')
            out.append(f'<text x="{cx}" y="{cy + 5}" font-size="14" fill="{INK}" '
                       f'font-weight="bold" text-anchor="middle">{esc(n["title"])}</text>')
            return
        fill = FILL if kind == "soft" else SURFACE
        stroke = MUT if kind == "soft" else SUB
        out.append(f'<rect x="{x}" y="{y}" width="{w}" height="{h}" rx="6" '
                   f'fill="{fill}" stroke="{stroke}" stroke-width="1.2"/>')
        block = 16 + 14 * len(n["subs"])
        ty = cy - block / 2 + 12
        out.append(f'<text x="{cx}" y="{ty:.1f}" font-size="12" fill="{INK}" '
                   f'font-weight="bold" text-anchor="middle">{esc(n["title"])}</text>')
        for i, s in enumerate(n["subs"]):
            out.append(f'<text x="{cx}" y="{ty + 14 + 14 * i:.1f}" font-size="10" '
                       f'fill="{SUB}" text-anchor="middle">{esc(s)}</text>')

    def _svg(self):
        out = [
            f'<svg xmlns="http://www.w3.org/2000/svg" width="{self.w}" '
            f'height="{self.h}" viewBox="0 0 {self.w} {self.h}" '
            f'font-family="{SANS}" content="{attr(self._mxfile())}">',
            f'<rect width="{self.w}" height="{self.h}" fill="{CANVAS}"/>',
            '<defs><marker id="arr" viewBox="0 0 10 10" refX="8.5" refY="5" '
            'markerWidth="6.5" markerHeight="6.5" orient="auto-start-reverse">'
            f'<path d="M0 0L10 5L0 10z" fill="{SUB}"/></marker></defs>',
        ]
        for nid in self.order:  # groups/bands under everything
            if self.nodes[nid]["kind"] in ("group", "band"):
                self._svg_node(out, self.nodes[nid])
        for e in self.edges:
            pts = [e["p1"]] + e["points"] + [e["p2"]]
            p = " ".join(f"{x},{y}" for x, y in pts)
            dash = ' stroke-dasharray="5 4"' if e["dashed"] else ""
            start = ' marker-start="url(#arr)"' if e["both"] else ""
            out.append(f'<polyline points="{p}" fill="none" stroke="{SUB}" '
                       f'stroke-width="1.3"{dash} marker-end="url(#arr)"{start}/>')
        for nid in self.order:
            if self.nodes[nid]["kind"] not in ("group", "band"):
                self._svg_node(out, self.nodes[nid])
        for e in self.edges:  # labels above nodes; halo pass first, then ink
            if e["label"]:
                lx, ly = e["label_pos"]
                common = (f'x="{lx}" y="{ly}" font-size="10" '
                          f'text-anchor="{e["label_anchor"]}"')
                out.append(f'<text {common} fill="none" stroke="{CANVAS}" '
                           f'stroke-width="3">{esc(e["label"])}</text>')
                out.append(f'<text {common} fill="{SUB}">{esc(e["label"])}</text>')
        for t in self.texts:
            extra = ""
            if t["style"]:
                extra += f' font-style="{t["style"]}"'
            if t["spacing"]:
                extra += f' letter-spacing="{t["spacing"]}"'
            out.append(f'<text x="{t["x"]}" y="{t["y"]}" font-size="{t["size"]}" '
                       f'fill="{t["fill"]}" font-weight="{t["weight"]}" '
                       f'text-anchor="{t["anchor"]}" font-family="{t["family"]}"'
                       f'{extra}>{esc(t["s"])}</text>')
        out.append("</svg>")
        return "\n".join(out)

    # -- draw.io (mxfile) rendering ----------------------------------------------

    def _mx_value(self, n):
        # raw HTML; escaped exactly once by attr() at emission
        v = f"<b>{n['title']}</b>"
        for s in n["subs"]:
            v += f'<br/><font style="font-size: 10px" color="{SUB}">{s}</font>'
        return v

    def _mx_node(self, out, nid, n):
        kind = n["kind"]
        if kind == "group":
            style = (f"rounded=1;dashed=1;fillColor=none;strokeColor={MUT};"
                     f"html=1;verticalAlign=top;align=left;spacing=10;"
                     f"fontSize=10;fontStyle=1;fontColor={SUB};")
            value = n["title"]
        elif kind == "band":
            style = (f"rounded=1;fillColor={INK};strokeColor=none;html=1;"
                     f"align=left;spacingLeft=24;fontSize=12;fontColor={SURFACE};")
            value = f"<b>{n['title']}</b>"
            if n["subs"]:
                value += (f'  <font style="font-size: 11px" '
                          f'color="{BORDER}" face="Courier New">'
                          f"{n['subs'][0]}</font>")
        elif kind == "circle":
            style = (f"ellipse;html=1;fillColor={SURFACE};strokeColor={INK};"
                     f"strokeWidth=1.5;fontSize=14;fontStyle=1;fontColor={INK};")
            value = n["title"]
        else:
            fill = FILL if kind == "soft" else SURFACE
            stroke = MUT if kind == "soft" else SUB
            style = (f"rounded=1;arcSize=10;whiteSpace=wrap;html=1;"
                     f"fillColor={fill};strokeColor={stroke};strokeWidth=1.2;"
                     f"fontSize=12;fontColor={INK};verticalAlign=middle;")
            value = self._mx_value(n)
        out.append(f'<mxCell id="{nid}" value="{attr(value)}" style="{style}" '
                   f'vertex="1" parent="1">'
                   f'<mxGeometry x="{n["x"]}" y="{n["y"]}" width="{n["w"]}" '
                   f'height="{n["h"]}" as="geometry"/></mxCell>')

    def _frac(self, nid, p):
        n = self.nodes[nid]
        return (round((p[0] - n["x"]) / n["w"], 3),
                round((p[1] - n["y"]) / n["h"], 3))

    def _mxfile(self):
        out = []
        for nid in self.order:
            if self.nodes[nid]["kind"] in ("group", "band"):
                self._mx_node(out, nid, self.nodes[nid])
        for nid in self.order:
            if self.nodes[nid]["kind"] not in ("group", "band"):
                self._mx_node(out, nid, self.nodes[nid])
        for i, e in enumerate(self.edges):
            (x1, y1), (x2, y2) = self._frac(e["src"], e["p1"]), self._frac(e["dst"], e["p2"])
            style = (f"html=1;rounded=0;endArrow=block;endFill=1;endSize=6;"
                     f"strokeColor={SUB};strokeWidth=1.3;fontSize=10;"
                     f"fontColor={SUB};labelBackgroundColor={CANVAS};"
                     f"exitX={x1};exitY={y1};exitDx=0;exitDy=0;"
                     f"entryX={x2};entryY={y2};entryDx=0;entryDy=0;")
            if e["dashed"]:
                style += "dashed=1;"
            if e["both"]:
                style += "startArrow=block;startFill=1;startSize=6;"
            label = attr(e["label"]) if e["label"] else ""
            pts = "".join(f'<mxPoint x="{x}" y="{y}"/>' for x, y in e["points"])
            pts = f'<Array as="points">{pts}</Array>' if pts else ""
            out.append(f'<mxCell id="e{i}" value="{label}" style="{style}" '
                       f'edge="1" parent="1" source="{e["src"]}" target="{e["dst"]}">'
                       f'<mxGeometry relative="1" as="geometry">{pts}</mxGeometry>'
                       f'</mxCell>')
        for i, t in enumerate(self.texts):
            bold = "fontStyle=1;" if t["weight"] == "bold" else ""
            if t["style"] == "italic":
                bold = "fontStyle=2;"
            align = {"start": "left", "middle": "center", "end": "right"}[t["anchor"]]
            w = int(len(t["s"]) * t["size"] * 0.62) + 8
            x = {"start": t["x"], "middle": t["x"] - w / 2, "end": t["x"] - w}[t["anchor"]]
            out.append(f'<mxCell id="t{i}" value="{attr(t["s"])}" '
                       f'style="text;html=1;align={align};verticalAlign=middle;'
                       f'fontSize={t["size"]};fontColor={t["fill"]};{bold}" '
                       f'vertex="1" parent="1">'
                       f'<mxGeometry x="{x:.0f}" y="{t["y"] - 12}" width="{w}" '
                       f'height="20" as="geometry"/></mxCell>')
        cells = "".join(out)
        return (f'<mxfile host="app.diagrams.net" agent="docs/diagrams/generate.py" '
                f'version="24.0.0" type="device">'
                f'<diagram id="{self.name}" name="{self.name}">'
                f'<mxGraphModel dx="800" dy="600" grid="0" gridSize="10" '
                f'guides="1" tooltips="1" connect="1" arrows="1" fold="1" '
                f'page="1" pageScale="1" pageWidth="{self.w}" '
                f'pageHeight="{self.h}" background="{CANVAS}" math="0" shadow="0">'
                f'<root><mxCell id="0"/><mxCell id="1" parent="0"/>{cells}</root>'
                f'</mxGraphModel></diagram></mxfile>')

    def save(self, path):
        with open(path, "w") as f:
            f.write(self._svg())
        print(f"wrote {path}")


# ---- sheet: concept ------------------------------------------------------------

def concept():
    d = Diagram("concept", 1160, 660)
    d.header("Home automation is regulation",
             "A household is a plant held at family-owned setpoints by feedback; "
             "when regulation falls short, the machinery itself is rewired — "
             "through git, reviewed.")

    d.node("outer", 40, 112, 1080, 164,
           "OUTER LOOP · ADAPT THE REGULATOR — STRUCTURAL, OWNER-GATED", kind="group")
    d.node("inner", 40, 304, 1080, 316,
           "INNER LOOP · REGULATE — RUNS CONTINUOUSLY ON THE BUS", kind="group")

    # outer loop: who may rewire the machine, and how
    d.node("owner", 70, 176, 180, 64, "Owner · AI agent",
           ["git + CLI · MCP propose"], kind="soft")
    d.node("repo", 350, 176, 200, 64, "House repo (git)",
           ["units · grants · code — structure"])
    d.node("plan", 650, 176, 170, 64, "plan / apply",
           ["tier derived from the diff"])
    d.edge("owner", "repo", (250, 208), (350, 208),
           label="edits · proposes", label_pos=(300, 200))
    d.edge("repo", "plan", (550, 208), (650, 208),
           label="desired state", label_pos=(600, 200))

    # inner loop: the classic control loop in homeostat vocabulary
    d.node("family", 70, 340, 180, 56, "Family",
           ["dashboard · voice — always wins"], kind="soft")
    d.node("setp", 70, 452, 180, 68, "Setpoints",
           ["home/config/** · validated"])
    d.node("cmp", 296, 462, 48, 48, "Σ", kind="circle")
    d.node("auto", 400, 452, 190, 68, "Automations",
           ["pure-code regulators"])
    d.node("adapt", 650, 452, 190, 68, "Adapters → devices",
           ["home/cmd/** · actuate"])
    d.node("house", 880, 438, 210, 96, "The house",
           ["temperature · light · presence · locks"])
    d.node("dist", 900, 340, 170, 48, "Disturbances",
           ["weather · sunset · people"], kind="soft")
    d.node("sens", 490, 545, 200, 50, "Sensors",
           ["home/state/**"])

    d.edge("family", "setp", (160, 396), (160, 452),
           label="adjusts", label_pos=(170, 428), label_anchor="start")
    d.edge("setp", "cmp", (250, 486), (296, 486))
    d.edge("cmp", "auto", (344, 486), (400, 486),
           label="error", label_pos=(372, 478))
    d.edge("auto", "adapt", (590, 486), (650, 486),
           label="commands", label_pos=(620, 478))
    d.edge("adapt", "house", (840, 486), (880, 486))
    d.edge("dist", "house", (985, 388), (985, 438))
    d.edge("house", "sens", (985, 534), (690, 570), points=[(985, 570)])
    d.edge("sens", "cmp", (490, 570), (320, 510), points=[(320, 570)],
           label="measured state", label_pos=(405, 562))
    d.text(284, 470, "+", size=12, fill=INK, weight="bold", anchor="middle")
    d.text(334, 528, "−", size=12, fill=INK, weight="bold", anchor="middle")

    # the loops meet: observation escalates to structure, structure reconfigures
    d.edge("house", "owner", (1090, 470), (160, 176),
           points=[(1104, 470), (1104, 146), (160, 146)], dashed=True,
           label="regulation falls short? observe → rewire", label_pos=(632, 158))
    d.edge("plan", "auto", (735, 240), (480, 452),
           points=[(735, 412), (480, 412)],
           label="reconfigures the regulator", label_pos=(727, 340),
           label_anchor="end")

    d.annot(70, 264, "tier gates authority: parameter-only auto-applies · "
                     "behavioral & structural wait for the owner")
    d.annot(70, 606, "the dashboard's “Now” view is this error signal — "
                     "a house in equilibrium renders an empty page")
    return d


# ---- sheet: architecture ---------------------------------------------------------

def architecture():
    d = Diagram("architecture", 1160, 760)
    d.header("Software architecture",
             "One small Rust core supervises plain OS processes that meet on a "
             "Zenoh bus; the house repo is the single source of truth, "
             "everything else is derived or live.")

    d.node("repo", 40, 120, 230, 110, "House repo (git)",
           ["zones.toml · units/*.toml", "entities/**/*.toml",
            "automations (*.py)"])
    d.node("cli", 330, 166, 180, 64, "homeostat CLI", ["plan · apply"])
    d.node("core", 570, 110, 310, 135, "Core — homeostat up (Rust)",
           ["validator · grant table",
            "supervisor: liveliness · backoff · breaker",
            "last-value cache · plan/apply engine"])
    d.node("bus", 40, 305, 1080, 46, "Zenoh bus",
           ["home/{state · cmd · config · health · clock · history · "
            "discovery · meta}"], kind="band", mono_x=160)

    d.edge("repo", "core", (270, 140), (570, 140),
           label="up: load · validate · expand", label_pos=(420, 132))
    d.edge("repo", "cli", (270, 198), (330, 198))
    d.edge("cli", "bus", (420, 230), (420, 305),
           label="over the bus", label_pos=(428, 272), label_anchor="start")
    d.edge("core", "bus", (725, 245), (725, 305), both=True,
           label="router session · validating queryables",
           label_pos=(733, 272), label_anchor="start")

    d.node("units", 40, 405, 1080, 175,
           "UNITS · SUPERVISED OS PROCESSES — UNIFORM MANIFESTS, "
           "LIVELINESS TOKEN = UP", kind="group")
    d.node("zigbee", 72, 450, 155, 86, "zigbee",
           ["adapter — one per protocol", "Zigbee2MQTT ↔ bus keys"])
    d.node("evening", 244, 450, 155, 86, "evening_lights",
           ["automation", "Python on the SDK"])
    d.node("clock", 416, 450, 155, 86, "clock",
           ["service", "civil time · DST"])
    d.node("recorder", 588, 450, 155, 86, "recorder",
           ["service", "history over the bus"])
    d.node("dash", 760, 450, 155, 86, "dashboard",
           ["service", "the family surface"])
    d.node("mcp", 932, 450, 155, 86, "mcp",
           ["service", "the agent surface"])
    for u in ("zigbee", "evening", "clock", "recorder", "dash", "mcp"):
        n = d.nodes[u]
        cx = n["x"] + n["w"] / 2
        d.edge(u, "bus", (cx, 450), (cx, 351), both=True)
    d.edge("core", "units", (880, 180), (1100, 405),
           points=[(1100, 180)], dashed=True,
           label="spawns · supervises", label_pos=(1094, 300), label_anchor="end")

    d.node("z2m", 48, 628, 200, 64, "Zigbee2MQTT · MQTT",
           ["radios · devices"], kind="soft")
    d.node("sqlite", 600, 628, 140, 56, "SQLite store",
           ["recorder-private"], kind="soft")
    d.node("browsers", 760, 628, 155, 64, "Family browsers",
           ["LAN / WireGuard"], kind="soft")
    d.node("agent", 932, 628, 155, 64, "AI agent",
           ["MCP · stdio / HTTP"], kind="soft")
    d.edge("zigbee", "z2m", (149.5, 536), (148, 628), both=True,
           label="mqtt", label_pos=(158, 606), label_anchor="start")
    d.edge("recorder", "sqlite", (665.5, 536), (665.5, 628))
    d.edge("dash", "browsers", (837.5, 536), (837.5, 628), both=True,
           label="HTTP + WS", label_pos=(846, 606), label_anchor="start")
    d.edge("mcp", "agent", (1009.5, 536), (1009.5, 628), both=True)
    d.edge("agent", "repo", (1009.5, 692), (40, 198),
           points=[(1009.5, 724), (26, 724), (26, 198)], dashed=True,
           label="propose: commit → plan → pending plan for the owner",
           label_pos=(520, 716))
    return d


if __name__ == "__main__":
    here = os.path.dirname(os.path.abspath(__file__))
    concept().save(os.path.join(here, "concept.drawio.svg"))
    architecture().save(os.path.join(here, "architecture.drawio.svg"))
