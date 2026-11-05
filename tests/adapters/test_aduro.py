"""The Aduro translation, without a bridge: NBE documents in, a burner out."""

import json
import sys
import unittest
from pathlib import Path
from types import SimpleNamespace

sys.path.insert(0, str(Path(__file__).resolve().parents[2] / "adapters"))

from aduro import ASPECT_DESCRIPTOR, Stoves, command_body, route, status_aspects

STOVE = SimpleNamespace(name="stove", room="living", id="aduro/stove1")
KEY = "home/state/living/stove/"


class FakeSession:
    def __init__(self):
        self.puts = []
        self.events = []

    def put_json(self, key, value):
        self.puts.append((key, value))

    def health_event(self, kind, **fields):
        self.events.append({"kind": kind, **fields})


class DialectTest(unittest.TestCase):
    def test_status_is_the_burner_vocabulary_plus_firmware_fields(self):
        aspects, reserved = status_aspects(
            {"smoke_temp": 180, "regulation.fixed_power": 50.0, "state": 5, "shaft_temp": 40}
        )
        self.assertEqual(
            aspects,
            {
                "flue_temperature": 180,
                "power_level": 50,
                "state": 5,
                "shaft_temp": 40,
                "on": True,
                "fault": False,
            },
        )
        self.assertIsInstance(aspects["power_level"], int)
        self.assertEqual(reserved, [])

    def test_the_run_state_says_whether_it_burns_and_whether_it_needs_someone(self):
        # (code, on, fault). The bridge sends codes as floats.
        for code, on, fault in (
            (5.0, True, False),  # normal
            (6.0, True, False),  # room temperature reached: a pause it resumes from
            (9.0, True, False),  # wood burning
            (24.0, True, False),  # air damper closed, an operating state
            (14.0, False, False),  # off by the switch
            (23.0, False, False),  # stopped by its own timer
            (20.0, False, True),  # no fuel
            (13.0, False, True),  # failed ignition
            (28.0, False, True),  # door open
            (34.0, False, True),  # check burn cup
            (33.0, False, True),  # CO sensor defect
        ):
            with self.subTest(code=code):
                aspects = status_aspects({"state": code})[0]
                self.assertEqual((aspects["on"], aspects["fault"]), (on, fault))

    def test_an_unknown_state_derives_nothing(self):
        # Neither burning nor stopped is a safe guess for a code the table
        # does not list, so `on` and `fault` keep their last values.
        for state in (7.0, "?"):
            with self.subTest(state=state):
                aspects = status_aspects({"state": state})[0]
                self.assertNotIn("on", aspects)
                self.assertNotIn("fault", aspects)
                self.assertEqual(aspects["state"], state, "the raw code still publishes")

    def test_the_descriptor_names_each_state(self):
        values = ASPECT_DESCRIPTOR["fields"]["state"]["values"]
        self.assertIn({"value": 20, "label": "no fuel"}, values)
        self.assertTrue(ASPECT_DESCRIPTOR["fields"]["fault"]["notable"])

    def test_a_field_naming_a_derived_aspect_is_reserved(self):
        aspects, reserved = status_aspects({"on": 1, "fault": 0, "available": 0, "state": 14})
        self.assertEqual(sorted(reserved), ["available", "fault", "on"])
        self.assertFalse(aspects["on"], "the derived value, not the wire field")
        self.assertFalse(aspects["fault"])

    def test_commands_take_only_their_values(self):
        self.assertEqual(command_body("on", True), {"path": "misc.start", "value": "1"})
        self.assertEqual(command_body("on", False), {"path": "misc.stop", "value": "1"})
        self.assertEqual(
            command_body("power_level", 50), {"path": "regulation.fixed_power", "value": 50}
        )
        for aspect, value in (("on", 1), ("power_level", 40), ("power_level", True), ("x", 1)):
            with self.subTest(aspect=aspect, value=value):
                self.assertIsNone(command_body(aspect, value))

    def test_route_finds_the_stove_by_its_base_topic(self):
        self.assertEqual(route("aduro/stove1/status", [STOVE]), (STOVE, ["status"]))
        self.assertEqual(route("other/status", [STOVE]), (None, None))


class StovesTest(unittest.TestCase):
    def setUp(self):
        self.now = 0.0
        self.session = FakeSession()
        self.stoves = Stoves(self.session, "aduro", [STOVE], clock=lambda: self.now)

    def message(self, kind, document):
        raw = document if isinstance(document, bytes) else json.dumps(document).encode()
        self.stoves.on_message(f"{STOVE.id}/{kind}", raw)

    def test_first_contact_is_available_and_bound(self):
        self.message("status", {"state": 14})
        self.assertEqual(self.session.puts[0], (KEY + "available", True))
        discovery = [v for k, v in self.session.puts if k == "home/discovery/aduro"]
        self.assertTrue(discovery[0][0]["bound"])

    def test_a_repeated_poll_is_not_a_sample(self):
        self.message("status", {"boiler_temp": 60})
        self.message("status", {"boiler_temp": 60})
        self.message("status", {"boiler_temp": 61})
        temps = [v for k, v in self.session.puts if k == KEY + "boiler_temperature"]
        self.assertEqual(temps, [60, 61])

    def test_operating_fields_are_prefixed(self):
        self.message("operating", {"power_kw": 4.2})
        self.assertIn((KEY + "operating_power_kw", 4.2), self.session.puts)

    def test_a_non_object_drops(self):
        self.message("status", b"[]")
        self.assertEqual(self.session.events[-1]["reason"], "malformed-payload")

    def test_an_unknown_state_is_reported_once(self):
        self.message("status", {"state": 5.0})
        self.message("status", {"state": 7.0})
        self.message("status", {"state": 7.0})
        unknown = [e for e in self.session.events if e["kind"] == "unknown-state"]
        self.assertEqual(unknown, [{"kind": "unknown-state", "topic": f"{STOVE.id}/status", "state": 7.0}])
        on = [v for k, v in self.session.puts if k == KEY + "on"]
        self.assertEqual(on, [True], "on keeps its last value through the unknown code")

    def test_silence_is_one_unavailable_and_one_event(self):
        self.message("status", {"state": 14})
        self.now = 301.0
        self.stoves.sweep(300.0)
        self.stoves.sweep(300.0)
        self.assertEqual(self.session.puts[-1], (KEY + "available", False))
        silent = [e for e in self.session.events if e["kind"] == "device-silent"]
        self.assertEqual(len(silent), 1)


if __name__ == "__main__":
    unittest.main()
