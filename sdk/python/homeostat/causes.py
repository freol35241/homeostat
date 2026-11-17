"""Why a device's state changed: its adapter's command, a person, or a rule.

See docs/design.md#device-reported-causes. An adapter whose devices report
the cause of a change, just before the change, attributes each change it
did not command. A person's input at the device counts as the family's,
and the adapter reports it to the arbiter. Any other cause is a rule on
the device. `Attribution` holds that bookkeeping for any adapter, with no
bus.
"""

import threading
import time
from collections.abc import Callable, Iterable

# A cause comes just before the change it caused, and a command's change
# comes back within a few seconds. Older ones are spent.
CAUSE_WINDOW_S = 2.0
ECHO_WINDOW_S = 10.0


class Attribution:
    """Why each tracked entity's boolean base aspect changed.

    The first value after a connect is the device's state, not a change.
    A change to the value the adapter last commanded, within
    `ECHO_WINDOW_S`, is that command's echo. Otherwise a cause reported
    within `CAUSE_WINDOW_S` before the change says why it happened. With
    none, a change is a person's for an entity whose firmware reports no
    causes, and unattributed for the rest.

    An adapter calls this from its device side and its command handlers
    at once, so every method locks.

    Parameters
    ----------
    person : dict of str to set of str
        The tracked entities, each with the causes that are a person's
        input. An entity not in it is never attributed.
    uncommanded : set of str
        The entities whose change with no cause is a person's
        (`uncommanded = "person"`).
    clock : Callable[[], float], optional
        Monotonic seconds; a fake in tests.
    """

    def __init__(
        self,
        person: dict[str, set[str]],
        uncommanded: set[str],
        clock: Callable[[], float] = time.monotonic,
    ):
        self._person = person
        self._uncommanded = uncommanded
        self._clock = clock
        self._lock = threading.Lock()
        self._last: dict[str, bool] = {}
        self._commanded: dict[str, tuple[bool, float]] = {}
        self._causes: dict[str, tuple[str, float]] = {}

    @classmethod
    def of(cls, entities: Iterable) -> "Attribution":
        """Return the Attribution for the entities that declare `[causes]`.

        Parameters
        ----------
        entities : Iterable of homeostat.house.Entity
            The adapter's entities.

        Returns
        -------
        Attribution
            Tracking those with `[causes]`.
        """
        declared = [e for e in entities if e.causes is not None]
        return cls(
            person={e.name: set(e.causes.get("person", [])) for e in declared},
            uncommanded={e.name for e in declared if e.causes.get("uncommanded") == "person"},
        )

    def tracks_any(self) -> bool:
        """Return whether any entity is tracked.

        Returns
        -------
        bool
            True when some entity declares `[causes]`.
        """
        return bool(self._person)

    def connected(self, entities: Iterable[str]) -> None:
        """Forget what is known of `entities`, so their next value is a baseline.

        Parameters
        ----------
        entities : Iterable of str
            The names of a device's entities, on a connect.
        """
        with self._lock:
            for entity in entities:
                self._last.pop(entity, None)
                self._causes.pop(entity, None)

    def commanded(self, entity: str, value: bool) -> None:
        """Note that the adapter told the device to set `value`.

        Parameters
        ----------
        entity : str
            The entity's name.
        value : bool
            The value sent.
        """
        with self._lock:
            self._commanded[entity] = (value, self._clock() + ECHO_WINDOW_S)

    def cause(self, entity: str, cause: str) -> None:
        """Note a cause the device reported for `entity`'s next change.

        Parameters
        ----------
        entity : str
            The entity's name.
        cause : str
            The cause in the device's words, such as `button`.
        """
        with self._lock:
            self._causes[entity] = (cause, self._clock() + CAUSE_WINDOW_S)

    def changed(self, entity: str, value: bool) -> tuple[str, str | None] | None:
        """Return why `entity` now reads `value`.

        Parameters
        ----------
        entity : str
            The entity's name.
        value : bool
            The value the device reports.

        Returns
        -------
        tuple of (str, str or None), or None
            ("person", cause), ("rule", cause) or ("unattributed", None).
            A person's change with no cause, under `uncommanded`, has cause
            None. None when there is nothing to attribute: an untracked
            entity, a first value, an unchanged one, or an echo.
        """
        if entity not in self._person:
            return None
        with self._lock:
            now = self._clock()
            before = self._last.get(entity)
            self._last[entity] = value
            if before is None or before == value:
                return None
            commanded = self._commanded.pop(entity, None)
            cause = self._causes.pop(entity, None)
            if commanded is not None and commanded[0] == value and now <= commanded[1]:
                return None
            if cause is not None and now <= cause[1]:
                return ("person" if cause[0] in self._person[entity] else "rule", cause[0])
            if entity in self._uncommanded:
                return ("person", None)
            return ("unattributed", None)
