# /// script
# requires-python = ">=3.11"
# dependencies = [
#     "homeostat",
#     "aioesphomeapi>=45,<46",
#     "zeroconf>=0.130,<1",
# ]
#
# [tool.uv.sources]
# homeostat = { git = "https://github.com/freol35241/homeostat", subdirectory = "sdk/python", tag = "v0.3.0" }
# ///
"""ESPHome adapter: native API, not MQTT (see docs/design.md, "ESPHome
adapter (settled 2026-07-16)").

One aioesphomeapi connection per BOUND device (never to a device with no
entity file), using the library's own ReconnectLogic — no broker, matching
encryption-default device configs and the dialect voice satellites will
reuse later. The entity file's `id` is `{device}/{object_id}`, the
OwnTracks two-segment shape; the device half resolves via mDNS
(`{device}.local:6053`) unless HOMEOSTAT_ESPHOME_DEVICES (a TOML file
outside the repo) gives that device a `host` override and/or a Noise `key`
— a device with no entry in that file, or an unset env var, is plaintext at
the mDNS default. Addresses and keys never enter the repo.

v1 vocabulary, grown by need, translation driven by what the entity FILE
declares (the z2m pattern — the wire is never trusted to redescribe an
entity the house already bound): switch -> capability "switch", aspect
"on" (bool). light -> "light", aspect "on" plus features "brightness" and
"color_temp" when the device's supported_color_modes carry them. ESPHome's
native brightness is a 0.0-1.0 float and color_temperature a float mired;
z2m's `brightness` aspect is the raw Zigbee 0-254 integer scale (see
zigbee2mqtt.py, and dashboard.html's `b / 254`), so brightness is rescaled
both ways (native * 254 in, /254.0 out) to match that scale exactly;
color_temp is already mireds on both sides and only rounded to an int.
sensor -> "sensor", aspect = the ESPHome device_class if the entity has
one, else its object_id (mirrors z2m's field-name pass-through).
binary_sensor -> "binary_sensor", aspect = device_class or object_id the
same way, EXCEPT device_class motion/occupancy/presence, which the entity
file instead binds as capability "presence" with aspect "occupancy" (a
bool) — z2m's own aspect name for its occupancy exposes, so the dashboard
treats both adapters' presence entities alike. Sensors take no commands.

Commands arrive as envelopes on home/cmd (and home/arbiter for arbitrated
entities, via keys.arbiter_keyexpr instead of keys.cmd_keyexpr — exactly
the zigbee2mqtt.py plan-time-expansion pattern); keys.parse_cmd_envelope
validates, and anything malformed, off-vocabulary, or aimed at a device
that isn't currently connected drops with a home/health/{unit}/event
("invalid-command" or "device-unavailable") instead of reaching the wire.

Discovery (home/discovery/{unit}) carries every entity of every connected
BOUND device (whether that particular entity is itself claimed by an
entity file or not — a device's other entities are exactly the kind of
"not yet claimed" record discovery exists for), each with a best-effort
suggested capability/features stanza and the raw ESPHome type/device_class
verbatim so an unmapped device_class stays visible rather than disappearing.
A best-effort mDNS browse of `_esphomelib._tcp` additionally surfaces
*unbound* device names (nothing to connect to yet, so no entity list) —
its record's `id` is the bare device name; failure or total absence of
mDNS (no multicast, sandboxed network, ...) is guarded completely and
never touches the bound-device connections, which are unaffected either
way. Anything unusable drops with a health event; the unit never crashes.
"""

import asyncio
import contextlib
import json
import os
import signal
import threading
import tomllib
from functools import partial
from pathlib import Path

from aioesphomeapi import (
    APIClient,
    BinarySensorInfo,
    BinarySensorState,
    ColorMode,
    LightInfo,
    LightState,
    ReconnectLogic,
    SensorInfo,
    SensorState,
    SwitchInfo,
    SwitchState,
)
from zeroconf import ServiceStateChange
from zeroconf.asyncio import AsyncServiceBrowser, AsyncServiceInfo, AsyncZeroconf

import homeostat
from homeostat import house, keys

ENV_DEVICES = "HOMEOSTAT_ESPHOME_DEVICES"
MDNS_SERVICE = "_esphomelib._tcp.local."
DEFAULT_PORT = 6053
PRESENCE_DEVICE_CLASSES = {"motion", "occupancy", "presence"}
BRIGHTNESS_SCALE = 254  # z2m's raw Zigbee scale (see zigbee2mqtt.py / dashboard.html)


def load_devices(path: str | None) -> dict:
    """The optional HOMEOSTAT_ESPHOME_DEVICES TOML: per-device `key`
    (Noise PSK) / `host` override. Unset env var: every device plaintext
    at its mDNS default (never a hard requirement)."""
    if not path:
        return {}
    return tomllib.loads(Path(path).read_text())


def resolve_host_port(device: str, devices: dict) -> tuple[str, int]:
    host = (devices.get(device) or {}).get("host")
    if not host:
        return f"{device}.local", DEFAULT_PORT
    if ":" in host:
        h, _, p = host.rpartition(":")
        return h, int(p)
    return host, DEFAULT_PORT


def light_features(modes: list) -> list[str]:
    """Best-effort brightness/color_temp features from a light's
    supported_color_modes (v1 vocabulary, grown by need)."""
    features = []
    if any(m not in (ColorMode.UNKNOWN, ColorMode.ON_OFF) for m in modes):
        features.append("brightness")
    if any(
        m in (ColorMode.COLOR_TEMPERATURE, ColorMode.RGB_COLOR_TEMPERATURE, ColorMode.RGB_COLD_WARM_WHITE)
        for m in modes
    ):
        features.append("color_temp")
    return features


def native_aspect(info) -> str:
    """A sensor/binary_sensor's bus aspect: its device_class if it has
    one, else its object_id — z2m's field-name pass-through, mirrored."""
    return info.device_class or info.object_id


def suggest(info) -> dict | None:
    """Best-effort entity-file stanza for a discovery record — the
    adapter suggests, plan/apply review decides (docs/design.md,
    Discovery)."""
    if isinstance(info, SwitchInfo):
        return {"capability": "switch", "features": []}
    if isinstance(info, LightInfo):
        return {"capability": "light", "features": light_features(info.supported_color_modes)}
    if isinstance(info, SensorInfo):
        return {"capability": "sensor", "features": []}
    if isinstance(info, BinarySensorInfo):
        if info.device_class in PRESENCE_DEVICE_CLASSES:
            return {"capability": "presence", "features": []}
        return {"capability": "binary_sensor", "features": []}
    return None


def describe(info) -> dict:
    """The raw ESPHome descriptor, verbatim, so an unmapped device_class
    or entity kind stays visible instead of disappearing."""
    body = {"type": type(info).__name__.removesuffix("Info"), "object_id": info.object_id}
    if getattr(info, "device_class", ""):
        body["device_class"] = info.device_class
    if isinstance(info, SensorInfo):
        body["unit_of_measurement"] = info.unit_of_measurement
    if isinstance(info, LightInfo):
        body["supported_color_modes"] = [m.name for m in info.supported_color_modes]
    return body


def state_values(entity, info, state):
    """Translates one incoming ESPHome EntityState into (aspect, value)
    pairs on the entity's OWN declared capability/features — translation
    is driven by what the entity file says the device is, never by
    re-deriving it from the wire (the z2m pattern)."""
    if isinstance(state, (SensorState, BinarySensorState)) and state.missing_state:
        return
    if entity.capability == "switch" and isinstance(state, SwitchState):
        yield "on", bool(state.state)
    elif entity.capability == "light" and isinstance(state, LightState):
        yield "on", bool(state.state)
        if "brightness" in entity.features:
            yield "brightness", round(state.brightness * BRIGHTNESS_SCALE)
        if "color_temp" in entity.features:
            yield "color_temp", round(state.color_temperature)
    elif entity.capability == "sensor" and isinstance(state, SensorState):
        yield native_aspect(info), state.state
    elif entity.capability == "presence" and isinstance(state, BinarySensorState):
        yield "occupancy", bool(state.state)
    elif entity.capability == "binary_sensor" and isinstance(state, BinarySensorState):
        yield native_aspect(info), bool(state.state)


async def run_device(device, bound, devices_conf, session, entity_runtime, entity_lock, bound_discovery, publish_discovery):
    """One APIClient + ReconnectLogic for one bound device: on every
    (re)connect, re-enumerates entities (device_info + list_entities, in
    one round trip), republishes this device's discovery slice, and
    (re)subscribes to state."""
    host, port = resolve_host_port(device, devices_conf)
    noise_psk = (devices_conf.get(device) or {}).get("key")
    client = APIClient(host, port, None, client_info="homeostat-esphome", noise_psk=noise_psk)
    key_map: dict[int, tuple] = {}

    async def on_connect() -> None:
        try:
            infos, _services = await client.list_entities_services()
        except Exception as err:
            session.health_event("drop", reason="list-entities-failed", device=device, error=str(err))
            return
        records = []
        new_key_map = {}
        for info in infos:
            entity = bound.get(info.object_id)
            records.append(
                {
                    "id": f"{device}/{info.object_id}",
                    "configured": entity is not None,
                    "entity": entity.name if entity else None,
                    "suggested": suggest(info),
                    "description": describe(info),
                }
            )
            if entity is not None:
                new_key_map[info.key] = (entity, info)
        key_map.clear()
        key_map.update(new_key_map)
        with entity_lock:
            for _key, (entity, info) in new_key_map.items():
                entity_runtime[entity.name] = {"client": client, "key": info.key}
        bound_discovery[device] = records
        publish_discovery()

        def on_state(state) -> None:
            hit = key_map.get(state.key)
            if hit is None:
                return
            entity, info = hit
            for aspect, value in state_values(entity, info, state):
                session.put_json(keys.state_key(entity.room, entity.name, aspect), value)

        client.subscribe_states(on_state)

    async def on_disconnect(_expected: bool) -> None:
        with entity_lock:
            for entity, _info in key_map.values():
                if entity_runtime.get(entity.name, {}).get("client") is client:
                    del entity_runtime[entity.name]
        key_map.clear()

    logic = ReconnectLogic(client=client, on_connect=on_connect, on_disconnect=on_disconnect, name=device)
    await logic.start()
    return client, logic


async def mdns_browse(unit, session, by_device, unbound_discovery, publish_discovery):
    """Best-effort inventory of unbound device names for home/discovery
    (docs/design.md, Discovery / ESPHome adapter): never a prerequisite
    for the bound-device connections, so every failure here is caught and
    reported as a health event rather than raised."""
    try:
        aiozc = AsyncZeroconf()
    except Exception as err:
        session.health_event("drop", reason="mdns-unavailable", error=str(err))
        return

    def on_change(zc, service_type, name, state_change) -> None:
        try:
            dev = name.partition(".")[0]
            if dev in by_device:
                return  # bound: its own connection already covers it
            if state_change is ServiceStateChange.Removed:
                if unbound_discovery.pop(dev, None) is not None:
                    publish_discovery()
                return
            info = AsyncServiceInfo(service_type, name)
            info.load_from_cache(zc)
            props = {
                (k.decode() if isinstance(k, bytes) else k): (
                    v.decode("utf-8", "replace") if isinstance(v, bytes) else v
                )
                for k, v in (info.properties or {}).items()
            }
            unbound_discovery[dev] = {
                "id": dev,
                "configured": False,
                "entity": None,
                "suggested": None,
                "description": {"mdns": props, "port": info.port},
            }
            publish_discovery()
        except Exception as err:
            session.health_event("drop", reason="mdns-record-error", name=name, error=str(err))

    try:
        browser = AsyncServiceBrowser(aiozc.zeroconf, MDNS_SERVICE, handlers=[on_change])
    except Exception as err:
        session.health_event("drop", reason="mdns-unavailable", error=str(err))
        await aiozc.async_close()
        return

    try:
        await asyncio.Event().wait()  # cancelled at shutdown
    finally:
        await browser.async_cancel()
        await aiozc.async_close()


def cmd_handler(entity, entity_runtime, entity_lock, loop, session):
    def handler(sample) -> None:
        key = str(sample.key_expr)
        aspect = key.split("/", 4)[4]
        try:
            payload = json.loads(sample.payload.to_bytes())
            value = keys.parse_cmd_envelope(payload)
        except ValueError:
            session.health_event("drop", reason="invalid-command", key=key)
            return

        with entity_lock:
            target = entity_runtime.get(entity.name)
        if target is None:
            session.health_event("drop", reason="device-unavailable", key=key)
            return
        client, esp_key = target["client"], target["key"]

        if entity.capability == "switch":
            if aspect != "on" or not isinstance(value, bool):
                session.health_event("drop", reason="invalid-command", key=key)
                return
            loop.call_soon_threadsafe(client.switch_command, esp_key, value)
        elif entity.capability == "light":
            if aspect == "on" and isinstance(value, bool):
                loop.call_soon_threadsafe(partial(client.light_command, esp_key, state=value))
            elif (
                aspect == "brightness"
                and "brightness" in entity.features
                and isinstance(value, (int, float))
            ):
                frac = max(0.0, min(1.0, value / BRIGHTNESS_SCALE))
                loop.call_soon_threadsafe(partial(client.light_command, esp_key, brightness=frac))
            elif (
                aspect == "color_temp"
                and "color_temp" in entity.features
                and isinstance(value, (int, float))
            ):
                loop.call_soon_threadsafe(
                    partial(client.light_command, esp_key, color_temperature=float(value))
                )
            else:
                session.health_event("drop", reason="invalid-command", key=key)
        else:
            session.health_event("drop", reason="invalid-command", key=key)  # sensors take no commands

    return handler


async def serve(unit, session, config, devices_conf) -> None:
    loop = asyncio.get_running_loop()
    entity_lock = threading.Lock()
    entity_runtime: dict[str, dict] = {}
    bound_discovery: dict[str, list[dict]] = {}
    unbound_discovery: dict[str, dict] = {}

    def publish_discovery() -> None:
        records = [r for recs in bound_discovery.values() for r in recs]
        records.extend(unbound_discovery.values())
        session.put_json(keys.discovery_key(unit), records)

    by_device: dict[str, dict[str, house.Entity]] = {}
    for entity in config.entities:
        device, _, object_id = entity.id.partition("/")
        by_device.setdefault(device, {})[object_id] = entity

    subscribers = [
        session.subscribe(
            keys.cmd_keyexpr(e.room, e.name), cmd_handler(e, entity_runtime, entity_lock, loop, session)
        )
        for e in config.entities
        if e.write_mode != "arbitrated"
    ] + [
        session.subscribe(
            keys.arbiter_keyexpr(e.room, e.name), cmd_handler(e, entity_runtime, entity_lock, loop, session)
        )
        for e in config.entities
        if e.write_mode == "arbitrated"
    ]

    devices = [
        await run_device(
            device, bound, devices_conf, session, entity_runtime, entity_lock, bound_discovery, publish_discovery
        )
        for device, bound in by_device.items()
    ]
    mdns_task = asyncio.create_task(mdns_browse(unit, session, by_device, unbound_discovery, publish_discovery))

    # Every bound device has a connection attempt in flight (the library's
    # own reconnect logic keeps trying); the mDNS browse is best-effort and
    # never gates this. The unit is wired up.
    session.ready()

    stop = asyncio.Event()
    for sig in (signal.SIGTERM, signal.SIGINT):
        loop.add_signal_handler(sig, stop.set)
    await stop.wait()

    mdns_task.cancel()
    with contextlib.suppress(asyncio.CancelledError):
        await mdns_task
    for client, logic in devices:
        await logic.stop()
        await client.disconnect(force=True)
    for sub in subscribers:
        sub.undeclare()


def main() -> None:
    unit = os.environ[keys.ENV_UNIT]
    config = house.load_adapter(unit)
    devices_conf = load_devices(os.environ.get(ENV_DEVICES))

    session = homeostat.connect()
    try:
        asyncio.run(serve(unit, session, config, devices_conf))
    finally:
        session.close()


if __name__ == "__main__":
    main()
