"""Key builders: segment validation and the cmd envelope, no bus.

Run: uv run --no-project --with-editable sdk/python python -m unittest discover sdk/python/tests
"""

import datetime
import json
import unittest
from pathlib import Path

from homeostat import keys
from homeostat.automation import _typed


class SegmentTest(unittest.TestCase):
    def test_the_key_segment_rule_is_the_cores(self):
        # tests/fixtures/key_segments.json: what the core accepts as a key
        # segment, the SDK builds keys from.
        cases = json.loads((Path(__file__).resolve().parents[3] / "tests/fixtures/key_segments.json").read_text())
        for name in cases["accepted"]:
            self.assertTrue(keys.valid_segment(name), repr(name))
        for name in cases["refused"]:
            self.assertFalse(keys.valid_segment(name), repr(name))

    def test_refuses_a_non_string(self):
        for name in (None, 3):
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
            {"value": 1, "priority": "manual", "options": [1]},
            {"value": 1, "priority": "manual", "options": "transition"},
        ):
            with self.assertRaises(ValueError, msg=repr(payload)):
                keys.parse_cmd_envelope(payload)


class OptionsTest(unittest.TestCase):
    def test_a_built_envelope_carries_options_only_when_given(self):
        self.assertNotIn("options", keys.cmd_envelope(1, "automation", "u"))
        self.assertNotIn("options", keys.cmd_envelope(1, "automation", "u", options={}))
        envelope = keys.cmd_envelope(1, "automation", "u", options={"transition": 1})
        self.assertEqual(envelope["options"], {"transition": 1})

    def test_options_must_be_an_object_with_string_keys(self):
        for options in ([1], {1: 1}):
            with self.assertRaises(ValueError, msg=repr(options)):
                keys.cmd_envelope(1, "automation", "u", options=options)

    def test_parsed_options_default_to_empty(self):
        payload = {"value": 1, "priority": "manual"}
        keys.parse_cmd_envelope(payload)
        self.assertEqual(keys.cmd_envelope_options(payload), {})
        payload["options"] = {"transition": 2}
        keys.parse_cmd_envelope(payload)
        self.assertEqual(keys.cmd_envelope_options(payload), {"transition": 2})


class HoldTest(unittest.TestCase):
    """The `hold` field of a manual-band command (docs/design.md#hold-intents)."""

    def test_a_built_envelope_carries_hold_only_when_given(self):
        self.assertNotIn("hold", keys.cmd_envelope(1, "manual", "u"))
        for hold in ("none", "report", "2026-11-17T06:00:00+01:00", "2026-11-17T05:00:00Z"):
            envelope = keys.cmd_envelope(1, "manual", "u", hold=hold)
            self.assertEqual(envelope["hold"], hold)
            self.assertEqual(keys.parse_cmd_envelope(envelope), 1)
            self.assertEqual(keys.cmd_envelope_hold(envelope), hold)

    def test_a_release_carries_no_value(self):
        envelope = keys.cmd_envelope(None, "manual", "u", hold="release")
        self.assertNotIn("value", envelope)
        self.assertIsNone(keys.parse_cmd_envelope(envelope))
        with self.assertRaises(ValueError):
            keys.cmd_envelope(True, "manual", "u", hold="release")

    def test_only_a_release_may_leave_out_the_value(self):
        with self.assertRaises(ValueError):
            keys.parse_cmd_envelope({"priority": "manual", "hold": "none"})

    def test_a_hold_below_the_manual_band_is_refused(self):
        for priority in ("automation", "agent", "family"):
            with self.assertRaises(ValueError, msg=priority):
                keys.cmd_envelope(1, priority, "u", hold="none")
            with self.assertRaises(ValueError, msg=priority):
                keys.parse_cmd_envelope({"value": 1, "priority": priority, "hold": "none"})

    def test_a_hold_is_a_word_or_an_instant_with_an_offset(self):
        for hold in ("forever", "24:00", "6:00", "06:00:00", "2026-11-17T06:00:00", "", 30, None, ["none"]):
            with self.assertRaises(ValueError, msg=repr(hold)):
                keys.parse_cmd_envelope({"value": 1, "priority": "manual", "hold": hold})

    def test_an_instant_is_at_most_the_horizon_ahead(self):
        now = datetime.datetime.now(datetime.timezone.utc)
        near = (now + keys.HOLD_HORIZON - datetime.timedelta(hours=1)).isoformat()
        far = (now + keys.HOLD_HORIZON + datetime.timedelta(hours=1)).isoformat()
        self.assertEqual(keys.parse_cmd_envelope({"value": 1, "priority": "manual", "hold": near}), 1)
        for hold in (far, "2400-01-01T00:00:00Z", "9999-12-31T23:59:59+00:00"):
            with self.assertRaises(ValueError, msg=hold):
                keys.parse_cmd_envelope({"value": 1, "priority": "manual", "hold": hold})

    def test_the_time_of_day_grammar_is_the_cores(self):
        # tests/fixtures/time_of_day.json: what the core accepts, the SDK
        # reads, both as a hold intent and as a `time` parameter.
        cases = json.loads((Path(__file__).resolve().parents[3] / "tests/fixtures/time_of_day.json").read_text())
        for s in cases["valid"]:
            self.assertTrue(keys.is_time_of_day(s), s)
            self.assertEqual(_typed("time", s).strftime("%H:%M"), s, "a `time` parameter reads it")
        for s in cases["invalid"]:
            self.assertFalse(keys.is_time_of_day(s), repr(s))

    def test_a_time_of_day_is_a_hold_intent_and_no_instant(self):
        for hold in ("00:00", "06:00", "23:59"):
            self.assertEqual(keys.parse_cmd_envelope({"value": 1, "priority": "manual", "hold": hold}), 1)
            self.assertIsNone(keys.hold_instant(hold))
        with self.assertRaises(ValueError):
            keys.cmd_envelope(1, "automation", "x", hold="06:00")

    def test_an_instant_reads_back_timezone_aware(self):
        instant = keys.hold_instant("2026-11-17T06:00:00+01:00")
        self.assertEqual(instant.isoformat(), "2026-11-17T06:00:00+01:00")
        self.assertIsNone(keys.hold_instant("none"))

    def test_hold_keys(self):
        self.assertEqual(keys.hold_key("arbiter", "r", "e", "a"), "home/hold/arbiter/r/e/a")
        self.assertEqual(keys.hold_keyexpr("arbiter"), "home/hold/arbiter/**")


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
        # The id is optional on the wire: an envelope from a publisher that
        # stamps none is still a valid command.
        self.assertIsNone(keys.cmd_envelope_id({"value": 1, "priority": "manual"}))
        self.assertEqual(keys.parse_cmd_envelope({"value": 1, "priority": "manual"}), 1)

    def test_a_payload_that_is_not_an_object_has_no_id(self):
        for payload in (None, "on", 42, [1], True):
            self.assertIsNone(keys.cmd_envelope_id(payload), repr(payload))


if __name__ == "__main__":
    unittest.main()
