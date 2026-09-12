# /// script
# requires-python = ">=3.11"
# dependencies = [
#     "homeostat",
#     "aiohttp>=3.12.14,<4",
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
  GET  /ws           snapshot of state/health/config plus every aspect
                     descriptor adapters publish in their discovery
                     records (docs/design.md, Aspect descriptors), then
                     live deltas
  POST /api/cmd      one command toward a device, published at the manual
                     band ({room, entity, aspect, value}): the capability's
                     vocabulary, or an aspect the entity's descriptor
                     declares a family-editable command, checked against
                     the descriptor's constraint
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
import math
import os
import re
import threading
import time
import traceback
from pathlib import Path

import aiohttp
from aiohttp import WSMsgType, web
from homeostat import ConfigWriteError, connect, house, keys
from homeostat.session import QueryError

ENV_HOSTS = "HOMEOSTAT_DASHBOARD_HOSTS"
ENV_TILES = "HOMEOSTAT_DASHBOARD_TILES"
ENV_GO2RTC = "HOMEOSTAT_GO2RTC"
DEFAULT_GO2RTC = "http://127.0.0.1:1984"
ALLOWED_NAMES = {"localhost", "homeostat", "homeostat.lan", "homeostat.local"}
WRITE_HEADER = "X-Homeostat"
CLIENT_QUEUE = 256  # pending deltas per WebSocket client before it is dropped
MODEL_TTL_S = 2.0  # a burst of page loads parses the house once

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
    "burner": {"on", "power_level"},
}
BASE_ASPECT = {
    "light": "on",
    "lock": "locked",
    "switch": "on",
    "climate": "setpoint",
    "burner": "on",
}
# The vocabulary's value types, in the descriptor-command shape so one
# check serves both: `on`/`locked` are bools (z2m, esphome, the lock
# adapters), `brightness`/`color_temp` numbers (z2m, esphome), `setpoint`
# a float (ivt490), `power_level` a number (aduro's 10/50/100). Bounds
# stay the adapter's (docs/adapters.md, Commands); the type is checked
# here so a JSON object never rides a manual-band envelope onto the bus.
VOCABULARY_COMMANDS = {
    "on": {"type": "bool"},
    "locked": {"type": "bool"},
    "brightness": {"type": "float"},
    "color_temp": {"type": "float"},
    "setpoint": {"type": "float"},
    "power_level": {"type": "float"},
}
# Command bodies are four short fields; aiohttp's 1 MiB default is a
# free memory sink for anything on the LAN.
MAX_BODY_BYTES = 64 * 1024
# The core's rule for one key segment (src/validate.rs): anything else is
# a wildcard, a separator, or a zenoh operator — none of which a browser
# may smuggle into a selector.
SEGMENT = re.compile(r"[A-Za-z0-9_.-]+")
HISTORY_LIMIT_MAX = 5000


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
                "pin": e.pin,
            }
            for e in model.entities
        ],
        "units": [
            {
                "name": u.name,
                "label": label_of(u.naming, u.name),
                "kind": u.kind,
                "description": u.description,
                # Every param, owner-level included: visibility is
                # house-wide, so a tuning constant off its default shows
                # as a deviation and reads in the unit overlay. Only the
                # WRITE is family-gated, at /api/param (#10).
                "params": u.params,
            }
            for u in model.units
        ],
    }


def descriptors_in(inventory) -> dict[str, dict]:
    """The aspect descriptors a discovery document carries, by entity
    name: records binding an entity (`entity` set) that describe its
    aspects (`aspects`, a {schema, groups, fields} object). Anything else
    in the document — unbound devices, raw protocol descriptions — is the
    agent's business, not the page's."""
    if not isinstance(inventory, list):
        return {}
    return {
        r["entity"]: r["aspects"]
        for r in inventory
        if isinstance(r, dict)
        and isinstance(r.get("entity"), str)
        and isinstance(r.get("aspects"), dict)
    }


def descriptor_command(descriptor: dict | None, aspect: str) -> dict | None:
    """The family-editable command a descriptor declares for `aspect`, with
    the field's `values` folded in for enums — or None: undescribed, no
    command, or a tier the family may not write (the /api/param rule)."""
    field = ((descriptor or {}).get("fields") or {}).get(aspect)
    if not isinstance(field, dict):
        return None
    command = field.get("command")
    if not isinstance(command, dict) or command.get("editable_by") != "family":
        return None
    constraint = command.get("constraint") if isinstance(command.get("constraint"), dict) else {}
    for bound in (command.get("step"), constraint.get("min"), constraint.get("max")):
        # A string step would make command_value_ok raise (a 500) and the
        # page would insert it into an attribute: not a command at all.
        if bound is not None and (isinstance(bound, bool) or not isinstance(bound, (int, float))):
            return None
    return dict(command, values=field.get("values") or [])


def command_value_ok(command: dict, value) -> bool:
    """Whether `value` satisfies a descriptor command: a member of an
    enum's values, or a number within the float/int constraint. A
    courtesy check before the bus — the adapter's own bounds are the
    enforcement (docs/design.md, IVT490: bounds live in the adapter)."""
    if command.get("type") == "enum":
        return any(isinstance(v, dict) and v.get("value") == value for v in command["values"])
    if command.get("type") == "bool":
        return isinstance(value, bool)
    if command.get("type") in ("float", "int"):
        if isinstance(value, bool) or not isinstance(value, (int, float)):
            return False
        if not math.isfinite(value):
            return False  # json.loads admits NaN and Infinity
        if command["type"] == "int" and not float(value).is_integer():
            return False
        c = command.get("constraint") or {}
        lo, hi = c.get("min"), c.get("max")
        return (lo is None or value >= lo) and (hi is None or value <= hi)
    return False


def valid_segment(name: str) -> bool:
    return SEGMENT.fullmatch(name) is not None and name not in (".", "..")


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


def mse_request(text: str) -> bool:
    """Whether a browser frame is the player's MSE request — the one
    go2rtc message type the relay forwards."""
    try:
        message = json.loads(text)
    except ValueError:
        return False
    return isinstance(message, dict) and message.get("type") == "mse"


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
        # entity name -> its adapter's aspect descriptor, lifted out of
        # home/discovery/{unit} records: the page renders described
        # aspects through the param-control shapes, and /api/cmd admits
        # the family-editable commands a descriptor declares.
        self.aspects: dict[str, dict] = {}
        # unit -> the entities its last discovery record described, so a
        # record that stops describing one retires the descriptor.
        self.described_by: dict[str, set[str]] = {}
        self.loop: asyncio.AbstractEventLoop | None = None
        # One bounded outbox per client, drained by its own writer task:
        # a browser that stops reading fills its queue and is dropped,
        # never a task per message per client.
        self.clients: dict[web.WebSocketResponse, asyncio.Queue] = {}
        self._subs = []

    def start(self, loop: asyncio.AbstractEventLoop) -> None:
        self.loop = loop
        # Subscribe first, seed after: the mirror holds last values, so a
        # subscription update always supersedes what the seed would write.
        self._subs = [
            self.session.subscribe("home/state/**", self._on_state),
            self.session.subscribe("home/health/**", self._on_health),
            self.session.subscribe("home/config/*/*", self._on_config),
            self.session.subscribe("home/discovery/*", self._on_discovery),
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
        for key, value in self.session.get_json("home/discovery/*"):
            unit = key.split("/")[2]
            with self.lock:
                if unit in self.described_by:
                    continue  # the subscription already delivered fresher
            self._apply_discovery(unit, value)

    def snapshot(self) -> dict:
        with self.lock:
            return {
                "type": "snapshot",
                "state": dict(self.state),
                "health": dict(self.health),
                "config": dict(self.config),
                "aspects": dict(self.aspects),
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

    def _apply_discovery(self, unit: str, value) -> None:
        """Diffs a unit's discovery record against what it described
        before: a descriptor it no longer carries is retired (value null
        on the wire), a new or changed one replaces the old."""
        found = descriptors_in(value)
        changes: list[tuple[str, dict | None]] = []
        with self.lock:
            for entity in self.described_by.get(unit, set()) - found.keys():
                self.aspects.pop(entity, None)
                changes.append((entity, None))
            for entity, descriptor in found.items():
                if self.aspects.get(entity) != descriptor:
                    self.aspects[entity] = descriptor
                    changes.append((entity, descriptor))
            self.described_by[unit] = set(found)
        for entity, descriptor in changes:
            self._emit({"type": "aspects", "entity": entity, "value": descriptor})

    def _on_discovery(self, sample) -> None:
        if (decoded := self._decode(sample)) is None:
            return
        key, value = decoded
        self._apply_discovery(key.split("/")[2], value)

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
        for ws, outbox in list(self.clients.items()):
            try:
                outbox.put_nowait(text)
            except asyncio.QueueFull:
                # A client that cannot keep up gets closed; on reconnect
                # it takes a fresh snapshot, which is what it missed.
                self.clients.pop(ws, None)
                asyncio.ensure_future(ws.close(code=aiohttp.WSCloseCode.TRY_AGAIN_LATER))

    async def writer(self, ws: web.WebSocketResponse, outbox: asyncio.Queue) -> None:
        try:
            while True:
                await ws.send_str(await outbox.get())
        except (ConnectionError, RuntimeError):
            self.clients.pop(ws, None)


def json_error(message: str, status: int = 400) -> web.Response:
    return web.json_response({"error": message}, status=status)


@web.middleware
async def guard(request: web.Request, handler):
    if not host_allowed(request.headers.get("Host", "")):
        return json_error("host not allowed", status=403)
    if request.method == "POST" and WRITE_HEADER not in request.headers:
        return json_error(f"missing {WRITE_HEADER} header", status=403)
    if request.headers.get("Upgrade", "").lower() == "websocket":
        # Browsers always send Origin on a WebSocket handshake; a foreign
        # page's (or an opaque `null`) is refused by the same host rule.
        origin = request.headers.get("Origin")
        if origin is not None:
            host = origin.split("://", 1)[-1].split("/", 1)[0]
            if not host_allowed(host):
                return json_error("origin not allowed", status=403)
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
        self._loaded_at = time.monotonic()
        self._refresh_lock = asyncio.Lock()

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

    @staticmethod
    def _load() -> house.HouseModel | None:
        try:
            return house.load_house(".")
        except Exception:
            # A half-written edit must not blank the page: keep the last
            # good model and leave a trace (captured at home/meta/{unit}/log).
            traceback.print_exc()
            return None

    async def refresh(self) -> None:
        """Re-parses the house off the event loop, at most once per
        MODEL_TTL_S: a burst of page loads costs one parse, and the loop
        keeps serving deltas meanwhile. The swap happens back on the loop,
        so a request never sees half a rebuild."""
        if time.monotonic() - self._loaded_at < MODEL_TTL_S:
            return
        async with self._refresh_lock:
            if time.monotonic() - self._loaded_at < MODEL_TTL_S:
                return
            loaded = await asyncio.get_running_loop().run_in_executor(None, self._load)
            if loaded is not None:
                self._rebuild(loaded)
            self._loaded_at = time.monotonic()


def make_app(hub: Hub, model: Model, page: Path, assets_dir: Path) -> web.Application:

    async def index(request: web.Request) -> web.StreamResponse:
        return web.FileResponse(page)

    async def api_model(request: web.Request) -> web.Response:
        await model.refresh()
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
        ws = web.WebSocketResponse(heartbeat=30)
        await ws.prepare(request)
        # The snapshot is queued first, then the client registered: a
        # delta landing between the snapshot build and registration would
        # otherwise miss this client for good, and one landing after it
        # queues behind the snapshot it is already included in.
        outbox: asyncio.Queue = asyncio.Queue(maxsize=CLIENT_QUEUE)
        outbox.put_nowait(json.dumps(hub.snapshot()))
        hub.clients[ws] = outbox
        writer = asyncio.create_task(hub.writer(ws, outbox))
        try:
            async for message in ws:  # client sends nothing; drain until close
                if message.type == WSMsgType.ERROR:
                    break
        finally:
            hub.clients.pop(ws, None)
            writer.cancel()
            with contextlib.suppress(asyncio.CancelledError):
                await writer
        return ws

    async def api_cmd(request: web.Request) -> web.Response:
        try:
            body = await request.json()
            room, entity = str(body["room"]), str(body["entity"])
            aspect, value = str(body["aspect"]), body["value"]
        except (ValueError, KeyError, TypeError):  # TypeError: a non-object body
            return json_error("body must be {room, entity, aspect, value}")
        spec = model.entities.get(entity)
        if spec is None or spec["room"] != room:
            return json_error(f"unknown entity {room}/{entity}")
        if not spec["commandable"]:
            return json_error(f"dashboard is not granted {spec['capability']} ({entity})")
        allowed = COMMANDABLE.get(spec["capability"], set())
        base = BASE_ASPECT.get(spec["capability"])
        vocabulary = aspect in allowed and (aspect == base or aspect in spec["features"])
        if vocabulary:
            if not command_value_ok(VOCABULARY_COMMANDS[aspect], value):
                return json_error(f"{entity} {aspect}: {value!r} is not a {VOCABULARY_COMMANDS[aspect]['type']}")
        else:
            with hub.lock:
                command = descriptor_command(hub.aspects.get(entity), aspect)
            if command is None:
                return json_error(f"{spec['capability']} {entity} takes no {aspect} command")
            if not command_value_ok(command, value):
                return json_error(f"{entity} {aspect}: {value!r} is outside the declared constraint")
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
        except (ValueError, KeyError, TypeError):
            return json_error("body must be {unit, param, value}")
        spec = model.units.get(unit)
        if spec is None or param not in spec["params"]:
            return json_error(f"no param {unit}.{param}")
        if spec["params"][param].get("editable_by") != "family":
            return json_error(f"{unit}.{param} is not family-editable")
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
        # Verbatim into a selector, a wildcard entity would fan the
        # per-series limit out over the whole store, and `/`, `#` or `$`
        # would raise inside the executor.
        if entity not in model.entities:
            return json_error(f"unknown entity {entity}")
        if not valid_segment(aspect):
            return json_error("aspect must be a single key segment")
        now = datetime.datetime.now(datetime.timezone.utc)
        try:
            hours = min(float(request.query.get("hours", "24")), 24 * 31)
            limit = max(1, min(int(request.query.get("limit", "500")), HISTORY_LIMIT_MAX))
            start = now - datetime.timedelta(hours=hours)
        except (ValueError, OverflowError):
            # timedelta raises on NaN/inf hours; same 400 as bad `lines`.
            return json_error("hours and limit must be numbers")
        selector = (
            f"{keys.history_key('state', entity, aspect)}"
            f"?from={start.isoformat(timespec='seconds')}"
            f";to={now.isoformat(timespec='seconds')};limit={limit}"
        )
        try:
            replies = await asyncio.get_running_loop().run_in_executor(
                None, hub.session.get_json, selector
            )
        except QueryError as error:
            return json_error(f"recorder: {error}", status=502)
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
        ws = web.WebSocketResponse(heartbeat=30)
        await ws.prepare(request)
        # Downstream is an opaque byte-for-byte relay from localhost go2rtc
        # — the browser edge of the media plane. Upstream carries only the
        # MSE request: go2rtc's socket also takes WebRTC offers (ICE to a
        # public STUN server) and the design is MSE-only (docs/design.md,
        # Cameras). Either side closing closes both.
        try:
            async with client["http"].ws_connect(
                f"{go2rtc_base()}/api/ws", params={"src": stream}
            ) as upstream:

                async def downstream() -> None:
                    async for message in upstream:
                        if message.type == WSMsgType.TEXT:
                            await ws.send_str(message.data)
                        elif message.type == WSMsgType.BINARY:
                            await ws.send_bytes(message.data)

                async def mse_requests() -> None:
                    async for message in ws:
                        if message.type == WSMsgType.TEXT and mse_request(message.data):
                            await upstream.send_str(message.data)

                directions = [
                    asyncio.create_task(mse_requests()),
                    asyncio.create_task(downstream()),
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

    app = web.Application(middlewares=[guard], client_max_size=MAX_BODY_BYTES)
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
