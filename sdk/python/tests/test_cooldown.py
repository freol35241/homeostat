"""Cooldown: pinned against an injected clock, no bus.

Run: uv run --no-project --with-editable sdk/python python -m unittest discover sdk/python/tests
"""

import unittest

from homeostat.cooldown import Cooldown


class FakeClock:
    def __init__(self):
        self.now = 1000.0

    def __call__(self):
        return self.now


class CooldownTest(unittest.TestCase):
    def setUp(self):
        self.clock = FakeClock()
        self.cooldown = Cooldown(clock=self.clock)

    def test_first_firing_is_ready_and_repeats_inside_the_window_are_not(self):
        self.assertTrue(self.cooldown.ready("intrusion", 600))
        # 417 samples in 56 seconds (#3): every one of them refused.
        for _ in range(417):
            self.clock.now += 56 / 417
            self.assertFalse(self.cooldown.ready("intrusion", 600))

    def test_refusals_do_not_extend_the_window(self):
        self.assertTrue(self.cooldown.ready("k", 600))
        self.clock.now += 599
        self.assertFalse(self.cooldown.ready("k", 600))
        self.clock.now += 1  # 600 s since the FIRING, not since the refusal
        self.assertTrue(self.cooldown.ready("k", 600))

    def test_keys_are_independent_and_reset_reopens(self):
        self.assertTrue(self.cooldown.ready("a", 600))
        self.assertTrue(self.cooldown.ready("b", 600))
        self.assertFalse(self.cooldown.ready("a", 600))
        self.cooldown.reset("a")
        self.assertTrue(self.cooldown.ready("a", 600))
        self.assertFalse(self.cooldown.ready("b", 600))


if __name__ == "__main__":
    unittest.main()
