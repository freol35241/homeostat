"""A unit's holds (docs/design.md#holds): `ctx.hold`, `ctx.release`, what
`ready` releases, and what a command or a pause does to them, without a bus.
"""

import datetime
import unittest

from homeostat.automation import Context
from homeostat.testing import FakeSample, FakeSession
from houses import automation, house

HOLD = "home/hold/heating/utility/stove/on"
CMD = "home/cmd/utility/stove/on"

MANIFEST = automation(
    "heating",
    f"""
[bus.publishes]
stove = {{ key = "{HOLD}", priority = "automation" }}
any = {{ key = "home/hold/heating/**", priority = "automation" }}
start = {{ key = "{CMD}", priority = "automation" }}
""",
)


class LoggingSession(FakeSession):
    """A FakeSession that also keeps its puts and deletes in one `log`."""

    def __init__(self, unit, served):
        super().__init__(unit, served=served)
        self.log = []

    def put_json(self, key, value, *, at=None):
        super().put_json(key, value, at=at)
        self.log.append(("put", key))

    def delete(self, key):
        super().delete(key)
        self.log.append(("delete", key))


def context(test, mirror=()):
    """A context for unit `heating` with one hold and one cmd publish, and a
    bus whose mirror holds `mirror` as (key, record). The session is
    `test.session`."""
    test.session = LoggingSession("heating", dict(mirror))
    return Context("heating", house(test, {"units/heating.toml": MANIFEST}), session=test.session)


class HoldTest(unittest.TestCase):
    def test_a_hold_publishes_its_wish_under_the_units_name(self):
        ctx = context(self)
        until = datetime.datetime(2026, 11, 18, 6, tzinfo=datetime.timezone(datetime.timedelta(hours=1)))
        ctx.hold("stove", False, until=until, reason="not today")
        [(key, record)] = self.session.puts
        self.assertEqual(key, HOLD)
        self.assertEqual(
            {k: v for k, v in record.items() if k != "id"},
            {"value": False, "priority": "automation", "until": "2026-11-18T05:00:00Z", "reason": "not today"},
        )
        self.assertTrue(record["id"])

    def test_holding_the_same_wish_again_publishes_nothing(self):
        ctx = context(self)
        ctx.hold("stove", False)
        ctx.hold("stove", False)
        ctx.hold("stove", True)
        self.assertEqual([r["value"] for _, r in self.session.puts], [False, True])
        self.assertNotEqual(self.session.puts[0][1]["id"], self.session.puts[1][1]["id"], "a new wish, a new id")

    def test_a_restart_keeps_the_id_of_an_unchanged_wish(self):
        # The arbiter forwards when the applied hold's id changes, so a
        # holder's restart must not mint a new one for the same wish.
        kept = {"value": False, "priority": "automation", "id": "c0ffee01"}
        ctx = context(self, mirror=[(HOLD, kept)])
        ctx.hold("stove", False)
        self.assertEqual(self.session.puts, [])

    def test_a_wildcard_hold_names_its_slots(self):
        ctx = context(self)
        ctx.hold("any", 19.0, room="utility", entity="heat_pump", aspect="setpoint")
        self.assertEqual(self.session.puts[0][0], "home/hold/heating/utility/heat_pump/setpoint")
        with self.assertRaises(ValueError):
            ctx.hold("any", 19.0)

    def test_only_a_hold_publish_holds(self):
        ctx = context(self)
        with self.assertRaises(ValueError):
            ctx.hold("start", True)
        with self.assertRaises(ValueError):
            ctx.release("start")

    def test_an_until_needs_a_timezone_and_a_near_end(self):
        ctx = context(self)
        with self.assertRaises(ValueError):
            ctx.hold("stove", False, until=datetime.datetime(2026, 11, 18, 6))  # noqa: DTZ001
        with self.assertRaises(ValueError):
            ctx.hold("stove", False, until=datetime.datetime(2099, 1, 1, tzinfo=datetime.timezone.utc))
        self.assertEqual(self.session.puts, [])


class ReleaseTest(unittest.TestCase):
    def test_release_deletes_the_key_once(self):
        ctx = context(self)
        ctx.hold("stove", False)
        ctx.release("stove")
        ctx.release("stove")
        self.assertEqual(self.session.deletes, [HOLD])

    def test_a_command_releases_the_units_own_hold_on_the_aspect(self):
        # The command first, so the arbiter, which drops the hold with the
        # command, does not resolve the delete on its own first.
        ctx = context(self)
        ctx.hold("stove", False)
        ctx.publish("start", True)
        self.assertEqual(self.session.log[1:], [("put", CMD), ("delete", HOLD)])

    def test_a_hold_publish_is_not_published_raw(self):
        ctx = context(self)
        with self.assertRaises(ValueError):
            ctx.publish("stove", False)

    def test_ready_releases_what_was_not_asserted_again(self):
        old = {"value": True, "priority": "automation", "id": "0ld0ld01"}
        other = "home/hold/heating/utility/heat_pump/setpoint"
        ctx = context(self, mirror=[(HOLD, old), (other, {"value": 21, "priority": "automation", "id": "x"})])
        ctx.hold("stove", True)
        ctx.ready()
        self.assertEqual(self.session.deletes, [other])
        self.assertEqual(self.session.events, [])
        self.assertTrue(self.session.is_ready)

    def test_a_pause_releases_every_hold_and_holds_nothing_until_resumed(self):
        ctx = context(self)
        ctx.hold("stove", False)
        self.session.deliver(FakeSample("home/config/heating/enabled", False))
        self.assertEqual(self.session.deletes, [HOLD])
        ctx.hold("stove", False)
        self.assertEqual(len(self.session.puts), 1, "a paused automation holds nothing")


class HoldWhileTest(unittest.TestCase):
    """A guard with hysteresis: hold above 200, release below 180."""

    def step(self, ctx, flue):
        return ctx.hold_while("stove", False, hold=flue > 200, release=flue < 180, reason="flue hot")

    def test_it_holds_across_the_gap_and_releases_below_it(self):
        ctx = context(self)
        held = [self.step(ctx, flue) for flue in (150, 190, 210, 195, 185, 175, 190)]
        self.assertEqual(held, [False, False, True, True, True, False, False])
        self.assertEqual(len(self.session.puts), 1, "held once, not on every reading")
        self.assertEqual(self.session.puts[0][1]["reason"], "flue hot")
        self.assertEqual(self.session.deletes, [HOLD])

    def test_a_restart_inside_the_gap_keeps_the_hold(self):
        kept = {"value": False, "priority": "automation", "reason": "flue hot", "id": "c0ffee01"}
        ctx = context(self, mirror=[(HOLD, kept)])
        self.assertTrue(self.step(ctx, 190))
        self.assertEqual(self.session.puts, [])


if __name__ == "__main__":
    unittest.main()
