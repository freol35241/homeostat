"""The arbiter's rule, without a bus: who holds an aspect, and for how long.

Also which entities it arbitrates, read from the grant table.
"""

import json
import sys
import unittest
from pathlib import Path
from types import SimpleNamespace

sys.path.insert(0, str(Path(__file__).resolve().parents[2] / "adapters"))

from arbiter import GRANTS_KEY, PENDING_KEY, Bindings, Leases, bindings

LAMP = ("hall", "lamp", "on")
HOLD_S = 60.0


class Clock:
    def __init__(self):
        self.now = 1000.0

    def __call__(self):
        return self.now


WALL = 1_700_000_000.0  # 2023-11-14T22:13:20Z


class LeasesTest(unittest.TestCase):
    def setUp(self):
        self.clock = Clock()
        self.leases = Leases(self.clock, lambda: WALL + self.clock.now - 1000.0)

    def wish(self, priority, actor="someone", target=LAMP, **kw):
        return self.leases.wish(target, priority, actor, HOLD_S, **kw)

    def held(self):
        return list(self.leases.records().values())

    def test_an_unheld_aspect_forwards_and_is_then_held(self):
        self.assertEqual(self.wish("automation"), ("forward", None))
        self.assertEqual(self.leases.next_deadline(), 1000.0 + HOLD_S)

    def test_a_lower_band_is_refused_and_counted_against_the_hold(self):
        self.wish("manual", "family")
        action, holder = self.wish("automation", "scheduler")
        self.assertEqual(action, "refuse")
        self.assertEqual((holder["priority"], holder["actor"]), ("manual", "family"))
        self.wish("automation", "scheduler")
        [hold] = self.held()
        self.assertEqual(hold["refused"], 2)
        self.assertEqual(hold["actor"], "family", "a refusal leaves the hold as it was")

    def test_a_higher_band_preempts_and_takes_the_hold(self):
        self.wish("automation", "scheduler")
        action, holder = self.wish("manual", "family")
        self.assertEqual(action, "preempt")
        self.assertEqual(holder["actor"], "scheduler")
        [hold] = self.held()
        self.assertEqual((hold["priority"], hold["actor"]), ("manual", "family"))

    def test_the_same_band_refreshes_the_hold_and_its_tally(self):
        self.wish("agent", "a")
        self.wish("automation")  # refused: one against the hold
        self.clock.now += 30
        self.assertEqual(self.wish("agent", "b")[0], "forward")
        [hold] = self.held()
        self.assertEqual((hold["actor"], hold["refused"]), ("b", 0))
        self.assertEqual(self.leases.next_deadline(), 1030.0 + HOLD_S)

    def test_an_expired_hold_reopens_the_aspect(self):
        self.wish("manual", "family")
        self.clock.now += HOLD_S
        self.assertEqual(self.wish("automation"), ("forward", None))

    def test_aspects_are_held_independently(self):
        self.wish("manual", "family")
        offset = ("hall", "lamp", "brightness")
        self.assertEqual(self.wish("automation", target=offset), ("forward", None))

    def test_prune_drops_only_what_has_ended(self):
        self.wish("manual", target=("a", "x", "on"))
        self.clock.now += 10
        self.wish("manual", target=("b", "y", "on"))
        self.assertFalse(self.leases.prune())
        self.clock.now = 1000.0 + HOLD_S
        self.assertTrue(self.leases.prune())
        self.assertEqual(self.leases.next_deadline(), 1010.0 + HOLD_S)

    def test_a_record_is_in_wall_time_and_fixed_when_taken(self):
        self.wish("manual", value=True)
        self.clock.now += 15
        [hold] = self.held()
        self.assertEqual(hold["schema"], 1)
        self.assertEqual((hold["room"], hold["entity"], hold["aspect"]), LAMP)
        self.assertEqual(hold["value"], True)
        self.assertEqual(hold["since"], "2023-11-14T22:13:20Z")
        self.assertEqual(hold["until"], "2023-11-14T22:14:20Z")

    def test_retain_drops_holds_on_entities_no_longer_arbitrated(self):
        self.wish("manual", target=("a", "x", "on"))
        self.wish("manual", target=("a", "x", "level"))
        self.wish("manual", target=("b", "y", "on"))
        self.assertFalse(self.leases.retain(frozenset({("a", "x"), ("b", "y")})))
        self.assertTrue(self.leases.retain(frozenset({("b", "y")})))
        self.assertEqual(list(self.leases.records()), [("b", "y", "on")])

    def test_the_records_leave_out_an_ended_hold_before_it_is_pruned(self):
        self.wish("manual")
        self.clock.now += HOLD_S
        self.assertEqual(self.held(), [])


class HoldIntentTest(unittest.TestCase):
    """What a manual command's `hold` does to the family hold
    (docs/design.md#hold-intents)."""

    def setUp(self):
        self.clock = Clock()
        self.leases = Leases(self.clock, lambda: WALL + self.clock.now - 1000.0)

    def wish(self, priority, actor="family", **kw):
        return self.leases.wish(LAMP, priority, actor, HOLD_S, **kw)

    def held(self):
        return list(self.leases.records().values())

    def test_none_acts_without_taking_over(self):
        self.assertEqual(self.wish("manual", value=True, hold="none"), ("forward", None))
        self.assertEqual(self.held(), [])
        self.assertEqual(self.wish("automation", "scheduler")[0], "forward")

    def test_none_clears_a_family_hold_but_leaves_a_lower_lease(self):
        self.wish("manual", value=True)
        self.wish("manual", value=False, hold="none")
        self.assertEqual(self.held(), [])
        self.wish("agent", "planner")
        self.wish("manual", value=True, hold="none")
        [hold] = self.held()
        self.assertEqual(hold["actor"], "planner", "nothing took the agent's lease over")

    def test_an_instant_holds_until_then(self):
        until = "2023-11-14T23:13:20+00:00"  # an hour after WALL
        self.assertEqual(self.wish("manual", value=True, hold=until)[0], "forward")
        [hold] = self.held()
        self.assertEqual(hold["until"], "2023-11-14T23:13:20Z")
        self.assertEqual(self.leases.next_deadline(), 1000.0 + 3600)
        self.assertEqual(self.wish("automation", "scheduler")[0], "refuse")

    def test_a_zero_hold_time_still_takes_over(self):
        # hold_minutes = 0: the wish wins and is reported as a takeover,
        # and the hold it takes has already ended.
        self.leases.wish(LAMP, "automation", "scheduler", HOLD_S)
        self.assertEqual(self.leases.wish(LAMP, "manual", "family", 0.0, value=True)[0], "preempt")
        self.assertEqual(self.held(), [])

    def test_an_instant_already_past_holds_nothing(self):
        self.wish("manual", value=True)
        self.wish("manual", value=False, hold="2023-11-14T22:00:00Z")
        self.assertEqual(self.held(), [])

    def test_report_holds_what_the_device_did(self):
        self.wish("automation", "scheduler")
        self.assertEqual(self.wish("manual", "esphome", value=False, hold="report")[0], "preempt")
        [hold] = self.held()
        self.assertEqual((hold["priority"], hold["actor"], hold["value"]), ("manual", "esphome", False))

    def test_release_clears_a_hold_another_surface_placed(self):
        self.wish("manual", "esphome", value=False, hold="report")
        self.wish("manual", "dashboard", hold="release")
        self.assertEqual(self.held(), [])
        self.assertEqual(self.wish("automation", "scheduler"), ("forward", None))

    def test_release_leaves_a_lower_lease(self):
        self.wish("automation", "scheduler")
        self.wish("manual", "dashboard", hold="release")
        self.assertEqual([h["actor"] for h in self.held()], ["scheduler"])


class RestoreTest(unittest.TestCase):
    """A restarted arbiter takes back the holds the mirror kept."""

    def test_a_hold_still_in_force_comes_back_with_its_deadline(self):
        clock = Clock()
        before = Leases(clock, lambda: WALL)
        before.wish(LAMP, "manual", "dashboard", HOLD_S, value=True)
        before.wish(LAMP, "automation", "scheduler", HOLD_S)  # refused
        records = list(before.records().values())

        later = Clock()
        later.now = 50.0
        after = Leases(later, lambda: WALL + 20)
        after.restore(records)
        self.assertEqual(list(after.records().values()), records)
        self.assertEqual(after.next_deadline(), 50.0 + HOLD_S - 20)

    def test_an_ended_or_unreadable_record_is_skipped(self):
        clock = Clock()
        before = Leases(clock, lambda: WALL)
        before.wish(LAMP, "manual", "dashboard", HOLD_S, value=True)
        [record] = before.records().values()
        ended = Leases(Clock(), lambda: WALL + HOLD_S)
        ended.restore([record])
        self.assertEqual(ended.records(), {})
        live = Leases(Clock(), lambda: WALL + 1)
        live.restore([{"schema": 1, "holds": []}, "junk", dict(record, priority="boss"),
                      dict(record, until="soon")])
        self.assertEqual(live.records(), {})


def entity(room, name, write):
    return {"name": name, "room": room, "capability": "lock", "write": write, "owner": "z"}


class BindingsTest(unittest.TestCase):
    def test_arbitrated_and_bound_come_from_every_row(self):
        door = entity("hall", "door", "arbitrated")
        lamp = entity("hall", "lamp", "shared")
        grants = [
            {"unit": "dashboard", "publish": "locks", "entities": [door]},
            {"unit": "z", "publish": "state", "entities": [door, lamp]},
        ]
        arbitrated, bound = bindings(grants)
        self.assertEqual(arbitrated, {("hall", "door")})
        self.assertEqual(bound, {("hall", "door"), ("hall", "lamp")})


class FakeSession:
    """Serves seeds to get_json and keeps each subscription's handler."""

    def __init__(self, seeds):
        self.seeds = seeds
        self.handlers = {}

    def subscribe(self, key, handler):
        self.handlers[key] = handler

    def get_json(self, key):
        return [(key, self.seeds[key])] if key in self.seeds else []

    def put(self, key, table):
        payload = SimpleNamespace(to_bytes=lambda: json.dumps(table).encode())
        self.handlers[key](SimpleNamespace(payload=payload))


def owner_row(*entities):
    return {"unit": "z", "publish": "state", "entities": list(entities)}


class RoutingDuringAnApplyTest(unittest.TestCase):
    """While an apply walks, the arbiter routes on the applied table and the
    pending one together, so a wish for an entity the walk binds or
    arbitrates is not lost before the applied table advances."""

    def setUp(self):
        self.changes = []
        applied = [owner_row(entity("hall", "door", "shared"))]
        self.session = FakeSession({GRANTS_KEY: applied})
        self.routes = Bindings(self.session, self.changes.append)

    def test_without_an_apply_the_applied_table_decides(self):
        self.assertEqual(self.routes.route(("hall", "door")), "bound")
        self.assertEqual(self.routes.route(("hall", "lamp")), "unbound")

    def test_an_entity_the_walk_arbitrates_or_binds_is_routed_at_once(self):
        self.session.put(PENDING_KEY, [owner_row(
            entity("hall", "door", "arbitrated"), entity("hall", "lamp", "arbitrated")
        )])
        self.assertEqual(self.routes.route(("hall", "door")), "arbitrated")
        self.assertEqual(self.routes.route(("hall", "lamp")), "arbitrated")
        self.assertEqual(self.changes[-1], {("hall", "door"), ("hall", "lamp")})

    def test_an_entity_leaving_arbitration_stays_arbitrated_until_the_apply_completes(self):
        arbitrated = [owner_row(entity("hall", "door", "arbitrated"))]
        session = FakeSession({GRANTS_KEY: arbitrated})
        routes = Bindings(session, self.changes.append)
        session.put(PENDING_KEY, [owner_row(entity("hall", "door", "shared"))])
        self.assertEqual(routes.route(("hall", "door")), "arbitrated")
        # The apply completes: the applied table advances, then the pending
        # one empties.
        session.put(GRANTS_KEY, [owner_row(entity("hall", "door", "shared"))])
        session.put(PENDING_KEY, [])
        self.assertEqual(routes.route(("hall", "door")), "bound")
        self.assertEqual(self.changes[-1], frozenset())

    def test_a_table_delivered_live_wins_over_its_seed(self):
        session = FakeSession({GRANTS_KEY: [owner_row(entity("hall", "door", "shared"))]})
        seed = session.get_json

        def get_json(key):
            # A newer table arrives while the get is in flight.
            if key == GRANTS_KEY:
                session.put(GRANTS_KEY, [owner_row(entity("hall", "door", "arbitrated"))])
            return seed(key)

        session.get_json = get_json
        routes = Bindings(session, self.changes.append)
        self.assertEqual(routes.route(("hall", "door")), "arbitrated")

    def test_a_pending_table_seeded_at_startup_counts(self):
        session = FakeSession({
            GRANTS_KEY: [owner_row(entity("hall", "door", "shared"))],
            PENDING_KEY: [owner_row(entity("hall", "door", "arbitrated"))],
        })
        changes = []
        routes = Bindings(session, changes.append)
        self.assertEqual(routes.route(("hall", "door")), "arbitrated")
        # Once, with both tables, so restored holds are checked against them.
        self.assertEqual(changes, [{("hall", "door")}])


if __name__ == "__main__":
    unittest.main()
