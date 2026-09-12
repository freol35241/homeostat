"""clock.py's boundary arithmetic, loaded directly from the adapter
script (it is not part of the homeostat package) since it is a pure
function with no bus: reused by the same discovery run as the SDK's own
tests (see the module docstring in any sibling test file for the command).

Run: uv run --no-project --with-editable sdk/python python -m unittest discover sdk/python/tests
"""

import datetime
import importlib.util
import unittest
import zoneinfo
from pathlib import Path

_SPEC = importlib.util.spec_from_file_location(
    "clock_under_test", Path(__file__).resolve().parents[3] / "adapters" / "clock.py"
)
clock = importlib.util.module_from_spec(_SPEC)
_SPEC.loader.exec_module(clock)


class NextBoundaryTest(unittest.TestCase):
    def test_ordinary_minute_is_sixty_seconds_away(self):
        now = datetime.datetime(2026, 6, 1, 12, 0, 30, tzinfo=datetime.timezone.utc)
        boundary = clock.next_boundary(now)
        self.assertEqual(boundary, datetime.datetime(2026, 6, 1, 12, 1, tzinfo=datetime.timezone.utc))
        self.assertEqual((boundary - now).total_seconds(), 30)

    def test_dst_fall_back_still_waits_one_real_minute(self):
        """The bug this guards against: computing the boundary in the
        LOCAL zone at Europe/Stockholm's 2026-10-25 fall-back (03:00 CEST
        -> 02:00 CET) makes `local.replace(...) + timedelta(minutes=1)`
        name a wall-clock minute that is really 61 minutes of elapsed UTC
        time away, so the repeated local hour is slept through and never
        published. Computed in UTC, the wait is always exactly one real
        minute regardless of what the local zone is doing."""
        tz = zoneinfo.ZoneInfo("Europe/Stockholm")
        # 2026-10-25 02:59:30 CEST == 2026-10-25 00:59:30 UTC.
        local = datetime.datetime(2026, 10, 25, 2, 59, 30, tzinfo=tz)
        now_utc = local.astimezone(datetime.timezone.utc)
        boundary = clock.next_boundary(now_utc)
        self.assertEqual((boundary - now_utc).total_seconds(), 30)
        # The published local time one real minute later is the FIRST
        # occurrence of the repeated hour (02:00 CET, fold=1 in absolute
        # terms — still CEST's immediate successor), not a jump to 03:00.
        published = boundary.astimezone(tz)
        self.assertEqual((published.hour, published.minute), (2, 0))
        self.assertEqual(published.utcoffset(), datetime.timedelta(hours=1), "already CET")

    def test_spring_forward_still_waits_one_real_minute(self):
        """The mirror case (2026-03-29, 01:59 CET -> 03:00 CEST): a
        wall-clock hour never happens at all, but UTC arithmetic still
        waits exactly one real minute and the local zone conversion
        correctly skips straight to 03:00."""
        tz = zoneinfo.ZoneInfo("Europe/Stockholm")
        local = datetime.datetime(2026, 3, 29, 1, 59, 30, tzinfo=tz)
        now_utc = local.astimezone(datetime.timezone.utc)
        boundary = clock.next_boundary(now_utc)
        self.assertEqual((boundary - now_utc).total_seconds(), 30)
        published = boundary.astimezone(tz)
        self.assertEqual((published.hour, published.minute), (3, 0))


if __name__ == "__main__":
    unittest.main()
