# /// script
# requires-python = ">=3.11"
# dependencies = [
#     "homeostat",
#     "paho-mqtt>=2,<3",
# ]
#
# [tool.uv.sources]
# homeostat = { path = "../sdk/python", editable = true }
# ///
"""Zigbee2MQTT adapter: a translating subscriber.

The broker prefix is the discovery endpoint's path
(`mqtt://broker:1883/VP52/zigbee2mqtt`), defaulting to `zigbee2mqtt`: an
estate that has run a non-default base topic for years cannot move it,
because other consumers address it directly. A wrong prefix is the
failure worth naming — the subscription SUCCEEDS and simply matches
nothing, so there is no SUBACK timeout and no error, just an adapter that
is permanently deaf while reporting healthy. The retained
`{base}/bridge/devices` inventory is therefore treated as proof of life:
silence past inventory_timeout_s (parameter, owner-editable, adapter-side
default 30 s) emits one "bridge-silent" health event naming the base topic
actually in use.

That guard covers boot only. Mid-run the bridge's own retained
{base}/bridge/state carries online/offline, and one "bridge-silent" event
goes out per down transition — the inventory cannot serve here, because
z2m republishes bridge/devices only on CHANGE, so its silence never
distinguishes a dead bridge from a stable estate.

Broker credentials come from HOMEOSTAT_MQTT_CREDENTIALS (a TOML outside
the repo, keyed by hostname) unless the endpoint carries them inline; a
broker that needs auth must not force its password into a unit manifest.

Device state published as JSON on {base}/{id} fans out to per-aspect
keys home/state/{room}/{entity}/{aspect}; commands on
home/cmd/{room}/{entity}/{aspect} translate to {base}/{id}/set. The
entity file's `id` is the z2m topic segment; the file stem is the entity
name. The z2m `state` field is normalized (`on` for lights/switches,
`locked` for locks); other scalar fields pass through under their z2m
names; nested objects (e.g. color) are deferred. Arbitrated entities (e.g.
locks) get no home/cmd subscription at all — plan-time expansion gives the
adapter's templated cmd subscription only its non-arbitrated bound
entities — and instead receive the arbiter's forwarded envelope on
home/arbiter/{room}/{entity}/{aspect}, translated the same way a cmd
envelope would be. Anything dropped emits a JSON event at
home/health/{unit}/event instead of crashing.

Device availability (docs/design.md, "Sensor dropout and availability"):
the bridge's availability feature on {base}/{id}/availability — both
the {"state": "online"|"offline"} payload and the legacy bare string —
maps to the reserved base aspect home/state/{room}/{entity}/available
(bool). Operational note: availability must be enabled in the z2m config;
without it the aspect simply never appears (opt-in by construction). On
loss the device's other aspects stand — stale, never false — and a native
device field that would mint the reserved aspect drops with a
"reserved-aspect" health event.

The retained {base}/bridge/devices inventory is republished at
home/discovery/{unit}: every paired device (coordinator excluded) as a
record carrying the entity-file binding `id`, whether an entity file
already binds it, a best-effort suggested capability/features stanza
mapped from the z2m `exposes` descriptor, and the raw definition for
anything the mapping does not cover (docs/design.md, Discovery). A bound
device's record also carries the entity's aspect descriptor (docs/
design.md, Aspect descriptors), generated from the same `exposes`: every
scalar expose becomes a labelled field — z2m's unit picks the kind
(°C → temperature, % → percent, anything else a number carrying the unit),
its category picks the group (diagnostic → diagnostics, config → config,
else readings), a settable config expose becomes an owner-tier command
with z2m's own value bounds, and the alarm-shaped binaries (water leak,
smoke, ...) are notable. The capability's own vocabulary (on, locked,
brightness, color_temp) is described as readings only: its controls are
the dashboard's bespoke widget, not a descriptor command.
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
# past this means the base topic is wrong (see the module docstring).
PARAM_DEFAULTS = {"inventory_timeout_s": 30.0}


class Params(LiveParams):
    """inventory_timeout_s from home/config/{unit}/*, live."""

    @property
    def inventory_timeout_s(self) -> float:
        return max(0.1, self.get("inventory_timeout_s"))


def availability_state(payload: bytes):
    """`online`/`offline` from an availability-shaped payload — the
    {"state": ...} object or the legacy bare string — or None if it is
    neither. Shared by device availability and the bridge's own state."""
    raw = payload.decode(errors="replace").strip()
    try:
        parsed = json.loads(raw)
    except ValueError:
        parsed = raw  # legacy availability payload: a bare string
    state = parsed.get("state") if isinstance(parsed, dict) else parsed
    return state if state in ("online", "offline") else None


def state_aspect(capability: str, z2m_field: str, value):
    """Maps one z2m JSON field to a (bus aspect, JSON value) pair."""
    if z2m_field == "state":
        if capability == "lock":
            return "locked", value == "LOCKED"
        return "on", value == "ON"
    return z2m_field, value


def suggest(exposes):
    """Best-effort entity-file stanza from a z2m exposes descriptor, or
    None when no confident mapping exists — the raw definition rides
    along in the record either way, so nothing becomes invisible."""
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


# Binary exposes whose `true` is out of the ordinary — z2m property names,
# so dialect knowledge (docs/design.md, Aspect descriptors: notable).
NOTABLE_BINARY = frozenset(
    {"battery_low", "water_leak", "smoke", "gas", "carbon_monoxide", "tamper", "vibration"}
)
KIND_BY_UNIT = {"°C": "temperature", "%": "percent"}
# The capability vocabulary the dashboard's own widgets command: described
# as readings, never as descriptor commands.
VOCABULARY_ASPECTS = frozenset({"on", "locked", "brightness", "color_temp"})
ACCESS_SET = 2  # z2m access bitmask: 1 published, 2 settable, 4 gettable
SPECIFIC_TYPES = ("light", "switch", "lock", "cover", "climate", "fan")


def describe(capability: str, exposes) -> dict | None:
    """The entity's aspect descriptor from its z2m exposes, or None when
    nothing scalar is exposed (docs/design.md, Aspect descriptors)."""
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
            return  # composite/list are deferred, like their state
        aspect, _ = state_aspect(capability, prop, None)
        if aspect == "available" or aspect in fields:
            return
        category = exp.get("category")
        group = {"diagnostic": "diagnostics", "config": "config"}.get(category, "readings")
        if prop == "battery":
            group = "readings"  # z2m files it under diagnostic; a family watches it
        label = exp.get("label") if isinstance(exp.get("label"), str) else prop.replace("_", " ")
        label = label[:1].lower() + label[1:]
        if label.replace(" ", "_") != prop:
            label = f"{label} ({prop})"
        field: dict = {"label": label, "group": group}
        unit = exp.get("unit") if isinstance(exp.get("unit"), str) else None
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
    return {"schema": 1, "groups": ["readings", "config", "diagnostics"], "fields": fields}


def inventory(devices, by_id):
    """The complete discovery document from one bridge/devices payload."""
    records = []
    for dev in devices:
        # Structurally malformed entries skip like id-less ones below: a
        # surprise inventory shape must never take the translator down.
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


def main():
    unit = os.environ[keys.ENV_UNIT]
    config = house.load_adapter(unit)
    by_id = {e.id: e for e in config.entities}

    endpoint = mqtt.parse_endpoint(config.endpoint)
    base = mqtt.base_topic(endpoint, DEFAULT_BASE_TOPIC)

    session = homeostat.connect()
    params = Params(session, PARAM_DEFAULTS)
    inventory_seen = threading.Event()
    # Device ids the bridge has told us about, bound or not, and the
    # bridge's last known liveness. Mutated and read on the paho callback
    # thread only. `online` starts unknown, so a bridge already offline at
    # boot reports on its first state message.
    known: set[str] = set()
    bridge = {"online": None}

    def unbound(topic: str, dev_id: str) -> None:
        """A device the BRIDGE knows but no entity file binds is a steady
        state, not a dropped message — discovery already reports it with
        configured=false, and the discovery-first workflow guarantees a
        period where every device is in exactly this state. Only a device
        absent from the inventory entirely is an anomaly worth an event."""
        if dev_id not in known:
            session.health_event("drop", reason="unknown-device", topic=topic)

    def on_z2m_message(client, userdata, msg):
        # Everything routed here matched a {base}/... subscription, so the
        # remainder is the device part. Splitting at a fixed position would
        # break the moment the base topic carries its own slashes.
        rest = msg.topic[len(base) + 1 :]
        if rest == "bridge/devices":
            inventory_seen.set()
            try:
                devices = json.loads(msg.payload)
            except ValueError:
                devices = None
            if not isinstance(devices, list):
                session.health_event("drop", reason="malformed-payload", topic=msg.topic)
                return
            records = inventory(devices, by_id)
            known.clear()
            known.update(record["id"] for record in records)
            session.put_json(keys.discovery_key(unit), records)
            return
        if rest == "bridge/state":
            # The bridge's own liveness. The inventory cannot carry this:
            # z2m republishes bridge/devices only on CHANGE, so its silence
            # never distinguishes a dead bridge from a stable estate.
            state = availability_state(msg.payload)
            if state is None:
                session.health_event("drop", reason="malformed-payload", topic=msg.topic)
                return
            online = state == "online"
            if not online and bridge["online"] is not False:
                # One event per down transition, the ivt490 precedent.
                session.health_event("bridge-silent", base_topic=base, state="offline")
            bridge["online"] = online
            return
        # Exactly {base}/{id}/availability — two segments would be a device
        # whose friendly name is literally "availability".
        if rest.endswith("/availability") and rest.count("/") == 1:
            dev_id = rest.split("/")[0]
            entity = by_id.get(dev_id)
            if entity is None:
                unbound(msg.topic, dev_id)
                return
            state = availability_state(msg.payload)
            if state is None:
                session.health_event("drop", reason="malformed-payload", topic=msg.topic)
                return
            session.put_json(
                keys.state_key(entity.room, entity.name, "available"), state == "online"
            )
            return
        entity = by_id.get(rest)
        if entity is None:
            unbound(msg.topic, rest)
            return
        try:
            payload = json.loads(msg.payload)
        except ValueError:
            payload = None
        if not isinstance(payload, dict):
            session.health_event("drop", reason="malformed-payload", topic=msg.topic)
            return
        for z2m_field, value in payload.items():
            if isinstance(value, (dict, list)):
                continue  # composite fields (color, ...) deferred
            aspect, value = state_aspect(entity.capability, z2m_field, value)
            if aspect == "available":
                # Reserved for the adapter's own liveness signal — a device
                # field must not impersonate it.
                session.health_event("drop", reason="reserved-aspect", topic=msg.topic)
                continue
            session.put_json(keys.state_key(entity.room, entity.name, aspect), value)

    def cmd_handler(entity):
        def handler(sample):
            key = str(sample.key_expr)
            aspect = key.split("/", 4)[4]
            try:
                payload = json.loads(sample.payload.to_bytes())
            except ValueError:
                session.health_event("drop", reason="malformed-payload", key=key)
                return
            try:
                value = keys.parse_cmd_envelope(payload)
            except ValueError:
                session.health_event("drop", reason="invalid-command", key=key)
                return
            if aspect == "on":
                if not isinstance(value, bool):
                    session.health_event("drop", reason="invalid-command", key=key)
                    return
                body = {"state": "ON" if value else "OFF"}
            elif aspect == "locked":
                if not isinstance(value, bool):
                    session.health_event("drop", reason="invalid-command", key=key)
                    return
                # z2m's lock vocabulary is asymmetric: state REPORTS are
                # LOCKED/UNLOCKED, but SET commands are LOCK/UNLOCK.
                body = {"state": "LOCK" if value else "UNLOCK"}
            elif "/" in aspect:
                session.health_event("drop", reason="invalid-command", key=key)
                return
            else:
                body = {aspect: value}
            client.publish(f"{base}/{entity.id}/set", json.dumps(body))

        return handler

    client = mqtt.connect(
        endpoint,
        on_z2m_message,
        [
            (f"{base}/+", 0),
            (f"{base}/+/availability", 0),
            (f"{base}/bridge/devices", 0),
            (f"{base}/bridge/state", 0),
        ],
    )

    subscribers = [
        session.subscribe(expr, cmd_handler(e))
        for e in config.entities
        for expr in keys.command_keyexprs(e)
    ]

    # Both translation directions are wired up: the unit is ready.
    session.ready()

    # A wrong base topic subscribes SUCCESSFULLY and then receives nothing:
    # no SUBACK timeout, no error, an adapter that is permanently deaf and
    # reports healthy. The retained inventory is the proof of life.
    stop = threading.Event()

    def bridge_watchdog():
        timeout = params.inventory_timeout_s
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            if inventory_seen.wait(min(0.25, timeout)) or stop.is_set():
                return
        # A degraded condition, not dropped input: the ivt490
        # device-silent precedent.
        session.health_event("bridge-silent", base_topic=base, timeout_s=timeout)

    watchdog = threading.Thread(target=bridge_watchdog, daemon=True)
    watchdog.start()

    mqtt.wait_for_shutdown()
    stop.set()

    # The MQTT loop stops first: an in-flight on_message during teardown
    # would otherwise put on a closed zenoh session.
    client.loop_stop()
    client.disconnect()
    for sub in subscribers:
        sub.undeclare()
    session.close()


if __name__ == "__main__":
    main()
