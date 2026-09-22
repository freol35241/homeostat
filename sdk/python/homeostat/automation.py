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
and `{room}`/`{entity}` templates expand against the entities this unit
binds — the same two expansions the core performs at plan time, so what a
unit subscribes to is what `plan` printed for it.
"""

import datetime
import inspect
import json
import os
import signal
import threading
import time
from collections.abc import Callable, Iterable
from pathlib import Path
from typing import Any

import tomllib
import zenoh

from . import house, keys
from .session import QueryError, QueryTimeout, UnitSession


def context(root: str | Path = ".") -> "Context":
    return Context(os.environ[keys.ENV_UNIT], root)


# The classes addressed per entity — home/{class}/{room}/{entity}/{aspect}
# — so they are the ones with slots to fill and the ones that expand.
# Mirrors ENTITY_ADDRESSED in src/keyspace.rs; `arbiter` is left out
# because it is an adapter's class and adapters use UnitSession directly,
# not a Context. Adding a class in the core without adding it here is what
# made a templated forecast publish unaddressable through `ctx` while
# `plan` validated it happily.
_ENTITY_ADDRESSED = ("state", "cmd", "forecast")

# The recorder's store description: the one history selector that answers
# whether it is up, whatever the store happens to hold.
_HISTORY_STATS = "home/history/stats"
# How long one poll of the recorder waits before it is tried again.
_RECORDER_POLL_S = 2.0


def _dedup(exprs: Iterable[str]) -> list[str]:
    """Order-preserving unique. Two entities in one room expand a `{room}`-only
    expression identically, and a duplicate subscription delivers twice."""
    return list(dict.fromkeys(exprs))


def _expand(
    expr: str, zones: dict[str, list[str]], entities: list[house.Entity]
) -> list[str]:
    """The expression as the core expands it at plan time (src/expand.rs):
    `{room}`/`{entity}` templates substituted per bound entity, or a zone
    in the room slot expanded to one expression per member room. The two
    are exclusive — a template is not a zone name — and only the
    entity-addressed classes expand at all: those are the ones with a room
    slot to fill (src/keyspace.rs, ENTITY_ADDRESSED)."""
    segments = expr.split("/")
    if len(segments) < 3 or segments[1] not in _ENTITY_ADDRESSED:
        return [expr]
    if "{room}" in segments or "{entity}" in segments:

        def substitute(entity: house.Entity) -> str:
            filled = {"{room}": entity.room, "{entity}": entity.name}
            return "/".join(filled.get(seg, seg) for seg in segments)

        # A cmd path to an arbitrated entity belongs to the arbiter, not
        # to the entity's owner, so a templated cmd expands only over the
        # rest. Automation-owned entities are never arbitrated
        # (`virtual-entity-arbitrated`), so this excludes nothing today;
        # it is here because the two expansions must not drift.
        return _dedup(
            substitute(entity)
            for entity in entities
            if segments[1] != "cmd" or entity.write_mode != "arbitrated"
        )
    rooms = zones.get(segments[2])
    if rooms is None:
        return [expr]
    return ["/".join([*segments[:2], room, *segments[3:]]) for room in rooms]


def _house_has_recorder(root: str | Path) -> bool:
    """Whether the house runs a recorder at all, read from the text: it is
    the unit declaring a publish under home/history/, a class src/grants.rs
    keeps to a single service. Without one there is nothing for `restore`
    to wait for, and waiting out its timeout would stall every start in a
    recorder-less house."""
    return any(
        spec.get("key", "").startswith("home/history/")
        for unit in house.load_house(root).units
        for spec in unit.publishes.values()
    )


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
        self._root = root
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
        # Entities are only needed to expand templates, and reading them
        # means parsing the whole house. Units without templates pay
        # nothing; those with them fail at startup rather than mid-run.
        self._entities: list[house.Entity] = []
        declared = [
            *self._subscribes.values(),
            *(p["key"] for p in self._publishes.values()),
        ]
        if any("{" in expr for expr in declared):
            self._entities = [
                e for e in house.load_house(root).entities if e.owner == unit
            ]

        self.unit = unit
        self.params = _Params(self)
        self._lock = threading.Lock()
        self._param_values: dict[str, Any] = {}
        self._subs: list = []
        self._recorder: bool | None = None
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
                    if name in self._param_values:
                        continue  # the subscription already delivered fresher
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

    def _variants(self, expr: str) -> list[str]:
        return _expand(expr, self._zones, self._entities)

    def subscribe(self, binding: str, handler: Callable[..., None]) -> None:
        """Subscribes a `[bus.subscribes]` binding; the handler receives
        (key, decoded JSON value). Non-JSON payloads are ignored.

        Subscribe, then get, merge — as for config: the current value of
        every matching key is read from the core's state mirror and
        delivered before this returns, so a restarted unit is not blind
        until its sources happen to publish again. A mirrored value can be
        arbitrarily old, and a handler cannot otherwise tell a catch-up
        from a fresh publish, so a handler declared as
        `(key, value, age_s)` receives the age in seconds — zero for a live
        sample — to hand to `Freshness.seen`. A two-argument handler gets
        the catch-up without it, i.e. as though it had just arrived.
        """
        wants_age = len(inspect.signature(handler).parameters) >= 3
        delivered: set[str] = set()
        # Orders catch-up against live samples. Its own lock, not
        # `self._lock`: the handler runs under it and may read `params`.
        order = threading.Lock()

        def deliver(key: str, value: Any, age_s: float) -> None:
            if wants_age:
                handler(key, value, age_s)
            else:
                handler(key, value)

        def callback(sample: zenoh.Sample) -> None:
            try:
                value = json.loads(sample.payload.to_bytes())
            except ValueError:
                return
            key = str(sample.key_expr)
            with order:
                delivered.add(key)
            deliver(key, value, 0.0)

        exprs = self._variants(self._subscribes[binding])
        for expr in exprs:
            self._subs.append(self._session.subscribe(expr, callback))
        for expr in exprs:
            for key, value, age_s in self._session.get_json_aged(expr):
                # Under the lock so a live sample for the same key, which
                # is fresher, is delivered after the catch-up, never before.
                with order:
                    if key in delivered:
                        continue
                    delivered.add(key)
                    deliver(key, value, age_s)

    def _concrete_key(
        self,
        binding: str,
        *,
        room: str | None,
        entity: str | None,
        aspect: str | None,
        source: str | None = None,
    ) -> str:
        """The one concrete key a `[bus.publishes]` binding addresses.
        Literal expression segments are defaults, wildcard and template
        segments must be named, and a key the declared expression does not
        cover is refused: the manifest stays the authority on intent.

        A forecast key carries one slot more than the rest — its source,
        which says WHO is claiming this future (docs/design.md, Sources)."""
        expr = self._publishes[binding]["key"]
        segments = expr.split("/")
        if segments[1] in _ENTITY_ADDRESSED:
            if segments[1] == "forecast":
                names = ("room", "entity", "aspect", "source")
                slots = {
                    "room": room,
                    "entity": entity,
                    "aspect": aspect,
                    "source": source,
                }
            else:
                if source is not None:
                    raise ValueError(f"publish {binding!r} takes no source slot")
                names = ("room", "entity", "aspect")
                slots = {"room": room, "entity": entity, "aspect": aspect}
            defaults = dict(zip(names, segments[2 : 2 + len(names)]))
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
            for variant in self._variants(expr)
        )
        if not covered:
            raise ValueError(f"key {key!r} is outside the declared {expr!r}")
        return key

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
        segments = spec["key"].split("/")
        key = self._concrete_key(binding, room=room, entity=entity, aspect=aspect)
        if segments[1] == "cmd":
            priority = spec["priority"]
            if priority is None:
                raise ValueError(
                    f"publish {binding!r} is a cmd publish with no priority "
                    "declared in the manifest"
                )
            value = keys.cmd_envelope(value, priority, self.unit)
        self._session.put_json(key, value)

    def publish_forecast(
        self,
        binding: str,
        issued: datetime.datetime,
        points,
        *,
        room: str | None = None,
        entity: str | None = None,
        aspect: str | None = None,
        source: str | None = None,
    ) -> None:
        """Publishes a forecast through a `[bus.publishes]` expression to
        one concrete key — `publish`, for the forecast class.

        The binding is the point. Without this an automation has to reach
        past its own manifest and hand a key to the session, which leaves
        the declaration `plan` validated doing nothing at runtime: the
        manifest stops being the authority on what this unit publishes.

        `points` are what the source said — see homeostat.forecast for the
        shape and for why the extent and the resampling live there."""
        self._session.put_forecast(
            self._concrete_key(
                binding, room=room, entity=entity, aspect=aspect, source=source
            ),
            issued,
            points,
        )

    def _has_recorder(self) -> bool:
        if self._recorder is None:
            self._recorder = _house_has_recorder(self._root)
        return self._recorder

    def restore(
        self,
        binding: str,
        *,
        room: str | None = None,
        entity: str | None = None,
        aspect: str | None = None,
        timeout_s: float = 30.0,
    ) -> tuple[Any, float] | None:
        """The last value this unit published on `binding`'s key, read back
        from the recorder as `(value, age_s)` — or None when there is
        nothing to restore.

        The core's state mirror is in-memory, so a core restart (every
        version upgrade is one) empties it and `subscribe`'s catch-up has
        nothing to replay. For most units that is correct. For a latch it
        is not: nothing can recompute a decision somebody made, and the
        only record that it was made is the recorder's.

        Never automatic, because the right behaviour is not the same for
        all state and only the unit knows which kind it holds — an rf433
        adapter must publish `false` at startup rather than resurrect an
        expired motion event (docs/design.md, One-way senders), while a
        fusion wants its inputs recomputed. So this is a call the unit
        makes, and the age comes with the value: a latch does not care how
        old its decision is, and a fusion very much does.

        Reads only a state key this unit itself publishes, addressed by the
        same slots as `publish` — a unit restoring somebody else's state is
        a different and worse thing.

        Returns None, rather than raising, when the house has no recorder,
        when the series has no rows, or when the store answers an error
        (logged as a `restore-failed` health event): a unit must still be
        able to start on its code defaults. Because there is no start order
        between units (docs/design.md, History / recorder) the recorder may not be
        answering yet, so this retries until `timeout_s` — call it before
        `ready()`, where a unit that is not yet able to do its job is
        exactly what the supervisor should see.
        """
        key = self._concrete_key(binding, room=room, entity=entity, aspect=aspect)
        segments = key.split("/")
        if segments[1] != "state":
            raise ValueError(f"restore {binding!r}: only state keys have a history")
        if not self._has_recorder():
            return None

        def failed(reason: str) -> None:
            self.health_event("restore-failed", key=key, reason=reason)

        # Wait for the recorder to answer, not for rows: a series with no
        # rows yet is not answered at all (the recorder replies per series
        # it holds), so waiting on the series itself would stall every
        # first start for the whole timeout. `stats` describes the store
        # and always answers while the recorder is up.
        #
        # A get that times out is the recorder not answering YET -- the
        # very case this loop exists for -- so it keeps waiting; only a
        # recorder answering with an error gives up. Each get is kept short
        # so the loop, not zenoh's 10 s default, decides how long to wait.
        deadline = time.monotonic() + timeout_s
        while True:
            try:
                if self._session.get_json(_HISTORY_STATS, timeout_s=_RECORDER_POLL_S):
                    break
            except QueryTimeout:
                pass
            except QueryError as err:
                failed(str(err))
                return None
            if time.monotonic() >= deadline:
                failed(f"recorder did not answer within {timeout_s}s")
                return None
            time.sleep(0.2)

        selector = f"{keys.history_key('state', segments[3], segments[4])}?limit=1"
        try:
            replies = self._session.get_json(selector)
        except QueryError as err:
            failed(str(err))
            return None
        for _, rows in replies:
            if rows:
                row = rows[-1]
                stamped = datetime.datetime.fromisoformat(row["ts"])
                age_s = (
                    datetime.datetime.now(datetime.timezone.utc) - stamped
                ).total_seconds()
                return row["value"], max(age_s, 0.0)
        return None

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
