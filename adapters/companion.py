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
"""Companion adapter: the house's own Android app, over the MQTT broker
(docs/design.md, Notifications; issue #51).

The membrane that makes a family phone a described part of the house. The
phone never touches the bus — it is a device, and devices speak a dialect
to an adapter (docs/design.md, Bus). Unlike every other dialect this one
is ours, so the app already speaks homeostat vocabulary and there is
nothing to rename; what this adapter provides is the ownership, the
grants and the plan visibility that a phone publishing for itself would
not have.

Each phone owns a topic subtree under the endpoint's base topic
(`companion` unless the endpoint path names another) and two entities:

    {base}/{phone}/available          <- retained birth `true`, LWT `false`
    {base}/{phone}/person/presence    <- bare JSON bool
    {base}/{phone}/person/position    <- {"lat","lon","accuracy","battery","fixed_at"}
    {base}/{phone}/notifier/ack       <- bare JSON epoch seconds
    {base}/{phone}/notifier/message   -> {"text","actor","sent_at"}, QoS 1
    {base}/{phone}/notifier/alert     -> the same, the urgent channel

The entity file's `id` is the two topic segments that name the subtree and
the face: "alice/person" binds the `person` entity (room `person`, the
reserved pseudo-room), "alice/notifier" the `notifier` entity for the same
phone. One phone is therefore two files, which is what the two
capabilities are; they share the first segment and that is how `available`
reaches both.

Published state:

- `presence` (bool) on the person, from the platform's geofence
  transitions. The app registers ONE geofence, `home`; continuous position
  is opt-in per phone and off by default (#51), so a phone that never
  publishes `position` is the normal case, not a degraded one.
- `lat`, `lon` and — when the fix carries them — `accuracy`, `battery`,
  `fixed_at`: scalar aspects, never one composite fix, so the recorder
  gives trails for free.
- `delivered` (epoch seconds) on the notifier when the BROKER acks the
  publish (QoS 1 PUBACK), which is what ntfy's `delivered` also means: the
  delivery path took it, not that anyone saw it.
- `acknowledged` (epoch seconds) when the person dismissed the
  notification in the app. This is the far-end receipt the notifications
  settlement said only an app could give; whether an unacknowledged
  `alert` escalates is the automation's policy over this aspect, never
  this adapter's.
- `available` (bool) on both of a phone's entities, from the app's
  retained birth message and its last will. A phone out of coverage is
  unreachable, and an automation that escalates should be able to see it.
  Stale, not false: every other aspect keeps its last value.

Commands — the notifier's two commandable aspects, each a non-empty
string, published at QoS 1 and NOT retained (a retained alert would fire
again on every reconnect; the queueing is the app's persistent session,
which is the whole reason the broker carries this instead of ntfy). The
envelope's `actor` travels as `actor` so the phone can say who spoke.
There is no rate floor: unlike ntfy there is no third-party server to
protect, and the cooldown that is house policy lives in the automation.

Health events: drop/malformed-payload (undecodable or wrongly typed
input from the phone), drop/invalid-command (bad envelope, unknown
aspect, non-string or empty text), drop/unknown-device (a subtree no
entity file binds, first sight only). Discovery is the static
one-record-per-entity document with the aspect descriptor.
"""

import json
import os
import threading
import time

import homeostat
from homeostat import house, keys, mqtt

DEFAULT_BASE_TOPIC = "companion"

# Commandable aspect -> the leaf it publishes on. Both channels are one
# topic each: the severity split is the app's notification channel, as it
# is ntfy's priority.
CHANNELS = {"message": "message", "alert": "alert"}

# Optional position fields, homeostat names already.
POSITION_OPTIONAL = ("accuracy", "battery", "fixed_at")

PERSON_DESCRIPTOR = {
    "schema": 1,
    "groups": ["presence"],
    "fields": {
        "presence": {"label": "home", "kind": "boolean", "group": "presence"},
    },
}

NOTIFIER_DESCRIPTOR = {
    "schema": 1,
    "groups": ["delivery"],
    "fields": {
        "delivered": {"label": "last delivered (epoch s)", "kind": "number", "group": "delivery"},
        "acknowledged": {"label": "last seen (epoch s)", "kind": "number", "group": "delivery"},
    },
}


def _number(value):
    """The value if it is a JSON number, else None — bools are not."""
    if isinstance(value, bool) or not isinstance(value, (int, float)):
        return None
    return value


def main():
    unit = os.environ[keys.ENV_UNIT]
    config = house.load_adapter(unit)
    endpoint = mqtt.parse_endpoint(config.endpoint)
    base = mqtt.base_topic(endpoint, DEFAULT_BASE_TOPIC)

    by_id = {e.id: e for e in config.entities}
    # A phone's entities, for the one topic that addresses the phone
    # itself rather than one of its faces.
    by_phone: dict[str, list] = {}
    for entity in config.entities:
        by_phone.setdefault(entity.id.split("/")[0], []).append(entity)

    session = homeostat.connect()

    seen_unknown: set[str] = set()
    available: dict[str, bool] = {}
    # PUBACK arrives on the network thread; the mid is the only handle
    # paho gives back, so the notifier waits for it there.
    pending: dict[int, object] = {}
    lock = threading.Lock()

    def drop(reason, **fields):
        session.health_event("drop", reason=reason, **fields)

    def put(entity, aspect, value):
        session.put_json(keys.state_key(entity.room, entity.name, aspect), value)

    def set_available(entity, value: bool) -> None:
        # On transition only (docs/adapters.md §7).
        if available.get(entity.name) == value:
            return
        available[entity.name] = value
        put(entity, "available", value)

    def on_phone_message(client, userdata, msg):
        rest = msg.topic[len(base) + 1 :].split("/")
        phone, leaf = rest[0], rest[-1]
        entity_id = "/".join(rest[:-1])
        try:
            payload = json.loads(msg.payload)
        except ValueError:
            drop("malformed-payload", topic=msg.topic)
            return

        if leaf == "available":
            entities = by_phone.get(phone)
            if not entities:
                if phone not in seen_unknown:
                    seen_unknown.add(phone)
                    drop("unknown-device", topic=msg.topic)
                return
            if not isinstance(payload, bool):
                drop("malformed-payload", topic=msg.topic)
                return
            for entity in entities:
                set_available(entity, payload)
            return

        entity = by_id.get(entity_id)
        if entity is None:
            # Once per subtree: a phone the owner has not bound yet keeps
            # publishing, and repeating the event per fix would bury the
            # health feed.
            if entity_id not in seen_unknown:
                seen_unknown.add(entity_id)
                drop("unknown-device", topic=msg.topic)
            return

        if leaf == "presence":
            if not isinstance(payload, bool):
                drop("malformed-payload", topic=msg.topic)
                return
            put(entity, "presence", payload)
        elif leaf == "position":
            lat = _number(payload.get("lat")) if isinstance(payload, dict) else None
            lon = _number(payload.get("lon")) if isinstance(payload, dict) else None
            if lat is None or lon is None:
                drop("malformed-payload", topic=msg.topic)
                return
            put(entity, "lat", lat)
            put(entity, "lon", lon)
            for field in POSITION_OPTIONAL:
                value = _number(payload.get(field))
                if value is not None:
                    put(entity, field, value)
        elif leaf == "ack":
            at = _number(payload)
            if at is None:
                drop("malformed-payload", topic=msg.topic)
                return
            put(entity, "acknowledged", at)

    def on_publish(client, userdata, mid, reason_code=None, properties=None):
        with lock:
            entity = pending.pop(mid, None)
        if entity is not None:
            put(entity, "delivered", time.time())

    topics = [
        (f"{base}/+/available", 1),
        (f"{base}/+/person/presence", 1),
        (f"{base}/+/person/position", 1),
        (f"{base}/+/notifier/ack", 1),
    ]
    client = mqtt.connect(endpoint, on_phone_message, topics)
    client.on_publish = on_publish

    def cmd_handler(entity):
        def handler(sample):
            key = str(sample.key_expr)
            aspect = key.split("/", 4)[4]
            try:
                payload = json.loads(sample.payload.to_bytes())
            except ValueError:
                drop("malformed-payload", key=key)
                return
            try:
                value = keys.parse_cmd_envelope(payload)
            except ValueError:
                drop("invalid-command", key=key)
                return
            if aspect not in CHANNELS or not isinstance(value, str) or not value.strip():
                drop("invalid-command", key=key, aspect=aspect, value=value)
                return
            actor = payload.get("actor")
            body = json.dumps(
                {
                    "text": value,
                    "actor": actor if isinstance(actor, str) and actor else unit,
                    "sent_at": time.time(),
                }
            )
            phone = entity.id.split("/")[0]
            with lock:
                info = client.publish(f"{base}/{phone}/notifier/{CHANNELS[aspect]}", body, qos=1)
                pending[info.mid] = entity

        return handler

    subscribers = [
        session.subscribe(expr, cmd_handler(e))
        for e in config.entities
        if e.capability == "notifier"
        for expr in keys.command_keyexprs(e)
    ]

    session.put_json(
        keys.discovery_key(unit),
        [
            {
                "id": e.id,
                "configured": True,
                "entity": e.name,
                "suggested": {
                    "capability": e.capability,
                    "features": ["alert", "acknowledged"] if e.capability == "notifier" else [],
                },
                "aspects": NOTIFIER_DESCRIPTOR if e.capability == "notifier" else PERSON_DESCRIPTOR,
            }
            for e in config.entities
        ],
    )

    session.ready()

    mqtt.wait_for_shutdown()

    # The MQTT loop stops first: an in-flight callback during teardown
    # would otherwise put on a closed zenoh session.
    client.loop_stop()
    client.disconnect()
    for sub in subscribers:
        sub.undeclare()
    session.close()


if __name__ == "__main__":
    main()
