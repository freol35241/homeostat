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
"""Zigbee2MQTT adapter: a translating subscriber.

Device state published as JSON on zigbee2mqtt/{id} fans out to per-aspect
keys home/state/{room}/{entity}/{aspect}; commands on
home/cmd/{room}/{entity}/{aspect} translate to zigbee2mqtt/{id}/set. The
entity file's `id` is the z2m topic segment; the file stem is the entity
name. The z2m `state` field is normalized (`on` for lights/switches,
`locked` for locks); other scalar fields pass through under their z2m
names; nested objects (e.g. color) are deferred. Arbitrated entities (e.g.
locks) get no home/cmd subscription at all — plan-time expansion gives the
adapter's templated cmd subscription only its non-arbitrated bound
entities — and instead receive the arbiter's forwarded envelope on
home/arbiter/{room}/{entity}/{aspect}, translated the same way a cmd
envelope would be. Anything dropped emits a JSON event at
home/health/{unit}/event instead of crashing.

The retained zigbee2mqtt/bridge/devices inventory is republished at
home/discovery/{unit}: every paired device (coordinator excluded) as a
record carrying the entity-file binding `id`, whether an entity file
already binds it, a best-effort suggested capability/features stanza
mapped from the z2m `exposes` descriptor, and the raw definition for
anything the mapping does not cover (docs/design.md, Discovery).
"""

import json
import os

import homeostat
from homeostat import house, keys, mqtt

BASE_TOPIC = "zigbee2mqtt"


def state_aspect(capability: str, z2m_field: str, value):
    """Maps one z2m JSON field to a (bus aspect, JSON value) pair."""
    if z2m_field == "state":
        if capability == "lock":
            return "locked", value == "LOCKED"
        return "on", value == "ON"
    return z2m_field, value


def suggest(exposes):
    """Best-effort entity-file stanza from a z2m exposes descriptor, or
    None when no confident mapping exists — the raw definition rides
    along in the record either way, so nothing becomes invisible."""
    for exp in exposes:
        if exp.get("type") == "light":
            inner = {f.get("property") for f in exp.get("features", [])}
            return {
                "capability": "light",
                "features": ["brightness"] if "brightness" in inner else [],
            }
        if exp.get("type") == "lock":
            return {"capability": "lock", "features": []}
        if exp.get("type") == "binary" and exp.get("property") == "occupancy":
            return {"capability": "presence", "features": []}
    return None


def inventory(devices, by_id):
    """The complete discovery document from one bridge/devices payload."""
    records = []
    for dev in devices:
        if dev.get("type") == "Coordinator":
            continue
        dev_id = dev.get("friendly_name") or dev.get("ieee_address")
        if not dev_id:
            continue
        definition = dev.get("definition") or {}
        entity = by_id.get(dev_id)
        records.append(
            {
                "id": dev_id,
                "configured": entity is not None,
                "entity": entity.name if entity else None,
                "suggested": suggest(definition.get("exposes") or []),
                "description": {
                    "vendor": definition.get("vendor"),
                    "model": definition.get("model"),
                    "description": definition.get("description"),
                    "exposes": definition.get("exposes"),
                },
            }
        )
    return records


def main():
    unit = os.environ[keys.ENV_UNIT]
    config = house.load_adapter(unit)
    by_id = {e.id: e for e in config.entities}

    endpoint = mqtt.parse_endpoint(config.endpoint)

    session = homeostat.connect()

    def on_z2m_message(client, userdata, msg):
        if msg.topic == f"{BASE_TOPIC}/bridge/devices":
            try:
                devices = json.loads(msg.payload)
            except ValueError:
                devices = None
            if not isinstance(devices, list):
                session.health_event("drop", reason="malformed-payload", topic=msg.topic)
                return
            session.put_json(keys.discovery_key(unit), inventory(devices, by_id))
            return
        entity = by_id.get(msg.topic.split("/", 1)[1])
        if entity is None:
            session.health_event("drop", reason="unknown-device", topic=msg.topic)
            return
        try:
            payload = json.loads(msg.payload)
        except ValueError:
            payload = None
        if not isinstance(payload, dict):
            session.health_event("drop", reason="malformed-payload", topic=msg.topic)
            return
        for z2m_field, value in payload.items():
            if isinstance(value, (dict, list)):
                continue  # composite fields (color, ...) deferred
            aspect, value = state_aspect(entity.capability, z2m_field, value)
            session.put_json(keys.state_key(entity.room, entity.name, aspect), value)

    def cmd_handler(entity):
        def handler(sample):
            key = str(sample.key_expr)
            aspect = key.split("/", 4)[4]
            try:
                payload = json.loads(sample.payload.to_bytes())
            except ValueError:
                session.health_event("drop", reason="malformed-payload", key=key)
                return
            try:
                value = keys.parse_cmd_envelope(payload)
            except ValueError:
                session.health_event("drop", reason="invalid-command", key=key)
                return
            if aspect == "on":
                if not isinstance(value, bool):
                    session.health_event("drop", reason="invalid-command", key=key)
                    return
                body = {"state": "ON" if value else "OFF"}
            elif aspect == "locked":
                if not isinstance(value, bool):
                    session.health_event("drop", reason="invalid-command", key=key)
                    return
                # z2m's lock vocabulary is asymmetric: state REPORTS are
                # LOCKED/UNLOCKED, but SET commands are LOCK/UNLOCK.
                body = {"state": "LOCK" if value else "UNLOCK"}
            elif "/" in aspect:
                session.health_event("drop", reason="invalid-command", key=key)
                return
            else:
                body = {aspect: value}
            client.publish(f"{BASE_TOPIC}/{entity.id}/set", json.dumps(body))

        return handler

    client = mqtt.connect(
        endpoint,
        on_z2m_message,
        [(f"{BASE_TOPIC}/+", 0), (f"{BASE_TOPIC}/bridge/devices", 0)],
    )

    # An arbitrated entity has no home/cmd subscription at all — not
    # subscribing IS the structural enforcement — and instead gets the
    # arbiter's forwarded, post-arbitration envelope on home/arbiter/**.
    subscribers = [
        session.subscribe(keys.cmd_keyexpr(e.room, e.name), cmd_handler(e))
        for e in config.entities
        if e.write_mode != "arbitrated"
    ] + [
        session.subscribe(keys.arbiter_keyexpr(e.room, e.name), cmd_handler(e))
        for e in config.entities
        if e.write_mode == "arbitrated"
    ]

    # Both translation directions are wired up: the unit is ready.
    session.ready()

    mqtt.wait_for_shutdown()

    for sub in subscribers:
        sub.undeclare()
    session.close()
    client.loop_stop()
    client.disconnect()


if __name__ == "__main__":
    main()
