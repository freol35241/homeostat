"""The recorder's read path and calendar, without a store or a bus."""

import datetime
import sqlite3
import sys
import tempfile
import threading
import types
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[2] / "adapters"))

from recorder import (
    KINDS,
    MAX_QUERY_LIMIT,
    SEED_TOLERANCE_US,
    Recorder,
    archive_boundary_us,
    as_issues,
    bucketed,
    changes_only,
    event_payload,
    holds_state_at,
    init_store,
    latest_state,
    month_start_us,
    parse_event_params,
    parse_forecast_params,
    parse_limit,
    parse_params,
    rows_per_day,
    split_selector,
    typed,
)

S = 1_000_000  # one second in µs
NUMBER, BOOL = KINDS.index("number"), KINDS.index("bool")
JAN_2026 = month_start_us(2026, 1)


class CalendarTest(unittest.TestCase):
    def test_months_carry_into_the_year(self):
        self.assertEqual(month_start_us(2025, 13), JAN_2026)
        self.assertEqual(month_start_us(2026, 0), month_start_us(2025, 12))

    def test_the_archive_boundary_is_the_start_of_a_closed_month(self):
        mid_october = month_start_us(2026, 10) + 14 * 86_400 * S
        self.assertEqual(archive_boundary_us(mid_october, 1), month_start_us(2026, 9))
        self.assertEqual(archive_boundary_us(mid_october, 12), month_start_us(2025, 10))


class ValuesTest(unittest.TestCase):
    def test_only_finite_scalars_are_stored(self):
        self.assertEqual(typed(True), ("bool", 1))
        self.assertEqual(typed(2.5), ("number", 2.5))
        self.assertEqual(typed("on"), ("string", "on"))
        for value in (float("nan"), float("inf"), None, [1], {"a": 1}):
            with self.subTest(value=value):
                self.assertIsNone(typed(value))

    def test_a_rate_needs_a_span(self):
        self.assertEqual(rows_per_day(2, 0, 86_400 * S), 2.0)
        self.assertIsNone(rows_per_day(1, 5, 5))

    def test_a_non_json_event_is_served_as_its_string(self):
        self.assertEqual(event_payload('{"kind": "drop"}'), {"kind": "drop"})
        self.assertEqual(event_payload("not json"), "not json")


class SelectorTest(unittest.TestCase):
    def test_parameters_split_without_url_decoding(self):
        self.assertEqual(
            split_selector("from=2026-01-01T00:00:00+01:00;limit=5;;"),
            {"from": "2026-01-01T00:00:00+01:00", "limit": "5"},
        )

    def test_a_samples_window_reads_rfc3339_with_an_offset(self):
        from_us, to_us, limit, bucket, changes = parse_params(
            "from=2026-01-01T01:00:00+01:00;to=2026-01-01T00:01:00Z;limit=20000"
        )
        self.assertEqual((from_us, to_us - from_us), (JAN_2026, 60 * S))
        self.assertEqual((limit, bucket, changes), (MAX_QUERY_LIMIT, 0, False))

    def test_bad_samples_parameters_are_refused(self):
        for raw in (
            "from=2026-01-01T00:00:00",
            "from=yesterday",
            "limit=0",
            "bucket=0",
            "changes=yes",
            "bucket=60;changes=1",
            "from=2000-01-01T00:00:00Z;to=2026-01-01T00:00:00Z;bucket=1",
        ):
            with self.subTest(raw=raw), self.assertRaises(ValueError):
                parse_params(raw)

    def test_limit_is_positive_and_clamped(self):
        self.assertEqual(parse_limit("3"), 3)
        self.assertEqual(parse_limit(str(MAX_QUERY_LIMIT * 2)), MAX_QUERY_LIMIT)
        with self.assertRaises(ValueError):
            parse_limit("-1")

    def test_a_forecast_read_is_at_or_a_closed_window(self):
        at, _, _, _ = parse_forecast_params("at=2026-01-01T00:00:00Z")
        self.assertEqual(at, JAN_2026)
        at, lo, hi, _ = parse_forecast_params(
            "valid_from=2026-01-01T00:00:00Z;valid_to=2026-01-01T01:00:00Z"
        )
        self.assertEqual((at, hi - lo), (None, 3600 * S))
        for raw in ("at=2026-01-01T00:00:00Z;valid_from=2026-01-01T00:00:00Z",
                    "valid_from=2026-01-01T00:00:00Z"):
            with self.subTest(raw=raw), self.assertRaises(ValueError):
                parse_forecast_params(raw)

    def test_an_events_read_takes_microseconds_and_a_key_expression(self):
        key, lo, hi, limit = parse_event_params("key=home/health/**;from=1;to=2;limit=3")
        self.assertEqual((key, lo, hi, limit), ("home/health/**", 1, 2, 3))
        for raw in ("from=2026-01-01T00:00:00Z", f"to={2**63}", "key=a//b"):
            with self.subTest(raw=raw), self.assertRaises(ValueError):
                parse_event_params(raw)


class FoldTest(unittest.TestCase):
    def test_a_number_bucket_is_its_mean_with_min_and_max(self):
        rows = [(0, "a", NUMBER, 1.0), (30 * S, "a", NUMBER, 3.0), (60 * S, "b", NUMBER, 5.0)]
        points = bucketed(rows, 60 * S, limit=10)
        self.assertEqual(
            [(p["value"], p["min"], p["max"], p["room"]) for p in points],
            [(2.0, 1.0, 3.0, "a"), (5.0, 5.0, 5.0, "b")],
        )

    def test_a_bool_bucket_is_its_last_value_and_the_newest_are_kept(self):
        rows = [(0, "a", BOOL, 1), (1, "a", BOOL, 0), (60 * S, "a", BOOL, 1)]
        points = bucketed(rows, 60 * S, limit=1)
        self.assertEqual([(p["value"], "min" in p) for p in points], [(True, False)])

    def test_changes_keep_each_run_start(self):
        rows = [(t, "a", BOOL, v) for t, v in enumerate([1, 1, 0, 0, 1])]
        self.assertEqual([r[0] for r in changes_only(rows, limit=10)], [0, 2, 4])
        self.assertEqual([r[0] for r in changes_only(rows, limit=2)], [2, 4])

    def test_issues_group_points_and_keep_the_newest(self):
        rows = [(1, JAN_2026, None, 1.0), (2, JAN_2026, JAN_2026 + 3600 * S, 2.0)]
        issues = as_issues(rows, limit=1)
        self.assertEqual(len(issues), 1)
        self.assertEqual(issues[0]["points"], [{"t": "2026-01-01T00:00:00.000000+00:00",
                                                 "v": 2.0, "d": 3600.0}])


if __name__ == "__main__":
    unittest.main()


class StoreTest(unittest.TestCase):
    """A real store in a temporary file, filled with SQL, without the writer."""

    def setUp(self):
        self.dir = tempfile.TemporaryDirectory()
        self.db = Path(self.dir.name) / "store.db"
        init_store(self.db)
        self.conn = sqlite3.connect(self.db)

    def tearDown(self):
        self.conn.close()
        self.dir.cleanup()

    def row(self, space, room, entity, aspect, ts, value):
        conn = self.conn
        conn.execute("INSERT OR IGNORE INTO rooms (name) VALUES (?)", (room,))
        conn.execute(
            "INSERT OR IGNORE INTO series (class, entity, aspect) VALUES (?, ?, ?)",
            (space, entity, aspect),
        )
        (series_id,) = conn.execute(
            "SELECT id FROM series WHERE class = ? AND entity = ? AND aspect = ? AND source = ''",
            (space, entity, aspect),
        ).fetchone()
        (room_id,) = conn.execute("SELECT id FROM rooms WHERE name = ?", (room,)).fetchone()
        kind, stored = typed(value)
        conn.execute(
            "INSERT INTO samples VALUES (?, ?, ?, ?, ?)",
            (series_id, ts, room_id, KINDS.index(kind), stored),
        )
        conn.commit()


class LatestStateTest(StoreTest):
    def test_each_state_series_gives_its_newest_row_typed(self):
        self.row("state", "livingroom", "lamp", "on", 1 * S, False)
        self.row("state", "livingroom", "lamp", "on", 2 * S, True)
        self.row("state", "kitchen", "temp", "temperature", 5 * S, 21.5)
        self.row("state", "hall", "door", "mode", 3 * S, "away")
        self.assertEqual(
            latest_state(self.conn),
            [
                {"key": "home/state/hall/door/mode", "value": "away", "ts": 3 * S},
                {"key": "home/state/livingroom/lamp/on", "value": True, "ts": 2 * S},
                {"key": "home/state/kitchen/temp/temperature", "value": 21.5, "ts": 5 * S},
            ],
        )

    def test_the_room_is_the_newest_rows(self):
        # An entity that moved is listed under the room it was last
        # recorded in. The core decides whether that is still where it is.
        self.row("state", "kitchen", "lamp", "on", 1 * S, True)
        self.row("state", "hallway", "lamp", "on", 2 * S, False)
        self.assertEqual(
            latest_state(self.conn),
            [{"key": "home/state/hallway/lamp/on", "value": False, "ts": 2 * S}],
        )

    def test_commands_are_not_listed(self):
        self.row("cmd", "livingroom", "lamp", "on", 1 * S, True)
        self.assertEqual(latest_state(self.conn), [])

    def test_an_empty_store_lists_nothing(self):
        self.assertEqual(latest_state(self.conn), [])


class HoldsStateTest(StoreTest):
    def test_a_row_at_or_after_the_time_is_held_within_the_tolerance(self):
        self.row("state", "livingroom", "lamp", "on", 10 * S, True)
        self.assertTrue(holds_state_at(self.conn, "lamp", "on", 10 * S))
        self.assertTrue(holds_state_at(self.conn, "lamp", "on", 10 * S + SEED_TOLERANCE_US))
        self.assertFalse(
            holds_state_at(self.conn, "lamp", "on", 10 * S + SEED_TOLERANCE_US + 1)
        )

    def test_an_unknown_series_is_not_held(self):
        self.assertFalse(holds_state_at(self.conn, "lamp", "on", 1))


class StampedRecordTest(StoreTest):
    """`record` for a sample whose publisher set the stamp: the core's replay."""

    KEY = "home/state/livingroom/lamp/on"

    def recorder(self, live):
        rec = Recorder.__new__(Recorder)
        rec.db_path = self.db
        rec.sess = types.SimpleNamespace(is_live=lambda sample: live)
        rec.queued = []
        rec.writer = types.SimpleNamespace(enqueue=lambda table, row: rec.queued.append((table, row)))
        rec._live = set()
        rec._live_lock = threading.Lock()
        return rec

    def sample(self, payload: bytes, ts_us: int):
        when = datetime.datetime(1970, 1, 1, tzinfo=datetime.UTC) + datetime.timedelta(
            microseconds=ts_us
        )
        return types.SimpleNamespace(
            key_expr=self.KEY,
            payload=types.SimpleNamespace(to_bytes=lambda: payload),
            timestamp=types.SimpleNamespace(get_time=lambda: when),
        )

    def test_the_replay_of_a_recorded_row_is_not_recorded_again(self):
        self.row("state", "livingroom", "lamp", "on", 10 * S, True)
        rec = self.recorder(live=False)
        rec.record(self.sample(b"true", 10 * S))
        self.assertEqual(rec.queued, [])

    def test_a_stamped_value_newer_than_the_store_is_recorded_at_its_stamp(self):
        self.row("state", "livingroom", "lamp", "on", 10 * S, True)
        rec = self.recorder(live=False)
        rec.record(self.sample(b"false", 20 * S))
        self.assertEqual(
            rec.queued,
            [("samples", (20 * S, "state", "livingroom", "lamp", "on", KINDS.index("bool"), 0))],
        )

    def test_a_replay_does_not_count_as_live_for_the_seed(self):
        rec = self.recorder(live=False)
        rec.record(self.sample(b"true", 20 * S))
        self.assertEqual(rec._live, set())

    def test_a_live_sample_is_stamped_on_receipt(self):
        rec = self.recorder(live=True)
        before = datetime.datetime.now(datetime.UTC).timestamp() * S
        rec.record(self.sample(b"true", 1 * S))
        (table, row), = rec.queued
        self.assertEqual(table, "samples")
        self.assertGreaterEqual(row[0], before)
        self.assertEqual(rec._live, {self.KEY})
