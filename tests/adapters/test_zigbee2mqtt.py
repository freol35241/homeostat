"""The zigbee2mqtt translation, without a broker: z2m's dialect in and out."""

import json
import sys
import unittest
from pathlib import Path
from types import SimpleNamespace

sys.path.insert(0, str(Path(__file__).resolve().parents[2] / "adapters"))

from zigbee2mqtt import (
    Bridge,
    availability_state,
    command_body,
    describe,
    inventory,
    state_aspect,
)

BASE = "zigbee2mqtt"


def entity(name, capability, features=(), room="hall", dev_id=None):
    return SimpleNamespace(
        name=name, room=room, capability=capability, features=list(features), id=dev_id or name
    )


class FakeSession:
    def __init__(self):
        self.puts = []
        self.events = []

    def put_json(self, key, value):
        self.puts.append((key, value))

    def health_event(self, kind, **fields):
        self.events.append({"kind": kind, **fields})


class DialectTest(unittest.TestCase):
    def test_availability_takes_the_object_or_the_legacy_string(self):
        self.assertEqual(availability_state(b'{"state": "online"}'), "online")
        self.assertEqual(availability_state(b"offline"), "offline")
        self.assertIsNone(availability_state(b'{"state": "maybe"}'))
        self.assertIsNone(availability_state(b"[1]"))

    def test_state_is_on_or_locked_by_capability(self):
        self.assertEqual(state_aspect("light", "state", "ON"), ("on", True))
        self.assertEqual(state_aspect("lock", "state", "UNLOCKED"), ("locked", False))
        self.assertEqual(state_aspect("light", "brightness", 80), ("brightness", 80))


class CommandBodyTest(unittest.TestCase):
    def test_the_base_aspect_takes_a_bool(self):
        lamp = entity("lamp", "light")
        self.assertEqual(command_body(lamp, "on", True, {}), {"state": "ON"})
        self.assertIsNone(command_body(lamp, "on", 1, {}))

    def test_a_lock_is_set_with_lock_and_unlock(self):
        door = entity("door", "lock")
        self.assertEqual(command_body(door, "locked", True, {}), {"state": "LOCK"})
        self.assertEqual(command_body(door, "locked", False, {}), {"state": "UNLOCK"})

    def test_a_vocabulary_feature_needs_declaring_and_a_number(self):
        lamp = entity("lamp", "light", features=["brightness"])
        self.assertEqual(command_body(lamp, "brightness", 120, {}), {"brightness": 120})
        self.assertIsNone(command_body(lamp, "brightness", True, {}))
        self.assertIsNone(command_body(entity("bare", "light"), "brightness", 120, {}))

    def test_a_descriptor_command_is_checked_against_its_type(self):
        trv = entity("trv", "sensor")
        fields = {
            "mode": {"command": {"type": "enum"}, "values": [{"value": "heat"}, {"value": "off"}]},
            "offset": {"command": {"type": "float"}},
            "battery": {"kind": "percent"},
        }
        self.assertEqual(command_body(trv, "mode", "heat", fields), {"mode": "heat"})
        self.assertIsNone(command_body(trv, "mode", "cool", fields))
        self.assertEqual(command_body(trv, "offset", -1.5, fields), {"offset": -1.5})
        self.assertIsNone(command_body(trv, "offset", "x", fields))
        self.assertIsNone(command_body(trv, "battery", 50, fields), "a reading takes no command")
        self.assertIsNone(command_body(trv, "unknown", 1, fields))


class DescribeTest(unittest.TestCase):
    def test_nothing_scalar_is_no_descriptor(self):
        self.assertIsNone(describe("sensor", [{"type": "composite", "property": "color"}]))

    def test_a_settable_number_is_an_owner_command_with_its_bounds(self):
        [field] = describe(
            "sensor",
            [
                {
                    "type": "numeric",
                    "property": "local_temperature_calibration",
                    "access": 7,
                    "value_min": -5,
                    "value_max": 5,
                    "value_step": 0.5,
                }
            ],
        )["fields"].values()
        self.assertEqual(
            field["command"],
            {
                "type": "float",
                "editable_by": "owner",
                "constraint": {"min": -5, "max": 5},
                "step": 0.5,
            },
        )

    def test_battery_is_a_reading_and_comes_last(self):
        descriptor = describe(
            "sensor",
            [
                {"type": "numeric", "property": "battery", "unit": "%", "category": "diagnostic"},
                {"type": "numeric", "property": "temperature", "unit": "°C"},
            ],
        )
        self.assertEqual(list(descriptor["fields"]), ["temperature", "battery"])
        self.assertEqual(descriptor["fields"]["battery"]["group"], "readings")
        self.assertEqual(descriptor["fields"]["temperature"]["kind"], "temperature")

    def test_the_vocabulary_is_described_as_readings_only(self):
        light = {
            "type": "light",
            "features": [{"type": "binary", "property": "state", "access": 7}],
        }
        field = describe("light", [light])["fields"]["on"]
        self.assertNotIn("command", field, "the dashboard's widget commands it")

    def test_a_notable_binary_says_so(self):
        field = describe("sensor", [{"type": "binary", "property": "water_leak"}])["fields"]
        self.assertTrue(field["water_leak"]["notable"])


class InventoryTest(unittest.TestCase):
    def test_every_device_but_the_coordinator_and_the_malformed(self):
        lamp = entity("lamp", "light", dev_id="0x01")
        devices = [
            {"type": "Coordinator", "ieee_address": "0x00"},
            "not a device",
            {"friendly_name": "0x01", "definition": {"exposes": [{"type": "light"}]}},
            {"ieee_address": "0x02", "definition": None},
            {"definition": {}},
        ]
        records = inventory(devices, {"0x01": lamp})
        self.assertEqual([r["id"] for r in records], ["0x01", "0x02"])
        self.assertEqual((records[0]["configured"], records[0]["entity"]), (True, "lamp"))
        self.assertEqual(records[0]["suggested"], {"capability": "light", "features": []})
        self.assertEqual((records[1]["configured"], records[1]["suggested"]), (False, None))


class BridgeTest(unittest.TestCase):
    def setUp(self):
        self.session = FakeSession()
        self.lamp = entity("lamp", "light", dev_id="0x01")
        self.bridge = Bridge(self.session, "zigbee", BASE, {"0x01": self.lamp})

    def message(self, rest, payload):
        raw = payload if isinstance(payload, bytes) else json.dumps(payload).encode()
        self.bridge.on_message(f"{BASE}/{rest}", raw)

    def test_the_inventory_is_published_and_marks_what_the_bridge_knows(self):
        self.message("bridge/devices", [{"friendly_name": "0x01"}, {"friendly_name": "0x02"}])
        self.assertTrue(self.bridge.inventory_seen.is_set())
        [(key, records)] = self.session.puts
        self.assertEqual(key, "home/discovery/zigbee")
        self.assertEqual([r["configured"] for r in records], [True, False])
        self.assertEqual(self.bridge.known, {"0x01", "0x02"})
        self.message("0x02", {"state": "ON"})
        self.assertEqual(self.session.events, [], "known but unbound is a steady state")
        self.message("0x03", {"state": "ON"})
        self.assertEqual(self.session.events[0]["reason"], "unknown-device")

    def test_state_fields_become_aspects(self):
        self.message("0x01", {"state": "ON", "brightness": 80, "color": {"x": 0.3}})
        self.assertEqual(
            self.session.puts,
            [("home/state/hall/lamp/on", True), ("home/state/hall/lamp/brightness", 80)],
        )

    def test_a_field_cannot_impersonate_availability(self):
        self.message("0x01", {"available": True})
        self.assertEqual(self.session.puts, [])
        self.assertEqual(self.session.events[0]["reason"], "reserved-aspect")

    def test_a_non_object_payload_drops(self):
        self.message("0x01", b"not json")
        self.assertEqual(self.session.events[0]["reason"], "malformed-payload")

    def test_device_availability_is_the_available_aspect(self):
        self.message("0x01/availability", {"state": "offline"})
        self.assertEqual(self.session.puts, [("home/state/hall/lamp/available", False)])

    def test_the_bridge_going_offline_is_one_event_per_transition(self):
        for state in ("offline", "offline", "online", "offline"):
            self.message("bridge/state", {"state": state})
        silent = [e for e in self.session.events if e["kind"] == "bridge-silent"]
        self.assertEqual(len(silent), 2)

    def test_descriptor_fields_are_kept_for_commands(self):
        self.message(
            "bridge/devices",
            [
                {
                    "friendly_name": "0x01",
                    "definition": {
                        "exposes": [{"type": "numeric", "property": "level", "access": 2}]
                    },
                }
            ],
        )
        fields = self.bridge.descriptor_fields["lamp"]
        self.assertEqual(command_body(self.lamp, "level", 3, fields), {"level": 3})


if __name__ == "__main__":
    unittest.main()
