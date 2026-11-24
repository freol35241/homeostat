"""Hold bookkeeping for an automation's Context (docs/design.md#holds).

A HoldBook keeps the holds one unit has published under
home/hold/{unit}/ and which of them it has asserted since it started.
It reads them from the state mirror on first use, so a restart keeps a
hold's id when its wish is unchanged. The Context validates a hold's
arguments and builds the wish; the HoldBook publishes and deletes.
"""

import json
import secrets
import threading
from collections.abc import Callable
from typing import Any

from . import keys
from .session import UnitSession


def _as_json(value: Any) -> Any:
    """Return `value` as JSON reads it back: a tuple as a list, and so on."""
    return json.loads(json.dumps(value))


class HoldBook:
    """The holds one unit has published, and which it has asserted.

    Parameters
    ----------
    session : UnitSession
        The unit's bus session.
    unit : str
        The unit's name.
    paused : Callable[[], bool]
        Whether the automation is paused. `hold` asks it under this
        book's lock, so a pause that releases every hold cannot race a
        hold that publishes one.
    """

    def __init__(self, session: UnitSession, unit: str, paused: Callable[[], bool]):
        self._session = session
        self._unit = unit
        self._paused = paused
        # hold key -> the record this unit holds there. None until read
        # from the mirror.
        self._holds: dict[str, dict] | None = None
        self._asserted: set[str] = set()
        self._lock = threading.Lock()

    def load(self) -> None:
        """Read the unit's holds from the mirror, if not read yet."""
        with self._lock:
            self._own()

    def hold(self, key: str, wish: dict[str, Any]) -> None:
        """Hold `wish` at `key`, unless paused or already held.

        Holding the same wish again publishes nothing and keeps the
        hold's id.

        Parameters
        ----------
        key : str
            The concrete hold key.
        wish : dict
            The hold's value, priority and optional `until` and `reason`.
        """
        # Under the lock, which `release` and a pause also take, so a hold
        # and a release racing on two subscription threads cannot leave a
        # key on the bus that the SDK no longer counts.
        with self._lock:
            if self._paused():
                return
            holds = self._own()
            self._asserted.add(key)
            current = holds.get(key)
            # Compared as JSON, so a tuple held again equals the list the
            # mirror returned and keeps its id.
            if current is not None and _as_json({k: v for k, v in current.items() if k != "id"}) == _as_json(wish):
                return
            record = dict(wish, id=secrets.token_hex(4))
            holds[key] = record
            self._session.put_json(key, record)

    def holding(self, key: str, value: Any) -> bool:
        """Return whether the unit holds `value` at `key`.

        Parameters
        ----------
        key : str
            The concrete hold key.
        value : Any
            The value to compare, as JSON.

        Returns
        -------
        bool
            Whether a hold at `key` has this value.
        """
        with self._lock:
            current = self._own().get(key)
        return current is not None and _as_json(current.get("value")) == _as_json(value)

    def release(self, key: str) -> None:
        """Release the hold at `key`. Releasing one not held does nothing.

        Parameters
        ----------
        key : str
            The concrete hold key.
        """
        with self._lock:
            self._asserted.discard(key)
            holds = self._own()
            if key not in holds:
                return
            del holds[key]
            self._session.delete(key)

    def release_all(self) -> None:
        """Release every hold the unit has published."""
        with self._lock:
            holds = self._own()
            for key in holds:
                self._session.delete(key)
            holds.clear()
            self._asserted.clear()

    def prune_unasserted(self) -> None:
        """Release every hold not asserted since the unit started."""
        with self._lock:
            holds = self._own()
            for key in [key for key in holds if key not in self._asserted]:
                del holds[key]
                self._session.delete(key)

    def _own(self) -> dict[str, dict]:
        """Return the holds this unit has published, read from the mirror once.

        Called with `self._lock` held.
        """
        if self._holds is None:
            self._holds = {
                key: record
                for key, record in self._session.get_json(keys.hold_keyexpr(self._unit))
                if isinstance(record, dict)
            }
        return self._holds
