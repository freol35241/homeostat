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

    def health_event(self, kind: str, **fields: Any) -> None:
        """Publishes a JSON event at home/health/{unit}/event."""
        self.put_json(keys.health_event_key(self.unit), {"kind": kind, **fields})

    def close(self) -> None:
        if self._token is not None:
            self._token.undeclare()
            self._token = None
        self._session.close()
