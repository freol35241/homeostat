"""The persons rule, without a bus: trackers in, a person out."""

import sys
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[2] / "adapters"))

from types import SimpleNamespace

from homeostat.house import Entity, SourceRef
from persons import Persons, Unit

ACCURACY = 500.0


def person(name="fredrik", **sources):
    return Entity(
        name=name,
        id="",
        capability="person",
        room="person",
        owner="persons",
        sources={n: SourceRef(entity=e, aspect=a) for n, (e, a) in sources.items()},
    )


FREDRIK = person(
    geofence=("fredrik_phone", "home"),
    gps=("fredrik_owntracks", "lat"),
    wifi=("fredriks_phone", "sighted"),
)


def fix(rule, tracker, lat, lon, accuracy, fixed_at):
    """Deliver one fix as an adapter publishes it: `fixed_at` last."""
    for aspect, value in (("lat", lat), ("lon", lon), ("accuracy", accuracy), ("fixed_at", fixed_at)):
        concerned = rule.seen(tracker, aspect, value)
    return concerned


class PresenceTest(unittest.TestCase):
    def setUp(self):
        self.rule = Persons([FREDRIK])

    def evaluate(self):
        return self.rule.evaluate(FREDRIK, ACCURACY)

    def test_he_drives_off(self):
        self.rule.seen("fredrik_phone", "home", False)
        self.rule.seen("fredriks_phone", "sighted", False)
        out = self.evaluate()
        self.assertIs(out.values["presence"], False)
        self.assertEqual(out.decision, "away: geofence left the geofence")

    def test_his_phone_sleeps_at_home(self):
        self.rule.seen("fredrik_phone", "home", True)
        self.rule.seen("fredriks_phone", "sighted", False)
        self.assertIs(self.evaluate().values["presence"], True)

    def test_he_arrives_before_the_geofence_does(self):
        self.rule.seen("fredrik_phone", "home", False)
        self.rule.seen("fredriks_phone", "sighted", True)
        self.assertIs(self.evaluate().values["presence"], True)

    def test_his_phone_dies_while_he_is_out_and_presence_is_held_stale(self):
        self.rule.seen("fredrik_phone", "home", True)
        self.rule.seen("fredrik_phone", "available", False)
        self.rule.seen("fredriks_phone", "sighted", False)
        out = self.evaluate()
        self.assertNotIn("presence", out.values)
        self.assertIs(out.values["presence_valid"], False)
        self.assertTrue(out.decision.startswith("unchanged"))

    def test_a_decided_presence_is_valid(self):
        self.rule.seen("fredrik_phone", "home", True)
        self.assertIs(self.evaluate().values["presence_valid"], True)

    def test_a_sighting_alone_cannot_say_he_left_when_he_has_a_geofence(self):
        self.rule.seen("fredriks_phone", "sighted", False)
        self.assertNotIn("presence", self.evaluate().values)

    def test_wifi_only_falls_back_to_the_sightings(self):
        dad = person("dad", wifi=("dads_phone", "sighted"), tablet=("dads_tablet", "sighted"))
        rule = Persons([dad])
        rule.seen("dads_phone", "sighted", False)
        # The tablet has said nothing yet, so nothing says he left.
        self.assertNotIn("presence", rule.evaluate(dad, ACCURACY).values)
        rule.seen("dads_tablet", "sighted", False)
        self.assertIs(rule.evaluate(dad, ACCURACY).values["presence"], False)
        # An unavailable tracker drops out of the fallback.
        rule.seen("dads_tablet", "sighted", True)
        rule.seen("dads_tablet", "available", False)
        self.assertIs(rule.evaluate(dad, ACCURACY).values["presence"], False)

    def test_the_sources_that_decided_are_the_ones_reported_used(self):
        self.rule.seen("fredrik_phone", "home", False)
        self.rule.seen("fredriks_phone", "sighted", True)
        out = self.evaluate()
        self.assertEqual(out.decision, "home: wifi says so")
        self.assertEqual(out.used[("presence", "wifi")], True)
        # The geofence was heard and outweighed: it did not decide.
        self.assertEqual(out.used[("presence", "geofence")], False)

    def test_nothing_decided_a_held_presence(self):
        self.rule.seen("fredriks_phone", "sighted", False)
        out = self.evaluate()
        self.assertEqual(
            (out.used[("presence", "geofence")], out.used[("presence", "wifi")]), (False, False)
        )


class PositionTest(unittest.TestCase):
    def setUp(self):
        self.rule = Persons([FREDRIK])

    def position(self):
        values = self.rule.evaluate(FREDRIK, ACCURACY).values
        return {a: values[a] for a in ("lat", "lon", "accuracy", "fixed_at") if a in values}

    def test_a_fix_counts_once_its_fixed_at_arrives(self):
        self.assertEqual(self.rule.seen("fredrik_owntracks", "lat", 57.7), [])
        self.assertEqual(self.rule.seen("fredrik_owntracks", "lon", 11.9), [])
        self.assertEqual(self.position(), {})
        self.assertEqual(self.rule.seen("fredrik_owntracks", "fixed_at", 900), [FREDRIK])
        self.assertEqual(self.position(), {"lat": 57.7, "lon": 11.9, "fixed_at": 900})

    def test_an_inaccurate_fix_never_reaches_the_person(self):
        fix(self.rule, "fredrik_owntracks", 57.7, 11.9, 12, 900)
        self.assertEqual(self.position(), {"lat": 57.7, "lon": 11.9, "accuracy": 12, "fixed_at": 900})
        # A network locate far away, delivered one aspect at a time. Its
        # lat and lon alone concern nobody, so they are never paired with
        # the good fix's accuracy.
        self.assertEqual(self.rule.seen("fredrik_owntracks", "lat", 59.3), [])
        self.assertEqual(self.rule.seen("fredrik_owntracks", "lon", 18.1), [])
        self.assertEqual(self.rule.seen("fredrik_owntracks", "accuracy", 2000), [])
        self.rule.seen("fredrik_owntracks", "fixed_at", 1000)
        # Complete, it is too coarse: the person gets no new position, and
        # the values already published stand.
        out = self.rule.evaluate(FREDRIK, ACCURACY)
        self.assertNotIn("lat", out.values)
        self.assertEqual(out.used[("lat", "gps")], False)

    def test_the_newest_fix_wins(self):
        two = person(gps=("phone", "lat"), watch=("watch", "lat"))
        rule = Persons([two])
        fix(rule, "phone", 1.0, 0.0, 10, 200)
        fix(rule, "watch", 2.0, 0.0, 10, 300)
        out = rule.evaluate(two, ACCURACY)
        self.assertEqual((out.values["lat"], out.values["fixed_at"]), (2.0, 300))
        for aspect in ("lat", "lon", "accuracy", "fixed_at"):
            self.assertEqual((out.used[(aspect, "watch")], out.used[(aspect, "gps")]), (True, False))

    def test_battery_stays_on_the_tracker(self):
        fix(self.rule, "fredrik_owntracks", 57.7, 11.9, 12, 900)
        self.rule.seen("fredrik_owntracks", "battery", 80)
        self.assertNotIn("battery", self.rule.evaluate(FREDRIK, ACCURACY).values)

    def test_commit_takes_a_fix_delivered_out_of_order(self):
        # The mirror's catch-up delivers keys in no set order.
        for aspect, value in (("fixed_at", 900), ("lat", 57.7), ("lon", 11.9)):
            self.rule.seen("fredrik_owntracks", aspect, value)
        self.assertEqual(self.position(), {})
        self.rule.commit("fredrik_owntracks")
        self.assertEqual(self.position(), {"lat": 57.7, "lon": 11.9, "fixed_at": 900})


class AvailabilityTest(unittest.TestCase):
    def test_without_a_report_availability_is_not_published(self):
        rule = Persons([FREDRIK])
        rule.seen("fredrik_phone", "home", True)
        self.assertNotIn("available", rule.evaluate(FREDRIK, ACCURACY).values)

    def test_true_when_any_tracker_reports_true(self):
        rule = Persons([FREDRIK])
        rule.seen("fredrik_phone", "available", False)
        rule.seen("fredriks_phone", "available", True)
        self.assertIs(rule.evaluate(FREDRIK, ACCURACY).values["available"], True)

    def test_false_only_when_every_tracker_reports_false(self):
        rule = Persons([FREDRIK])
        rule.seen("fredrik_phone", "available", False)
        rule.seen("fredriks_phone", "available", False)
        # owntracks reports no availability, so the person is not known to be down.
        self.assertNotIn("available", rule.evaluate(FREDRIK, ACCURACY).values)
        rule.seen("fredrik_owntracks", "available", False)
        self.assertIs(rule.evaluate(FREDRIK, ACCURACY).values["available"], False)


class FakeContext:
    def __init__(self):
        self.params = SimpleNamespace(max_accuracy_m=ACCURACY)
        self.published = []
        self.decisions = []

    def publish(self, binding, value, *, room, entity, aspect):
        self.published.append((entity, aspect, value))

    def source_used(self, entity, aspect, source, used):
        pass

    def decision(self, rule, outcome):
        self.decisions.append((rule, outcome))


class UnitTest(unittest.TestCase):
    def test_a_restart_does_not_act_on_half_of_the_catch_up(self):
        ctx = FakeContext()
        unit = Unit(ctx, [FREDRIK])
        # The mirror, in an order that reads `home` before the phone's
        # `available = false`, plus the person as the running unit left it.
        unit.on_state("home/state/person/fredrik/presence", False)
        unit.on_state("home/state/person/fredrik/presence_valid", False)
        unit.on_state("home/state/person/fredrik_phone/home", True)
        unit.on_state("home/state/person/fredrik_phone/available", False)
        self.assertEqual(ctx.published, [])
        unit.start()
        self.assertNotIn(("fredrik", "presence", True), ctx.published)
        # Nothing changed, so nothing is republished.
        self.assertEqual(ctx.published, [])
        self.assertTrue(ctx.decisions[-1][1].startswith("unchanged"))

    def test_after_the_catch_up_each_change_is_evaluated(self):
        ctx = FakeContext()
        unit = Unit(ctx, [FREDRIK])
        unit.start()
        unit.on_state("home/state/person/fredriks_phone/sighted", True)
        self.assertIn(("fredrik", "presence", True), ctx.published)


class SeenTest(unittest.TestCase):
    def test_only_listed_trackers_and_read_aspects_concern_a_person(self):
        rule = Persons([FREDRIK])
        self.assertEqual(rule.seen("fredrik_phone", "home", True), [FREDRIK])
        self.assertEqual(rule.seen("fredrik_phone", "battery", 50), [])
        self.assertEqual(rule.seen("someone_else", "home", True), [])
        # The person's own published aspects come back on the subscription.
        self.assertEqual(rule.seen("fredrik", "presence", True), [])


if __name__ == "__main__":
    unittest.main()
