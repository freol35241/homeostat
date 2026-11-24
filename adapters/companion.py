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
"""Companion adapter: the house's own Android app, over the MQTT broker.

The wire contract is docs/companion-protocol.md. It specifies every topic
and payload, and what the app owes about sessions, retention and wills.
It is normative, and homeostat-app implements against it.
docs/design.md#the-companion-app holds the reasoning.

This adapter makes a family phone a described part of the house. The
phone never touches the bus. It is a device, and devices speak a dialect
to an adapter (docs/design.md#bus). This dialect is ours, so the app
already speaks homeostat vocabulary and there is nothing to rename. The
adapter adds the ownership, the grants and the plan visibility that a
phone publishing for itself would not have.

Each phone owns a topic subtree under the endpoint's base topic
(`companion` unless the endpoint path names another) and two entities:

    {base}/{phone}/available          <- retained birth `true`, LWT `false`
    {base}/{phone}/person/presence    <- retained bare JSON bool
    {base}/{phone}/person/position    <- {"lat","lon","accuracy","battery","fixed_at"}
    {base}/{phone}/notifier/ack       <- the acknowledged alert's `sent_at`
    {base}/{phone}/notifier/message   -> {"text","actor","sent_at"}, QoS 1
    {base}/{phone}/notifier/alert     -> the same, the urgent channel

The entity file's `id` is the two topic segments that name the subtree and
the face: "alice/person" binds the phone's `tracker` entity (room
`person`, the reserved pseudo-room), "alice/notifier" the `notifier`
entity for the same phone. One phone is therefore two files, which is
what the two capabilities are; they share the first segment and that is
how `available` reaches both. The wire keeps the face name `person`; the
human is a `person` entity the persons automation computes from this
tracker (docs/design.md#map-and-people).

Published state:

- `home` (bool) on the tracker, from the platform's geofence transitions,
  which the wire calls `presence`. The app registers a single geofence,
  `home`. Continuous position is opt-in per phone and off by default, so a
  phone that never publishes `position` is the normal case.
- `lat`, `lon`, `fixed_at` and, when the fix carries them, `accuracy` and
  `battery`: scalar aspects, never one composite fix, so the recorder
  gives trails for free. `fixed_at` is published last, because it
  completes a fix for the persons automation. A fix without one is
  stamped with its arrival time.
- `delivered` (epoch seconds) on the notifier when the broker acks the
  publish (QoS 1 PUBACK). ntfy's `delivered` means the same: the delivery
  path took the message, and nobody has necessarily seen it.
- `acknowledged` (epoch seconds): the `sent_at` of the newest alert the
  person has dismissed or opened in the app, echoed back by the app. It
  is this adapter's clock, so an automation that sent an alert at `t`
  reads `acknowledged >= t` as seen. This is the far-end receipt only an
  app can give; whether an unacknowledged `alert` escalates is the
  automation's policy over this aspect, never this adapter's.
- `available` (bool) on both of a phone's entities, from the app's
  retained birth message and its last will. A phone out of coverage is
  unreachable, and an automation that escalates should be able to see it.
  Stale, not false: every other aspect keeps its last value.

Commands: the notifier's two commandable aspects, `message` and `alert`.
Each takes a non-empty string. The QoS, retain flag and queueing are in
docs/companion-protocol.md. The envelope's `actor` travels as `actor`, so
the phone can say who spoke. There is no rate floor, because there is no
third-party server to protect as there is with ntfy. A cooldown is house
policy and lives in the automation.

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

# Optional position fields, homeostat names already. `fixed_at` follows
# them, last.
POSITION_OPTIONAL = ("accuracy", "battery")

TRACKER_DESCRIPTOR = {
    "schema": 1,
    "groups": ["presence"],
    "fields": {
        "home": {"label": "inside the home geofence (home)", "kind": "boolean", "group": "presence"},
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
    """Return the value if it is a JSON number, else None; bools are not."""
    if isinstance(value, bool) or not isinstance(value, (int, float)):
        return None
    return value


class Phones:
    """The phones' side of the bus: one `on_message` per MQTT message.

    It turns a phone's `available`, `presence`, `position` and `ack`
    under `base` into its entities' aspects. `put` and `drop` are shared
    with the command direction in `main`, which publishes `delivered`
    and reports refused commands through them. `session` is anything with
    `put_json` and `health_event`.
    """

    def __init__(self, session, base: str, entities):
        self.session = session
        self.base = base
        self.by_id = {e.id: e for e in entities}
        # A phone's entities, for the one topic that addresses the phone
        # itself rather than one of its faces.
        self.by_phone: dict[str, list] = {}
        for entity in entities:
            self.by_phone.setdefault(entity.id.split("/")[0], []).append(entity)
        self.seen_unknown: set[str] = set()
        self.available: dict[str, bool] = {}
        self.acknowledged: dict[str, float] = {}

    def drop(self, reason, **fields):
        self.session.health_event("drop", reason=reason, **fields)

    def put(self, entity, aspect, value):
        self.session.put_json(keys.state_key(entity.room, entity.name, aspect), value)

    def set_available(self, entity, value: bool) -> None:
        # On transition only (docs/adapters.md §7).
        if self.available.get(entity.name) == value:
            return
        self.available[entity.name] = value
        self.put(entity, "available", value)

    def on_message(self, topic: str, raw: bytes) -> None:
        """Translate one message from a phone's subtree onto the bus."""
        rest = topic[len(self.base) + 1 :].split("/")
        phone, leaf = rest[0], rest[-1]
        entity_id = "/".join(rest[:-1])
        try:
            payload = json.loads(raw)
        except ValueError:
            self.drop("malformed-payload", topic=topic)
            return

        if leaf == "available":
            entities = self.by_phone.get(phone)
            if not entities:
                if phone not in self.seen_unknown:
                    self.seen_unknown.add(phone)
                    self.drop("unknown-device", topic=topic)
                return
            if not isinstance(payload, bool):
                self.drop("malformed-payload", topic=topic)
                return
            for entity in entities:
                self.set_available(entity, payload)
            return

        entity = self.by_id.get(entity_id)
        if entity is None:
            # Once per subtree: a phone the owner has not bound yet keeps
            # publishing, and repeating the event per fix would bury the
            # health feed.
            if entity_id not in self.seen_unknown:
                self.seen_unknown.add(entity_id)
                self.drop("unknown-device", topic=topic)
            return

        if leaf == "presence":
            if not isinstance(payload, bool):
                self.drop("malformed-payload", topic=topic)
                return
            self.put(entity, "home", payload)
        elif leaf == "position":
            lat = _number(payload.get("lat")) if isinstance(payload, dict) else None
            lon = _number(payload.get("lon")) if isinstance(payload, dict) else None
            if lat is None or lon is None:
                self.drop("malformed-payload", topic=topic)
                return
            self.put(entity, "lat", lat)
            self.put(entity, "lon", lon)
            for field in POSITION_OPTIONAL:
                value = _number(payload.get(field))
                if value is not None:
                    self.put(entity, field, value)
            fixed_at = _number(payload.get("fixed_at"))
            self.put(entity, "fixed_at", fixed_at if fixed_at is not None else time.time())
        elif leaf == "ack":
            sent_at = _number(payload)
            if sent_at is None:
                self.drop("malformed-payload", topic=topic)
                return
            # The person may open an older alert after a newer one, and
            # QoS 1 may repeat an ack, so `acknowledged` only moves forward.
            if sent_at > self.acknowledged.get(entity.name, 0):
                self.acknowledged[entity.name] = sent_at
                self.put(entity, "acknowledged", sent_at)


def main():
    unit = os.environ[keys.ENV_UNIT]
    config = house.load_adapter(unit)
    endpoint = mqtt.parse_endpoint(config.endpoint)
    base = mqtt.base_topic(endpoint, DEFAULT_BASE_TOPIC)

    session = homeostat.connect()
    phones = Phones(session, base, config.entities)

    # PUBACK arrives on the network thread; the mid is the only handle
    # paho gives back, so the notifier waits for it there.
    pending: dict[int, object] = {}
    lock = threading.Lock()

    def on_publish(client, userdata, mid, reason_code=None, properties=None):
        with lock:
            entity = pending.pop(mid, None)
        if entity is not None:
            phones.put(entity, "delivered", time.time())

    topics = [
        (f"{base}/+/available", 1),
        (f"{base}/+/person/presence", 1),
        (f"{base}/+/person/position", 1),
        (f"{base}/+/notifier/ack", 1),
    ]
    client = mqtt.connect(
        endpoint,
        lambda _client, _userdata, msg: phones.on_message(msg.topic, msg.payload),
        topics,
        health=session.health_event,
    )
    client.on_publish = on_publish

    def cmd_handler(entity):
        def handler(sample):
            key = str(sample.key_expr)
            aspect = key.split("/", 4)[4]
            try:
                payload = json.loads(sample.payload.to_bytes())
            except ValueError:
                phones.drop("malformed-payload", key=key)
                return
            # The id ends a command someone is waiting on, so every drop
            # below carries it.
            cmd_id = keys.cmd_envelope_id(payload)
            try:
                value = keys.parse_cmd_envelope(payload)
            except ValueError:
                phones.drop("invalid-command", key=key, cmd_id=cmd_id)
                return
            # A report or a release is for the arbiter, and nothing
            # arbitrates a notifier, so one reaching here was sent in error.
            stray = keys.cmd_envelope_hold(payload) in ("report", "release")
            if stray or aspect not in CHANNELS or not isinstance(value, str) or not value.strip():
                phones.drop("invalid-command", key=key, aspect=aspect, value=value, cmd_id=cmd_id)
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
                "aspects": NOTIFIER_DESCRIPTOR if e.capability == "notifier" else TRACKER_DESCRIPTOR,
            }
            for e in config.entities
        ],
    )

    session.ready()

    homeostat.wait_for_shutdown()

    # The MQTT loop stops first: an in-flight callback during teardown
    # would otherwise put on a closed zenoh session.
    client.loop_stop()
    client.disconnect()
    for sub in subscribers:
        sub.undeclare()
    session.close()


if __name__ == "__main__":
    main()
