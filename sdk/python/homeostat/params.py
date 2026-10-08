"""Live numeric adapter parameters from home/config/{unit}/*.

The step-4 read pattern: subscribe first, then seed via get. A value the
subscription already delivered wins over the seed — the served reply may
predate a write that raced startup. Adapter-side defaults let a manifest
omit any parameter; non-numeric (and non-finite) values are ignored.

Owned here once rather than copied into each adapter; adapters subclass
it with typed properties over `get`.
"""

import json
import math

from . import keys


class LiveParams:
    """Live numeric parameter values for one unit, over adapter-side defaults.

    Parameters
    ----------
    session : UnitSession
        The unit's bus session; its `unit` names the config keys followed.
    defaults : dict
        Parameter name to default value. Only these names are tracked.
    """

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
        """Return the current value of parameter `name`.

        Parameters
        ----------
        name : str
            A parameter named in `defaults`.

        Returns
        -------
        int or float
            The latest valid live value, or the default until one arrives.

        Raises
        ------
        KeyError
            If `name` is not in `defaults`.
        """
        return self._values[name]
