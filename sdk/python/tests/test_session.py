"""UnitSession's pure parts over a stubbed zenoh session: what put_json
refuses to publish, and the shared command prologue.

Run: uv run --no-project --with-editable sdk/python python -m unittest discover sdk/python/tests
"""

import json
import unittest

from homeostat.session import UnitSession


class FakeZenoh:
    def __init__(self):
        self.puts = []

    def put(self, key, payload):
        self.puts.append((key, json.loads(payload)))


class FakeSample:
    def __init__(self, key, raw: bytes):
        self.key_expr = key
        self.payload = self
        self._raw = raw

    def to_bytes(self):
        return self._raw


def stub_session():
    session = UnitSession.__new__(UnitSession)
    session.unit = "u"
    session._session = FakeZenoh()
    session._token = None
    return session


class PutJsonTest(unittest.TestCase):
    def test_finite_values_publish(self):
        session = stub_session()
        session.put_json("home/state/r/e/a", 21.5)
        session.put_json("home/state/r/e/b", {"nested": [1, "x", None]})
        self.assertEqual(
            session._session.puts,
            [("home/state/r/e/a", 21.5), ("home/state/r/e/b", {"nested": [1, "x", None]})],
        )

    def test_non_finite_drops_with_an_event_and_never_publishes(self):
        session = stub_session()
        for value in (float("nan"), float("inf"), -float("inf"), {"deep": [float("nan")]}):
            session._session.puts.clear()
            session.put_json("home/state/r/e/a", value)
            self.assertEqual(
                session._session.puts,
                [("home/health/u/event", {"kind": "drop", "reason": "non-finite", "key": "home/state/r/e/a"})],
            )


class ParseCommandTest(unittest.TestCase):
    def test_envelope_yields_aspect_and_value(self):
        session = stub_session()
        sample = FakeSample(
            "home/cmd/kitchen/lamp/brightness",
            json.dumps({"value": 200, "priority": "manual", "actor": "test"}).encode(),
        )
        self.assertEqual(session.parse_command(sample), ("brightness", 200))
        self.assertEqual(session._session.puts, [])

    def test_non_json_is_malformed_payload(self):
        session = stub_session()
        sample = FakeSample("home/cmd/kitchen/lamp/on", b"not json")
        self.assertIsNone(session.parse_command(sample))
        self.assertEqual(
            session._session.puts,
            [("home/health/u/event", {"kind": "drop", "reason": "malformed-payload", "key": "home/cmd/kitchen/lamp/on"})],
        )

    def test_bare_value_is_invalid_command(self):
        session = stub_session()
        sample = FakeSample("home/cmd/kitchen/lamp/on", b"true")
        self.assertIsNone(session.parse_command(sample))
        self.assertEqual(
            session._session.puts,
            [("home/health/u/event", {"kind": "drop", "reason": "invalid-command", "key": "home/cmd/kitchen/lamp/on"})],
        )


if __name__ == "__main__":
    unittest.main()
