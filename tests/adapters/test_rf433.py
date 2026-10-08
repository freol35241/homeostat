"""The 433 MHz translation, without a bridge: codes in, held booleans out."""

import sys
import unittest
from pathlib import Path
from types import SimpleNamespace

sys.path.insert(0, str(Path(__file__).resolve().parents[2] / "adapters"))

from rf433 import EVENTS_SUFFIX, LWT_SUFFIX, MAX_UNBOUND, Radio, aspect_for, code_from

BASE = "rfbridge"
EVENTS = f"{BASE}/{EVENTS_SUFFIX}"


def entity(name, code, capability="binary_sensor", features=("contact",)):
    return SimpleNamespace(
        name=name, room="hall", id=code, capability=capability, features=list(features)
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
    def test_a_code_is_a_bare_decimal_or_a_json_value(self):
        for raw in (b"13951014", b" 13951014 ", b'{"value": 13951014}', b'{"value": "13951014"}'):
            with self.subTest(raw=raw):
                self.assertEqual(code_from(raw), "13951014")
        for raw in (b"", b"door", b"[1]", b'{"value": null}', b"{"):
            with self.subTest(raw=raw):
                self.assertIsNone(code_from(raw))

    def test_the_aspect_comes_from_the_entity_file(self):
        self.assertEqual(aspect_for(entity("d", "1")), "contact")
        self.assertEqual(aspect_for(entity("p", "1", "presence", ())), "occupancy")
        self.assertIsNone(aspect_for(entity("x", "1", features=())))
        self.assertIsNone(aspect_for(entity("x", "1", features=("a", "b"))))


class RadioTest(unittest.TestCase):
    def setUp(self):
        self.now = 0.0
        self.new_codes = 0
        self.session = FakeSession()
        self.door = entity("door", "111")
        self.radio = Radio(
            self.session,
            BASE,
            [self.door, entity("odd", "222", features=())],
            hold_for=lambda aspect: 10.0,
            on_new_code=self.heard_new,
            clock=lambda: self.now,
        )

    def heard_new(self):
        self.new_codes += 1

    def hear(self, code):
        self.radio.on_message(EVENTS, code.encode())

    def test_startup_is_false_and_a_misconfigured_entity_says_so_once(self):
        self.radio.start()
        self.assertEqual(self.session.puts, [("home/state/hall/door/contact", False)])
        self.assertEqual(self.session.events[0]["reason"], "no-aspect")

    def test_a_burst_is_true_once_and_held_from_the_latest(self):
        self.hear("111")
        self.now = 5.0
        self.hear("111")
        self.assertEqual(self.session.puts, [("home/state/hall/door/contact", True)])
        self.now = 14.0
        self.radio.expire()
        self.assertEqual(len(self.session.puts), 1, "held 10 s from the second burst")
        self.now = 15.0
        self.radio.expire()
        self.assertEqual(self.session.puts[-1], ("home/state/hall/door/contact", False))

    def test_the_bridge_lwt_is_every_entitys_availability(self):
        self.radio.on_message(f"{BASE}/{LWT_SUFFIX}", b"Offline")
        self.assertEqual(
            self.session.puts,
            [("home/state/hall/door/available", False), ("home/state/hall/odd/available", False)],
        )

    def test_a_malformed_payload_drops(self):
        self.radio.on_message(EVENTS, b"door")
        self.assertEqual(self.session.events[0]["reason"], "malformed-payload")

    def test_discovery_has_news_only_on_a_first_hearing(self):
        for code in ("111", "111", "999", "999"):
            self.hear(code)
        self.assertEqual(self.new_codes, 2)
        records = {r["id"]: r for r in self.radio.inventory()}
        self.assertTrue(records["111"]["heard"])
        self.assertFalse(records["222"]["heard"], "bound and silent is still listed")
        self.assertEqual(records["999"]["configured"], False)
        self.assertNotIn("aspects", records["222"])

    def test_unbound_codes_are_capped_oldest_first(self):
        for code in range(MAX_UNBOUND + 1):
            self.hear(str(1000 + code))
        unbound = [r["id"] for r in self.radio.inventory() if not r["configured"]]
        self.assertEqual(len(unbound), MAX_UNBOUND)
        self.assertNotIn("1000", unbound)

    def test_a_detector_is_notable_and_a_door_is_not(self):
        smoke = entity("smoke", "333", features=("smoke",))
        radio = Radio(self.session, BASE, [smoke, self.door], lambda _: 1.0, lambda: None)
        self.assertTrue(radio.descriptor(smoke)["fields"]["smoke"]["notable"])
        self.assertNotIn("notable", radio.descriptor(self.door)["fields"]["contact"])


if __name__ == "__main__":
    unittest.main()
