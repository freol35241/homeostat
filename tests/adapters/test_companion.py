"""The companion translation, without a broker: a phone's messages in, aspects out."""

import json
import sys
import time
import unittest
from pathlib import Path
from types import SimpleNamespace

from homeostat.testing import FakeSession

sys.path.insert(0, str(Path(__file__).resolve().parents[2] / "adapters"))

from companion import Phones

TRACKER = SimpleNamespace(name="alice_tracker", room="person", id="alice/person", capability="tracker")
NOTIFIER = SimpleNamespace(name="alice_phone", room="person", id="alice/notifier", capability="notifier")


class PhonesTest(unittest.TestCase):
    def setUp(self):
        self.session = FakeSession("companion")
        self.phones = Phones(self.session, "companion", [TRACKER, NOTIFIER])

    def message(self, path, payload, base="companion"):
        raw = payload if isinstance(payload, bytes) else json.dumps(payload).encode()
        before = len(self.session.puts)
        self.phones.on_message(f"{base}/{path}", raw)
        return [(key.removeprefix("home/state/person/"), value) for key, value in self.session.puts[before:]]

    def drops(self):
        return [e["reason"] for e in self.session.events if e["kind"] == "drop"]

    def test_presence_is_home_on_the_tracker(self):
        self.assertEqual(self.message("alice/person/presence", True), [("alice_tracker/home", True)])

    def test_a_fix_fans_out_with_fixed_at_last(self):
        fix = {"fixed_at": 1700, "battery": 87, "accuracy": 12, "lon": 18.06, "lat": 59.33}
        self.assertEqual(
            self.message("alice/person/position", fix),
            [
                ("alice_tracker/lat", 59.33),
                ("alice_tracker/lon", 18.06),
                ("alice_tracker/accuracy", 12),
                ("alice_tracker/battery", 87),
                ("alice_tracker/fixed_at", 1700),
            ],
        )

    def test_a_fix_without_fixed_at_is_stamped_on_arrival(self):
        before = time.time()
        published = self.message("alice/person/position", {"lat": 1, "lon": 2, "battery": "87"})
        self.assertEqual([aspect for aspect, _ in published], ["alice_tracker/lat", "alice_tracker/lon", "alice_tracker/fixed_at"])
        self.assertTrue(before <= published[-1][1] <= time.time())

    def test_a_fix_without_numeric_lat_and_lon_drops(self):
        for fix in ({"lat": 1}, {"lat": "59", "lon": 18}, {"lat": True, "lon": 18}, [59, 18]):
            with self.subTest(fix=fix):
                self.assertEqual(self.message("alice/person/position", fix), [])
                self.assertEqual(self.drops()[-1], "malformed-payload")

    def test_available_reaches_both_entities_on_transition_only(self):
        both = [("alice_tracker/available", True), ("alice_phone/available", True)]
        self.assertEqual(self.message("alice/available", True), both)
        self.assertEqual(self.message("alice/available", True), [])
        self.assertEqual(
            self.message("alice/available", False),
            [("alice_tracker/available", False), ("alice_phone/available", False)],
        )

    def test_acknowledged_only_moves_forward(self):
        self.assertEqual(self.message("alice/notifier/ack", 100.5), [("alice_phone/acknowledged", 100.5)])
        self.assertEqual(self.message("alice/notifier/ack", 50), [])
        self.assertEqual(self.message("alice/notifier/ack", 100.5), [])
        self.assertEqual(self.message("alice/notifier/ack", 200), [("alice_phone/acknowledged", 200)])

    def test_a_wrongly_typed_payload_drops(self):
        for path, payload in (
            ("alice/person/presence", b"{"),
            ("alice/person/presence", "true"),
            ("alice/available", 1),
            ("alice/notifier/ack", True),
            ("alice/notifier/ack", "100"),
        ):
            with self.subTest(path=path, payload=payload):
                self.assertEqual(self.message(path, payload), [])
                self.assertEqual(self.drops()[-1], "malformed-payload")

    def test_an_unbound_subtree_is_reported_once(self):
        for _ in range(2):
            self.assertEqual(self.message("bob/person/presence", True), [])
            self.assertEqual(self.message("bob/available", True), [])
        self.assertEqual(self.drops(), ["unknown-device", "unknown-device"])

    def test_the_base_topic_may_have_several_levels(self):
        self.phones = Phones(self.session, "house/phones", [TRACKER])
        self.assertEqual(
            self.message("alice/person/presence", False, base="house/phones"),
            [("alice_tracker/home", False)],
        )


if __name__ == "__main__":
    unittest.main()
