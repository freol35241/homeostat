"""A fake house behind the real dashboard page.

The page under test is `adapters/dashboard.html`, served exactly as the
dashboard unit serves it, with its real assets. Everything behind it is
canned: no supervisor, no bus, no clock. That is the point — the states
worth testing (a forecast whose horizon has ended, two providers
disagreeing, a hold at each band, a contributor that dropped out) take
minutes to stage on a live house and are a few lines of JSON here, and a
test can decide exactly when a delta arrives and therefore when the page
re-renders.

What is NOT canned: the page, its assets, and the shapes. Anything the
fixtures get wrong about what the real unit emits, the canary test in
run.py (a real supervised house, booted once) is there to catch.

Time-relative data — history, forecasts — is GENERATED per request rather
than stored, because a canned timestamp goes stale: a forecast frozen into
a file would be a spent one by tomorrow, and a test would start failing
for a reason that has nothing to do with the page.
"""

import json
import math
import os
import pathlib
import time

from aiohttp import web

ROOT = pathlib.Path(__file__).resolve().parents[2]
# The page under test. The override exists to check the harness itself:
# point it at a deliberately broken copy and the suite must go red, because
# a net that cannot fail is indistinguishable from one that passes.
PAGE = pathlib.Path(os.environ.get("HOMEOSTAT_TEST_PAGE") or ROOT / "adapters" / "dashboard.html")
ASSETS = ROOT / "adapters" / "assets"
FIXTURES = pathlib.Path(__file__).resolve().parent / "fixtures"


def load(name: str):
    return json.loads((FIXTURES / f"{name}.json").read_text())


def iso(epoch: float) -> str:
    return time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime(epoch))


class FakeHouse:
    """The server plus the handles a test drives it by."""

    def __init__(self) -> None:
        self.model = load("model")
        self.snapshot = load("snapshot")
        # Every POST the page made, in order: what a test asserts a tap by.
        self.requests: list[dict] = []
        self.clients: list[web.WebSocketResponse] = []
        self.app = self._build()
        self.runner: web.AppRunner | None = None
        self.port = 0

    # ---- the handles ----------------------------------------------------

    async def push(self, message: dict) -> None:
        """Delivers one WebSocket message to every open page, the way the
        hub does. A test uses this to force a re-render at a chosen
        moment."""
        for ws in list(self.clients):
            if ws.closed:
                self.clients.remove(ws)
                continue
            await ws.send_str(json.dumps(message))

    def posted(self, path: str) -> list[dict]:
        return [r["body"] for r in self.requests if r["path"] == path]

    # ---- the server -----------------------------------------------------

    def _build(self) -> web.Application:
        app = web.Application()
        app.router.add_get("/", self.index)
        app.router.add_get("/api/model", self.api_model)
        app.router.add_get("/ws", self.ws)
        app.router.add_get("/api/history", self.api_history)
        app.router.add_get("/api/forecasts", self.api_forecasts)
        app.router.add_get("/api/source-events", self.api_source_events)
        app.router.add_get("/api/logs", self.api_logs)
        app.router.add_post("/api/cmd", self.api_cmd)
        app.router.add_post("/api/param", self.api_param)
        app.router.add_post("/api/lights/off", self.api_lights_off)
        # The camera card mounts a player that opens this socket. Accepting
        # and saying nothing keeps a camera in the fixture house without
        # the console error a refused connection would log — which the
        # smoke test would otherwise read as a page fault.
        app.router.add_get("/api/camera/{entity}/live", self.api_camera)
        app.router.add_get("/assets/{name}", self.api_asset)
        app.router.add_get("/tiles.pmtiles", self.api_tiles)
        return app

    async def start(self) -> str:
        self.runner = web.AppRunner(self.app)
        await self.runner.setup()
        site = web.TCPSite(self.runner, "127.0.0.1", 0)
        await site.start()
        self.port = site._server.sockets[0].getsockname()[1]
        return f"http://127.0.0.1:{self.port}/"

    async def stop(self) -> None:
        for ws in list(self.clients):
            await ws.close()
        if self.runner is not None:
            await self.runner.cleanup()

    # ---- routes ---------------------------------------------------------

    async def index(self, request: web.Request) -> web.StreamResponse:
        return web.FileResponse(PAGE, headers={"Cache-Control": "no-cache"})

    async def api_asset(self, request: web.Request) -> web.StreamResponse:
        name = request.match_info["name"]
        path = (ASSETS / name).resolve()
        if ASSETS not in path.parents or not path.exists():
            raise web.HTTPNotFound()
        types = {".js": "text/javascript", ".css": "text/css", ".svg": "image/svg+xml"}
        return web.FileResponse(
            path, headers={"Content-Type": types.get(path.suffix, "text/plain")}
        )

    async def api_model(self, request: web.Request) -> web.Response:
        return web.json_response(dict(self.model, tiles=False))

    async def ws(self, request: web.Request) -> web.WebSocketResponse:
        ws = web.WebSocketResponse()
        await ws.prepare(request)
        self.clients.append(ws)
        await ws.send_str(json.dumps(dict(self._freshen(self.snapshot), type="snapshot")))
        async for _ in ws:
            pass
        if ws in self.clients:
            self.clients.remove(ws)
        return ws

    def _freshen(self, snapshot: dict) -> dict:
        """Re-stamps the time-bearing documents against now.

        A forecast frozen into a fixture is a spent one by tomorrow, and a
        hold frozen into one has expired before the test starts — both
        would fail for a reason that has nothing to do with the page. The
        structure is the fixture's; the clock is this method's. A test that
        wants a spent forecast or a lapsed hold pushes its own document
        with explicit timestamps, which is the honest way to ask for one.
        """
        now = time.time()
        fresh = dict(snapshot)
        forecasts = {}
        for key, doc in (snapshot.get("forecasts") or {}).items():
            points = doc.get("points") or []
            forecasts[key] = dict(
                doc,
                issued=iso(now - 3600),
                points=[
                    dict(p, t=iso(now + i * 3600)) for i, p in enumerate(points)
                ],
            )
        fresh["forecasts"] = forecasts
        holds = {}
        for key, doc in (snapshot.get("holds") or {}).items():
            holds[key] = dict(
                doc,
                holds=[
                    dict(h, since=iso(now - 300), until=iso(now + 1500))
                    for h in (doc.get("holds") or [])
                ],
            )
        fresh["holds"] = holds
        return fresh

    async def api_history(self, request: web.Request) -> web.Response:
        """A plausible day for whatever was asked for: the page draws the
        series it gets, and no test asserts on these numbers."""
        entity = request.query.get("entity", "")
        aspect = request.query.get("aspect", "")
        hours = float(request.query.get("hours", "24"))
        now = time.time()
        step = max(60.0, hours * 3600 / 120)
        points = []
        t = now - hours * 3600
        while t <= now:
            points.append(
                {
                    "ts": iso(t),
                    "room": "livingroom",
                    "value": round(20 + 1.5 * math.sin((t - now) / 9000), 2),
                }
            )
            t += step
        return web.json_response(
            {"series": [{"key": f"home/history/state/{entity}/{aspect}", "points": points}]}
        )

    async def api_forecasts(self, request: web.Request) -> web.Response:
        """Three kept issues about the next twelve hours, newest last."""
        now = time.time()
        issues = []
        for age_h in (6, 3, 1):
            issued = now - age_h * 3600
            issues.append(
                {
                    "schema": 1,
                    "issued": iso(issued),
                    "source": "model",
                    "points": [
                        {"t": iso(issued + h * 3600), "v": round(20 + h * 0.1 + age_h * 0.05, 2)}
                        for h in range(12)
                    ],
                }
            )
        return web.json_response({"issues": issues})

    async def api_source_events(self, request: web.Request) -> web.Response:
        return web.json_response({"events": []})

    async def api_logs(self, request: web.Request) -> web.Response:
        return web.json_response({"lines": [{"stream": "stdout", "line": "staged"}]})

    async def _record(self, request: web.Request) -> dict:
        body = await request.json()
        self.requests.append({"path": request.path, "body": body})
        return body

    async def api_cmd(self, request: web.Request) -> web.Response:
        await self._record(request)
        return web.json_response({"ok": True, "id": "test0001"})

    async def api_param(self, request: web.Request) -> web.Response:
        body = await self._record(request)
        return web.json_response({"ok": True, "value": body.get("value")})

    async def api_lights_off(self, request: web.Request) -> web.Response:
        await self._record(request)
        return web.json_response({"ok": True, "sent": 1})

    async def api_camera(self, request: web.Request) -> web.WebSocketResponse:
        ws = web.WebSocketResponse()
        await ws.prepare(request)
        async for _ in ws:
            pass
        return ws

    async def api_tiles(self, request: web.Request) -> web.Response:
        raise web.HTTPNotFound()
