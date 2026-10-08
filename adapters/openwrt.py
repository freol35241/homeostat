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
"""OpenWrt adapter: router WAN state and WiFi presence over ubus JSON-RPC.

See docs/design.md#the-capability-vocabulary. The adapter speaks ubus
JSON-RPC over HTTP (uhttpd-mod-ubus, rpcd session auth) and polls each
configured router (`poll_router`). It is read-only and takes no commands.

Scope is WAN state and presence. Network metrics and tunnel reachability
belong to the monitoring stack. There is no tunnel state: a wireguard
interface's up flag is true whether or not a peer can be reached, and
per-peer state would publish each peer's endpoint, a movement trace of
whoever carries the device. Combining WiFi sightings with location into
"someone is home" is an ordinary automation, because which MAC is whose
is house knowledge, and this adapter cannot write onto person entities.

Binding: a `router` entity's `id` is its name in the credentials file,
with aspect `wan`. A `presence` entity's `id` is the device MAC in
lowercase, with aspect `presence` (`Adapter.cycle`). `classify` drops
other bindings with a health event.

Configuration: [discovery].endpoint is the path of the credentials file,
normally `${HOMEOSTAT_OPENWRT}`: a TOML file outside the repo keyed by
router name, with `host` (optionally `host:port`), `username` and
`password`. Use a dedicated read-only rpcd ACL login per router, not
root; the ACL needs only `network.interface` and `hostapd.*`. Live
parameters: poll_interval_s (default 30) and away_delay_s (default 180).

Aspects publish on change, plus each entity's value after its first
successful poll, so the recorder stores only transitions.

Discovery: every configured router with whether it answered, and every
station seen this cycle (`Adapter.publish_discovery`).

Health events: `drop` (malformed-id, router-unconfigured,
unsupported-capability), router-unreachable, router-poll-failed,
presence-partial and unknown-interface.
"""

import asyncio
import contextlib
import json
import os
import signal
import time
from pathlib import Path
from typing import ClassVar

import aiohttp
import homeostat
import tomllib
from homeostat import house, keys
from homeostat.params import LiveParams

NULL_SID = "0" * 32
HTTP_TIMEOUT_S = 10
PARAM_DEFAULTS = {"poll_interval_s": 30, "away_delay_s": 180}


class UbusError(Exception):
    """Any failure of a ubus round trip.

    HTTP status, JSON-RPC error, non-zero ubus status code, unparseable
    body.
    """


# A ubus reply is a small JSON document. A larger reply is a failure, so a
# compromised or misbehaving router cannot make the unit buffer it.
MAX_RESPONSE_BYTES = 1024 * 1024
# How much of the body one read takes. Only a buffer size: the cap above
# is what bounds memory, and it is checked after every chunk.
RESPONSE_CHUNK_BYTES = 64 * 1024


async def ubus_rpc(http: aiohttp.ClientSession, url: str, method: str, params: list):
    payload = {"jsonrpc": "2.0", "id": 1, "method": method, "params": params}
    try:
        async with http.post(
            url, json=payload, timeout=aiohttp.ClientTimeout(total=HTTP_TIMEOUT_S),
            # The login travels over plain HTTP to uhttpd's /ubus. A
            # redirect would replay it, password included, at whatever
            # host the reply names.
            allow_redirects=False,
        ) as response:
            if response.status != 200:
                raise UbusError(f"HTTP {response.status}")
            # Read until EOF. `content.read(n)` returns whatever is
            # buffered, up to n, which for a chunked reply is the first
            # chunk, so a single read truncates the document. rpcd answers
            # chunked (OpenWrt 23.x, bodies of 1.4-5.7 kB). The size cap is
            # checked after each chunk, so it does not depend on how the
            # body is framed.
            raw = bytearray()
            async for chunk in response.content.iter_chunked(RESPONSE_CHUNK_BYTES):
                raw += chunk
                if len(raw) > MAX_RESPONSE_BYTES:
                    raise UbusError(f"response exceeds {MAX_RESPONSE_BYTES} bytes")
            data = json.loads(bytes(raw))
    except aiohttp.ClientError as err:
        raise UbusError(str(err)) from err
    except asyncio.TimeoutError as err:
        raise UbusError("timeout") from err
    except ValueError as err:
        raise UbusError(f"unparseable response: {err}") from err
    if not isinstance(data, dict) or "error" in data:
        raise UbusError(f"rpc error: {data.get('error') if isinstance(data, dict) else data}")
    return data.get("result")


async def ubus_call(http, url: str, sid: str, obj: str, method: str, args: dict) -> dict:
    result = await ubus_rpc(http, url, "call", [sid, obj, method, args])
    if not isinstance(result, list) or not result or result[0] != 0:
        raise UbusError(f"{obj}.{method}: status {result!r}")
    return result[1] if len(result) > 1 else {}


async def ubus_list(http, url: str, sid: str, pattern: str) -> list[str]:
    """Return the object names matching a pattern (hostapd.*, one per BSS)."""
    result = await ubus_rpc(http, url, "list", [sid, pattern])
    if isinstance(result, dict):
        return sorted(result)
    if isinstance(result, list) and result and isinstance(result[-1], dict):
        return sorted(result[-1])
    return []


async def login(http, url: str, username: str, password: str) -> str:
    data = await ubus_call(
        http, url, NULL_SID, "session", "login", {"username": username, "password": password}
    )
    sid = data.get("ubus_rpc_session")
    if not sid:
        raise UbusError("login reply missing ubus_rpc_session")
    return sid


class Params(LiveParams):
    """poll_interval_s / away_delay_s from home/config/{unit}/*, live."""

    @property
    def poll_interval_s(self) -> float:
        return max(1, self.get("poll_interval_s"))

    @property
    def away_delay_s(self) -> float:
        return max(0, self.get("away_delay_s"))


def load_routers(endpoint: str | None) -> dict:
    """Load the HOMEOSTAT_OPENWRT TOML behind [discovery].endpoint.

    Per-router host/username/password keyed by router name. Missing or
    unreadable is a startup error (visible via the supervisor's backoff).
    """
    if not endpoint:
        raise ValueError("openwrt adapter requires [discovery].endpoint")
    return tomllib.loads(Path(endpoint).read_text())


def classify(entities, routers, session):
    """Split bound entities by capability, dropping unusable bindings.

    Each dropped binding gets a health event, and the unit keeps running.
    A presence id that is not lowercase drops with `malformed-id`, a router
    id missing from the credentials file with `router-unconfigured`, and
    any other capability with `unsupported-capability`.
    """
    router_entities, trackers = [], []
    for entity in entities:
        if entity.capability == "router":
            if entity.id not in routers:
                session.health_event("drop", reason="router-unconfigured", entity=entity.name)
                continue
            router_entities.append(entity)
        elif entity.capability == "presence":
            if entity.id != entity.id.lower():
                # Sightings are lowercased, so an uppercase MAC would read
                # absent forever with no trace.
                session.health_event(
                    "drop", reason="malformed-id", entity=entity.name,
                    error="device MAC must be lowercase",
                )
                continue
            trackers.append(entity)
        else:
            session.health_event(
                "drop", reason="unsupported-capability", entity=entity.name,
                capability=entity.capability,
            )
    return router_entities, trackers


async def poll_router(http, name: str, conf: dict):
    """One router, one cycle: fresh login, interface dump, station union.

    Each cycle logs in again, because rpcd expires idle sessions. It asks
    `network.interface dump` for WAN state and `get_clients` on every
    hostapd BSS for WiFi sightings. Any failure marks the router
    unreachable.
    """
    url = f"http://{conf['host']}/ubus"
    sid = await login(http, url, conf["username"], conf["password"])

    dump = await ubus_call(http, url, sid, "network.interface", "dump", {})
    interfaces = {
        iface.get("interface"): iface
        for iface in dump.get("interface", [])
        if iface.get("interface")
    }

    stations: set[str] = set()
    for obj in await ubus_list(http, url, sid, "hostapd.*"):
        clients = await ubus_call(http, url, sid, obj, "get_clients", {})
        stations |= {mac.lower() for mac in clients.get("clients", {})}

    return interfaces, stations


class Adapter:
    """Router WAN and device presence state, carried across poll cycles and published on change."""

    def __init__(self, session, routers, router_entities, trackers):
        self.session = session
        self.routers = routers
        self.router_entities = router_entities
        self.trackers = trackers
        self.params = Params(session, PARAM_DEFAULTS)
        self.published: dict[str, object] = {}
        self.last_discovery: str | None = None
        self.reachable: dict[str, bool] = {}
        self.noted: set = set()  # degraded conditions already announced
        self.blind: frozenset = frozenset()  # routers silent as of last cycle
        self.last_seen: dict[str, float] = {}  # mac -> monotonic sighting time

    def note(self, key: tuple, kind: str, **fields) -> None:
        """One health event per down transition of a degraded condition.

        Degraded conditions get their own event kind, not `drop`, since
        nothing was dropped.
        """
        if key not in self.noted:
            self.session.health_event(kind, **fields)
            self.noted.add(key)

    def clear(self, key: tuple) -> None:
        self.noted.discard(key)

    def publish(self, key: str, value) -> None:
        if self.published.get(key) != value:
            self.session.put_json(key, value)
            self.published[key] = value

    async def cycle(self, http) -> None:
        """Poll every router once and publish what changed.

        An unreachable router reports one `router-unreachable` per down
        transition (`router-poll-failed` when it answers with something
        that does not parse), and its aspects keep their last values. With
        every router silent nothing publishes. A router whose dump has no
        interface named `wan` reports `unknown-interface` once, until one
        reappears.

        A presence entity is present when any BSS of any router sighted it
        within away_delay_s. The delay also covers access point reboots.
        """
        interfaces: dict[str, dict] = {}
        sightings: dict[str, list[str]] = {}  # mac -> routers that saw it
        polled_ok: set[str] = set()

        for name, conf in self.routers.items():
            try:
                ifaces, stations = await poll_router(http, name, conf)
            except UbusError as err:
                if self.reachable.get(name, True):
                    self.session.health_event(
                        "router-unreachable", router=name, error=str(err)
                    )
                self.reachable[name] = False
                continue
            except Exception as err:
                # A payload shape this code does not expect (hostapd clients
                # as a list, an interface dump that is not one, ...). The
                # router answered but the answer did not parse. Its aspects
                # go stale and the unit keeps running.
                if self.reachable.get(name, True):
                    self.session.health_event(
                        "router-poll-failed", router=name, error=str(err)
                    )
                self.reachable[name] = False
                continue
            self.reachable[name] = True
            polled_ok.add(name)
            interfaces[name] = ifaces
            for mac in stations:
                sightings.setdefault(mac, []).append(name)

        if not polled_ok:
            return  # no router answered: everything keeps its last value

        now_mono = time.monotonic()
        for mac in sightings:
            self.last_seen[mac] = now_mono

        for entity in self.router_entities:
            if entity.id not in polled_ok:
                continue
            wan = interfaces[entity.id].get("wan")
            if wan is None:
                self.note(("iface", entity.name), "unknown-interface", entity=entity.name, iface="wan")
                continue
            self.clear(("iface", entity.name))
            self.publish(keys.state_key(entity.room, entity.name, "wan"), bool(wan.get("up")))

        # Some routers silent. A sighting still counts, but absence needs
        # every router to see nothing: with one silent, a device on it
        # would read away. Unsighted devices keep their last value, as
        # when no router answers. Report the blind spot each time the set
        # of silent routers changes; router-unreachable is reported once
        # and says nothing about how long the outage lasts.
        blind = frozenset(self.routers) - polled_ok
        if self.trackers and blind != self.blind and blind:
            self.session.health_event("presence-partial", routers=sorted(blind))
        self.blind = blind

        away_delay = self.params.away_delay_s
        for entity in self.trackers:
            seen_at = self.last_seen.get(entity.id)
            present = entity.id in sightings or (
                seen_at is not None and now_mono - seen_at < away_delay
            )
            if not present and blind:
                continue
            self.publish(keys.state_key(entity.room, entity.name, "presence"), present)

        self.publish_discovery(sightings)

    # A class-level constant, shared read-only across instances. One
    # boolean per capability: everything this adapter publishes, so a
    # static table.
    ASPECT_DESCRIPTORS: ClassVar[dict] = {
        "router": {
            "schema": 1,
            "groups": ["readings"],
            "fields": {
                "wan": {
                    "label": "WAN link (wan)", "kind": "boolean", "group": "readings",
                    "values": [{"value": True, "label": "up"}, {"value": False, "label": "down"}],
                }
            },
        },
        "presence": {
            "schema": 1,
            "groups": ["readings"],
            "fields": {
                "presence": {
                    "label": "on the WiFi (presence)", "kind": "boolean", "group": "readings",
                    "values": [{"value": True, "label": "present"}, {"value": False, "label": "away"}],
                }
            },
        },
    }

    def publish_discovery(self, sightings) -> None:
        """Publish the complete current view of the periphery (docs/design.md#discovery).

        Built from data the cycle already fetched, and republished only when
        it changes: every configured router with whether it answered, and
        every station seen this cycle as a bare MAC, suggested `presence`,
        with the routers that saw it.
        """
        bound = {e.id: e.name for e in self.router_entities + self.trackers}

        def record(rid: str, capability: str, description: dict) -> dict:
            rec = {
                "id": rid,
                "configured": rid in bound,
                "entity": bound.get(rid),
                "suggested": {"capability": capability, "features": []},
                "description": description,
            }
            if rid in bound:
                rec["aspects"] = self.ASPECT_DESCRIPTORS[capability]
            return rec

        records = [
            record(name, "router", {"reachable": self.reachable.get(name, False)})
            for name in self.routers
        ]
        for mac in sorted(sightings):
            records.append(record(mac, "presence", {"seen_by": sorted(sightings[mac])}))

        records.sort(key=lambda r: r["id"])
        serialized = json.dumps(records)
        if serialized != self.last_discovery:
            self.session.put_json(keys.discovery_key(self.session.unit), records)
            self.last_discovery = serialized


async def serve(session, routers, config) -> None:
    loop = asyncio.get_running_loop()
    stop = asyncio.Event()
    for sig in (signal.SIGTERM, signal.SIGINT):
        loop.add_signal_handler(sig, stop.set)

    adapter = Adapter(session, routers, *classify(config.entities, routers, session))

    async with aiohttp.ClientSession() as http:
        first = True
        while not stop.is_set():
            await adapter.cycle(http)
            if first:
                # The first cycle has run, whether or not the routers
                # answered; later cycles keep trying.
                session.ready()
                first = False
            with contextlib.suppress(asyncio.TimeoutError):
                await asyncio.wait_for(stop.wait(), timeout=adapter.params.poll_interval_s)


def main() -> None:
    unit = os.environ[keys.ENV_UNIT]
    config = house.load_adapter(unit)
    routers = load_routers(config.endpoint)

    session = homeostat.connect()
    try:
        asyncio.run(serve(session, routers, config))
    finally:
        session.close()


if __name__ == "__main__":
    main()
