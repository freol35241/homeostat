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
"""Aduro adapter: an Aduro pellet burner through the aduro2mqtt bridge.

See docs/design.md#burners-and-interlocks. The burner speaks the NBE UDP
protocol, and github.com/freol35241/aduro2mqtt bridges it to MQTT. The
bridge polls every ADURO_POLL_INTERVAL (default 30 s) and republishes
every topic each cycle, changed or not. This adapter reads
{base}/status and {base}/operating and sends commands to {base}/set.

Binding: an entity file's `id` is the bridge's MQTT_BASE_TOPIC
(`aduro2mqtt` by default), and the file stem is the entity name. One
entity per burner, capability `burner` with the `power_level` feature.
The entity is arbitrated, so the family's `on` and an automation's
`power_level` hold separate leases, and commands arrive on
home/arbiter/{room}/{entity}/{aspect}.

State: each status field publishes under its firmware name, each
operating field as `operating_{field}`, and only when its value changes
(`Stoves.on_message`). `on`, `power_level`, `flue_temperature` and
`boiler_temperature` are the burner vocabulary (`status_aspects`).
`available` goes false after availability_timeout_s without a message
(`Stoves.sweep`).

Commands: `on` (a bool) and `power_level` (10, 50 or 100); see
`command_body`.

Configuration: the endpoint is the MQTT broker, with credentials inline
or in HOMEOSTAT_MQTT_CREDENTIALS. The live parameter
availability_timeout_s is owner-editable, default 300 s (about nine
polls).

Discovery: one record per entity with the aspect descriptor and a
`bound` flag that turns true the first time the base topic is seen.

Health events: `drop` (malformed-payload, reserved-aspect,
invalid-command) and device-silent.

Combustion safety is not this adapter's job. The burner has its own
alarm layer (shaft and boiler temperature limits). A house-local flue
cutout publishing `on = false` is an ordinary automation that can be
overridden, and the house must not rely on it.

Operational note: the bridge's README wires Home Assistant switches and
selects straight to {base}/set. Disable them, and any Node-RED writer,
before this adapter goes live, so it is the device's only writer.
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


# Status field -> burner vocabulary. `power_level` is the fixed output
# setting (10 / 50 / 100 on this device). `flue_temperature` is the
# exhaust reading, which the flue cutouts at the house this was written
# for read. The derived `on` is in status_aspects.
ASPECT_OVERRIDES = {
    "smoke_temp": "flue_temperature",
    "boiler_temp": "boiler_temperature",
    "regulation.fixed_power": "power_level",
}

# Run-state codes that mean the burner is not making heat. 14 (idle,
# unlit) is the only code observed so far. Add codes as the heating
# season shows them; `state` and `substate` are published raw so they can
# be read off the bus.
OFF_STATES = frozenset({14})

# Names a raw status field may not publish as: the adapter's own liveness
# signal and the normalized vocabulary. A wire field named `on` would
# otherwise overwrite the derived one and corrupt the publish-on-change
# cache.
RESERVED_STATUS_FIELDS = frozenset({"available", "on", *ASPECT_OVERRIDES.values()})

POWER_LEVELS = (10, 50, 100)

# Commandable aspect -> the NBE set path (None for `on`, whose path
# depends on the value: misc.start / misc.stop).
#
# Commands are not retained. They all share the one {base}/set topic, so
# a retained slot would hold whichever came last, and a misc.start or
# misc.stop replayed when the bridge reconnects would act, not restore.
COMMANDS = {"on": None, "power_level": "regulation.fixed_power"}

# The aspect descriptor (docs/design.md#aspect-descriptors). The dashboard
# renders descriptor commands as enums or numbers, so `on` is described as
# a two-valued enum, which shows as an off/on control on the card. Labels
# keep the firmware field name in parentheses where it differs.
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
# How long a command takes to come back (docs/design.md#aspect-descriptors:
# readback_s). The bridge polls every ADURO_POLL_INTERVAL (30 s by default)
# and a command is read back on a poll after the burner acted on it: two
# polls and some slack. A bridge configured to poll slower outruns this,
# and the page then says "no answer" early.
READBACK_S = 75
ASPECT_DESCRIPTOR = {
    "schema": 1,
    "groups": ASPECT_GROUPS,
    "fields": ASPECT_FIELDS,
    "readback_s": READBACK_S,
}


def status_aspects(status: dict) -> tuple[dict, list[str]]:
    """Map one status document to the bus aspects it yields.

    Returns the aspects and the raw fields dropped for naming a reserved
    aspect. The aspects are the normalized names, the derived `on`, and
    every other field under its firmware name, including `shaft_temp` (the
    feed shaft, the device's own fire-safety reading) and `power_pct`
    (actual output). Firmware names may contain dots
    (`regulation.fixed_power` is a valid key segment). `power_level` is an
    int when the wire float is integral, so it matches the enum.

    `on` is derived from the run state: the burner is on unless `state` is
    in OFF_STATES, so an unknown code reads as on. Start and stop are
    momentary writes in this dialect (misc.start / misc.stop), so `on` is
    always the device's own readback and not an echo of a command.
    """
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
    """Return the {base}/set payload for a validated command, or None.

    None when the value is not one this aspect takes: a bool for `on`, or
    one of POWER_LEVELS (an int, not a bool) for `power_level`. `on` sends
    {"path": "misc.start" | "misc.stop", "value": "1"}, the bridge's own
    switch shape. The caller drops a None with `invalid-command`.
    """
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
    """Return the bound entity and the topic's segments past its base-topic prefix.

    (None, None) when the topic is under no bound entity's base topic.
    """
    for entity in entities:
        prefix = f"{entity.id}/"
        if topic.startswith(prefix):
            return entity, topic[len(prefix) :].split("/")
    return None, None


class Stoves:
    """The stove-to-bus direction: status and operating documents in, aspects out.

    It publishes on change, tracks each stove's availability from when it
    last spoke, and keeps discovery's `bound` flag: whether a stove has
    been heard at all. `session` is anything with `put_json` and
    `health_event`; the clock is injectable. paho delivers messages on one
    thread and the watchdog sweeps on another, so availability is locked.
    """

    def __init__(self, session, unit: str, entities, clock=time.monotonic):
        self.session = session
        self.unit = unit
        self.entities = list(entities)
        self.clock = clock
        self.seen: set[str] = set()
        # Publish-on-change: the last value put on the bus per (entity, aspect).
        self.last: dict[tuple[str, str], object] = {}
        self.lock = threading.Lock()
        self.last_rx = {e.id: clock() for e in self.entities}
        self.available: dict[str, bool] = {}

    def set_available(self, entity, value: bool) -> bool:
        """Publish an availability change; True when it was one."""
        with self.lock:
            if self.available.get(entity.name) == value:
                return False
            self.available[entity.name] = value
            self.session.put_json(keys.state_key(entity.room, entity.name, "available"), value)
        return True

    def inventory(self) -> list[dict]:
        """Return the discovery document: every configured stove."""
        return [
            {
                "id": e.id,
                "configured": True,
                "entity": e.name,
                "bound": e.id in self.seen,
                "suggested": {"capability": "burner", "features": ["power_level"]},
                "aspects": ASPECT_DESCRIPTOR,
            }
            for e in self.entities
        ]

    def on_message(self, topic: str, raw: bytes) -> None:
        """Translate one {id}/status or {id}/operating document.

        {base}/status is the `status *` response: its positional CSV mapped
        onto pyduro's STATUS_PARAMS names (116 fields) as one JSON object,
        with values converted to float where they parse. {base}/operating
        is one JSON object. The bridge also publishes {base}/advanced,
        {base}/settings/{group} (17 groups), {base}/consumption/{key} (9
        arrays) and {base}/logs. Those are not subscribed: settings are
        configuration, and the rest is detail nothing reads.

        A field is published only when its value changed since the last
        publish, and once on the first poll after start; late joiners read
        the core's mirror. The bridge republishes every field each poll, so
        forwarding every message would record each field 2,700 times a
        day. The status topic alone would then write two and a half times
        as much as the whole heat-pump adapter.

        Operating fields get the prefix `operating_`, which keeps the two
        NBE namespaces apart without checking one against the other. A
        field that would publish as a reserved name drops with
        `reserved-aspect`, and a field name that is not a valid key segment
        with `malformed-payload`; the rest of the document still publishes.
        """
        session = self.session
        entity, rest = route(topic, self.entities)
        if entity is None or len(rest) != 1 or rest[0] not in ("status", "operating"):
            return

        self.last_rx[entity.id] = self.clock()
        self.set_available(entity, True)
        if entity.id not in self.seen:
            self.seen.add(entity.id)
            session.put_json(keys.discovery_key(self.unit), self.inventory())

        try:
            document = json.loads(raw)
            if not isinstance(document, dict):
                raise ValueError("not an object")
        except ValueError:
            session.health_event("drop", reason="malformed-payload", topic=topic)
            return

        if rest[0] == "status":
            aspects, reserved = status_aspects(document)
            for field in reserved:
                session.health_event("drop", reason="reserved-aspect", topic=topic, field=field)
        else:
            aspects = {f"operating_{field}": value for field, value in document.items()}
        for aspect, value in aspects.items():
            try:
                key = keys.state_key(entity.room, entity.name, aspect)
            except ValueError:
                session.health_event("drop", reason="malformed-payload", topic=topic, field=aspect)
                continue
            if self.last.get((entity.name, aspect), _UNSET) == value:
                continue  # unchanged since the last poll
            self.last[(entity.name, aspect)] = value
            session.put_json(key, value)

    def sweep(self, timeout_s: float) -> None:
        """Mark a stove silent for `timeout_s` unavailable, one event per transition.

        The bridge publishes on a cadence, so silence is the loss signal.
        The bridge skips a topic's publish when the burner does not answer,
        so an unreachable burner and a dead bridge both go silent. The next
        message marks the stove available again, and the other aspects
        keep their last values.
        """
        now = self.clock()
        for entity in self.entities:
            if now - self.last_rx[entity.id] > timeout_s and self.set_available(entity, False):
                self.session.health_event("device-silent", topic=entity.id)


def main():
    unit = os.environ[keys.ENV_UNIT]
    config = house.load_adapter(unit)
    endpoint = mqtt.parse_endpoint(config.endpoint)

    session = homeostat.connect()
    params = Params(session, PARAM_DEFAULTS)
    stoves = Stoves(session, unit, config.entities)

    def cmd_handler(entity):
        def handler(sample):
            parsed = session.parse_command(sample)
            if parsed is None:
                return
            aspect, value, cmd_id = parsed
            body = command_body(aspect, value) if aspect in COMMANDS else None
            if body is None:
                session.health_event(
                    "drop",
                    reason="invalid-command",
                    key=str(sample.key_expr),
                    aspect=aspect,
                    value=value,
                    cmd_id=cmd_id,
                )
                return
            client.publish(f"{entity.id}/set", json.dumps(body))

        return handler

    topics = [
        (topic, 0)
        for e in config.entities
        for topic in (f"{e.id}/status", f"{e.id}/operating")
    ]
    client = mqtt.connect(
        endpoint,
        lambda _client, _userdata, msg: stoves.on_message(msg.topic, msg.payload),
        topics,
        health=session.health_event,
    )

    subscribers = [
        session.subscribe(expr, cmd_handler(e))
        for e in config.entities
        for expr in keys.command_keyexprs(e)
    ]

    session.put_json(keys.discovery_key(unit), stoves.inventory())

    stop = threading.Event()

    def watchdog():
        while True:
            timeout = params.availability_timeout_s
            if stop.wait(min(1.0, timeout / 4)):
                return
            stoves.sweep(timeout)

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
