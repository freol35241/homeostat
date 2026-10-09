# /// script
# requires-python = ">=3.11"
# dependencies = [
#     "homeostat==0.17.0",
#     "paho-mqtt>=2,<3",
# ]
# ///
"""Zigbee2MQTT adapter: Zigbee devices on the bus through a z2m bridge's MQTT topics.

Binding: an entity file's `id` is the device's z2m topic segment, its
friendly name or IEEE address. The file stem is the entity name. A
friendly name containing "/" is not supported, because the device
subscription is {base}/+.

State: JSON on {base}/{id} fans out to home/state/{room}/{entity}/{aspect}.
z2m's `state` field becomes `on` (lights, switches) or `locked` (locks);
other scalar fields keep their z2m names. When availability is enabled in
the z2m config, {base}/{id}/availability becomes the reserved `available`
aspect. Without it the aspect does not appear. `Bridge.on_message` has the
details.

Commands: the capability's base aspect (`on` or `locked`), a declared
`brightness` or `color_temp`, or a command the device's exposes describe.
Each is sent to {base}/{id}/set; `command_body` has the translation.

Discovery: the retained {base}/bridge/devices inventory is republished at
home/discovery/{unit}, and each bound device's record carries an aspect
descriptor generated from its exposes (`inventory`, `describe`).

Configuration: the base topic is the endpoint's path
(`mqtt://broker:1883/VP52/zigbee2mqtt`), default `zigbee2mqtt`. Broker
credentials are inline in the endpoint or in the TOML file that
HOMEOSTAT_MQTT_CREDENTIALS names, keyed by hostname. The live parameter
inventory_timeout_s (owner-editable, default 30 s) is how long to wait for
the inventory at startup.

Health events: `drop` (malformed-payload, unknown-device, reserved-aspect,
invalid-command), and `bridge-silent` when the inventory does not arrive in
time or the bridge reports itself offline.
"""

import json
import os
import threading
import time

import homeostat
from homeostat import house, keys, mqtt
from homeostat.params import LiveParams

DEFAULT_BASE_TOPIC = "zigbee2mqtt"
# The retained bridge/devices inventory arrives on subscribe, so silence
# past this means the base topic is wrong (see bridge_watchdog in main).
PARAM_DEFAULTS = {"inventory_timeout_s": 30.0}


class Params(LiveParams):
    """inventory_timeout_s from home/config/{unit}/*, live."""

    @property
    def inventory_timeout_s(self) -> float:
        return max(0.1, self.get("inventory_timeout_s"))


def availability_state(payload: bytes):
    """Return `online`/`offline` from an availability-shaped payload, or None.

    The payload is the {"state": "online"|"offline"} object or the legacy
    bare string; None if it is neither. Used for device availability and
    for the bridge's own state.
    """
    raw = payload.decode(errors="replace").strip()
    try:
        parsed = json.loads(raw)
    except ValueError:
        parsed = raw  # legacy availability payload: a bare string
    state = parsed.get("state") if isinstance(parsed, dict) else parsed
    return state if state in ("online", "offline") else None


def state_aspect(capability: str, z2m_field: str, value):
    """Map one z2m JSON field to a (bus aspect, JSON value) pair.

    `state` becomes `locked` for a lock and `on` otherwise, as a bool.
    Every other field keeps its z2m name and value.
    """
    if z2m_field == "state":
        if capability == "lock":
            return "locked", value == "LOCKED"
        return "on", value == "ON"
    return z2m_field, value


def suggest(exposes):
    """Suggest a best-effort entity-file stanza from a z2m exposes descriptor.

    None when no confident mapping exists. The record carries the raw
    definition either way, so an agent can still see the device.
    """
    for exp in exposes:
        if not isinstance(exp, dict):
            continue
        if exp.get("type") == "light":
            features = exp.get("features")
            inner = {
                f.get("property")
                for f in (features if isinstance(features, list) else [])
                if isinstance(f, dict)
            }
            return {
                "capability": "light",
                "features": ["brightness"] if "brightness" in inner else [],
            }
        if exp.get("type") == "lock":
            return {"capability": "lock", "features": []}
        if exp.get("type") == "binary" and exp.get("property") == "occupancy":
            return {"capability": "presence", "features": []}
    return None


# Binary exposes whose `true` is out of the ordinary, by z2m property name
# (docs/design.md#aspect-descriptors: notable).
NOTABLE_BINARY = frozenset(
    {"battery_low", "water_leak", "smoke", "gas", "carbon_monoxide", "tamper", "vibration"}
)
KIND_BY_UNIT = {"°C": "temperature", "%": "percent"}
# Exposes that z2m 1.34 and later categorise as diagnostic. Older z2m has
# no `category` field, so these are recognised by property. Voltage is
# diagnostic only as a battery voltage (mV); a plug's mains voltage (V) is
# a reading.
DIAGNOSTIC_PROPERTIES = frozenset({"linkquality"})
# The capability vocabulary, which the dashboard's own widgets command.
# These are described as readings and get no descriptor command.
VOCABULARY_ASPECTS = frozenset({"on", "locked", "brightness", "color_temp"})
# The capability's base aspect (docs/manifest.md, Capability vocabulary):
# the one command every entity of that capability takes.
BASE_ASPECT = {"light": "on", "switch": "on", "lock": "locked"}
ACCESS_SET = 2  # z2m access bitmask: 1 published, 2 settable, 4 gettable
SPECIFIC_TYPES = ("light", "switch", "lock", "cover", "climate", "fan")


def describe(capability: str, exposes) -> dict | None:
    """Return the entity's aspect descriptor from its z2m exposes.

    Every scalar expose becomes a labelled field
    (docs/design.md#aspect-descriptors):

    - z2m's unit picks the kind: °C is temperature, % is percent, anything
      else is a number that carries the unit.
    - z2m's category picks the group: diagnostic goes to diagnostics,
      config to config, everything else to readings. Without a category
      (z2m before 1.34), DIAGNOSTIC_PROPERTIES and a battery voltage in mV
      go to diagnostics.
    - battery is a reading, ordered last.
    - A settable numeric or enum expose becomes an owner-tier command,
      with z2m's own value bounds.
    - The alarm-like binaries in NOTABLE_BINARY are notable.
    - The capability's vocabulary (VOCABULARY_ASPECTS) is described as
      readings only, because the dashboard's own widget controls it.

    None when nothing scalar is exposed.
    """
    fields: dict[str, dict] = {}

    def add(exp) -> None:
        if not isinstance(exp, dict):
            return
        etype = exp.get("type")
        if etype in SPECIFIC_TYPES:
            for feature in exp.get("features") or []:
                add(feature)
            return
        prop = exp.get("property")
        if not isinstance(prop, str) or etype not in ("numeric", "binary", "enum", "text"):
            return  # composite and list exposes are not published as state either
        aspect, _ = state_aspect(capability, prop, None)
        if aspect == "available" or aspect in fields:
            return
        unit = exp.get("unit") if isinstance(exp.get("unit"), str) else None
        category = exp.get("category")
        group = {"diagnostic": "diagnostics", "config": "config"}.get(category, "readings")
        if category is None and (prop in DIAGNOSTIC_PROPERTIES or (prop == "voltage" and unit == "mV")):
            group = "diagnostics"
        if prop == "battery":
            group = "readings"  # z2m files it under diagnostic, but the family watches it
        label = exp.get("label") if isinstance(exp.get("label"), str) else prop.replace("_", " ")
        label = label[:1].lower() + label[1:]
        if label.replace(" ", "_") != prop:
            label = f"{label} ({prop})"
        field: dict = {"label": label, "group": group}
        if etype == "numeric":
            field["kind"] = KIND_BY_UNIT.get(unit, "number")
            if field["kind"] == "number" and unit:
                field["unit"] = unit
        elif etype == "binary":
            field["kind"] = "boolean"
            if prop in NOTABLE_BINARY:
                field["notable"] = True
            if aspect == "locked":
                field["values"] = [
                    {"value": True, "label": "locked"},
                    {"value": False, "label": "unlocked"},
                ]
        elif etype == "enum":
            field["kind"] = "enum"
            field["values"] = [
                {"value": v, "label": str(v)}
                for v in exp.get("values") or []
                if isinstance(v, (str, int, float)) and not isinstance(v, bool)
            ]
        else:
            field["kind"] = "text"
        access = exp.get("access")
        settable = isinstance(access, int) and bool(access & ACCESS_SET)
        if settable and aspect not in VOCABULARY_ASPECTS and etype in ("numeric", "enum"):
            command: dict = {"type": "enum" if etype == "enum" else "float", "editable_by": "owner"}
            if etype == "numeric":
                constraint = {
                    k: exp[src]
                    for k, src in (("min", "value_min"), ("max", "value_max"))
                    if isinstance(exp.get(src), (int, float)) and not isinstance(exp.get(src), bool)
                }
                if constraint:
                    command["constraint"] = constraint
                if isinstance(exp.get("value_step"), (int, float)):
                    command["step"] = exp["value_step"]
            field["command"] = command
        fields[aspect] = field

    for exp in exposes:
        add(exp)
    if not fields:
        return None
    if "battery" in fields:
        # z2m lists battery first, and the room card reads field order as
        # priority, so battery goes last.
        fields["battery"] = fields.pop("battery")
    return {"schema": 1, "groups": ["readings", "config", "diagnostics"], "fields": fields}


def inventory(devices, by_id):
    """Build the complete discovery document from one bridge/devices payload.

    One record per paired device, the coordinator excluded, with the
    binding `id`, whether an entity file binds it, a suggested stanza from
    `suggest`, and the raw definition for what the mapping does not cover
    (docs/design.md#discovery). A bound device's record also carries its
    aspect descriptor from `describe`.
    """
    records = []
    for dev in devices:
        # Malformed entries are skipped like id-less ones below, so an
        # unexpected inventory shape cannot stop the translator.
        if not isinstance(dev, dict):
            continue
        if dev.get("type") == "Coordinator":
            continue
        dev_id = dev.get("friendly_name") or dev.get("ieee_address")
        if not dev_id or not isinstance(dev_id, str):
            continue
        definition = dev.get("definition")
        if not isinstance(definition, dict):
            definition = {}
        entity = by_id.get(dev_id)
        exposes = definition.get("exposes") or []
        record = {
            "id": dev_id,
            "configured": entity is not None,
            "entity": entity.name if entity else None,
            "suggested": suggest(exposes),
            "description": {
                "vendor": definition.get("vendor"),
                "model": definition.get("model"),
                "description": definition.get("description"),
                "exposes": definition.get("exposes"),
            },
        }
        if entity is not None:
            descriptor = describe(entity.capability, exposes)
            if descriptor is not None:
                record["aspects"] = descriptor
        records.append(record)
    return records


def command_body(entity, aspect: str, value, fields: dict) -> dict | None:
    """Return the {base}/{id}/set payload for a command this entity takes, or None.

    A command this entity takes is its capability's base aspect (a bool), a
    declared vocabulary feature (a number) or a command its exposes-derived
    descriptor `fields` carry (a float, or one of the enum's values). `on`
    sends {"state": "ON"|"OFF"}, `locked` sends {"state": "LOCK"|"UNLOCK"},
    and anything else sends {aspect: value}. None for an unknown aspect or
    a value of the wrong type, which the caller drops with
    `invalid-command`.
    """
    if aspect == BASE_ASPECT.get(entity.capability):
        if not isinstance(value, bool):
            return None
        if aspect == "locked":
            # z2m reports LOCKED/UNLOCKED but takes LOCK/UNLOCK as commands.
            return {"state": "LOCK" if value else "UNLOCK"}
        return {"state": "ON" if value else "OFF"}
    number = isinstance(value, (int, float)) and not isinstance(value, bool)
    if aspect in VOCABULARY_ASPECTS and aspect in entity.features:
        return {aspect: value} if number else None
    command = fields.get(aspect, {}).get("command")
    if command is None:
        return None
    if command["type"] == "enum":
        allowed = [v["value"] for v in fields[aspect]["values"]]
        if isinstance(value, bool) or not isinstance(value, (str, int, float)) or value not in allowed:
            return None
    elif not number:
        return None
    return {aspect: value}


class Bridge:
    """The z2m-to-bus direction: one `on_message` per MQTT message.

    It holds what the messages build up: the device ids the bridge has
    reported (bound or not), the bridge's last known liveness and, per
    bound entity, the descriptor fields its exposes yielded (the commands
    beyond the base vocabulary, for command_body). `session` is anything
    with `put_json` and `health_event`. paho calls `on_message` from its
    one network thread, so nothing here is locked.
    """

    def __init__(self, session, unit: str, base: str, by_id: dict):
        self.session = session
        self.unit = unit
        self.base = base
        self.by_id = by_id
        self.inventory_seen = threading.Event()
        self.known: set[str] = set()
        # Unknown at boot, so a bridge already offline reports on its first
        # state message.
        self.online: bool | None = None
        self.descriptor_fields: dict[str, dict] = {}

    def unbound(self, topic: str, dev_id: str) -> None:
        """Report a message for an unbound device only when the bridge does not know it.

        A device the bridge knows but no entity file binds is normal:
        discovery already reports it with configured=false, and every
        device is in that state between pairing and binding. An event per
        publish would flood the recorder for as long as it stays unbound.
        Only a device missing from the inventory gets an event.
        """
        if dev_id not in self.known:
            self.session.health_event("drop", reason="unknown-device", topic=topic)

    def on_message(self, topic: str, raw: bytes) -> None:
        """Translate one message under the base topic onto the bus.

        bridge/devices updates the inventory and republishes discovery.
        bridge/state is the bridge's liveness. {id}/availability becomes
        the `available` aspect. {id} is device state: each scalar field is
        published as its own aspect. A field that would publish as
        `available` drops with `reserved-aspect`, and a field name that is
        not a valid key segment drops with `malformed-payload`; the other
        fields still publish.
        """
        session = self.session
        # Everything routed here matched a {base}/... subscription, so the
        # remainder is the device part. Splitting at a fixed position would
        # break when the base topic contains slashes.
        rest = topic[len(self.base) + 1 :]
        if rest == "bridge/devices":
            self.inventory_seen.set()
            try:
                devices = json.loads(raw)
            except ValueError:
                devices = None
            if not isinstance(devices, list):
                session.health_event("drop", reason="malformed-payload", topic=topic)
                return
            records = inventory(devices, self.by_id)
            self.known = {record["id"] for record in records}
            for record in records:
                if record["configured"]:
                    fields = record.get("aspects", {}).get("fields", {})
                    self.descriptor_fields[record["entity"]] = fields
            session.put_json(keys.discovery_key(self.unit), records)
            return
        if rest == "bridge/state":
            # The bridge's own liveness, used after startup. The inventory
            # cannot serve: z2m republishes bridge/devices only when it
            # changes, so its silence does not distinguish a dead bridge
            # from a stable one.
            state = availability_state(raw)
            if state is None:
                session.health_event("drop", reason="malformed-payload", topic=topic)
                return
            online = state == "online"
            if not online and self.online is not False:
                # One event per down transition.
                session.health_event("bridge-silent", base_topic=self.base, state="offline")
            self.online = online
            return
        # Only {base}/{id}/availability. A single segment would be a device
        # whose friendly name is "availability".
        if rest.endswith("/availability") and rest.count("/") == 1:
            dev_id = rest.split("/")[0]
            entity = self.by_id.get(dev_id)
            if entity is None:
                self.unbound(topic, dev_id)
                return
            state = availability_state(raw)
            if state is None:
                session.health_event("drop", reason="malformed-payload", topic=topic)
                return
            session.put_json(
                keys.state_key(entity.room, entity.name, "available"), state == "online"
            )
            return
        entity = self.by_id.get(rest)
        if entity is None:
            self.unbound(topic, rest)
            return
        try:
            payload = json.loads(raw)
        except ValueError:
            payload = None
        if not isinstance(payload, dict):
            session.health_event("drop", reason="malformed-payload", topic=topic)
            return
        for z2m_field, value in payload.items():
            if isinstance(value, (dict, list)):
                continue  # composite fields (color, ...) deferred
            aspect, value = state_aspect(entity.capability, z2m_field, value)
            if aspect == "available":
                # Reserved for the adapter's own liveness signal.
                session.health_event("drop", reason="reserved-aspect", topic=topic)
                continue
            try:
                key = keys.state_key(entity.room, entity.name, aspect)
            except ValueError:
                # A field name the key schema refuses. The rest of the
                # payload still publishes.
                session.health_event("drop", reason="malformed-payload", topic=topic, field=z2m_field)
                continue
            session.put_json(key, value)


def main():
    unit = os.environ[keys.ENV_UNIT]
    config = house.load_adapter(unit)
    by_id = {e.id: e for e in config.entities}

    endpoint = mqtt.parse_endpoint(config.endpoint)
    # The base topic is configurable because an installation that has used
    # a non-default one for years cannot move it: other consumers address
    # it directly.
    base = mqtt.base_topic(endpoint, DEFAULT_BASE_TOPIC)

    session = homeostat.connect()
    params = Params(session, PARAM_DEFAULTS)
    bridge = Bridge(session, unit, base, by_id)

    def cmd_handler(entity):
        def handler(sample):
            parsed = session.parse_command(sample)
            if parsed is None:
                return
            aspect, value, cmd_id = parsed
            body = command_body(entity, aspect, value, bridge.descriptor_fields.get(entity.name, {}))
            if body is None:
                session.health_event(
                    "drop", reason="invalid-command", key=str(sample.key_expr), cmd_id=cmd_id
                )
                return
            client.publish(f"{base}/{entity.id}/set", json.dumps(body))

        return handler

    client = mqtt.connect(
        endpoint,
        lambda _client, _userdata, msg: bridge.on_message(msg.topic, msg.payload),
        [
            (f"{base}/+", 0),
            (f"{base}/+/availability", 0),
            (f"{base}/bridge/devices", 0),
            (f"{base}/bridge/state", 0),
        ],
        health=session.health_event,
    )

    subscribers = [
        session.subscribe(expr, cmd_handler(e))
        for e in config.entities
        for expr in keys.command_keyexprs(e)
    ]

    session.ready()

    # A wrong base topic subscribes without error and then receives
    # nothing, so the adapter would be deaf while reporting healthy. The
    # retained inventory is the proof of life: if it has not arrived after
    # inventory_timeout_s, report one `bridge-silent` naming the base
    # topic in use. This covers startup only; bridge/state covers later.
    stop = threading.Event()

    def bridge_watchdog():
        timeout = params.inventory_timeout_s
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            if bridge.inventory_seen.wait(min(0.25, timeout)) or stop.is_set():
                return
        # A degraded condition, so its own event kind (docs/adapters.md#6-health-events).
        session.health_event("bridge-silent", base_topic=base, timeout_s=timeout)

    watchdog = threading.Thread(target=bridge_watchdog, daemon=True)
    watchdog.start()

    mqtt.wait_for_shutdown()
    stop.set()

    # Stop the MQTT loop first, or an in-flight on_message could put on
    # a closed zenoh session.
    client.loop_stop()
    client.disconnect()
    for sub in subscribers:
        sub.undeclare()
    session.close()


if __name__ == "__main__":
    main()
