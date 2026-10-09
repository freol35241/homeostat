# /// script
# requires-python = ">=3.11"
# dependencies = [
#     "homeostat==0.17.0",
#     "paho-mqtt>=2,<3",
# ]
# ///
"""IVT490 adapter: an IVT490 heat pump through its ESP8266 interface board.

The board (github.com/freol35241/IVT490-interface-esp8266, tracked on the
GT3_2_boiler_emulation branch) reads the pump's control board over
serial, emulates the GT2 digipot and drives an EXT_IN relay. It is
house-specific hardware with no replacement firmware to move to, so the
adapter speaks the board's own MQTT dialect.

Binding: an entity file's `id` is the board's MQTT base topic; each board
owns its whole topic tree. The file stem is the entity name, with one
entity per heat pump and capability `climate`. The entity is arbitrated
(docs/design.md#arbitrated-mode), so commands arrive already arbitrated
on home/arbiter/{room}/{entity}/{aspect}, and the family's manual
setpoint wins.

State: scalar leaves under {base}/ivt490/state and {base}/controller/state
publish to home/state/{room}/{entity}/{aspect} (`Pumps.on_message`). A
controller field that carries a validity flag also publishes
`{aspect}_valid`. `available` goes false after availability_timeout_s
without a message from the board (`Pumps.sweep`).

Commands (COMMANDS): setpoint, feed_temperature_target,
outdoor_temperature_offset and operating_mode. Only the setpoint is
published retained; the comment on COMMANDS explains why.

Feeds: indoor_temperature_actual and outdoor_temperature_offset can be
wired to a source with `[inputs] <input> = { entity, aspect }` in the
entity file (`Feed`). An unknown input name stops the unit at startup.

Configuration: the endpoint is the MQTT broker, with credentials inline
or in HOMEOSTAT_MQTT_CREDENTIALS. The live parameter
availability_timeout_s is owner-editable, default 300 s.

Discovery: one record per bound entity with its aspect descriptor
(`aspect_descriptor`) and a `bound` flag (`Pumps.inventory`).

Health events: `drop` (malformed-payload, reserved-aspect, null-value,
invalid-command, invalid-feed, feed-source-unavailable), device-silent
and feed-source-lost.

Operational note: disable any Node-RED flow that writes to this board's
controller/set topics before this adapter goes live. Read-only flows can
stay.
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
# every aspect the state fan-out can produce. These are the 28
# serial-protocol fields (with the "serial" wrapper stripped) and the
# underscore-joined thermistor sensor leaves. state_aspect() checks
# controller fields against this set for collisions.
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

# The three firmware fields that map to the climate vocabulary: serial GT1
# (Framledningstemperatur, the feed line reading), the controller's live
# indoor reading, and the controller's indoor target. The bus setpoint is
# therefore the device's own readback, not an echo of a command.
ASPECT_OVERRIDES = {
    ("state", "GT1"): "feed_temperature",
    ("controller", "indoor_temperature_feedback"): "indoor_temperature",
    ("controller", "indoor_temperature_target"): "setpoint",
}

# The three operating modes of the GT3_2 boiler-sensor emulation
# (src/Controller.h, OperatingMode): 1=BAU (normal), 2=BLOCK (suppress
# heating), 3=BOOST (force heating).
OPERATING_MODES = (1, 2, 3)

# The outdoor-temperature offset's bound, in kelvin, shared by the command
# aspect and the feedable input. The firmware has no range check: the
# offset is added as-is to the outdoor reading, and the NTC emulator
# saturates at the ends of its digipot (roughly -33 degC to a readback of
# about 130 degC). This bound is the only check in the chain, so it must
# admit anything the firmware can act on.
OFFSET_BOUNDS = (-50.0, 50.0)

# Device inputs an entity file may wire to a source
# (docs/design.md#device-feeds), with their bounds. Where a matching
# command exists, the bounds are the same.
FEEDABLE = {
    "indoor_temperature_actual": (-50.0, 60.0),
    "outdoor_temperature_offset": OFFSET_BOUNDS,
}

# Commandable aspect -> ({base}/controller/set/{field}, (min, max) for the
# float aspects or None for operating_mode, retain).
#
# The bounds are adapter constants because they are device physics, not
# house configuration. A command of the wrong type or out of range drops
# with `invalid-command`, carrying the aspect and value; nothing is
# clamped. The firmware stores each control value as given, so these
# bounds are the only check in the chain and must admit everything the
# firmware can act on. Values are sent as stringified floats, and
# operating_mode as an integer string. operating_mode is the GT3_2
# boiler-sensor emulation's mode (see OPERATING_MODES).
#
# The retain flag follows a distinction the firmware makes, visible in
# what it publishes back (docs/adapters.md, Retaining a command):
#
#   feed_temperature_target      {"value": 0,     "valid": false}
#   indoor_temperature_feedback  {"value": 18.84, "valid": true}
#   outdoor_temperature_offset   {"value": 0.013, "valid": true}
#   indoor_temperature_target    {"value": 19}                     <- no `valid`
#
# GENERAL_CONTROL_VALUES_VALIDITY expires the timestamped values. The
# setpoint has no validity flag because it does not go stale: it is a
# stored decision, and one set four days ago is as true as one set a
# minute ago.
#
# The setpoint is retained. The firmware default is 20 degC, and a reboot
# adopts it. At the house this was written for, the board reboots daily
# on an uptime timer, so a non-retained setpoint would be lost every day.
# The retained value is redelivered when the board reconnects, which also
# repairs a write lost in transit.
#
# The other three are not retained, and getting this wrong is the
# dangerous case. An automation that stops writing (stale inputs, a dead
# price feed) relies on the firmware expiring its value so the pump falls
# back to curve control. A retained value would be redelivered on the
# next reconnect and re-applied after the writer stopped, leaving the
# pump stuck on it. These values stay fresh through their writer's
# cadence instead. Fed inputs are not retained for the same reason (see
# Feed).
COMMANDS = {
    "setpoint": ("indoor_temperature_target", (10.0, 30.0), True),
    "feed_temperature_target": ("feed_temperature_target", (20.0, 60.0), False),
    "outdoor_temperature_offset": ("outdoor_temperature_offset", OFFSET_BOUNDS, False),
    "operating_mode": ("operating_mode", None, False),
}

# The aspect descriptor (docs/design.md#aspect-descriptors) this adapter
# publishes in each bound entity's discovery record: labels, kinds and
# groups for the aspects worth a family-facing name, and the commands the
# dashboard may render, with bounds taken from COMMANDS. The family tier
# gets the indoor target, the one control that is family intent. The
# GT3_2 emulation's mode is driven by an automation at the house this was
# written for, so it is owner tuning like the feed target and curve
# offset: shown with a badge and written only through the bus. Labels
# keep the firmware's sensor code in parentheses so the page and the
# manual agree on what a reading is. Aspects not named here still publish
# and render, in the page's diagnostics group under their firmware names.
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
# How long a command takes to come back
# (docs/design.md#aspect-descriptors: readback_s). The firmware stores a
# set value at once and reports it in its controller state, published
# every GENERAL_STATE_PUBLISH_INTERVAL (10 s in the firmware's config
# template). Three cycles, so one dropped publish does not read as "no
# answer".
READBACK_S = 30


def aspect_descriptor(entity) -> dict:
    """Return ASPECT_FIELDS plus a `command` on each aspect this entity takes commands for.

    See commands_for: a fed input has one master, so it is described but
    not commandable.
    """
    fields = {aspect: dict(field) for aspect, field in ASPECT_FIELDS.items()}
    for aspect, (_field, bounds, _retain) in commands_for(entity).items():
        command = {"type": "enum" if bounds is None else "float", "editable_by": COMMAND_TIER[aspect]}
        if bounds is not None:
            command["constraint"] = {"min": bounds[0], "max": bounds[1]}
        if aspect in COMMAND_STEP:
            command["step"] = COMMAND_STEP[aspect]
        fields[aspect]["command"] = command
    return {"schema": 1, "groups": list(ASPECT_GROUPS), "fields": fields, "readback_s": READBACK_S}


def state_field(segments: list[str]) -> str:
    """Map a flattened subtopic path under {base}/ivt490/state to a firmware field name.

    The firmware's document wraps the 28 serial-protocol fields in a
    "serial" object and carries two thermistor sensor objects, "GT2" and
    "GT3_2", each {raw, filtered}. The "serial" wrapper is a serialization
    artifact and is stripped, so serial fields keep their bare firmware
    names. Any other nested path joins with underscores (GT2/raw ->
    GT2_raw), so the aspect stays a single key segment. None of the joined
    names collides with a serial field name.
    """
    if segments and segments[0] == "serial":
        segments = segments[1:]
    return "_".join(segments)


# Names a raw firmware field may not mint: the adapter's own liveness
# signal and the normalized names, which only their overrides may yield.
RESERVED_ASPECTS = frozenset({"available", *ASPECT_OVERRIDES.values()})


def state_aspect(source: str, field: str) -> str | None:
    """Map one firmware field (`source` "state" or "controller") to a bus aspect name.

    The name is one of the three normalizations (ASPECT_OVERRIDES) or the
    firmware name. A controller field that shares a name with a state
    aspect gets the prefix `controller_`, so the two namespaces stay
    apart. No such collision exists in the current firmware; the check is
    for a later revision. Returns None for a raw field that would publish
    as a reserved name: `available`, or a normalized name arriving under
    its bus name. Callers drop those with `reserved-aspect`.
    """
    override = ASPECT_OVERRIDES.get((source, field))
    if override is not None:
        return override
    if field in RESERVED_ASPECTS:
        return None
    if source == "controller" and field in STATE_ASPECTS:
        return f"controller_{field}"
    return field


def field_value(payload: bytes):
    """Unwrap a controller per-field payload into (value, valid).

    The controller (src/Controller.h, Controller::serialize) publishes
    feed_temperature_target, indoor_temperature_feedback and
    outdoor_temperature_offset as {value, valid}, indoor_temperature_target
    as {value}, and operating_mode as a bare integer. A plain JSON scalar
    passes through as (scalar, None). A {"value": ...[, "valid": ...]}
    object yields its value and its valid flag, or None for the flag when
    there is none. Raises ValueError or KeyError on anything else; callers
    drop those with `malformed-payload`.
    """
    parsed = json.loads(payload)
    if isinstance(parsed, dict):
        return parsed["value"], parsed.get("valid")
    return parsed, None


def route(topic: str, entities):
    """Return the bound entity and the topic's segments past its base-topic prefix.

    (None, None) when the topic is under no bound entity's base topic.
    That should not happen, since the adapter subscribes only
    `{entity.id}/...` per entity.
    """
    for entity in entities:
        prefix = f"{entity.id}/"
        if topic.startswith(prefix):
            return entity, topic[len(prefix) :].split("/")
    return None, None


def commands_for(entity) -> dict:
    """Return COMMANDS minus any aspect whose set field this entity feeds.

    A fed input has one master, so a command naming it drops with
    `invalid-command` like any unknown aspect.
    """
    fed = set(entity.inputs)
    return {aspect: cmd for aspect, cmd in COMMANDS.items() if cmd[0] not in fed}


def command_payload(entity, aspect: str, value):
    """Return (set field, body, retain) for a command this entity takes, or None.

    None for an aspect it takes no command for (including one it feeds),
    and for a value outside the command's range: an int of OPERATING_MODES
    for operating_mode, otherwise a number within bounds (not a bool).

    The firmware's toFloat()/toInt() treat 0 as a parse failure, so the
    device discards an outdoor_temperature_offset of 0.0 even though it is
    in range. The adapter sends it anyway and does not work around the
    firmware.
    """
    command = commands_for(entity).get(aspect)
    if command is None:
        return None
    field, bounds, retain = command
    if bounds is None:
        if isinstance(value, bool) or not isinstance(value, int) or value not in OPERATING_MODES:
            return None
        return field, str(value), retain
    lo, hi = bounds
    if isinstance(value, bool) or not isinstance(value, (int, float)) or not lo <= value <= hi:
        return None
    return field, str(float(value)), retain


class Pumps:
    """The controller-to-bus direction: state subtopics in, aspects out.

    It tracks each controller's availability from when it last spoke and
    keeps discovery's `bound` flag. `session` is anything with `put_json`
    and `health_event`; the clock is injectable. paho delivers on one
    thread and the watchdog sweeps on another, so availability is locked.
    """

    def __init__(self, session, unit: str, entities, clock=time.monotonic):
        self.session = session
        self.unit = unit
        self.entities = list(entities)
        self.clock = clock
        self.seen: set[str] = set()
        self.lock = threading.Lock()
        # Seeded now, so a device that never speaks flips false after one
        # timeout.
        self.last_rx = {e.id: clock() for e in self.entities}
        self.available: dict[str, bool] = {}

    def set_available(self, entity, value: bool) -> bool:
        """Publish `available` on transition only; return True when it was one.

        The publish stays under the lock so the receive path and the
        watchdog cannot interleave decision and publication.
        """
        with self.lock:
            if self.available.get(entity.name) == value:
                return False
            self.available[entity.name] = value
            self.session.put_json(keys.state_key(entity.room, entity.name, "available"), value)
        return True

    def inventory(self) -> list[dict]:
        """Return the discovery document: every configured controller.

        Each record has the base-topic id, a suggested `climate` stanza, the
        aspect descriptor, and `bound`, which turns true (with a republish)
        the first time the base topic is seen on the broker. The document
        is static. This dialect has one address per entity file, known up
        front, so there is no incremental inventory of unbound devices as
        in OwnTracks or Zigbee2MQTT.
        """
        return [
            {
                "id": e.id,
                "configured": True,
                "entity": e.name,
                "bound": e.id in self.seen,
                "suggested": {"capability": "climate", "features": []},
                "aspects": aspect_descriptor(e),
            }
            for e in self.entities
        ]

    def on_message(self, topic: str, raw: bytes) -> None:
        """Translate one message under a controller's base topic.

        The adapter subscribes {base}/ivt490/state/+, {base}/ivt490/state/+/+
        and {base}/controller/state/+. The firmware publishes the whole
        document as one blob, then each sub-object as a blob and each
        scalar leaf on its own subtopic (src/main.cpp,
        publish_json_object), so an object payload is skipped: its leaves
        arrive on deeper subtopics. {base}/ivt490/raw, the unparsed serial
        line, is not subscribed. Any message marks the board available.
        """
        session = self.session
        entity, rest = route(topic, self.entities)
        if entity is None:
            return

        self.last_rx[entity.id] = self.clock()
        self.set_available(entity, True)

        if entity.id not in self.seen:
            self.seen.add(entity.id)
            session.put_json(keys.discovery_key(self.unit), self.inventory())

        if len(rest) in (3, 4) and rest[0] == "ivt490" and rest[1] == "state":
            try:
                value = json.loads(raw)
            except ValueError:
                session.health_event("drop", reason="malformed-payload", topic=topic)
                return
            if isinstance(value, (dict, list)):
                return  # a nested blob; its leaves arrive on deeper subtopics
            valid = None
            aspect = state_aspect("state", state_field(rest[2:]))
        elif len(rest) == 3 and rest[0] == "controller" and rest[1] == "state":
            try:
                value, valid = field_value(raw)
            except (ValueError, KeyError):
                session.health_event("drop", reason="malformed-payload", topic=topic)
                return
            aspect = state_aspect("controller", rest[2])
        else:
            return  # a whole-document blob topic

        if aspect is None:
            # A raw field that would publish as `available` or as a
            # normalized aspect.
            session.health_event("drop", reason="reserved-aspect", topic=topic)
            return
        try:
            key = keys.state_key(entity.room, entity.name, aspect)
        except ValueError:
            # A topic segment the key schema refuses (empty, a wildcard).
            session.health_event("drop", reason="malformed-payload", topic=topic)
            return
        if valid is not None:
            # The firmware's `valid` goes false once a timestamped reading
            # ages past the device's validity window. The control loop then
            # drops that term and falls back to curve-only control on the
            # filtered outdoor sensor. Publishing the value alone would
            # show a number the device no longer trusts, so the flag is an
            # aspect of its own. That keeps "stale" and "absent" apart,
            # live and in the recorder. onvif.py makes the same split for
            # motion, inferred there rather than reported.
            #
            # The flag goes out before the value, so a consumer reacting to
            # the new value reads this snapshot's validity from the mirror.
            session.put_json(keys.state_key(entity.room, entity.name, f"{aspect}_valid"), bool(valid))
        if value is None:
            # The controller republishes its tracked fields as
            # {"value": null} until it has a reading, in bursts for a
            # minute or two after the board's daily reboot. On the bus,
            # "no reading" means not publishing; `valid` above has already
            # gone out. A null would sit on a key whose descriptor says
            # number, and a `subscribe` catch-up would replay it as the
            # last known value. The event keeps the burst visible.
            session.health_event("drop", reason="null-value", topic=topic)
            return
        session.put_json(key, value)

    def sweep(self, timeout_s: float) -> None:
        """Mark a controller silent for `timeout_s` unavailable, one event per transition.

        The firmware publishes on every serial telegram, so silence is the
        loss signal. Any routed message marks the board available again.
        The other aspects keep their last values.
        """
        now = self.clock()
        for entity in self.entities:
            if now - self.last_rx[entity.id] > timeout_s and self.set_available(entity, False):
                # A degraded condition, so its own event kind.
                self.session.health_event("device-silent", topic=entity.id)


class Feed:
    """One fed input: forward the source aspect while the source is available.

    Each sample within bounds goes to {base}/controller/set/{input} as a
    stringified float, not retained: the firmware's own validity window
    ends a stale feed, and a retained value would outlive its source
    across a reconnect. While the source's `available` is false nothing is
    forwarded, and the topic's retained slot is cleared once with an empty
    retained publish, so a value left by an earlier writer cannot stand in
    for the lost source.

    One subscriber covers both the value and `available` keys. zenoh
    orders samples within a subscriber but not across two, and a value
    must not be dropped because the `available = true` before it was
    delivered second. `publish` is the MQTT client's publish(topic,
    payload, retain=...).
    """

    def __init__(self, session, publish, entity, input_name: str, source):
        self.session = session
        self.publish = publish
        self.input_name = input_name
        self.bounds = FEEDABLE[input_name]
        self.topic = f"{entity.id}/controller/set/{input_name}"
        self.value_key = keys.state_key(source.room, source.entity, source.aspect)
        self.available_key = keys.state_key(source.room, source.entity, "available")
        self.source_available = True
        self.dropped = False

    def on_sample(self, key: str, raw: bytes) -> None:
        """Handle one sample from the source entity's state keys."""
        session = self.session
        if key not in (self.value_key, self.available_key):
            return  # another aspect of the source entity
        try:
            payload = json.loads(raw)
        except ValueError:
            session.health_event("drop", reason="malformed-payload", key=key)
            return
        if key == self.available_key:
            if payload is False and self.source_available:
                self.source_available = False
                self.publish(self.topic, b"", retain=True)
                session.health_event("feed-source-lost", input=self.input_name, key=self.value_key)
            elif payload is True:
                self.source_available = True
                self.dropped = False
            return
        if not self.source_available:
            # Once per outage: a trace that the source kept publishing
            # while marked unavailable.
            if not self.dropped:
                self.dropped = True
                session.health_event(
                    "drop", reason="feed-source-unavailable", input=self.input_name, key=key
                )
            return
        lo, hi = self.bounds
        if isinstance(payload, bool) or not isinstance(payload, (int, float)) or not lo <= payload <= hi:
            session.health_event(
                "drop", reason="invalid-feed", input=self.input_name, key=key, value=payload
            )
            return
        self.publish(self.topic, str(float(payload)), retain=False)


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
    pumps = Pumps(session, unit, config.entities)

    def cmd_handler(entity):
        def handler(sample):
            parsed = session.parse_command(sample)
            if parsed is None:
                return
            aspect, value, cmd_id = parsed
            command = command_payload(entity, aspect, value)
            if command is None:
                session.health_event(
                    "drop",
                    reason="invalid-command",
                    key=str(sample.key_expr),
                    aspect=aspect,
                    value=value,
                    cmd_id=cmd_id,
                )
                return
            field, body, retain = command
            client.publish(f"{entity.id}/controller/set/{field}", body, retain=retain)

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
    client = mqtt.connect(
        endpoint,
        lambda _client, _userdata, msg: pumps.on_message(msg.topic, msg.payload),
        topics,
        health=session.health_event,
    )

    subscribers = [
        session.subscribe(expr, cmd_handler(e))
        for e in config.entities
        for expr in keys.command_keyexprs(e)
    ]

    for e in config.entities:
        for input_name, source in e.inputs.items():
            feed = Feed(session, client.publish, e, input_name, source)
            subscribers.append(
                session.subscribe(
                    keys.state_keyexpr(source.room, source.entity),
                    lambda sample, feed=feed: feed.on_sample(
                        str(sample.key_expr), sample.payload.to_bytes()
                    ),
                )
            )

    session.put_json(keys.discovery_key(unit), pumps.inventory())

    stop = threading.Event()

    def watchdog():
        while True:
            timeout = params.availability_timeout_s
            if stop.wait(min(1.0, timeout / 4)):
                return
            pumps.sweep(timeout)

    watchdog_thread = threading.Thread(target=watchdog, daemon=True)
    watchdog_thread.start()

    session.ready()

    mqtt.wait_for_shutdown()

    stop.set()
    watchdog_thread.join(timeout=5)
    # Stop the MQTT loop first, or an in-flight on_message could put on
    # a closed zenoh session.
    client.loop_stop()
    client.disconnect()
    for sub in subscribers:
        sub.undeclare()
    session.close()


if __name__ == "__main__":
    main()
