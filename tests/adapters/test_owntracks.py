"""OwnTracks translation, without a broker: fixes in, a person's aspects out."""

import json
import sys
import unittest
from pathlib import Path
from types import SimpleNamespace

sys.path.insert(0, str(Path(__file__).resolve().parents[2] / "adapters"))

from owntracks import MAX_UNBOUND, Tracker

ALICE = SimpleNamespace(name="alice", room="person", id="alice/phone")


class FakeSession:
    def __init__(self):
        self.puts = []
        self.events = []

    def put_json(self, key, value):
        self.puts.append((key, value))

    def health_event(self, kind, **fields):
        self.events.append({"kind": kind, **fields})


class TrackerTest(unittest.TestCase):
    def setUp(self):
        self.session = FakeSession()
        self.new_devices = 0
        self.tracker = Tracker(self.session, {"alice/phone": ALICE}, self.saw_new)

    def saw_new(self):
        self.new_devices += 1

    def message(self, dev_id, payload):
        raw = payload if isinstance(payload, bytes) else json.dumps(payload).encode()
        self.tracker.on_message(f"owntracks/{dev_id}", raw)

    def test_a_fix_becomes_the_persons_aspects(self):
        self.message(
            "alice/phone",
            {"_type": "location", "lat": 59.3, "lon": 18.1, "acc": 12, "batt": 80, "tst": 1700},
        )
        self.assertEqual(
            self.session.puts,
            [
                ("home/state/person/alice/lat", 59.3),
                ("home/state/person/alice/lon", 18.1),
                ("home/state/person/alice/accuracy", 12),
                ("home/state/person/alice/battery", 80),
                ("home/state/person/alice/fixed_at", 1700),
            ],
        )

    def test_optional_fields_are_optional(self):
        self.message("alice/phone", {"_type": "location", "lat": 1, "lon": 2})
        self.assertEqual([k.rsplit("/", 1)[1] for k, _ in self.session.puts], ["lat", "lon"])

    def test_other_owntracks_traffic_is_ignored(self):
        self.message("alice/phone", {"_type": "transition", "event": "enter"})
        self.assertEqual((self.session.puts, self.session.events), ([], []))

    def test_a_malformed_fix_drops(self):
        for payload in (b"{", [1], {"_type": "location", "lat": 1}):
            with self.subTest(payload=payload):
                self.message("alice/phone", payload)
                self.assertEqual(self.session.events[-1]["reason"], "malformed-payload")

    def test_an_unbound_phone_drops_once_and_is_in_discovery(self):
        for _ in range(3):
            self.message("bob/phone", {"_type": "location", "lat": 1, "lon": 2})
        self.assertEqual([e["reason"] for e in self.session.events], ["unknown-device"])
        self.assertEqual(self.new_devices, 1)
        [record] = self.tracker.records()
        self.assertEqual((record["id"], record["configured"]), ("bob/phone", False))

    def test_discovery_keeps_a_bounded_number_of_unbound_devices(self):
        self.message("alice/phone", {"_type": "lwt"})
        for n in range(MAX_UNBOUND + 1):
            self.message(f"stranger/{n}", {"_type": "lwt"})
        records = self.tracker.records()
        unbound = [r["id"] for r in records if not r["configured"]]
        self.assertEqual(len(unbound), MAX_UNBOUND)
        self.assertNotIn("stranger/0", unbound, "the oldest unbound goes first")
        self.assertIn("alice/phone", [r["id"] for r in records], "a bound device stays")


if __name__ == "__main__":
    unittest.main()
