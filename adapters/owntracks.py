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
"""OwnTracks adapter: a translating subscriber, same shape as Zigbee2MQTT.

Phones reach the house over MQTT through the existing broker, not
OwnTracks' HTTP mode: the broker's retained message is each phone's
last-known position across restarts, and the house gains no second
ingress surface. The topic prefix is fixed at `owntracks`.

Phone location published as JSON on owntracks/{user}/{device} fans out to
per-aspect keys home/state/person/{entity}/{aspect}: lat, lon, accuracy
(from `acc`), battery (from `batt`) and fixed_at (from `tst`) — scalar
aspects, not one composite fix, so the recorder gives position trails for
free (docs/design.md#map-and-people). accuracy/battery/fixed_at
are omitted when the fix does not carry them. The entity file's `id` is
the two OwnTracks topic segments ("{user}/{device}"); the file stem is the
entity name; person entities bind capability = "person", room = "person"
(the reserved pseudo-room — persons move, so they are never a physical
room). Non-location `_type` payloads (transition, lwt, waypoint, cmd, ...)
are normal OwnTracks traffic and are ignored. Persons are read-only: there
is no command subscription. Anything unusable — malformed JSON, or a
location payload missing lat/lon — drops with "malformed-payload" at
home/health/{unit}/event instead of crashing. A location from an unbound
pair drops with "unknown-device" on first sight only: discovery already
carries it, and a phone nobody has bound yet publishes forever.

There is no `available` aspect. A retained position has no liveness
semantics, so the protocol offers no real loss signal and the adapter
does not fake one; an old fix shows its age through fixed_at.

Unlike z2m there is no retained bridge inventory to mirror: every
user/device pair seen on the broker, bound or not, is tracked incrementally
as traffic arrives and the complete inventory is republished at
home/discovery/{unit} when a new device appears — coalesced to at most one
publish per DISCOVERY_COALESCE_S, and holding only the MAX_UNBOUND most
recently first-seen unbound pairs (the oldest evicted), since the pairs
are whatever the broker carries — each record carrying the entity-file
binding `id`, whether an entity file already binds it, and a suggested
capability/features stanza (capability "person", features []).
"""

import json
import os
import threading

import homeostat
from homeostat import house, keys, mqtt

BASE_TOPIC = "owntracks"
MAX_UNBOUND = 200
DISCOVERY_COALESCE_S = 5.0
# OwnTracks location fields -> the person's aspects, in publish order.
FIELDS = (("lat", "lat"), ("lon", "lon"), ("acc", "accuracy"), ("batt", "battery"), ("tst", "fixed_at"))


class Tracker:
    """The OwnTracks-to-bus direction: one `on_message` per MQTT message.

    It keeps the discovery inventory, every device seen in first-seen
    order with at most MAX_UNBOUND unbound ones, and calls `on_new_device`
    (outside its lock) on each first sight so the caller can schedule a
    discovery publish. `session` is anything with `put_json` and
    `health_event`.
    """

    def __init__(self, session, by_id: dict, on_new_device):
        self.session = session
        self.by_id = by_id
        self.on_new_device = on_new_device
        self.lock = threading.Lock()
        self.inventory: dict[str, dict] = {}

    def records(self) -> list[dict]:
        """Return the discovery document as it stands."""
        with self.lock:
            return list(self.inventory.values())

    def on_message(self, topic: str, raw: bytes) -> None:
        """Translate one owntracks/{user}/{device} message onto the bus."""
        session = self.session
        _, user, device = topic.split("/")
        dev_id = f"{user}/{device}"
        with self.lock:
            first_sight = dev_id not in self.inventory
            if first_sight:
                entity = self.by_id.get(dev_id)
                self.inventory[dev_id] = {
                    "id": dev_id,
                    "configured": entity is not None,
                    "entity": entity.name if entity else None,
                    "suggested": {"capability": "person", "features": []},
                }
                unbound = [k for k, r in self.inventory.items() if not r["configured"]]
                if len(unbound) > MAX_UNBOUND:
                    del self.inventory[unbound[0]]
        if first_sight:
            self.on_new_device()

        try:
            payload = json.loads(raw)
        except ValueError:
            payload = None
        if not isinstance(payload, dict):
            session.health_event("drop", reason="malformed-payload", topic=topic)
            return
        if payload.get("_type") != "location":
            return  # transition, lwt, waypoint, cmd, ... — normal OwnTracks traffic
        if "lat" not in payload or "lon" not in payload:
            session.health_event("drop", reason="malformed-payload", topic=topic)
            return

        entity = self.by_id.get(dev_id)
        if entity is None:
            # Once, on first sight. A phone nobody has bound yet keeps
            # publishing forever, and discovery already carries it with
            # configured=false — repeating the event per fix would bury
            # the health feed during exactly the discovery-first pass the
            # design asks for.
            if first_sight:
                session.health_event("drop", reason="unknown-device", topic=topic)
            return

        for field, aspect in FIELDS:
            if field in payload:
                session.put_json(keys.state_key(entity.room, entity.name, aspect), payload[field])


def main():
    unit = os.environ[keys.ENV_UNIT]
    config = house.load_adapter(unit)
    by_id = {e.id: e for e in config.entities}

    endpoint = mqtt.parse_endpoint(config.endpoint)

    session = homeostat.connect()
    # New devices are published together: one timer pending at a time.
    timer_lock = threading.Lock()
    timer: list[threading.Timer | None] = [None]

    def publish_discovery() -> None:
        with timer_lock:
            timer[0] = None
        session.put_json(keys.discovery_key(unit), tracker.records())

    def schedule_discovery() -> None:
        with timer_lock:
            if timer[0] is None:
                timer[0] = threading.Timer(DISCOVERY_COALESCE_S, publish_discovery)
                timer[0].daemon = True
                timer[0].start()

    tracker = Tracker(session, by_id, schedule_discovery)

    client = mqtt.connect(
        endpoint,
        lambda _client, _userdata, msg: tracker.on_message(msg.topic, msg.payload),
        f"{BASE_TOPIC}/+/+",
    )

    # Persons are read-only: no command subscription, unlike z2m's lights.
    session.ready()

    mqtt.wait_for_shutdown()

    # The MQTT loop stops first: an in-flight on_message during teardown
    # would otherwise put on a closed zenoh session.
    client.loop_stop()
    client.disconnect()
    with timer_lock:
        if timer[0] is not None:
            timer[0].cancel()
    session.close()


if __name__ == "__main__":
    main()
