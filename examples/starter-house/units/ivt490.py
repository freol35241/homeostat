# /// script
# requires-python = ">=3.11"
# dependencies = [
#     "homeostat",
#     "paho-mqtt>=2,<3",
# ]
#
# [tool.uv.sources]
# homeostat = { git = "https://github.com/freol35241/homeostat", subdirectory = "sdk/python", tag = "v0.3.0" }
# ///
"""IVT490 heat-pump adapter (docs/design.md, "IVT490 heat-pump adapter
(settled 2026-07-18)").

The bespoke ESP8266 interface board (github.com/freol35241/IVT490-interface-
esp8266) speaks its own MQTT dialect: {base}/state (one JSON blob) and
{base}/state/{field} (one topic per raw heat-pump field —
lib/IVT490/IVT490.cpp's serialize_IVT490State; {base}/state/raw is the
unparsed serial line and is ignored) mirror {base}/controller/state and
{base}/controller/state/{field} for the onboard feedback controller
(lib/IVT490/IVT490.h, Controller::serialize). The entity file's `id` is the
device's own MQTT base topic — unlike Zigbee2MQTT there is no shared root;
each interface board owns its whole topic tree — and the file stem is the
entity name. One entity per physical heat pump.

State fan-out subscribes the per-field topics only, both {base}/state/+
and {base}/controller/state/+, and publishes each field to
home/state/{room}/{entity}/{aspect} (state_aspect). Three fields are
normalized to the climate vocabulary: GT1 (Framledningstemperatur, the
feed line reading) to "feed_temperature"; the controller's
indoor_temperature_feedback (its live indoor reading, kept fresh by the
device's own feedback loop) to "indoor_temperature"; the controller's
indoor_temperature_target to "setpoint" — so the bus setpoint is always
the device's own readback, never an echo of a command. Every other field
passes through under its firmware name; a controller field that happened
to share a name with a raw heat-pump field would be prefixed
(controller_{field}) to keep the two namespaces distinguishable — no such
collision exists in the dialect today, but the guard stays live for
whichever firmware revision changes that.

ArduinoJson (v6, per platformio.ini) serializes a per-field topic's
payload with variant.as<String>(), which falls back to serializeJson() for
anything that is not already a string. Plain scalar fields (everything
under {base}/state/*, plus the controller's control_value and
vacation_mode) arrive as ordinary JSON numbers/booleans, but the
controller's freshness-tracked fields (feed_temperature_target,
indoor_temperature_feedback, outdoor_temperature_offset,
indoor_temperature_target, indoor_temperature_weight) serialize as nested
{"value": ..., "valid": ...} objects — field_value() unwraps both shapes
uniformly.

The heat pump is an arbitrated entity (docs/design.md, Arbitrated mode):
all commands ride the arbiter, and the family's manual setpoint always
wins — its templated home/cmd subscription therefore expands to nothing
at plan time, so every command this adapter ever sees has already been
arbitrated, on home/arbiter/{room}/{entity}/{aspect}. Commandable aspects
(COMMANDS): "setpoint" (indoor_temperature_target, the climate base
aspect), "feed_temperature_target", and "outdoor_temperature_offset"
translate to their {base}/controller/set/{field} topics as stringified
floats; "vacation" translates to {base}/controller/set/vacation_mode as
"1"/"0", the same integer-string convention the dialect's own boolean
fields use on the wire. NOTE: src/main.cpp's onMqttMessage subscribes to
that topic but has no branch that reads it — vacation_mode is not actually
settable over MQTT in this firmware revision. The adapter still publishes
it faithfully as specified; fixing that is a firmware change, outside this
adapter's dialect boundary. Bounds are adapter constants — device physics,
not house config (setpoint 10-30 degC, feed_temperature_target 20-60 degC,
outdoor_temperature_offset +/-10 K, vacation strictly boolean): a
wrong-type or out-of-range command DROPS with an "invalid-command" health
event carrying the offending aspect and value, never clamped. A malformed
or envelope-less command (keys.parse_cmd_envelope) drops the same way,
like every other adapter.

Discovery is a small, static document at home/discovery/{unit}: one
record per bound entity carrying its base-topic id, a suggested
capability "climate" stanza, and a `bound` flag that starts false and
flips permanently true (with a republish) the first time that base topic
is actually seen on the broker. The OwnTracks/Zigbee2MQTT incremental
inventory pattern — discovering devices never bound by any entity file —
is overkill for a dialect with exactly one address per entity file, known
up front.

Operational note: any Node-RED flow WRITING to this device's
controller/set topics must be disabled before this adapter goes live —
one master per device; read-only flows can coexist.
"""

import json
import os

import homeostat
from homeostat import house, keys, mqtt

# lib/IVT490/IVT490.cpp, serialize_IVT490State: the raw heat-pump field
# names, in their serialized order. Passthrough vocabulary for
# {base}/state/+, and the collision guard in state_aspect().
STATE_FIELDS = frozenset(
    {
        "GT1",
        "GT1_target",
        "GT1_LLT",
        "GT1_LL",
        "GT1_UL",
        "GT2_heatpump",
        "GT2_sensor",
        "GT3_1",
        "GT3_2",
        "GT3_2_LL",
        "GT3_2_UL",
        "GT3_2_ULT",
        "GT3_3",
        "GT3_3_target",
        "GT3_3_LL",
        "GT3_4",
        "GT5",
        "GT6",
        "electricity_supplement",
        "GP1",
        "GP2",
        "GP3",
        "compressor",
        "vacation",
        "P1",
        "alarm",
        "fan",
        "SV1_open",
        "SV1_close",
    }
)

ASPECT_OVERRIDES = {
    ("state", "GT1"): "feed_temperature",
    ("controller", "indoor_temperature_feedback"): "indoor_temperature",
    ("controller", "indoor_temperature_target"): "setpoint",
}

# Commandable aspect -> ({base}/controller/set/{field}, (min, max) or None
# for the strictly-boolean vacation aspect (docs/design.md settlement).
COMMANDS = {
    "setpoint": ("indoor_temperature_target", (10.0, 30.0)),
    "feed_temperature_target": ("feed_temperature_target", (20.0, 60.0)),
    "outdoor_temperature_offset": ("outdoor_temperature_offset", (-10.0, 10.0)),
    "vacation": ("vacation_mode", None),
}


def state_aspect(source: str, field: str) -> str:
    """Maps one firmware field (`source` "state" or "controller") to a bus
    aspect name — the three settled normalizations, or the firmware name
    passed through, prefixed `controller_` on a name collision between the
    two namespaces (see module docstring)."""
    override = ASPECT_OVERRIDES.get((source, field))
    if override is not None:
        return override
    if source == "controller" and field in STATE_FIELDS:
        return f"controller_{field}"
    return field


def field_value(payload: bytes):
    """Unwraps a per-field topic's payload: a plain JSON scalar passes
    through unchanged, a nested {"value": ..., "valid": ...} object (the
    controller's freshness-tracked fields) yields its "value" member.
    Raises ValueError/KeyError on anything else — callers drop these with
    a "malformed-payload" health event."""
    parsed = json.loads(payload)
    if isinstance(parsed, dict):
        return parsed["value"]
    return parsed


def route(topic: str, entities):
    """The bound entity and the topic's segments past its base-topic
    prefix, or (None, None). Defensive only: the adapter subscribes
    exactly `{entity.id}/...` per entity, so paho never calls back with a
    topic that fails to resolve here."""
    for entity in entities:
        prefix = f"{entity.id}/"
        if topic.startswith(prefix):
            return entity, topic[len(prefix) :].split("/")
    return None, None


def main():
    unit = os.environ[keys.ENV_UNIT]
    config = house.load_adapter(unit)

    endpoint = mqtt.parse_endpoint(config.endpoint)

    session = homeostat.connect()
    seen: set[str] = set()

    def inventory():
        return [
            {
                "id": e.id,
                "configured": True,
                "entity": e.name,
                "bound": e.id in seen,
                "suggested": {"capability": "climate", "features": []},
            }
            for e in config.entities
        ]

    def on_ivt_message(client, userdata, msg):
        entity, rest = route(msg.topic, config.entities)
        if entity is None:
            return

        if entity.id not in seen:
            seen.add(entity.id)
            session.put_json(keys.discovery_key(unit), inventory())

        if len(rest) == 2 and rest[0] == "state":
            source, field = "state", rest[1]
        elif len(rest) == 3 and rest[0] == "controller" and rest[1] == "state":
            source, field = "controller", rest[2]
        else:
            return  # a blob topic ({base}/state, {base}/controller/state)

        if source == "state" and field == "raw":
            return  # the unparsed serial line, not a heat-pump field

        try:
            value = field_value(msg.payload)
        except (ValueError, KeyError):
            session.health_event("drop", reason="malformed-payload", topic=msg.topic)
            return
        aspect = state_aspect(source, field)
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

            command = COMMANDS.get(aspect)
            if command is None:
                session.health_event(
                    "drop", reason="invalid-command", key=key, aspect=aspect, value=value
                )
                return
            field, bounds = command
            if bounds is None:
                if not isinstance(value, bool):
                    session.health_event(
                        "drop", reason="invalid-command", key=key, aspect=aspect, value=value
                    )
                    return
                body = "1" if value else "0"
            else:
                lo, hi = bounds
                if isinstance(value, bool) or not isinstance(value, (int, float)) or not (
                    lo <= value <= hi
                ):
                    session.health_event(
                        "drop", reason="invalid-command", key=key, aspect=aspect, value=value
                    )
                    return
                body = str(float(value))
            client.publish(f"{entity.id}/controller/set/{field}", body)

        return handler

    topics = [(f"{e.id}/state/+", 0) for e in config.entities] + [
        (f"{e.id}/controller/state/+", 0) for e in config.entities
    ]
    client = mqtt.connect(endpoint, on_ivt_message, topics)

    # An arbitrated entity has no home/cmd subscription at all — not
    # subscribing IS the structural enforcement — and instead gets the
    # arbiter's forwarded, post-arbitration envelope on home/arbiter/**.
    subscribers = [
        session.subscribe(keys.cmd_keyexpr(e.room, e.name), cmd_handler(e))
        for e in config.entities
        if e.write_mode != "arbitrated"
    ] + [
        session.subscribe(keys.arbiter_keyexpr(e.room, e.name), cmd_handler(e))
        for e in config.entities
        if e.write_mode == "arbitrated"
    ]

    session.put_json(keys.discovery_key(unit), inventory())

    # Both translation directions are wired up: the unit is ready.
    session.ready()

    mqtt.wait_for_shutdown()

    for sub in subscribers:
        sub.undeclare()
    session.close()
    client.loop_stop()
    client.disconnect()


if __name__ == "__main__":
    main()
