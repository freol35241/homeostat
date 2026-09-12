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
automation: a live sample that triggers a recompute was seen just now, so
the fresh set is never empty and a mean over it never divides by zero.
A catch-up delivery after a restart is the exception — it carries the
mirror's age, and may already be stale — so a handler that can be
triggered by one checks for an empty fresh set.

No timer lives here. An automation that must react to silence — nothing
arriving at all — subscribes to `home/clock/minute` and calls `fresh()`
from that handler too.
"""

import time
from collections.abc import Callable
from typing import Any


class Freshness:
    def __init__(self, clock: Callable[[], float] = time.monotonic):
        self._clock = clock
        self._seen: dict[str, tuple[Any, float]] = {}

    def seen(self, source: str, value: Any, age_s: float = 0.0) -> None:
        """Records `value` from `source` (typically the bus key) as seen
        `age_s` seconds ago — zero for a live sample, the mirror's age for
        a catch-up delivery, so a restart cannot pass off an hours-old
        reading as fresh."""
        self._seen[source] = (value, self._clock() - age_s)

    def fresh(self, max_age_s: float) -> dict[str, Any]:
        """The latest value per source seen within the last `max_age_s`
        seconds, keyed by source. A source last seen longer ago than that
        is left out; it returns the moment it publishes again."""
        cutoff = self._clock() - max_age_s
        return {source: value for source, (value, at) in self._seen.items() if at >= cutoff}

    def forget(self, source: str) -> None:
        """Drops a source, e.g. on a binding's `available = false`."""
        self._seen.pop(source, None)
