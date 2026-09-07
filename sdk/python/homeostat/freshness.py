"""Bounded-age inputs for automations (docs/design.md, Availability).

`available` is device liveness, not data freshness: a sensor can die
mid-reading and its last value stays trusted until the bridge notices,
which for a battery device can be hours. An automation that fuses several
sources therefore keeps its own staleness policy — the cadence is house
knowledge (a parameter), never a core TTL — and the bookkeeping behind
that policy is what this helper is.

Graduated from the temperature fusion at #7: a map of the latest value and
the monotonic time it was seen, per source, and "the fresh ones" at
recompute time. The subtlety worth owning here rather than in every
automation: the sample that triggers a recompute was seen just now, so
the fresh set is never empty and a mean over it never divides by zero.

No timer lives here. An automation that must react to silence — nothing
arriving at all — subscribes to `home/clock/minute` and calls `fresh()`
from that handler too.
"""

import time
from typing import Any, Callable


class Freshness:
    def __init__(self, clock: Callable[[], float] = time.monotonic):
        self._clock = clock
        self._seen: dict[str, tuple[Any, float]] = {}

    def seen(self, source: str, value: Any) -> None:
        """Records `value` from `source` (typically the bus key) as of now."""
        self._seen[source] = (value, self._clock())

    def fresh(self, max_age_s: float) -> dict[str, Any]:
        """The latest value per source seen within the last `max_age_s`
        seconds, keyed by source. A source last seen longer ago than that
        is left out; it returns the moment it publishes again."""
        cutoff = self._clock() - max_age_s
        return {source: value for source, (value, at) in self._seen.items() if at >= cutoff}

    def forget(self, source: str) -> None:
        """Drops a source, e.g. on a binding's `available = false`."""
        self._seen.pop(source, None)
