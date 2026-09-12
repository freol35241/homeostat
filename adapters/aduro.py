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
"""Aduro pellet-burner adapter (docs/design.md, "Burners and interlocks
(settled 2026-09-09, #37, #38)").

The burner speaks the NBE UDP protocol; github.com/freol35241/aduro2mqtt
bridges it to MQTT. The bridge polls on a fixed interval
(ADURO_POLL_INTERVAL, default 30 s) and republishes EVERY topic each cycle,
changed or not: {base}/status (the `status *` response, positional CSV
mapped onto pyduro's STATUS_PARAMS names — 116 fields — as one JSON
object, values floated where they parse), {base}/operating and
{base}/advanced (one JSON object each), {base}/settings/{group} (17 groups
of device configuration), {base}/consumption/{key} (9 arrays) and
{base}/logs. Commands go to {base}/set as {"path": "<group>.<name>",
"value": ...}. The entity file's `id` is the bridge's MQTT_BASE_TOPIC
(`aduro2mqtt` by default); the file stem is the entity name. One entity
per burner.

What becomes state, and how much. Republish-on-poll means an adapter
forwarding each message would record every field 2,700 times a day
whether or not it ever moved — the status topic alone is two and a half
times the reporting house's entire heat-pump adapter (#37). So the
adapter subscribes exactly {base}/status and {base}/operating, and
publishes a field ONLY WHEN ITS VALUE CHANGES (plus once, on the first
poll after start; late joiners read the core's state mirror). Settings
are configuration, not samples; consumption, advanced and logs are the
long tail — none are subscribed. Status fields publish under their
firmware names (dots included: `regulation.fixed_power` is a legal key
segment); operating fields publish as `operating_{field}`, the prefix
keeping the two NBE namespaces apart without a table of one to check
the other against.

Four normalizations carry the `burner` vocabulary:

- `on` (the base aspect, bool) is DERIVED from the run state: the burner
  is on unless `state` is one of OFF_STATES. Start and stop are momentary
  writes in this dialect (misc.start / misc.stop), so `on` can only ever
  be the device's own readback, never an echo of a command. OFF_STATES
  holds the single code observed so far — 14, the burner idle and unlit
  through the survey that settled #37 — and grows as the heating season
  produces codes; `state` and `substate` pass through raw beside it for
  exactly that purpose. A code not in OFF_STATES reads as on.
- `power_level` (feature) is status `regulation.fixed_power`, the fixed
  output setting, 10 / 50 / 100 on this device — published as an int
  when integral so the enum matches.
- `flue_temperature` is status `smoke_temp` (the exhaust reading; the
  flue cutouts at the reporting house read this one).
- `boiler_temperature` is status `boiler_temp`.

Everything else passes through, including `shaft_temp` (the feed shaft,
the device's own fire-safety reading) and `power_pct` (actual output) —
except a raw field that would mint a reserved name (`available`, or one
of the normalized names above arriving under its bus name rather than
its firmware name), which drops with a "reserved-aspect" event, and a
field name that is not a legal key segment, which drops with
"malformed-payload"; the rest of the document still publishes.

Commands (COMMANDS) — the burner is an arbitrated entity (the family's
`on` and an automation's `power_level` lease independently, per aspect),
so every command arrives on home/arbiter/{room}/{entity}/{aspect}:

- `on`: strictly a bool. true -> {"path": "misc.start", "value": "1"},
  false -> {"path": "misc.stop", "value": "1"}, the bridge's own switch
  shape.
- `power_level`: strictly the integer 10, 50 or 100 ->
  {"path": "regulation.fixed_power", "value": <int>}.

Anything else — a wrong type, an out-of-enum level, an unknown aspect,
a malformed or envelope-less payload — DROPS with an "invalid-command"
(or "malformed-payload") health event and never reaches the device.

Discovery is the static one-record-per-entity document (the ivt490
shape): base-topic id, a suggested `burner` capability stanza with the
`power_level` feature, the aspect descriptor (ASPECT_FIELDS) and a
`bound` flag that flips true the first time the base topic is seen.

Availability: the bridge publishes on a cadence, so silence is the loss
signal — a receive timer flips home/state/{room}/{entity}/available to
false after availability_timeout_s (parameter, owner-editable, default
300 s, about nine polls) without a message from the base topic, with one
"device-silent" health event per down transition; the next message flips
it back. The bridge skips a topic's publish when the burner does not
answer, so an unreachable burner and a dead bridge both go silent. On
loss every other aspect stands — stale, never false.

Operational note: the bridge's README wires Home Assistant switches and
selects straight to {base}/set; disable them (and any Node-RED writer)
before this adapter goes live — one master per device.
"""

import json
import os
import threading
import time

import homeostat
from homeostat import house, keys, mqtt
from homeostat.params import LiveParams

PARAM_DEFAULTS = {"availability_timeout_s": 300.0}
_UNSET = object()


class Params(LiveParams):
    """availability_timeout_s from home/config/{unit}/*, live."""

    @property
    def availability_timeout_s(self) -> float:
        return max(0.1, self.get("availability_timeout_s"))


# Status field -> burner vocabulary (see module docstring).
ASPECT_OVERRIDES = {
    "smoke_temp": "flue_temperature",
    "boiler_temp": "boiler_temperature",
    "regulation.fixed_power": "power_level",
}

# Run-state codes that mean the burner is not making heat. 14 is the one
# code observed to date (idle, unlit); extend from the heating season.
OFF_STATES = frozenset({14})

# Names a status field may not mint raw: the adapter's own liveness
# signal, and the vocabulary the overrides derive (a wire field literally
# named `on` would otherwise overwrite the derived one and poison the
# publish-on-change cache).
RESERVED_STATUS_FIELDS = frozenset({"available", "on", *ASPECT_OVERRIDES.values()})

POWER_LEVELS = (10, 50, 100)

# Commandable aspect -> the NBE set path (None for `on`, whose path
# depends on the value: misc.start / misc.stop).
COMMANDS = {"on": None, "power_level": "regulation.fixed_power"}

# The aspect descriptor (docs/design.md, Aspect descriptors). The dashboard
# renders descriptor commands as enums or numbers, so `on` is described as
# a two-valued enum — a segmented off/on control on the card. Labels keep
# the firmware field name in parentheses where it differs.
ASPECT_GROUPS = ["control", "readings", "status"]
T = "temperature"
ASPECT_FIELDS = {
    "on": {
        "label": "burner",
        "kind": "boolean",
        "group": "control",
        "values": [{"value": False, "label": "off"}, {"value": True, "label": "on"}],
        "command": {"type": "enum", "editable_by": "family"},
    },
    "power_level": {
        "label": "power",
        "kind": "enum",
        "group": "control",
        "values": [{"value": level, "label": f"{level}%"} for level in POWER_LEVELS],
        "command": {"type": "enum", "editable_by": "family"},
    },
    "flue_temperature": {"label": "flue (smoke_temp)", "kind": T, "group": "readings"},
    "boiler_temperature": {"label": "boiler (boiler_temp)", "kind": T, "group": "readings"},
    "shaft_temp": {"label": "feed shaft (shaft_temp)", "kind": T, "group": "readings"},
    "power_pct": {"label": "output (power_pct)", "kind": "percent", "group": "readings"},
    "state": {"label": "run state (state)", "kind": "number", "group": "status"},
    "substate": {"label": "run substate (substate)", "kind": "number", "group": "status"},
}
ASPECT_DESCRIPTOR = {"schema": 1, "groups": ASPECT_GROUPS, "fields": ASPECT_FIELDS}


def status_aspects(status: dict) -> tuple[dict, list[str]]:
    """One status document to the bus aspects it yields: the normalized
    names, the derived `on`, and every other field under its firmware
    name — plus the raw fields dropped for naming a reserved aspect.
    `power_level` is an int when the wire float is integral."""
    aspects = {}
    reserved = []
    for field, value in status.items():
        if field in RESERVED_STATUS_FIELDS:
            reserved.append(field)
            continue
        aspect = ASPECT_OVERRIDES.get(field, field)
        if aspect == "power_level" and isinstance(value, float) and value.is_integer():
            value = int(value)
        aspects[aspect] = value
    state = status.get("state")
    if isinstance(state, (int, float)):  # a run-state code is a number
        aspects["on"] = state not in OFF_STATES
    return aspects, reserved


def command_body(aspect: str, value):
    """The {base}/set payload for a validated command, or None when the
    value is not one this aspect takes (a bool for `on`, one of
    POWER_LEVELS — an int, never a bool — for `power_level`)."""
    if aspect == "on":
        if not isinstance(value, bool):
            return None
        return {"path": "misc.start" if value else "misc.stop", "value": "1"}
    if aspect == "power_level":
        if isinstance(value, bool) or not isinstance(value, int) or value not in POWER_LEVELS:
            return None
        return {"path": COMMANDS[aspect], "value": value}
    return None


def route(topic: str, entities):
    """The bound entity and the topic's segments past its base-topic
    prefix, or (None, None)."""
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
    params = Params(session, PARAM_DEFAULTS)
    seen: set[str] = set()

    # Publish-on-change: the last value put on the bus per (entity, aspect).
    last: dict[tuple[str, str], object] = {}

    availability_lock = threading.Lock()
    last_rx = {e.id: time.monotonic() for e in config.entities}
    available: dict[str, bool] = {}

    def set_available(entity, value: bool) -> bool:
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
                "suggested": {"capability": "burner", "features": ["power_level"]},
                "aspects": ASPECT_DESCRIPTOR,
            }
            for e in config.entities
        ]

    def on_aduro_message(client, userdata, msg):
        entity, rest = route(msg.topic, config.entities)
        if entity is None or len(rest) != 1 or rest[0] not in ("status", "operating"):
            return

        last_rx[entity.id] = time.monotonic()
        set_available(entity, True)
        if entity.id not in seen:
            seen.add(entity.id)
            session.put_json(keys.discovery_key(unit), inventory())

        try:
            document = json.loads(msg.payload)
            if not isinstance(document, dict):
                raise ValueError("not an object")
        except ValueError:
            session.health_event("drop", reason="malformed-payload", topic=msg.topic)
            return

        if rest[0] == "status":
            aspects, reserved = status_aspects(document)
            for field in reserved:
                session.health_event("drop", reason="reserved-aspect", topic=msg.topic, field=field)
        else:
            aspects = {f"operating_{field}": value for field, value in document.items()}
        for aspect, value in aspects.items():
            try:
                key = keys.state_key(entity.room, entity.name, aspect)
            except ValueError:
                session.health_event(
                    "drop", reason="malformed-payload", topic=msg.topic, field=aspect
                )
                continue
            if last.get((entity.name, aspect), _UNSET) == value:
                continue  # unchanged since the last poll: not a sample
            last[(entity.name, aspect)] = value
            session.put_json(key, value)

    def cmd_handler(entity):
        def handler(sample):
            parsed = session.parse_command(sample)
            if parsed is None:
                return
            aspect, value = parsed
            body = command_body(aspect, value) if aspect in COMMANDS else None
            if body is None:
                session.health_event(
                    "drop", reason="invalid-command", key=str(sample.key_expr), aspect=aspect, value=value
                )
                return
            client.publish(f"{entity.id}/set", json.dumps(body))

        return handler

    topics = [
        (topic, 0)
        for e in config.entities
        for topic in (f"{e.id}/status", f"{e.id}/operating")
    ]
    client = mqtt.connect(endpoint, on_aduro_message, topics)

    subscribers = [
        session.subscribe(expr, cmd_handler(e))
        for e in config.entities
        for expr in keys.command_keyexprs(e)
    ]

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
                    session.health_event("device-silent", topic=entity.id)

    watchdog_thread = threading.Thread(target=watchdog, daemon=True)
    watchdog_thread.start()

    session.ready()

    mqtt.wait_for_shutdown()

    stop.set()
    watchdog_thread.join(timeout=5)
    # The MQTT loop stops first: an in-flight on_message during teardown
    # would otherwise put on a closed zenoh session.
    client.loop_stop()
    client.disconnect()
    for sub in subscribers:
        sub.undeclare()
    session.close()


if __name__ == "__main__":
    main()
