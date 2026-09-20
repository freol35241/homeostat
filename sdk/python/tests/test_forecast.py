"""Forecast payloads and resampling: pure, no bus.

Run: uv run --no-project --with-editable sdk/python python -m unittest discover sdk/python/tests
"""

import datetime
import json
import unittest

from homeostat.forecast import MAX_POINTS, Forecast, Point, decode, encode

UTC = datetime.timezone.utc


def t(hour, minute=0):
    return datetime.datetime(2026, 9, 20, hour, minute, tzinfo=UTC)


def forecast(pairs, issued=None):
    """`pairs` are (t, v) instants or (t, v, d) intervals."""
    return Forecast(
        issued=issued or t(8),
        points=tuple(Point(*p) for p in pairs),
    )


class EncodeTest(unittest.TestCase):
    def test_roundtrip_keeps_issue_time_and_points(self):
        payload = encode(t(8), [(t(9), 1.23), (t(10), 1.45)])
        back = decode(payload)
        self.assertEqual(back.issued, t(8))
        self.assertEqual([(p.t, p.v) for p in back.points], [(t(9), 1.23), (t(10), 1.45)])
        self.assertEqual(back.horizon_end, t(10))

    def test_points_may_be_irregular(self):
        # The whole point of the shape: hourly then three-hourly, as real
        # weather sources actually publish.
        payload = encode(t(8), [(t(9), 1.0), (t(10), 2.0), (t(13), 3.0)])
        self.assertEqual([p.t.hour for p in decode(payload).points], [9, 10, 13])

    def test_encode_sorts_rather_than_demanding_sorted_input(self):
        payload = encode(t(8), [(t(11), 3.0), (t(9), 1.0), (t(10), 2.0)])
        self.assertEqual([p.v for p in decode(payload).points], [1.0, 2.0, 3.0])

    def test_encode_accepts_point_objects_and_rfc3339_strings(self):
        payload = encode(t(8), [Point(t(9), 1.0), ("2026-09-20T10:00:00+00:00", 2.0)])
        self.assertEqual([p.t for p in decode(payload).points], [t(9), t(10)])

    def test_refuses_what_a_consumer_could_not_trust(self):
        # Deliberately naive: refusing this is the behaviour under test,
        # since "09:00" names different instants in different places.
        naive = datetime.datetime(2026, 9, 20, 9)  # noqa: DTZ001
        for label, call in [
            ("naive issued", lambda: encode(naive, [(t(9), 1.0)])),
            ("naive point", lambda: encode(t(8), [(naive, 1.0)])),
            ("non-finite", lambda: encode(t(8), [(t(9), float("inf"))])),
            ("nan", lambda: encode(t(8), [(t(9), float("nan"))])),
            ("bool", lambda: encode(t(8), [(t(9), True)])),
            ("string value", lambda: encode(t(8), [(t(9), "warm")])),
            ("duplicate instant", lambda: encode(t(8), [(t(9), 1.0), (t(9), 2.0)])),
        ]:
            with self.subTest(label), self.assertRaises(ValueError):
                call()

    def test_point_guard_is_generous_but_real(self):
        many = [(t(0) + datetime.timedelta(minutes=15 * i), 1.0) for i in range(MAX_POINTS + 1)]
        with self.assertRaises(ValueError):
            encode(t(8), many)
        encode(t(8), many[:MAX_POINTS])  # one under the guard is fine


class ExtentTest(unittest.TestCase):
    """A point may describe a window rather than an instant: an
    accumulation over it, or a value holding across it. Dropping that is
    lossy in the same way dropping irregular spacing would be."""

    def test_extent_survives_a_roundtrip_and_instants_stay_bare(self):
        payload = encode(t(8), [(t(9), 1.0), (t(10), 2.0, 10800)])
        # written only where a producer gave one, so an instant series is
        # unchanged on the wire
        raw = json.loads(payload)
        self.assertNotIn("d", raw["points"][0])
        self.assertEqual(raw["points"][1]["d"], 10800.0)
        back = decode(payload)
        self.assertIsNone(back.points[0].d)
        self.assertEqual(back.points[1].d, 10800.0)

    def test_an_interval_covers_its_window_half_open(self):
        p = Point(t(9), 5.0, 3600)
        self.assertTrue(p.covers(t(9)))
        self.assertTrue(p.covers(t(9, 59)))
        self.assertFalse(p.covers(t(10)))  # the next window's edge
        self.assertFalse(p.covers(t(8, 59)))

    def test_an_instant_covers_only_itself(self):
        p = Point(t(9), 5.0)
        self.assertTrue(p.covers(t(9)))
        self.assertFalse(p.covers(t(9, 1)))

    def test_the_horizon_runs_to_the_end_of_a_final_interval(self):
        # Without this the last window would be unreadable past its start,
        # which for a coarse trailing interval is most of it.
        f = forecast([(t(9), 1.0), (t(12), 2.0, 3 * 3600)])
        self.assertEqual(f.horizon_end, t(15))
        self.assertEqual(f.at(t(14, 59), "step", 3600), 2.0)
        self.assertIsNone(f.at(t(15), "step", 3600))
        # a trailing instant still ends the horizon at itself
        self.assertEqual(forecast([(t(9), 1.0)]).horizon_end, t(9))

    def test_a_declared_extent_needs_no_gap_heuristic(self):
        # Two 15-minute windows an hour apart: the source said what each
        # covers, so the hole between them is not a judgement call and
        # max_gap_s has nothing to decide.
        f = forecast([(t(9), 1.0, 900), (t(10), 2.0, 900)])
        self.assertEqual(f.at(t(9, 10), "step", 999999), 1.0)
        self.assertIsNone(f.at(t(9, 30), "step", 999999))
        self.assertEqual(f.at(t(10, 10), "step", 1), 2.0)

    def test_a_held_value_reads_to_the_end_of_the_last_window(self):
        # The case a gap heuristic cannot serve: the final point has no
        # successor, so its hold length is only knowable if declared.
        f = forecast([(t(9), 1.0, 900), (t(9, 15), 2.0, 900)])
        self.assertEqual(f.at(t(9, 20), "step", 3600), 2.0)
        self.assertIsNone(f.at(t(9, 30), "step", 3600))

    def test_interpolating_an_interval_is_refused_not_guessed(self):
        f = forecast([(t(9), 1.0, 3600), (t(10), 2.0, 3600)])
        with self.assertRaises(ValueError):
            f.at(t(9, 30), "linear", 3600)

    def test_a_nonsense_extent_is_refused(self):
        for bad in (0, -60, float("inf"), float("nan"), True, "6h"):
            with self.subTest(bad), self.assertRaises(ValueError):
                encode(t(8), [(t(9), 1.0, bad)])

    def test_resampling_an_interval_series_lands_in_its_windows(self):
        f = forecast([(t(9), 1.0, 900), (t(9, 15), 2.0, 900), (t(9, 30), 3.0, 900)])
        self.assertEqual(f.resample(t(9), 900, 4, "step", 3600), [1.0, 2.0, 3.0, None])


class DecodeTest(unittest.TestCase):
    def test_a_future_schema_is_refused_not_guessed_at(self):
        # A compact encoding would arrive as schema 2; reading it as 1
        # would silently mean something else.
        payload = json.dumps({"schema": 2, "issued": t(8).isoformat(), "points": []}).encode()
        with self.assertRaises(ValueError):
            decode(payload)

    def test_malformed_payloads_raise_the_uniform_sentinel(self):
        for label, payload in [
            ("not json", b"{"),
            ("not an object", b"[]"),
            ("no issued", json.dumps({"schema": 1, "points": []}).encode()),
            (
                "points not a list",
                json.dumps({"schema": 1, "issued": t(8).isoformat(), "points": {}}).encode(),
            ),
            (
                "descending points",
                json.dumps(
                    {
                        "schema": 1,
                        "issued": t(8).isoformat(),
                        "points": [
                            {"t": t(10).isoformat(), "v": 1.0},
                            {"t": t(9).isoformat(), "v": 2.0},
                        ],
                    }
                ).encode(),
            ),
        ]:
            with self.subTest(label), self.assertRaises(ValueError):
                decode(payload)


class AgeTest(unittest.TestCase):
    def test_age_is_measured_from_issue(self):
        self.assertEqual(forecast([(t(9), 1.0)], issued=t(8)).age_s(now=t(10)), 7200.0)


class ReadingTest(unittest.TestCase):
    """`at` has no default mode, because the two readings disagree and
    guessing for everyone is the mistake a regular grid would have made."""

    def setUp(self):
        self.f = forecast([(t(9), 10.0), (t(10), 20.0)])

    def test_step_holds_and_linear_moves(self):
        self.assertEqual(self.f.at(t(9, 30), "step", 3600), 10.0)
        self.assertEqual(self.f.at(t(9, 30), "linear", 3600), 15.0)

    def test_a_point_itself_reads_the_same_either_way(self):
        for mode in ("step", "linear"):
            with self.subTest(mode):
                self.assertEqual(self.f.at(t(9), mode, 3600), 10.0)
                self.assertEqual(self.f.at(t(10), mode, 3600), 20.0)

    def test_outside_the_horizon_is_none_not_an_edge_value(self):
        for mode in ("step", "linear"):
            with self.subTest(mode):
                self.assertIsNone(self.f.at(t(8), mode, 3600))
                self.assertIsNone(self.f.at(t(11), mode, 3600))

    def test_a_gap_wider_than_allowed_stays_missing(self):
        # hours 9 and 10, then nothing until 17: neither holding the 10:00
        # value for seven hours nor drawing a line through the hole is
        # something the source said.
        gappy = forecast([(t(9), 10.0), (t(10), 20.0), (t(17), 30.0)])
        self.assertIsNone(gappy.at(t(13), "step", 3600))
        self.assertIsNone(gappy.at(t(13), "linear", 3600))
        # and the same instant reads fine once the gap is permitted
        self.assertEqual(gappy.at(t(13), "step", 8 * 3600), 20.0)

    def test_an_unknown_mode_is_refused(self):
        with self.assertRaises(ValueError):
            self.f.at(t(9, 30), "spline", 3600)


class ResampleTest(unittest.TestCase):
    def test_a_grid_is_available_to_a_consumer_that_wants_one(self):
        f = forecast([(t(9), 10.0), (t(10), 20.0), (t(11), 30.0)])
        self.assertEqual(f.resample(t(9), 3600, 3, "step", 3600), [10.0, 20.0, 30.0])
        self.assertEqual(
            f.resample(t(9), 1800, 3, "linear", 3600), [10.0, 15.0, 20.0]
        )

    def test_uncovered_slots_are_none_not_invented(self):
        f = forecast([(t(9), 10.0), (t(10), 20.0)])
        self.assertEqual(f.resample(t(9), 3600, 4, "step", 3600), [10.0, 20.0, None, None])

    def test_refuses_a_nonsense_grid(self):
        f = forecast([(t(9), 10.0)])
        with self.assertRaises(ValueError):
            f.resample(t(9), 0, 3, "step", 3600)
        with self.assertRaises(ValueError):
            f.resample(t(9), 3600, -1, "step", 3600)


if __name__ == "__main__":
    unittest.main()
