"""LiveParams against a stub session: what a config value must be to be
believed.

Run: uv run --no-project --with-editable sdk/python python -m unittest discover sdk/python/tests
"""

import unittest

from homeostat.params import LiveParams
from homeostat.testing import FakeSample, FakeSession


class LiveParamsTest(unittest.TestCase):
    def test_seeds_numbers_and_ignores_the_rest(self):
        session = FakeSession("u", served={
            "home/config/u/hold_minutes": 5,
            "home/config/u/timeout_s": True,  # a bool is not a number
            "home/config/u/unknown": 1,  # not one of this unit's parameters
        })
        params = LiveParams(session, {"hold_minutes": 30.0, "timeout_s": 300.0})
        self.assertEqual(params.get("hold_minutes"), 5)
        self.assertEqual(params.get("timeout_s"), 300.0)
        with self.assertRaises(KeyError):
            params.get("unknown")

    def test_live_values_reject_bool_nan_and_unknown_names(self):
        session = FakeSession("u")
        params = LiveParams(session, {"hold_minutes": 30.0})
        session.deliver(FakeSample("home/config/u/hold_minutes", 2.5))
        self.assertEqual(params.get("hold_minutes"), 2.5)
        session.deliver(FakeSample("home/config/u/hold_minutes", False))
        self.assertEqual(params.get("hold_minutes"), 2.5)
        session.deliver(FakeSample("home/config/u/hold_minutes", float("nan")))
        self.assertEqual(params.get("hold_minutes"), 2.5)
        session.deliver(FakeSample("home/config/u/hold_minutes", float("inf")))
        self.assertEqual(params.get("hold_minutes"), 2.5)
        session.deliver(FakeSample("home/config/u/hold_minutes", "7"))
        self.assertEqual(params.get("hold_minutes"), 2.5)
        session.deliver(FakeSample("home/config/u/other", 9))
        with self.assertRaises(KeyError):
            params.get("other")


    def test_an_invalid_live_value_during_the_seed_keeps_the_served_one(self):
        # The subscription delivers a bad value before the seed's get
        # returns. It is not kept, so it must not stop the served one.
        session = FakeSession("u", served={"home/config/u/hold_minutes": 5})
        original = session.subscribe

        def subscribe(keyexpr, callback):
            sub = original(keyexpr, callback)
            callback(FakeSample("home/config/u/hold_minutes", "seven"))
            return sub

        session.subscribe = subscribe
        params = LiveParams(session, {"hold_minutes": 30.0})
        self.assertEqual(params.get("hold_minutes"), 5)


if __name__ == "__main__":
    unittest.main()
