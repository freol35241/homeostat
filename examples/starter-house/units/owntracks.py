# /// script
# requires-python = ">=3.11"
# dependencies = [
#     "homeostat==0.18.0",
#     "paho-mqtt>=2,<3",
# ]
# ///
"""OwnTracks adapter: phone locations on the bus from OwnTracks over MQTT.

Phones publish to the house's existing MQTT broker, not to OwnTracks'
HTTP mode. The broker's retained message keeps each phone's last
position across restarts, and the house gains no second way in. The
topic prefix is fixed at `owntracks`.

Binding: an entity file's `id` is the two OwnTracks topic segments,
`{user}/{device}`, and the file stem is the entity name. Person entities
bind capability `person` in the reserved pseudo-room `person`, since
people move between rooms.

State: a location on owntracks/{user}/{device} fans out to
home/state/person/{entity}/{aspect} as lat, lon, accuracy (from `acc`),
battery (from `batt`) and fixed_at (from `tst`) (`Tracker.on_message`).
These are separate scalar aspects so the recorder keeps position trails
(docs/design.md#map-and-people). accuracy, battery and fixed_at are
omitted when the fix lacks them. Persons are read-only: there is no
command subscription.

There is no `available` aspect. A retained position says nothing about
whether the phone is alive, so there is no real loss signal; an old fix
shows its age through fixed_at.

Discovery: every user/device pair seen on the broker, bound or not, with
a suggested `person` stanza. There is no retained inventory to mirror,
so pairs are added as traffic arrives. The document is republished at
most once per DISCOVERY_COALESCE_S, and only the MAX_UNBOUND most
recently first-seen unbound pairs are kept, since the pairs are whatever
the broker carries.

Health events: `drop` with malformed-payload (bad JSON, or a location
without lat/lon) or unknown-device (a location from an unbound pair,
first sight only).
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
            return  # transition, lwt, waypoint, cmd, ...: normal OwnTracks traffic
        if "lat" not in payload or "lon" not in payload:
            session.health_event("drop", reason="malformed-payload", topic=topic)
            return

        entity = self.by_id.get(dev_id)
        if entity is None:
            # Once, on first sight. A phone nobody has bound keeps
            # publishing, and discovery already carries it with
            # configured=false. An event per fix would bury the health feed
            # while devices are being discovered and bound.
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

    # Persons are read-only, so there is no command subscription.
    session.ready()

    mqtt.wait_for_shutdown()

    # Stop the MQTT loop first, or an in-flight on_message could put on
    # a closed zenoh session.
    client.loop_stop()
    client.disconnect()
    with timer_lock:
        if timer[0] is not None:
            timer[0].cancel()
    session.close()


if __name__ == "__main__":
    main()
