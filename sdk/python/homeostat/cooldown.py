"""Per-key cooldowns for automations that reach people (docs/design.md,
Notifications).

A notification per event is a notification 417 times when a motion
episode arrives as 417 samples (#3), so the live estate's alarm flow
limits itself to one message per ten minutes and that limiter is
load-bearing. The window is house policy — a family-editable parameter —
and this is the bookkeeping behind it, owned once rather than hand-rolled
per unit: the monotonic time each key last fired, and whether a key may
fire again now.

`ready(key, window_s)` answers and, when true, records the firing, so the
call site is one `if`. A key that has never fired is ready. The adapter
side carries its own floor (`min_interval_s`) as defense in depth; this
helper is the norm, that floor is the backstop.
"""

import time
from typing import Callable


class Cooldown:
    def __init__(self, clock: Callable[[], float] = time.monotonic):
        self._clock = clock
        self._fired: dict[str, float] = {}

    def ready(self, key: str, window_s: float) -> bool:
        """True, and the key marked as fired now, when at least `window_s`
        seconds have passed since the key last fired (or it never has);
        False otherwise, leaving the record untouched."""
        now = self._clock()
        last = self._fired.get(key)
        if last is not None and now - last < window_s:
            return False
        self._fired[key] = now
        return True

    def reset(self, key: str) -> None:
        """Forgets a key, so its next `ready` is True — e.g. when the
        condition that fired it has cleared and the next occurrence is a
        new episode, not a repeat."""
        self._fired.pop(key, None)
