"""What one gesture action sends (docs/design.md#gesture-actions), without a bus."""

import sys
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[2] / "adapters"))

from buttons import UnknownState, commands


class CommandsTest(unittest.TestCase):
    def test_a_toggle_flips_the_known_state(self):
        self.assertEqual(commands({"toggle": "on"}, "light", {"on": False}.get), [("on", True, None)])
        self.assertEqual(commands({"toggle": "on", "hold": "none"}, "light", {"on": True}.get), [("on", False, "none")])

    def test_a_toggle_without_a_boolean_state_sends_nothing(self):
        for state in ({}, {"on": "unknown"}, {"on": 1}):
            with self.subTest(state=state), self.assertRaises(UnknownState):
                commands({"toggle": "on"}, "light", state.get)

    def test_a_set_sends_one_command_per_aspect_with_the_hold(self):
        sent = commands({"set": {"on": True, "brightness": 120}, "hold": "06:00"}, "light", {}.get)
        self.assertEqual(sorted(sent), [("brightness", 120, "06:00"), ("on", True, "06:00")])

    def test_a_release_hands_back_the_base_aspect(self):
        self.assertEqual(commands({"hold": "release"}, "lock", {}.get), [("locked", None, "release")])


if __name__ == "__main__":
    unittest.main()
