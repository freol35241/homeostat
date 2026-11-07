"""Totals over a window from values that each cover an interval.

A rain forecast point with `d`, and a recorded hourly total whose entity
declares `interval_s`, each say how much fell over `[t, t + d)`. A window's
total counts each value by the share of its interval inside the window,
with the amount taken as spread evenly over the interval: an hour half
inside counts half, and a 6 h interval with two hours inside counts a
third (docs/design.md#totals-over-intervals).

A stretch of the window that no value covers is never counted as zero.
Zero rain is the reading that turns the watering on, so a missing hour
raises instead.
"""

import datetime
from collections.abc import Iterable
from typing import TYPE_CHECKING

if TYPE_CHECKING:
    from .forecast import Point


class IntervalsUncovered(Exception):
    """The values do not cover the window, so there is no total.

    Attributes
    ----------
    reason : str
        "not-intervals" when a value in the window has no interval, "gap"
        when the window starts before the values do or two intervals leave
        a hole or overlap, and "short" when the values end before the
        window does.
    """

    def __init__(self, reason: str, detail: str):
        super().__init__(f"{reason}: {detail}")
        self.reason = reason


def total(points: Iterable["Point"], start: datetime.datetime, end: datetime.datetime) -> float:
    """Return the total over `[start, end)` of values that each cover an interval.

    Parameters
    ----------
    points : iterable of Point
        The values, ascending by `t`, each covering `[t, t + d)`.
    start : datetime.datetime
        The window's start, timezone-aware.
    end : datetime.datetime
        The window's end, timezone-aware.

    Returns
    -------
    float
        The total over the window.

    Raises
    ------
    ValueError
        If `end` is before `start`.
    IntervalsUncovered
        If the values do not cover the whole window.
    """
    if end < start:
        raise ValueError("the window's end is before its start")
    summed = 0.0
    covered_to = start
    previous_end = None
    for point in points:
        if point.d is None:
            if start <= point.t < end:
                raise IntervalsUncovered("not-intervals", f"{point.t.isoformat()} has no interval")
            continue
        point_end = point.t + datetime.timedelta(seconds=point.d)
        if previous_end is not None and point.t < previous_end:
            raise IntervalsUncovered(
                "gap", f"{point.t.isoformat()} starts before the previous interval ends"
            )
        previous_end = point_end
        if point_end <= start or point.t >= end:
            continue
        if point.t > covered_to:
            raise IntervalsUncovered("gap", f"nothing covers {covered_to.isoformat()}")
        inside = (min(point_end, end) - max(point.t, start)).total_seconds()
        summed += point.v * inside / point.d
        covered_to = point_end
    if covered_to < end:
        raise IntervalsUncovered("short", f"the values end at {covered_to.isoformat()}")
    return summed
