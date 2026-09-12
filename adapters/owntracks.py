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

Phone location published as JSON on owntracks/{user}/{device} fans out to
per-aspect keys home/state/person/{entity}/{aspect}: lat, lon, accuracy
(from `acc`), battery (from `batt`) and fixed_at (from `tst`) — scalar
aspects, not one composite fix, so the recorder gives position trails for
free (docs/design.md, Map and person entities). accuracy/battery/fixed_at
are omitted when the fix does not carry them. The entity file's `id` is
the two OwnTracks topic segments ("{user}/{device}"); the file stem is the
entity name; person entities bind capability = "person", room = "person"
(the reserved pseudo-room — persons move, so they are never a physical
room). Non-location `_type` payloads (transition, lwt, waypoint, cmd, ...)
are normal OwnTracks traffic and are ignored. Persons are read-only: there
is no command subscription. Anything unusable — malformed JSON, or a
location payload missing lat/lon — emits a JSON event at
home/health/{unit}/event instead of crashing.

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


def main():
    unit = os.environ[keys.ENV_UNIT]
    config = house.load_adapter(unit)
    by_id = {e.id: e for e in config.entities}

    endpoint = mqtt.parse_endpoint(config.endpoint)

    session = homeostat.connect()
    inventory = {}  # dev_id -> record, in first-seen order
    lock = threading.Lock()  # inventory and the pending timer, across threads
    timer: list[threading.Timer | None] = [None]

    def publish_discovery() -> None:
        with lock:
            timer[0] = None
            records = list(inventory.values())
        session.put_json(keys.discovery_key(unit), records)

    def on_owntracks_message(client, userdata, msg):
        _, user, device = msg.topic.split("/")
        dev_id = f"{user}/{device}"
        with lock:
            first_sight = dev_id not in inventory
            if first_sight:
                entity = by_id.get(dev_id)
                inventory[dev_id] = {
                    "id": dev_id,
                    "configured": entity is not None,
                    "entity": entity.name if entity else None,
                    "suggested": {"capability": "person", "features": []},
                }
                unbound = [k for k, r in inventory.items() if not r["configured"]]
                if len(unbound) > MAX_UNBOUND:
                    del inventory[unbound[0]]
                if timer[0] is None:
                    timer[0] = threading.Timer(DISCOVERY_COALESCE_S, publish_discovery)
                    timer[0].daemon = True
                    timer[0].start()

        try:
            payload = json.loads(msg.payload)
        except ValueError:
            payload = None
        if not isinstance(payload, dict):
            session.health_event("drop", reason="malformed-payload", topic=msg.topic)
            return
        if payload.get("_type") != "location":
            return  # transition, lwt, waypoint, cmd, ... — normal OwnTracks traffic
        if "lat" not in payload or "lon" not in payload:
            session.health_event("drop", reason="malformed-payload", topic=msg.topic)
            return

        entity = by_id.get(dev_id)
        if entity is None:
            # Once, on first sight. A phone nobody has bound yet keeps
            # publishing forever, and discovery already carries it with
            # configured=false — repeating the event per fix would bury
            # the health feed during exactly the discovery-first pass the
            # design asks for.
            if first_sight:
                session.health_event("drop", reason="unknown-device", topic=msg.topic)
            return

        session.put_json(keys.state_key(entity.room, entity.name, "lat"), payload["lat"])
        session.put_json(keys.state_key(entity.room, entity.name, "lon"), payload["lon"])
        if "acc" in payload:
            session.put_json(keys.state_key(entity.room, entity.name, "accuracy"), payload["acc"])
        if "batt" in payload:
            session.put_json(keys.state_key(entity.room, entity.name, "battery"), payload["batt"])
        if "tst" in payload:
            session.put_json(keys.state_key(entity.room, entity.name, "fixed_at"), payload["tst"])

    client = mqtt.connect(endpoint, on_owntracks_message, f"{BASE_TOPIC}/+/+")

    # Persons are read-only: no command subscription, unlike z2m's lights.
    session.ready()

    mqtt.wait_for_shutdown()

    # The MQTT loop stops first: an in-flight on_message during teardown
    # would otherwise put on a closed zenoh session.
    client.loop_stop()
    client.disconnect()
    with lock:
        if timer[0] is not None:
            timer[0].cancel()
    session.close()


if __name__ == "__main__":
    main()
