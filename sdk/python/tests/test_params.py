"""LiveParams against a stub session: what a config value must be to be
believed.

Run: uv run --no-project --with-editable sdk/python python -m unittest discover sdk/python/tests
"""

import json
import unittest

from homeostat.params import LiveParams


class FakeSample:
    def __init__(self, key, value):
        self.key_expr = key
        self.payload = self
        self._value = value

    def to_bytes(self):
        return json.dumps(self._value).encode()


sample = FakeSample


class FakeSession:
    unit = "u"

    def __init__(self, served):
        self.served = served
        self.callback = None

    def subscribe(self, keyexpr, callback):
        self.callback = callback
        return object()

    def get_json(self, selector):
        return self.served


class LiveParamsTest(unittest.TestCase):
    def test_seeds_numbers_and_ignores_the_rest(self):
        session = FakeSession([
            ("home/config/u/hold_minutes", 5),
            ("home/config/u/timeout_s", True),  # a bool is not a number
            ("home/config/u/unknown", 1),  # not one of this unit's parameters
        ])
        params = LiveParams(session, {"hold_minutes": 30.0, "timeout_s": 300.0})
        self.assertEqual(params.get("hold_minutes"), 5)
        self.assertEqual(params.get("timeout_s"), 300.0)
        with self.assertRaises(KeyError):
            params.get("unknown")

    def test_live_values_reject_bool_nan_and_unknown_names(self):
        session = FakeSession([])
        params = LiveParams(session, {"hold_minutes": 30.0})
        session.callback(sample("home/config/u/hold_minutes", 2.5))
        self.assertEqual(params.get("hold_minutes"), 2.5)
        session.callback(sample("home/config/u/hold_minutes", False))
        self.assertEqual(params.get("hold_minutes"), 2.5)
        session.callback(sample("home/config/u/hold_minutes", float("nan")))
        self.assertEqual(params.get("hold_minutes"), 2.5)
        session.callback(sample("home/config/u/hold_minutes", float("inf")))
        self.assertEqual(params.get("hold_minutes"), 2.5)
        session.callback(sample("home/config/u/hold_minutes", "7"))
        self.assertEqual(params.get("hold_minutes"), 2.5)
        session.callback(sample("home/config/u/other", 9))
        with self.assertRaises(KeyError):
            params.get("other")


if __name__ == "__main__":
    unittest.main()
