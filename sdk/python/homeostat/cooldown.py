"""Per-key cooldowns for automations that reach people (docs/design.md#notifications).

One notification per event means 417 notifications when a motion
episode arrives as 417 samples. The alarm flow in the live installation
therefore limits itself to one message per ten minutes, and it depends on
that limit. The window is house policy, set by a family-editable parameter.
This class does the bookkeeping for it, so units do not each write their
own. It records the monotonic time each key last fired and answers
whether a key may fire again now.

`ready(key, window_s)` answers and, when true, records the firing, so the
call site is one `if`. A key that has never fired is ready. The adapter
side also enforces its own minimum interval (`min_interval_s`). This
helper sets the normal rate, and the adapter's minimum is a second line
of defence.
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

        The key may fire when at least `window_s` seconds have passed
        since it last fired, or when it has never fired. In that case the
        key is marked as fired now. Otherwise the record is left
        unchanged.

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

        Use this when the condition that fired the key has cleared, so the
        next occurrence is a new episode rather than a repeat.

        Parameters
        ----------
        key : str
            The key to forget; a key never fired is left as it is.
        """
        self._fired.pop(key, None)
