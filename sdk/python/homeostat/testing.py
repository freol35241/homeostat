"""Test doubles for a unit's bus session, for unit tests without a bus.

`FakeSession` is a `UnitSession` over a fake zenoh session. It inherits
UnitSession's methods unchanged, so a put is checked, encoded and stamped
by the real code, and a sample's age follows the real rule. The fake
zenoh session records what reaches it and answers gets from values the
test serves or fails. It is simpler than zenoh in the ways FakeSession's
docstring lists.

Pass a `FakeSession` to an adapter's translation code where the session
goes, or to `Context(unit, root, session=...)` to run an automation's
Context over a house directory. `FakeSample` is what the test hands to
the unit's subscribers with `FakeSession.deliver`.

A test that needs a reply neither `serve` nor `fail` can give, such as a
slow one, overrides `get_json_stamped` on its own session.
"""

import datetime
import json
from collections.abc import Callable, Iterable
from typing import Any

import zenoh

from . import keys
from .session import UnitSession

__all__ = ["FakeSample", "FakeSession"]

# The ID in the stamps this session sets with `put_json(..., at=)`. It is
# not a router's, so a consumer takes such a value as stamped.
_SESSION_ID = zenoh.TimestampId(b"\xfa")


class _Bytes:
    def __init__(self, raw: bytes):
        self._raw = raw

    def to_bytes(self) -> bytes:
        return self._raw


class FakeSample:
    """A received sample, as a subscriber or a get reads it.

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
    attachment : bytes or None, optional
        The sample's attachment, which carries a mirror reply's age.
    """

    def __init__(
        self,
        key: str,
        payload: Any,
        *,
        timestamp: Any = None,
        kind: zenoh.SampleKind = zenoh.SampleKind.PUT,
        attachment: bytes | None = None,
    ):
        raw = payload if isinstance(payload, bytes) else json.dumps(payload).encode()
        self.key_expr = key
        self.payload = _Bytes(raw)
        self.timestamp = timestamp
        self.kind = kind
        self.attachment = None if attachment is None else _Bytes(attachment)


class _Handle:
    def __init__(self, entries: list, entry: Any = None):
        self._entries = entries
        self._entry = entry

    def undeclare(self) -> None:
        if self._entry in self._entries:
            self._entries.remove(self._entry)


class _Reply:
    def __init__(self, ok: FakeSample | None = None, err: FakeSample | None = None):
        self.ok = ok
        self.err = err


class _Publisher:
    def __init__(self, fake: "FakeSession", key: str):
        self._fake = fake
        self._key = zenoh.KeyExpr(key)

    @property
    def matching_status(self) -> Any:
        matching = any(
            zenoh.KeyExpr(expr).includes(self._key) for expr in list(self._fake.subscribed_elsewhere)
        )
        return type("MatchingStatus", (), {"matching": matching})()


class _Liveliness:
    def __init__(self, fake: "FakeSession"):
        self._fake = fake

    def declare_token(self, key: str) -> "_Token":
        unit = key.split("/")[2]
        self._fake.alive.add(unit)
        return _Token(self._fake.alive, unit)

    def get(self, key: str, timeout: float | None = None) -> list:
        alive = {keys.liveliness_key(unit) for unit in self._fake.alive}
        return [_Reply(FakeSample(key, b""))] if key in alive else []


class _Token:
    def __init__(self, alive: set[str], unit: str):
        self._alive = alive
        self._unit = unit

    def undeclare(self) -> None:
        self._alive.discard(self._unit)


class _Info:
    def __init__(self, fake: "FakeSession"):
        self._fake = fake

    def routers_zid(self) -> list[str]:
        return list(self._fake.routers)


class _Transport:
    """The zenoh session a FakeSession's own code talks to."""

    def __init__(self, fake: "FakeSession"):
        self._fake = fake
        self.info = _Info(fake)

    def put(self, key: str, payload: Any, **options: Any) -> None:
        value = json.loads(payload)
        if key == keys.health_event_key(self._fake.unit):
            self._fake.events.append(value)
            return
        self._fake.puts.append((key, value))
        self._fake.put_options.append(options)

    def delete(self, key: str) -> None:
        self._fake.deletes.append(key)

    def get(self, selector: str, timeout: float | None = None, payload: Any = None) -> list:
        self._fake.gets.append(selector)
        expr = zenoh.KeyExpr(selector.split("?", 1)[0])
        failed = [
            error
            for key, error in self._fake._failures.items()
            if expr.includes(zenoh.KeyExpr(key))
        ]
        if failed:
            return [
                _Reply(err=FakeSample(selector, error.encode()))
                for error in failed
                if error is not None
            ]
        if payload is not None:
            # A config write. The core would validate it; the fake stores
            # it as given and replies with it.
            value = json.loads(payload)
            self._fake.serve(selector, value)
            return [_Reply(FakeSample(selector, value))]
        return [
            _Reply(
                FakeSample(
                    key,
                    value,
                    timestamp=stamp,
                    attachment=None if age_s is None else str(age_s).encode(),
                )
            )
            for key, (value, age_s, stamp) in self._fake._served.items()
            if expr.includes(zenoh.KeyExpr(key))
        ]

    def declare_subscriber(self, keyexpr: str, callback: Callable[[Any], None]) -> _Handle:
        entry = (zenoh.KeyExpr(keyexpr), callback)
        self._fake._subscribers.append(entry)
        return _Handle(self._fake._subscribers, entry)

    def declare_publisher(self, key: str) -> _Publisher:
        return _Publisher(self._fake, key)

    def declare_queryable(self, keyexpr: str, callback: Callable[[Any], None]) -> _Handle:
        return _Handle([])

    def liveliness(self) -> _Liveliness:
        return _Liveliness(self._fake)

    def new_timestamp(self) -> zenoh.Timestamp:
        return zenoh.Timestamp(datetime.datetime.now(datetime.timezone.utc), _SESSION_ID)

    def close(self) -> None:
        pass


class FakeSession(UnitSession):
    """A `UnitSession` whose zenoh session is a fake that records and serves.

    What reaches the fake transport is recorded in order. A health event
    goes to `events` and not to `puts`. A put that the session refuses,
    such as a non-finite value, is not recorded, as it never reaches the
    bus. Gets are answered from the values set with `served` or `serve`,
    by key expression, or fail as `fail` says. A put or a delete does not
    change what is served. The unit's own puts are not delivered to its own
    subscribers, and its own subscribers do not count for `has_subscriber`.
    A zenoh session in peer mode does both.

    Parameters
    ----------
    unit : str, optional
        The unit's name.
    served : dict or None, optional
        Key to value, served with age zero and no stamp.
    routers : iterable of str, optional
        The router IDs whose stamps mark a sample as live. With none, every
        sample is live, as on a real session that knows no router.

    Attributes
    ----------
    puts : list of tuple of (str, Any)
        ``(key, value)`` per put, the value decoded from the JSON sent.
    put_options : list of dict
        The zenoh options each of `puts` was sent with, such as its stamp
        or a command's priority.
    deletes : list of str
        The key of each delete.
    events : list of dict
        Each health event, as sent: ``{"kind": kind, **fields}``.
    gets : list of str
        The selector of each get, in order.
    alive : set of str
        The units whose liveliness token is declared. `ready` adds this
        unit, and a test adds others for `is_alive`.
    subscribed_elsewhere : set of str
        Key expressions other units subscribe to, for `has_subscriber`.
    routers : list of str
        The router IDs, as given.
    """

    def __init__(
        self,
        unit: str = "unit",
        *,
        served: dict[str, Any] | None = None,
        routers: Iterable[str] = (),
    ):
        self.puts: list[tuple[str, Any]] = []
        self.put_options: list[dict] = []
        self.deletes: list[str] = []
        self.events: list[dict] = []
        self.gets: list[str] = []
        self.alive: set[str] = set()
        self.subscribed_elsewhere: set[str] = set()
        self.routers = list(routers)
        self._served: dict[str, tuple[Any, float | None, Any]] = {}
        self._failures: dict[str, str | None] = {}
        self._subscribers: list[tuple[zenoh.KeyExpr, Callable[[Any], None]]] = []
        super().__init__(unit, "", transport=_Transport(self))
        for key, value in (served or {}).items():
            self.serve(key, value)

    @property
    def is_ready(self) -> bool:
        """Whether this unit's liveliness token is declared."""
        return self.unit in self.alive

    def serve(
        self, key: str, value: Any, *, age_s: float | None = 0.0, stamp: Any = None
    ) -> None:
        """Serve `value` at `key` to every later get that matches it.

        Serving a key again replaces its value, and ends a `fail` on it.

        Parameters
        ----------
        key : str
            The concrete key.
        value : Any
            The value. Bytes are sent as they are, and any other value is
            encoded as JSON.
        age_s : float or None, optional
            The age the reply carries in its attachment, in seconds. None
            sends no attachment.
        stamp : zenoh.Timestamp or None, optional
            The stamp the reply carries.
        """
        self._failures.pop(key, None)
        self._served[key] = (value, age_s, stamp)

    def fail(self, key: str, error: str | None = None) -> None:
        """Make every later get that matches `key` fail, until it is served again.

        The get receives an error reply carrying `error`, as zenoh delivers
        a queryable's refusal. zenoh reports a get that ran out of time as
        an error reply carrying "Timeout". With `error` None the get
        receives no reply at all, as when nothing answers.

        A failing key takes precedence over every served one, so the get
        receives only the failures that match it.

        Parameters
        ----------
        key : str
            The concrete key.
        error : str or None, optional
            The error reply's payload, or None for no reply.
        """
        self._failures[key] = error

    def deliver(self, sample: FakeSample) -> None:
        """Hand `sample` to every subscriber whose key expression matches it.

        Parameters
        ----------
        sample : FakeSample
            The sample to deliver.

        Raises
        ------
        AssertionError
            If no subscriber matches, so a test cannot pass on a sample
            nobody received.
        """
        key = zenoh.KeyExpr(str(sample.key_expr))
        matching = [callback for expr, callback in self._subscribers if expr.includes(key)]
        if not matching:
            raise AssertionError(f"no subscriber matches {key}")
        for callback in matching:
            callback(sample)
