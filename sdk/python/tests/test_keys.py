"""Key builders: segment validation and the cmd envelope, no bus.

Run: uv run --no-project --with-editable sdk/python python -m unittest discover sdk/python/tests
"""

import unittest

from homeostat import keys


class SegmentTest(unittest.TestCase):
    def test_accepts_the_core_rule(self):
        for name in ("on", "GT3_2_raw", "regulation.fixed_power", "lamp-1", "0x00158d0001a2b3c4"):
            self.assertTrue(keys.valid_segment(name), name)

    def test_refuses_what_the_core_refuses(self):
        # "**" would put on a wildcard and fan out to every aspect subscriber;
        # "" and "#" raise inside zenoh; "/" breaks the key schema.
        for name in ("", "**", "*", "a/b", "#", "$x", "a b", "å", ".", "..", None, 3):
            self.assertFalse(keys.valid_segment(name), repr(name))

    def test_builders_raise_on_a_bad_segment(self):
        self.assertEqual(keys.state_key("kitchen", "lamp", "on"), "home/state/kitchen/lamp/on")
        with self.assertRaises(ValueError):
            keys.state_key("kitchen", "lamp", "**")
        with self.assertRaises(ValueError):
            keys.state_key("kitchen", "lamp", "")
        with self.assertRaises(ValueError):
            keys.cmd_key("kitchen", "lamp", "a/b")
        with self.assertRaises(ValueError):
            keys.arbiter_key("kitchen", "**", "on")
        with self.assertRaises(ValueError):
            keys.cmd_keyexpr("*", "lamp")
        with self.assertRaises(ValueError):
            keys.history_key("state", "lamp", "#")
        with self.assertRaises(ValueError):
            keys.discovery_key("")
        with self.assertRaises(ValueError):
            keys.config_key("unit", "a b")

    def test_builders_keep_their_shapes(self):
        self.assertEqual(keys.cmd_keyexpr("r", "e"), "home/cmd/r/e/**")
        self.assertEqual(keys.arbiter_keyexpr("r", "e"), "home/arbiter/r/e/**")
        self.assertEqual(keys.config_keyexpr("u"), "home/config/u/*")
        self.assertEqual(keys.history_key("cmd", "e", "a"), "home/history/cmd/e/a")
        self.assertEqual(keys.liveliness_key("u"), "home/health/u/alive")
        self.assertEqual(keys.health_event_key("u"), "home/health/u/event")


class EnvelopeTest(unittest.TestCase):
    def test_returns_the_value(self):
        self.assertEqual(keys.parse_cmd_envelope({"value": 21.5, "priority": "manual", "actor": "x"}), 21.5)
        self.assertIs(keys.parse_cmd_envelope({"value": None, "priority": "automation"}), None)

    def test_rejects_non_envelopes(self):
        for payload in (
            True,
            42,
            "on",
            [1],
            {},
            {"value": 1},
            {"priority": "manual"},
            {"value": 1, "priority": "urgent"},
            {"value": 1, "priority": None},
        ):
            with self.assertRaises(ValueError, msg=repr(payload)):
                keys.parse_cmd_envelope(payload)


class CorrelationIdTest(unittest.TestCase):
    def test_every_built_envelope_carries_one(self):
        first = keys.cmd_envelope(21.5, "manual", "dashboard")
        second = keys.cmd_envelope(21.5, "manual", "dashboard")
        self.assertNotEqual(first["id"], second["id"], "two commands, two ids")
        self.assertEqual(keys.cmd_envelope_id(first), first["id"])

    def test_a_caller_may_supply_its_own(self):
        envelope = keys.cmd_envelope(True, "manual", "dashboard", cmd_id="c0ffee01")
        self.assertEqual(envelope["id"], "c0ffee01")

    def test_an_envelope_without_one_reads_as_none(self):
        # The id is optional on the wire: a hand-rolled publisher, or an
        # envelope minted before ids existed, is still a valid command.
        self.assertIsNone(keys.cmd_envelope_id({"value": 1, "priority": "manual"}))
        self.assertEqual(keys.parse_cmd_envelope({"value": 1, "priority": "manual"}), 1)

    def test_a_payload_that_is_not_an_object_has_no_id(self):
        for payload in (None, "on", 42, [1], True):
            self.assertIsNone(keys.cmd_envelope_id(payload), repr(payload))


if __name__ == "__main__":
    unittest.main()
