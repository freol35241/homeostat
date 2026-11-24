"""Live numeric adapter parameters from home/config/{unit}/*.

It subscribes first, then seeds the values with a get, as the Context
does for an automation's parameters. A value the subscription has already
delivered takes precedence over the seed, because the get reply may
predate a write that raced startup. Adapter-side defaults let a manifest
omit any parameter. Non-numeric and non-finite values are ignored, and a
payload that is not JSON is dropped with a "malformed-payload" event.

Adapters share this class and subclass it with typed properties over
`get`.
"""

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
        self._sub = session.subscribe_json(keys.config_keyexpr(session.unit), self._on_config)
        for key, value in session.get_json(keys.config_keyexpr(session.unit)):
            name = key.rsplit("/", 1)[-1]
            if name not in self._live:
                self._store(name, value)

    def _on_config(self, key: str, value, sample) -> None:
        name = key.rsplit("/", 1)[-1]
        # Only a value that is kept counts as live, so an invalid one does
        # not make the seed skip the value the core serves.
        if self._store(name, value):
            self._live.add(name)

    def _store(self, name: str, value) -> bool:
        if (
            name in self._values
            and isinstance(value, (int, float))
            and not isinstance(value, bool)
            and math.isfinite(value)
        ):
            self._values[name] = value
            return True
        return False

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
