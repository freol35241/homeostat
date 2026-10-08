# /// script
# requires-python = ">=3.11"
# dependencies = [
#     "aiohttp",
#     "playwright",
# ]
# ///
"""Browser tests for the dashboard page.

    uv run --script tests/browser/run.py                    # all
    uv run --script tests/browser/run.py Smoke              # one case
    uv run --script tests/browser/run.py --install-browser  # once, per machine

The page is the real `adapters/dashboard.html`; the house behind it is
`server.py`'s fixtures (see that file for why). Assertions go through the
DOM and the network only — the page's script is an IIFE, so there are no
internals to reach, which keeps every assertion to something a person or
another process could observe.

What these tests are for (docs/design.md, Dashboard): locking in rules we
have already learned, and one broad net — no page errors, every view
renders, every widget kind draws. They are NOT the discovery mechanism.
Rendering bugs are found by a person opening a browser, and that habit is
the thing to keep; this suite carries the boring half so nobody re-runs
twenty checks by hand.

Deliberately not asserted: pixels, screenshots, whole-HTML snapshots, CSS
beyond the handful that is behaviour, exact copy (match a shape, not a
wording), and chart path coordinates — geometry is asserted numerically in
tests/js.
"""

import functools
import http.server
import pathlib
import subprocess
import sys
import tempfile
import threading
import unittest

# server.py sits beside this file; `uv run --script` does not add that
# directory to the path, and the tests must run from any cwd.
sys.path.insert(0, str(pathlib.Path(__file__).resolve().parent))
from playwright.async_api import async_playwright
from server import FakeHouse

PHONE = {"width": 390, "height": 844}
DESKTOP = {"width": 1280, "height": 900}


class PageTest(unittest.IsolatedAsyncioTestCase):
    """One fake house and one browser page per test, so a test can never
    inherit another's state — the bugs this suite exists for are about
    state surviving, and a shared page would hide them."""

    viewport = DESKTOP

    async def asyncSetUp(self) -> None:
        self.house = FakeHouse()
        url = await self.house.start()
        self.pw = await async_playwright().start()
        self.browser = await self.pw.chromium.launch()
        context = await self.browser.new_context(
            viewport=self.viewport, timezone_id="Europe/Stockholm", locale="en-GB"
        )
        self.page = await context.new_page()
        # A thrown exception or a console error IS a failure, in every
        # test: the page renders on, so nothing else would notice.
        self.faults: list[str] = []
        self.page.on("pageerror", lambda e: self.faults.append(f"pageerror: {e}"))
        self.page.on(
            "console",
            lambda m: self.faults.append(f"console: {m.text}") if m.type == "error" else None,
        )
        await self.page.goto(url, wait_until="networkidle")
        await self.page.wait_for_timeout(600)

    async def asyncTearDown(self) -> None:
        await self.browser.close()
        await self.pw.stop()
        await self.house.stop()
        self.assertEqual(self.faults, [], "the page faulted")

    async def view(self, name: str) -> None:
        if not await self.page.locator(f'button[data-view="{name}"]:visible').count():
            # On a phone, Health and Not shown are behind the top bar's
            # status button.
            await self.page.click("#topbar-status")
        await self.page.click(f'button[data-view="{name}"]:visible')
        await self.page.wait_for_timeout(500)

    async def text(self) -> str:
        return await self.page.locator("#view").inner_text()


class Smoke(PageTest):
    """The broad net: the odds against a bug nobody has thought of are
    better here than in any targeted assertion."""

    async def test_every_view_renders_something(self):
        for name in ("now", "heating", "downstairs", "everything", "health", "notshown"):
            await self.view(name)
            body = (await self.text()).strip()
            self.assertTrue(body, f"view {name} rendered an empty body")
            self.assertNotIn("&MIDDOT", body, f"view {name} rendered an unescaped entity")
            self.assertNotIn("undefined", body.lower(), f"view {name} rendered an undefined")

    async def test_every_widget_kind_draws(self):
        # tile / people / deviations / map
        await self.view("now")
        self.assertGreater(await self.page.locator('[data-action="history-detail"]').count(), 0)
        self.assertIn("PEOPLE", await self.text())
        self.assertIn("OUT OF THE ORDINARY", await self.text())
        # group / dial / chart / burner / entity(camera)
        await self.view("heating")
        heating = await self.text()
        self.assertIn("Heating", heating)
        self.assertGreater(await self.page.locator(".chart-wrap").count(), 1, "charts")
        self.assertGreater(await self.page.locator(".dial .dial-arc").count(), 0, "a dial")
        self.assertIn("wood stove", heating.lower())
        self.assertIn("porch camera", heating.lower())
        # unit / params / room
        await self.view("downstairs")
        downstairs = await self.text()
        self.assertIn("evening lights", downstairs.lower())
        self.assertIn("Livingroom", downstairs)
        self.assertGreater(await self.page.locator(".param-row").count(), 0, "params")

    async def test_the_nav_is_the_file_and_the_chrome_is_the_rail(self):
        # dashboard.toml REPLACES the nav; Health and Not shown are fixed
        # chrome below it, never views (docs/design.md, Views are text).
        names = await self.page.locator("nav button[data-view]:visible").evaluate_all(
            "els => els.map(e => e.getAttribute('data-view'))"
        )
        self.assertEqual(names, ["now", "heating", "downstairs", "everything"])
        pinned = await self.page.locator("#rail .pin-btn[data-view]").evaluate_all(
            "els => els.map(e => e.getAttribute('data-view'))"
        )
        self.assertEqual(pinned, ["health", "notshown"])
        # Under them, where the page is served from.
        about = await self.page.locator("#rail-about").inner_text()
        self.assertIn("homeostat 0.16.1", about)
        self.assertIn("house 4f2a9c1", about)


class ChoiceSurvivesRerender(PageTest):
    """`docs/design.md`, Dashboard: what a reader chose survives a
    re-render only if it is held outside the markup.

    Live state re-renders the panel and replaces its nodes; the rule has
    five instances, and a change to one ships the others broken unless
    each is re-checked. One test each, so the insight stays a checklist
    item.
    """

    async def rerender(self) -> None:
        """A state delta, which is what re-renders the panel in a house."""
        await self.house.push(
            {
                "type": "state",
                "key": "home/state/livingroom/livingroom_temp/temperature",
                "value": 20.9,
            }
        )
        await self.page.wait_for_timeout(400)

    async def open_sources(self) -> None:
        await self.view("heating")
        await self.page.click(
            '[data-action="history-detail"][data-entity="downstairs_temperature"]'
        )
        await self.page.wait_for_timeout(700)
        await self.page.click('[data-action="chart-layer"][data-layer="sources"]')
        await self.page.wait_for_timeout(700)

    async def test_a_pinned_source_stays_pinned(self):
        await self.open_sources()
        entries = self.page.locator(".source-legend span[data-source]")
        self.assertGreater(await entries.count(), 1, "two contributors to tell apart")
        name = await entries.nth(1).get_attribute("data-source")
        await entries.nth(1).click()
        await self.page.mouse.move(5, 5)  # a hover must not be what holds it

        await self.rerender()

        highlighted = self.page.locator(".source-legend span.hi")
        self.assertEqual(await highlighted.count(), 1, "the pin survived")
        self.assertEqual(await highlighted.get_attribute("data-source"), name)

    async def test_the_chosen_layer_stays_chosen(self):
        await self.open_sources()
        await self.rerender()
        active = self.page.locator('[data-action="chart-layer"].active')
        self.assertEqual(await active.get_attribute("data-layer"), "sources")

    async def test_the_chosen_range_stays_chosen(self):
        await self.view("heating")
        await self.page.click('[data-action="history-detail"][data-entity="livingroom_temp"]')
        await self.page.wait_for_timeout(700)
        await self.page.click('[data-action="range-chip"][data-hours="168"]')
        await self.page.wait_for_timeout(700)

        await self.rerender()

        active = self.page.locator('[data-action="range-chip"].active')
        self.assertEqual(await active.get_attribute("data-hours"), "168")

    async def test_an_unsent_drag_is_not_snatched_back(self):
        # A slider moved but not RELEASED holds the reader's value, not the
        # house's: a delta mid-drag that reset it would fight the finger.
        # `input` is the drag, `change` is the release — Playwright's fill()
        # fires both, which is a finished drag and a different test.
        await self.view("downstairs")
        slider = self.page.locator(
            'input[data-action="slider"][data-kind="brightness"][data-entity="livingroom_lamp"]'
        )
        await slider.evaluate(
            "el => { el.value = '35'; el.dispatchEvent(new Event('input', { bubbles: true })); }"
        )
        await self.page.wait_for_timeout(200)
        self.assertEqual(self.house.posted("/api/cmd"), [], "a drag in progress is not a command")

        await self.rerender()

        self.assertEqual(await slider.input_value(), "35")

        # Releasing it is what commands, once — and in the device's scale,
        # not the slider's: the control is a percent, `brightness` is
        # 0-254, so 35 % leaves as 89.
        await slider.dispatch_event("change")
        await self.page.wait_for_timeout(300)
        self.assertEqual(
            self.house.posted("/api/cmd"),
            [{"room": "livingroom", "entity": "livingroom_lamp", "aspect": "brightness", "value": 89}],
        )


class RulesAboutControls(PageTest):
    """The house's say over a control, and the command's own stages."""

    async def test_a_declared_step_reaches_the_slider_and_its_buttons(self):
        # dashboard.toml's [[control]] for this lamp says 5.
        await self.view("downstairs")
        slider = self.page.locator(
            'input[data-action="slider"][data-kind="brightness"][data-entity="livingroom_lamp"]'
        )
        self.assertEqual(await slider.get_attribute("step"), "5")
        self.assertEqual(
            await slider.evaluate("el => getComputedStyle(el).touchAction"),
            "pan-y",
            "a scroll that starts on the thumb must not drag it",
        )
        nudge = self.page.locator(
            '[data-action="brightness-step"][data-entity="livingroom_lamp"]'
        ).first
        self.assertEqual(await nudge.get_attribute("data-delta"), "-5")

    async def test_a_command_is_a_proposal_until_the_device_answers(self):
        await self.view("downstairs")
        toggle = self.page.locator(
            '[data-action="toggle-light"][data-entity="livingroom_lamp"]'
        ).first
        await toggle.click()
        await self.page.wait_for_timeout(300)

        posted = self.house.posted("/api/cmd")
        self.assertEqual(len(posted), 1, "one tap, one command")
        self.assertEqual(
            posted[0],
            {"room": "livingroom", "entity": "livingroom_lamp", "aspect": "on", "value": False},
        )
        self.assertGreater(
            await self.page.locator(".cmd-pending").count(), 0, "pending until answered"
        )

        # Stage 4: the device reports back and the command is over.
        await self.house.push(
            {"type": "state", "key": "home/state/livingroom/livingroom_lamp/on", "value": False}
        )
        await self.page.wait_for_timeout(400)
        self.assertEqual(await self.page.locator(".cmd-pending").count(), 0)


    async def test_steps_add_up_to_one_command_for_where_they_end(self):
        await self.view("downstairs")
        key = "home/state/livingroom/heat_pump/setpoint"
        before = self.house.snapshot["state"][key]
        plus = self.page.locator(
            '[data-action="aspect-step"][data-entity="heat_pump"][data-aspect="setpoint"][data-delta="0.5"]'
        ).first
        for _ in range(3):
            await plus.click()
        await self.page.wait_for_timeout(150)  # the render after the last tap; well inside the settle
        status = self.page.locator('[data-cmd-status="livingroom/heat_pump/setpoint"]')
        # The request is on the control at once, and nothing has gone out
        # while the taps continue.
        self.assertIn(f"Asked {before + 1.5:.1f}°", await status.text_content())
        self.assertEqual(self.house.posted("/api/cmd"), [])
        await self.page.wait_for_timeout(1000)
        self.assertEqual(
            self.house.posted("/api/cmd"),
            [{"room": "livingroom", "entity": "heat_pump", "aspect": "setpoint", "value": before + 1.5}],
            "three taps, one command, for where they ended",
        )

        # A bridge republishing the old value has not answered.
        await self.house.push({"type": "state", "key": key, "value": before})
        await self.page.wait_for_timeout(300)
        self.assertIn(f"still {before:.1f}°", await status.text_content())

        # The device takes it, and the control says so.
        await self.house.push({"type": "state", "key": key, "value": before + 1.5})
        await self.page.wait_for_timeout(300)
        self.assertIn(f"✓ {before + 1.5:.1f}°", await status.text_content())

    async def test_the_detail_overlay_shows_a_command_in_flight(self):
        await self.view("downstairs")
        await self.page.locator(
            '[data-action="toggle-light"][data-entity="livingroom_lamp"]'
        ).first.click()
        await self.page.wait_for_timeout(300)
        # Opening the overlay is a render of its own, with no delta behind
        # it: the command must be said there too.
        await self.page.locator(
            '#view [data-action="entity-detail"][data-entity="livingroom_lamp"] .entity-name'
        ).first.click()
        await self.page.wait_for_timeout(400)
        self.assertEqual(
            await self.page.locator(
                '#overlay-panel [data-cmd-status="livingroom/livingroom_lamp/on"]'
            ).count(),
            1,
        )

    async def test_a_command_nothing_hears_says_so_at_once(self):
        self.house.heard = False
        await self.view("downstairs")
        await self.page.locator(
            '[data-action="toggle-light"][data-entity="livingroom_lamp"]'
        ).first.click()
        status = self.page.locator('[data-cmd-status="livingroom/livingroom_lamp/on"]')
        await status.wait_for(timeout=2000)
        await self.page.wait_for_timeout(300)
        self.assertIn("Nothing is listening", await status.text_content())


class ViewsAreText(PageTest):
    """`docs/design.md`, Views are text: a view shows the dashboard.toml
    block that makes it, so what is on screen has a name to say."""

    async def test_a_view_shows_its_text(self):
        await self.view("heating")
        await self.page.click('[data-action="view-text"][data-view-name="heating"]')
        await self.page.wait_for_timeout(400)
        text = await self.page.locator("#overlay-panel pre.view-text").inner_text()
        self.assertTrue(text.startswith('[[view]]\nname = "heating"\nwidgets = ['), text)
        self.assertIn('{ kind = "burner", entity = "stove" },', text)
        self.assertIn('    { kind = "dial", entity = "heat_pump" },', text)

    async def test_chrome_has_no_text(self):
        await self.view("health")
        self.assertEqual(await self.page.locator('[data-action="view-text"]').count(), 0)


class HoldsOnNow(PageTest):
    """`docs/design.md`, Arbitrated mode: a hold is a deviation when it
    displaced somebody, and possession shows on the control either way."""

    async def test_a_hold_over_an_automated_aspect_is_a_deviation(self):
        await self.view("now")
        body = await self.text()
        self.assertIn("heat pump", body.lower())
        self.assertRegex(body, r"held by \w+ until \d{2}:\d{2}")
        self.assertRegex(body, r"2 wishes refused")

    async def test_a_hold_that_displaces_nobody_says_nothing_here(self):
        # The same hold, moved to an aspect no unit is granted to drive.
        await self.house.push(
            {
                "type": "hold",
                "key": "home/hold/arbiter",
                "value": {
                    "schema": 1,
                    "holds": [
                        {
                            "room": "hallway",
                            "entity": "front_door",
                            "aspect": "locked",
                            "priority": "manual",
                            "actor": "dashboard",
                            "since": "2026-09-25T06:00:00Z",
                            "until": "2099-01-01T00:00:00Z",
                            "refused": 0,
                        }
                    ],
                },
            }
        )
        await self.view("now")
        self.assertNotRegex(await self.text(), r"held by")

    async def test_a_held_aspect_is_marked_on_its_control(self):
        await self.view("heating")
        await self.page.click('[data-action="entity-detail"][data-entity="heat_pump"]')
        await self.page.wait_for_timeout(700)
        marks = self.page.locator("#overlay-panel .stale-mark")
        texts = await marks.evaluate_all("els => els.map(e => e.textContent)")
        self.assertIn("held", texts)


class SmokePhone(Smoke):
    """The same net at phone width, where the family surface mostly lives:
    a different nav, a different grid, the same page."""

    viewport = PHONE

    async def test_the_nav_is_the_file_and_the_chrome_is_the_rail(self):
        # On a phone the bottom bar is the file's views and nothing else;
        # Health and Not shown are behind the top bar's status button, with
        # the about lines (docs/design.md, Views are text).
        names = await self.page.locator("button[data-view]:visible").evaluate_all(
            "els => els.map(e => e.getAttribute('data-view'))"
        )
        self.assertEqual(names, ["now", "heating", "downstairs", "everything"])
        await self.page.click("#topbar-status")
        sheet = self.page.locator("#status-sheet")
        self.assertEqual(
            await sheet.locator(".pin-btn[data-view]:visible").evaluate_all(
                "els => els.map(e => e.getAttribute('data-view'))"
            ),
            ["health", "notshown"],
        )
        self.assertIn("homeostat 0.16.1", await sheet.inner_text())
        # A tap outside puts it away and does nothing else.
        await self.page.locator('#tabs button[data-view="heating"]').click()
        self.assertFalse(await sheet.is_visible(), "a tap outside puts the sheet away")
        self.assertEqual(
            await self.page.locator('#tabs button.active').get_attribute("data-view"), "now"
        )
        await self.page.click("#topbar-status")
        await sheet.locator('[data-view="health"]').click()
        self.assertFalse(await sheet.is_visible(), "a pick puts the sheet away")
        self.assertEqual(await self.page.locator("#topbar-status.active").count(), 1)


class PagesDemo(unittest.IsolatedAsyncioTestCase):
    """The static demo on GitHub Pages (demo-site/README.md), as built and
    as served: under /homeostat/, with demo-site/shim.js standing in for
    the dashboard unit. The shim is a second fake of the unit's endpoints
    beside server.py, so this is what keeps it honest — a page that starts
    asking for something the shim does not answer faults here, or fails a
    request, instead of breaking the published demo where nobody looks."""

    viewport = DESKTOP

    def setUp(self) -> None:
        # Built and served before the event loop's half of the setup: the
        # build is a blocking subprocess.
        root = pathlib.Path(__file__).resolve().parents[2]
        self.site = tempfile.TemporaryDirectory()
        subprocess.run(
            [sys.executable, str(root / "scripts" / "build_demo_site.py"),
             str(pathlib.Path(self.site.name) / "homeostat")],
            check=True, capture_output=True,
        )
        class Quiet(http.server.SimpleHTTPRequestHandler):
            def log_message(self, *args) -> None:
                pass

        handler = functools.partial(Quiet, directory=self.site.name)
        self.httpd = http.server.ThreadingHTTPServer(("127.0.0.1", 0), handler)
        threading.Thread(target=self.httpd.serve_forever, daemon=True).start()

    async def asyncSetUp(self) -> None:
        self.pw = await async_playwright().start()
        self.browser = await self.pw.chromium.launch()
        context = await self.browser.new_context(
            viewport=self.viewport, timezone_id="Europe/Stockholm", locale="en-GB"
        )
        self.page = await context.new_page()
        self.faults: list[str] = []
        self.page.on("pageerror", lambda e: self.faults.append(f"pageerror: {e}"))
        self.page.on(
            "console",
            lambda m: self.faults.append(f"console: {m.text}") if m.type == "error" else None,
        )
        self.page.on("requestfailed", lambda r: self.faults.append(f"request failed: {r.url}"))
        self.page.on(
            "response",
            lambda r: self.faults.append(f"HTTP {r.status}: {r.url}") if r.status >= 400 else None,
        )
        port = self.httpd.server_address[1]
        await self.page.goto(f"http://127.0.0.1:{port}/homeostat/", wait_until="networkidle")
        await self.page.wait_for_timeout(600)

    async def asyncTearDown(self) -> None:
        await self.browser.close()
        await self.pw.stop()
        self.httpd.shutdown()
        self.site.cleanup()
        self.assertEqual(self.faults, [], "the demo faulted")

    async def view(self, name: str) -> None:
        await self.page.click(f'button[data-view="{name}"]:visible')
        await self.page.wait_for_timeout(500)

    async def test_every_view_renders_something(self):
        for name in ("now", "heating", "downstairs", "everything", "health", "notshown"):
            await self.view(name)
            body = (await self.page.locator("#view").inner_text()).strip()
            self.assertTrue(body, f"view {name} rendered an empty body")
            self.assertNotIn("undefined", body.lower(), f"view {name} rendered an undefined")
            # Every chart the view draws got history from the shim.
            await self.page.wait_for_timeout(300)

    async def test_a_command_is_answered_by_the_shim(self):
        await self.view("now")
        self.assertIn("1 light on", await self.page.locator("#view").inner_text())
        await self.page.click('[data-action="lights-off"]:visible')
        await self.page.wait_for_timeout(1500)
        self.assertNotIn("1 light on", await self.page.locator("#view").inner_text())


def install_browser() -> int:
    """Fetches the Chromium this script's locked Playwright expects, with
    the system libraries it needs. One command for CI and the devcontainer,
    so the browser can never be a version the lockfile did not ask for."""
    return subprocess.call(
        [sys.executable, "-m", "playwright", "install", "--with-deps", "chromium"]
    )


if __name__ == "__main__":
    if "--install-browser" in sys.argv:
        raise SystemExit(install_browser())
    unittest.main(verbosity=2)
