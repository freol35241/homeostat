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
"""OpenWrt network adapter (see docs/design.md, "Network presence and
connectivity (settled 2026-07-25)").

Named for the dialect it speaks: ubus JSON-RPC over HTTP (uhttpd-mod-ubus,
rpcd session auth). The first polling adapter — each cycle logs in fresh
(rpcd expires idle sessions; nothing to renew) and asks every configured
router three questions: `network.interface dump` for WAN and tunnel state,
`get_clients` on every hostapd BSS for WiFi sightings, and the
`luci.wireguard` status call for handshakes when a bound tunnel's proto is
wireguard. Scope is presence and connectivity state only; network metrics
are the monitoring stack's job, deliberately.

The manifest's [discovery].endpoint is the HOMEOSTAT_OPENWRT credentials
file itself: an out-of-repo TOML keyed by router name with `host`
(optionally `host:port`), `username`, `password` — a dedicated read-only
rpcd ACL login per router, never root. Entity binding: a `router` entity's
id is its name in that file (aspect `wan`); a `vpn` entity's id is
`{router}/{netifd-interface}` (aspect `up` — for proto wireguard, up means
interface up AND a peer handshake younger than 180s, so monitored tunnels
must run persistent-keepalive); a `presence` entity's id is the device MAC,
lowercase (aspect `presence` — sighted on any BSS of any router, absent
only after away_delay_s of continuous non-sighting, which also absorbs AP
reboots).

Aspects publish on transition only, plus each entity's current value after
its first successful poll; a poll is a read, not an event. An unreachable
router drops with one "router-unreachable" health event per down
transition and its aspects go stale rather than false. Read-only by
design: no cmd surface until a command is actually wanted.
A bound entity's discovery record carries its aspect descriptor (docs/
design.md, Aspect descriptors): one boolean per capability, with value
labels ("up"/"down", "present"/"away") — the whole of what this adapter
speaks, so a static table, not a generated one.
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
WG_HANDSHAKE_FRESH_S = 180  # WG rekeys ~2min under traffic; keepalive assumed
PARAM_DEFAULTS = {"poll_interval_s": 30, "away_delay_s": 180}


class UbusError(Exception):
    """Any failure of a ubus round trip: HTTP status, JSON-RPC error,
    non-zero ubus status code, unparseable body."""


# A ubus reply is a small JSON document; a compromised or misbehaving
# router must not pin the unit's memory on an oversized one.
MAX_RESPONSE_BYTES = 1024 * 1024


async def ubus_rpc(http: aiohttp.ClientSession, url: str, method: str, params: list):
    payload = {"jsonrpc": "2.0", "id": 1, "method": method, "params": params}
    try:
        async with http.post(
            url, json=payload, timeout=aiohttp.ClientTimeout(total=HTTP_TIMEOUT_S),
            # A redirect would replay the login call (the plaintext rpcd
            # password included) at whatever host the reply names.
            allow_redirects=False,
        ) as response:
            if response.status != 200:
                raise UbusError(f"HTTP {response.status}")
            raw = await response.content.read(MAX_RESPONSE_BYTES + 1)
            if len(raw) > MAX_RESPONSE_BYTES:
                raise UbusError(f"response exceeds {MAX_RESPONSE_BYTES} bytes")
            data = json.loads(raw)
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
    """Object names matching a pattern (hostapd.* — one per BSS)."""
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


def wireguard_fresh(iface_status, now_epoch: float) -> bool:
    """A live tunnel has some peer handshake younger than the freshness
    window; peers arrive as a list or dict depending on the luci version.
    Entries of any other shape count as no handshake — this runs outside
    the per-router poll guard, so it must not raise."""
    peers = iface_status.get("peers", []) if isinstance(iface_status, dict) else []
    if isinstance(peers, dict):
        peers = list(peers.values())
    latest = 0
    for peer in peers:
        if not isinstance(peer, dict):
            continue
        with contextlib.suppress(TypeError, ValueError):
            latest = max(latest, int(peer.get("latest_handshake", 0)))
    return latest > 0 and now_epoch - latest < WG_HANDSHAKE_FRESH_S


class Params(LiveParams):
    """poll_interval_s / away_delay_s from home/config/{unit}/*, live."""

    @property
    def poll_interval_s(self) -> float:
        return max(1, self.get("poll_interval_s"))

    @property
    def away_delay_s(self) -> float:
        return max(0, self.get("away_delay_s"))


def load_routers(endpoint: str | None) -> dict:
    """The HOMEOSTAT_OPENWRT TOML behind [discovery].endpoint: per-router
    host/username/password keyed by router name. Missing or unreadable is
    a startup error (visible via the supervisor's backoff)."""
    if not endpoint:
        raise ValueError("openwrt adapter requires [discovery].endpoint")
    return tomllib.loads(Path(endpoint).read_text())


def classify(entities, routers, session):
    """Splits bound entities by capability, dropping unusable bindings
    with a health event each — one bad entity never takes the unit down."""
    router_entities, vpn_entities, trackers = [], [], []
    for entity in entities:
        if entity.capability == "router":
            if entity.id not in routers:
                session.health_event("drop", reason="router-unconfigured", entity=entity.name)
                continue
            router_entities.append(entity)
        elif entity.capability == "vpn":
            router_name, _, iface = entity.id.partition("/")
            if not iface or router_name not in routers:
                session.health_event(
                    "drop",
                    reason="router-unconfigured" if iface else "malformed-id",
                    entity=entity.name,
                )
                continue
            vpn_entities.append(entity)
        elif entity.capability == "presence":
            if entity.id != entity.id.lower():
                # Sightings are lowercased; an uppercase MAC would just
                # read absent forever with no trace.
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
    return router_entities, vpn_entities, trackers


async def poll_router(http, name: str, conf: dict, wg_ifaces: set[str]):
    """One router, one cycle: fresh login, interface dump, station union,
    and wireguard status when a bound tunnel needs it. Any failure of the
    login/dump/hostapd path marks the router unreachable; a failed
    wireguard call degrades only the wireguard tunnels (wg_status None)."""
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

    wg_status: dict | None = {}
    if any(interfaces.get(iface, {}).get("proto") == "wireguard" for iface in wg_ifaces):
        try:
            wg_status = await ubus_call(http, url, sid, "luci.wireguard", "getWireguardStatus", {})
        except UbusError:
            wg_status = None

    return interfaces, stations, wg_status


class Adapter:
    def __init__(self, session, routers, router_entities, vpn_entities, trackers):
        self.session = session
        self.routers = routers
        self.router_entities = router_entities
        self.vpn_entities = vpn_entities
        self.trackers = trackers
        self.params = Params(session, PARAM_DEFAULTS)
        self.published: dict[str, object] = {}
        self.last_discovery: str | None = None
        self.reachable: dict[str, bool] = {}
        self.noted: set = set()  # degraded conditions already announced
        self.last_seen: dict[str, float] = {}  # mac -> monotonic sighting time

    def note(self, key: tuple, kind: str, **fields) -> None:
        """One health event per down transition of a degraded condition.
        Degraded conditions publish kind = condition (the recorder's
        backend-outage precedent) — nothing was dropped."""
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
        wg_by_router: dict[str, set[str]] = {}
        for entity in self.vpn_entities:
            router, _, iface = entity.id.partition("/")
            wg_by_router.setdefault(router, set()).add(iface)

        interfaces: dict[str, dict] = {}
        wg_status: dict[str, dict | None] = {}
        sightings: dict[str, list[str]] = {}  # mac -> routers that saw it
        polled_ok: set[str] = set()

        for name, conf in self.routers.items():
            try:
                ifaces, stations, wg = await poll_router(
                    http, name, conf, wg_by_router.get(name, set())
                )
            except UbusError as err:
                if self.reachable.get(name, True):
                    self.session.health_event(
                        "router-unreachable", router=name, error=str(err)
                    )
                self.reachable[name] = False
                continue
            except Exception as err:
                # A payload shape this build did not anticipate (hostapd
                # clients as a list, a non-dict wireguard peer, ...): the
                # router answered, the answer just did not parse. Stale, not
                # a crash loop — one bad router never takes the unit down.
                if self.reachable.get(name, True):
                    self.session.health_event(
                        "router-poll-failed", router=name, error=str(err)
                    )
                self.reachable[name] = False
                continue
            self.reachable[name] = True
            polled_ok.add(name)
            interfaces[name] = ifaces
            wg_status[name] = wg
            for mac in stations:
                sightings.setdefault(mac, []).append(name)

        if not polled_ok:
            return  # total blindness: everything stays stale

        now_mono = time.monotonic()
        now_epoch = time.time()
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

        for entity in self.vpn_entities:
            router, _, iface_name = entity.id.partition("/")
            if router not in polled_ok:
                continue
            iface = interfaces[router].get(iface_name)
            if iface is None:
                self.note(("iface", entity.name), "unknown-interface", entity=entity.name, iface=iface_name)
                continue
            self.clear(("iface", entity.name))
            up = bool(iface.get("up"))
            if iface.get("proto") == "wireguard":
                status = wg_status[router]
                if status is None:
                    self.note(("wg", router), "wireguard-status-unavailable", router=router)
                    continue
                self.clear(("wg", router))
                up = up and wireguard_fresh(status.get(iface_name, {}), now_epoch)
            self.publish(keys.state_key(entity.room, entity.name, "up"), up)

        away_delay = self.params.away_delay_s
        for entity in self.trackers:
            seen_at = self.last_seen.get(entity.id)
            present = entity.id in sightings or (
                seen_at is not None and now_mono - seen_at < away_delay
            )
            self.publish(keys.state_key(entity.room, entity.name, "presence"), present)

        self.publish_discovery(interfaces, sightings)

    # A class-level constant, shared read-only across instances — never
    # mutated per router.
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
        "vpn": {
            "schema": 1,
            "groups": ["readings"],
            "fields": {
                "up": {
                    "label": "tunnel (up)", "kind": "boolean", "group": "readings",
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

    def publish_discovery(self, interfaces, sightings) -> None:
        """The complete current view of the periphery (docs/design.md,
        Discovery), from data the cycle already fetched; republished only
        when it changes."""
        bound = {e.id: e.name for e in self.router_entities + self.vpn_entities + self.trackers}

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
        for router, ifaces in interfaces.items():
            for name, iface in ifaces.items():
                device = str(iface.get("device", ""))
                tunnel = iface.get("proto") == "wireguard" or device.startswith(("tun", "tap"))
                if tunnel or f"{router}/{name}" in bound:
                    records.append(
                        record(
                            f"{router}/{name}",
                            "vpn",
                            {"proto": iface.get("proto"), "up": bool(iface.get("up"))},
                        )
                    )
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
                # The first cycle has run (reachable or not — its own loop
                # keeps trying); the unit is wired up.
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
