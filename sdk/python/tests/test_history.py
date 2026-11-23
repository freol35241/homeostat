"""The history selector grammar and the row cap, without a bus.

Run: uv run --no-project --with-editable sdk/python python -m unittest discover sdk/python/tests
"""

import datetime
import unittest

from homeostat import history


class SelectorTest(unittest.TestCase):
    def test_the_row_cap_is_ten_thousand(self):
        # A running recorder and a newer SDK must agree on it: the SDK reads
        # a reply of exactly this many rows as a cut window.
        self.assertEqual(history.MAX_ROWS, 10_000)

    def test_parameters_are_written_in_order_and_read_back(self):
        start = datetime.datetime(2026, 1, 1, 1, tzinfo=datetime.timezone(datetime.timedelta(hours=1)))
        written = history.selector(
            "home/history/state/rain/amount", {"from": start, "limit": 5, "changes": "1"}
        )
        self.assertEqual(
            written,
            "home/history/state/rain/amount?from=2026-01-01T00:00:00+00:00;limit=5;changes=1",
        )
        _, _, raw = written.partition("?")
        params = history.split_params(raw)
        self.assertEqual(params, {"from": "2026-01-01T00:00:00+00:00", "limit": "5", "changes": "1"})
        self.assertEqual(
            history.rfc3339_us("from", params["from"]), int(start.timestamp() * 1_000_000)
        )

    def test_parameters_split_without_url_decoding(self):
        self.assertEqual(
            history.split_params("from=2026-01-01T00:00:00+01:00;limit=5;;"),
            {"from": "2026-01-01T00:00:00+01:00", "limit": "5"},
        )


if __name__ == "__main__":
    unittest.main()
