"""The arbiter's rule, without a bus: who holds an aspect, and for how long."""

import sys
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[2] / "adapters"))

from arbiter import Leases

LAMP = ("hall", "lamp", "on")
HOLD_S = 60.0


class Clock:
    def __init__(self):
        self.now = 1000.0

    def __call__(self):
        return self.now


class LeasesTest(unittest.TestCase):
    def setUp(self):
        self.clock = Clock()
        self.leases = Leases(self.clock)

    def wish(self, priority, actor="someone", target=LAMP):
        return self.leases.wish(target, priority, actor, HOLD_S)

    def test_an_unheld_aspect_forwards_and_is_then_held(self):
        self.assertEqual(self.wish("automation"), ("forward", None))
        self.assertEqual(self.leases.next_deadline(), 1000.0 + HOLD_S)

    def test_a_lower_band_is_refused_and_counted_against_the_hold(self):
        self.wish("manual", "family")
        action, holder = self.wish("automation", "scheduler")
        self.assertEqual(action, "refuse")
        self.assertEqual((holder["priority"], holder["actor"]), ("manual", "family"))
        self.wish("automation", "scheduler")
        [hold] = self.leases.document(wall=0.0)["holds"]
        self.assertEqual(hold["refused"], 2)
        self.assertEqual(hold["actor"], "family", "a refusal leaves the hold as it was")

    def test_a_higher_band_preempts_and_takes_the_hold(self):
        self.wish("automation", "scheduler")
        action, holder = self.wish("manual", "family")
        self.assertEqual(action, "preempt")
        self.assertEqual(holder["actor"], "scheduler")
        [hold] = self.leases.document(wall=0.0)["holds"]
        self.assertEqual((hold["priority"], hold["actor"]), ("manual", "family"))

    def test_the_same_band_refreshes_the_hold_and_its_tally(self):
        self.wish("agent", "a")
        self.wish("automation")  # refused: one against the hold
        self.clock.now += 30
        self.assertEqual(self.wish("agent", "b")[0], "forward")
        [hold] = self.leases.document(wall=0.0)["holds"]
        self.assertEqual((hold["actor"], hold["refused"]), ("b", 0))
        self.assertEqual(self.leases.next_deadline(), 1030.0 + HOLD_S)

    def test_an_expired_hold_reopens_the_aspect(self):
        self.wish("manual", "family")
        self.clock.now += HOLD_S
        self.assertEqual(self.wish("automation"), ("forward", None))

    def test_aspects_are_held_independently(self):
        self.wish("manual", "family")
        offset = ("hall", "lamp", "brightness")
        self.assertEqual(self.wish("automation", target=offset), ("forward", None))

    def test_prune_drops_only_what_has_ended(self):
        self.wish("manual", target=("a", "x", "on"))
        self.clock.now += 10
        self.wish("manual", target=("b", "y", "on"))
        self.assertFalse(self.leases.prune())
        self.clock.now = 1000.0 + HOLD_S
        self.assertTrue(self.leases.prune())
        self.assertEqual(self.leases.next_deadline(), 1010.0 + HOLD_S)

    def test_the_document_is_ordered_and_in_wall_time(self):
        self.wish("manual", target=("b", "y", "on"))
        self.wish("agent", target=("a", "x", "on"))
        self.clock.now += 15
        doc = self.leases.document(wall=1_700_000_000.0)
        self.assertEqual(doc["schema"], 1)
        self.assertEqual([h["room"] for h in doc["holds"]], ["a", "b"])
        # Taken 15 s ago, 45 s left.
        self.assertEqual(doc["holds"][0]["since"], "2023-11-14T22:13:05Z")
        self.assertEqual(doc["holds"][0]["until"], "2023-11-14T22:14:05Z")

    def test_the_document_leaves_out_an_ended_hold_before_it_is_pruned(self):
        self.wish("manual")
        self.clock.now += HOLD_S
        self.assertEqual(self.leases.document(wall=0.0)["holds"], [])


if __name__ == "__main__":
    unittest.main()
