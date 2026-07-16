"""Automation-side SDK: the Context (see docs/design.md, step 4).

A Context gives an automation exactly the surface its manifest declares:
subscriptions by binding name from [bus.subscribes], typed live parameters
from [params.*] (seeded via a bus get, updated live by a config
subscription), and publishing through [bus.publishes] expressions.

Publishes go to concrete keys only — a put on a `**` expression would hand
adapters an unparseable wildcard key. Literal segments of the publish
expression are defaults, wildcard segments must be named, and any key the
declared expression does not cover is refused: the manifest stays the
authority on intent.

Zone references in the room slot expand against the house's zones.toml,
the same expansion the core performs at plan time.
"""

import datetime
import json
import os
import signal
import threading
import tomllib
from pathlib import Path
from typing import Any, Callable

import zenoh

from . import keys
from .session import UnitSession


def context(root: str | Path = ".") -> "Context":
    return Context(os.environ[keys.ENV_UNIT], root)


def _typed(param_type: str, value: Any) -> Any:
    if param_type == "time" and isinstance(value, str):
        return datetime.time.fromisoformat(value)
    return value


class _Params:
    """Attribute access to the current typed parameter values."""

    def __init__(self, ctx: "Context"):
        object.__setattr__(self, "_ctx", ctx)

    def __getattr__(self, name: str) -> Any:
        ctx = object.__getattribute__(self, "_ctx")
        try:
            spec = ctx._param_specs[name]
        except KeyError:
            raise AttributeError(f"no parameter {name!r} in the manifest") from None
        with ctx._lock:
            return _typed(spec["type"], ctx._param_values[name])


class Context:
    def __init__(self, unit: str, root: str | Path = "."):
        root = Path(root)
        manifest = tomllib.loads((root / "units" / f"{unit}.toml").read_text())
        bus = manifest.get("bus", {})
        self._subscribes: dict[str, str] = bus.get("subscribes", {})
        self._publishes: dict[str, dict] = {
            name: {"key": spec["key"], "priority": spec.get("priority")}
            for name, spec in bus.get("publishes", {}).items()
        }
        self._param_specs: dict[str, dict] = manifest.get("params", {})
        self._zones: dict[str, list[str]] = {}
        zones_path = root / "zones.toml"
        if zones_path.exists():
            self._zones = tomllib.loads(zones_path.read_text()).get("zones", {})

        self.unit = unit
        self.params = _Params(self)
        self._lock = threading.Lock()
        self._param_values: dict[str, Any] = {}
        self._subs: list = []
        self._session = UnitSession(unit, os.environ[keys.ENV_BUS])

        if self._param_specs:
            # Subscribe, then get, merge: the get covers everything before
            # the subscription, the subscriber everything after.
            self._subs.append(
                self._session.subscribe(keys.config_keyexpr(unit), self._on_config)
            )
            served = dict(self._session.get_json(keys.config_keyexpr(unit)))
            with self._lock:
                for name, spec in self._param_specs.items():
                    self._param_values[name] = served.get(
                        keys.config_key(unit, name), spec["default"]
                    )

    def _on_config(self, sample: zenoh.Sample) -> None:
        param = str(sample.key_expr).rsplit("/", 1)[1]
        if param not in self._param_specs:
            return
        try:
            value = json.loads(sample.payload.to_bytes())
        except ValueError:
            return
        with self._lock:
            self._param_values[param] = value

    def _room_variants(self, expr: str) -> list[str]:
        """The expression, with a zone in the room slot expanded to one
        expression per member room (state/cmd keys only)."""
        segments = expr.split("/")
        if len(segments) < 3 or segments[1] not in ("state", "cmd"):
            return [expr]
        rooms = self._zones.get(segments[2])
        if rooms is None:
            return [expr]
        return ["/".join([*segments[:2], room, *segments[3:]]) for room in rooms]

    def subscribe(self, binding: str, handler: Callable[[str, Any], None]) -> None:
        """Subscribes a `[bus.subscribes]` binding; the handler receives
        (key, decoded JSON value). Non-JSON payloads are ignored."""

        def callback(sample: zenoh.Sample) -> None:
            try:
                value = json.loads(sample.payload.to_bytes())
            except ValueError:
                return
            handler(str(sample.key_expr), value)

        for expr in self._room_variants(self._subscribes[binding]):
            self._subs.append(self._session.subscribe(expr, callback))

    def publish(
        self,
        binding: str,
        value: Any,
        *,
        room: str | None = None,
        entity: str | None = None,
        aspect: str | None = None,
    ) -> None:
        """Publishes through a `[bus.publishes]` expression to one concrete
        key. Literal expression segments are defaults; wildcard segments
        must be named via room/entity/aspect. cmd-class publishes are
        wrapped in the envelope automatically (priority from the manifest's
        publish declaration, actor this unit)."""
        spec = self._publishes[binding]
        expr = spec["key"]
        segments = expr.split("/")
        if segments[1] in ("state", "cmd"):
            slots = {"room": room, "entity": entity, "aspect": aspect}
            defaults = dict(zip(("room", "entity", "aspect"), segments[2:5]))
            parts = []
            for slot, given in slots.items():
                part = given if given is not None else defaults.get(slot)
                if part is None or "*" in part or "{" in part:
                    raise ValueError(f"publish {binding!r} needs a concrete {slot}")
                parts.append(part)
            key = "/".join(["home", segments[1], *parts])
        else:
            if room or entity or aspect:
                raise ValueError(f"publish {binding!r} takes no key slots")
            key = expr
        covered = any(
            zenoh.KeyExpr(variant).includes(zenoh.KeyExpr(key))
            for variant in self._room_variants(expr)
        )
        if not covered:
            raise ValueError(f"key {key!r} is outside the declared {expr!r}")
        if segments[1] == "cmd":
            priority = spec["priority"]
            if priority is None:
                raise ValueError(
                    f"publish {binding!r} is a cmd publish with no priority "
                    "declared in the manifest"
                )
            value = keys.cmd_envelope(value, priority, self.unit)
        self._session.put_json(key, value)

    def health_event(self, kind: str, **fields: Any) -> None:
        self._session.health_event(kind, **fields)

    def ready(self) -> None:
        self._session.ready()

    def run(self) -> None:
        """Blocks until SIGTERM/SIGINT, then closes the session."""
        stop = threading.Event()
        signal.signal(signal.SIGTERM, lambda *_: stop.set())
        signal.signal(signal.SIGINT, lambda *_: stop.set())
        stop.wait()
        self.close()

    def close(self) -> None:
        for sub in self._subs:
            sub.undeclare()
        self._subs.clear()
        self._session.close()
