"""The arbiter's rule, without a bus: which hold wins an aspect, what a
command does, and what is forwarded.

Also which entities it arbitrates and who may hold them, read from the
grant table.
"""

import json
import sys
import unittest
from pathlib import Path
from types import SimpleNamespace

sys.path.insert(0, str(Path(__file__).resolve().parents[2] / "adapters"))

from arbiter import GRANTS_KEY, PENDING_KEY, Bindings, Resolver, bindings, holders

STOVE = ("utility", "stove", "on")
LAMP = ("hall", "lamp", "on")
HOLD_S = 60.0
WALL = 1_700_000_000.0  # 2023-11-14T22:13:20Z


class Clock:
    def __init__(self):
        self.now = 1000.0

    def __call__(self):
        return self.now


BANDS = {"heating": "automation", "flue_guard": "automation", "planner": "agent"}


class Base(unittest.TestCase):
    """A resolver where `heating` and `flue_guard` hold at automation and
    `planner` at agent, and the stove's rest value is on = false."""

    def setUp(self):
        self.clock = Clock()
        self.resolver = Resolver(
            band_of=lambda unit, target: BANDS.get(unit),
            rest_of=lambda target: False if target == STOVE else None,
            clock=self.clock,
            wall=lambda: WALL + self.clock.now - 1000.0,
        )

    def hold(self, unit, value, hold_id, target=STOVE, seen=None, **extra):
        effects = []
        record = {"value": value, "id": hold_id, **extra}
        self.resolver.put_hold(target, unit, record, WALL + (seen or self.clock.now), effects)
        return effects

    def drop(self, unit, target=STOVE):
        effects = []
        self.resolver.drop_hold(target, unit, effects)
        return effects

    def command(self, priority, actor, value=None, target=STOVE, **kw):
        effects = []
        forwarded = self.resolver.command(target, priority, actor, HOLD_S, effects, value=value, **kw)
        return forwarded, effects

    @staticmethod
    def forwards(effects):
        return [(e[2]["value"], e[2]["actor"], e[2]["id"]) for e in effects if e[0] == "forward"]

    @staticmethod
    def events(effects):
        return [e[1] for e in effects if e[0] == "event"]


class WhichHoldWinsTest(Base):
    def test_a_hold_is_forwarded_once(self):
        self.assertEqual(self.forwards(self.hold("heating", True, "h1")), [(True, "heating", "h1")])
        self.resolver.resolve(STOVE, effects := [])
        self.assertEqual(effects, [], "the applied hold is not sent again")

    def test_a_higher_band_wins_and_its_end_hands_back(self):
        self.hold("heating", True, "h1")
        effects = self.hold("planner", False, "p1")
        self.assertEqual(self.forwards(effects), [(False, "planner", "p1")])
        self.assertEqual(self.events(effects), ["preempt"])
        effects = self.drop("planner")
        self.assertEqual(self.forwards(effects), [(True, "heating", "h1")])
        self.assertEqual(self.events(effects), ["resume"])

    def test_at_one_band_the_rest_value_wins(self):
        self.hold("flue_guard", False, "f1", seen=1.0)
        effects = self.hold("heating", True, "h1", seen=2.0)
        self.assertEqual(self.forwards(effects), [], "the newer start loses to the stop")
        self.assertEqual(self.resolver.winner(STOVE)["unit"], "flue_guard")

    def test_without_a_rest_value_the_newest_wins(self):
        self.hold("heating", 19, "h1", target=LAMP, seen=1.0)
        effects = self.hold("flue_guard", 21, "f1", target=LAMP, seen=2.0)
        self.assertEqual(self.forwards(effects), [(21, "flue_guard", "f1")])

    def test_a_hold_without_a_grant_does_not_count(self):
        self.assertEqual(self.hold("stranger", True, "s1"), [])
        self.assertIsNone(self.resolver.winner(STOVE))

    def test_a_hold_ends_at_its_until(self):
        self.hold("heating", True, "h1")
        self.hold("planner", False, "p1", until="2023-11-14T22:14:20Z")  # 60 s after WALL
        self.assertEqual(self.resolver.next_deadline(), 1000.0 + 60)
        self.clock.now += 60
        self.resolver.expire(effects := [])
        self.assertEqual(self.forwards(effects), [(True, "heating", "h1")])

    def test_a_record_that_is_not_a_hold_is_refused(self):
        effects = []
        for record in ({"id": "x"}, {"value": 1}, "junk", {"value": 1, "id": "x", "until": "soon"}):
            self.assertFalse(self.resolver.put_hold(STOVE, "heating", record, WALL, effects), record)
        self.assertEqual(effects, [])


class FamilyTest(Base):
    def test_the_family_masks_a_hold_and_hands_back_when_it_ends(self):
        self.hold("heating", True, "h1")
        forwarded, effects = self.command("manual", "dashboard", False, cmd_id="c1")
        self.assertTrue(forwarded, "the family's command is sent itself")
        self.assertEqual(self.events(effects), ["preempt"])
        self.assertEqual(self.forwards(self.hold("heating", True, "h2")), [], "masked")
        self.clock.now += HOLD_S
        self.resolver.expire(effects := [])
        self.assertEqual(self.forwards(effects), [(True, "heating", "h2")], "sent when the family's hold ends")

    def test_release_sends_the_hold_beneath_at_once_and_nothing_without_one(self):
        self.hold("heating", True, "h1")
        self.command("manual", "dashboard", False, cmd_id="c1")
        forwarded, effects = self.command("manual", "dashboard", hold="release")
        self.assertFalse(forwarded)
        self.assertEqual(self.forwards(effects), [(True, "heating", "h1")])
        self.command("manual", "dashboard", False, target=LAMP)
        forwarded, effects = self.command("manual", "dashboard", hold="release", target=LAMP)
        self.assertEqual((forwarded, self.forwards(effects)), (False, []))

    def test_a_nudge_over_a_hold_is_not_undone(self):
        self.hold("heating", True, "h1")
        forwarded, effects = self.command("manual", "dashboard", False, hold="none", cmd_id="n1")
        self.assertTrue(forwarded)
        self.resolver.resolve(STOVE, effects := [])
        self.assertEqual(self.forwards(effects), [], "the hold beneath waits for its next change")
        self.assertEqual(self.forwards(self.hold("heating", True, "h2")), [(True, "heating", "h2")])

    def test_a_report_holds_without_sending(self):
        forwarded, _ = self.command("manual", "esphome", False, hold="report")
        self.assertFalse(forwarded)
        self.assertEqual(self.resolver.winner(STOVE)["value"], False)

    def test_an_instant_holds_until_then_and_one_past_holds_nothing(self):
        self.command("manual", "dashboard", True, hold="2023-11-14T23:13:20+00:00")
        self.assertEqual(self.resolver.next_deadline(), 1000.0 + 3600)
        self.command("manual", "dashboard", False, hold="2023-11-14T22:00:00Z")
        self.assertIsNone(self.resolver.winner(STOVE))

    def test_the_family_record_carries_the_value_and_wall_time(self):
        self.command("manual", "dashboard", True, cmd_id="c1")
        self.clock.now += 15
        [record] = self.resolver.family_records().values()
        self.assertEqual(
            {k: record[k] for k in ("actor", "value", "id", "since", "until", "refused")},
            {"actor": "dashboard", "value": True, "id": "c1", "since": "2023-11-14T22:13:20Z",
             "until": "2023-11-14T22:14:20Z", "refused": 0},
        )


class CommandTest(Base):
    def test_a_command_with_nothing_held_goes_through_and_holds_nothing(self):
        forwarded, effects = self.command("automation", "evening", True)
        self.assertTrue(forwarded)
        self.assertEqual(effects, [])
        self.assertIsNone(self.resolver.winner(STOVE))

    def test_a_command_below_a_hold_is_refused_and_counted(self):
        self.command("manual", "dashboard", False)
        forwarded, effects = self.command("automation", "evening", True, cmd_id="c9")
        self.assertFalse(forwarded)
        [(_, kind, fields)] = effects
        self.assertEqual((kind, fields["holder_actor"], fields["cmd_id"]), ("refuse", "dashboard", "c9"))
        self.assertEqual(next(iter(self.resolver.family_records().values()))["refused"], 1)

    def test_at_one_band_a_command_must_agree_with_the_hold(self):
        self.hold("heating", True, "h1")
        self.assertFalse(self.command("automation", "evening", False)[0])
        self.assertTrue(self.command("automation", "evening", True)[0])

    def test_a_command_does_not_lose_to_its_senders_own_hold(self):
        self.hold("heating", True, "h1")
        self.assertTrue(self.command("automation", "heating", False)[0])

    def test_the_hold_beneath_a_command_waits_for_its_next_change(self):
        self.hold("heating", True, "h1")
        self.command("automation", "evening", True)
        self.resolver.resolve(STOVE, effects := [])
        self.assertEqual(effects, [])


class LifecycleTest(Base):
    def test_retain_forgets_entities_no_longer_arbitrated(self):
        self.hold("heating", True, "h1")
        self.command("manual", "dashboard", True, target=LAMP)
        self.assertTrue(self.resolver.retain(frozenset({("hall", "lamp")})))
        self.assertIsNone(self.resolver.winner(STOVE))
        self.assertEqual(list(self.resolver.family_records()), [LAMP])

    def test_a_restart_sends_nothing_already_applied(self):
        self.hold("heating", True, "h1")
        self.command("manual", "dashboard", False, cmd_id="c1", target=LAMP)
        records = list(self.resolver.family_records().values())
        document = self.resolver.document()

        later = Clock()
        later.now = 50.0
        after = Resolver(
            band_of=lambda unit, target: BANDS.get(unit), rest_of=lambda target: None,
            clock=later, wall=lambda: WALL + 20,
        )
        after.restore(records, document)
        after.put_hold(STOVE, "heating", {"value": True, "id": "h1"}, WALL, effects := [])
        self.assertEqual(effects, [])
        after.resolve_all(effects)
        self.assertEqual(effects, [], "neither the hold nor the family's is sent again")
        self.assertEqual(after.next_deadline(), 50.0 + HOLD_S - 20)

    def test_an_ended_or_unreadable_family_record_is_skipped(self):
        self.command("manual", "dashboard", True, cmd_id="c1")
        [record] = self.resolver.family_records().values()
        ended = Resolver(lambda u, t: None, lambda t: None, clock=Clock(), wall=lambda: WALL + HOLD_S)
        ended.restore([record, "junk", {"room": "x"}], None)
        self.assertEqual(ended.family_records(), {})

    def test_the_document_lists_the_holds_winner_first(self):
        self.hold("heating", True, "h1")
        self.hold("planner", False, "p1", reason="away")
        [entry] = self.resolver.document()["aspects"]
        self.assertEqual([h["unit"] for h in entry["holds"]], ["planner", "heating"])
        self.assertEqual(entry["holds"][0]["reason"], "away")
        self.assertEqual(entry["applied"]["id"], "p1")


def hold_row(unit, band, keys, *entities):
    return {"unit": unit, "publish": "hold", "capability": "burner", "priority": band,
            "keys": keys, "entities": list(entities), "hold": True}


class HoldersTest(unittest.TestCase):
    def test_a_hold_row_gives_its_unit_a_band_on_the_keys_it_covers(self):
        stove = {"name": "stove", "room": "utility", "capability": "burner", "write": "arbitrated", "owner": "aduro"}
        session = FakeSession({GRANTS_KEY: [
            hold_row("heating", "automation", ["home/hold/heating/utility/stove/on"], stove),
            {"unit": "aduro", "publish": "state", "entities": [stove]},
        ]})
        routes = Bindings(session, lambda arbitrated: None)
        self.assertEqual(routes.band_of("heating", STOVE), "automation")
        self.assertIsNone(routes.band_of("heating", ("utility", "stove", "power_level")))
        self.assertIsNone(routes.band_of("flue_guard", STOVE))
        self.assertIs(routes.rest_of(STOVE), False)
        self.assertEqual(set(holders(session.seeds[GRANTS_KEY])), {("utility", "stove")})


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
