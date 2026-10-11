"""Pausing an automation: the implicit `enabled` parameter, dropped commands,
the paused/resumed events and the redelivery on resume, without a bus.

Run: uv run --no-project --with-editable sdk/python python -m unittest discover sdk/python/tests
"""

import datetime
import threading
import types
import unittest
from unittest import mock

from homeostat.automation import Context
from homeostat.house import params_of

CMD = "home/cmd/kitchen/lamp/on"
STATE = "home/state/global/fused/temperature"


def stamp(seconds_ago):
    """A stamp ordered by time, as zenoh orders the router's."""
    return datetime.datetime.now(datetime.UTC) - datetime.timedelta(seconds=seconds_ago)


class ParamsOfTest(unittest.TestCase):
    def test_an_automation_has_enabled_without_declaring_it(self):
        params = params_of({"unit": {"kind": "automation"}})
        self.assertEqual(
            params["enabled"],
            {"type": "bool", "default": True, "editable_by": "family"},
        )

    def test_a_declared_enabled_takes_its_place(self):
        declared = {"type": "bool", "default": True, "editable_by": "owner"}
        params = params_of({"unit": {"kind": "automation"}, "params": {"enabled": declared}})
        self.assertEqual(params["enabled"], declared)

    def test_adapters_and_services_have_no_enabled(self):
        for kind in ("adapter", "service"):
            self.assertNotIn("enabled", params_of({"unit": {"kind": kind}}))


def context(enabled=True, current=()):
    """A context with one cmd and one state publish, `enabled` as given,
    and a bus whose mirror holds `current` as (key, value, stamp)."""
    ctx = Context.__new__(Context)
    ctx.unit = "porch"
    ctx._lock = threading.Lock()
    ctx._param_specs = params_of({"unit": {"kind": "automation"}})
    ctx._param_values = {"enabled": enabled}
    ctx._publishes = {
        "lamp": {"key": CMD, "priority": "automation"},
        "fused": {"key": STATE, "priority": None},
    }
    ctx._zones = {}
    ctx._entities = []
    ctx._subscribes = {"lamp": CMD.replace("cmd", "state")}
    ctx._subs = []
    ctx._redeliveries = []
    ctx.puts = []
    ctx.events = []
    ctx.current = list(current)
    ctx._session = types.SimpleNamespace(
        put_json=lambda key, value: ctx.puts.append((key, value)),
        health_event=lambda kind, **fields: ctx.events.append(kind),
        subscribe=lambda expr, callback: object(),
        get_json_stamped=lambda expr: [(k, v, 0.0, s) for k, v, s in ctx.current],
        sample_age=lambda sample: 0.0,
    )
    return ctx


def config_sample(value):
    return types.SimpleNamespace(
        key_expr="home/config/porch/enabled",
        payload=types.SimpleNamespace(to_bytes=lambda: value),
    )


class InlineThread:
    """Runs the target when started, so a test sees its effect at once."""

    def __init__(self, target, daemon=None):
        self.target = target

    def start(self):
        self.target()


class PublishTest(unittest.TestCase):
    def test_a_paused_automation_drops_its_commands(self):
        ctx = context(enabled=False)
        ctx.publish("lamp", True)
        self.assertEqual(ctx.puts, [])

    def test_a_paused_automation_still_publishes_state(self):
        ctx = context(enabled=False)
        ctx.publish("fused", 21.5)
        self.assertEqual(ctx.puts, [(STATE, 21.5)])

    def test_an_enabled_automation_sends_its_commands(self):
        ctx = context(enabled=True)
        ctx.publish("lamp", True)
        self.assertEqual(len(ctx.puts), 1)
        self.assertEqual(ctx.puts[0][0], CMD)
        self.assertEqual(ctx.puts[0][1]["actor"], "porch")


class PauseEventsTest(unittest.TestCase):
    def test_each_change_emits_an_event_and_repeats_do_not(self):
        ctx = context(enabled=True)
        with mock.patch("homeostat.automation.threading.Thread", InlineThread):
            ctx._on_config(config_sample(b"false"))
            ctx._on_config(config_sample(b"false"))
            ctx._on_config(config_sample(b"true"))
            ctx._on_config(config_sample(b"true"))
        self.assertEqual(ctx.events, ["paused", "resumed"])
        self.assertFalse(ctx.paused)

    def test_paused_reads_the_live_value(self):
        ctx = context(enabled=True)
        with mock.patch("homeostat.automation.threading.Thread", InlineThread):
            ctx._on_config(config_sample(b"false"))
        self.assertTrue(ctx.paused)


class RedeliverTest(unittest.TestCase):
    KEY = "home/state/kitchen/lamp/on"

    def test_resuming_delivers_the_current_values_again(self):
        now = stamp(5)
        ctx = context(enabled=True, current=[(self.KEY, True, now)])
        delivered = []
        ctx.subscribe("lamp", lambda key, value: delivered.append(value))
        self.assertEqual(delivered, [True], "the catch-up at subscribe")
        with mock.patch("homeostat.automation.threading.Thread", InlineThread):
            ctx._on_config(config_sample(b"false"))
            ctx._on_config(config_sample(b"true"))
        self.assertEqual(delivered, [True, True], "delivered again on resume")

    def test_a_value_older_than_the_one_delivered_is_not_redelivered(self):
        ctx = context(enabled=True, current=[(self.KEY, True, stamp(5))])
        delivered = []
        ctx.subscribe("lamp", lambda key, value: delivered.append(value))
        # The mirror answers with an older value than the one delivered,
        # as it would if the get raced a live sample.
        ctx.current = [(self.KEY, False, stamp(60))]
        with mock.patch("homeostat.automation.threading.Thread", InlineThread):
            ctx._on_config(config_sample(b"false"))
            ctx._on_config(config_sample(b"true"))
        self.assertEqual(delivered, [True])


class DecisionTest(unittest.TestCase):
    """`decision` reports why a rule acted, on change only, and marks a
    paused automation's outcome."""

    def context(self, enabled=True):
        ctx = context(enabled=enabled)
        ctx._decisions = {}
        ctx.emitted = []
        ctx._session.health_event = lambda kind, **fields: ctx.emitted.append((kind, fields))
        return ctx

    def test_only_a_change_of_outcome_is_reported(self):
        ctx = self.context()
        for _ in range(3):
            ctx.decision("night_off", "waiting: someone is home")
        ctx.decision("night_off", "turning lights off")
        self.assertEqual(
            ctx.emitted,
            [
                ("decision", {"rule": "night_off", "outcome": "waiting: someone is home"}),
                ("decision", {"rule": "night_off", "outcome": "turning lights off"}),
            ],
        )

    def test_rules_are_tracked_apart(self):
        ctx = self.context()
        ctx.decision("night_off", "outside the window")
        ctx.decision("morning_on", "outside the window")
        self.assertEqual(len(ctx.emitted), 2)

    def test_a_paused_outcome_says_so(self):
        ctx = self.context(enabled=False)
        ctx.decision("night_off", "turning lights off")
        self.assertEqual(
            ctx.emitted[-1][1]["outcome"], "turning lights off (paused)"
        )

    def test_a_resumed_rule_reports_its_outcome_again(self):
        ctx = self.context()
        ctx.decision("night_off", "turning lights off")
        with mock.patch("homeostat.automation.threading.Thread", InlineThread):
            ctx._on_config(config_sample(b"false"))
            ctx.decision("night_off", "turning lights off")
            ctx._on_config(config_sample(b"true"))
            ctx.decision("night_off", "turning lights off")
        self.assertEqual(
            [f["outcome"] for kind, f in ctx.emitted if kind == "decision"],
            ["turning lights off", "turning lights off (paused)", "turning lights off"],
        )


if __name__ == "__main__":
    unittest.main()
