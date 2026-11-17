"""Pausing an automation: the implicit `enabled` parameter, dropped commands,
the paused/resumed events and the redelivery on resume, without a bus.

Run: uv run --no-project --with-editable sdk/python python -m unittest discover sdk/python/tests
"""

import datetime
import unittest
from unittest import mock

import zenoh
from homeostat.automation import Context
from homeostat.house import params_of
from homeostat.testing import FakeSample, FakeSession
from houses import automation, house
from houses import entity as house_entity

CMD = "home/cmd/kitchen/lamp/on"
STATE = "home/state/global/fused/temperature"
# The router's stamps mark a sample live. A press replayed after a restart
# carries the replaying publisher's.
ROUTER = str(zenoh.TimestampId(b"\x01"))


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


def context(test, enabled=True, current=(), subscribes=None, files=()):
    """A context with one cmd and one state publish, `enabled` as given,
    and a bus whose mirror holds `current` as (key, value, stamp). The
    session is `test.session`.

    `subscribes` is the manifest's `[bus.subscribes]`, binding to key, and
    `files` are further files of the house.
    """
    subscribes = subscribes or {"lamp": CMD.replace("cmd", "state")}
    bindings = "".join(f'{name} = "{key}"\n' for name, key in subscribes.items())
    manifest = automation(
        "porch",
        f"""
[bus.publishes]
lamp = {{ key = "{CMD}", priority = "automation" }}
fused = {{ key = "{STATE}" }}
[bus.subscribes]
{bindings}""",
    )
    test.session = FakeSession(
        "porch", served={"home/config/porch/enabled": enabled}, routers=[ROUTER]
    )
    for key, value, at in current:
        test.session.serve(key, value, stamp=at)
    root = house(test, {"units/porch.toml": manifest, **dict(files)})
    return Context("porch", root, session=test.session)


def config_sample(value):
    return FakeSample("home/config/porch/enabled", value)


class InlineThread:
    """Runs the target when started, so a test sees its effect at once."""

    def __init__(self, target, daemon=None):
        self.target = target

    def start(self):
        self.target()


class PublishTest(unittest.TestCase):
    def test_a_paused_automation_drops_its_commands(self):
        ctx = context(self, enabled=False)
        ctx.publish("lamp", True)
        self.assertEqual(self.session.puts, [])

    def test_a_paused_automation_still_publishes_state(self):
        ctx = context(self, enabled=False)
        ctx.publish("fused", 21.5)
        self.assertEqual(self.session.puts, [(STATE, 21.5)])

    def test_an_enabled_automation_sends_its_commands(self):
        ctx = context(self, enabled=True)
        ctx.publish("lamp", True)
        self.assertEqual(len(self.session.puts), 1)
        self.assertEqual(self.session.puts[0][0], CMD)
        self.assertEqual(self.session.puts[0][1]["actor"], "porch")


class PauseEventsTest(unittest.TestCase):
    def test_each_change_emits_an_event_and_repeats_do_not(self):
        ctx = context(self, enabled=True)
        with mock.patch("homeostat.automation.threading.Thread", InlineThread):
            self.session.deliver(config_sample(b"false"))
            self.session.deliver(config_sample(b"false"))
            self.session.deliver(config_sample(b"true"))
            self.session.deliver(config_sample(b"true"))
        self.assertEqual([e["kind"] for e in self.session.events], ["paused", "resumed"])
        self.assertFalse(ctx.paused)

    def test_paused_reads_the_live_value(self):
        ctx = context(self, enabled=True)
        with mock.patch("homeostat.automation.threading.Thread", InlineThread):
            self.session.deliver(config_sample(b"false"))
        self.assertTrue(ctx.paused)


class RedeliverTest(unittest.TestCase):
    KEY = "home/state/kitchen/lamp/on"

    def test_resuming_delivers_the_current_values_again(self):
        now = stamp(5)
        ctx = context(self, enabled=True, current=[(self.KEY, True, now)])
        delivered = []
        ctx.subscribe("lamp", lambda key, value: delivered.append(value))
        self.assertEqual(delivered, [True], "the catch-up at subscribe")
        with mock.patch("homeostat.automation.threading.Thread", InlineThread):
            self.session.deliver(config_sample(b"false"))
            self.session.deliver(config_sample(b"true"))
        self.assertEqual(delivered, [True, True], "delivered again on resume")

    def test_a_value_older_than_the_one_delivered_is_not_redelivered(self):
        ctx = context(self, enabled=True, current=[(self.KEY, True, stamp(5))])
        delivered = []
        ctx.subscribe("lamp", lambda key, value: delivered.append(value))
        # The mirror answers with an older value than the one delivered,
        # as it would if the get raced a live sample.
        self.session.serve(self.KEY, False, stamp=stamp(60))
        with mock.patch("homeostat.automation.threading.Thread", InlineThread):
            self.session.deliver(config_sample(b"false"))
            self.session.deliver(config_sample(b"true"))
        self.assertEqual(delivered, [True])


class DecisionTest(unittest.TestCase):
    """`decision` reports why a rule acted, on change only, and marks a
    paused automation's outcome."""

    def context(self, enabled=True):
        return context(self, enabled=enabled)

    def test_only_a_change_of_outcome_is_reported(self):
        ctx = self.context()
        for _ in range(3):
            ctx.decision("night_off", "waiting: someone is home")
        ctx.decision("night_off", "turning lights off")
        self.assertEqual(
            self.session.events,
            [
                {"kind": "decision", "rule": "night_off", "outcome": "waiting: someone is home"},
                {"kind": "decision", "rule": "night_off", "outcome": "turning lights off"},
            ],
        )

    def test_rules_are_tracked_apart(self):
        ctx = self.context()
        ctx.decision("night_off", "outside the window")
        ctx.decision("morning_on", "outside the window")
        self.assertEqual(len(self.session.events), 2)

    def test_a_paused_outcome_says_so(self):
        ctx = self.context(enabled=False)
        ctx.decision("night_off", "turning lights off")
        self.assertEqual(
            self.session.events[-1]["outcome"], "turning lights off (paused)"
        )

    def test_a_resumed_rule_reports_its_outcome_again(self):
        ctx = self.context()
        ctx.decision("night_off", "turning lights off")
        with mock.patch("homeostat.automation.threading.Thread", InlineThread):
            self.session.deliver(config_sample(b"false"))
            ctx.decision("night_off", "turning lights off")
            self.session.deliver(config_sample(b"true"))
            ctx.decision("night_off", "turning lights off")
        self.assertEqual(
            [e["outcome"] for e in self.session.events if e["kind"] == "decision"],
            ["turning lights off", "turning lights off (paused)", "turning lights off"],
        )


INPUTS = {"inputs": "home/state/**"}


class PressTest(unittest.TestCase):
    """A button's `action` is a press, not a state
    (docs/design.md#buttons)."""

    ACTION = "home/state/hall/remote/action"

    def remote(self, capability):
        """The house files that make `remote` an entity of `capability`."""
        return {
            "units/z2m.toml": 'schema = 1\n[unit]\nname = "z2m"\nkind = "adapter"\n'
            '[entities]\ndir = "entities/z2m/"\n',
            "entities/z2m/remote.toml": house_entity("hall", capability),
        }

    def test_the_last_press_is_not_caught_up_or_redelivered(self):
        ctx = context(
            self,
            current=[(self.ACTION, "single", stamp(5)), (STATE, 21.0, stamp(5))],
            subscribes=INPUTS,
            files=self.remote("button"),
        )
        delivered = []
        ctx.subscribe("inputs", lambda key, value: delivered.append(key))
        with mock.patch("homeostat.automation.threading.Thread", InlineThread):
            self.session.deliver(config_sample(b"false"))
            self.session.deliver(config_sample(b"true"))
        self.assertEqual(delivered, [STATE, STATE])

    def test_an_action_on_another_capability_is_state(self):
        # z2m passes `action` through under its own name on a device bound
        # as a sensor; that entity is not a button.
        ctx = context(
            self,
            current=[(self.ACTION, "single", stamp(5))],
            subscribes=INPUTS,
            files=self.remote("sensor"),
        )
        delivered = []
        ctx.subscribe("inputs", lambda key, value: delivered.append(key))
        self.assertEqual(delivered, [self.ACTION])

    def test_a_replayed_press_is_not_a_press(self):
        ctx = context(self, subscribes=INPUTS, files=self.remote("button"))
        delivered = []
        ctx.subscribe("inputs", lambda key, value: delivered.append(value))
        replayed = zenoh.Timestamp(stamp(60), zenoh.TimestampId(b"\x02"))
        self.session.deliver(FakeSample(self.ACTION, b'"single"', timestamp=replayed))
        self.session.deliver(FakeSample(self.ACTION, b'"double"'))
        self.assertEqual(delivered, ["double"])

    def test_a_house_that_cannot_be_read_counts_an_action_as_a_press(self):
        ctx = context(
            self,
            current=[(self.ACTION, "single", stamp(5))],
            subscribes=INPUTS,
            files={"units/half.toml": "[unit\nname ="},
        )
        delivered = []
        ctx.subscribe("inputs", lambda key, value: delivered.append(key))
        self.assertEqual(delivered, [])
        self.assertEqual([e.get("reason") for e in self.session.events], ["house-unreadable"])


if __name__ == "__main__":
    unittest.main()
