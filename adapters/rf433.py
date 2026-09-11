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
"""RF433 adapter: one-way senders behind an OpenMQTTGateway bridge.

A sub-GHz PIR, door contact or smoke detector transmits when something
happens and never transmits again — there is no "clear". The bridge
(OpenMQTTGateway on a Sonoff RF Bridge, or any gateway publishing the same
shape) republishes each burst on ONE topic, {base}/SRFBtoMQTT, and the only
thing distinguishing one device from another is the decimal code in the
payload. So the entity file's `id` is that code, and the file stem is the
entity name — the same addressing every adapter uses, with the device's
"address" being what it transmits.

THE DECAY IS THIS ADAPTER'S (docs/design.md, One-way senders). The radio's
lack of an off is a protocol fact, so synthesizing one is dialect knowledge
and belongs in the membrane; it is never a core TTL. Three rules:

  1. TRANSITIONS ONLY. `true` on the first assertion, `false` when the hold
     expires. A repeat burst inside the hold extends the deadline and
     publishes nothing — 433 MHz senders repeat each burst several times by
     design, so publishing per burst would be a per-motion flood.
  2. `false` FOR EVERY BOUND ENTITY AT STARTUP. A held `true` is this
     adapter's own construct, not a device reading, so after a restart
     "nothing has asserted within the hold" is the honest state rather than
     an invented one. It is also what stops a crash-looping adapter from
     leaving a motion sensor stuck on: every restart self-clears.
  3. THE HOLD IS A PARAMETER, PER ASPECT, not per entity. A contact, a PIR
     and a smoke detector want different holds (seconds, a minute, ten
     minutes) but every contact wants the same one, so the entity's
     capability and features pick the parameter and the house tunes three
     numbers rather than one per device.

Capabilities: a PIR binds `presence` and publishes `occupancy`; a contact
or a detector binds `binary_sensor` and publishes the aspect its `features`
name (`contact`, `smoke`, ...), because the radio cannot say what it is and
the entity file is where that knowledge lives. A smoke detector's field
carries `notable` so a detector firing reaches Now as a deviation.

Availability is the bridge's own LWT, not a receive timer: silence from a
433 MHz sender is its normal state and says nothing about the gateway.

⚠️ TWO PAYLOAD SHAPES, ONE OF THEM UNTESTED. Older gateway firmware
publishes a bare decimal string ("13951014"); current firmware publishes
JSON ({"raw": ..., "value": 13951014, "delay": ...}). Both are accepted,
the JSON path reading `value`. The bare-decimal path is the one exercised
against real hardware (a bridge reporting version 0.5); THE JSON PATH IS
WRITTEN TO THE DOCUMENTED SHAPE AND HAS NOT BEEN SEEN ON A WIRE — if you
run current firmware, that is the path worth confirming.
"""

import json
import os
import threading
import time

import homeostat
from homeostat import house, keys, mqtt
from homeostat.params import LiveParams

DEFAULT_BASE = "home/OpenMQTTGateway"
EVENTS_SUFFIX = "SRFBtoMQTT"
LWT_SUFFIX = "LWT"
ONLINE = "online"

PARAM_DEFAULTS = {
    "occupancy_hold_s": 15.0,
    "contact_hold_s": 60.0,
    "smoke_hold_s": 600.0,
    "hold_s": 60.0,
}

# How often the sweeper looks for expired holds. Not a tuning knob: it only
# bounds how late a `false` can be, and the shortest sensible hold is
# seconds.
TICK_S = 0.25

# Aspect -> the parameter that holds it. Anything else falls back to
# `hold_s`, so a sensor class nobody anticipated still decays.
HOLD_PARAM = {
    "occupancy": "occupancy_hold_s",
    "presence": "occupancy_hold_s",
    "contact": "contact_hold_s",
    "smoke": "smoke_hold_s",
}

# Aspects worth surfacing on Now when they go true (docs/design.md, Aspect
# descriptors: notable). A detector firing is a deviation; a door opening is
# not.
NOTABLE = frozenset({"smoke", "gas", "carbon_monoxide", "water_leak"})


class Params(LiveParams):
    """The holds, from home/config/{unit}/*, live."""

    def hold_for(self, aspect: str) -> float:
        return max(0.1, float(self.get(HOLD_PARAM.get(aspect, "hold_s"))))


def aspect_for(entity) -> str:
    """The aspect a bound entity publishes.

    `presence` has one in the vocabulary; `binary_sensor` is "a boolean
    under its native name" and the radio cannot say which name, so the
    entity's first feature is it.
    """
    if entity.capability == "presence":
        return entity.features[0] if entity.features else "occupancy"
    return entity.features[0] if entity.features else None


def code_from(payload: bytes):
    """The decimal code in a bridge payload, as a string, or None.

    Two firmware generations: a bare decimal, and JSON carrying `value`.
    Normalized through int so "13951014", " 13951014 " and 13951014 are one
    code — the entity file writes it one way and the wire may not.
    """
    text = payload.decode("utf-8", "replace").strip()
    if not text:
        return None
    if text[0] in "{[":
        try:
            document = json.loads(text)
        except ValueError:
            return None
        if not isinstance(document, dict):
            return None
        value = document.get("value")
    else:
        value = text
    try:
        return str(int(value))
    except (TypeError, ValueError):
        return None


def main():
    unit = os.environ[keys.ENV_UNIT]
    config = house.load_adapter(unit)
    endpoint = mqtt.parse_endpoint(config.endpoint)
    base = mqtt.base_topic(endpoint, DEFAULT_BASE)

    session = homeostat.connect()
    params = Params(session, PARAM_DEFAULTS)

    by_code = {e.id: e for e in config.entities}
    aspects = {e.name: aspect_for(e) for e in config.entities}
    sighted: set[str] = set()

    lock = threading.Lock()
    deadline: dict[str, float] = {}  # entity name -> monotonic expiry

    def publish(entity, value: bool) -> None:
        session.put_json(keys.state_key(entity.room, entity.name, aspects[entity.name]), value)

    def descriptor(entity) -> dict:
        aspect = aspects[entity.name]
        field = {"label": aspect.replace("_", " "), "kind": "boolean", "group": "readings"}
        if aspect in NOTABLE:
            field["notable"] = True
        return {"schema": 1, "groups": ["readings"], "fields": {aspect: field}}

    def inventory() -> list[dict]:
        """Bound entities, then codes heard but bound to nothing.

        Bound entities are listed whether or not they have ever
        transmitted, because their descriptors are what the dashboard
        renders from and a door that nobody opened today still has a card.
        `bound` says whether this adapter has actually heard the code.

        Unbound codes are the other half, and they are the normal state of
        a 433 MHz estate -- neighbours' remotes, a car key, the doorbell.
        Discovery is how a device gets identified at all: press it, watch
        the code appear, write the entity file. Their suggestion is
        deliberately weak, because the radio says nothing about what
        transmitted.
        """
        records = []
        for entity in config.entities:
            record = {
                "id": entity.id,
                "configured": True,
                "entity": entity.name,
                "bound": entity.id in sighted,
                "suggested": {
                    "capability": entity.capability,
                    "features": list(entity.features),
                },
            }
            if aspects[entity.name] is not None:
                record["aspects"] = descriptor(entity)
            records.append(record)
        for code in sorted(sighted - set(by_code)):
            records.append({
                "id": code,
                "configured": False,
                "entity": None,
                "bound": True,
                "suggested": {"capability": "binary_sensor", "features": []},
            })
        return records

    def note(code: str) -> None:
        """Republish discovery when a code is heard for the first time."""
        if code in sighted:
            return
        sighted.add(code)
        session.put_json(keys.discovery_key(unit), inventory())

    def on_message(client, userdata, msg):
        if msg.topic == f"{base}/{LWT_SUFFIX}":
            online = msg.payload.decode("utf-8", "replace").strip().lower() == ONLINE
            for entity in config.entities:
                session.put_json(keys.state_key(entity.room, entity.name, "available"), online)
            return

        code = code_from(msg.payload)
        if code is None:
            session.health_event("drop", reason="malformed-payload", topic=msg.topic)
            return

        note(code)
        entity = by_code.get(code)
        if entity is None:
            return  # an unbound code; discovery carries it, the log does not
        if aspects[entity.name] is None:
            session.health_event(
                "drop", reason="no-aspect", entity=entity.name,
                hint="a binary_sensor needs one feature naming its aspect",
            )
            return

        with lock:
            fresh = entity.name not in deadline
            deadline[entity.name] = time.monotonic() + params.hold_for(aspects[entity.name])
        if fresh:
            publish(entity, True)   # transitions only: a repeat burst just extends

    def sweeper():
        while not stop.wait(TICK_S):
            now = time.monotonic()
            with lock:
                expired = [name for name, when in deadline.items() if when <= now]
                for name in expired:
                    del deadline[name]
            for name in expired:
                publish(by_name[name], False)

    by_name = {e.name: e for e in config.entities}
    stop = threading.Event()

    client = mqtt.connect(
        endpoint, on_message,
        [(f"{base}/{EVENTS_SUFFIX}", 0), (f"{base}/{LWT_SUFFIX}", 0)],
    )

    for entity in config.entities:
        if aspects[entity.name] is not None:
            publish(entity, False)      # rule 2: the honest state at startup
    session.put_json(keys.discovery_key(unit), inventory())

    threading.Thread(target=sweeper, daemon=True, name="rf433-sweeper").start()

    session.ready()

    mqtt.wait_for_shutdown()

    stop.set()
    # The MQTT loop stops first: an in-flight on_message during teardown
    # would otherwise put on a closed zenoh session.
    client.loop_stop()
    client.disconnect()
    session.close()


if __name__ == "__main__":
    main()
