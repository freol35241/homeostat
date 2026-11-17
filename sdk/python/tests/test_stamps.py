"""Stamp ordering, live-or-replay detection and the subscribe merge, without a bus.

Run: uv run --no-project --with-editable sdk/python python -m unittest discover sdk/python/tests
"""

import datetime
import functools
import time
import unittest

from homeostat.automation import Context
from homeostat.stamps import Newest, stamp_us
from homeostat.testing import FakeSample, FakeSession
from houses import automation, house

ROUTER = "f413c7630f684d5b96917cb73d0a46b8"
REPLAY = "01"


@functools.total_ordering
class FakeStamp:
    """Ordered as zenoh orders timestamps: by time, then by publisher ID."""

    def __init__(self, when: datetime.datetime, publisher: str = ROUTER):
        self._when = when
        self._publisher = publisher

    def get_time(self):
        return self._when

    def get_id(self):
        return self._publisher

    def _key(self):
        return (self._when, self._publisher)

    def __eq__(self, other):
        return self._key() == other._key()

    def __lt__(self, other):
        return self._key() < other._key()


def at(seconds_ago: float, publisher: str = ROUTER) -> FakeStamp:
    now = datetime.datetime.now(datetime.UTC)
    return FakeStamp(now - datetime.timedelta(seconds=seconds_ago), publisher)


class StampUsTest(unittest.TestCase):
    def test_a_stamp_converts_to_exact_microseconds(self):
        when = datetime.datetime(2026, 10, 10, 6, 0, 0, 123456, tzinfo=datetime.UTC)
        expected = int(when.timestamp()) * 1_000_000 + 123456
        self.assertEqual(stamp_us(FakeStamp(when)), expected)

    def test_no_stamp_is_none(self):
        self.assertIsNone(stamp_us(None))


class NewestTest(unittest.TestCase):
    def test_the_first_value_for_a_key_is_admitted(self):
        self.assertTrue(Newest().admit("k", at(5)))

    def test_a_newer_stamp_is_admitted_and_an_older_or_equal_one_is_not(self):
        newest = Newest()
        old, new = at(60), at(1)
        self.assertTrue(newest.admit("k", old))
        self.assertTrue(newest.admit("k", new))
        self.assertFalse(newest.admit("k", old), "a replay after a live value")
        self.assertFalse(newest.admit("k", new), "the same value twice")

    def test_keys_are_ordered_independently(self):
        newest = Newest()
        self.assertTrue(newest.admit("a", at(1)))
        self.assertTrue(newest.admit("b", at(60)))

    def test_a_catch_up_of_the_value_already_delivered_is_dropped(self):
        newest = Newest()
        stamp = at(1)
        self.assertTrue(newest.admit("k", stamp))
        self.assertFalse(newest.admit("k", stamp, catch_up=True))

    def test_an_older_catch_up_is_dropped_and_a_newer_one_admitted(self):
        newest = Newest()
        self.assertTrue(newest.admit("k", at(30)))
        self.assertFalse(newest.admit("k", at(60), catch_up=True))
        self.assertTrue(newest.admit("k", at(1), catch_up=True))

    def test_without_stamps_live_always_wins_and_a_late_catch_up_does_not(self):
        newest = Newest()
        self.assertTrue(newest.admit("k", None))
        self.assertTrue(newest.admit("k", None))
        self.assertFalse(newest.admit("k", None, catch_up=True))
        self.assertTrue(Newest().admit("k", None, catch_up=True))

    def test_a_stamped_value_after_an_unstamped_one_is_admitted(self):
        newest = Newest()
        self.assertTrue(newest.admit("k", None))
        self.assertTrue(newest.admit("k", at(60)))


class LiveOrReplayTest(unittest.TestCase):
    def test_no_stamp_is_live(self):
        sess = FakeSession(routers=[ROUTER])
        sample = FakeSample("k", b"1")
        self.assertTrue(sess.is_live(sample))
        self.assertEqual(sess.sample_age(sample), 0.0)

    def test_the_routers_stamp_is_live_even_when_it_is_old(self):
        # A live sample is stamped on arrival, so its age is not read from
        # the stamp: a clock difference between hosts must not age it.
        sess = FakeSession(routers=[ROUTER])
        sample = FakeSample("k", b"1", timestamp=at(30, ROUTER))
        self.assertTrue(sess.is_live(sample))
        self.assertEqual(sess.sample_age(sample), 0.0)

    def test_a_publishers_stamp_is_a_replay_aged_from_the_stamp(self):
        sess = FakeSession(routers=[ROUTER])
        sample = FakeSample("k", b"1", timestamp=at(600, REPLAY))
        self.assertFalse(sess.is_live(sample))
        self.assertAlmostEqual(sess.sample_age(sample), 600, delta=2)

    def test_a_replay_stamped_in_the_future_is_age_zero(self):
        sess = FakeSession(routers=[ROUTER])
        sample = FakeSample("k", b"1", timestamp=at(-60, REPLAY))
        self.assertEqual(sess.sample_age(sample), 0.0)

    def test_with_no_router_known_every_sample_is_live(self):
        sess = FakeSession()
        self.assertTrue(sess.is_live(FakeSample("k", b"1", timestamp=at(600, REPLAY))))

    def test_the_router_is_looked_up_once(self):
        calls = []
        sess = FakeSession(routers=[ROUTER])
        sess._session.info.routers_zid = lambda: calls.append(1) or [ROUTER]
        for _ in range(3):
            sess.is_live(FakeSample("k", b"1", timestamp=at(1, ROUTER)))
        self.assertEqual(len(calls), 1)


class SubscribeMergeTest(unittest.TestCase):
    """`subscribe`'s catch-up against live samples and replays, ordered by stamp."""

    KEY = "home/state/livingroom/lamp/on"

    def context(self, catch_up, during_get=None):
        """A context whose bus answers the catch-up get with `catch_up`.

        `during_get(deliver)` runs inside the get, as a live sample that
        lands between subscribing and the get's answer. The session is
        `self.session`, and the handler appends to `self.delivered`.
        """
        self.session = FakeSession("lamp", routers=[ROUTER])
        for key, value, age_s, stamp in catch_up:
            self.session.serve(key, value, age_s=age_s, stamp=stamp)
        if during_get:
            served = self.session.get_json_stamped

            def get_json_stamped(selector, *, timeout_s=None):
                if selector == self.KEY:
                    during_get(self.session.deliver)
                return served(selector, timeout_s=timeout_s)

            self.session.get_json_stamped = get_json_stamped
        manifest = automation("lamp", f'[bus.subscribes]\nlamp = "{self.KEY}"\n')
        self.delivered = []
        return Context("lamp", house(self, {"units/lamp.toml": manifest}), session=self.session)

    def handler(self):
        return lambda key, value, age_s: self.delivered.append((value, round(age_s)))

    def test_a_catch_up_is_delivered_with_the_mirrors_age(self):
        ctx = self.context([(self.KEY, True, 42.0, at(42))])
        ctx.subscribe("lamp", self.handler())
        self.assertEqual(self.delivered, [(True, 42)])

    def test_a_replay_arriving_live_carries_its_age(self):
        ctx = self.context([])
        ctx.subscribe("lamp", self.handler())
        self.session.deliver(FakeSample(self.KEY, b"true", timestamp=at(300, REPLAY)))
        self.assertEqual(self.delivered, [(True, 300)])

    def test_a_replay_after_a_live_value_is_dropped(self):
        ctx = self.context([])
        ctx.subscribe("lamp", self.handler())
        self.session.deliver(FakeSample(self.KEY, b"false", timestamp=at(0, ROUTER)))
        self.session.deliver(FakeSample(self.KEY, b"true", timestamp=at(300, REPLAY)))
        self.assertEqual(self.delivered, [(False, 0)])

    def test_a_live_value_after_a_replay_is_delivered(self):
        ctx = self.context([])
        ctx.subscribe("lamp", self.handler())
        self.session.deliver(FakeSample(self.KEY, b"true", timestamp=at(300, REPLAY)))
        self.session.deliver(FakeSample(self.KEY, b"false", timestamp=at(0, ROUTER)))
        self.assertEqual(self.delivered, [(True, 300), (False, 0)])

    def test_a_catch_up_older_than_a_live_sample_during_the_get_is_dropped(self):
        stamp = at(60)
        ctx = self.context(
            [(self.KEY, True, 60.0, stamp)],
            during_get=lambda cb: cb(FakeSample(self.KEY, b"false", timestamp=at(0, ROUTER))),
        )
        ctx.subscribe("lamp", self.handler())
        self.assertEqual(self.delivered, [(False, 0)])

    def test_the_catch_up_of_a_value_delivered_live_is_not_repeated(self):
        stamp = at(1, ROUTER)
        ctx = self.context(
            [(self.KEY, True, 1.0, stamp)],
            during_get=lambda cb: cb(FakeSample(self.KEY, b"true", timestamp=stamp)),
        )
        ctx.subscribe("lamp", self.handler())
        self.assertEqual(self.delivered, [(True, 0)])

    def test_a_two_argument_handler_gets_values_without_ages(self):
        ctx = self.context([(self.KEY, True, 42.0, at(42))])
        got = []
        ctx.subscribe("lamp", lambda key, value: got.append((key, value)))
        self.assertEqual(got, [(self.KEY, True)])


class AgeClockTest(unittest.TestCase):
    def test_age_uses_the_wall_clock_in_microseconds(self):
        sess = FakeSession(routers=[ROUTER])
        sample = FakeSample("k", b"1", timestamp=at(2.5, REPLAY))
        before = time.time()
        age = sess.sample_age(sample)
        self.assertGreaterEqual(age, 2.4)
        self.assertLess(age, 2.5 + (time.time() - before) + 0.1)


if __name__ == "__main__":
    unittest.main()
