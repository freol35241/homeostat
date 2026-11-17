"""Attribution of a device's changes (docs/design.md#device-reported-causes), without a bus."""

import unittest

from homeostat.causes import CAUSE_WINDOW_S, ECHO_WINDOW_S, Attribution


class Clock:
    def __init__(self):
        self.now = 100.0

    def __call__(self):
        return self.now


class AttributionTest(unittest.TestCase):
    """`relay` and `relay_2` report causes, `sonoff` reports none and is
    declared `uncommanded = "person"`, `bare` reports none."""

    def setUp(self):
        self.clock = Clock()
        self.a = Attribution(
            person={"relay": {"button"}, "relay_2": {"button"}, "sonoff": set(), "bare": set()},
            uncommanded={"sonoff"},
            clock=self.clock,
        )
        for entity in ("relay", "relay_2", "sonoff", "bare"):
            self.assertIsNone(self.a.changed(entity, False), "the first value is a baseline")

    def test_a_person_cause_makes_the_change_the_familys(self):
        self.a.cause("relay", "button")
        self.assertEqual(self.a.changed("relay", True), ("person", "button"))

    def test_any_other_cause_is_a_rule(self):
        self.a.cause("relay", "valve_timeout")
        self.assertEqual(self.a.changed("relay", True), ("rule", "valve_timeout"))

    def test_each_relay_has_its_own_cause(self):
        self.a.cause("relay_2", "button")
        self.assertEqual(self.a.changed("relay", True), ("unattributed", None))
        self.assertEqual(self.a.changed("relay_2", True), ("person", "button"))

    def test_a_spent_cause_says_nothing(self):
        self.a.cause("relay", "button")
        self.clock.now += CAUSE_WINDOW_S + 1
        self.assertEqual(self.a.changed("relay", True), ("unattributed", None))

    def test_the_adapters_own_command_is_an_echo(self):
        self.a.commanded("relay", True)
        self.assertIsNone(self.a.changed("relay", True))
        self.a.commanded("relay", False)
        self.clock.now += ECHO_WINDOW_S + 1
        self.assertEqual(self.a.changed("relay", False), ("unattributed", None), "a late change is not an echo")

    def test_two_commands_before_the_first_echo_are_two_echoes(self):
        self.a.commanded("sonoff", True)
        self.a.commanded("sonoff", False)
        self.assertIsNone(self.a.changed("sonoff", True))
        self.assertIsNone(self.a.changed("sonoff", False))
        self.assertEqual(self.a.changed("sonoff", True), ("person", None), "each command explains one change")

    def test_a_cause_wins_over_a_command_the_device_ignored(self):
        self.a.commanded("relay", True)
        self.a.cause("relay", "button")
        self.assertEqual(self.a.changed("relay", True), ("person", "button"))

    def test_a_connect_sets_a_new_baseline(self):
        self.a.cause("relay", "button")
        self.a.connected(["relay"])
        self.assertIsNone(self.a.changed("relay", True), "the state after a reconnect is not a change")
        self.assertIsNone(self.a.changed("relay", True), "nor is the same value again")

    def test_firmware_without_causes_is_a_person_only_when_declared(self):
        self.assertEqual(self.a.changed("sonoff", True), ("person", None))
        self.assertEqual(self.a.changed("bare", True), ("unattributed", None))

    def test_an_untracked_entity_is_never_attributed(self):
        self.assertIsNone(self.a.changed("lamp", True))
        self.assertIsNone(self.a.changed("lamp", False))


if __name__ == "__main__":
    unittest.main()
