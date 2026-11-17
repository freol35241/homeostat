"""Test doubles for a unit's bus session, for unit tests without a bus.

`FakeSession` stands in for a `UnitSession`. Pass it to an adapter's
translation code where the session goes, or to `Context(unit, root,
session=...)` to run an automation's Context over a house directory. It
records what the unit publishes and serves the gets it makes from values
the test sets. `FakeSample` is what the test hands to the unit's
subscribers.

Only the part of `UnitSession` that unit tests use is here. A test that
needs a reply served values cannot express, such as an error reply or a
slow one, overrides `get_json_stamped` on its own session.
"""

import datetime
import json
import time
from collections.abc import Callable, Iterable
from typing import Any

import zenoh

from .stamps import stamp_us

__all__ = ["FakeSample", "FakeSession"]


class _Payload:
    def __init__(self, raw: bytes):
        self._raw = raw

    def to_bytes(self) -> bytes:
        return self._raw


class FakeSample:
    """A received sample, as a subscriber reads it.

    Parameters
    ----------
    key : str
        The sample's key.
    payload : Any
        The payload. Bytes are taken as they are, and any other value is
        encoded as JSON.
    timestamp : zenoh.Timestamp or None, optional
        The sample's stamp. Anything with `get_time()` and `get_id()` will
        do.
    kind : zenoh.SampleKind, optional
        PUT, or DELETE for a deleted key.
    """

    def __init__(
        self,
        key: str,
        payload: Any,
        *,
        timestamp: Any = None,
        kind: zenoh.SampleKind = zenoh.SampleKind.PUT,
    ):
        raw = payload if isinstance(payload, bytes) else json.dumps(payload).encode()
        self.key_expr = key
        self.payload = _Payload(raw)
        self.timestamp = timestamp
        self.kind = kind


class _Subscriber:
    def __init__(self, session: "FakeSession", entry: tuple):
        self._session = session
        self._entry = entry

    def undeclare(self) -> None:
        if self._entry in self._session._subscribers:
            self._session._subscribers.remove(self._entry)


class FakeSession:
    """A unit's session that records what it sends and serves what it asks.

    Puts, deletes and health events are recorded in the order they are
    made. A health event goes to `events` and not to `puts`. Gets are
    answered from the values set with `served` or `serve`, by key
    expression. A put or a delete does not change what is served.

    A sample is live when it has no stamp, when no router is known, or
    when its stamp carries a router's ID, as on the real session.

    Parameters
    ----------
    unit : str, optional
        The unit's name.
    served : dict or None, optional
        Key to value, served with age zero and no stamp.
    routers : iterable of str, optional
        The router IDs whose stamps mark a sample as live.

    Attributes
    ----------
    unit : str
        The unit's name.
    puts : list of tuple of (str, Any)
        ``(key, value)`` per `put_json`.
    deletes : list of str
        The key of each `delete`.
    events : list of dict
        Each health event, as ``{"kind": kind, **fields}``.
    gets : list of str
        The selector of each get, in order.
    is_ready : bool
        Whether `ready` was called.
    """

    def __init__(
        self,
        unit: str = "unit",
        *,
        served: dict[str, Any] | None = None,
        routers: Iterable[str] = (),
    ):
        self.unit = unit
        self.puts: list[tuple[str, Any]] = []
        self.deletes: list[str] = []
        self.events: list[dict] = []
        self.gets: list[str] = []
        self.is_ready = False
        self._routers = set(routers)
        self._served: dict[str, tuple[Any, float, Any]] = {}
        self._subscribers: list[tuple[zenoh.KeyExpr, Callable[[Any], None]]] = []
        for key, value in (served or {}).items():
            self.serve(key, value)

    def serve(self, key: str, value: Any, *, age_s: float = 0.0, stamp: Any = None) -> None:
        """Serve `value` at `key` to every later get that matches it.

        Serving a key again replaces its value.

        Parameters
        ----------
        key : str
            The concrete key.
        value : Any
            The decoded JSON value a get returns.
        age_s : float, optional
            The age a get reports, in seconds.
        stamp : zenoh.Timestamp or None, optional
            The stamp a get reports.
        """
        self._served[key] = (value, age_s, stamp)

    def deliver(self, sample: FakeSample) -> None:
        """Hand `sample` to every subscriber whose key expression matches it.

        Parameters
        ----------
        sample : FakeSample
            The sample to deliver.
        """
        key = zenoh.KeyExpr(str(sample.key_expr))
        for expr, callback in list(self._subscribers):
            if expr.includes(key):
                callback(sample)

    def ready(self) -> None:
        """Record that the unit declared itself ready."""
        self.is_ready = True

    def put_json(self, key: str, value: Any, *, at: datetime.datetime | None = None) -> None:
        """Record a put as ``(key, value)``.

        Parameters
        ----------
        key : str
            The concrete key.
        value : Any
            The value.
        at : datetime.datetime or None, optional
            Accepted as `UnitSession.put_json` accepts it, and not recorded.
        """
        self.puts.append((key, value))

    def delete(self, key: str) -> None:
        """Record a delete.

        Parameters
        ----------
        key : str
            The concrete key.
        """
        self.deletes.append(key)

    def health_event(self, kind: str, **fields: Any) -> None:
        """Record a health event as ``{"kind": kind, **fields}``.

        Parameters
        ----------
        kind : str
            The event's `kind`.
        **fields : Any
            Further fields of the event.
        """
        self.events.append({"kind": kind, **fields})

    def subscribe(self, keyexpr: str, callback: Callable[[Any], None]) -> _Subscriber:
        """Keep `callback` for the samples `deliver` hands over on `keyexpr`.

        Parameters
        ----------
        keyexpr : str
            The key expression to subscribe to.
        callback : Callable
            Called with each matching sample.

        Returns
        -------
        object
            A subscriber whose `undeclare()` stops the deliveries.
        """
        entry = (zenoh.KeyExpr(keyexpr), callback)
        self._subscribers.append(entry)
        return _Subscriber(self, entry)

    def get_json(self, selector: str, *, timeout_s: float | None = None) -> list[tuple[str, Any]]:
        """Return ``(key, value)`` per served key the selector matches.

        As `get_json_stamped`, without the ages and stamps.

        Parameters
        ----------
        selector : str
            The key expression, optionally with parameters, which are
            ignored.
        timeout_s : float or None, optional
            Accepted and ignored.

        Returns
        -------
        list of tuple of (str, Any)
            ``(key, value)`` per match.
        """
        return [(key, value) for key, value, _, _ in self.get_json_stamped(selector, timeout_s=timeout_s)]

    def get_json_aged(
        self, selector: str, *, timeout_s: float | None = None
    ) -> list[tuple[str, Any, float]]:
        """Return ``(key, value, age_s)`` per served key the selector matches.

        As `get_json_stamped`, without the stamps.

        Parameters
        ----------
        selector : str
            The key expression, optionally with parameters, which are
            ignored.
        timeout_s : float or None, optional
            Accepted and ignored.

        Returns
        -------
        list of tuple of (str, Any, float)
            ``(key, value, age_s)`` per match.
        """
        return [
            (key, value, age_s)
            for key, value, age_s, _ in self.get_json_stamped(selector, timeout_s=timeout_s)
        ]

    def get_json_stamped(
        self, selector: str, *, timeout_s: float | None = None
    ) -> list[tuple[str, Any, float, Any]]:
        """Record the get and return what is served under it.

        `get_json` and `get_json_aged` answer through this method, so a
        test that overrides it changes all three.

        Parameters
        ----------
        selector : str
            The key expression, optionally with parameters, which are
            ignored.
        timeout_s : float or None, optional
            Accepted and ignored.

        Returns
        -------
        list of tuple of (str, Any, float, Any)
            ``(key, value, age_s, stamp)`` per match, in the order served.
        """
        self.gets.append(selector)
        expr = zenoh.KeyExpr(selector.split("?", 1)[0])
        return [
            (key, value, age_s, stamp)
            for key, (value, age_s, stamp) in self._served.items()
            if expr.includes(zenoh.KeyExpr(key))
        ]

    def is_live(self, sample: Any) -> bool:
        """Return whether a sample is live, as `UnitSession.is_live` does.

        Parameters
        ----------
        sample : FakeSample
            A received sample.

        Returns
        -------
        bool
            True for a live sample.
        """
        return self.is_live_stamp(getattr(sample, "timestamp", None))

    def is_live_stamp(self, stamp: Any) -> bool:
        """Return whether a stamp marks a live value.

        Parameters
        ----------
        stamp : zenoh.Timestamp or None
            The stamp.

        Returns
        -------
        bool
            True for no stamp, no known router, or a router's stamp.
        """
        if stamp is None or not self._routers:
            return True
        return str(stamp.get_id()) in self._routers

    def sample_age(self, sample: Any) -> float:
        """Return how old a sample's value is, as `UnitSession.sample_age` does.

        Parameters
        ----------
        sample : FakeSample
            A received sample.

        Returns
        -------
        float
            Zero for a live sample, else the seconds since its stamp.
        """
        stamped = None if self.is_live(sample) else stamp_us(sample.timestamp)
        if stamped is None:
            return 0.0
        return max(0.0, (time.time_ns() // 1000 - stamped) / 1e6)
