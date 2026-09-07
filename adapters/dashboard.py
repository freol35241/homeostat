# /// script
# requires-python = ">=3.11"
# dependencies = [
#     "homeostat",
#     "aiohttp>=3.9",
# ]
#
# [tool.uv.sources]
# homeostat = { path = "../sdk/python", editable = true }
# ///
"""Dashboard service: the family's web surface (see docs/design.md, Dashboard).

An adapter for humans: HTTP + WebSocket toward browsers, the SDK toward the
bus. Serves dashboard.html (one self-contained file next to this script) and
a small API generated entirely from the house's text:

  GET  /api/model    manifests rendered for the browser (zones, entities,
                     units; params filtered to editable_by = "family";
                     each entity marked commandable iff this unit's own
                     manifest grants its capability)
  GET  /ws           snapshot of state/health/config, then live deltas
  POST /api/cmd      one command toward a device, published at the manual
                     band ({room, entity, aspect, value})
  POST /api/param    a parameter write through the core's validating config
                     queryable ({unit, param, value})
  POST /api/lights/off  the whole-house darken: one manual-band off-command
                     per bound light — group actions are manual-edge
                     fan-outs, never a relay entity (docs/design.md,
                     Dashboard)
  GET  /api/history  recorder proxy for sparklines (?entity=..&aspect=..)
  GET  /api/logs     unit's captured stdout/stderr tail, for the unit detail
                     overlay (?unit=..&lines=N), proxying the supervisor's
                     home/meta/{unit}/log queryable
  GET  /api/camera/{entity}/snapshot   go2rtc frame.jpeg proxy — the room-
                     card poster for camera entities
  GET  /api/camera/{entity}/live       WebSocket relayed byte-for-byte to
                     go2rtc's api/ws (MSE) — browsers never speak go2rtc
                     (docs/design.md, Cameras); HOMEOSTAT_GO2RTC overrides
                     the localhost default for both. Both address the
                     stream by the camera's entity id, which is how the
                     go2rtc shim names it — resolved server-side, since
                     ids are not part of the browser-facing model
  GET  /assets/*     vendored libraries (Leaflet, protomaps-leaflet, the
                     go2rtc player), allowlisted by filename
  GET  /tiles.pmtiles  self-hosted PMTiles region extract for the map
                     widget, from HOMEOSTAT_DASHBOARD_TILES; 404 if unset

Access is local-only by design (LAN / WireGuard); network reachability is
the credential, so the gate is structural, not auth: every request's Host
must resolve to a non-global address or an allowlisted name (DNS-rebinding
defense), writes require the X-Homeostat header, and a WebSocket Origin, if
present, is held to the same host rule. Extra hostnames (reverse-proxy
setups) go in HOMEOSTAT_DASHBOARD_HOSTS, comma-separated — ports and names
don't belong in the repo. HOMEOSTAT_DASHBOARD_TILES points at the house's
self-hosted PMTiles region extract for the map widget (never in the repo);
unset means the map renders without a base layer.
"""

import argparse
import asyncio
import contextlib
import datetime
import ipaddress
import json
import os
import threading
import time
import traceback
from pathlib import Path

import aiohttp
from aiohttp import WSMsgType, web

from homeostat import ConfigWriteError, connect, house, keys

ENV_HOSTS = "HOMEOSTAT_DASHBOARD_HOSTS"
ENV_TILES = "HOMEOSTAT_DASHBOARD_TILES"
ENV_GO2RTC = "HOMEOSTAT_GO2RTC"
DEFAULT_GO2RTC = "http://127.0.0.1:1984"
ALLOWED_NAMES = {"localhost", "homeostat", "homeostat.lan", "homeostat.local"}
WRITE_HEADER = "X-Homeostat"

# Vendored assets served at /assets/{name} — allowlisted by filename so
# the route can't become a path-traversal surface.
ASSETS = {
    "leaflet.js": "text/javascript",
    "leaflet.css": "text/css",
    "protomaps-leaflet.js": "text/javascript",
    "video-rtc.js": "text/javascript",
    "dashboard-logic.js": "text/javascript",
}

# Commandable aspects per capability: the capability's base aspect plus
# whatever features the entity declares. A lock wish still just goes to
# home/cmd at manual band like any other command — for an arbitrated entity
# the arbiter (not the dashboard) is what enforces the family always
# winning over automations.
COMMANDABLE = {
    "light": {"on", "brightness", "color_temp"},
    "lock": {"locked"},
    "switch": {"on"},
    "climate": {"setpoint"},
}
BASE_ASPECT = {"light": "on", "lock": "locked", "switch": "on", "climate": "setpoint"}


def granted_capabilities(publishes: dict) -> set[str]:
    """The capabilities this unit's [bus.publishes] grants it: the ones on
    its cmd-class publishes, which is exactly what `plan` resolves into the
    grant table. A capability COMMANDABLE knows but this manifest does not
    name is refused at /api/cmd and rendered read-only, so the running
    dashboard cannot do what its own grant table says it cannot. Grants
    resolve at plan time (docs/design.md, Grants), so this is the unit
    honouring its declaration, not a boundary against a unit that lies."""
    return {
        spec["capability"]
        for spec in publishes.values()
        if isinstance(spec, dict)
        and spec.get("key", "").startswith("home/cmd/")
        and "capability" in spec
    }


def label_of(naming: dict, name: str) -> str:
    return naming.get("en") or name.replace("_", " ")


def build_model(model: house.HouseModel, granted: set[str]) -> dict:
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
                "commandable": e.capability in granted,
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


def tiles_path() -> Path | None:
    """The house's self-hosted PMTiles extract, if HOMEOSTAT_DASHBOARD_TILES
    is set and points at a real file."""
    raw = os.environ.get(ENV_TILES)
    if not raw:
        return None
    path = Path(raw)
    return path if path.is_file() else None


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
        key = str(sample.key_expr)
        try:
            return key, json.loads(sample.payload.to_bytes())
        except ValueError:
            # Dropped input always leaves a trace; zenoh would just log
            # the callback exception and lose the delta silently.
            self.session.health_event("drop", reason="malformed-payload", key=key)
            return None

    def _on_state(self, sample) -> None:
        if (decoded := self._decode(sample)) is None:
            return
        key, value = decoded
        with self.lock:
            self.state[key] = value
        self._emit({"type": "state", "key": key, "value": value})

    def _on_config(self, sample) -> None:
        if (decoded := self._decode(sample)) is None:
            return
        key, value = decoded
        with self.lock:
            self.config[key] = value
        self._emit({"type": "config", "key": key, "value": value})

    def _on_health(self, sample) -> None:
        if (decoded := self._decode(sample)) is None:
            return
        key, value = decoded
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


class Model:
    """The dashboard's view of the whole house, rebuilt on demand.

    Its inputs are every manifest and every entity file of every unit, so
    a binding added to another adapter changes what this page should show
    while changing none of this unit's own files. The manifest declares
    `inputs = "house"` so `apply` restarts it, and this rebuild means a
    browser refresh is enough even without one — the failure being avoided
    is a dashboard that renders confidently and omits a room that exists.
    """

    def __init__(self, unit: str) -> None:
        self.unit = unit
        self._rebuild(house.load_house("."))

    def _rebuild(self, loaded: house.HouseModel) -> None:
        own = next(u for u in loaded.units if u.name == self.unit)
        self.model = build_model(loaded, granted_capabilities(own.publishes))
        self.entities = {e["name"]: e for e in self.model["entities"]}
        self.units = {u["name"]: u for u in self.model["units"]}
        # go2rtc names each stream by entity id (adapters/go2rtc.py renders
        # its config from the same HOMEOSTAT_CAMERAS keys the onvif adapter
        # addresses cameras by), so the media proxies must ask for the id —
        # and it is not in the browser-facing model, which carries only what
        # the page renders. Resolved here, server-side, where it stays.
        self.stream_names = {
            e.name: e.id for e in loaded.entities if e.capability == "camera"
        }

    def reload(self) -> None:
        try:
            loaded = house.load_house(".")
        except Exception:
            # A half-written edit must not blank the page: keep the last
            # good model and leave a trace (captured at home/meta/{unit}/log).
            traceback.print_exc()
            return
        self._rebuild(loaded)


def make_app(hub: Hub, model: Model, page: Path, assets_dir: Path) -> web.Application:

    async def index(request: web.Request) -> web.StreamResponse:
        return web.FileResponse(page)

    async def api_model(request: web.Request) -> web.Response:
        model.reload()
        return web.json_response(dict(model.model, tiles=tiles_path() is not None))

    async def api_asset(request: web.Request) -> web.StreamResponse:
        name = request.match_info["name"]
        content_type = ASSETS.get(name)
        if content_type is None:
            raise web.HTTPNotFound()
        return web.FileResponse(assets_dir / name, headers={"Content-Type": content_type})

    async def api_tiles(request: web.Request) -> web.StreamResponse:
        path = tiles_path()
        if path is None:
            raise web.HTTPNotFound()
        return web.FileResponse(path)

    async def ws_handler(request: web.Request) -> web.WebSocketResponse:
        origin = request.headers.get("Origin")
        if origin is not None:
            host = origin.split("://", 1)[-1].split("/", 1)[0]
            if not host_allowed(host):
                raise web.HTTPForbidden(text="origin not allowed")
        ws = web.WebSocketResponse(heartbeat=30)
        await ws.prepare(request)
        # Registered before the snapshot: a delta landing between the
        # snapshot build and registration would otherwise miss this client
        # for good. The other order is harmless — a delta broadcast racing
        # the snapshot is included in or superseded by it.
        hub.clients.add(ws)
        await ws.send_str(json.dumps(hub.snapshot()))
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
        spec = model.entities.get(entity)
        if spec is None or spec["room"] != room:
            return json_error(f"unknown entity {room}/{entity}")
        if not spec["commandable"]:
            return json_error(f"dashboard is not granted {spec['capability']} ({entity})")
        allowed = COMMANDABLE.get(spec["capability"], set())
        base = BASE_ASPECT.get(spec["capability"])
        if aspect not in allowed or (aspect != base and aspect not in spec["features"]):
            return json_error(f"{spec['capability']} {entity} takes no {aspect} command")
        # priority "manual": matches this unit's [bus.publishes] declaration
        # (units/dashboard.toml) — the family always wins over automations.
        envelope = keys.cmd_envelope(value, "manual", "dashboard")
        hub.session.put_json(keys.cmd_key(room, entity, aspect), envelope)
        return web.json_response({"ok": True})

    async def api_lights_off(request: web.Request) -> web.Response:
        # "Darken the whole house": family intent over a set of entities,
        # fanned out here at the manual band where the family always wins —
        # never relayed through a virtual entity, whose owner would
        # re-publish at the automation band. Every light gets the command,
        # lit or not: idempotent, and immune to stale state.
        lights = [
            e for e in model.model["entities"] if e["capability"] == "light" and e["commandable"]
        ]
        envelope = keys.cmd_envelope(False, "manual", "dashboard")
        for spec in lights:
            hub.session.put_json(keys.cmd_key(spec["room"], spec["name"], "on"), envelope)
        return web.json_response({"ok": True, "lights": len(lights)})

    async def api_param(request: web.Request) -> web.Response:
        try:
            body = await request.json()
            unit, param, value = str(body["unit"]), str(body["param"]), body["value"]
        except (ValueError, KeyError):
            return json_error("body must be {unit, param, value}")
        spec = model.units.get(unit)
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
        now = datetime.datetime.now(datetime.timezone.utc)
        try:
            hours = min(float(request.query.get("hours", "24")), 24 * 31)
            limit = min(int(request.query.get("limit", "500")), 5000)
            start = now - datetime.timedelta(hours=hours)
        except (ValueError, OverflowError):
            # timedelta raises on NaN/inf hours; same 400 as bad `lines`.
            return json_error("hours and limit must be numbers")
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

    async def api_logs(request: web.Request) -> web.Response:
        unit = request.query.get("unit", "")
        if unit not in model.units:
            return json_error(f"unknown unit {unit}", status=404)
        selector = f"home/meta/{unit}/log"
        lines_param = request.query.get("lines")
        if lines_param is not None:
            try:
                lines = int(lines_param)
            except ValueError:
                return json_error("lines must be an integer")
            selector += f"?lines={max(1, min(lines, 500))}"
        replies = await asyncio.get_running_loop().run_in_executor(
            None, hub.session.get_json, selector
        )
        # A unit with no captured output yet gets no reply — empty, not an error.
        entries = replies[0][1] if replies else []
        return web.json_response(entries)

    def camera_stream(request: web.Request) -> str | None:
        """The go2rtc stream name for a bound camera entity, or None for an
        unknown or non-camera entity (the proxies' 404)."""
        spec = model.entities.get(request.match_info["entity"])
        if spec is None or spec["capability"] != "camera":
            return None
        return model.stream_names[spec["name"]]

    def go2rtc_base() -> str:
        return os.environ.get(ENV_GO2RTC, DEFAULT_GO2RTC).rstrip("/")

    async def api_camera_snapshot(request: web.Request) -> web.Response:
        stream = camera_stream(request)
        if stream is None:
            return json_error(f"unknown camera {request.match_info['entity']}", status=404)
        try:
            async with client["http"].get(
                f"{go2rtc_base()}/api/frame.jpeg",
                params={"src": stream},
                timeout=aiohttp.ClientTimeout(total=10),
            ) as upstream:
                if upstream.status != 200:
                    return json_error("snapshot unavailable", status=502)
                body = await upstream.read()
        except (aiohttp.ClientError, asyncio.TimeoutError):
            return json_error("snapshot unavailable", status=502)
        return web.Response(body=body, content_type="image/jpeg")

    async def api_camera_live(request: web.Request) -> web.WebSocketResponse:
        stream = camera_stream(request)
        if stream is None:
            raise web.HTTPNotFound()
        origin = request.headers.get("Origin")
        if origin is not None:
            host = origin.split("://", 1)[-1].split("/", 1)[0]
            if not host_allowed(host):
                raise web.HTTPForbidden(text="origin not allowed")
        ws = web.WebSocketResponse(heartbeat=30)
        await ws.prepare(request)
        # An opaque byte-for-byte relay to localhost go2rtc — the browser
        # edge of the media plane. Either side closing closes both.
        try:
            async with client["http"].ws_connect(
                f"{go2rtc_base()}/api/ws", params={"src": stream}
            ) as upstream:

                async def pump(source, sink) -> None:
                    async for message in source:
                        if message.type == WSMsgType.TEXT:
                            await sink.send_str(message.data)
                        elif message.type == WSMsgType.BINARY:
                            await sink.send_bytes(message.data)

                directions = [
                    asyncio.create_task(pump(ws, upstream)),
                    asyncio.create_task(pump(upstream, ws)),
                ]
                _done, pending = await asyncio.wait(
                    directions, return_when=asyncio.FIRST_COMPLETED
                )
                for task in pending:
                    task.cancel()
                    with contextlib.suppress(asyncio.CancelledError):
                        await task
        except aiohttp.ClientError:
            pass  # upstream refused: the close below is the browser's signal
        await ws.close()
        return ws

    client: dict = {}

    async def outbound_client(app: web.Application):
        client["http"] = aiohttp.ClientSession()
        yield
        await client["http"].close()

    app = web.Application(middlewares=[guard])
    app.cleanup_ctx.append(outbound_client)
    app.router.add_get("/", index)
    app.router.add_get("/api/model", api_model)
    app.router.add_get("/ws", ws_handler)
    app.router.add_post("/api/cmd", api_cmd)
    app.router.add_post("/api/lights/off", api_lights_off)
    app.router.add_post("/api/param", api_param)
    app.router.add_get("/api/history", api_history)
    app.router.add_get("/api/logs", api_logs)
    app.router.add_get("/api/camera/{entity}/snapshot", api_camera_snapshot)
    app.router.add_get("/api/camera/{entity}/live", api_camera_live)
    app.router.add_get("/assets/{name}", api_asset)
    app.router.add_get("/tiles.pmtiles", api_tiles)
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

    model = Model(os.environ[keys.ENV_UNIT])
    script_dir = Path(__file__).resolve().parent
    page = script_dir / "dashboard.html"
    assets_dir = script_dir / "assets"
    session = connect()
    hub = Hub(session)
    try:
        asyncio.run(serve(make_app(hub, model, page, assets_dir), hub, args.host, args.port))
    finally:
        session.close()


if __name__ == "__main__":
    main()
