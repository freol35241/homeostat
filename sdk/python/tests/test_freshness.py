"""Freshness: pinned against an injected clock, no bus.

Run: uv run --no-project --with-editable sdk/python python -m unittest discover sdk/python/tests
"""

import unittest

from homeostat.freshness import Freshness


class FakeClock:
    def __init__(self):
        self.now = 1000.0

    def __call__(self):
        return self.now


class FreshnessTest(unittest.TestCase):
    def setUp(self):
        self.clock = FakeClock()
        self.inputs = Freshness(clock=self.clock)

    def test_triggering_sample_is_fresh_by_construction(self):
        # Even with max_age 0 the sample seen this instant is in the set:
        # a recompute triggered by a sample never sees an empty fresh set.
        self.inputs.seen("a", 20.0)
        self.assertEqual(self.inputs.fresh(0), {"a": 20.0})

    def test_stale_sources_drop_out_and_return_when_they_publish(self):
        self.inputs.seen("a", 20.0)
        self.clock.now += 100
        self.inputs.seen("b", 22.0)
        self.assertEqual(self.inputs.fresh(3600), {"a": 20.0, "b": 22.0})
        self.clock.now += 3550  # a is now 3650 s old, b 3550 s
        self.assertEqual(self.inputs.fresh(3600), {"b": 22.0})
        self.inputs.seen("a", 21.0)
        self.assertEqual(self.inputs.fresh(3600), {"a": 21.0, "b": 22.0})

    def test_catch_up_carries_its_age(self):
        # A restart delivers mirrored values with their age: one within
        # the policy counts, one beyond it does not, and neither is
        # passed off as seen just now.
        self.inputs.seen("a", 20.0, age_s=100)
        self.inputs.seen("b", 22.0, age_s=4000)
        self.assertEqual(self.inputs.fresh(3600), {"a": 20.0})
        self.clock.now += 3501  # a is now 3601 s old
        self.assertEqual(self.inputs.fresh(3600), {})

    def test_latest_value_wins(self):
        self.inputs.seen("a", 20.0)
        self.inputs.seen("a", 20.5)
        self.assertEqual(self.inputs.fresh(10), {"a": 20.5})

    def test_boundary_is_inclusive(self):
        self.inputs.seen("a", 20.0)
        self.clock.now += 60
        self.assertEqual(self.inputs.fresh(60), {"a": 20.0})
        self.clock.now += 0.001
        self.assertEqual(self.inputs.fresh(60), {})

    def test_forget(self):
        self.inputs.seen("a", 20.0)
        self.inputs.forget("a")
        self.inputs.forget("never-seen")
        self.assertEqual(self.inputs.fresh(3600), {})


if __name__ == "__main__":
    unittest.main()
