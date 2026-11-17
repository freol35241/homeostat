"""The arbiter's rule, without a bus: which hold wins an aspect, what a
command does, and what is forwarded.

Also which entities it arbitrates and who may hold them, read from the
grant table.
"""

import datetime
import json
import sys
import unittest
from pathlib import Path
from types import SimpleNamespace

sys.path.insert(0, str(Path(__file__).resolve().parents[2] / "adapters"))

from arbiter import (
    GRANTS_KEY,
    PENDING_KEY,
    Bindings,
    Resolver,
    bindings,
    holders,
    policy_entry,
)

STOVE = ("utility", "stove", "on")
LAMP = ("hall", "lamp", "on")
HOLD_S = 60.0
WALL = 1_700_000_000.0  # 2023-11-14T22:13:20Z
CET = datetime.timedelta(hours=1)
CEST = datetime.timedelta(hours=2)
# A family stop's policy: hold until the next 06:00.
STOP = {"until": "06:00"}


class Clock:
    def __init__(self):
        self.now = 1000.0

    def __call__(self):
        return self.now


BANDS = {"heating": "automation", "flue_guard": "automation", "planner": "agent"}
# Declared on_loss per holder; the others take the default.
LOSS = {"planner": "keep"}


class Base(unittest.TestCase):
    """A resolver where `heating` and `flue_guard` hold at automation and
    `planner` at agent, and the stove's rest value is on = false."""

    def setUp(self):
        self.clock = Clock()
        self.resolver = Resolver(
            grant_of=lambda unit, target: (BANDS[unit], LOSS.get(unit)) if unit in BANDS else None,
            rest_of=lambda target: False if target == STOVE else None,
            clock=self.clock,
            wall=lambda: WALL + self.clock.now - 1000.0,
            offset=CET,
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

    def test_a_record_that_is_not_a_hold_is_refused_and_drops_the_hold(self):
        effects = []
        for record in ({"id": "x"}, {"value": 1}, "junk", {"value": 1, "id": "x", "until": "soon"}):
            self.assertFalse(self.resolver.put_hold(STOVE, "heating", record, WALL, effects), record)
        self.assertEqual(effects, [])
        self.hold("heating", True, "h1")
        self.resolver.put_hold(STOVE, "heating", "junk", WALL, [])
        self.assertIsNone(self.resolver.winner(STOVE), "the mirror now holds the junk, not the hold")

    def test_the_rest_value_is_matched_by_type(self):
        self.hold("flue_guard", 0, "f1", seen=2.0)
        self.hold("heating", True, "h1", seen=1.0)
        self.assertEqual(self.resolver.winner(STOVE)["unit"], "flue_guard", "0 is not False: the newest wins")


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
        self.command("manual", "dashboard", True, cmd_id="c1")
        forwarded, effects = self.command("manual", "dashboard", False, hold="none", cmd_id="n1")
        self.assertTrue(forwarded)
        self.assertEqual(effects, [], "the hold beneath stands again, but nothing is sent or said")
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

    def test_a_command_releases_its_senders_hold_whichever_arrives_first(self):
        # The SDK deletes the hold's key after the command. The two arrive
        # on two subscribers, in either order, and neither order may send
        # the hold beneath or undo the command.
        for delete_first in (False, True):
            with self.subTest(delete_first=delete_first):
                self.setUp()
                self.hold("heating", 18, "h1", target=LAMP)
                self.hold("planner", 21, "p1", target=LAMP)
                if delete_first:
                    effects = self.drop("planner", target=LAMP)
                    self.assertEqual(self.forwards(effects), [(18, "heating", "h1")], "a release sends the hold beneath")
                    continue
                forwarded, effects = self.command("agent", "planner", 22, target=LAMP)
                self.assertTrue(forwarded)
                self.assertEqual(self.forwards(effects), [])
                self.assertEqual(self.forwards(self.drop("planner", target=LAMP)), [], "the late delete undoes nothing")
                self.assertEqual(self.resolver.winner(LAMP)["unit"], "heating")

    def test_the_hold_beneath_a_command_waits_for_its_next_change(self):
        self.hold("heating", True, "h1")
        self.command("automation", "evening", True)
        self.resolver.resolve(STOVE, effects := [])
        self.assertEqual(effects, [])


class FamilyPolicyTest(Base):
    """The pellet burner of docs/design.md#family-policy: a family stop
    holds until 06:00, a family start holds nothing."""

    def test_a_stop_locks_out_automatic_starts_until_the_morning(self):
        self.command("manual", "dashboard", False, cmd_id="c1", policy=STOP)
        [record] = self.resolver.family_records().values()
        # 23:13 local (+01:00), so the next 06:00 is tomorrow's.
        self.assertEqual((record["until"], record["at"]), ("2023-11-15T05:00:00Z", "2023-11-15T06:00"))
        self.assertFalse(self.command("automation", "start_rule", True)[0], "an automatic start is refused")
        self.assertTrue(self.command("automation", "flue_stop", False)[0], "an automatic stop still goes through")

    def test_a_start_holds_nothing_and_clears_a_stop(self):
        self.command("manual", "dashboard", False, policy=STOP)
        forwarded, _ = self.command("manual", "dashboard", True, policy="none")
        self.assertTrue(forwarded)
        self.assertEqual(self.resolver.family_records(), {})
        self.assertTrue(self.command("automation", "flue_stop", False)[0], "the stop rule acts on the start")

    def test_an_explicit_hold_wins_over_the_policy(self):
        self.command("manual", "dashboard", True, hold="2023-11-14T23:13:20Z", policy="none")
        self.assertEqual(self.resolver.next_deadline(), 1000.0 + 3600)

    def test_without_a_policy_entry_the_hold_lasts_hold_minutes(self):
        self.command("manual", "dashboard", True)
        self.assertEqual(self.resolver.next_deadline(), 1000.0 + HOLD_S)

    def test_a_time_of_day_follows_the_house_clock_across_dst(self):
        self.resolver.set_offset(CEST, [])
        self.command("manual", "dashboard", False, policy=STOP)
        [record] = self.resolver.family_records().values()
        self.assertEqual(record["until"], "2023-11-15T04:00:00Z")
        # The clocks go back overnight. 06:00 is an hour later than it was.
        self.resolver.set_offset(CET, [])
        [record] = self.resolver.family_records().values()
        self.assertEqual(record["until"], "2023-11-15T05:00:00Z")
        self.assertEqual(self.resolver.next_deadline(), 1000.0 + 6 * 3600 + 46 * 60 + 40)

    def test_next_change_ends_when_the_hold_beneath_changes(self):
        self.hold("heating", True, "h1")
        self.command("manual", "dashboard", False, cmd_id="c1", policy={"until": "next change"})
        self.assertEqual(self.resolver.next_deadline(), 1000.0 + 24 * 3600, "at most a day")
        self.assertEqual(self.forwards(self.hold("heating", True, "h1")), [], "the same hold again is no change")
        effects = self.hold("heating", False, "h2")
        self.assertEqual(self.forwards(effects), [(False, "heating", "h2")])
        self.assertEqual(self.events(effects), ["resume"])
        self.assertEqual(self.resolver.family_records(), {})

    def test_next_change_without_a_hold_beneath_ends_when_one_comes(self):
        self.command("manual", "dashboard", False, policy={"until": "next change"})
        self.assertEqual(self.forwards(self.hold("heating", True, "h1")), [(True, "heating", "h1")])

    def test_a_command_from_the_unit_beneath_is_a_change(self):
        # Its command releases its hold, and the family's hold with it.
        self.hold("heating", True, "h1")
        self.command("manual", "dashboard", False, policy={"until": "next change"})
        self.assertTrue(self.command("automation", "heating", True)[0])
        self.assertEqual(self.resolver.family_records(), {})

    def test_a_restart_keeps_how_the_policy_placed_the_hold(self):
        self.hold("heating", True, "h1")
        self.command("manual", "dashboard", False, cmd_id="c1", policy={"until": "next change"})
        self.command("manual", "dashboard", False, cmd_id="c2", target=LAMP, policy=STOP)
        records = list(self.resolver.family_records().values())
        after = Resolver(
            grant_of=lambda unit, target: (BANDS[unit], LOSS.get(unit)) if unit in BANDS else None,
            rest_of=lambda target: None, clock=Clock(), wall=lambda: WALL, offset=CEST,
        )
        after.restore(records, self.resolver.document())
        self.assertEqual(after.family_records(), self.resolver.family_records())
        after.put_hold(STOVE, "heating", {"value": True, "id": "h1"}, WALL, [])
        after.set_offset(CET, [])
        self.assertIn(STOVE, after.family_records(), "the hold beneath is the one it was placed over")
        after.put_hold(STOVE, "heating", {"value": False, "id": "h2"}, WALL, [])
        self.assertEqual(list(after.family_records()), [LAMP])

    def test_a_restart_before_the_grants_are_read_keeps_a_next_change_hold(self):
        # The arbiter restores the holds before it reads the grant table,
        # when no unit hold counts yet.
        self.hold("heating", True, "h1")
        self.command("manual", "dashboard", False, cmd_id="c1", policy={"until": "next change"})
        records = list(self.resolver.family_records().values())
        read = []
        after = Resolver(
            grant_of=lambda unit, target: (BANDS[unit], LOSS.get(unit)) if read and unit in BANDS else None,
            rest_of=lambda target: None, clock=Clock(), wall=lambda: WALL, offset=CET,
            grants_read=lambda: bool(read),
        )
        after.restore(records, self.resolver.document())
        after.put_hold(STOVE, "heating", {"value": True, "id": "h1"}, WALL, [])
        read.append(True)
        after.resolve_all(effects := [])
        self.assertEqual(effects, [])
        self.assertIn(STOVE, after.family_records())

    def test_a_refused_command_that_ends_the_hold_sends_the_new_winner(self):
        self.hold("flue_guard", 18, "g1", target=LAMP, seen=1.0)
        self.hold("heating", 20, "h1", target=LAMP, seen=2.0)
        self.command("manual", "dashboard", 22, target=LAMP, policy={"until": "next change"})
        # heating's command releases its hold, which ends the family's, and
        # the guard's hold refuses the command.
        forwarded, effects = self.command("automation", "heating", 19, target=LAMP)
        self.assertFalse(forwarded)
        self.assertEqual(self.forwards(effects), [(18, "flue_guard", "g1")])
        self.assertEqual(self.resolver.family_records(), {})

    def test_a_time_of_day_before_the_house_clock_is_known_says_so(self):
        resolver = Resolver(lambda u, t: None, lambda t: None, clock=Clock(), wall=lambda: WALL)
        resolver.command(STOVE, "manual", "dashboard", HOLD_S, effects := [], value=False, policy=STOP)
        self.assertEqual(self.events(effects), ["clock-unknown"])
        resolver.set_offset(CET, [])
        resolver.command(STOVE, "manual", "dashboard", HOLD_S, effects := [], value=False, policy=STOP)
        self.assertEqual(self.events(effects), [])


class CommandAgreesTest(Base):
    def test_a_command_that_agrees_with_a_higher_hold_goes_through(self):
        self.hold("planner", False, "p1")
        self.assertTrue(self.command("automation", "flue_stop", False)[0])
        self.assertFalse(self.command("automation", "start_rule", True)[0])


class PolicyEntryTest(unittest.TestCase):
    def test_a_key_matches_its_value_by_type(self):
        values = {"false": "none", "1": {"until": "06:00"}, "high": "none", "any": {"until": "next change"}}
        self.assertEqual(policy_entry(values, False), "none")
        self.assertEqual(policy_entry(values, 1), {"until": "06:00"})
        self.assertEqual(policy_entry(values, 1.0), {"until": "06:00"})
        self.assertEqual(policy_entry(values, "high"), "none")
        self.assertEqual(policy_entry(values, True), {"until": "next change"}, "true is not 1")
        self.assertIsNone(policy_entry({"false": "none"}, 0), "0 is not false")


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
            grant_of=lambda unit, target: (BANDS[unit], LOSS.get(unit)) if unit in BANDS else None, rest_of=lambda target: None,
            clock=later, wall=lambda: WALL + 20,
        )
        after.restore(records, document)
        self.assertEqual(after.document()["aspects"][1]["refused"], 0)
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


class WhenAHolderStopsTest(Base):
    """docs/design.md#when-a-holder-stops."""

    def test_a_gone_holders_rest_value_hold_stands_and_any_other_ends(self):
        self.hold("heating", True, "h1", seen=1.0)
        self.hold("flue_guard", False, "f1", seen=2.0)
        effects = []
        self.resolver.holder("flue_guard", True, effects)
        self.assertEqual(self.forwards(effects), [], "a dead flue guard keeps the burner off")
        self.resolver.holder("flue_guard", False, [])
        self.drop("flue_guard")
        self.hold("heating", True, "h2")
        self.resolver.holder("heating", True, [])
        self.assertIsNone(self.resolver.winner(STOVE), "a dead start rule stops asking")

    def test_a_declared_release_ends_even_a_rest_value_hold(self):
        LOSS["flue_guard"] = "release"
        self.addCleanup(LOSS.pop, "flue_guard")
        self.hold("flue_guard", False, "f1")
        self.resolver.holder("flue_guard", True, [])
        self.assertIsNone(self.resolver.winner(STOVE))

    def test_a_holder_away_but_not_gone_moves_nothing(self):
        # What an apply's restart looks like: the holder is briefly away,
        # and nobody marks it gone.
        self.hold("heating", 19, "h1", target=LAMP)
        self.hold("planner", 23, "p1", target=LAMP)
        self.resolver.holder("planner", False, effects := [])
        self.assertEqual(effects, [])
        self.assertEqual(self.resolver.winner(LAMP)["unit"], "planner")

    def test_a_declared_keep_keeps_any_hold(self):
        self.hold("heating", 19, "h1", target=LAMP)
        self.hold("planner", 23, "p1", target=LAMP)
        self.resolver.holder("planner", True, effects := [])
        self.assertEqual(self.forwards(effects), [])
        self.assertEqual(self.resolver.winner(LAMP)["unit"], "planner")

    def test_a_holder_coming_back_counts_again(self):
        self.hold("heating", 19, "h1", target=LAMP)
        self.resolver.holder("heating", True, [])
        self.assertIsNone(self.resolver.winner(LAMP))
        self.resolver.holder("heating", False, effects := [])
        self.assertEqual(self.forwards(effects), [], "the applied hold is not sent again")
        self.assertEqual(self.resolver.winner(LAMP)["id"], "h1")


class DeliveryTest(Base):
    def test_a_hold_the_adapter_could_not_deliver_is_sent_when_the_device_is_back(self):
        self.hold("heating", True, "h1")
        self.resolver.undelivered("h1")
        self.resolver.available("utility", "stove", effects := [])
        self.assertEqual(self.forwards(effects), [(True, "heating", "h1")])
        self.resolver.available("utility", "stove", effects := [])
        self.assertEqual(effects, [], "once")

    def test_an_undelivered_hold_survives_an_arbiter_restart(self):
        self.hold("heating", True, "h1")
        self.resolver.undelivered("h1")
        document = self.resolver.document()
        after = Resolver(
            grant_of=lambda unit, target: (BANDS[unit], None) if unit in BANDS else None,
            rest_of=lambda target: None, clock=Clock(), wall=lambda: WALL,
        )
        after.restore([], document)
        after.put_hold(STOVE, "heating", {"value": True, "id": "h1"}, WALL, [])
        after.available("utility", "stove", effects := [])
        self.assertEqual([e[2]["id"] for e in effects if e[0] == "forward"], ["h1"])

    def test_a_delivered_hold_is_not_sent_again(self):
        self.hold("heating", True, "h1")
        self.resolver.available("utility", "stove", effects := [])
        self.assertEqual(effects, [], "a device changed by hand while away is not changed back")

    def test_a_dropped_command_is_not_sent_again(self):
        self.command("automation", "evening", True, cmd_id="c1")
        self.resolver.undelivered("c1")
        self.resolver.available("utility", "stove", effects := [])
        self.assertEqual(effects, [])


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
        self.assertEqual(routes.grant_of("heating", STOVE), ("automation", None))
        self.assertIsNone(routes.grant_of("heating", ("utility", "stove", "power_level")))
        self.assertIsNone(routes.grant_of("flue_guard", STOVE))
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



class PolicyBindingTest(unittest.TestCase):
    def test_the_policy_comes_from_the_newest_owner_row(self):
        door = dict(entity("hall", "door", "arbitrated"), family={"locked": {"false": {"until": "23:00"}}})
        session = FakeSession({GRANTS_KEY: [
            {"unit": "dashboard", "publish": "locks", "capability": "lock", "entities": [entity("hall", "door", "arbitrated")]},
            owner_row(door),
        ]})
        routes = Bindings(session, lambda arbitrated: None)
        self.assertEqual(routes.policy_of(("hall", "door", "locked"), False), {"until": "23:00"})
        self.assertIsNone(routes.policy_of(("hall", "door", "locked"), True))
        # An apply that drops the policy decides from its start.
        session.put(PENDING_KEY, [owner_row(entity("hall", "door", "arbitrated"))])
        self.assertIsNone(routes.policy_of(("hall", "door", "locked"), False))


if __name__ == "__main__":
    unittest.main()
