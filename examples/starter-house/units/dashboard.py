# /// script
# requires-python = ">=3.11"
# dependencies = [
#     "homeostat==0.17.0",
#     "aiohttp>=3.12.14,<4",
# ]
# ///
"""Dashboard service: the family's web page for the house.

Serves dashboard.html, which sits beside this script with its assets under
assets/, and an HTTP and WebSocket API built from the house's own files.
See docs/design.md#dashboard.

Routes, each documented on its handler in make_app:

  GET  /                          the page
  GET  /api/model                 the house model for the browser
  GET  /ws                        a snapshot of the bus, then live deltas
  POST /api/cmd                   one manual-band command toward a device
  POST /api/lights/off            an off command to every commandable light
  POST /api/param                 a family-editable parameter write
  GET  /api/history               recorder proxy for charts
  GET  /api/forecasts             recorder proxy for stored forecast issues
  GET  /api/source-events         when each source of a computed value was used
  GET  /api/logs                  a unit's captured stdout/stderr tail
  GET  /api/camera/{entity}/live  MSE relay to go2rtc
  GET  /assets/*                  stylesheet, logic, modules, vendored libraries
  GET  /tiles.pmtiles             the map widget's tile extract

The page is for the house network only (LAN or WireGuard) and has no
login. `guard` holds the checks.

Configuration:

- --host (default 0.0.0.0) and --port (default HOMEOSTAT_DASHBOARD_PORT,
  else 8600).
- HOMEOSTAT_DASHBOARD_HOSTS: extra host names the page answers to,
  comma-separated, for reverse-proxy setups. Host names and ports stay
  out of the repo.
- HOMEOSTAT_DASHBOARD_TILES: a PMTiles region extract for the map widget.
  Unset, the map renders without a base layer.
- HOMEOSTAT_GO2RTC: go2rtc's base URL, default http://127.0.0.1:1984.

The manifest declares `watches = "house"`, since the model is built from
every unit's files (see Model).

Health events: `drop` (malformed-payload) for a bus sample that is not JSON.
"""

import argparse
import asyncio
import contextlib
import datetime
import importlib.metadata
import ipaddress
import json
import math
import os
import threading
import time
import traceback
from pathlib import Path

import aiohttp
import zenoh
from aiohttp import WSMsgType, web
from homeostat import ConfigWriteError, connect, house, keys
from homeostat.session import QueryError

ENV_HOSTS = "HOMEOSTAT_DASHBOARD_HOSTS"
ENV_TILES = "HOMEOSTAT_DASHBOARD_TILES"
ENV_GO2RTC = "HOMEOSTAT_GO2RTC"
DEFAULT_GO2RTC = "http://127.0.0.1:1984"
ALLOWED_NAMES = {"localhost", "homeostat", "homeostat.lan", "homeostat.local"}
# The addresses that count as the house's own network: private, loopback,
# link-local and unspecified IPv4; loopback, unspecified, unique-local and
# link-local IPv6. A LAN or a WireGuard tunnel is in these ranges; public
# and special-purpose ranges are not. The MCP server applies the same rule,
# and tests/fixtures/host_gate.json tests both.
HOUSE_NETWORKS = tuple(
    ipaddress.ip_network(network)
    for network in (
        "10.0.0.0/8",
        "172.16.0.0/12",
        "192.168.0.0/16",
        "127.0.0.0/8",
        "169.254.0.0/16",
        "0.0.0.0/32",
        "::1/128",
        "::/128",
        "fc00::/7",
        "fe80::/10",
    )
)
WRITE_HEADER = "X-Homeostat"
CLIENT_QUEUE = 256  # pending deltas per WebSocket client before it is dropped
MODEL_TTL_S = 2.0  # a burst of page loads parses the house once
ABOUT_KEY = "home/meta/system/about"
# The SDK this unit runs against, which a house pins to the release its
# copy of this file and dashboard.html came from (scripts/sync_starter.sh).
try:
    DASHBOARD_VERSION: str | None = importlib.metadata.version("homeostat")
except importlib.metadata.PackageNotFoundError:
    DASHBOARD_VERSION = None

# Assets served at /assets/{name}: the files below by name, and the page's
# modules under assets/dashboard/ (MODULES). Nothing else is served, so the
# route cannot be used for path traversal.
#
# dashboard.html, dashboard.css, dashboard-logic.js and the modules are
# written against each other and change together at an upgrade. aiohttp's
# FileResponse sets ETag and Last-Modified but no Cache-Control, so a
# browser falls back to heuristic freshness, commonly a tenth of the
# file's age. A file unchanged for two weeks is then not revalidated for
# about a day. An upgrade inside that day pairs the new page with the old
# logic, and the page renders empty and ignores taps while the backend is
# healthy and the log is clean. This is most likely after a long stable
# release, and worst on a phone, which has no console and no easy hard
# reload.
#
# `no-cache` means "cache it, but revalidate before use". The ETag makes
# the revalidation a 304. Versioned asset URLs with long caching are not
# used: the version would have to reach every `src` and `import` in files
# that are meant to stay hand-editable, either by rewriting them at serve
# time or by editing them at every release, and sync_starter.sh exists to
# avoid hand-edited versions.
REVALIDATE = {"Cache-Control": "no-cache"}

ASSETS = {
    "leaflet.js": "text/javascript",
    "leaflet.css": "text/css",
    "protomaps-leaflet.js": "text/javascript",
    "video-rtc.js": "text/javascript",
    "dashboard-logic.js": "text/javascript",
    "dashboard.css": "text/css",
    "homeostat-mark.svg": "image/svg+xml",
}
# The page's ES modules: any .js file under this directory of assets/, by
# a path that resolves inside it once `..` and symlinks are followed. A
# browser runs a module only when it is served as JavaScript.
MODULES = "dashboard"

# Commandable aspects per capability: the capability's base aspect plus
# the features the entity declares. A lock command goes to home/cmd at the
# manual band like any other. For an arbitrated entity it is the arbiter
# that makes the family win over automations.
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
# a float (ivt490), `power_level` a number (aduro's 10/50/100). The
# adapter checks bounds (docs/adapters.md, Commands). The type is checked
# here so a JSON object cannot reach the bus in a manual-band envelope.
VOCABULARY_COMMANDS = {
    "on": {"type": "bool"},
    "locked": {"type": "bool"},
    "brightness": {"type": "float"},
    "color_temp": {"type": "float"},
    "setpoint": {"type": "float"},
    "power_level": {"type": "float"},
}
# Command bodies are four short fields. aiohttp's 1 MiB default would let
# anything on the LAN make the dashboard buffer far more than it needs.
MAX_BODY_BYTES = 64 * 1024
HISTORY_LIMIT_MAX = 5000
# How far before a drawn window to read source-participation events. A
# source excluded before the window opened has no transition inside it
# and would read as used for the whole span. A week covers a sensor that
# has been ignored for a while without reading all of history.
SOURCE_EVENT_LOOKBACK_H = 24 * 7
SOURCE_EVENT_MAX = 500

# Issues one /api/forecasts reply may carry. A week of hourly issues is
# 168 full horizons: megabytes on the wire and an unreadable chart. The
# page asks for the newest few and says how many it drew.
FORECAST_ISSUE_MAX = 200


def granted_capabilities(publishes: dict) -> set[str]:
    """Return the capabilities this unit's [bus.publishes] grants it.

    They are the capabilities on its cmd-class publishes, which `plan`
    resolves into the grant table. A capability COMMANDABLE knows but this
    manifest does not name is refused at /api/cmd and rendered read-only,
    so the dashboard stays within its own grants. Grants are resolved at
    plan time (docs/design.md#commanding); this check is the unit keeping
    to its declaration and is not a security boundary.
    """
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
                # What this entity's value is derived from, where it is
                # computed: the history overlay draws these beside it.
                "sources": {
                    name: {
                        "entity": src.entity,
                        "aspect": src.aspect,
                        "note": src.note,
                        "precision": src.precision,
                    }
                    for name, src in e.sources.items()
                },
            }
            for e in model.entities
        ],
        "units": [
            {
                "name": u.name,
                "label": label_of(u.naming, u.name),
                "kind": u.kind,
                "description": u.description,
                # Every param, owner-level ones included, so a tuning
                # constant off its default shows as a deviation in the
                # unit overlay. Only writes are limited to family-editable
                # params, at /api/param.
                "params": u.params,
                "subscribes": u.subscribes,
            }
            for u in model.units
        ],
        # dashboard.toml's views as written (core-validated), or null: the
        # page then renders its generated views.
        "views": model.views,
        # The step each named control moves in, keyed by what is
        # controlled, so the page applies it wherever that control is
        # drawn: room card, view or overlay.
        "controls": model.controls,
    }


def commandable_aspects(entity: dict, descriptor: dict | None) -> set[str]:
    """Return the aspects an entity takes commands on.

    They are its capability's vocabulary (the base aspect plus the features
    it declares) and whatever its descriptor declares a command for. These
    are the two sources /api/cmd accepts, so the card names no field the
    page would refuse.
    """
    allowed = COMMANDABLE.get(entity["capability"], set())
    base = BASE_ASPECT.get(entity["capability"])
    aspects = {a for a in allowed if a == base or a in entity["features"]}
    fields = (descriptor or {}).get("fields")
    if isinstance(fields, dict):
        aspects |= {a for a, f in fields.items() if isinstance(f, dict) and f.get("command")}
    return aspects


def key_expr(key: str):
    """Return a zenoh key expression, or None when the string is not one.

    The core's manifest parser admits some shapes zenoh refuses (`**/**`,
    a `$`), and the model is re-read while files are being edited. A bad
    expression is skipped so it does not turn into a 500 for every browser.
    """
    try:
        return zenoh.KeyExpr(key)
    except zenoh.ZError:
        return None


def driven_aspects(model: dict, grants: list, descriptors: dict[str, dict]) -> dict[str, str]:
    """Map each commandable aspect to the lowest band any unit may command it at.

    Keyed "room/entity/aspect" -> band.

    The page uses this to decide whether an arbiter hold is a deviation: a
    hold displaces someone when it is above a band some unit normally
    writes at. The grant table is the source, because it says who may
    command what at which band, resolved at plan time. Inferring it from an
    automation's last refusal would depend on how often that automation
    publishes (docs/design.md#views-are-text).
    """
    entities = {e["name"]: e for e in model["entities"]}
    lowest: dict[str, str] = {}
    for g in grants:
        if not (isinstance(g, dict) and g.get("capability") and isinstance(g.get("priority"), str)):
            continue
        band = g["priority"]
        if band not in keys.CMD_PRIORITIES:
            continue
        granted = [k for k in (key_expr(k) for k in g.get("keys", [])) if k is not None]
        for e in g.get("entities", []):
            if not (isinstance(e, dict) and isinstance(e.get("name"), str)):
                continue
            spec = entities.get(e["name"])
            if spec is None:
                continue
            for aspect in commandable_aspects(spec, descriptors.get(e["name"])):
                try:
                    ke = zenoh.KeyExpr(keys.cmd_key(e["room"], e["name"], aspect))
                except (KeyError, ValueError, zenoh.ZError):
                    continue
                if not any(g_ke.intersects(ke) for g_ke in granted):
                    continue
                slot = f"{e['room']}/{e['name']}/{aspect}"
                current = lowest.get(slot)
                if current is None or keys.CMD_PRIORITIES.index(band) < keys.CMD_PRIORITIES.index(
                    current
                ):
                    lowest[slot] = band
    return lowest


def unit_relations(
    model: dict, grants: list, state_keys, descriptors: dict[str, dict]
) -> dict[str, dict]:
    """Return, per unit, the fields it drives and the fields it reads.

    These are the unit card's Drives and From sections, derived from what
    the manifests declare. Each is an {entity, aspect} pair. A relation is
    per aspect, and an entity is often both driven and read (an automation
    commands a lamp's `on` and subscribes to it), so listing whole entities
    would show the same entity twice without saying which aspect.

    Drives: the entities on the unit's cmd-class rows of the grant table
    (home/meta/system/grants), with each commandable aspect whose cmd key
    the grant's resolved keys reach. The keys are part of a grant's
    identity, so `.../on` and `.../**` are different grants. An entity
    whose commandable aspects are unknown (no vocabulary, no descriptor
    yet) keeps a bare row (`aspect: null`).

    Reads: each [bus.subscribes] state expression, with its room slot
    expanded through the zones as the core expands it, intersected with
    the concrete state keys on the bus. Matching against
    `{room}/{entity}/**` instead would make every entity in a room a source
    of a `*/presence` subscription.
    """
    zones = model["zones"]
    entities = {e["name"]: e for e in model["entities"]}
    # Keys the bus delivered are well-formed.
    concrete: dict = {}
    for key in state_keys:
        parts = key.split("/")
        if len(parts) >= 5 and parts[0] == "home" and parts[1] == "state":
            concrete[zenoh.KeyExpr(key)] = (parts[3], "/".join(parts[4:]))
    out = {}
    for unit in model["units"]:
        drives: set[tuple[str, str | None]] = set()
        for g in grants:
            if not (isinstance(g, dict) and g.get("unit") == unit["name"] and g.get("capability")):
                continue
            granted = [k for k in (key_expr(k) for k in g.get("keys", [])) if k is not None]
            for e in g.get("entities", []):
                if not (isinstance(e, dict) and isinstance(e.get("name"), str)):
                    continue
                spec = entities.get(e["name"])
                aspects = commandable_aspects(spec, descriptors.get(e["name"])) if spec else set()
                reached = set()
                for aspect in aspects:
                    try:
                        ke = zenoh.KeyExpr(keys.cmd_key(e["room"], e["name"], aspect))
                    except (KeyError, ValueError, zenoh.ZError):
                        continue  # a grant row the core would not have written
                    if any(g_ke.intersects(ke) for g_ke in granted):
                        reached.add(aspect)
                drives.update((e["name"], aspect) for aspect in reached or {None})
        sources: set[tuple[str, str]] = set()
        for expr in unit["subscribes"].values():
            if not isinstance(expr, str):
                continue
            parts = expr.split("/")
            if len(parts) < 4 or parts[0] != "home" or parts[1] != "state":
                continue
            if "{" in expr:
                continue  # a template over the unit's own entities: Publishes, not From
            for room in zones.get(parts[2], [parts[2]]):
                ke = key_expr("/".join(parts[:2] + [room] + parts[3:]))
                if ke is None:
                    continue
                sources.update(field for key, field in concrete.items() if ke.intersects(key))
        out[unit["name"]] = {
            "drives": [{"entity": e, "aspect": a} for e, a in sorted(drives, key=field_order)],
            "sources": [{"entity": e, "aspect": a} for e, a in sorted(sources, key=field_order)],
        }
    return out


def field_order(field: tuple[str, str | None]) -> tuple[str, str]:
    return (field[0], field[1] or "")


def descriptors_in(inventory) -> dict[str, dict]:
    """Return the aspect descriptors a discovery document carries, by entity name.

    They come from records binding an entity (`entity` set) that describe
    its aspects (`aspects`, a {schema, groups, fields} object). The rest of
    the document (unbound devices, raw protocol descriptions) is for agents
    and is ignored here.
    """
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
    """Return the family-editable command a descriptor declares for `aspect`, or None.

    The field's `values` are folded in for enums. None means undescribed,
    no command, or a tier the family may not write (the /api/param rule).
    """
    field = ((descriptor or {}).get("fields") or {}).get(aspect)
    if not isinstance(field, dict):
        return None
    command = field.get("command")
    if not isinstance(command, dict) or command.get("editable_by") != "family":
        return None
    constraint = command.get("constraint") if isinstance(command.get("constraint"), dict) else {}
    for bound in (command.get("step"), constraint.get("min"), constraint.get("max")):
        # A non-numeric bound would make command_value_ok raise (a 500),
        # and the page would insert it into an attribute, so treat the
        # field as having no command.
        if bound is not None and (isinstance(bound, bool) or not isinstance(bound, (int, float))):
            return None
    return dict(command, values=field.get("values") or [])


def command_value_ok(command: dict, value) -> bool:
    """Return whether `value` satisfies a descriptor command.

    It must be a member of an enum's values, or a number within the
    float/int constraint. This is an early check for the browser's sake;
    the adapter enforces its own bounds (docs/design.md#aspect-descriptors).
    """
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


def reachable(session, spec: dict, aspect: str) -> bool:
    """Return whether a command for `aspect` of the entity can reach its device.

    It can when the owning unit holds its liveliness token and something
    subscribes to the key that unit listens on: the arbiter's forward key
    for an arbitrated entity, the command key otherwise. A command that
    reaches no one leaves no trace, so without this the page could only
    find out by waiting for its timeout.

    Liveliness is checked first, because a subscriber on the command key
    proves little: the recorder subscribes to every command, and the
    arbiter to every one it arbitrates. Blocking; /api/cmd runs it off the
    event loop.
    """
    if not session.is_alive(spec["owner"]):
        return False
    room, entity = spec["room"], spec["name"]
    if spec["write_mode"] == "arbitrated":
        return session.has_subscriber(keys.arbiter_key(room, entity, aspect))
    return session.has_subscriber(keys.cmd_key(room, entity, aspect))


def host_of(value: str) -> str:
    """Return the host part of a Host header value.

    A port is stripped only when it is all digits, and a bracketed IPv6
    literal is unwrapped. The MCP server parses it the same way
    (src/mcp/http.rs).
    """
    if value.startswith("["):
        return value[1:].split("]", 1)[0]
    host, sep, port = value.rpartition(":")
    return host if sep and all(c in "0123456789" for c in port) else value


def host_allowed(host_header: str) -> bool:
    """Whether a Host (or WS Origin host) is one the house answers to.

    That is a house-network address (HOUSE_NETWORKS) or a known name. A
    rebound public domain arrives as its own name and is refused.
    """
    host = host_of(host_header)
    if host in ALLOWED_NAMES:
        return True
    extra = {h.strip() for h in os.environ.get(ENV_HOSTS, "").split(",") if h.strip()}
    if host in extra:
        return True
    try:
        address = ipaddress.ip_address(host)
    except ValueError:
        return False
    return any(address in network for network in HOUSE_NETWORKS)


def mse_request(text: str) -> bool:
    """Return whether a browser frame is the player's MSE request.

    That is the one go2rtc message type the relay forwards.
    """
    try:
        message = json.loads(text)
    except ValueError:
        return False
    return isinstance(message, dict) and message.get("type") == "mse"


def tiles_path() -> Path | None:
    """Return the house's self-hosted PMTiles extract, or None.

    Only if HOMEOSTAT_DASHBOARD_TILES is set and points at a real file.
    """
    raw = os.environ.get(ENV_TILES)
    if not raw:
        return None
    path = Path(raw)
    return path if path.is_file() else None


class Hub:
    """Bus-facing caches plus WebSocket fan-out.

    Zenoh callbacks arrive on zenoh threads; deltas cross into asyncio via
    call_soon_threadsafe.
    """

    def __init__(self, session):
        self.session = session
        self.lock = threading.Lock()
        self.state: dict[str, object] = {}
        # The current forecast per home/forecast key, kept live like state
        # instead of read from the recorder. The page shows what the house
        # believes now, and a house without a recorder still sees its
        # forecasts. The recorder's copy is for checking past forecasts
        # (/api/forecasts).
        self.forecasts: dict[str, object] = {}
        # home/hold/{unit} -> what that arbiter is holding now. The
        # preempt/refuse events say what happened; this says what is in
        # force, which a browser opening during a hold needs
        # (docs/design.md#arbitrated-mode).
        self.holds: dict[str, object] = {}
        self.health: dict[str, object] = {}
        self.config: dict[str, object] = {}
        # entity name -> its adapter's aspect descriptor, taken from the
        # home/discovery/{unit} records. The page renders described aspects
        # with the param controls, and /api/cmd accepts the family-editable
        # commands a descriptor declares.
        self.aspects: dict[str, dict] = {}
        # unit -> the entities its last discovery record described, so a
        # record that stops describing one retires the descriptor.
        self.described_by: dict[str, set[str]] = {}
        # The grant table, read once at start. Grants change only with a
        # manifest, and a manifest change restarts this unit.
        self.grants: list = []
        self.loop: asyncio.AbstractEventLoop | None = None
        # One bounded outbox per client, drained by its own writer task. A
        # browser that stops reading fills its queue and is dropped. This
        # avoids a task per message per client.
        self.clients: dict[web.WebSocketResponse, asyncio.Queue] = {}
        self._subs = []
        self._about: dict = {}
        self._about_at = -MODEL_TTL_S

    def start(self, loop: asyncio.AbstractEventLoop) -> None:
        self.loop = loop
        # Subscribe first, then seed from the mirror. The seed uses
        # setdefault, so a value the subscription already delivered wins.
        self._subs = [
            self.session.subscribe("home/state/**", self._on_state),
            self.session.subscribe("home/forecast/**", self._on_forecast),
            self.session.subscribe("home/hold/*", self._on_hold),
            self.session.subscribe("home/health/**", self._on_health),
            self.session.subscribe("home/config/*/*", self._on_config),
            self.session.subscribe("home/discovery/*", self._on_discovery),
        ]
        for key, value in self.session.get_json("home/state/**"):
            with self.lock:
                self.state.setdefault(key, value)
        for key, value in self.session.get_json("home/forecast/**"):
            with self.lock:
                self.forecasts.setdefault(key, value)
        for key, value in self.session.get_json("home/hold/*"):
            with self.lock:
                self.holds.setdefault(key, value)
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
        for _key, value in self.session.get_json("home/meta/system/grants"):
            if isinstance(value, list):
                self.grants = value

    def about(self) -> dict:
        """Return the versions the page's footer shows.

        The core's `about` (its version and build commit, the house commit
        last applied) plus this unit's SDK version, which is the release its
        dashboard.html was copied from. A house pins the two together
        (scripts/sync_starter.sh), so a core and a dashboard from different
        releases are visible as such.

        Blocking (a bus query), so it runs off the event loop, and at most
        once per MODEL_TTL_S. The house commit changes on an apply that
        does not restart this unit, so it is re-read instead of cached for
        the unit's life. A failed query keeps the last answer.
        """
        if time.monotonic() - self._about_at >= MODEL_TTL_S:
            # The footer is not worth failing /api/model over, so any error
            # keeps the last answer.
            with contextlib.suppress(Exception):
                for _key, value in self.session.get_json(ABOUT_KEY, timeout_s=2):
                    if isinstance(value, dict):
                        self._about = value
            self._about_at = time.monotonic()
        return dict(self._about, dashboard={"version": DASHBOARD_VERSION})

    def relations(self, model: dict) -> dict[str, dict]:
        with self.lock:
            state_keys = list(self.state)
            descriptors = dict(self.aspects)
        return unit_relations(model, self.grants, state_keys, descriptors)

    def driven(self, model: dict) -> dict[str, str]:
        with self.lock:
            descriptors = dict(self.aspects)
        return driven_aspects(model, self.grants, descriptors)

    def snapshot(self) -> dict:
        with self.lock:
            return {
                "type": "snapshot",
                "state": dict(self.state),
                "forecasts": dict(self.forecasts),
                "holds": dict(self.holds),
                "health": dict(self.health),
                "config": dict(self.config),
                "aspects": dict(self.aspects),
            }

    def _decode(self, sample):
        key = str(sample.key_expr)
        try:
            return key, json.loads(sample.payload.to_bytes())
        except ValueError:
            # Report the drop. An exception here would only be logged by
            # zenoh, and the delta would be lost without a trace.
            self.session.health_event("drop", reason="malformed-payload", key=key)
            return None

    def _on_state(self, sample) -> None:
        if (decoded := self._decode(sample)) is None:
            return
        key, value = decoded
        with self.lock:
            self.state[key] = value
        self._emit({"type": "state", "key": key, "value": value})

    def _on_hold(self, sample) -> None:
        if (decoded := self._decode(sample)) is None:
            return
        key, value = decoded
        with self.lock:
            self.holds[key] = value
        self._emit({"type": "hold", "key": key, "value": value})

    def _on_forecast(self, sample) -> None:
        if (decoded := self._decode(sample)) is None:
            return
        key, value = decoded
        with self.lock:
            self.forecasts[key] = value
        self._emit({"type": "forecast", "key": key, "value": value})

    def _apply_discovery(self, unit: str, value) -> None:
        """Diff a unit's discovery record against what it described before.

        A descriptor it no longer carries is retired (value null on the
        wire); a new or changed one replaces the old.
        """
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
                # A client that cannot keep up is closed. On reconnect it
                # gets a fresh snapshot, which covers what it missed.
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
    """Refuse requests that do not come from the house network.

    Being on the network is the credential, so the checks are about where a
    request comes from, not who sends it. The Host header must be a
    house-network address or an allowed name (ALLOWED_NAMES plus
    HOMEOSTAT_DASHBOARD_HOSTS), which defeats DNS rebinding. A POST needs
    the X-Homeostat header, which a cross-site form cannot set. A
    WebSocket Origin, when present, must pass the same host rule.
    """
    if not host_allowed(request.headers.get("Host", "")):
        return json_error("host not allowed", status=403)
    if request.method == "POST" and WRITE_HEADER not in request.headers:
        return json_error(f"missing {WRITE_HEADER} header", status=403)
    if request.headers.get("Upgrade", "").lower() == "websocket":
        # Browsers send Origin on a WebSocket handshake. Another site's
        # origin, or an opaque `null`, fails the same host rule.
        origin = request.headers.get("Origin")
        if origin is not None:
            host = origin.split("://", 1)[-1].split("/", 1)[0]
            if not host_allowed(host):
                return json_error("origin not allowed", status=403)
    return await handler(request)


class Model:
    """The dashboard's view of the whole house, rebuilt on demand.

    Its inputs are every manifest and every entity file of every unit, so
    a binding added to another adapter changes what this page shows
    without changing any of this unit's own files. The manifest declares
    `watches = "house"` so `apply` restarts it, and the rebuild means a
    browser refresh picks up a change even without a restart. Otherwise
    the page could omit a room that exists with nothing to show it.
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
        # addresses cameras by), so the media proxy asks for the id. Ids
        # are not in the browser-facing model, so the lookup stays here.
        self.stream_names = {
            e.name: e.id for e in loaded.entities if e.capability == "camera"
        }

    @staticmethod
    def _load() -> house.HouseModel | None:
        try:
            return house.load_house(".")
        except Exception:
            # A half-written edit must not blank the page. Keep the last
            # good model and print the error (captured at
            # home/meta/{unit}/log).
            traceback.print_exc()
            return None

    async def refresh(self) -> None:
        """Re-parse the house off the event loop, at most once per MODEL_TTL_S.

        A burst of page loads costs one parse, and the loop keeps serving
        deltas meanwhile. The swap happens back on the loop, so a request
        never sees half a rebuild.
        """
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
        return web.FileResponse(page, headers=REVALIDATE)

    async def api_model(request: web.Request) -> web.Response:
        """GET /api/model: the house model rendered for the browser.

        Zones, entities, units and the views of dashboard.toml. Each entity
        is marked commandable when this unit's own manifest grants its
        capability. Each unit carries the fields it drives (from the grant
        table) and reads (from its subscriptions), for the unit card.
        `about` has the versions for the footer, `driven` the lowest band
        each aspect is commanded at, and `tiles` whether a tile extract is
        configured.
        """
        await model.refresh()
        about = await asyncio.get_running_loop().run_in_executor(None, hub.about)
        relations = hub.relations(model.model)
        units = [dict(u, **relations[u["name"]]) for u in model.model["units"]]
        return web.json_response(
            dict(
                model.model,
                units=units,
                tiles=tiles_path() is not None,
                # Which aspects something drives, and at what band. The page
                # compares an arbiter hold against this to tell "the family
                # took over from the heating" from "the family locked a door
                # nothing automates" (docs/design.md#views-are-text).
                driven=hub.driven(model.model),
                about=about,
            )
        )

    async def api_asset(request: web.Request) -> web.StreamResponse:
        """GET /assets/*: an allowlisted file, or a module under assets/dashboard/.

        The vendored libraries (Leaflet, protomaps-leaflet, the go2rtc
        player) and the page's own files are in ASSETS by name. A module is
        any .js file whose path resolves inside assets/dashboard/.
        """
        name = request.match_info["name"]
        content_type = ASSETS.get(name)
        path = assets_dir / name
        if content_type is None and name.endswith(".js"):
            modules = (assets_dir / MODULES).resolve()
            # A path with a NUL raises ValueError; it gets a 404 like any
            # other missing file.
            with contextlib.suppress(ValueError):
                path = path.resolve()
                if modules in path.parents and path.is_file():
                    content_type = "text/javascript"
        if content_type is None:
            raise web.HTTPNotFound()
        return web.FileResponse(path, headers={"Content-Type": content_type, **REVALIDATE})

    async def api_tiles(request: web.Request) -> web.StreamResponse:
        """GET /tiles.pmtiles: the HOMEOSTAT_DASHBOARD_TILES file, or 404 if unset."""
        path = tiles_path()
        if path is None:
            raise web.HTTPNotFound()
        return web.FileResponse(path)

    async def ws_handler(request: web.Request) -> web.WebSocketResponse:
        """GET /ws: a snapshot, then live deltas.

        The snapshot holds state, forecasts, holds, health, config and every
        aspect descriptor adapters publish in their discovery records
        (docs/design.md#aspect-descriptors).
        """
        ws = web.WebSocketResponse(heartbeat=30)
        await ws.prepare(request)
        # Queue the snapshot, then register the client, with no await in
        # between. A delta broadcast after registration queues behind the
        # snapshot, which may already include it.
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
        """POST /api/cmd {room, entity, aspect, value}: one command at the manual band.

        The aspect must be in the capability's vocabulary, or one the
        entity's descriptor declares as a family-editable command, and the
        value must pass that command's type and constraint. The reply
        carries the envelope's id and `heard`, whether the command can
        reach its device (see `reachable`).
        """
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
        # The manual band matches this unit's [bus.publishes] declaration
        # (units/dashboard.toml). The family wins over automations.
        envelope = keys.cmd_envelope(value, "manual", "dashboard")
        key = keys.cmd_key(room, entity, aspect)
        heard = await asyncio.get_running_loop().run_in_executor(
            None, reachable, hub.session, spec, aspect
        )
        hub.session.put_json(key, envelope)
        # The id goes back to the browser so the control can show the
        # command as pending until something ends it: a readback, an
        # arbiter refusal or an adapter's drop.
        return web.json_response({"ok": True, "id": envelope["id"], "heard": heard})

    async def api_lights_off(request: web.Request) -> web.Response:
        """POST /api/lights/off: one manual-band off command per commandable light.

        The fan-out happens here, at the manual band. A virtual entity
        relaying it would republish at the automation band
        (docs/design.md#commanding). Every light gets the command whether
        it is lit or not, so the result does not depend on possibly stale
        state.
        """
        lights = [
            e for e in model.model["entities"] if e["capability"] == "light" and e["commandable"]
        ]
        envelope = keys.cmd_envelope(False, "manual", "dashboard")
        for spec in lights:
            hub.session.put_json(keys.cmd_key(spec["room"], spec["name"], "on"), envelope)
        return web.json_response({"ok": True, "lights": len(lights)})

    async def api_param(request: web.Request) -> web.Response:
        """POST /api/param {unit, param, value}: a family-editable parameter write.

        Goes through the core's validating config queryable.
        """
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
        """GET /api/history?entity=..&aspect=..&hours=..: recorder proxy for charts.

        Optional bucket=<s> gives one point per bucket and changes=1 a
        state's runs, the recorder's chart shapes. class=state (the
        default) is what the house did, class=cmd what was asked of it; a
        command strip under a chart shows intent against outcome.
        """
        entity = request.query.get("entity", "")
        aspect = request.query.get("aspect", "")
        if not entity or not aspect:
            return json_error("entity and aspect are required")
        # The two series classes the recorder keeps per (entity, aspect).
        # The page asks for no other.
        series_class = request.query.get("class", "state")
        if series_class not in ("state", "cmd"):
            return json_error("class must be state or cmd")
        # These go into a selector as they are. A wildcard entity would
        # spread the per-series limit over the whole store, and `/`, `#` or
        # `$` would raise inside the executor.
        if entity not in model.entities:
            return json_error(f"unknown entity {entity}")
        if not keys.valid_segment(aspect):
            return json_error("aspect must be a single key segment")
        now = datetime.datetime.now(datetime.timezone.utc)
        try:
            hours = min(float(request.query.get("hours", "24")), 24 * 31)
            limit = max(1, min(int(request.query.get("limit", "500")), HISTORY_LIMIT_MAX))
            bucket = int(request.query.get("bucket", "0"))
            start = now - datetime.timedelta(hours=hours)
        except (ValueError, OverflowError):
            # timedelta raises OverflowError on NaN or infinite hours.
            return json_error("hours, limit and bucket must be numbers")
        changes = request.query.get("changes") == "1"
        if bucket < 0 or (bucket and hours * 3600 / bucket > HISTORY_LIMIT_MAX):
            # The recorder refuses a fold finer than a reply can carry. The
            # page does not ask for one, so this is a plain 400.
            return json_error(f"bucket must be positive and no finer than {HISTORY_LIMIT_MAX} per window")
        if bucket and changes:
            return json_error("bucket and changes are exclusive")
        selector = (
            f"{keys.history_key(series_class, entity, aspect)}"
            f"?from={start.isoformat(timespec='seconds')}"
            f";to={now.isoformat(timespec='seconds')};limit={limit}"
        )
        # The recorder's chart shapes (docs/design.md#read-path): one point
        # per bucket for a line, or the runs of a state for a timeline.
        if bucket:
            selector += f";bucket={bucket}"
        elif changes:
            selector += ";changes=1"
        try:
            replies = await asyncio.get_running_loop().run_in_executor(
                None, hub.session.get_json, selector
            )
        except QueryError as error:
            return json_error(f"recorder: {error}", status=502)
        return web.json_response(
            {"series": [{"key": key, "points": points} for key, points in replies]}
        )

    async def api_forecasts(request: web.Request) -> web.Response:
        """GET /api/forecasts: the forecast issues the recorder kept for one aspect.

        These are what the house predicted at the time, where /ws carries
        what it predicts now. A separate route from /api/history because
        the reply is a different shape: history answers in rows, this in
        issues. `limit` counts issues, as the recorder does, so a reply
        never holds part of an issue.
        """
        entity = request.query.get("entity", "")
        aspect = request.query.get("aspect", "")
        if not entity or not aspect:
            return json_error("entity and aspect are required")
        if entity not in model.entities:
            return json_error(f"unknown entity {entity}")
        if not keys.valid_segment(aspect):
            return json_error("aspect must be a single key segment")
        now = datetime.datetime.now(datetime.timezone.utc)
        try:
            hours = min(float(request.query.get("hours", "24")), 24 * 31)
            limit = max(1, min(int(request.query.get("limit", "60")), FORECAST_ISSUE_MAX))
            start = now - datetime.timedelta(hours=hours)
        except (ValueError, OverflowError):
            return json_error("hours and limit must be numbers")
        # The window is the drawn one. An issue is kept when it said
        # anything about the window, so a forecast made before the window
        # that reaches into it is included. The wildcard in the source slot
        # returns every provider for this aspect, each as its own series
        # (docs/design.md#forecasts).
        selector = (
            f"{keys.history_key('forecast', entity, aspect)}/*"
            f"?valid_from={start.isoformat(timespec='seconds')}"
            f";valid_to={now.isoformat(timespec='seconds')};limit={limit}"
        )
        try:
            replies = await asyncio.get_running_loop().run_in_executor(
                None, hub.session.get_json, selector
            )
        except QueryError as error:
            return json_error(f"recorder: {error}", status=502)
        # The reply key's last segment is the source. Tagging each issue
        # with it gives the page one flat list, since the chart draws
        # issues, and still says which provider each line came from.
        issues = []
        for key, payload in replies:
            source = str(key).rsplit("/", 1)[-1]
            for issue in payload or []:
                issues.append({**issue, "source": source})
        return web.json_response({"issues": issues})

    async def api_source_events(request: web.Request) -> web.Response:
        """GET /api/source-events: when each declared source of a computed value went in or out.

        Declared sources say what may contribute; these events say what did
        (docs/design.md#which-sources-a-computation-actually-used). The
        window read is wider than the window drawn by
        SOURCE_EVENT_LOOKBACK_H (see there).
        """
        entity = request.query.get("entity", "")
        aspect = request.query.get("aspect", "")
        if entity not in model.entities:
            return json_error(f"unknown entity {entity}")
        if not keys.valid_segment(aspect):
            return json_error("aspect must be a single key segment")
        owner = model.entities[entity].get("owner")
        if not owner:
            return web.json_response({"events": []})
        try:
            hours = min(float(request.query.get("hours", "24")), 24 * 31)
        except (ValueError, OverflowError):
            return json_error("hours must be a number")
        now = datetime.datetime.now(datetime.timezone.utc)
        # The recorder's events path takes integer microseconds; its
        # samples path takes RFC3339.
        to_us = int(now.timestamp() * 1_000_000)
        from_us = to_us - int((hours + SOURCE_EVENT_LOOKBACK_H) * 3600 * 1_000_000)
        selector = (
            f"home/history/events?key=home/health/{owner}/event"
            f";from={from_us};to={to_us};limit={SOURCE_EVENT_MAX}"
        )
        try:
            replies = await asyncio.get_running_loop().run_in_executor(
                None, hub.session.get_json, selector
            )
        except QueryError as error:
            return json_error(f"recorder: {error}", status=502)
        out = []
        for _key, payload in replies:
            for row in payload or []:
                event = row.get("payload") or {}
                if isinstance(event, str):
                    continue
                kind = event.get("kind")
                if kind not in ("source-dropped", "source-restored"):
                    continue
                if event.get("entity") != entity or event.get("aspect") != aspect:
                    continue
                out.append(
                    {
                        "ts": row.get("ts"),
                        "source": event.get("source"),
                        "used": kind == "source-restored",
                    }
                )
        out.sort(key=lambda e: e["ts"] or 0)
        return web.json_response({"events": out})

    async def api_logs(request: web.Request) -> web.Response:
        """GET /api/logs?unit=..&lines=N: a unit's captured stdout/stderr tail.

        Proxies the supervisor's home/meta/{unit}/log queryable, for the
        unit detail overlay.
        """
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
        # A unit with no captured output yet sends no reply; return an
        # empty list.
        entries = replies[0][1] if replies else []
        return web.json_response(entries)

    def camera_stream(request: web.Request) -> str | None:
        """Return the go2rtc stream name for a bound camera entity, or None.

        None for an unknown or non-camera entity (the proxy's 404).
        """
        spec = model.entities.get(request.match_info["entity"])
        if spec is None or spec["capability"] != "camera":
            return None
        return model.stream_names[spec["name"]]

    def go2rtc_base() -> str:
        return os.environ.get(ENV_GO2RTC, DEFAULT_GO2RTC).rstrip("/")

    async def api_camera_live(request: web.Request) -> web.WebSocketResponse:
        """GET /api/camera/{entity}/live: a WebSocket relayed to go2rtc's api/ws (MSE).

        Browsers do not talk to go2rtc directly (docs/design.md#cameras).
        The stream is addressed by the camera's entity id, resolved here.
        There is no snapshot route: a still frame needs a transcode
        (go2rtc's frame.jpeg runs ffmpeg for an H.264 source), and the
        media path only remuxes.
        """
        stream = camera_stream(request)
        if stream is None:
            raise web.HTTPNotFound()
        ws = web.WebSocketResponse(heartbeat=30)
        await ws.prepare(request)
        # Downstream relays go2rtc's frames byte for byte. Upstream passes
        # only the MSE request: go2rtc's socket also takes WebRTC offers
        # (ICE to a public STUN server), and the design is MSE-only.
        # Either side closing closes both.
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
            pass  # go2rtc refused; closing the browser's socket tells it
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
    app.router.add_get("/api/forecasts", api_forecasts)
    app.router.add_get("/api/source-events", api_source_events)
    app.router.add_get("/api/logs", api_logs)
    app.router.add_get("/api/camera/{entity}/live", api_camera_live)
    app.router.add_get("/assets/{name:.+}", api_asset)
    app.router.add_get("/tiles.pmtiles", api_tiles)
    return app


async def serve(app: web.Application, hub: Hub, host: str, port: int) -> None:
    hub.start(asyncio.get_running_loop())
    runner = web.AppRunner(app)
    await runner.setup()
    site = web.TCPSite(runner, host, port)
    await site.start()
    hub.session.ready()  # ready once the server accepts connections

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
