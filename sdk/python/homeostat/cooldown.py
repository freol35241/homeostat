"""Per-key cooldowns for automations that reach people (docs/design.md#notifications).

A notification per event is a notification 417 times when a motion
episode arrives as 417 samples, so the live estate's alarm flow limits
itself to one message per ten minutes and that limiter is load-bearing.
The window is house policy — a family-editable parameter — and this is
the bookkeeping behind it, owned once rather than hand-rolled per unit:
the monotonic time each key last fired, and whether a key may fire again
now.

`ready(key, window_s)` answers and, when true, records the firing, so the
call site is one `if`. A key that has never fired is ready. The adapter
side carries its own floor (`min_interval_s`) as defense in depth; this
helper is the norm, that floor is the backstop.
"""

import time
from collections.abc import Callable


class Cooldown:
    """The monotonic time each key last fired, and whether it may fire again.

    Parameters
    ----------
    clock : Callable[[], float], optional
        Monotonic time source in seconds; `time.monotonic` by default.
    """

    def __init__(self, clock: Callable[[], float] = time.monotonic):
        self._clock = clock
        self._fired: dict[str, float] = {}

    def ready(self, key: str, window_s: float) -> bool:
        """Return whether `key` may fire now, marking it as fired if so.

        True, and the key marked as fired now, when at least `window_s`
        seconds have passed since the key last fired (or it never has);
        False otherwise, leaving the record untouched.

        Parameters
        ----------
        key : str
            The key to check and, when ready, mark as fired.
        window_s : float
            Minimum seconds between two firings of `key`.

        Returns
        -------
        bool
            True if the key fired now, False if its window has not passed.
        """
        now = self._clock()
        last = self._fired.get(key)
        if last is not None and now - last < window_s:
            return False
        self._fired[key] = now
        return True

    def reset(self, key: str) -> None:
        """Forget `key`, so its next `ready` is True.

        For example when the condition that fired it has cleared and the
        next occurrence is a new episode, not a repeat.

        Parameters
        ----------
        key : str
            The key to forget; a key never fired is left as it is.
        """
        self._fired.pop(key, None)
