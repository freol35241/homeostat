# /// script
# requires-python = ">=3.11"
# dependencies = [
#     "homeostat",
#     "aiohttp>=3.9",
# ]
#
# [tool.uv.sources]
# homeostat = { git = "https://github.com/freol35241/homeostat", subdirectory = "sdk/python", tag = "v0.3.0" }
# ///
"""Dashboard service: the family's web surface (see docs/design.md, Dashboard).

An adapter for humans: HTTP + WebSocket toward browsers, the SDK toward the
bus. Serves dashboard.html (one self-contained file next to this script) and
a small API generated entirely from the house's text:

  GET  /api/model    manifests rendered for the browser (zones, entities,
                     units; params filtered to editable_by = "family")
  GET  /ws           snapshot of state/health/config, then live deltas
  POST /api/cmd      one command toward a device, published at the manual
                     band ({room, entity, aspect, value})
  POST /api/param    a parameter write through the core's validating config
                     queryable ({unit, param, value})
  GET  /api/history  recorder proxy for sparklines (?entity=..&aspect=..)

Access is local-only by design (LAN / WireGuard); network reachability is
the credential, so the gate is structural, not auth: every request's Host
must resolve to a non-global address or an allowlisted name (DNS-rebinding
defense), writes require the X-Homeostat header, and a WebSocket Origin, if
present, is held to the same host rule. Extra hostnames (reverse-proxy
setups) go in HOMEOSTAT_DASHBOARD_HOSTS, comma-separated — ports and names
don't belong in the repo.
"""

import argparse
import asyncio
import datetime
import ipaddress
import json
import os
import threading
import time
from pathlib import Path

from aiohttp import WSMsgType, web

from homeostat import ConfigWriteError, connect, house, keys

ENV_HOSTS = "HOMEOSTAT_DASHBOARD_HOSTS"
ALLOWED_NAMES = {"localhost", "homeostat", "homeostat.lan", "homeostat.local"}
WRITE_HEADER = "X-Homeostat"

# Commandable aspects per capability: "on" plus whatever features the entity
# declares. Locks stay read-only until the arbiter exists (structural
# enforcement lives in the adapters; the dashboard doesn't pretend otherwise).
COMMANDABLE = {"light": {"on", "brightness", "color_temp"}}


def label_of(naming: dict, name: str) -> str:
    return naming.get("en") or name.replace("_", " ")


def build_model(model: house.HouseModel) -> dict:
    return {
        "zones": model.zones,
        "entities": [
            {
                "name": e.name,
                "label": label_of(e.naming, e.name),
                "capability": e.capability,
                "features": e.features,
                "room": e.room,
                "write_mode": e.write_mode,
                "owner": e.owner,
            }
            for e in model.entities
        ],
        "units": [
            {
                "name": u.name,
                "label": label_of(u.naming, u.name),
                "kind": u.kind,
                "description": u.description,
                "params": {
                    name: spec
                    for name, spec in u.params.items()
                    if spec.get("editable_by") == "family"
                },
            }
            for u in model.units
        ],
    }


def host_allowed(host_header: str) -> bool:
    """Host (and WS Origin host) must be a non-global address or a known
    name. A rebound public domain arrives as its own name and is refused."""
    host = host_header.rsplit(":", 1)[0] if not host_header.startswith("[") else (
        host_header.split("]")[0].lstrip("[")
    )
    if host in ALLOWED_NAMES:
        return True
    extra = {h.strip() for h in os.environ.get(ENV_HOSTS, "").split(",") if h.strip()}
    if host in extra:
        return True
    try:
        return not ipaddress.ip_address(host).is_global
    except ValueError:
        return False


class Hub:
    """Bus-facing caches plus WebSocket fan-out. Zenoh callbacks arrive on
    zenoh threads; deltas cross into asyncio via call_soon_threadsafe."""

    def __init__(self, session):
        self.session = session
        self.lock = threading.Lock()
        self.state: dict[str, object] = {}
        self.health: dict[str, object] = {}
        self.config: dict[str, object] = {}
        self.loop: asyncio.AbstractEventLoop | None = None
        self.clients: set[web.WebSocketResponse] = set()
        self._subs = []

    def start(self, loop: asyncio.AbstractEventLoop) -> None:
        self.loop = loop
        # Subscribe first, seed after: the mirror holds last values, so a
        # subscription update always supersedes what the seed would write.
        self._subs = [
            self.session.subscribe("home/state/**", self._on_state),
            self.session.subscribe("home/health/**", self._on_health),
            self.session.subscribe("home/config/*/*", self._on_config),
        ]
        for key, value in self.session.get_json("home/state/**"):
            with self.lock:
                self.state.setdefault(key, value)
        for key, value in self.session.get_json("home/health/*"):
            with self.lock:
                self.health.setdefault(key, value)
        for key, value in self.session.get_json("home/config/*/*"):
            with self.lock:
                self.config.setdefault(key, value)

    def snapshot(self) -> dict:
        with self.lock:
            return {
                "type": "snapshot",
                "state": dict(self.state),
                "health": dict(self.health),
                "config": dict(self.config),
            }

    def _decode(self, sample):
        return str(sample.key_expr), json.loads(sample.payload.to_bytes())

    def _on_state(self, sample) -> None:
        key, value = self._decode(sample)
        with self.lock:
            self.state[key] = value
        self._emit({"type": "state", "key": key, "value": value})

    def _on_config(self, sample) -> None:
        key, value = self._decode(sample)
        with self.lock:
            self.config[key] = value
        self._emit({"type": "config", "key": key, "value": value})

    def _on_health(self, sample) -> None:
        key, value = self._decode(sample)
        segments = key.split("/")
        if len(segments) == 3:  # home/health/{unit}: supervision status
            with self.lock:
                self.health[key] = value
            self._emit({"type": "health", "key": key, "value": value})
        elif segments[-1] == "event":
            self._emit({"type": "event", "key": key, "value": value, "ts": int(time.time())})

    def _emit(self, message: dict) -> None:
        if self.loop is not None:
            self.loop.call_soon_threadsafe(self._broadcast, json.dumps(message))

    def _broadcast(self, text: str) -> None:
        for ws in set(self.clients):
            asyncio.ensure_future(self._send(ws, text))

    async def _send(self, ws: web.WebSocketResponse, text: str) -> None:
        try:
            await ws.send_str(text)
        except (ConnectionError, RuntimeError):
            self.clients.discard(ws)


def json_error(message: str, status: int = 400) -> web.Response:
    return web.json_response({"error": message}, status=status)


@web.middleware
async def guard(request: web.Request, handler):
    if not host_allowed(request.headers.get("Host", "")):
        return json_error("host not allowed", status=403)
    if request.method == "POST" and WRITE_HEADER not in request.headers:
        return json_error(f"missing {WRITE_HEADER} header", status=403)
    return await handler(request)


def make_app(hub: Hub, model: dict, page: Path) -> web.Application:
    entities = {e["name"]: e for e in model["entities"]}
    units = {u["name"]: u for u in model["units"]}

    async def index(request: web.Request) -> web.StreamResponse:
        return web.FileResponse(page)

    async def api_model(request: web.Request) -> web.Response:
        return web.json_response(model)

    async def ws_handler(request: web.Request) -> web.WebSocketResponse:
        origin = request.headers.get("Origin")
        if origin is not None:
            host = origin.split("://", 1)[-1].split("/", 1)[0]
            if not host_allowed(host):
                raise web.HTTPForbidden(text="origin not allowed")
        ws = web.WebSocketResponse(heartbeat=30)
        await ws.prepare(request)
        await ws.send_str(json.dumps(hub.snapshot()))
        hub.clients.add(ws)
        try:
            async for message in ws:  # client sends nothing; drain until close
                if message.type == WSMsgType.ERROR:
                    break
        finally:
            hub.clients.discard(ws)
        return ws

    async def api_cmd(request: web.Request) -> web.Response:
        try:
            body = await request.json()
            room, entity = str(body["room"]), str(body["entity"])
            aspect, value = str(body["aspect"]), body["value"]
        except (ValueError, KeyError):
            return json_error("body must be {room, entity, aspect, value}")
        spec = entities.get(entity)
        if spec is None or spec["room"] != room:
            return json_error(f"unknown entity {room}/{entity}")
        allowed = COMMANDABLE.get(spec["capability"], set())
        if aspect not in allowed or (aspect != "on" and aspect not in spec["features"]):
            return json_error(f"{spec['capability']} {entity} takes no {aspect} command")
        hub.session.put_json(keys.cmd_key(room, entity, aspect), value)
        return web.json_response({"ok": True})

    async def api_param(request: web.Request) -> web.Response:
        try:
            body = await request.json()
            unit, param, value = str(body["unit"]), str(body["param"]), body["value"]
        except (ValueError, KeyError):
            return json_error("body must be {unit, param, value}")
        spec = units.get(unit)
        if spec is None or param not in spec["params"]:
            return json_error(f"no family-editable param {unit}.{param}")
        try:
            stored = await asyncio.get_running_loop().run_in_executor(
                None, hub.session.write_config, unit, param, value
            )
        except ConfigWriteError as error:
            return json_error(str(error))
        return web.json_response({"ok": True, "value": stored})

    async def api_history(request: web.Request) -> web.Response:
        entity = request.query.get("entity", "")
        aspect = request.query.get("aspect", "")
        if not entity or not aspect:
            return json_error("entity and aspect are required")
        hours = min(float(request.query.get("hours", "24")), 24 * 31)
        limit = min(int(request.query.get("limit", "500")), 5000)
        now = datetime.datetime.now(datetime.timezone.utc)
        start = now - datetime.timedelta(hours=hours)
        selector = (
            f"{keys.history_key('state', entity, aspect)}"
            f"?from={start.isoformat(timespec='seconds')}"
            f";to={now.isoformat(timespec='seconds')};limit={limit}"
        )
        replies = await asyncio.get_running_loop().run_in_executor(
            None, hub.session.get_json, selector
        )
        return web.json_response(
            {"series": [{"key": key, "points": points} for key, points in replies]}
        )

    app = web.Application(middlewares=[guard])
    app.router.add_get("/", index)
    app.router.add_get("/api/model", api_model)
    app.router.add_get("/ws", ws_handler)
    app.router.add_post("/api/cmd", api_cmd)
    app.router.add_post("/api/param", api_param)
    app.router.add_get("/api/history", api_history)
    return app


async def serve(app: web.Application, hub: Hub, host: str, port: int) -> None:
    hub.start(asyncio.get_running_loop())
    runner = web.AppRunner(app)
    await runner.setup()
    site = web.TCPSite(runner, host, port)
    await site.start()
    hub.session.ready()  # up means "accepting connections", not "spawned"

    stop = asyncio.Event()
    loop = asyncio.get_running_loop()
    import signal

    for sig in (signal.SIGTERM, signal.SIGINT):
        loop.add_signal_handler(sig, stop.set)
    await stop.wait()
    await runner.cleanup()


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--host", default="0.0.0.0")
    parser.add_argument(
        "--port", type=int, default=int(os.environ.get("HOMEOSTAT_DASHBOARD_PORT", "8600"))
    )
    args = parser.parse_args()

    model = build_model(house.load_house("."))
    page = Path(__file__).resolve().parent / "dashboard.html"
    session = connect()
    hub = Hub(session)
    try:
        asyncio.run(serve(make_app(hub, model, page), hub, args.host, args.port))
    finally:
        session.close()


if __name__ == "__main__":
    main()
