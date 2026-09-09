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
operating_mode a bare integer — field_value() unwraps the nested shapes,
keeping the flag, and passes bare scalars through uniformly. The entity
file's `id` is the device's own MQTT base topic — unlike Zigbee2MQTT
there is no shared root; each interface board owns its whole topic tree —
and the file stem is the entity name. One entity per physical heat pump.

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

The controller's validity flags are aspects of their own. Each field the
firmware timestamps carries a `valid` predicate (src/Controller.h) that
goes false once the reading ages past the device's validity window, at
which point the control loop silently drops that term and falls back to
curve-only control on the filtered outdoor sensor. Publishing the value
alone would leave a confident-looking number on the bus that the device
itself has stopped believing, so a {value, valid} field also publishes
home/state/{room}/{entity}/{aspect}_valid as a boolean — the same
distinction adapters/onvif.py keeps for motion, here reported by the
device rather than inferred. "Stale" and "absent" stay distinguishable
live and in the recorder afterwards, which is what makes "when did it go
stale" answerable months later. A {value}-only field grows no such
aspect. (These are read-only observations; `available` remains the
receive-timer signal below, unaffected.)

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
fifth set topic, controller/set/indoor_temperature_actual, is NOT a command
aspect: it is a sensor-feedback input, a continuous signal with one master
rather than contestable intent (docs/design.md, "Device feeds"). It and
outdoor_temperature_offset are the FEEDABLE inputs: an entity file wires
one with `[inputs] <input> = { entity, aspect }`, and the adapter then
subscribes that source's state key and forwards each sample to
{base}/controller/set/{input} as a stringified float, NOT retained — the
firmware's own validity window is what ends a stale feed, and a retained
value would outlive its source across a device reconnect. While the
source's `available` is false nothing is forwarded, and the topic's
retained slot is cleared once (an empty retained publish) so a value left
by a previous master cannot stand in for a source that has gone. A wired
input is no longer a command aspect for that entity (one master), so a
command naming it drops with invalid-command like any unknown aspect. An
input name the adapter does not know is a configuration error and the
unit refuses to start, which the supervisor makes visible. Bounds are
adapter constants — device physics, not
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
capability "climate" stanza, the entity's aspect descriptor (ASPECT_FIELDS
— labels, kinds, groups and the command vocabulary the dashboard renders;
docs/design.md, Aspect descriptors), and a `bound` flag that starts false
and flips permanently true (with a republish) the first time that base
topic is actually seen on the broker. The OwnTracks/Zigbee2MQTT incremental
inventory pattern — discovering devices never bound by any entity file —
is overkill for a dialect with exactly one address per entity file, known
up front.

Device availability (docs/design.md, "Sensor dropout and availability"):
this firmware publishes continuously — every serial telegram fans out —
so silence IS the loss signal. A receive timer flips
home/state/{room}/{entity}/available to false after
availability_timeout_s (parameter, owner-editable, adapter-side default
300 s) without a single routed message from that device's base topic,
with one "device-silent" health event per down transition; any routed
message flips it back to true. The timer is seeded at startup, so a
device that never speaks goes false after one timeout. On loss every
other aspect stands — stale, never false.

Operational note: any Node-RED flow WRITING to this device's
controller/set topics must be disabled before this adapter goes live —
one master per device; read-only flows can coexist.
"""

import json
import os
import threading
import time

import homeostat
from homeostat import house, keys, mqtt
from homeostat.params import LiveParams

PARAM_DEFAULTS = {"availability_timeout_s": 300.0}


class Params(LiveParams):
    """availability_timeout_s from home/config/{unit}/*, live."""

    @property
    def availability_timeout_s(self) -> float:
        return max(0.1, self.get("availability_timeout_s"))


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

# Device inputs an entity file may wire to a source (docs/design.md, Device
# feeds). Values forwarded as floats within the same physical bounds as the
# matching command where one exists.
FEEDABLE = {
    "indoor_temperature_actual": (-50.0, 60.0),
    "outdoor_temperature_offset": (-10.0, 10.0),
}

# Commandable aspect -> ({base}/controller/set/{field}, (min, max) for the
# float aspects, or None for the strictly-enumerated operating_mode.
COMMANDS = {
    "setpoint": ("indoor_temperature_target", (10.0, 30.0)),
    "feed_temperature_target": ("feed_temperature_target", (20.0, 60.0)),
    "outdoor_temperature_offset": ("outdoor_temperature_offset", (-10.0, 10.0)),
    "operating_mode": ("operating_mode", None),
}

# The aspect descriptor (docs/design.md, Aspect descriptors) this adapter
# publishes in each bound entity's discovery record: labels, kinds and
# groups for the aspects worth a family-facing name, and the command
# vocabulary the dashboard may render — bounds straight from COMMANDS so
# there is one source. The family tier gets the one lever that is family
# intent, the indoor target; the GT3_2 emulation's mode is driven by an
# automation at the reporting house, so it is owner tuning like the feed
# target and curve offset — visible with a badge, written only through
# the bus. Labels keep the firmware's sensor code in parentheses so the
# page and the manual agree on what a reading is. Every aspect not named
# here still publishes and renders, in the page's diagnostics group under
# its firmware name.
ASPECT_GROUPS = ["control", "readings", "status", "limits"]
# Labels follow lib/IVT490/IVT490.h's field comments (Swedish, the pump's
# own manual vocabulary) with the firmware code kept in parentheses.
T = "temperature"
ASPECT_FIELDS = {
    "setpoint": {"label": "indoor target", "kind": T, "group": "control"},
    "operating_mode": {
        "label": "mode",
        "kind": "enum",
        "group": "control",
        "values": [
            {"value": 1, "label": "normal"},
            {"value": 2, "label": "block"},
            {"value": 3, "label": "boost"},
        ],
    },
    "feed_temperature_target": {"label": "feed target", "kind": T, "group": "control"},
    "outdoor_temperature_offset": {
        "label": "curve offset",
        "kind": "temperature_delta",
        "group": "control",
    },
    # readings: temperatures (Framledning, ute, tappvarmvatten, ...)
    "indoor_temperature": {
        "label": "indoor",
        "kind": T,
        "group": "readings",
        "valid": "indoor_temperature_valid",
    },
    "feed_temperature": {"label": "feed line (GT1)", "kind": T, "group": "readings"},
    "GT1_target": {"label": "feed line target (GT1_target)", "kind": T, "group": "readings"},
    "GT2": {"label": "outdoor (GT2)", "kind": T, "group": "readings"},
    "GT3_1": {"label": "tap hot water (GT3_1)", "kind": T, "group": "readings"},
    "GT3_2": {"label": "hot water tank (GT3_2)", "kind": T, "group": "readings"},
    "GT3_3": {"label": "heating water (GT3_3)", "kind": T, "group": "readings"},
    "GT3_3_target": {"label": "heating water target (GT3_3_target)", "kind": T, "group": "readings"},
    "GT3_4": {"label": "extra accumulator tank (GT3_4)", "kind": T, "group": "readings"},
    "GT5": {"label": "indoor sensor (GT5)", "kind": T, "group": "readings"},
    "GT6": {"label": "hot gas (GT6)", "kind": T, "group": "readings"},
    # status: what is running, switching, or tripped
    "compressor": {"label": "compressor", "kind": "boolean", "group": "status"},
    "electricity_supplement": {
        "label": "electric backup use (electricity_supplement)",
        "kind": "percent",
        "group": "status",
    },
    "fan": {"label": "fan", "kind": "boolean", "group": "status"},
    "P1": {"label": "circulation pump (P1)", "kind": "boolean", "group": "status"},
    "SV1_open": {"label": "shunt opening (SV1_open)", "kind": "boolean", "group": "status"},
    "SV1_close": {"label": "shunt closing (SV1_close)", "kind": "boolean", "group": "status"},
    "GP1": {"label": "low-pressure switch (GP1)", "kind": "boolean", "group": "status"},
    "GP2": {"label": "high-pressure switch (GP2)", "kind": "boolean", "group": "status"},
    "GP3": {"label": "defrost switch (GP3)", "kind": "boolean", "group": "status"},
    "vacation": {"label": "vacation mode (lowered feed)", "kind": "boolean", "group": "status"},
    "alarm": {"label": "alarm", "kind": "boolean", "group": "status", "notable": True},
    # limits: the pump's own bounds
    "GT1_LL": {"label": "feed lower limit (GT1_LL)", "kind": T, "group": "limits"},
    "GT1_UL": {"label": "feed upper limit (GT1_UL)", "kind": T, "group": "limits"},
    "GT1_LLT": {"label": "feed lower limit for backup (GT1_LLT)", "kind": T, "group": "limits"},
    "GT3_2_LL": {"label": "tank lower limit (GT3_2_LL)", "kind": T, "group": "limits"},
    "GT3_2_UL": {"label": "tank upper limit (GT3_2_UL)", "kind": T, "group": "limits"},
    "GT3_2_ULT": {"label": "tank upper limit for backup (GT3_2_ULT)", "kind": T, "group": "limits"},
    "GT3_3_LL": {"label": "heating water lower limit (GT3_3_LL)", "kind": T, "group": "limits"},
}
COMMAND_TIER = {
    "setpoint": "family",
    "operating_mode": "owner",
    "feed_temperature_target": "owner",
    "outdoor_temperature_offset": "owner",
}
COMMAND_STEP = {"setpoint": 0.5}


def aspect_descriptor(entity) -> dict:
    """ASPECT_FIELDS plus a `command` on each aspect this entity takes
    commands for (commands_for: a fed input has one master, so it is
    described but not commandable)."""
    fields = {aspect: dict(field) for aspect, field in ASPECT_FIELDS.items()}
    for aspect, (_field, bounds) in commands_for(entity).items():
        command = {"type": "enum" if bounds is None else "float", "editable_by": COMMAND_TIER[aspect]}
        if bounds is not None:
            command["constraint"] = {"min": bounds[0], "max": bounds[1]}
        if aspect in COMMAND_STEP:
            command["step"] = COMMAND_STEP[aspect]
        fields[aspect]["command"] = command
    return {"schema": 1, "groups": list(ASPECT_GROUPS), "fields": fields}


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
    """Unwraps a controller per-field payload into (value, valid): a plain
    JSON scalar passes through as (scalar, None), a nested
    {"value": ...[, "valid": ...]} object (the controller's tracked fields)
    yields its "value" member and its "valid" flag, None when the field
    carries none. Raises ValueError/KeyError on anything else — callers
    drop these with a "malformed-payload" health event."""
    parsed = json.loads(payload)
    if isinstance(parsed, dict):
        return parsed["value"], parsed.get("valid")
    return parsed, None


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


def commands_for(entity) -> dict:
    """COMMANDS minus any aspect whose set field this entity feeds: a fed
    input has one master."""
    fed = set(entity.inputs)
    return {aspect: cmd for aspect, cmd in COMMANDS.items() if cmd[0] not in fed}


def main():
    unit = os.environ[keys.ENV_UNIT]
    config = house.load_adapter(unit)
    for entity in config.entities:
        unknown = set(entity.inputs) - set(FEEDABLE)
        if unknown:
            raise SystemExit(
                f"{entity.name}: unknown input(s) {sorted(unknown)}; "
                f"this adapter feeds {sorted(FEEDABLE)}"
            )

    endpoint = mqtt.parse_endpoint(config.endpoint)

    session = homeostat.connect()
    params = Params(session, PARAM_DEFAULTS)
    seen: set[str] = set()

    # Receive-timer availability (see module docstring): last_rx is seeded
    # now so a device that never speaks flips false after one timeout.
    availability_lock = threading.Lock()
    last_rx = {e.id: time.monotonic() for e in config.entities}
    available: dict[str, bool] = {}

    def set_available(entity, value: bool) -> bool:
        """Publishes on transition only; returns True when it was one. The
        publish stays under the lock so the receive path and the watchdog
        cannot interleave decision and publication."""
        with availability_lock:
            if available.get(entity.name) == value:
                return False
            available[entity.name] = value
            session.put_json(keys.state_key(entity.room, entity.name, "available"), value)
        return True

    def inventory():
        return [
            {
                "id": e.id,
                "configured": True,
                "entity": e.name,
                "bound": e.id in seen,
                "suggested": {"capability": "climate", "features": []},
                "aspects": aspect_descriptor(e),
            }
            for e in config.entities
        ]

    def on_ivt_message(client, userdata, msg):
        entity, rest = route(msg.topic, config.entities)
        if entity is None:
            return

        last_rx[entity.id] = time.monotonic()
        set_available(entity, True)

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
                value, valid = field_value(msg.payload)
            except (ValueError, KeyError):
                session.health_event("drop", reason="malformed-payload", topic=msg.topic)
                return
            aspect = state_aspect("controller", rest[2])
            if valid is not None:
                # Ahead of the value, so a consumer reacting to the new
                # value reads this snapshot's validity from the mirror and
                # never the previous one's.
                session.put_json(
                    keys.state_key(entity.room, entity.name, f"{aspect}_valid"),
                    bool(valid),
                )
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

            command = commands_for(entity).get(aspect)
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

    subscribers = [
        session.subscribe(expr, cmd_handler(e))
        for e in config.entities
        for expr in keys.command_keyexprs(e)
    ]

    def feed_handler(entity, input_name, source):
        """Forwards the source aspect while the source is available; on
        loss, clears the set topic's retained slot once. One subscriber
        covers both the value and `available` keys: zenoh orders samples
        within a subscriber, not across two, and a value arriving before
        the `available = true` that precedes it must not be dropped."""
        lo, hi = FEEDABLE[input_name]
        topic = f"{entity.id}/controller/set/{input_name}"
        state = {"available": True, "dropped": False}
        value_key = keys.state_key(source.room, source.entity, source.aspect)
        available_key = keys.state_key(source.room, source.entity, "available")

        def handler(sample):
            key = str(sample.key_expr)
            if key not in (value_key, available_key):
                return  # another aspect of the source entity
            try:
                payload = json.loads(sample.payload.to_bytes())
            except ValueError:
                session.health_event("drop", reason="malformed-payload", key=key)
                return
            if key == available_key:
                if payload is False and state["available"]:
                    state["available"] = False
                    client.publish(topic, b"", retain=True)
                    session.health_event("feed-source-lost", input=input_name, key=value_key)
                elif payload is True:
                    state["available"] = True
                    state["dropped"] = False
                return
            if not state["available"]:
                # Once per outage, not per sample: a trace that the source
                # kept talking while marked unavailable.
                if not state["dropped"]:
                    state["dropped"] = True
                    session.health_event(
                        "drop", reason="feed-source-unavailable", input=input_name, key=key
                    )
                return
            if isinstance(payload, bool) or not isinstance(payload, (int, float)) or not (
                lo <= payload <= hi
            ):
                session.health_event(
                    "drop", reason="invalid-feed", input=input_name, key=key, value=payload
                )
                return
            client.publish(topic, str(float(payload)), retain=False)

        return handler

    for e in config.entities:
        for input_name, source in e.inputs.items():
            handler = feed_handler(e, input_name, source)
            subscribers.append(
                session.subscribe(keys.state_key(source.room, source.entity, "*"), handler)
            )

    session.put_json(keys.discovery_key(unit), inventory())

    stop = threading.Event()

    def watchdog():
        while True:
            timeout = params.availability_timeout_s
            if stop.wait(min(1.0, timeout / 4)):
                return
            now = time.monotonic()
            for entity in config.entities:
                if now - last_rx[entity.id] > timeout and set_available(entity, False):
                    # A degraded condition, not dropped input: kind =
                    # condition, the recorder's backend-outage precedent.
                    session.health_event("device-silent", topic=entity.id)

    watchdog_thread = threading.Thread(target=watchdog, daemon=True)
    watchdog_thread.start()

    # Both translation directions are wired up: the unit is ready.
    session.ready()

    mqtt.wait_for_shutdown()

    stop.set()
    watchdog_thread.join(timeout=5)
    for sub in subscribers:
        sub.undeclare()
    session.close()
    client.loop_stop()
    client.disconnect()


if __name__ == "__main__":
    main()
