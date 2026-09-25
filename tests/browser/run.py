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

What these tests are for (docs/design.md, Dashboard; issue #181): locking
in rules we have already learned, and one broad net — no page errors, every
view renders, every widget kind draws. They are NOT the discovery
mechanism. Every rendering bug this project has had was found by a person
opening a browser, and that habit is the thing to keep; this suite carries
the boring half so nobody re-runs twenty checks by hand.

Deliberately not asserted: pixels, screenshots, whole-HTML snapshots, CSS
beyond the handful that is behaviour, exact copy (match a shape, not a
wording), and chart path coordinates — geometry is asserted numerically in
tests/js.
"""

import pathlib
import subprocess
import sys
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
        pinned = await self.page.locator(".pin-btn[data-view]").evaluate_all(
            "els => els.map(e => e.getAttribute('data-view'))"
        )
        self.assertEqual(pinned, ["health", "notshown"])


class ChoiceSurvivesRerender(PageTest):
    """`docs/design.md`, Dashboard: what a reader chose survives a
    re-render only if it is held outside the markup.

    Live state re-renders the panel and replaces its nodes; the rule has
    five instances and one of them (the source legend's pin) shipped
    broken because nobody re-checked the others. One test each, so the
    insight stays a checklist item.
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
        # On a phone the same six live in the tab bar, the views scrolling
        # and the chrome staying put (docs/design.md, Views are text).
        names = await self.page.locator("button[data-view]:visible").evaluate_all(
            "els => els.map(e => e.getAttribute('data-view'))"
        )
        self.assertEqual(
            names, ["now", "heating", "downstairs", "everything", "health", "notshown"]
        )


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
