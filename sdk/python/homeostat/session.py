"""Bus session for a supervised unit.

`connect()` reads HOMEOSTAT_UNIT and HOMEOSTAT_BUS, which the supervisor
sets. It opens a client session against the supervisor's router and
returns a UnitSession. Scouting is off, because the topology is explicit.
Call `ready()` once the unit can do its job. The supervisor treats a unit
as up when its liveliness token is declared, not when its process
starts.
"""

import json
import os
import time
from collections.abc import Callable
from typing import Any

import zenoh

from . import forecast, keys
from .stamps import stamp_us

# The classes that carry commands: a request (home/cmd) and the arbiter's
# forward of it (home/arbiter). put_json sends both as commands.
COMMAND_PREFIXES = ("home/cmd/", "home/arbiter/")


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
        The router's zenoh endpoint, as HOMEOSTAT_BUS gives it.
    """

    def __init__(self, unit: str, endpoint: str):
        self.unit = unit
        config = zenoh.Config()
        config.insert_json5("mode", '"client"')
        config.insert_json5("connect/endpoints", json.dumps([endpoint]))
        config.insert_json5("scouting/multicast/enabled", "false")
        config.insert_json5("scouting/gossip/enabled", "false")
        self._session = zenoh.open(config)
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

    def put_json(self, key: str, value: Any) -> None:
        """Publish a JSON-encoded value.

        A value containing a non-finite float (NaN, Infinity) is dropped
        with a "non-finite" health event instead. Python's json accepts and
        emits these, but JSON has no syntax for them. Every consumer would
        otherwise have to guard against them, and the recorder cannot store
        them (docs/design.md#bus-payload-conventions).

        Parameters
        ----------
        key : str
            The concrete key to put on. A home/cmd/ or home/arbiter/ key is
            sent as a command.
        value : Any
            A JSON-encodable value.
        """
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

    def parse_forecast(self, sample: zenoh.Sample):
        """Decode a subscribed forecast sample, or drop it with a health event.

        Returns None after a "malformed-payload" drop event, as
        `parse_command` does. What to do when `issued` is old is the
        consumer's own policy, not the SDK's (see homeostat.forecast).

        Parameters
        ----------
        sample : zenoh.Sample
            A sample received on a forecast key.

        Returns
        -------
        Forecast or None
            The decoded forecast, or None if the payload was malformed.
        """
        key = str(sample.key_expr)
        try:
            return forecast.decode(sample.payload.to_bytes())
        except ValueError as error:
            self.health_event("drop", reason="malformed-payload", key=key, detail=str(error))
            return None

    def parse_command(self, sample: zenoh.Sample):
        """Run the command prologue every adapter shares (docs/adapters.md, §4).

        The result is the aspect, the envelope's value and its correlation
        id. It is None after a drop event: "malformed-payload" for a payload
        that is not JSON, and "invalid-command" for one that is not an
        envelope.

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
        return aspect, value, cmd_id

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

    def is_live(self, sample: zenoh.Sample) -> bool:
        """Return whether a sample is a live publish rather than a replay.

        The core's router stamps a sample on arrival unless its publisher
        set a stamp. A sample with the router's stamp, or with none, is
        live: its value is true now. A sample whose publisher set the stamp
        says when its value was true, which can be long ago. The core does
        that when it replays recorded state after a restart
        (docs/design.md#replay-after-a-core-restart).

        Parameters
        ----------
        sample : zenoh.Sample
            A received sample.

        Returns
        -------
        bool
            True for a live sample.
        """
        stamp = getattr(sample, "timestamp", None)
        if stamp is None:
            return True
        if not self._router_ids:
            self._router_ids = {str(zid) for zid in self._session.info.routers_zid()}
        # With no router known, every sample is taken as live, as before
        # the core stamped samples.
        return not self._router_ids or str(stamp.get_id()) in self._router_ids

    def sample_age(self, sample: zenoh.Sample) -> float:
        """Return how old a sample's value is, in seconds.

        Zero for a live sample. For a replay, the time since the stamp its
        publisher set, and never negative.

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
        Non-JSON payloads are ignored, as a subscriber ignores them.
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
        self._session.close()
