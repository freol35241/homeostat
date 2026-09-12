"""Bus session for a supervised unit.

`connect()` reads HOMEOSTAT_UNIT / HOMEOSTAT_BUS (handed down by the
supervisor), opens a client session against the supervisor's router — no
scouting, topology is explicit — and returns a UnitSession. Call `ready()`
once the unit is actually able to do its job: the liveliness token, not the
process, is what "up" means to the supervisor.
"""

import json
import os
from typing import Any, Callable

import zenoh

from . import keys


def connect() -> "UnitSession":
    unit = os.environ[keys.ENV_UNIT]
    endpoint = os.environ[keys.ENV_BUS]
    return UnitSession(unit, endpoint)


class ConfigWriteError(Exception):
    """A parameter write the core rejected (constraint, unknown key, ...)."""


class QueryError(Exception):
    """A queryable answered a get with an error reply (the recorder's
    "limit: 0 is not positive", "store unavailable: ...")."""


class UnitSession:
    def __init__(self, unit: str, endpoint: str):
        self.unit = unit
        config = zenoh.Config()
        config.insert_json5("mode", '"client"')
        config.insert_json5("connect/endpoints", json.dumps([endpoint]))
        config.insert_json5("scouting/multicast/enabled", "false")
        config.insert_json5("scouting/gossip/enabled", "false")
        self._session = zenoh.open(config)
        self._token = None

    def ready(self) -> None:
        """Declares the liveliness token at home/health/{unit}/alive."""
        self._token = self._session.liveliness().declare_token(
            keys.liveliness_key(self.unit)
        )

    def put_json(self, key: str, value: Any) -> None:
        """Publishes a JSON-encoded value."""
        self._session.put(key, json.dumps(value))

    def subscribe(self, keyexpr: str, callback: Callable[[zenoh.Sample], None]):
        return self._session.declare_subscriber(keyexpr, callback)

    def declare_queryable(self, keyexpr: str, callback: Callable[[zenoh.Query], None]):
        return self._session.declare_queryable(keyexpr, callback)

    def get_json(self, selector: str) -> list[tuple[str, Any]]:
        """Queries the bus, returning (key, decoded JSON) per ok reply."""
        return [(key, value) for key, value, _ in self.get_json_aged(selector)]

    def get_json_aged(self, selector: str) -> list[tuple[str, Any, float]]:
        """Queries the bus, returning (key, decoded JSON, age in seconds)
        per ok reply. The age is the reply's attachment as the core's
        last-value mirrors write it; a reply without one is age zero.
        Non-JSON payloads are ignored, as a subscriber ignores them."""
        values = []
        for reply in self._session.get(selector):
            sample = reply.ok
            if sample is None:
                # An error reply is the queryable saying no; swallowing it
                # would read as an empty result.
                if (err := reply.err) is not None:
                    text = err.payload.to_bytes().decode(errors="replace")
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
            values.append((str(sample.key_expr), value, age_s))
        return values

    def write_config(self, unit: str, param: str, value: Any) -> Any:
        """Writes a parameter through the core's validating config queryable.

        A GET with payload against the concrete key: the core validates the
        value against the manifest constraint, stores it, republishes it, and
        replies the stored value. A rejected write raises ConfigWriteError
        with the core's message; a plain put would bypass validation.
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
        """Publishes a JSON event at home/health/{unit}/event."""
        self.put_json(keys.health_event_key(self.unit), {"kind": kind, **fields})

    def close(self) -> None:
        if self._token is not None:
            self._token.undeclare()
            self._token = None
        self._session.close()
