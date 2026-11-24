"""Bus session for a supervised unit.

`connect()` reads HOMEOSTAT_UNIT and HOMEOSTAT_BUS, which the supervisor
sets. It opens a client session against the supervisor's router and
returns a UnitSession. Scouting is off, because the topology is explicit.
Call `ready()` once the unit can do its job. The supervisor treats a unit
as up when its liveliness token is declared, not when its process
starts. `wait_for_shutdown()` blocks until the supervisor's SIGTERM.
`shutdown_event()` returns the event the signal sets, for a unit whose
worker threads stop on it, and `async_shutdown_event()` is its asyncio
form.
"""

import asyncio
import datetime
import json
import os
import signal
import threading
import time
import weakref
from collections.abc import Callable
from typing import Any

import zenoh

from . import forecast, keys
from .stamps import stamp_us

# The classes that carry commands: a request (home/cmd) and the arbiter's
# forward of it (home/arbiter). put_json sends both as commands.
COMMAND_PREFIXES = ("home/cmd/", "home/arbiter/")

# The sessions not yet closed.
_open_sessions: "weakref.WeakSet[UnitSession]" = weakref.WeakSet()


def _close_open_sessions() -> None:
    """Close every session still open when the main thread finishes.

    zenoh runs each subscriber's callback on a non-daemon Python thread,
    which lasts until the session closes, and the interpreter waits for
    those threads before it exits. Without this, a unit whose `main()`
    raises, calls `sys.exit()` or returns with a session open never exits,
    and the supervisor never sees it stop.
    """
    for session in list(_open_sessions):
        session.close()


# threading runs these hooks when the main thread finishes, before it waits
# for non-daemon threads; atexit runs after that wait. concurrent.futures
# registers its shutdown the same way. The function is private, so typeshed
# does not declare it.
threading._register_atexit(_close_open_sessions)  # pyright: ignore[reportAttributeAccessIssue]


def connect() -> "UnitSession":
    """Open a bus session for the unit the supervisor started.

    The unit name and the router endpoint come from HOMEOSTAT_UNIT and
    HOMEOSTAT_BUS.

    Returns
    -------
    UnitSession
        The open session; call its `ready()` once the unit can do its job.

    Raises
    ------
    KeyError
        If HOMEOSTAT_UNIT or HOMEOSTAT_BUS is unset.
    """
    unit = os.environ[keys.ENV_UNIT]
    endpoint = os.environ[keys.ENV_BUS]
    return UnitSession(unit, endpoint)


# The process's one shutdown event. Its handlers are installed on the first
# call to `shutdown_event`, because a second install would replace them and
# strand the event an earlier caller holds.
_shutdown = threading.Event()
_shutdown_installed = False

# The asyncio shutdown event of each running loop, for the same reason.
_async_shutdown: "weakref.WeakKeyDictionary[asyncio.AbstractEventLoop, asyncio.Event]" = (
    weakref.WeakKeyDictionary()
)


def shutdown_event() -> threading.Event:
    """Return the event that SIGTERM or SIGINT sets.

    SIGTERM is the supervisor's stop signal. A unit that only waits for it
    calls `wait_for_shutdown`. One that runs worker threads passes them the
    event, so they see the stop too. Every call returns the same event, so
    a unit can hand it to a worker and still call `Context.run`.

    Returns
    -------
    threading.Event
        Set when either signal arrives.

    Raises
    ------
    ValueError
        If the first call is made from a thread other than the main thread,
        the only one Python lets install signal handlers.
    """
    global _shutdown_installed
    if not _shutdown_installed:
        signal.signal(signal.SIGTERM, lambda *_: _shutdown.set())
        signal.signal(signal.SIGINT, lambda *_: _shutdown.set())
        _shutdown_installed = True
    return _shutdown


def wait_for_shutdown() -> None:
    """Block until SIGTERM or SIGINT.

    Raises
    ------
    ValueError
        If the first call to `shutdown_event` is made from a thread other
        than the main thread.
    """
    shutdown_event().wait()


def async_shutdown_event() -> asyncio.Event:
    """Return the asyncio event that SIGTERM or SIGINT sets.

    The asyncio form of `shutdown_event`. The handlers are the running
    loop's, so the signal wakes the loop wherever it is waiting. Every call
    on one loop returns the same event.

    Returns
    -------
    asyncio.Event
        Set when either signal arrives.

    Raises
    ------
    RuntimeError
        If no event loop is running in this thread, or if the loop's first
        call is made from a thread other than the main thread.
    """
    loop = asyncio.get_running_loop()
    stop = _async_shutdown.get(loop)
    if stop is None:
        stop = asyncio.Event()
        for sig in (signal.SIGTERM, signal.SIGINT):
            loop.add_signal_handler(sig, stop.set)
        _async_shutdown[loop] = stop
    return stop


class ConfigWriteError(Exception):
    """A parameter write the core rejected (constraint, unknown key, ...)."""


class QueryError(Exception):
    """A queryable answered a get with an error reply.

    For example the recorder's "limit: 0 is not positive" or "store
    unavailable: ...".
    """


class QueryTimeout(QueryError):
    """A get ran out of time before every queryable had answered.

    Zenoh delivers this as an error reply too. It means the queryable has
    not answered yet, not that it refused.
    """


class UnitSession:
    """A unit's client session on the bus, against the supervisor's router.

    Parameters
    ----------
    unit : str
        The unit's name.
    endpoint : str
        The router's zenoh endpoint, as HOMEOSTAT_BUS gives it. Unused when
        `transport` is given.
    transport : zenoh.Session or None, optional
        The zenoh session to talk through. None opens a client session
        against `endpoint`. `homeostat.testing.FakeSession` passes a fake
        one, so tests run this class's own code without a bus.
    """

    def __init__(self, unit: str, endpoint: str, *, transport: Any = None):
        self.unit = unit
        if transport is None:
            config = zenoh.Config()
            config.insert_json5("mode", '"client"')
            config.insert_json5("connect/endpoints", json.dumps([endpoint]))
            config.insert_json5("scouting/multicast/enabled", "false")
            config.insert_json5("scouting/gossip/enabled", "false")
            transport = zenoh.open(config)
        self._session: zenoh.Session = transport
        _open_sessions.add(self)
        self._token = None
        # key -> publisher, for has_subscriber. Matching status belongs to a
        # publisher, and a new publisher knows nothing until the router has
        # told it. Each key's publisher is therefore declared once and
        # reused.
        self._publishers: dict[str, Any] = {}
        # The router's ID, as the stamps it sets carry it. Read on first use,
        # once the session has reached the router.
        self._router_ids: set[str] = set()

    def ready(self) -> None:
        """Declare the liveliness token at home/health/{unit}/alive."""
        self._token = self._session.liveliness().declare_token(
            keys.liveliness_key(self.unit)
        )

    def put_json(self, key: str, value: Any, *, at: datetime.datetime | None = None) -> None:
        """Publish a JSON-encoded value.

        A value containing a non-finite float (NaN, Infinity) is dropped
        with a "non-finite" health event instead. Python's json accepts and
        emits these, but JSON has no syntax for them. Every consumer would
        otherwise have to guard against them, and the recorder cannot store
        them (docs/design.md#bus-payload-conventions).

        `at` stamps a state value with the time it describes, such as the
        start of the hour an hourly total covers, instead of the time it
        is published (docs/design.md#timestamps). Consumers then see the
        value's real age, and the recorder stores it at that time. A value
        about the future is a forecast, so `at` may not be later than now.
        The core's router replaces a stamp more than half a second ahead of
        its own clock with one of its own, which makes the value live. A
        unit runs on the core's host, so the two clocks agree.

        Parameters
        ----------
        key : str
            The concrete key to put on. A home/cmd/ or home/arbiter/ key is
            sent as a command.
        value : Any
            A JSON-encodable value.
        at : datetime.datetime or None, optional
            The time the value describes, timezone-aware. Only a state key
            takes one.

        Raises
        ------
        ValueError
            If `at` is given for a key outside home/state/, has no timezone,
            or is in the future.
        """
        timestamp = None
        if at is not None:
            if not key.startswith("home/state/"):
                raise ValueError(f"only a state value takes a stamp, not {key!r}")
            if at.tzinfo is None:
                raise ValueError("a stamp needs a timezone")
            if at > datetime.datetime.now(datetime.timezone.utc):
                raise ValueError(f"a stamp may not be in the future: {at.isoformat()}")
            # The session's own ID, which differs from the router's, so a
            # consumer can tell this value from one stamped on arrival.
            timestamp = zenoh.Timestamp(at, self._session.new_timestamp().get_id())
        try:
            encoded = json.dumps(value, allow_nan=False)
        except ValueError:
            self.health_event("drop", reason="non-finite", key=key)
            return
        if key.startswith(COMMAND_PREFIXES):
            # By default zenoh drops a put when the link is congested. That
            # is fine for a temperature reading, which the next one
            # replaces, but not for "unlock the door". Commands block until
            # there is room instead, and go ahead of data in the queues.
            self._session.put(
                key,
                encoded,
                congestion_control=zenoh.CongestionControl.BLOCK,
                priority=zenoh.Priority.INTERACTIVE_HIGH,
            )
            return
        if timestamp is not None:
            self._session.put(key, encoded, timestamp=timestamp)
            return
        self._session.put(key, encoded)

    def put_forecast(self, key: str, issued, points) -> None:
        """Publish a forecast (docs/design.md#forecasts).

        The forecast is retained like state, so a consumer that restarts
        mid-horizon has its inputs at once instead of waiting for the next
        issue. The SDK refuses a naive timestamp, a non-finite value, two
        points at one instant, or more points than the limit. Such a
        payload is dropped with an "invalid-forecast" health event that
        carries the reason, as for every other refusal on the producer
        side. The unit stays up, and the event says what it tried to
        publish.

        Parameters
        ----------
        key : str
            A concrete `home/forecast/{room}/{entity}/{aspect}/{source}` key.
        issued : datetime.datetime
            When the forecast was issued; must carry a UTC offset.
        points : iterable of Point or tuple
            The points, as `homeostat.forecast.encode` accepts them.
        """
        try:
            encoded = forecast.encode(issued, points)
        except ValueError as error:
            self.health_event("drop", reason="invalid-forecast", key=key, detail=str(error))
            return
        self._session.put(key, encoded)

    def parse_command(self, sample: zenoh.Sample):
        """Run the command prologue every adapter shares (docs/adapters.md#4-commands).

        The result is the aspect, the envelope's value and its correlation
        id. It is None after a drop event: "malformed-payload" for a payload
        that is not JSON, and "invalid-command" for one that is not an
        envelope. It is also None, with no event, for a hold intent of
        "report" or "release", which asks the device for nothing
        (docs/design.md#hold-intents). An adapter that honours command
        options calls
        `parse_command_with_options` instead.

        The id is returned because the adapter's own later validation (out
        of range, no such command) can end the same command. The publisher
        is waiting to hear which stage stopped it. A payload that did not
        parse has no id to report.

        Parameters
        ----------
        sample : zenoh.Sample
            A sample received on a command key.

        Returns
        -------
        tuple of (str, Any, str or None), or None
            ``(aspect, value, cmd_id)``, or None if the sample was dropped.
        """
        parsed = self.parse_command_with_options(sample)
        return None if parsed is None else parsed[:3]

    def parse_command_with_options(self, sample: zenoh.Sample):
        """Run the command prologue and also return the envelope's options.

        It drops what `parse_command` drops, and an envelope whose options
        are not an object (docs/design.md#command-options).

        Parameters
        ----------
        sample : zenoh.Sample
            A sample received on a command key.

        Returns
        -------
        tuple of (str, Any, str or None, dict), or None
            ``(aspect, value, cmd_id, options)``, or None if the sample was
            dropped.
        """
        key = str(sample.key_expr)
        aspect = key.split("/", 4)[4]
        try:
            payload = json.loads(sample.payload.to_bytes())
        except ValueError:
            self.health_event("drop", reason="malformed-payload", key=key)
            return None
        cmd_id = keys.cmd_envelope_id(payload)
        try:
            value = keys.parse_cmd_envelope(payload)
        except ValueError:
            self.health_event("drop", reason="invalid-command", key=key, cmd_id=cmd_id)
            return None
        if keys.cmd_envelope_hold(payload) in ("report", "release"):
            # Both speak to the arbiter about the family hold, and neither
            # asks the device for anything. An adapter receives one only for
            # an aspect nothing arbitrates, where there is no hold to keep.
            return None
        return aspect, value, cmd_id, keys.cmd_envelope_options(payload)

    def delete(self, key: str) -> None:
        """Delete a key, so the core's mirror forgets it.

        A unit that publishes one key per item, such as one hold per held
        aspect, deletes the key when the item ends. Subscribers see a sample
        of kind DELETE.

        Parameters
        ----------
        key : str
            The concrete key to delete.
        """
        self._session.delete(key)

    def has_subscriber(self, key: str, *, wait_s: float = 0.5) -> bool:
        """Return whether anything on the bus subscribes to `key`.

        That is, whether a put there reaches anyone at all. A client session
        filters writes on the publishing side, so a put that matches no
        subscriber is dropped silently. This method is the only way for the
        publisher to find out.

        It blocks for up to `wait_s`. The first time a key is asked about,
        its publisher has not yet heard from the router, and an immediate
        False could report a missing subscriber that exists. The wait ends
        as soon as the answer is True.

        Parameters
        ----------
        key : str
            The concrete key a put would go to.
        wait_s : float, optional
            Longest time in seconds to wait for a match.

        Returns
        -------
        bool
            True if a subscriber matches `key`, False if none did in time.
        """
        publisher = self._publishers.get(key)
        if publisher is None:
            publisher = self._publishers[key] = self._session.declare_publisher(key)
        deadline = time.monotonic() + wait_s
        # zenoh's stub types matching_status as bool; it is a MatchingStatus.
        while not publisher.matching_status.matching:  # pyright: ignore[reportAttributeAccessIssue]
            if time.monotonic() >= deadline:
                return False
            time.sleep(0.02)
        return True

    def is_alive(self, unit: str, *, timeout_s: float = 2.0) -> bool:
        """Return whether `unit` holds its liveliness token (home/health/{unit}/alive).

        This is the supervisor's own test for "up". It answers for the unit
        itself. A subscriber match on a key could come from any unit. The
        recorder subscribes to every command, for example, so a match on a
        command key says nothing about the adapter that should act on it.

        Parameters
        ----------
        unit : str
            The unit to ask about.
        timeout_s : float, optional
            Seconds to wait for liveliness replies.

        Returns
        -------
        bool
            True if the unit's token is declared.
        """
        replies = self._session.liveliness().get(keys.liveliness_key(unit), timeout=timeout_s)
        return any(reply.ok is not None for reply in replies)

    def subscribe(self, keyexpr: str, callback: Callable[[zenoh.Sample], None]):
        """Declare a subscriber that calls `callback` for each sample on `keyexpr`.

        Parameters
        ----------
        keyexpr : str
            The key expression to subscribe to.
        callback : Callable[[zenoh.Sample], None]
            Called with each sample, on zenoh's thread.

        Returns
        -------
        zenoh.Subscriber
            The subscriber; undeclare it to stop.
        """
        return self._session.declare_subscriber(keyexpr, callback)

    def subscribe_json(self, keyexpr: str, handler: Callable[[str, Any, zenoh.Sample], None]):
        """Declare a subscriber that decodes each sample's JSON payload.

        A put whose payload is not JSON is dropped with a "malformed-payload"
        event naming its key, as `parse_command` drops one
        (docs/adapters.md#6-health-events). A delete has no payload and is skipped.

        Parameters
        ----------
        keyexpr : str
            The key expression to subscribe to.
        handler : Callable[[str, Any, zenoh.Sample], None]
            Called as `handler(key, value, sample)` for each put that
            decodes, on zenoh's thread.

        Returns
        -------
        zenoh.Subscriber
            The subscriber; undeclare it to stop.
        """

        def callback(sample: zenoh.Sample) -> None:
            decoded = self.decode_json(sample)
            if decoded is not None:
                handler(*decoded, sample)

        return self.subscribe(keyexpr, callback)

    def decode_json(self, sample: zenoh.Sample) -> tuple[str, Any] | None:
        """Return a sample's key and decoded JSON payload, by `subscribe_json`'s rule.

        A put whose payload is not JSON is dropped with a "malformed-payload"
        event naming its key. A delete has no payload and is skipped.

        Parameters
        ----------
        sample : zenoh.Sample
            The sample a subscriber received.

        Returns
        -------
        tuple[str, Any] | None
            The key and the value, or None for a sample that is skipped.
        """
        if sample.kind == zenoh.SampleKind.DELETE:
            return None
        key = str(sample.key_expr)
        try:
            return key, json.loads(sample.payload.to_bytes())
        except ValueError:
            self.health_event("drop", reason="malformed-payload", key=key)
            return None

    def is_live(self, sample: zenoh.Sample) -> bool:
        """Return whether a sample is a live publish rather than a stamped one.

        The core's router stamps a sample on arrival unless its publisher
        set a stamp. A sample with the router's stamp, or with none, is
        live: its value is true now. A sample whose publisher set the stamp
        says when its value was true, which can be long ago. The core does
        that when it replays recorded state after a restart
        (docs/design.md#replay-after-a-core-restart), and a unit does it
        with `put_json(..., at=)`.

        Parameters
        ----------
        sample : zenoh.Sample
            A received sample.

        Returns
        -------
        bool
            True for a live sample.
        """
        return self.is_live_stamp(getattr(sample, "timestamp", None))

    def is_live_stamp(self, stamp: Any) -> bool:
        """Return whether a stamp was set by the core's router on arrival.

        As `is_live`, for a stamp read from a mirror reply rather than a
        received sample.

        Parameters
        ----------
        stamp : zenoh.Timestamp or None
            The stamp.

        Returns
        -------
        bool
            True for a router stamp or no stamp.
        """
        if stamp is None:
            return True
        if not self._router_ids:
            self._router_ids = {str(zid) for zid in self._session.info.routers_zid()}
        # With no router known, no stamp can be told from a router's, so
        # every sample is taken as live.
        return not self._router_ids or str(stamp.get_id()) in self._router_ids

    def sample_age(self, sample: zenoh.Sample) -> float:
        """Return how old a sample's value is, in seconds.

        Zero for a live sample. For a stamped one, the time since the stamp
        its publisher set, and never negative.

        Parameters
        ----------
        sample : zenoh.Sample
            A received sample.

        Returns
        -------
        float
            The value's age in seconds.
        """
        stamped = None if self.is_live(sample) else stamp_us(sample.timestamp)
        if stamped is None:
            return 0.0
        return max(0.0, (time.time_ns() // 1000 - stamped) / 1e6)

    def declare_queryable(self, keyexpr: str, callback: Callable[[zenoh.Query], None]):
        """Declare a queryable that calls `callback` for each get on `keyexpr`.

        Parameters
        ----------
        keyexpr : str
            The key expression to answer for.
        callback : Callable[[zenoh.Query], None]
            Called with each query, on zenoh's thread.

        Returns
        -------
        zenoh.Queryable
            The queryable; undeclare it to stop.
        """
        return self._session.declare_queryable(keyexpr, callback)

    def get_json(
        self, selector: str, *, timeout_s: float | None = None
    ) -> list[tuple[str, Any]]:
        """Query the bus, returning (key, decoded JSON) per ok reply.

        As `get_json_aged`, without the ages.

        Parameters
        ----------
        selector : str
            The key expression to query, optionally with parameters.
        timeout_s : float or None, optional
            Bound on the wait for replies; zenoh's default (10 s) when None.

        Returns
        -------
        list of tuple of (str, Any)
            ``(key, value)`` per ok reply with a JSON payload.

        Raises
        ------
        QueryTimeout
            If the wait for replies runs out.
        QueryError
            If a queryable answers with an error reply.
        """
        return [
            (key, value)
            for key, value, _ in self.get_json_aged(selector, timeout_s=timeout_s)
        ]

    def get_json_aged(
        self, selector: str, *, timeout_s: float | None = None
    ) -> list[tuple[str, Any, float]]:
        """Query the bus, returning (key, decoded JSON, age in seconds) per ok reply.

        As `get_json_stamped`, without the stamps.

        Parameters
        ----------
        selector : str
            The key expression to query, optionally with parameters.
        timeout_s : float or None, optional
            Bound on the wait for replies, in seconds.

        Returns
        -------
        list of tuple of (str, Any, float)
            ``(key, value, age_s)`` per ok reply with a JSON payload.

        Raises
        ------
        QueryTimeout
            If the wait for replies runs out.
        QueryError
            If a queryable answers with an error reply.
        """
        return [
            (key, value, age_s)
            for key, value, age_s, _ in self.get_json_stamped(selector, timeout_s=timeout_s)
        ]

    def get_json_stamped(
        self, selector: str, *, timeout_s: float | None = None
    ) -> list[tuple[str, Any, float, Any]]:
        """Query the bus, returning (key, decoded JSON, age, stamp) per ok reply.

        The age is read from the reply's attachment, which the core's
        last-value mirrors write. A reply without one has age zero. The
        stamp is the reply's timestamp: the mirrored value's own, or None.
        A reply whose payload is not JSON is left out, with no event.
        `timeout_s` bounds the wait for replies, with zenoh's default of
        10 s when None. Running out raises QueryTimeout.

        Parameters
        ----------
        selector : str
            The key expression to query, optionally with parameters.
        timeout_s : float or None, optional
            Bound on the wait for replies, in seconds.

        Returns
        -------
        list of tuple of (str, Any, float, zenoh.Timestamp or None)
            ``(key, value, age_s, stamp)`` per ok reply with a JSON payload.

        Raises
        ------
        QueryTimeout
            If the wait for replies runs out.
        QueryError
            If a queryable answers with an error reply.
        """
        values = []
        for reply in self._session.get(selector, timeout=timeout_s):
            sample = reply.ok
            if sample is None:
                # An error reply means the queryable refused. Ignoring it
                # would look like an empty result.
                if (err := reply.err) is not None:
                    text = err.payload.to_bytes().decode(errors="replace")
                    # Zenoh reports its own timeout as an error reply with
                    # this payload. There is no other signal for it.
                    if text == "Timeout":
                        raise QueryTimeout(text)
                    try:
                        decoded = json.loads(text)
                    except ValueError:
                        decoded = text
                    if isinstance(decoded, dict) and "error" in decoded:
                        decoded = decoded["error"]
                    raise QueryError(str(decoded))
                continue
            try:
                value = json.loads(sample.payload.to_bytes())
            except ValueError:
                continue
            attachment = sample.attachment
            age_s = float(attachment.to_bytes()) if attachment is not None else 0.0
            stamp = getattr(sample, "timestamp", None)
            values.append((str(sample.key_expr), value, age_s, stamp))
        return values

    def write_config(self, unit: str, param: str, value: Any) -> Any:
        """Write a parameter through the core's validating config queryable.

        This sends a GET with a payload to the concrete key. The core
        validates the value against the manifest constraint, stores it,
        republishes it, and replies with the stored value. A rejected write
        raises ConfigWriteError with the core's message. A plain put would
        bypass validation.

        Parameters
        ----------
        unit : str
            The unit whose parameter to write.
        param : str
            The parameter's name, as declared in the unit's manifest.
        value : Any
            The new value, JSON-encodable.

        Returns
        -------
        Any
            The value as the core stored it.

        Raises
        ------
        ConfigWriteError
            If the core rejects the write, or nothing replies.
        """
        key = keys.config_key(unit, param)
        for reply in self._session.get(key, payload=json.dumps(value)):
            sample = reply.ok
            if sample is not None:
                return json.loads(sample.payload.to_bytes())
            err = reply.err
            if err is not None:
                try:
                    message = json.loads(err.payload.to_bytes())["error"]
                except (ValueError, KeyError, TypeError):
                    message = err.payload.to_bytes().decode(errors="replace")
                raise ConfigWriteError(message)
        raise ConfigWriteError(f"no reply for {key} — is the core running?")

    def health_event(self, kind: str, **fields: Any) -> None:
        """Publish a JSON event at home/health/{unit}/event.

        Parameters
        ----------
        kind : str
            The event's `kind`.
        **fields : Any
            Further JSON-encodable fields of the event.
        """
        self.put_json(keys.health_event_key(self.unit), {"kind": kind, **fields})

    def close(self) -> None:
        """Undeclare the liveliness token, if declared, and close the session."""
        if self._token is not None:
            self._token.undeclare()
            self._token = None
        _open_sessions.discard(self)
        self._session.close()
