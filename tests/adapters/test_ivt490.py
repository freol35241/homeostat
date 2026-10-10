"""The IVT490 translation, without a broker: firmware topics in and out."""

import json
import sys
import unittest
from pathlib import Path
from types import SimpleNamespace

sys.path.insert(0, str(Path(__file__).resolve().parents[2] / "adapters"))

from ivt490 import (
    Feed,
    Pumps,
    aspect_descriptor,
    command_payload,
    field_value,
    state_aspect,
    state_field,
)

PUMP = SimpleNamespace(name="heatpump", room="utility", id="ivt", inputs={})
KEY = "home/state/utility/heatpump/"


class FakeSession:
    def __init__(self):
        self.puts = []
        self.events = []

    def put_json(self, key, value):
        self.puts.append((key, value))

    def health_event(self, kind, **fields):
        self.events.append({"kind": kind, **fields})


class DialectTest(unittest.TestCase):
    def test_nested_state_paths_flatten_without_the_serial_wrapper(self):
        self.assertEqual(state_field(["serial", "GT1"]), "GT1")
        self.assertEqual(state_field(["alarm", "code"]), "alarm_code")

    def test_aspects_normalise_collide_and_reserve(self):
        self.assertEqual(state_aspect("state", "GT1"), "feed_temperature")
        self.assertEqual(state_aspect("controller", "indoor_temperature_target"), "setpoint")
        self.assertEqual(state_aspect("controller", "GT1"), "controller_GT1")
        self.assertEqual(state_aspect("state", "GT2"), "GT2")
        self.assertIsNone(state_aspect("state", "available"))
        self.assertIsNone(state_aspect("state", "setpoint"), "only its override may mint it")

    def test_a_controller_field_is_a_scalar_or_a_value_object(self):
        self.assertEqual(field_value(b"21.5"), (21.5, None))
        self.assertEqual(field_value(b'{"value": 21.5, "valid": false}'), (21.5, False))
        with self.assertRaises(KeyError):
            field_value(b'{"valid": true}')


class CommandTest(unittest.TestCase):
    def test_commands_take_their_range_and_nothing_else(self):
        self.assertEqual(
            command_payload(PUMP, "setpoint", 21), ("indoor_temperature_target", "21.0", True)
        )
        self.assertEqual(
            command_payload(PUMP, "operating_mode", 2), ("operating_mode", "2", False)
        )
        for aspect, value in (
            ("setpoint", 31),
            ("setpoint", True),
            ("operating_mode", 2.0),
            ("operating_mode", 4),
            ("brightness", 1),
        ):
            with self.subTest(aspect=aspect, value=value):
                self.assertIsNone(command_payload(PUMP, aspect, value))

    def test_a_fed_input_is_not_a_command(self):
        fed = SimpleNamespace(**{**vars(PUMP), "inputs": {"outdoor_temperature_offset": None}})
        self.assertIsNone(command_payload(fed, "outdoor_temperature_offset", 1.0))
        fields = aspect_descriptor(fed)["fields"]
        self.assertNotIn("command", fields["outdoor_temperature_offset"])
        self.assertEqual(fields["setpoint"]["command"]["editable_by"], "family")


class PumpsTest(unittest.TestCase):
    def setUp(self):
        self.now = 0.0
        self.session = FakeSession()
        self.pumps = Pumps(self.session, "ivt490", [PUMP], clock=lambda: self.now)

    def message(self, rest, raw):
        self.pumps.on_message(f"{PUMP.id}/{rest}", raw)

    def test_a_state_field_is_an_aspect_and_contact_is_available(self):
        self.message("ivt490/state/GT1", b"42.5")
        self.assertEqual(self.session.puts[0], (KEY + "available", True))
        self.assertIn((KEY + "feed_temperature", 42.5), self.session.puts)

    def test_validity_goes_out_ahead_of_its_value(self):
        self.message("controller/state/indoor_temperature_feedback", b'{"value": 20, "valid": 1}')
        tail = self.session.puts[-2:]
        self.assertEqual(
            tail, [(KEY + "indoor_temperature_valid", True), (KEY + "indoor_temperature", 20)]
        )

    def test_a_null_reading_is_not_published(self):
        self.message("controller/state/indoor_temperature_feedback", b'{"value": null, "valid": 0}')
        self.assertEqual(self.session.puts[-1], (KEY + "indoor_temperature_valid", False))
        self.assertEqual(self.session.events[-1]["reason"], "null-value")

    def test_drops(self):
        cases = [
            ("ivt490/state/GT1", b"{", "malformed-payload"),
            ("controller/state/x", b'{"valid": 1}', "malformed-payload"),
            ("ivt490/state/available", b"1", "reserved-aspect"),
        ]
        for rest, raw, reason in cases:
            with self.subTest(rest=rest):
                self.message(rest, raw)
                self.assertEqual(self.session.events[-1]["reason"], reason)

    def test_a_nested_blob_is_left_to_its_leaves(self):
        self.message("ivt490/state/serial", json.dumps({"GT1": 1}).encode())
        self.assertNotIn("serial", " ".join(k for k, _ in self.session.puts))

    def test_silence_is_one_unavailable_and_one_event(self):
        self.message("ivt490/state/GT1", b"1")
        self.now = 400.0
        self.pumps.sweep(300.0)
        self.pumps.sweep(300.0)
        self.assertEqual(self.session.puts[-1], (KEY + "available", False))
        self.assertEqual(len([e for e in self.session.events if e["kind"] == "device-silent"]), 1)


class FeedTest(unittest.TestCase):
    SOURCE = SimpleNamespace(room="living", entity="thermo", aspect="temperature")
    VALUE = "home/state/living/thermo/temperature"
    AVAILABLE = "home/state/living/thermo/available"

    def setUp(self):
        self.session = FakeSession()
        self.sent = []
        self.feed = Feed(
            self.session,
            lambda topic, payload, retain: self.sent.append((topic, payload, retain)),
            PUMP,
            "indoor_temperature_actual",
            self.SOURCE,
        )

    def sample(self, key, value):
        self.feed.on_sample(key, json.dumps(value).encode())

    def test_a_reading_is_forwarded_unretained(self):
        self.sample(self.VALUE, 21)
        self.assertEqual(self.sent, [("ivt/controller/set/indoor_temperature_actual", "21.0", False)])

    def test_loss_clears_the_retained_slot_once_and_drops_once(self):
        self.sample(self.AVAILABLE, False)
        self.sample(self.AVAILABLE, False)
        self.assertEqual(self.sent, [("ivt/controller/set/indoor_temperature_actual", b"", True)])
        self.sample(self.VALUE, 21)
        self.sample(self.VALUE, 22)
        reasons = [e.get("reason", e["kind"]) for e in self.session.events]
        self.assertEqual(reasons, ["feed-source-lost", "feed-source-unavailable"])
        self.sample(self.AVAILABLE, True)
        self.sample(self.VALUE, 23)
        self.assertEqual(self.sent[-1][1], "23.0")

    def test_an_out_of_bounds_reading_is_refused(self):
        self.sample(self.VALUE, 99)
        self.sample(self.VALUE, True)
        self.assertEqual(self.sent, [])
        self.assertEqual([e["reason"] for e in self.session.events], ["invalid-feed"] * 2)

    def test_other_aspects_of_the_source_are_ignored(self):
        self.sample("home/state/living/thermo/humidity", 50)
        self.assertEqual((self.sent, self.session.events), ([], []))

    def test_a_replayed_reading_is_not_forwarded(self):
        # The core replays the last recorded value after a restart. It can
        # be old, and the pump is fed readings, not history.
        self.feed.on_sample(self.VALUE, b"21", live=False)
        self.assertEqual((self.sent, self.session.events), ([], []))
        self.sample(self.VALUE, 22)
        self.assertEqual(self.sent, [("ivt/controller/set/indoor_temperature_actual", "22.0", False)])

    def test_a_replayed_availability_does_not_clear_the_slot(self):
        self.feed.on_sample(self.AVAILABLE, b"false", live=False)
        self.assertEqual((self.sent, self.session.events), ([], []))
        self.sample(self.VALUE, 21)
        self.assertEqual(len(self.sent), 1)


if __name__ == "__main__":
    unittest.main()
