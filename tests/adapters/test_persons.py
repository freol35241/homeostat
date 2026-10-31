"""The persons rule, without a bus: trackers in, a person out."""

import sys
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[2] / "adapters"))

from homeostat.house import Entity, SourceRef
from persons import Persons

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


class PresenceTest(unittest.TestCase):
    def setUp(self):
        self.rule = Persons([FREDRIK], clock=lambda: 1000.0)

    def presence(self, p=FREDRIK):
        return self.rule.evaluate(p, ACCURACY).values.get("presence")

    def test_he_drives_off(self):
        self.rule.seen("fredrik_phone", "home", False)
        self.rule.seen("fredriks_phone", "sighted", False)
        self.assertIs(self.presence(), False)

    def test_his_phone_sleeps_at_home(self):
        self.rule.seen("fredrik_phone", "home", True)
        self.rule.seen("fredriks_phone", "sighted", False)
        self.assertIs(self.presence(), True)

    def test_he_arrives_before_the_geofence_does(self):
        self.rule.seen("fredrik_phone", "home", False)
        self.rule.seen("fredriks_phone", "sighted", True)
        self.assertIs(self.presence(), True)

    def test_his_phone_dies_while_he_is_out(self):
        self.rule.seen("fredrik_phone", "home", True)
        self.rule.seen("fredrik_phone", "available", False)
        self.rule.seen("fredriks_phone", "sighted", False)
        out = self.rule.evaluate(FREDRIK, ACCURACY)
        self.assertNotIn("presence", out.values)
        self.assertEqual(out.used[("presence", "geofence")], False)
        self.assertTrue(out.decision.startswith("unchanged"))

    def test_a_sighting_alone_cannot_say_he_left_when_he_has_a_geofence(self):
        self.rule.seen("fredriks_phone", "sighted", False)
        self.assertIsNone(self.presence())

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

    def test_the_deciding_sources_are_named(self):
        self.rule.seen("fredrik_phone", "home", False)
        self.rule.seen("fredriks_phone", "sighted", True)
        out = self.rule.evaluate(FREDRIK, ACCURACY)
        self.assertEqual(out.decision, "home: wifi says so")
        self.assertEqual(out.used[("presence", "geofence")], True)
        self.assertEqual(out.used[("presence", "wifi")], True)


class PositionTest(unittest.TestCase):
    def setUp(self):
        self.now = 1000.0
        self.rule = Persons([FREDRIK], clock=lambda: self.now)

    def fix(self, tracker, lat, lon, **extra):
        self.rule.seen(tracker, "lat", lat)
        self.rule.seen(tracker, "lon", lon)
        for aspect, value in extra.items():
            self.rule.seen(tracker, aspect, value)

    def test_the_newest_fix_wins(self):
        two = person(gps=("phone", "lat"), watch=("watch", "lat"))
        rule = Persons([two])
        for tracker, lat, at in (("phone", 1.0, 200), ("watch", 2.0, 300)):
            rule.seen(tracker, "lat", lat)
            rule.seen(tracker, "lon", 0.0)
            rule.seen(tracker, "fixed_at", at)
        out = rule.evaluate(two, ACCURACY)
        self.assertEqual((out.values["lat"], out.values["fixed_at"]), (2.0, 300))
        self.assertEqual((out.used[("lat", "watch")], out.used[("lat", "gps")]), (True, False))

    def test_an_inaccurate_fix_is_not_used(self):
        self.fix("fredrik_owntracks", 57.7, 11.9, accuracy=2000, fixed_at=900)
        out = self.rule.evaluate(FREDRIK, ACCURACY)
        self.assertNotIn("lat", out.values)
        self.assertEqual(out.used[("lat", "gps")], False)

    def test_a_fix_without_fixed_at_is_dated_on_arrival(self):
        self.rule.seen("fredrik_owntracks", "lat", 57.7, age_s=30.0)
        self.rule.seen("fredrik_owntracks", "lon", 11.9)
        out = self.rule.evaluate(FREDRIK, ACCURACY)
        self.assertEqual(out.values["fixed_at"], 970.0)
        self.assertNotIn("accuracy", out.values)

    def test_battery_stays_on_the_tracker(self):
        self.fix("fredrik_owntracks", 57.7, 11.9, battery=80)
        self.assertNotIn("battery", self.rule.evaluate(FREDRIK, ACCURACY).values)


class AvailabilityTest(unittest.TestCase):
    def test_unavailable_only_when_every_tracker_is(self):
        rule = Persons([FREDRIK])
        rule.seen("fredrik_phone", "available", False)
        rule.seen("fredriks_phone", "available", False)
        # owntracks reports no availability, so the person is not known to be down.
        self.assertIs(rule.evaluate(FREDRIK, ACCURACY).values["available"], True)
        rule.seen("fredrik_owntracks", "available", False)
        self.assertIs(rule.evaluate(FREDRIK, ACCURACY).values["available"], False)


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
