#!/usr/bin/env python3
"""Build the static dashboard demo (demo-site/README.md) for GitHub Pages.

The output is a directory Pages can serve as it is. The page is the real
adapters/dashboard.html with its real assets; the house behind it is the
browser tests' fixture house (tests/browser/fixtures), answered in the
page by demo-site/shim.js instead of by the dashboard unit. Three edits
make that possible, and each is checked so a change to the page fails the
build instead of shipping a broken demo:

  - asset paths become relative, because Pages serves the site under
    /<repo>/ and an absolute /assets/... would point outside it;
  - demo/data.js and demo/shim.js load first in <head>, ahead of every
    script that talks to the unit;
  - the fixtures lose what a static page cannot show (the camera, which
    needs a stream) and gain what makes a demo legible (positions for the
    two people on the map, the hallway lamp reachable).

Usage: scripts/build_demo_site.py [OUT]   (default: _site)
"""

import json
import os
import pathlib
import re
import shutil
import sys
import tomllib

ROOT = pathlib.Path(__file__).resolve().parents[1]
PAGE = ROOT / "adapters" / "dashboard.html"
ASSETS = ROOT / "adapters" / "assets"
FIXTURES = ROOT / "tests" / "browser" / "fixtures"
SHIM = ROOT / "demo-site" / "shim.js"

# Where the demo house is; the people's positions are offsets from it.
HOME = (59.3326, 18.0649)


def fail(message: str) -> None:
    sys.exit(f"build_demo_site: {message}")


def page() -> str:
    html = PAGE.read_text()
    # "./", not a bare "assets/": the page imports one asset as an ES
    # module, and a module specifier must start with "./" or "/".
    for quote in ('"', "'"):
        html = html.replace(f"{quote}/assets/", f"{quote}./assets/")
    if re.search(r"""["'(]/assets/""", html):
        fail("an absolute /assets/ path survived; the page references assets in a new way")
    if html.count("<head>") != 1:
        fail("expected exactly one <head> to load the demo scripts after")
    return html.replace(
        "<head>",
        '<head>\n<script src="demo/data.js"></script>\n<script src="demo/shim.js"></script>',
        1,
    )


def without(widgets: list, entity: str) -> list:
    kept = []
    for widget in widgets:
        if widget.get("entity") == entity:
            continue
        if "widgets" in widget:
            widget = dict(widget, widgets=without(widget["widgets"], entity))
        kept.append(widget)
    return kept


def data() -> dict:
    model = json.loads((FIXTURES / "model.json").read_text())
    snapshot = json.loads((FIXTURES / "snapshot.json").read_text())
    cameras = [e["name"] for e in model["entities"] if e.get("capability") == "camera"]
    for camera in cameras:
        model["entities"] = [e for e in model["entities"] if e["name"] != camera]
        for view in model["views"]:
            if "widgets" in view:
                view["widgets"] = without(view["widgets"], camera)
        snapshot["state"] = {
            k: v for k, v in snapshot["state"].items() if k.split("/")[3] != camera
        }
    state = snapshot["state"]
    if "home/state/hallway/hallway_lamp/available" in state:
        state["home/state/hallway/hallway_lamp/available"] = True
    people = [e["name"] for e in model["entities"] if e.get("capability") == "person"]
    for i, person in enumerate(people):
        home = state.get(f"home/state/person/{person}/presence", True)
        offset = (0.0001 * i, 0.0002 * i) if home else (0.012, -0.021)
        state[f"home/state/person/{person}/lat"] = round(HOME[0] + offset[0], 6)
        state[f"home/state/person/{person}/lon"] = round(HOME[1] + offset[1], 6)
        state[f"home/state/person/{person}/accuracy"] = 15
    # The page's about lines say what the demo was built from: this
    # checkout's release, and its commit when CI says which it is. There is
    # no house repo behind it, so no house commit.
    version = tomllib.loads((ROOT / "Cargo.toml").read_text())["package"]["version"]
    model["about"] = {"homeostat": {"version": version, "commit": os.environ.get("GITHUB_SHA")}}
    return {"model": model, "snapshot": snapshot}


def main() -> None:
    out = pathlib.Path(sys.argv[1] if len(sys.argv) > 1 else ROOT / "_site")
    if out.exists():
        shutil.rmtree(out)
    shutil.copytree(ASSETS, out / "assets")
    (out / "demo").mkdir(parents=True)
    shutil.copy(SHIM, out / "demo" / "shim.js")
    (out / "demo" / "data.js").write_text(
        "window.HOMEOSTAT_DEMO = " + json.dumps(data(), separators=(",", ":")) + ";\n"
    )
    (out / "index.html").write_text(page())
    (out / ".nojekyll").write_text("")
    print(f"built the dashboard demo in {out}")


if __name__ == "__main__":
    main()
