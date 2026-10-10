"""Automation-side SDK: the Context (see docs/design.md#the-sdks-view-of-a-unit).

A Context gives an automation the bus access its manifest declares:

- subscriptions by binding name from [bus.subscribes];
- typed live parameters from [params.*], seeded by a bus get and kept
  current by a config subscription;
- publishing through [bus.publishes] expressions.

Publishes go to concrete keys only. A put on a `**` expression would give
adapters a wildcard key they cannot parse. Literal segments of the publish
expression are defaults, and wildcard segments must be named. A key the
declared expression does not cover is refused, so the manifest decides
what the unit may publish.

A zone in the room slot expands against the house's zones.toml.
`{room}`/`{entity}` templates expand against the entities this unit
binds. The core performs the same two expansions at plan time, so a unit
subscribes to what `plan` printed for it.
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
from .stamps import Newest


def context(root: str | Path = ".") -> "Context":
    """Return the Context for the unit the supervisor started.

    The unit's name comes from HOMEOSTAT_UNIT.

    Parameters
    ----------
    root : str or Path, optional
        The house root.

    Returns
    -------
    Context
        The automation's bus surface, as its manifest declares it.

    Raises
    ------
    KeyError
        If HOMEOSTAT_UNIT or HOMEOSTAT_BUS is unset.
    """
    return Context(os.environ[keys.ENV_UNIT], root)


# The classes addressed per entity, as home/{class}/{room}/{entity}/{aspect}.
# These are the classes with slots to fill and the ones that expand.
# Mirrors ENTITY_ADDRESSED in src/keyspace.rs. `arbiter` is left out
# because only adapters use it, and adapters use UnitSession directly
# instead of a Context. If the core adds a class and this tuple does not,
# `plan` accepts that class's templated publishes but `ctx` cannot address
# them.
_ENTITY_ADDRESSED = ("state", "cmd", "forecast")

# The recorder's store description. It is the only history selector that
# answers whenever the recorder is up, whatever the store holds.
_HISTORY_STATS = "home/history/stats"


def _dedup(exprs: Iterable[str]) -> list[str]:
    """Return `exprs` without duplicates, in their original order.

    Two entities in one room expand a `{room}`-only expression identically,
    and a duplicate subscription delivers twice.
    """
    return list(dict.fromkeys(exprs))


def _expand(
    expr: str, zones: dict[str, list[str]], entities: list[house.Entity]
) -> list[str]:
    """Expand an expression as the core does at plan time (src/expand.rs).

    `{room}`/`{entity}` templates are substituted per bound entity.
    Otherwise a zone in the room slot expands to one expression per member
    room. The two cannot both apply, since a template is not a zone name.
    Only the entity-addressed classes expand, because only they have a room
    slot to fill (src/keyspace.rs, ENTITY_ADDRESSED).
    """
    segments = expr.split("/")
    if len(segments) < 3 or segments[1] not in _ENTITY_ADDRESSED:
        return [expr]
    if "{room}" in segments or "{entity}" in segments:

        def substitute(entity: house.Entity) -> str:
            filled = {"{room}": entity.room, "{entity}": entity.name}
            return "/".join(filled.get(seg, seg) for seg in segments)

        # A cmd path to an arbitrated entity belongs to the arbiter, not to
        # the entity's owner, so a templated cmd skips arbitrated entities.
        # Automation-owned entities cannot be arbitrated
        # (`virtual-entity-arbitrated`), so this currently excludes
        # nothing. It keeps this expansion in step with the core's.
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
    """Return whether the house runs a recorder, read from the manifests.

    The recorder is the unit that declares a publish under home/history/.
    src/grants.rs allows only one service to publish there. Without a
    recorder, `restore` has nothing to wait for, and waiting out its
    timeout would delay every start in a house without one.
    """
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
    """An automation's bus surface, as its manifest declares it.

    `context()` builds one for the unit the supervisor started.

    Parameters
    ----------
    unit : str
        The automation unit's name; its manifest is units/{unit}.toml.
    root : str or Path, optional
        The house root.

    Attributes
    ----------
    unit : str
        The unit's name.
    params : object
        The current typed parameter values, one attribute per `[params.*]`
        entry; a `time` parameter reads as a `datetime.time`.

    Raises
    ------
    KeyError
        If HOMEOSTAT_BUS is unset.
    """

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
        # means parsing the whole house. Units without templates skip it.
        # Units with templates fail at startup if it goes wrong, not while
        # running.
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
        # (entity, aspect, source) -> last reported participation, so
        # `source_used` can emit on transition rather than on every call.
        self._sources_used: dict[tuple[str, str, str], bool] = {}

        if self._param_specs:
            # Subscribe, then get, then merge. The get covers everything
            # before the subscription, and the subscriber everything after.
            self._subs.append(
                self._session.subscribe(keys.config_keyexpr(unit), self._on_config)
            )
            served = dict(self._session.get_json(keys.config_keyexpr(unit)))
            with self._lock:
                for name, spec in self._param_specs.items():
                    if name in self._param_values:
                        continue  # the subscription delivered a newer value
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
        """Subscribe a `[bus.subscribes]` binding and deliver its current values.

        The handler receives (key, decoded JSON value). Non-JSON payloads
        are ignored.

        This subscribes, then gets, then merges, as for config. The current
        value of every matching key is read from the core's state mirror and
        delivered before this returns. A restarted unit therefore has its
        inputs without waiting for its sources to publish again.

        A mirrored value can be any age, and a handler cannot otherwise
        tell a catch-up from a new publish. The same holds for a value the
        core replays after a restart, which arrives as a sample stamped with
        when it was recorded. A handler declared as `(key, value, age_s)`
        receives the age in seconds, which is zero for a live sample, to
        pass to `Freshness.seen`. A two-argument handler gets the value
        without the age, as though it had just arrived.

        Values are ordered by their stamps. One older than the value
        already delivered for its key is dropped, so a catch-up or a replay
        never replaces a newer live value.

        Parameters
        ----------
        binding : str
            A `[bus.subscribes]` binding name.
        handler : Callable[..., None]
            Called as `handler(key, value)`, or as `handler(key, value,
            age_s)` when it takes three or more parameters.

        Raises
        ------
        KeyError
            If `binding` is not declared in `[bus.subscribes]`.
        QueryError
            If the catch-up get is answered with an error reply, or times
            out (QueryTimeout).
        """
        wants_age = len(inspect.signature(handler).parameters) >= 3
        newest = Newest()
        # Orders catch-up against live samples. It is a separate lock from
        # `self._lock` because the handler runs under it and may read
        # `params`.
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
                if not newest.admit(key, getattr(sample, "timestamp", None)):
                    return
            deliver(key, value, self._session.sample_age(sample))

        exprs = self._variants(self._subscribes[binding])
        for expr in exprs:
            self._subs.append(self._session.subscribe(expr, callback))
        for expr in exprs:
            for key, value, age_s, stamp in self._session.get_json_stamped(expr):
                # Under the lock, so a live sample for the same key, which
                # is newer, is delivered after the catch-up and not before.
                with order:
                    if not newest.admit(key, stamp, catch_up=True):
                        continue
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
        """Return the concrete key a `[bus.publishes]` binding addresses.

        Literal expression segments are defaults, and wildcard and template
        segments must be named. A key the declared expression does not cover
        is refused, so the manifest decides what the unit may publish.

        A forecast key has one more slot than the others: its source, which
        says who publishes this forecast (docs/design.md#forecasts).
        """
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
            defaults: dict[str, str] = dict(zip(names, segments[2 : 2 + len(names)], strict=False))
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
        """Publish through a `[bus.publishes]` expression to one concrete key.

        Literal expression segments are defaults. Wildcard segments must be
        named with room/entity/aspect. cmd-class publishes are wrapped in the
        envelope automatically. The priority comes from the manifest's
        publish declaration, and the actor is this unit.

        Parameters
        ----------
        binding : str
            A `[bus.publishes]` binding name.
        value : Any
            The value, JSON-encodable.
        room : str or None, optional
            The room slot; required where the expression's is a wildcard.
        entity : str or None, optional
            The entity slot; required where the expression's is a wildcard.
        aspect : str or None, optional
            The aspect slot; required where the expression's is a wildcard.

        Raises
        ------
        KeyError
            If `binding` is not declared in `[bus.publishes]`.
        ValueError
            If the slots do not make one concrete key inside the declared
            expression, or a cmd publish declares no priority.
        """
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
        """Publish a forecast through a `[bus.publishes]` expression to one concrete key.

        This is `publish`, for the forecast class.

        Publishing through a binding is the purpose of this method. Without
        it, an automation would have to bypass its manifest and give a key
        to the session. The declaration `plan` validated would then have no
        effect at runtime, and the manifest would no longer decide what this
        unit publishes.

        `points` are what the source said. See homeostat.forecast for their
        shape, and for why the extent and the resampling are handled
        there.

        Parameters
        ----------
        binding : str
            A `[bus.publishes]` binding name with a forecast key.
        issued : datetime.datetime
            When the forecast was issued; must carry a UTC offset.
        points : iterable of Point or tuple
            The points, as `homeostat.forecast.encode` accepts them.
        room : str or None, optional
            The room slot, as for `publish`.
        entity : str or None, optional
            The entity slot, as for `publish`.
        aspect : str or None, optional
            The aspect slot, as for `publish`.
        source : str or None, optional
            The source slot: who publishes this forecast.

        Raises
        ------
        KeyError
            If `binding` is not declared in `[bus.publishes]`.
        ValueError
            If the slots do not make one concrete key inside the declared
            expression.
        """
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
        """Read back the last value this unit published on `binding`'s key.

        It is read from the recorder as `(value, age_s)`, or None when there
        is nothing to restore.

        The core's state mirror is in memory, so a core restart empties it,
        and every version upgrade restarts the core. `subscribe`'s catch-up
        then has nothing to replay. For most units that is correct. A latch
        is different. Nothing can recompute a decision somebody made, and
        the recorder holds the only record of it.

        Restoring is not automatic, because the right behaviour differs
        between kinds of state and only the unit knows which kind it holds.
        An rf433 adapter must publish `false` at startup rather than bring
        back an expired motion event (docs/design.md#one-way-senders). A
        fusion wants its inputs recomputed. The unit therefore calls this
        itself, and gets the age with the value. A latch does not care how
        old its decision is, but a fusion does.

        It reads only a state key this unit publishes, addressed by the
        same slots as `publish`. Restoring another unit's state is not
        supported.

        It returns None, rather than raising, when the house has no
        recorder, when the series has no rows, or when the store answers
        with an error. The last case is logged as a `restore-failed` health
        event. A unit must still be able to start on its code defaults.
        Units have no start order
        (docs/design.md#restoring-a-units-own-last-value), so the recorder
        may not be answering yet, and this retries until `timeout_s`. Call
        it before `ready()`. Until then the supervisor sees the unit as not
        yet able to do its job, which is accurate.

        Parameters
        ----------
        binding : str
            A `[bus.publishes]` binding name with a state key.
        room : str or None, optional
            The room slot, as for `publish`.
        entity : str or None, optional
            The entity slot, as for `publish`.
        aspect : str or None, optional
            The aspect slot, as for `publish`.
        timeout_s : float, optional
            Seconds to keep waiting for the recorder to answer.

        Returns
        -------
        tuple of (Any, float) or None
            ``(value, age_s)``, or None when there is nothing to restore.

        Raises
        ------
        KeyError
            If `binding` is not declared in `[bus.publishes]`.
        ValueError
            If the binding is not a state key, or the slots do not make one
            concrete key inside the declared expression.
        """
        key = self._concrete_key(binding, room=room, entity=entity, aspect=aspect)
        segments = key.split("/")
        if segments[1] != "state":
            raise ValueError(f"restore {binding!r}: only state keys have a history")
        if not self._has_recorder():
            return None

        def failed(reason: str) -> None:
            self.health_event("restore-failed", key=key, reason=reason)

        # Wait for the recorder to answer, not for rows. The recorder
        # replies once per series it holds, so a series with no rows gets no
        # reply at all. Waiting on the series itself would delay every first
        # start by the whole timeout. `stats` describes the store and always
        # answers while the recorder is up.
        #
        # Send one query at a time, and let it wait for the rest of the
        # deadline. The recorder answers queries one at a time (zenoh runs a
        # queryable's callback serially). A get that gives up and asks again
        # leaves its query in the recorder's queue. Once an answer takes
        # longer than the get's timeout, every later answer arrives after
        # its get has given up, and the loop never receives one. A get that
        # no queryable serves returns at once with no replies, which means
        # the recorder is not up yet. Only that case is retried.
        deadline = time.monotonic() + timeout_s
        while True:
            remaining = max(deadline - time.monotonic(), 0.1)
            try:
                if self._session.get_json(_HISTORY_STATS, timeout_s=remaining):
                    break
            except QueryTimeout:
                failed(
                    f"recorder took the query but did not answer within {timeout_s}s"
                )
                return None
            except QueryError as err:
                failed(str(err))
                return None
            if time.monotonic() >= deadline:
                failed(f"no recorder answered within {timeout_s}s")
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
        """Publish a JSON event at home/health/{unit}/event.

        Parameters
        ----------
        kind : str
            The event's `kind`.
        **fields : Any
            Further JSON-encodable fields of the event.
        """
        self._session.health_event(kind, **fields)

    def source_used(self, entity: str, aspect: str, source: str, used: bool) -> None:
        """Report whether one declared source is currently folded into a computed value.

        See docs/design.md#which-sources-a-computation-actually-used.

        Declared sources say what may contribute. This reports what did.
        Without it, a source dropped as stale, failed on a plausibility
        check, or excluded by a house rule would still be shown as
        contributing. A dropped source is often the explanation for an odd
        value.

        It emits only on a change. Hand-written producers often get
        "emit on transition" wrong, and a stream that repeats every tick
        cannot be turned into intervals. Call it every time you decide,
        including at startup. The first call for a triple always reports,
        so a consumer that starts mid-window does not mistake silence for
        agreement.

        Parameters
        ----------
        entity : str
            The computed entity.
        aspect : str
            The computed aspect.
        source : str
            The contributor's name, as the entity's `[sources]` table keys it.
        used : bool
            Whether the source contributed to this computation.
        """
        triple = (entity, aspect, source)
        if self._sources_used.get(triple) == used:
            return
        self._sources_used[triple] = used
        self._session.health_event(
            "source-restored" if used else "source-dropped",
            entity=entity,
            aspect=aspect,
            source=source,
        )

    def ready(self) -> None:
        """Declare the liveliness token: the unit is able to do its job."""
        self._session.ready()

    def run(self) -> None:
        """Block until SIGTERM/SIGINT, then close the session."""
        stop = threading.Event()
        signal.signal(signal.SIGTERM, lambda *_: stop.set())
        signal.signal(signal.SIGINT, lambda *_: stop.set())
        stop.wait()
        self.close()

    def close(self) -> None:
        """Undeclare this context's subscriptions and close its session."""
        for sub in self._subs:
            sub.undeclare()
        self._subs.clear()
        self._session.close()
