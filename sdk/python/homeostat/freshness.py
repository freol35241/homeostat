"""Bounded-age inputs for automations (docs/design.md#staleness).

`available` reports device liveness, not data freshness. A sensor can
die mid-reading, and its last value stays trusted until the bridge
notices. For a battery device that can take hours. An automation that
combines several sources therefore keeps its own staleness policy. The
cadence is house knowledge, so it comes from a parameter and not from a
TTL in the core. This class does the bookkeeping for that policy.

It maps each source to its latest value and the monotonic time it was
seen, and returns the fresh ones at recompute time. A live sample that
triggers a recompute was seen just now, so the fresh set is not empty and
a mean over it does not divide by zero. A catch-up delivery after a
restart is different. It carries the mirror's age and may already be
stale, so a handler that a catch-up delivery can trigger must check for
an empty fresh set.

This class has no timer. An automation that must react when nothing
arrives at all subscribes to `home/clock/minute` and also calls `fresh()`
from that handler.
"""

import time
from collections.abc import Callable
from typing import Any


class Freshness:
    """The latest value per source and the monotonic time it was seen.

    Parameters
    ----------
    clock : Callable[[], float], optional
        Monotonic time source in seconds; `time.monotonic` by default.
    """

    def __init__(self, clock: Callable[[], float] = time.monotonic):
        self._clock = clock
        self._seen: dict[str, tuple[Any, float]] = {}

    def seen(self, source: str, value: Any, age_s: float = 0.0) -> None:
        """Record `value` from `source` as seen `age_s` seconds ago.

        `age_s` is zero for a live sample and the mirror's age for a
        catch-up delivery, so a restart cannot pass off an hours-old
        reading as fresh.

        Parameters
        ----------
        source : str
            Where the value came from, typically the bus key.
        value : Any
            The value seen.
        age_s : float, optional
            How long ago the value was seen, in seconds.
        """
        self._seen[source] = (value, self._clock() - age_s)

    def fresh(self, max_age_s: float) -> dict[str, Any]:
        """Return the latest value per source seen within the last `max_age_s` seconds.

        A source last seen longer ago than that is left out. It is included
        again as soon as it publishes.

        Parameters
        ----------
        max_age_s : float
            Maximum age in seconds of a value still counted as fresh.

        Returns
        -------
        dict[str, Any]
            The fresh values, keyed by source.
        """
        cutoff = self._clock() - max_age_s
        return {source: value for source, (value, at) in self._seen.items() if at >= cutoff}

    def forget(self, source: str) -> None:
        """Drop a source, e.g. on a binding's `available = false`.

        Parameters
        ----------
        source : str
            The source to forget; an unknown one is ignored.
        """
        self._seen.pop(source, None)
