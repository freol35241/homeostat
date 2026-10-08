"""Forecasts: a series' future, on the bus (docs/design.md#forecasts).

A forecast rides `home/forecast/{room}/{entity}/{aspect}/{source}` — the
same room/entity/aspect as `home/state`, because it is the same series
extended forward, then the source that claims it. The entity's aspect
descriptor therefore already supplies its label, kind and unit, and past
and future share one chart axis.

The payload is what the source actually said:

    {"schema": 1,
     "issued": "2026-09-20T08:00:00Z",
     "points": [{"t": "2026-09-20T09:00:00Z", "v": 1.23},
                {"t": "2026-09-20T12:00:00Z", "v": 0.4, "d": 10800}, ...]}

A point is an instant unless it carries `d`, the extent in seconds it
describes — an accumulation over a window, or a value that holds across
one. See `Point`.

What makes a forecast different from state is not the array — it is that
each point carries TWO times, when it was said and when it is about,
where a state sample carries one. The array transports one issue
atomically; the value inside it is a scalar like any other.

Points are irregular by design. A regular grid cannot represent an
irregular series while the reverse is trivial, and forcing one would make
the producer resample — which is lossy, and worse, not single-valued: a
spot price is a step function that HOLDS for its interval, a temperature
forecast interpolates. A grid would bake one of those readings into the
producer where no consumer could see or override it. So the wire carries
the source's own points and the resampling lives here, in the SDK, where
a consumer names the rule it wants.

`issued` is required. Without it nothing downstream can tell a stale
forecast from a fresh one, and staleness is the whole safety story: a
consumer applies its own max age (the `Freshness` reasoning — freshness
policy is the automation's, never a core TTL) and a controller that
refuses simply stops writing, which is what makes a house fall back to
its own control (see adapters/ivt490.py, the FEEDABLE inputs).
"""

import datetime
import json
import math
from dataclasses import dataclass
from itertools import pairwise

SCHEMA = 1

# A guard against a runaway producer, not a design constraint: 15-minute
# resolution over a week is 672 points, so nothing legitimate comes near
# this. The core never inspects state-class payloads (src/world.rs mirrors
# without reading), so this is the SDK's own refusal, raised where the
# producer can see it rather than discovered by the recorder later.
MAX_POINTS = 2048


def _parse_ts(value) -> datetime.datetime:
    """Parse an RFC3339 timestamp with an offset, as the recorder's samples path uses.

    A naive one is refused rather than guessed at: "09:00" means
    different instants in different places, and a forecast that is an hour
    wrong is worse than one that is absent.
    """
    if not isinstance(value, str):
        raise ValueError(f"timestamp must be a string, got {value!r}")
    parsed = datetime.datetime.fromisoformat(value)
    if parsed.tzinfo is None:
        raise ValueError(f"timestamp {value!r} has no UTC offset")
    return parsed


@dataclass(frozen=True)
class Point:
    """One predicted value, and what it is predicted for.

    `d` is the point's extent in seconds, and it is the difference between
    an instant and an interval. Without it the value is instantaneous at
    `t` — a temperature. With it the value describes `[t, t + d)`: an
    accumulation over that window (rainfall in a period), or a value that
    holds across it (a tariff over its settlement period).

    Carrying it is the same rule as carrying irregular spacing. Sources
    state the interval — that is what they said — and a bare instant
    discards it: an accumulation over six hours, stamped at one end, reads
    as a spike at that instant, and a held value's length can otherwise
    only be guessed from the gap to the next point, which fails at the end
    of a horizon, where there is no next point.

    Attributes
    ----------
    t : datetime.datetime
        The instant the value is for, or the start of its interval.
    v : float
        The predicted value.
    d : float or None
        The extent in seconds, or None for an instant.
    """

    t: datetime.datetime
    v: float
    d: float | None = None

    def covers(self, when: datetime.datetime) -> bool:
        """Return whether this point speaks for `when`.

        An instant speaks only for itself; an interval for `[t, t + d)`,
        half-open so abutting intervals do not both claim their shared edge.

        Parameters
        ----------
        when : datetime.datetime
            The instant asked about.

        Returns
        -------
        bool
            True if this point's value holds at `when`.
        """
        if self.d is None:
            return when == self.t
        return self.t <= when < self.t + datetime.timedelta(seconds=self.d)


@dataclass(frozen=True)
class Forecast:
    """A decoded forecast: when it was issued, and what it says.

    Attributes
    ----------
    issued : datetime.datetime
        When the source issued the forecast.
    points : tuple of Point
        The points, strictly ascending in time.
    """

    issued: datetime.datetime
    points: tuple[Point, ...]

    @property
    def horizon_end(self) -> datetime.datetime | None:
        """The end of what this forecast covers, or None if it has no points.

        That is the last point's instant, or the end of its interval where
        it declares one. Without this an interval-valued final point would
        be unreadable past its start.
        """
        if not self.points:
            return None
        last = self.points[-1]
        if last.d is None:
            return last.t
        return last.t + datetime.timedelta(seconds=last.d)

    def age_s(self, now: datetime.datetime | None = None) -> float:
        """Return the seconds since this forecast was issued.

        This is what a consumer checks against its own tolerance before
        acting on it.

        Parameters
        ----------
        now : datetime.datetime or None, optional
            The current time; the system clock, in UTC, when None.

        Returns
        -------
        float
            The forecast's age in seconds.
        """
        now = now or datetime.datetime.now(datetime.timezone.utc)
        return (now - self.issued).total_seconds()

    def at(self, when: datetime.datetime, mode: str, max_gap_s: float) -> float | None:
        """Return the value predicted for `when`, or None if the forecast does not cover it.

        `mode` is the reading the series carries and has no default,
        because guessing it is exactly the mistake a regular grid would
        have made for everyone:

          "step"    the value holds from its point until the next one — a
                    tariff, a schedule, anything piecewise-constant.
          "linear"  the value moves between points — a temperature.

        `max_gap_s` is what keeps a hole honest where points are instants.
        A source that returns hours 0-5 and 12-24 has said nothing about
        the middle, and both modes would otherwise invent it: "step" by
        holding a six-hour-old value, "linear" by drawing a straight line
        through the gap. A wider gap than this yields None, so missing data
        stays missing. Where a point declares its own extent there is
        nothing to guess and `max_gap_s` does not apply: the source said
        what the value covers, and outside it the answer is None.

        Asking for "linear" across an interval-valued point raises: a value
        the source declared to span a window is not a sample to interpolate
        between, and quietly averaging two accumulations is the kind of
        invention this helper exists to refuse.

        Parameters
        ----------
        when : datetime.datetime
            The instant to read.
        mode : str
            "step" or "linear", as above.
        max_gap_s : float
            The widest gap in seconds between two instant points to read
            across.

        Returns
        -------
        float or None
            The predicted value, or None where the forecast says nothing.

        Raises
        ------
        ValueError
            If `mode` is neither "step" nor "linear", or "linear" is asked
            across an interval-valued point.
        """
        if mode not in ("step", "linear"):
            raise ValueError(f"mode must be 'step' or 'linear', got {mode!r}")
        points = self.points
        end = self.horizon_end  # None exactly when there are no points
        if end is None or when < points[0].t:
            return None
        # An interval ends half-open, so its own end instant is past it.
        if when > end or (when == end and points[-1].d is not None):
            return None
        # Points are ascending (enforced on decode), so the last one at or
        # before `when` and its successor bracket it.
        lo = 0
        for i, p in enumerate(points):
            if p.t <= when:
                lo = i
            else:
                break
        left = points[lo]
        if left.d is not None:
            if mode == "linear":
                raise ValueError(
                    f"point at {left.t.isoformat()} spans {left.d}s; "
                    "an interval is read with 'step', not interpolated"
                )
            return left.v if left.covers(when) else None
        if left.t == when:
            return left.v
        right = points[lo + 1] if lo + 1 < len(points) else None
        if right is None:
            return None
        if (right.t - left.t).total_seconds() > max_gap_s:
            return None
        if mode == "step":
            return left.v
        span = (right.t - left.t).total_seconds()
        return left.v + (right.v - left.v) * ((when - left.t).total_seconds() / span)

    def resample(
        self,
        start: datetime.datetime,
        step_s: float,
        count: int,
        mode: str,
        max_gap_s: float,
    ) -> list[float | None]:
        """Return `count` values on a regular grid from `start`.

        For a consumer that wants one — an optimiser's horizon, say. A slot
        the forecast does not cover is None rather than a fabricated number,
        so a controller can refuse instead of optimising against invention.

        Parameters
        ----------
        start : datetime.datetime
            The first slot's instant.
        step_s : float
            The grid's spacing in seconds; must be positive.
        count : int
            The number of slots; must not be negative.
        mode : str
            "step" or "linear", as for `at`.
        max_gap_s : float
            As for `at`.

        Returns
        -------
        list of float or None
            One value per slot, None where the forecast says nothing.

        Raises
        ------
        ValueError
            If `step_s` is not positive, `count` is negative, or `at`
            refuses `mode`.
        """
        if step_s <= 0:
            raise ValueError("step_s must be positive")
        if count < 0:
            raise ValueError("count must not be negative")
        return [
            self.at(start + datetime.timedelta(seconds=i * step_s), mode, max_gap_s)
            for i in range(count)
        ]


def _extent(d) -> float | None:
    """Return a point's declared extent in seconds, validated, or None for an instant.

    Zero is refused along with the negatives: a window of no
    length is not an interval, and admitting it would give `covers` an
    empty range that nothing could ever read.
    """
    if d is None:
        return None
    if isinstance(d, bool) or not isinstance(d, (int, float)):
        raise ValueError(f"forecast point extent must be a number, got {d!r}")
    d = float(d)
    if not math.isfinite(d) or d <= 0:
        raise ValueError(f"forecast point extent must be positive and finite, got {d}")
    return d


def encode(issued: datetime.datetime, points) -> bytes:
    """Return the wire payload for `points`, sorted and checked.

    A point is `Point(t, v, d=None)` or a `(t, v)` / `(t, v, d)` tuple,
    where `d` is the optional extent described on `Point`. `d` is written
    only where a producer gave one, so a series of instants is unchanged
    on the wire.

    Sorting is done here rather than demanded of the producer — ascending
    order is a cheap invariant that makes every reader simpler, and it is
    shape, not meaning.

    Parameters
    ----------
    issued : datetime.datetime
        When the forecast was issued; must carry a UTC offset.
    points : iterable of Point or tuple
        The points, in any order; a tuple's `t` may be an RFC3339 string.

    Returns
    -------
    bytes
        The JSON payload, UTF-8 encoded.

    Raises
    ------
    ValueError
        On anything a consumer could not trust: a naive timestamp, a
        non-finite or non-numeric value or extent, a duplicated instant, or
        more points than MAX_POINTS.
    """
    if issued.tzinfo is None:
        raise ValueError("issued has no UTC offset")
    prepared: list[Point] = []
    for point in points:
        if isinstance(point, Point):
            t, v, d = point.t, point.v, point.d
        else:
            t, v, *rest = point
            d = rest[0] if rest else None
        if isinstance(t, str):
            t = _parse_ts(t)
        elif t.tzinfo is None:
            raise ValueError("a forecast point has no UTC offset")
        if isinstance(v, bool) or not isinstance(v, (int, float)):
            raise ValueError(f"forecast value must be a number, got {v!r}")
        v = float(v)
        if not math.isfinite(v):
            # The recorder drops non-finite samples with an event; refusing
            # here names the producer instead.
            raise ValueError("forecast value must be finite")
        prepared.append(Point(t, v, _extent(d)))
    if len(prepared) > MAX_POINTS:
        raise ValueError(f"forecast has {len(prepared)} points, over the {MAX_POINTS} guard")
    prepared.sort(key=lambda p: p.t)
    for earlier, later in pairwise(prepared):
        if earlier.t == later.t:
            raise ValueError(f"forecast has two points at {earlier.t.isoformat()}")
    return json.dumps(
        {
            "schema": SCHEMA,
            "issued": issued.isoformat(),
            "points": [
                {"t": p.t.isoformat(), "v": p.v} if p.d is None
                else {"t": p.t.isoformat(), "v": p.v, "d": p.d}
                for p in prepared
            ],
        }
    ).encode()


def decode(payload: bytes) -> Forecast:
    """Parse a wire payload into a `Forecast`.

    Parameters
    ----------
    payload : bytes
        The payload as received.

    Returns
    -------
    Forecast
        The decoded forecast, its points strictly ascending.

    Raises
    ------
    ValueError
        On anything malformed — the codebase's uniform "bad input"
        sentinel, so a subscriber drops it with a malformed-payload health
        event like any other.
    """
    parsed = json.loads(payload)
    if not isinstance(parsed, dict):
        raise ValueError("forecast payload is not an object")
    schema = parsed.get("schema")
    if schema != SCHEMA:
        # Louder than ignoring it: a future encoding (a compact one, say)
        # must not be read as if it were this one.
        raise ValueError(f"forecast schema {schema!r} is not {SCHEMA}")
    issued = _parse_ts(parsed.get("issued"))
    raw = parsed.get("points")
    if not isinstance(raw, list):
        raise ValueError("forecast points is not a list")
    points: list[Point] = []
    for item in raw:
        if not isinstance(item, dict):
            raise ValueError("a forecast point is not an object")
        v = item.get("v")
        if isinstance(v, bool) or not isinstance(v, (int, float)):
            raise ValueError(f"forecast value must be a number, got {v!r}")
        points.append(Point(_parse_ts(item.get("t")), float(v), _extent(item.get("d"))))
    for earlier, later in pairwise(points):
        if earlier.t >= later.t:
            raise ValueError("forecast points are not strictly ascending in time")
    return Forecast(issued=issued, points=tuple(points))
