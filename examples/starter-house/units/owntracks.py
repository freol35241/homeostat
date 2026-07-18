# /// script
# requires-python = ">=3.11"
# dependencies = [
#     "homeostat",
#     "paho-mqtt>=2,<3",
# ]
#
# [tool.uv.sources]
# homeostat = { git = "https://github.com/freol35241/homeostat", subdirectory = "sdk/python", tag = "v0.3.0" }
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
home/discovery/{unit} on each new device, each record carrying the
entity-file binding `id`, whether an entity file already binds it, and a
suggested capability/features stanza (capability "person", features []).
"""

import json
import os

import homeostat
from homeostat import house, keys, mqtt

BASE_TOPIC = "owntracks"


def main():
    unit = os.environ[keys.ENV_UNIT]
    config = house.load_adapter(unit)
    by_id = {e.id: e for e in config.entities}

    endpoint = mqtt.parse_endpoint(config.endpoint)

    session = homeostat.connect()
    inventory = {}

    def on_owntracks_message(client, userdata, msg):
        _, user, device = msg.topic.split("/")
        dev_id = f"{user}/{device}"
        if dev_id not in inventory:
            entity = by_id.get(dev_id)
            inventory[dev_id] = {
                "id": dev_id,
                "configured": entity is not None,
                "entity": entity.name if entity else None,
                "suggested": {"capability": "person", "features": []},
            }
            session.put_json(keys.discovery_key(unit), list(inventory.values()))

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

    session.close()
    client.loop_stop()
    client.disconnect()


if __name__ == "__main__":
    main()
