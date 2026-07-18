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
"""IVT490 heat-pump adapter (docs/design.md, "IVT490 heat-pump adapter
(settled 2026-07-18)").

The bespoke ESP8266 interface board (github.com/freol35241/IVT490-interface-
esp8266, tracked firmware ref: the GT3_2_boiler_emulation branch) speaks
its own MQTT dialect. Heat-pump state lives under {base}/ivt490/state: the
whole document as one JSON blob, then — src/main.cpp's publish_json_object
recursing into every nested JsonObject — each sub-object as its own blob
and each scalar leaf on its own subtopic. The document (lib/IVT490/
IVT490.cpp, State::serialize) wraps the 28 serial-protocol fields in a
"serial" object and carries two thermistor sensor objects, "GT2" and
"GT3_2", each {raw, filtered} — so scalars arrive at
{base}/ivt490/state/serial/{field} and
{base}/ivt490/state/{sensor}/{raw|filtered}. {base}/ivt490/raw is the
unparsed serial line and is never subscribed. Controller state
(src/Controller.h, Controller::serialize) lives at {base}/controller/state
and {base}/controller/state/{field}: feed_temperature_target,
indoor_temperature_feedback and outdoor_temperature_offset are
{value, valid} objects, indoor_temperature_target is a {value} object,
operating_mode a bare integer — field_value() unwraps the nested shapes
and passes bare scalars through uniformly. The entity file's `id` is the
device's own MQTT base topic — unlike Zigbee2MQTT there is no shared root;
each interface board owns its whole topic tree — and the file stem is the
entity name. One entity per physical heat pump.

State fan-out subscribes the scalar levels ({base}/ivt490/state/+,
{base}/ivt490/state/+/+ and {base}/controller/state/+; an object payload
on any of them is a nested blob whose leaves arrive on deeper subtopics,
and is skipped) and publishes each field to
home/state/{room}/{entity}/{aspect}. Aspect naming (state_field): the
"serial" wrapper is a serialization artifact and is stripped — serial
fields keep their bare firmware names — while any other nested path joins
its segments with underscores (GT2/raw -> "GT2_raw", GT2/filtered ->
"GT2_filtered", likewise GT3_2), because a multi-segment aspect would fall
outside the home/state/{room}/{entity}/{aspect} key slot; none of the
joined names collide with a serial field name. Three fields normalize to
the climate vocabulary: serial GT1 (Framledningstemperatur, the feed line
reading) to "feed_temperature"; the controller's
indoor_temperature_feedback (its live indoor reading) to
"indoor_temperature"; the controller's indoor_temperature_target to
"setpoint" — so the bus setpoint is always the device's own readback,
never an echo of a command. Everything else passes through under its
firmware name — including "vacation", a read-only state boolean in this
firmware; a controller field that happened to share a name with a state
aspect would be prefixed (controller_{field}) to keep the namespaces
distinguishable — no such collision exists in the dialect today, but the
guard stays live for whichever firmware revision changes that.

The heat pump is an arbitrated entity (docs/design.md, Arbitrated mode):
all commands ride the arbiter, and the family's manual setpoint always
wins — its templated home/cmd subscription therefore expands to nothing
at plan time, so every command this adapter ever sees has already been
arbitrated, on home/arbiter/{room}/{entity}/{aspect}. Commandable aspects
(COMMANDS): "setpoint" (indoor_temperature_target, the climate base
aspect), "feed_temperature_target" and "outdoor_temperature_offset"
translate to their {base}/controller/set/{field} topics as stringified
floats; "operating_mode" — the GT3_2 boiler-sensor emulation — is
strictly the integer 1, 2 or 3 (1=BAU normal, 2=BLOCK suppress heating,
3=BOOST force heating; src/Controller.h, OperatingMode) and translates to
{base}/controller/set/operating_mode as an integer string. The firmware's
fifth set topic, controller/set/indoor_temperature_actual, is deliberately
NOT a command aspect: it is the sensor-feedback input reserved for a
future automation that streams a real indoor temperature reading into the
device's control loop. Bounds are adapter constants — device physics, not
house config (setpoint 10-30 degC, feed_temperature_target 20-60 degC,
outdoor_temperature_offset +/-10 K): a wrong-type or out-of-range command
DROPS with an "invalid-command" health event carrying the offending
aspect and value, never clamped. A malformed or envelope-less command
(keys.parse_cmd_envelope) drops the same way, like every other adapter.
NOTE: the firmware's toFloat()/toInt() parse treats 0 as failure, so a
legitimate outdoor_temperature_offset of exactly 0.0 — in range here — is
silently discarded by the device; the adapter forwards it faithfully
rather than working around firmware behavior.

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

# lib/IVT490/IVT490.cpp, State::serialize (GT3_2_boiler_emulation branch):
# every aspect the state fan-out can produce — the 28 serial-protocol
# fields (published under the stripped "serial" wrapper) plus the
# underscore-joined thermistor sensor leaves. The collision guard in
# state_aspect() checks controller fields against this set.
STATE_ASPECTS = frozenset(
    {
        "GT1",
        "GT1_target",
        "GT1_LLT",
        "GT1_LL",
        "GT1_UL",
        "GT2",
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
        # The GT2 and GT3_2 thermistor sensor objects, flattened.
        "GT2_raw",
        "GT2_filtered",
        "GT3_2_raw",
        "GT3_2_filtered",
    }
)

ASPECT_OVERRIDES = {
    ("state", "GT1"): "feed_temperature",
    ("controller", "indoor_temperature_feedback"): "indoor_temperature",
    ("controller", "indoor_temperature_target"): "setpoint",
}

# The three operating modes of the GT3_2 boiler-sensor emulation
# (src/Controller.h, OperatingMode): 1=BAU, 2=BLOCK, 3=BOOST.
OPERATING_MODES = (1, 2, 3)

# Commandable aspect -> ({base}/controller/set/{field}, (min, max) for the
# float aspects, or None for the strictly-enumerated operating_mode.
COMMANDS = {
    "setpoint": ("indoor_temperature_target", (10.0, 30.0)),
    "feed_temperature_target": ("feed_temperature_target", (20.0, 60.0)),
    "outdoor_temperature_offset": ("outdoor_temperature_offset", (-10.0, 10.0)),
    "operating_mode": ("operating_mode", None),
}


def state_field(segments: list[str]) -> str:
    """Flattened subtopic path under {base}/ivt490/state to a firmware
    field name: the "serial" wrapper object is stripped (a serialization
    artifact, not device vocabulary), any other nested path joins with
    underscores so the aspect stays a single key segment."""
    if segments and segments[0] == "serial":
        segments = segments[1:]
    return "_".join(segments)


def state_aspect(source: str, field: str) -> str:
    """Maps one firmware field (`source` "state" or "controller") to a bus
    aspect name — the three settled normalizations, or the firmware name
    passed through, prefixed `controller_` on a name collision between the
    two namespaces (see module docstring)."""
    override = ASPECT_OVERRIDES.get((source, field))
    if override is not None:
        return override
    if source == "controller" and field in STATE_ASPECTS:
        return f"controller_{field}"
    return field


def field_value(payload: bytes):
    """Unwraps a controller per-field payload: a plain JSON scalar passes
    through unchanged, a nested {"value": ...[, "valid": ...]} object (the
    controller's tracked fields) yields its "value" member. Raises
    ValueError/KeyError on anything else — callers drop these with a
    "malformed-payload" health event."""
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

        if len(rest) in (3, 4) and rest[0] == "ivt490" and rest[1] == "state":
            try:
                value = json.loads(msg.payload)
            except ValueError:
                session.health_event("drop", reason="malformed-payload", topic=msg.topic)
                return
            if isinstance(value, (dict, list)):
                return  # a nested blob; its leaves arrive on deeper subtopics
            aspect = state_aspect("state", state_field(rest[2:]))
        elif len(rest) == 3 and rest[0] == "controller" and rest[1] == "state":
            try:
                value = field_value(msg.payload)
            except (ValueError, KeyError):
                session.health_event("drop", reason="malformed-payload", topic=msg.topic)
                return
            aspect = state_aspect("controller", rest[2])
        else:
            return  # a whole-document blob topic

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
                if (
                    isinstance(value, bool)
                    or not isinstance(value, int)
                    or value not in OPERATING_MODES
                ):
                    session.health_event(
                        "drop", reason="invalid-command", key=key, aspect=aspect, value=value
                    )
                    return
                body = str(value)
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

    topics = [
        (topic, 0)
        for e in config.entities
        for topic in (
            f"{e.id}/ivt490/state/+",
            f"{e.id}/ivt490/state/+/+",
            f"{e.id}/controller/state/+",
        )
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
