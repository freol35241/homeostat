"""UnitSession's pure parts over FakeSession's fake zenoh session: what
put_json refuses to publish, and the shared command prologue.

Run: uv run --no-project --with-editable sdk/python python -m unittest discover sdk/python/tests
"""

import datetime
import json
import threading
import unittest

from homeostat import keys
from homeostat.session import ConfigWriteError, QueryError, QueryTimeout
from homeostat.stamps import stamp_us
from homeostat.testing import FakeSample, FakeSession


class PutJsonTest(unittest.TestCase):
    def test_finite_values_publish(self):
        session = FakeSession("u")
        session.put_json("home/state/r/e/a", 21.5)
        session.put_json("home/state/r/e/b", {"nested": [1, "x", None]})
        self.assertEqual(
            session.puts,
            [("home/state/r/e/a", 21.5), ("home/state/r/e/b", {"nested": [1, "x", None]})],
        )

    def test_non_finite_drops_with_an_event_and_never_publishes(self):
        session = FakeSession("u")
        for value in (float("nan"), float("inf"), -float("inf"), {"deep": [float("nan")]}):
            session.events.clear()
            session.put_json("home/state/r/e/a", value)
            self.assertEqual(session.puts, [])
            self.assertEqual(
                session.events,
                [{"kind": "drop", "reason": "non-finite", "key": "home/state/r/e/a"}],
            )


class StampedPutTest(unittest.TestCase):
    """`at` stamps a state value with the time it describes. The stamp
    carries the session's ID, which is not the router's, so consumers take
    the value as stamped rather than live."""

    HOUR = datetime.datetime(2026, 11, 7, 12, 0, tzinfo=datetime.timezone.utc)

    def test_a_state_value_carries_the_time_it_describes(self):
        session = FakeSession("u")
        local = datetime.timezone(datetime.timedelta(hours=1))
        session.put_json("home/state/garden/rain/precipitation", 0.4, at=self.HOUR.astimezone(local))
        (options,) = session.put_options
        stamp = options["timestamp"]
        self.assertEqual(stamp_us(stamp), int(self.HOUR.timestamp()) * 1_000_000)
        self.assertEqual(stamp.get_id(), session._session.new_timestamp().get_id())

    def test_a_stamp_is_refused_where_it_cannot_mean_anything(self):
        session = FakeSession("u")
        future = datetime.datetime.now(datetime.timezone.utc) + datetime.timedelta(minutes=5)
        for key, at in (
            ("home/cmd/garden/valve/on", self.HOUR),
            ("home/state/garden/rain/precipitation", self.HOUR.replace(tzinfo=None)),
            ("home/state/garden/rain/precipitation", future),
        ):
            with self.subTest(key=key, at=at), self.assertRaises(ValueError):
                session.put_json(key, 1.0, at=at)
        self.assertEqual((session.puts, session.events), ([], []))


class ParseCommandTest(unittest.TestCase):
    def test_envelope_yields_aspect_value_and_id(self):
        session = FakeSession("u")
        sample = FakeSample(
            "home/cmd/kitchen/lamp/brightness",
            json.dumps(
                {"value": 200, "priority": "manual", "actor": "test", "id": "c0ffee01"}
            ).encode(),
        )
        self.assertEqual(session.parse_command(sample), ("brightness", 200, "c0ffee01"))
        self.assertEqual((session.puts, session.events), ([], []))

    def test_an_envelope_without_an_id_still_parses(self):
        # The id is optional on the wire; an adapter must not refuse a
        # command for the lack of one.
        session = FakeSession("u")
        sample = FakeSample(
            "home/cmd/kitchen/lamp/brightness",
            json.dumps({"value": 200, "priority": "manual", "actor": "test"}).encode(),
        )
        self.assertEqual(session.parse_command(sample), ("brightness", 200, None))
        self.assertEqual(
            session.parse_command_with_options(sample), ("brightness", 200, None, {})
        )

    def test_an_envelope_hands_back_its_options(self):
        session = FakeSession("u")
        sample = FakeSample(
            "home/cmd/kitchen/lamp/brightness",
            json.dumps(
                {"value": 200, "priority": "automation", "options": {"transition": 1.5}}
            ).encode(),
        )
        self.assertEqual(
            session.parse_command_with_options(sample),
            ("brightness", 200, None, {"transition": 1.5}),
        )
        # parse_command leaves the options out, so a three-value unpack works.
        self.assertEqual(session.parse_command(sample), ("brightness", 200, None))

    def test_options_that_are_not_an_object_are_invalid_command(self):
        session = FakeSession("u")
        sample = FakeSample(
            "home/cmd/kitchen/lamp/on",
            json.dumps({"value": True, "priority": "manual", "options": [1], "id": "badbeef1"}).encode(),
        )
        self.assertIsNone(session.parse_command(sample))
        self.assertEqual(session.events[0]["reason"], "invalid-command")
        self.assertEqual(session.events[0]["cmd_id"], "badbeef1")

    def test_non_json_is_malformed_payload(self):
        session = FakeSession("u")
        sample = FakeSample("home/cmd/kitchen/lamp/on", b"not json")
        self.assertIsNone(session.parse_command(sample))
        self.assertEqual(
            session.events,
            [{"kind": "drop", "reason": "malformed-payload", "key": "home/cmd/kitchen/lamp/on"}],
        )

    def test_bare_value_is_invalid_command(self):
        # A bare value is JSON, so it reaches the envelope check — but it is
        # not an object and so carries no id to report.
        session = FakeSession("u")
        sample = FakeSample("home/cmd/kitchen/lamp/on", b"true")
        self.assertIsNone(session.parse_command(sample))
        self.assertEqual(
            session.events,
            [{"kind": "drop", "reason": "invalid-command", "key": "home/cmd/kitchen/lamp/on", "cmd_id": None}],
        )

    def test_a_bad_envelope_carrying_an_id_reports_it(self):
        # The stage-3 case the dashboard needs: the command is dead, and the
        # drop names which one, so a pending control resolves instead of
        # waiting out its timeout.
        session = FakeSession("u")
        sample = FakeSample(
            "home/cmd/kitchen/lamp/on",
            json.dumps({"value": True, "priority": "urgent", "id": "badbeef0"}).encode(),
        )
        self.assertIsNone(session.parse_command(sample))
        self.assertEqual(session.events[0]["cmd_id"], "badbeef0")


    def test_a_report_or_a_release_asks_the_device_for_nothing(self):
        # An adapter receives these only for an aspect nothing arbitrates.
        # There is no hold to keep, and the device must not be driven.
        session = FakeSession("u")
        for envelope in (
            {"value": True, "priority": "manual", "hold": "report"},
            {"priority": "manual", "hold": "release"},
        ):
            sample = FakeSample("home/cmd/kitchen/lamp/on", json.dumps(envelope).encode())
            self.assertIsNone(session.parse_command(sample), envelope)
        self.assertEqual((session.puts, session.events), ([], []), "no drop event either")

    def test_a_command_with_a_hold_until_or_none_reaches_the_device(self):
        session = FakeSession("u")
        for hold in ("none", "2026-11-17T06:00:00+01:00"):
            sample = FakeSample(
                "home/cmd/kitchen/lamp/on",
                json.dumps({"value": True, "priority": "manual", "hold": hold}).encode(),
            )
            self.assertEqual(session.parse_command(sample), ("on", True, None), hold)


class GetReplyTest(unittest.TestCase):
    """What a get makes of the replies zenoh hands back."""

    KEY = "home/history/stats"

    def test_an_error_reply_raises_with_the_queryables_message(self):
        session = FakeSession("u")
        session.fail(self.KEY, json.dumps({"error": "store unavailable: disk I/O error"}))
        with self.assertRaisesRegex(QueryError, "^store unavailable: disk I/O error$") as caught:
            session.get_json(self.KEY)
        self.assertNotIsInstance(caught.exception, QueryTimeout)

    def test_zenohs_own_timeout_raises_query_timeout(self):
        session = FakeSession("u")
        session.fail(self.KEY, "Timeout")
        with self.assertRaises(QueryTimeout):
            session.get_json(self.KEY)

    def test_a_reply_without_an_attachment_is_age_zero(self):
        session = FakeSession("u")
        session.serve("home/state/r/e/a", 1, age_s=None)
        session.serve("home/state/r/e/b", 2, age_s=3.0)
        self.assertEqual(
            session.get_json_aged("home/state/r/e/*"),
            [("home/state/r/e/a", 1, 0.0), ("home/state/r/e/b", 2, 3.0)],
        )


class WriteConfigTest(unittest.TestCase):
    KEY = keys.config_key("pump", "hold_minutes")

    def test_a_rejected_write_raises_with_the_cores_message(self):
        session = FakeSession("u")
        session.fail(self.KEY, json.dumps({"error": "90 is above the maximum 60"}))
        with self.assertRaisesRegex(ConfigWriteError, "^90 is above the maximum 60$"):
            session.write_config("pump", "hold_minutes", 90)

    def test_a_write_nobody_answers_raises(self):
        session = FakeSession("u")
        session.fail(self.KEY)
        with self.assertRaisesRegex(ConfigWriteError, "no reply"):
            session.write_config("pump", "hold_minutes", 30)


class DeleteTest(unittest.TestCase):
    def test_a_delete_goes_to_the_bus_as_one(self):
        session = FakeSession("u")
        session.delete("home/hold/arbiter/r/e/a")
        self.assertEqual(session.deletes, ["home/hold/arbiter/r/e/a"])


class HasSubscriberTest(unittest.TestCase):
    def test_answers_from_the_publishers_matching_status(self):
        session = FakeSession("u")
        session.subscribed_elsewhere.add("home/cmd/r/lamp/on")
        self.assertTrue(session.has_subscriber("home/cmd/r/lamp/on"))
        self.assertFalse(session.has_subscriber("home/cmd/r/gone/on", wait_s=0.05))

    def test_a_subscriber_that_arrives_during_the_wait_counts(self):
        session = FakeSession("u")
        key = "home/cmd/r/lamp/on"
        threading.Timer(0.05, lambda: session.subscribed_elsewhere.add(key)).start()
        self.assertTrue(session.has_subscriber(key, wait_s=2))

    def test_one_publisher_per_key(self):
        session = FakeSession("u")
        declared = []
        declare = session._session.declare_publisher
        session._session.declare_publisher = lambda key: declared.append(key) or declare(key)
        for _ in range(3):
            session.has_subscriber("home/cmd/r/gone/on", wait_s=0)
        self.assertEqual(declared, ["home/cmd/r/gone/on"])


class CommandPutTest(unittest.TestCase):
    def test_commands_block_and_go_first_while_data_keeps_the_defaults(self):
        # zenoh drops a put on a congested link by default: right for a
        # reading, wrong for "unlock the door".
        import zenoh

        session = FakeSession("u")
        session.put_json("home/cmd/hall/door/locked", {"value": False})
        session.put_json("home/arbiter/hall/door/locked", {"value": False})
        session.put_json("home/state/hall/door/locked", True)
        command = {
            "congestion_control": zenoh.CongestionControl.BLOCK,
            "priority": zenoh.Priority.INTERACTIVE_HIGH,
        }
        self.assertEqual(session.put_options, [command, command, {}])


class IsAliveTest(unittest.TestCase):
    def test_answers_from_the_units_liveliness_token(self):
        session = FakeSession("u")
        session.alive.add("zigbee")
        self.assertTrue(session.is_alive("zigbee"))
        self.assertFalse(session.is_alive("esphome"))


if __name__ == "__main__":
    unittest.main()
