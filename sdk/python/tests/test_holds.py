"""A unit's holds (docs/design.md#holds): `ctx.hold`, `ctx.release`, what
`ready` releases, and what a command or a pause does to them, without a bus.
"""

import datetime
import threading
import types
import unittest

from homeostat.automation import Context
from homeostat.house import params_of

HOLD = "home/hold/heating/utility/stove/on"
CMD = "home/cmd/utility/stove/on"


def context(mirror=()):
    """A context for unit `heating` with one hold and one cmd publish, and a
    bus whose mirror holds `mirror` as (key, record)."""
    ctx = Context.__new__(Context)
    ctx.unit = "heating"
    ctx._lock = threading.Lock()
    ctx._param_specs = params_of({"unit": {"kind": "automation"}})
    ctx._param_values = {"enabled": True}
    ctx._publishes = {
        "stove": {"key": "home/hold/heating/utility/stove/on", "priority": "automation"},
        "any": {"key": "home/hold/heating/**", "priority": "automation"},
        "start": {"key": CMD, "priority": "automation"},
    }
    ctx._zones = {}
    ctx._entities = []
    ctx._holds = None
    ctx._asserted = set()
    ctx._holds_lock = threading.Lock()
    ctx.puts, ctx.deletes, ctx.events, ctx.log = [], [], [], []

    def put(key, value, at=None):
        ctx.puts.append((key, value))
        ctx.log.append(("put", key))

    def delete(key):
        ctx.deletes.append(key)
        ctx.log.append(("delete", key))

    ctx._session = types.SimpleNamespace(
        put_json=put,
        delete=delete,
        get_json=lambda expr, timeout_s=None: list(mirror),
        health_event=lambda kind, **fields: ctx.events.append(kind),
        ready=lambda: ctx.events.append("ready"),
    )
    return ctx


class HoldTest(unittest.TestCase):
    def test_a_hold_publishes_its_wish_under_the_units_name(self):
        ctx = context()
        until = datetime.datetime(2026, 11, 18, 6, tzinfo=datetime.timezone(datetime.timedelta(hours=1)))
        ctx.hold("stove", False, until=until, reason="not today")
        [(key, record)] = ctx.puts
        self.assertEqual(key, HOLD)
        self.assertEqual(
            {k: v for k, v in record.items() if k != "id"},
            {"value": False, "priority": "automation", "until": "2026-11-18T05:00:00Z", "reason": "not today"},
        )
        self.assertTrue(record["id"])

    def test_holding_the_same_wish_again_publishes_nothing(self):
        ctx = context()
        ctx.hold("stove", False)
        ctx.hold("stove", False)
        ctx.hold("stove", True)
        self.assertEqual([r["value"] for _, r in ctx.puts], [False, True])
        self.assertNotEqual(ctx.puts[0][1]["id"], ctx.puts[1][1]["id"], "a new wish, a new id")

    def test_a_restart_keeps_the_id_of_an_unchanged_wish(self):
        # The arbiter forwards when the applied hold's id changes, so a
        # holder's restart must not mint a new one for the same wish.
        kept = {"value": False, "priority": "automation", "id": "c0ffee01"}
        ctx = context(mirror=[(HOLD, kept)])
        ctx.hold("stove", False)
        self.assertEqual(ctx.puts, [])

    def test_a_wildcard_hold_names_its_slots(self):
        ctx = context()
        ctx.hold("any", 19.0, room="utility", entity="heat_pump", aspect="setpoint")
        self.assertEqual(ctx.puts[0][0], "home/hold/heating/utility/heat_pump/setpoint")
        with self.assertRaises(ValueError):
            ctx.hold("any", 19.0)

    def test_only_a_hold_publish_holds(self):
        ctx = context()
        with self.assertRaises(ValueError):
            ctx.hold("start", True)
        with self.assertRaises(ValueError):
            ctx.release("start")

    def test_an_until_needs_a_timezone_and_a_near_end(self):
        ctx = context()
        with self.assertRaises(ValueError):
            ctx.hold("stove", False, until=datetime.datetime(2026, 11, 18, 6))  # noqa: DTZ001
        with self.assertRaises(ValueError):
            ctx.hold("stove", False, until=datetime.datetime(2099, 1, 1, tzinfo=datetime.timezone.utc))
        self.assertEqual(ctx.puts, [])


class ReleaseTest(unittest.TestCase):
    def test_release_deletes_the_key_once(self):
        ctx = context()
        ctx.hold("stove", False)
        ctx.release("stove")
        ctx.release("stove")
        self.assertEqual(ctx.deletes, [HOLD])

    def test_a_command_releases_the_units_own_hold_on_the_aspect(self):
        # The command first, so the arbiter, which drops the hold with the
        # command, does not resolve the delete on its own first.
        ctx = context()
        ctx.hold("stove", False)
        ctx.publish("start", True)
        self.assertEqual(ctx.log[1:], [("put", CMD), ("delete", HOLD)])

    def test_a_hold_publish_is_not_published_raw(self):
        ctx = context()
        with self.assertRaises(ValueError):
            ctx.publish("stove", False)

    def test_ready_releases_what_was_not_asserted_again(self):
        old = {"value": True, "priority": "automation", "id": "0ld0ld01"}
        other = "home/hold/heating/utility/heat_pump/setpoint"
        ctx = context(mirror=[(HOLD, old), (other, {"value": 21, "priority": "automation", "id": "x"})])
        ctx.hold("stove", True)
        ctx.ready()
        self.assertEqual(ctx.deletes, [other])
        self.assertEqual(ctx.events, ["ready"])

    def test_a_pause_releases_every_hold_and_holds_nothing_until_resumed(self):
        ctx = context()
        ctx.hold("stove", False)
        ctx._param_specs = {"enabled": {}}
        sample = lambda raw: types.SimpleNamespace(
            key_expr="home/config/heating/enabled", payload=types.SimpleNamespace(to_bytes=lambda: raw)
        )
        ctx._redeliveries = []
        ctx._on_config(sample(b"false"))
        self.assertEqual(ctx.deletes, [HOLD])
        ctx.hold("stove", False)
        self.assertEqual(len(ctx.puts), 1, "a paused automation holds nothing")


if __name__ == "__main__":
    unittest.main()
