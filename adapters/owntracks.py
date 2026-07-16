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
home/discovery/{unit} on each new device, each record carrying the
entity-file binding `id`, whether an entity file already binds it, and a
suggested capability/features stanza (capability "person", features []).
"""

import json
import os
import signal
import threading
from urllib.parse import urlparse

import paho.mqtt.client as mqtt

import homeostat
from homeostat import house, keys

BASE_TOPIC = "owntracks"


def main():
    unit = os.environ[keys.ENV_UNIT]
    config = house.load_adapter(unit)
    by_id = {e.id: e for e in config.entities}

    endpoint = urlparse(config.endpoint)
    if endpoint.scheme != "mqtt":
        raise ValueError(f"unsupported endpoint scheme: {config.endpoint}")

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

    subscribed = threading.Event()
    client = mqtt.Client(mqtt.CallbackAPIVersion.VERSION2)
    client.on_message = on_owntracks_message
    client.on_connect = lambda c, *_: c.subscribe(f"{BASE_TOPIC}/+/+", 0)
    client.on_subscribe = lambda *_: subscribed.set()
    client.connect(endpoint.hostname, endpoint.port or 1883)
    client.loop_start()
    if not subscribed.wait(timeout=30):
        raise TimeoutError("no MQTT SUBACK within 30s")

    # Persons are read-only: no command subscription, unlike z2m's lights.
    session.ready()

    stop = threading.Event()
    signal.signal(signal.SIGTERM, lambda *_: stop.set())
    signal.signal(signal.SIGINT, lambda *_: stop.set())
    stop.wait()

    session.close()
    client.loop_stop()
    client.disconnect()


if __name__ == "__main__":
    main()
