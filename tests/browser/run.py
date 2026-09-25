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
