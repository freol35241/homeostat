"""Availability from a device's publish cadence (docs/adapters.md#7-availability).

A device that publishes on a known cadence is lost when it goes quiet.
The adapter calls `SilenceWatch.contact` for each message from a device. A
watchdog thread then marks a device unavailable once it has been silent
longer than the timeout. `available` is put on transition only, with one
`device-silent` health event per down transition. Every other aspect
keeps its last value.

The timeout is the adapter's live `availability_timeout_s`, passed to
`start` as a callable so an owner's edit applies on the next sweep.
"""

import threading
import time
from collections.abc import Callable, Iterable

from . import keys
from .house import Entity


class SilenceWatch:
    """Each entity's `available`, timed from when it last spoke.

    Every entity counts from construction, so one that never speaks goes
    unavailable after one timeout. Messages and sweeps arrive on
    different threads, so the last contact and availability are locked.
    A sweep decides and puts under the lock, so a message lands either
    before the decision or after the put. In the second case it marks
    the entity available again.

    Parameters
    ----------
    session : UnitSession
        Anything with `put_json` and `health_event`.
    entities : iterable of Entity
        The bound entities. The `device-silent` event names an entity by
        its `id`, as `topic`.
    clock : Callable[[], float], optional
        Monotonic time source in seconds; `time.monotonic` by default.
    """

    def __init__(
        self, session, entities: Iterable[Entity], clock: Callable[[], float] = time.monotonic
    ):
        self._session = session
        self._entities = list(entities)
        self._clock = clock
        self._lock = threading.Lock()
        self._last_rx = {e.id: clock() for e in self._entities}
        self._available: dict[str, bool] = {}
        self._heard: set[str] = set()
        self._stop = threading.Event()
        self._thread: threading.Thread | None = None

    def contact(self, entity: Entity) -> bool:
        """Record a message from `entity` and mark it available.

        Parameters
        ----------
        entity : Entity
            One of the watched entities.

        Returns
        -------
        bool
            True the first time `entity` is heard, so the caller can
            republish discovery with the entity bound.
        """
        with self._lock:
            self._last_rx[entity.id] = self._clock()
            self._set_available(entity, True)
        if entity.id in self._heard:
            return False
        self._heard.add(entity.id)
        return True

    def heard(self, entity: Entity) -> bool:
        """Return whether `entity` has been heard since the watch was built.

        Parameters
        ----------
        entity : Entity
            One of the watched entities.

        Returns
        -------
        bool
            True once `contact` has been called for `entity`.
        """
        return entity.id in self._heard

    def sweep(self, timeout_s: float) -> None:
        """Mark each entity silent for over `timeout_s` unavailable.

        Only a transition to unavailable emits `device-silent`, so a
        device that stays silent is reported once.

        Parameters
        ----------
        timeout_s : float
            Seconds of silence after which an entity is unavailable.
        """
        now = self._clock()
        for entity in self._entities:
            with self._lock:
                if now - self._last_rx[entity.id] > timeout_s and self._set_available(entity, False):
                    self._session.health_event("device-silent", topic=entity.id)

    def start(self, timeout: Callable[[], float]) -> None:
        """Start the watchdog thread, which sweeps until `stop`.

        Call it once per watch. The thread reads the timeout before each
        wait and waits a quarter of it, at most one second.

        Parameters
        ----------
        timeout : Callable[[], float]
            Returns the current silence timeout in seconds.
        """

        def watchdog():
            while True:
                timeout_s = timeout()
                if self._stop.wait(min(1.0, timeout_s / 4)):
                    return
                self.sweep(timeout_s)

        self._thread = threading.Thread(target=watchdog, daemon=True)
        self._thread.start()

    def stop(self) -> None:
        """Stop the watchdog thread, waiting up to five seconds for it.

        The watch cannot be started again afterwards.
        """
        self._stop.set()
        if self._thread is not None:
            self._thread.join(timeout=5)

    def _set_available(self, entity: Entity, value: bool) -> bool:
        """Put an availability change; return True when it was one.

        The caller holds the lock.
        """
        if self._available.get(entity.name) == value:
            return False
        self._available[entity.name] = value
        self._session.put_json(keys.state_key(entity.room, entity.name, "available"), value)
        return True
