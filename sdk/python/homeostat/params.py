"""Live numeric adapter parameters from home/config/{unit}/*.

The step-4 read pattern: subscribe first, then seed via get. A value the
subscription already delivered wins over the seed — the served reply may
predate a write that raced startup. Adapter-side defaults let a manifest
omit any parameter; non-numeric (and non-finite) values are ignored.

Graduated from the ivt490/openwrt adapters, which carried identical
copies; adapters subclass with typed properties over `get`.
"""

import json
import math

from . import keys


class LiveParams:
    def __init__(self, session, defaults: dict):
        self._values = dict(defaults)
        self._live: set[str] = set()
        self._sub = session.subscribe(keys.config_keyexpr(session.unit), self._on_config)
        for key, value in session.get_json(keys.config_keyexpr(session.unit)):
            name = key.rsplit("/", 1)[-1]
            if name not in self._live:
                self._store(name, value)

    def _on_config(self, sample) -> None:
        try:
            value = json.loads(sample.payload.to_bytes())
        except ValueError:
            return
        name = str(sample.key_expr).rsplit("/", 1)[-1]
        self._live.add(name)
        self._store(name, value)

    def _store(self, name: str, value) -> None:
        if (
            name in self._values
            and isinstance(value, (int, float))
            and not isinstance(value, bool)
            and math.isfinite(value)
        ):
            self._values[name] = value

    def get(self, name: str):
        return self._values[name]
