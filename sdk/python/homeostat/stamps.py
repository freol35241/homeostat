"""Sample timestamps: which value for a key is the newest.

The core's router stamps every sample that arrives without a stamp, so
every sample on the bus carries one (docs/design.md#timestamps). A
publisher may set its own stamp instead, saying when the value was true.
The core does that when it replays recorded state after a restart.
"""

import datetime
import threading
from typing import Any

__all__ = ["Newest", "stamp_us"]

_EPOCH = datetime.datetime(1970, 1, 1, tzinfo=datetime.UTC)
_MICROSECOND = datetime.timedelta(microseconds=1)


def stamp_us(stamp: Any) -> int | None:
    """Return a zenoh timestamp as integer microseconds since the epoch.

    Parameters
    ----------
    stamp : zenoh.Timestamp or None
        The sample's timestamp.

    Returns
    -------
    int or None
        Microseconds since 1970-01-01 UTC, or None when there is no stamp.
    """
    if stamp is None:
        return None
    return (stamp.get_time() - _EPOCH) // _MICROSECOND


class Newest:
    """The newest stamp delivered per key, to drop values that are older.

    A replayed value can arrive after a live one for the same key, and a
    catch-up reply can arrive after the live sample it predates. Both are
    older than what was already delivered and must not replace it. Stamps
    are compared as zenoh orders them: by time, then by publisher.

    A value without a stamp comes from a core that does not stamp. It is
    ordered by arrival, as before stamps existed: a live sample is always
    delivered, and a catch-up for a key already delivered is not.
    """

    def __init__(self):
        self._stamps: dict[str, Any] = {}
        self._lock = threading.Lock()

    def admit(self, key: str, stamp: Any, *, catch_up: bool = False) -> bool:
        """Record a value for `key` and return whether it is newer.

        Parameters
        ----------
        key : str
            The sample's key.
        stamp : zenoh.Timestamp or None
            The sample's timestamp.
        catch_up : bool, optional
            True for a value read from a last-value mirror rather than
            received live.

        Returns
        -------
        bool
            True when the value should be delivered: no value for the key
            was delivered before, or this one is newer.
        """
        with self._lock:
            known = key in self._stamps
            previous = self._stamps.get(key)
            if stamp is None:
                if catch_up and known:
                    return False
                self._stamps.setdefault(key, None)
                return True
            if previous is not None and not previous < stamp:
                return False
            self._stamps[key] = stamp
            return True
