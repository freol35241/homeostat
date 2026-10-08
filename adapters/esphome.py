# /// script
# requires-python = ">=3.11"
# dependencies = [
#     "homeostat",
#     "aioesphomeapi>=45,<46",
#     "zeroconf>=0.130,<1",
# ]
#
# [tool.uv.sources]
# homeostat = { path = "../sdk/python", editable = true }
# ///
"""ESPHome adapter: ESPHome devices on the bus over the native API.

The adapter keeps one aioesphomeapi connection per bound device, with the
library's ReconnectLogic. It uses the native API instead of MQTT because
it needs no broker, it matches device configs that encrypt by default, and
voice satellites will use the same dialect. A device that no entity file
binds is never connected to.

Binding: an entity file's `id` is `{device}/{object_id}`. The capability
the entity file declares drives translation; the wire does not redescribe
a bound entity (`state_values`):

- switch: capability `switch`, aspect `on`.
- light: capability `light`, aspect `on`, plus `brightness` and
  `color_temp` when declared as features.
- sensor: capability `sensor`, aspect = the device_class, else the
  object_id. Sensors take no commands.
- binary_sensor: capability `binary_sensor`, aspect named the same way,
  or capability `presence` with aspect `occupancy` for device classes
  motion, occupancy and presence.

Configuration: a device is reached at `{device}.local:6053` (mDNS) unless
the TOML file that HOMEOSTAT_ESPHOME_DEVICES names gives it a `host`
override. A Noise `key` in the same file enables encryption. A device with
no entry, or an unset variable, means plaintext at the mDNS default.
Addresses and keys stay out of the repo.

Commands: see `cmd_handler`.

Availability: `available` is true once a device's entities are enumerated
after a connect, and false on an unexpected disconnect.

Discovery (home/discovery/{unit}): every entity of every connected bound
device, bound or not, with an aspect descriptor on the bound ones
(`aspect_descriptor`), plus unbound device names from a best-effort mDNS
browse (`mdns_browse`).

Health events: `drop` (malformed-payload, invalid-command,
device-unavailable, reserved-aspect, list-entities-failed,
mdns-record-error) and mdns-unavailable.
"""

import asyncio
import contextlib
import os
import signal
import threading
from functools import partial
from pathlib import Path

import homeostat
import tomllib
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
from homeostat import house, keys
from zeroconf import ServiceStateChange
from zeroconf.asyncio import AsyncServiceBrowser, AsyncServiceInfo, AsyncZeroconf

ENV_DEVICES = "HOMEOSTAT_ESPHOME_DEVICES"
MDNS_SERVICE = "_esphomelib._tcp.local."
DEFAULT_PORT = 6053
PRESENCE_DEVICE_CLASSES = {"motion", "occupancy", "presence"}
# ESPHome's native brightness is a 0.0-1.0 float. z2m's `brightness` is
# the raw Zigbee 0-254 integer (see zigbee2mqtt.py, and the dashboard's
# `b / 254`), so brightness is rescaled both ways to match it. color_temp
# is mireds on both sides and is only rounded to an int.
BRIGHTNESS_SCALE = 254


def load_devices(path: str | None) -> dict:
    """Load the optional HOMEOSTAT_ESPHOME_DEVICES TOML.

    Per-device `key` (Noise PSK) and `host` override. With the variable
    unset, every device is plaintext at its mDNS default; the file is
    optional.
    """
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
    """Derive best-effort brightness/color_temp features from a light's supported_color_modes."""
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
    """Return a sensor/binary_sensor's bus aspect.

    Its device_class if it has one, else its object_id, like z2m's
    field-name pass-through.
    """
    return info.device_class or info.object_id


def suggest(info) -> dict | None:
    """Suggest a best-effort entity-file stanza for a discovery record.

    The adapter suggests and plan/apply review decides
    (docs/design.md#discovery). motion, occupancy and presence binary
    sensors suggest `presence`, whose `occupancy` aspect is z2m's name for
    its occupancy exposes, so the dashboard treats both adapters' presence
    entities alike.
    """
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


# Binary-sensor device classes whose `true` is out of the ordinary
# (Home Assistant's device-class vocabulary, which ESPHome reuses).
NOTABLE_DEVICE_CLASSES = {
    "smoke", "gas", "carbon_monoxide", "moisture", "problem", "safety", "tamper", "battery",
}
KIND_BY_UNIT = {"°C": "temperature", "%": "percent"}


def aspect_label(info, aspect: str) -> str:
    """Return the device's own entity name, first letter lowercased, as the label.

    The aspect follows in parentheses when the two differ.
    """
    name = (getattr(info, "name", "") or aspect).strip()
    label = name[:1].lower() + name[1:]
    return label if label.replace(" ", "_") == aspect else f"{label} ({aspect})"


def aspect_descriptor(entity, info) -> dict | None:
    """Return the bound entity's aspect descriptor from its EntityInfo.

    Built on the capability the entity file declares, like the
    translation. One ESPHome entity is one bus aspect, so the descriptor
    has one field, or three for a light. A sensor's unit_of_measurement
    picks the kind (°C is temperature, % is percent, anything else a
    number carrying the unit), the ESPHome name is the label, and the
    alarm-like binary device classes in NOTABLE_DEVICE_CLASSES are
    notable. None for a kind this adapter publishes nothing for.
    """
    fields: dict[str, dict] = {}
    if entity.capability == "switch" and isinstance(info, SwitchInfo):
        fields["on"] = {"label": "on", "kind": "boolean", "group": "readings"}
    elif entity.capability == "light" and isinstance(info, LightInfo):
        fields["on"] = {"label": "on", "kind": "boolean", "group": "readings"}
        if "brightness" in entity.features:
            fields["brightness"] = {"label": "brightness", "kind": "number", "group": "readings"}
        if "color_temp" in entity.features:
            fields["color_temp"] = {
                "label": "color temperature (color_temp)", "kind": "number", "unit": "mired",
                "group": "readings",
            }
    elif entity.capability == "sensor" and isinstance(info, SensorInfo):
        aspect = native_aspect(info)
        unit = info.unit_of_measurement or None
        field = {"label": aspect_label(info, aspect), "kind": KIND_BY_UNIT.get(unit, "number"), "group": "readings"}
        if field["kind"] == "number" and unit:
            field["unit"] = unit
        fields[aspect] = field
    elif entity.capability == "presence" and isinstance(info, BinarySensorInfo):
        fields["occupancy"] = {
            "label": aspect_label(info, "occupancy"), "kind": "boolean", "group": "readings",
            "values": [{"value": True, "label": "occupied"}, {"value": False, "label": "clear"}],
        }
    elif entity.capability == "binary_sensor" and isinstance(info, BinarySensorInfo):
        aspect = native_aspect(info)
        field = {"label": aspect_label(info, aspect), "kind": "boolean", "group": "readings"}
        if info.device_class in NOTABLE_DEVICE_CLASSES:
            field["notable"] = True
        fields[aspect] = field
    if not fields:
        return None
    return {"schema": 1, "groups": ["readings"], "fields": fields}


def describe(info) -> dict:
    """Return the raw ESPHome descriptor, verbatim.

    Verbatim so an unmapped device_class or entity kind is still visible
    in discovery.
    """
    body = {"type": type(info).__name__.removesuffix("Info"), "object_id": info.object_id}
    if getattr(info, "device_class", ""):
        body["device_class"] = info.device_class
    if isinstance(info, SensorInfo):
        body["unit_of_measurement"] = info.unit_of_measurement
    if isinstance(info, LightInfo):
        body["supported_color_modes"] = [m.name for m in info.supported_color_modes]
    return body


def state_values(entity, info, state):
    """Translate one incoming ESPHome EntityState into (aspect, value) pairs.

    The pairs follow the entity's declared capability and features. What
    the entity file says the device is drives translation, and the wire is
    not used to re-derive it (as in zigbee2mqtt.py).
    """
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
    """Start one APIClient + ReconnectLogic for one bound device.

    On every (re)connect, it re-enumerates entities (device_info +
    list_entities, in one round trip), republishes this device's discovery
    slice, marks its bound entities available and (re)subscribes to state.
    A device whose entity list cannot be read drops with
    `list-entities-failed` and is disconnected, so ReconnectLogic retries
    it. A state whose aspect would be `available` drops with
    `reserved-aspect`, and one that is not a valid key segment with
    `malformed-payload`.
    """
    host, port = resolve_host_port(device, devices_conf)
    noise_psk = (devices_conf.get(device) or {}).get("key")
    client = APIClient(host, port, None, client_info="homeostat-esphome", noise_psk=noise_psk)
    key_map: dict[int, tuple] = {}

    async def on_connect() -> None:
        try:
            infos, _services = await client.list_entities_services()
        except Exception as err:
            session.health_event("drop", reason="list-entities-failed", device=device, error=str(err))
            # ReconnectLogic is already READY here, so returning would leave
            # the device connected with no state subscription and no retry.
            # Disconnecting re-enters its retry loop. Errors are suppressed
            # because raising out of on_connect would kill the reconnect
            # task.
            with contextlib.suppress(Exception):
                await client.disconnect()
            return
        records = []
        new_key_map = {}
        for info in infos:
            entity = bound.get(info.object_id)
            record = {
                "id": f"{device}/{info.object_id}",
                "configured": entity is not None,
                "entity": entity.name if entity else None,
                "suggested": suggest(info),
                "description": describe(info),
            }
            if entity is not None and (descriptor := aspect_descriptor(entity, info)):
                record["aspects"] = descriptor
            records.append(record)
            if entity is not None:
                new_key_map[info.key] = (entity, info)
        key_map.clear()
        key_map.update(new_key_map)
        with entity_lock:
            for entity, info in new_key_map.values():
                entity_runtime[entity.name] = {"client": client, "key": info.key}
        bound_discovery[device] = records
        publish_discovery()
        for entity, _info in new_key_map.values():
            session.put_json(keys.state_key(entity.room, entity.name, "available"), True)

        def on_state(state) -> None:
            hit = key_map.get(state.key)
            if hit is None:
                return
            entity, info = hit
            for aspect, value in state_values(entity, info, state):
                if aspect == "available":
                    # Reserved for the adapter's own liveness signal.
                    session.health_event(
                        "drop", reason="reserved-aspect", device=device, object_id=info.object_id
                    )
                    continue
                try:
                    key = keys.state_key(entity.room, entity.name, aspect)
                except ValueError:
                    # A device_class/object_id the key schema refuses.
                    session.health_event(
                        "drop", reason="malformed-payload", device=device, object_id=info.object_id
                    )
                    continue
                session.put_json(key, value)

        client.subscribe_states(on_state)

    async def on_disconnect(expected: bool) -> None:
        # A requested disconnect (adapter shutdown) is not a device loss,
        # so only an unexpected one marks the entities unavailable. Other
        # aspects keep their last values.
        if not expected:
            for entity, _info in key_map.values():
                session.put_json(keys.state_key(entity.room, entity.name, "available"), False)
        with entity_lock:
            for entity, _info in key_map.values():
                if entity_runtime.get(entity.name, {}).get("client") is client:
                    del entity_runtime[entity.name]
        key_map.clear()

    logic = ReconnectLogic(client=client, on_connect=on_connect, on_disconnect=on_disconnect, name=device)
    await logic.start()
    return client, logic


async def mdns_browse(unit, session, by_device, unbound_discovery, publish_discovery):
    """Browse mDNS for a best-effort inventory of unbound device names for home/discovery.

    See docs/design.md#discovery. An unbound device's record has the bare
    device name as `id` and no entity list, since there is no connection
    to read one from. The bound-device connections do not depend on this,
    so every failure (no multicast, a sandboxed network, ...) is caught
    and reported: `mdns-unavailable`, or `drop` with `mdns-record-error`
    for one unreadable record.
    """
    try:
        aiozc = AsyncZeroconf()
    except Exception as err:
        session.health_event("mdns-unavailable", error=str(err))
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
        session.health_event("mdns-unavailable", error=str(err))
        await aiozc.async_close()
        return

    try:
        await asyncio.Event().wait()  # cancelled at shutdown
    finally:
        await browser.async_cancel()
        await aiozc.async_close()


def cmd_handler(entity, entity_runtime, entity_lock, loop, session):
    """Return the command handler for one entity.

    A switch takes `on` (a bool). A light takes `on` (a bool), and
    `brightness` and `color_temp` (numbers) when they are declared
    features. Sensors take no commands. A command for a device that is not
    connected drops with `device-unavailable`; anything else that does not
    fit drops with `invalid-command`.
    """
    def handler(sample) -> None:
        parsed = session.parse_command(sample)
        if parsed is None:
            return
        aspect, value, cmd_id = parsed
        key = str(sample.key_expr)

        with entity_lock:
            target = entity_runtime.get(entity.name)
        if target is None:
            session.health_event("drop", reason="device-unavailable", key=key, cmd_id=cmd_id)
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
        session.subscribe(expr, cmd_handler(e, entity_runtime, entity_lock, loop, session))
        for e in config.entities
        for expr in keys.command_keyexprs(e)
    ]

    devices = [
        await run_device(
            device, bound, devices_conf, session, entity_runtime, entity_lock, bound_discovery, publish_discovery
        )
        for device, bound in by_device.items()
    ]
    mdns_task = asyncio.create_task(mdns_browse(unit, session, by_device, unbound_discovery, publish_discovery))

    # Every bound device has a connection attempt in flight, and the
    # library's reconnect logic keeps trying. The mDNS browse is best
    # effort and does not gate readiness.
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
