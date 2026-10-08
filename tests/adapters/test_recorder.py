"""The recorder's read path and calendar, without a store or a bus."""

import sys
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[2] / "adapters"))

from recorder import (
    KINDS,
    MAX_QUERY_LIMIT,
    archive_boundary_us,
    as_issues,
    bucketed,
    changes_only,
    event_payload,
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
