# /// script
# requires-python = ">=3.11"
# dependencies = [
#     "homeostat",
# ]
#
# [tool.uv.sources]
# homeostat = { path = "../sdk/python", editable = true }
# ///
"""Arbiter service: resolves who drives each arbitrated aspect.

See docs/design.md#arbitrated-mode. Plan-time expansion leaves an
adapter's arbitrated entities out of its home/cmd subscription, so this
service subscribes home/cmd/** and decides which wishes go through. A
wish for a non-arbitrated entity is ignored, since its own adapter
consumes it.

Which entities are arbitrated comes from the grant table the core serves
at home/meta/system/grants (`Bindings`), read live. An apply that flips an
entity's write mode restarts the entity's adapter but not this service,
whose own files are unchanged, so a set read from the repo at startup
would go stale. The core advances that table only when an apply
completes, while the adapter restarts partway through the walk, so the
service also follows home/meta/system/grants_pending, the tables of the
applies started since the last one that completed, and routes on both.

Two kinds of write reach an aspect (docs/design.md#holds). A hold is a
standing wish: a unit publishes it at home/hold/{unit}/{room}/{entity}/
{aspect}, and its hold grant gives its band. The family's hold is this
service's own, placed by a manual-band command and published under its
name. A command is a one-off push on home/cmd. `Resolver` keeps both and
the hold it last applied per aspect, and forwards to home/arbiter/... when
the hold that wins changes (docs/design.md#which-hold-wins).

A manual-band command may carry a `hold` intent (docs/design.md#hold-intents):
"none" forwards without holding, an RFC3339 instant holds until then,
"report" holds without forwarding because the device already did it, and
"release" hands the aspect back without forwarding anything.

The family's holds are published at home/hold/{unit}/{room}/{entity}/
{aspect} and the outcome per aspect at home/arbitration/{unit}, both
mirrored by the core. A restart takes the family's holds and the applied
holds back from the mirror, so it sends nothing that was already sent.

Configuration: the live parameter hold_minutes (family-editable), seeded
from the manifest default.

It also follows each unit's health, so a gone holder's holds end as its
`on_loss` says, and its adapters' drops and the devices' `available`, so
a hold an adapter could not deliver is sent again when the device is
back.

Health events at home/health/{unit}/event: `drop` (off-schema-key,
invalid-command, unbound, malformed-hold), `refuse` when a command loses
to a hold, `preempt` when a higher hold takes over, and `resume` when a
lower one takes over after it.
"""

import datetime
import json
import os
import secrets
import signal
import threading
import time
from functools import partial

import homeostat
import zenoh
from homeostat import house, keys
from homeostat.params import LiveParams

PARAM = "hold_minutes"
GRANTS_KEY = "home/meta/system/grants"
PENDING_KEY = "home/meta/system/grants_pending"
RANKS = keys.CMD_PRIORITIES


class Params(LiveParams):
    """hold_minutes from home/config/{unit}/*, live.

    LiveParams (subscribe-then-get, seeded from the manifest default) and
    not automation.Context, because Context.publish only knows the
    state/cmd key-slot shape, and this service forwards to a different
    home/arbiter key for each wish.
    """

    @property
    def hold_minutes(self) -> float:
        return self.get(PARAM)


def bindings(grants: list) -> tuple[frozenset, frozenset]:
    """Return the (room, entity) pairs that are arbitrated, and all bound.

    Every bound entity sits in its owner's binding row of the grant table,
    and each row entity carries its write mode.
    """
    arbitrated, bound = set(), set()
    for grant in grants:
        for e in grant["entities"]:
            pair = (e["room"], e["name"])
            bound.add(pair)
            if e["write"] == "arbitrated":
                arbitrated.add(pair)
    return frozenset(arbitrated), frozenset(bound)


def holders(grants: list) -> dict:
    """Return each held (room, entity)'s hold rows and its capability.

    The value maps (room, entity) to (capability, [(unit, band, [key
    expressions], on_loss)]), from the grant table's hold rows (`"hold":
    true`).
    """
    out: dict = {}
    for grant in grants:
        if not grant.get("hold"):
            continue
        exprs = [zenoh.KeyExpr(k) for k in grant.get("keys", [])]
        for e in grant["entities"]:
            pair = (e["room"], e["name"])
            _, rows = out.setdefault(pair, (e.get("capability"), []))
            rows.append((grant["unit"], grant["priority"], exprs, grant.get("on_loss")))
    return out


class Bindings:
    """The arbitrated and bound entities and the hold grants, live from the grant tables.

    The core puts the applied table when an apply completes, and the pending
    table when one starts and again when it completes. An entity counts as
    arbitrated, or as bound, if either table says so. That is safe in both
    directions while the walk restarts units one at a time:

    - An entity becoming arbitrated. Before its adapter restarts, the
      adapter takes the wish from home/cmd and does not hear the forward.
      After the restart, it hears only the forward. Either way the command
      runs once.
    - An entity leaving arbitration. Before the restart, the adapter hears
      only the forward. After it, the adapter takes the wish from home/cmd,
      and the forward reaches no one.

    While the adapter takes wishes from home/cmd, in either case, a refusal
    is reported but does not stop the command. After a halted walk that
    lasts until an apply completes, for an adapter the walk did not reach.

    A hold grant counts from either table too, so a holder restarted by the
    walk keeps its holds counted.

    This follows the LiveParams read pattern for each key: subscribe, then
    seed with a get, and a table the subscription has already delivered
    wins over the seed.

    `on_change` is called with the new arbitrated set on each live table,
    under the lock, so two tables arriving together cannot apply their sets
    in the wrong order. The seed calls it once, after both reads, so holds
    restored from the mirror are checked against the tables.
    """

    def __init__(self, session, on_change):
        self._tables: dict[str, list] = {GRANTS_KEY: [], PENDING_KEY: []}
        self._live: set[str] = set()
        # Everything read from one pair of tables, in one attribute,
        # replaced in one assignment, so a reader never pairs sets from
        # different tables.
        self._sets: tuple = (frozenset(), frozenset(), {})
        self._on_change = on_change
        self._lock = threading.Lock()
        self._subs = [
            session.subscribe(key, partial(self._on_table, key)) for key in (GRANTS_KEY, PENDING_KEY)
        ]
        seeded = False
        for key in (GRANTS_KEY, PENDING_KEY):
            for _key, value in session.get_json(key):
                with self._lock:
                    if key not in self._live:
                        self._tables[key] = value
                        self._sets = self._read()
                        seeded = True
        if seeded:
            with self._lock:
                self._on_change(self._sets[0])

    def _read(self) -> tuple:
        tables = self._tables[GRANTS_KEY] + self._tables[PENDING_KEY]
        return (*bindings(tables), holders(tables))

    def _on_table(self, key: str, sample) -> None:
        with self._lock:
            self._live.add(key)
            self._tables[key] = json.loads(sample.payload.to_bytes())
            self._sets = self._read()
            self._on_change(self._sets[0])

    def route(self, target: tuple[str, str]) -> str:
        """Return "arbitrated", "bound" or "unbound" for a (room, entity)."""
        arbitrated, bound, _ = self._sets
        if target in arbitrated:
            return "arbitrated"
        return "bound" if target in bound else "unbound"

    def grant_of(self, unit: str, target: tuple[str, str, str]) -> tuple[str, str | None] | None:
        """Return the band and `on_loss` `unit` may hold `target` with, or None without a grant."""
        room, entity, aspect = target
        _, rows = self._sets[2].get((room, entity), (None, []))
        key = zenoh.KeyExpr(keys.hold_key(unit, room, entity, aspect))
        for holder, band, exprs, on_loss in rows:
            if holder == unit and any(expr.includes(key) for expr in exprs):
                return band, on_loss
        return None

    def rest_of(self, target: tuple[str, str, str]):
        """Return the value `target` is safe at, or None (keys.REST_VALUES)."""
        room, entity, aspect = target
        capability, _ = self._sets[2].get((room, entity), (None, []))
        rest = keys.REST_VALUES.get(capability)
        return rest[1] if rest is not None and rest[0] == aspect else None


def iso(epoch: float) -> str:
    """Format an epoch second as RFC3339 UTC.

    That is the spelling every other timestamp on this bus uses.
    """
    return (
        datetime.datetime.fromtimestamp(epoch, datetime.timezone.utc)
        .isoformat(timespec="seconds")
        .replace("+00:00", "Z")
    )


class Resolver:
    """Per arbitrated (room, entity, aspect): the holds, and the one applied.

    Two kinds of hold stand on an aspect. The family's is placed by a
    manual-band command and ranks above every unit's. A unit's is published
    under its name and ranks by the band of its hold grant. The hold that
    wins is the highest ranked. At one rank the one for the aspect's rest
    value wins, and otherwise the newest (docs/design.md#which-hold-wins).

    `applied` records, per aspect, the id of the hold the device was last
    given. When the winner's id differs, the winner is forwarded. A
    command records the hold that won when it was sent as applied, so the
    hold beneath a command is not sent again until it changes.

    Enforcement uses time.monotonic(), which a clock step cannot shorten or
    stretch. The family hold's published `since` and `until` are
    wall-clock, fixed when it is placed, because another process, and this
    one after a restart, can only use a wall-clock deadline.

    A holder that is gone, its breaker open, keeps the holds its `on_loss`
    keeps: by default those for the rest value
    (docs/design.md#when-a-holder-stops). Otherwise its holds count.

    A hold forwarded to a device its adapter found unavailable is sent
    again when the device's `available` returns. A device changed by hand
    while it was away is not changed back. The applied entry carries
    `undelivered` meanwhile, so an arbiter restart keeps it.

    Pure bookkeeping: no bus, no threads. Each change appends what it
    causes to `effects`, as ("forward", target, envelope) or ("event",
    kind, fields), for the caller to carry out. `grant_of(unit, target)`
    and `rest_of(target)` come from the grant table; `clock` and `wall`
    are time.monotonic and time.time in the unit and fakes in tests.
    """

    def __init__(self, grant_of, rest_of, clock=time.monotonic, wall=time.time):
        self._grant_of = grant_of
        self._rest_of = rest_of
        self._clock = clock
        self._wall = wall
        self._family: dict[tuple, dict] = {}
        self._holds: dict[tuple, dict[str, dict]] = {}
        self._applied: dict[tuple, dict] = {}
        self._winner: dict[tuple, dict] = {}
        self._refused: dict[tuple, int] = {}
        self._gone: set[str] = set()
        self._undelivered: set[tuple] = set()

    # ---- what stands on an aspect

    def candidates(self, target, *, without: str | None = None) -> list[dict]:
        """Return the holds in force on `target`, the winner first.

        `without` leaves out one unit's hold, as a command from that unit
        does not lose to its own hold.
        """
        now = self._clock()
        rest = self._rest_of(target)

        def is_rest(value) -> bool:
            # By type too, so a hold of 0 is not the rest value False.
            return rest is not None and type(value) is type(rest) and value == rest

        out = []
        family = self._family.get(target)
        if family is not None and now < family["deadline"]:
            out.append({
                "id": family["id"], "unit": family["actor"], "priority": "manual",
                "value": family["value"], "seen": family["seen"], "family": True,
                "until": family["until"],
            })
        for unit, hold in self._holds.get(target, {}).items():
            if unit == without:
                continue
            grant = self._grant_of(unit, target)
            if grant is None or (hold["deadline"] is not None and hold["deadline"] <= now):
                continue
            band, on_loss = grant
            if unit in self._gone and not (on_loss == "keep" or (on_loss is None and is_rest(hold["value"]))):
                continue
            candidate = {
                "id": hold["id"], "unit": unit, "priority": band, "value": hold["value"],
                "seen": hold["seen"], "family": False,
            }
            for field in ("until", "reason"):
                if hold.get(field) is not None:
                    candidate[field] = hold[field]
            out.append(candidate)
        out.sort(key=lambda c: (RANKS.index(c["priority"]), is_rest(c["value"]), c["seen"]), reverse=True)
        return out

    def winner(self, target, *, without: str | None = None) -> dict | None:
        """Return the hold that wins `target`, or None with none in force."""
        held = self.candidates(target, without=without)
        return held[0] if held else None

    def targets(self) -> set:
        """Every aspect something stands on or was applied to."""
        return set(self._family) | set(self._holds) | set(self._applied)

    # ---- resolution

    def resolve(self, target, effects: list) -> None:
        """Forward the winner of `target` if it is not the hold last applied."""
        won = self.winner(target)
        self._note_winner(target, won, effects)
        if won is None:
            return
        applied = self._applied.get(target)
        if applied is not None and applied.get("id") == won["id"]:
            return
        self._applied[target] = {
            "id": won["id"], "unit": won["unit"], "priority": won["priority"], "value": won["value"],
        }
        effects.append((
            "forward", target,
            {"value": won["value"], "priority": won["priority"], "actor": won["unit"], "id": won["id"]},
        ))

    def resolve_all(self, effects: list) -> None:
        for target in sorted(self.targets()):
            self.resolve(target, effects)

    def _note_winner(self, target, won: dict | None, effects: list) -> None:
        """Record the hold in force, and say when another band took over."""
        before = self._winner.get(target)
        if won is None:
            self._winner.pop(target, None)
            return
        self._winner[target] = won
        if before is None or before["id"] == won["id"]:
            return
        rank_before, rank_now = RANKS.index(before["priority"]), RANKS.index(won["priority"])
        if rank_now == rank_before:
            return
        room, entity, aspect = target
        effects.append((
            "event", "preempt" if rank_now > rank_before else "resume",
            {
                "room": room, "entity": entity, "aspect": aspect,
                "from_priority": before["priority"], "from_actor": before["unit"],
                "to_priority": won["priority"], "to_actor": won["unit"],
            },
        ))

    # ---- commands

    def command(self, target, priority, actor, hold_s, effects, *, value=None, hold=None, cmd_id=None) -> bool:
        """Arbitrate one command, and return whether its envelope is forwarded.

        A manual-band command places, clears or reports the family's hold
        as its `hold` intent says (docs/design.md#hold-intents). A
        lower-band command first releases its sender's own hold. It is
        refused when a hold above its band is in force, or one at its band
        holds another value. A forwarded command holds nothing.
        """
        if priority == "manual":
            return self._family_command(target, actor, hold_s, effects, value, hold, cmd_id)
        # A command from a unit first releases that unit's own hold. It goes
        # here, with the command, and not when the SDK's delete of the key
        # arrives: that comes on another subscriber, in either order, and
        # resolving it on its own would send the hold beneath first or undo
        # the command after.
        held = self._holds.get(target)
        if held is not None and held.pop(actor, None) is not None and not held:
            del self._holds[target]
        won = self.winner(target, without=actor)
        if won is not None and (
            RANKS.index(won["priority"]) > RANKS.index(priority)
            or (won["priority"] == priority and won["value"] != value)
        ):
            self._refused[target] = self._refused.get(target, 0) + 1
            if won["family"]:
                self._family[target]["refused"] += 1
            room, entity, aspect = target
            effects.append((
                "event", "refuse",
                {
                    "room": room, "entity": entity, "aspect": aspect, "priority": priority,
                    "actor": actor, "cmd_id": cmd_id,
                    "holder_priority": won["priority"], "holder_actor": won["unit"],
                },
            ))
            return False
        self._applied_by_command(target, priority, actor, value, cmd_id)
        return True

    def _applied_by_command(self, target, priority, actor, value, cmd_id) -> None:
        # The hold in force counts as applied, so it is sent again only once
        # it changes.
        won = self.winner(target)
        self._applied[target] = {
            "id": won["id"] if won else None, "command": cmd_id, "unit": actor,
            "priority": priority, "value": value,
        }

    def _family_command(self, target, actor, hold_s, effects, value, hold, cmd_id) -> bool:
        seconds = hold_s
        instant = hold not in (None, "none", "report", "release")
        if instant:
            seconds = keys.hold_instant(hold).timestamp() - self._wall()
        if hold in ("none", "release") or (instant and seconds <= 0):
            self._family.pop(target, None)
            if hold == "release":
                # Nothing is sent for the release itself. The hold beneath,
                # if any, is sent now.
                self.resolve(target, effects)
                return False
            self._applied_by_command(target, "manual", actor, value, cmd_id)
            # The hold beneath stands again but is not sent, so no event
            # says it took over.
            won = self.winner(target)
            if won is None:
                self._winner.pop(target, None)
            else:
                self._winner[target] = won
            return True
        wall = self._wall()
        family = {
            "id": cmd_id or secrets.token_hex(4),
            "actor": actor,
            "value": value,
            "deadline": self._clock() + seconds,
            "seen": wall,
            "since": iso(wall),
            "until": iso(wall + seconds),
            # A new family hold starts its count again, because the count
            # belongs to the hold in force.
            "refused": 0,
        }
        self._family[target] = family
        self._applied[target] = {"id": family["id"], "unit": actor, "priority": "manual", "value": value}
        self._note_winner(target, self.winner(target), effects)
        # A report says the device already did it, so nothing is sent.
        return hold != "report"

    # ---- units' holds

    def put_hold(self, target, unit, record, seen: float, effects: list) -> bool:
        """Take one unit's hold on `target` and resolve; False if it does not read as one.

        A record that does not read as a hold drops the unit's hold, as the
        mirror now carries that record and not the hold.
        """
        until = None
        if isinstance(record, dict) and record.get("until") is not None:
            try:
                until = datetime.datetime.fromisoformat(record["until"])
            except (TypeError, ValueError):
                until = False
        if (
            not isinstance(record, dict)
            or "value" not in record
            or not isinstance(record.get("id"), str)
            or until is False
            or (until is not None and until.tzinfo is None)
        ):
            self.drop_hold(target, unit, effects)
            return False
        deadline = None
        if until is not None:
            deadline = self._clock() + (until.timestamp() - self._wall())
        self._holds.setdefault(target, {})[unit] = {
            "value": record["value"], "id": record["id"], "until": record.get("until"),
            "reason": record.get("reason"), "deadline": deadline, "seen": seen,
        }
        self.resolve(target, effects)
        return True

    def drop_hold(self, target, unit, effects: list) -> None:
        """Forget one unit's hold on `target`, released or deleted, and resolve."""
        held = self._holds.get(target)
        if held is None or held.pop(unit, None) is None:
            return
        if not held:
            del self._holds[target]
        self.resolve(target, effects)

    # ---- holders and devices

    def holder(self, unit: str, gone: bool, effects: list) -> None:
        """Note whether `unit` is gone, and resolve what it holds if that changed."""
        if (unit in self._gone) == gone:
            return
        if gone:
            self._gone.add(unit)
        else:
            self._gone.discard(unit)
        for target in sorted(t for t, held in self._holds.items() if unit in held):
            self.resolve(target, effects)

    def undelivered(self, cmd_id: str) -> bool:
        """Note that the adapter dropped the forward `cmd_id` with the device away.

        Returns whether a hold's forward was named.
        """
        named = False
        for target, applied in self._applied.items():
            if applied.get("id") == cmd_id and "command" not in applied:
                applied["undelivered"] = True
                self._undelivered.add(target)
                named = True
        return named

    def available(self, room: str, entity: str, effects: list) -> None:
        """Send again what an adapter could not deliver while the device was away."""
        for target in sorted(t for t in self._undelivered if t[:2] == (room, entity)):
            self._undelivered.discard(target)
            won = self.winner(target)
            applied = self._applied.get(target)
            if applied is not None:
                applied.pop("undelivered", None)
            if won is None or applied is None or won["id"] != applied.get("id"):
                continue
            effects.append((
                "forward", target,
                {"value": won["value"], "priority": won["priority"], "actor": won["unit"], "id": won["id"]},
            ))

    # ---- time and the grant table

    def expire(self, effects: list) -> None:
        """Drop the holds whose end has passed, and resolve what they stood on."""
        now = self._clock()
        changed = {t for t, f in self._family.items() if f["deadline"] <= now}
        for target in changed:
            del self._family[target]
        for target, held in list(self._holds.items()):
            for unit in [u for u, h in held.items() if h["deadline"] is not None and h["deadline"] <= now]:
                del held[unit]
                changed.add(target)
            if not held:
                del self._holds[target]
        for target in sorted(changed):
            self.resolve(target, effects)

    def next_deadline(self) -> float | None:
        """Return the earliest end of a hold in force, or None."""
        ends = [f["deadline"] for f in self._family.values()]
        ends += [h["deadline"] for held in self._holds.values() for h in held.values() if h["deadline"] is not None]
        return min(ends, default=None)

    def retain(self, arbitrated: frozenset) -> bool:
        """Forget everything on entities outside `arbitrated`; True when something went.

        An entity that stops being arbitrated is commanded by its adapter
        directly, so a hold on it no longer holds anything.
        """
        gone = [t for t in self.targets() if t[:2] not in arbitrated]
        for target in gone:
            for table in (self._family, self._holds, self._applied, self._winner, self._refused):
                table.pop(target, None)
            self._undelivered.discard(target)
        return bool(gone)

    # ---- what is published

    def family_records(self) -> dict:
        """Return the family's hold record per target, for home/hold/{unit}/...

        A record answers "is this aspect held by the family right now?".
        The preempt and refuse events cannot: they are an audit trail, and
        a consumer that joins during a hold has missed them.
        """
        now = self._clock()
        out = {}
        for (room, entity, aspect), family in self._family.items():
            if family["deadline"] <= now:
                continue
            out[(room, entity, aspect)] = {
                "schema": 1,
                "room": room,
                "entity": entity,
                "aspect": aspect,
                "priority": "manual",
                "actor": family["actor"],
                # What the family asked for, or what the device reported.
                "value": family["value"],
                "id": family["id"],
                "since": family["since"],
                # Enforcement uses the monotonic deadline; a countdown is
                # drawn from this.
                "until": family["until"],
                # How many commands this hold has refused.
                "refused": family["refused"],
            }
        return out

    def document(self) -> dict:
        """Return the arbitration document: per aspect, the applied hold and what stands.

        `holds` lists the holds in force, the winner first.
        """
        aspects = []
        for target in sorted(self.targets()):
            room, entity, aspect = target
            holds = [{k: v for k, v in c.items() if k != "seen"} for c in self.candidates(target)]
            aspects.append({
                "room": room, "entity": entity, "aspect": aspect,
                "applied": self._applied.get(target),
                "holds": holds,
                "refused": self._refused.get(target, 0),
            })
        return {"schema": 1, "aspects": aspects}

    def restore(self, family_records: list, document) -> None:
        """Take back what a previous run published and the mirror kept.

        A family hold whose `until` has passed, or a record that does not
        read as one, is skipped. The applied holds come from the
        arbitration document, so a winner that was already sent is not
        sent again.
        """
        now, wall = self._clock(), self._wall()
        for record in family_records:
            try:
                target = (record["room"], record["entity"], record["aspect"])
                until = datetime.datetime.fromisoformat(record["until"]).timestamp()
                family = {
                    "id": str(record["id"]),
                    "actor": str(record["actor"]),
                    "value": record.get("value"),
                    "deadline": now + (until - wall),
                    "seen": datetime.datetime.fromisoformat(record["since"]).timestamp(),
                    "since": str(record["since"]),
                    "until": record["until"],
                    "refused": int(record.get("refused", 0)),
                }
            except (TypeError, KeyError, ValueError):
                continue
            if until > wall:
                self._family[target] = family
        entries = document.get("aspects") if isinstance(document, dict) else None
        for entry in entries if isinstance(entries, list) else []:
            try:
                target = (entry["room"], entry["entity"], entry["aspect"])
                applied = entry["applied"]
            except (TypeError, KeyError):
                continue
            if isinstance(applied, dict) and "id" in applied:
                self._applied[target] = applied
                if applied.get("undelivered"):
                    self._undelivered.add(target)
            if isinstance(entry.get("refused"), int) and entry["refused"]:
                self._refused[target] = entry["refused"]


def main():
    unit = os.environ[keys.ENV_UNIT]
    own = next(u for u in house.load_house(".").units if u.name == unit)

    session = homeostat.connect()
    params = Params(session, {PARAM: float(own.params[PARAM]["default"])})
    own_prefix = f"home/hold/{unit}/"
    document_key = f"home/arbitration/{unit}"
    # Guards the resolver and what was published, and wakes the expiry
    # thread when the earliest deadline moves so it does not sleep past a
    # hold's end.
    changed = threading.Condition(threading.Lock())
    # Filled once the grant table is read. Until then a unit's hold is
    # stored but not resolved, since its grant is not known.
    routes_box: list = []
    resolver = Resolver(
        grant_of=lambda u, t: routes_box[0].grant_of(u, t) if routes_box else None,
        rest_of=lambda t: routes_box[0].rest_of(t) if routes_box else None,
    )
    # hold key -> the family record last put there, and the document.
    published: dict[str, dict] = {}
    published_doc: list = [None]

    def carry_out_locked(effects: list) -> None:
        for effect in effects:
            if effect[0] == "forward":
                _, (room, entity, aspect), envelope = effect
                session.put_json(keys.arbiter_key(room, entity, aspect), envelope)
            else:
                _, kind, fields = effect
                session.health_event(kind, **fields)
        # One key per family hold. A record that changed is put again, and
        # the key of a hold that ended is deleted, so the mirror and every
        # subscriber forget it.
        current = {
            keys.hold_key(unit, *target): record
            for target, record in resolver.family_records().items()
        }
        for key in published.keys() - current.keys():
            session.delete(key)
        for key, record in current.items():
            if published.get(key) != record:
                session.put_json(key, record)
        published.clear()
        published.update(current)
        document = resolver.document()
        if document != published_doc[0]:
            session.put_json(document_key, document)
            published_doc[0] = document
        changed.notify_all()

    def hold_target(key: str):
        parts = key.split("/")
        if len(parts) != 6 or parts[1] != "hold":
            return None
        return parts[2], (parts[3], parts[4], parts[5])

    def on_hold(sample) -> None:
        key = str(sample.key_expr)
        if key.startswith(own_prefix):
            return  # the family's holds are this service's own
        parsed = hold_target(key)
        if parsed is None:
            return
        holder, target = parsed
        effects: list = []
        with changed:
            if sample.kind == zenoh.SampleKind.DELETE:
                resolver.drop_hold(target, holder, effects)
            else:
                try:
                    record = json.loads(sample.payload.to_bytes())
                except ValueError:
                    record = None
                seen = time.time() - session.sample_age(sample)
                if not resolver.put_hold(target, holder, record, seen, effects):
                    session.health_event("drop", reason="malformed-hold", key=key)
            if routes_box:
                carry_out_locked(effects)

    hold_sub = session.subscribe("home/hold/**", on_hold)

    # A holder is gone once its breaker opens. The supervisor also reports
    # `stopped` for a unit an apply restarts and for every unit at a
    # shutdown, so `stopped` is not gone: a holder briefly away must not
    # move the device. A holder an apply removes loses its grant instead.
    gone_statuses = ("open",)

    def on_health(sample) -> None:
        holder = str(sample.key_expr).split("/")[2]
        try:
            status = json.loads(sample.payload.to_bytes()).get("status")
        except (ValueError, AttributeError):
            return
        effects: list = []
        with changed:
            resolver.holder(holder, status in gone_statuses, effects)
            if routes_box:
                carry_out_locked(effects)

    def on_event(sample) -> None:
        # An adapter's drop of a forward it could not deliver names the
        # forward's id (docs/design.md#when-a-holder-stops).
        try:
            event = json.loads(sample.payload.to_bytes())
        except ValueError:
            return
        if (
            isinstance(event, dict)
            and event.get("kind") == "drop"
            and event.get("reason") == "device-unavailable"
            and isinstance(event.get("cmd_id"), str)
        ):
            with changed:
                if resolver.undelivered(event["cmd_id"]) and routes_box:
                    carry_out_locked([])

    def on_available(sample) -> None:
        parts = str(sample.key_expr).split("/")
        try:
            back = json.loads(sample.payload.to_bytes()) is True
        except ValueError:
            return
        if not back or len(parts) != 5:
            return
        effects: list = []
        with changed:
            resolver.available(parts[2], parts[3], effects)
            if routes_box:
                carry_out_locked(effects)

    health_subs = [
        session.subscribe("home/health/*", on_health),
        session.subscribe("home/health/*/event", on_event),
        session.subscribe("home/state/*/*/available", on_available),
    ]

    # The family's holds, the units' holds and the applied holds outlive
    # this process: the mirror kept what was published. This runs before
    # Bindings, whose seed drops what stands on an entity no longer
    # arbitrated. A failed read is not taken as "nothing held": a holder
    # does not publish an unchanged hold again, so the unit fails and the
    # supervisor starts it again.
    def mirrored(selector: str) -> list:
        return session.get_json_aged(selector, timeout_s=5)

    kept = mirrored(keys.hold_keyexpr(unit))
    documents = [value for _key, value, _age in mirrored(document_key)]
    with changed:
        published.update((key, record) for key, record, _age in kept)
        resolver.restore([record for _key, record, _age in kept], documents[0] if documents else None)
        published_doc[0] = documents[0] if documents else None
        for key, record, age in mirrored("home/hold/**"):
            parsed = hold_target(key)
            if parsed is None or key.startswith(own_prefix):
                continue
            holder, target = parsed
            resolver.put_hold(target, holder, record, time.time() - age, [])
        for key, health, _age in mirrored("home/health/*"):
            if isinstance(health, dict):
                resolver.holder(key.split("/")[2], health.get("status") in gone_statuses, [])

    def on_arbitrated(arbitrated: frozenset) -> None:
        effects: list = []
        with changed:
            resolver.retain(arbitrated)
            if routes_box:
                # A changed grant can change which holds count.
                resolver.resolve_all(effects)
                carry_out_locked(effects)

    routes = Bindings(session, on_arbitrated)
    with changed:
        routes_box.append(routes)
        effects: list = []
        resolver.resolve_all(effects)
        carry_out_locked(effects)

    def expiry_loop(stop: threading.Event) -> None:
        """One thread for every hold with an end, waiting on the earliest.

        Expiry is otherwise lazy, evaluated on the next resolution of that
        aspect. A hold beneath one that ended would then wait for it, and
        the published record would go on showing a hold that had ended.
        """
        while not stop.is_set():
            with changed:
                effects: list = []
                resolver.expire(effects)
                carry_out_locked(effects)
                deadline = resolver.next_deadline()
                # Capped, because a wait beyond threading.TIMEOUT_MAX raises
                # and would end this thread. A long hold costs a wake-up an
                # hour.
                timeout = None if deadline is None else min(3600.0, max(0.05, deadline - time.monotonic()))
                changed.wait(timeout=timeout)

    def cmd_handler(sample):
        key = str(sample.key_expr)
        parts = key.split("/", 4)
        if len(parts) < 5:
            session.health_event("drop", reason="off-schema-key", key=key)
            return
        room, entity, aspect = parts[2:5]
        route = routes.route((room, entity))
        if route == "bound":
            return  # not arbitrated: its own adapter consumes this wish
        if route == "unbound":
            # No unit binds the entity, so no adapter consumes the wish
            # either. Without this event it would vanish without a trace.
            try:
                cmd_id = keys.cmd_envelope_id(json.loads(sample.payload.to_bytes()))
            except ValueError:
                cmd_id = None
            session.health_event("drop", reason="unbound", key=key, cmd_id=cmd_id)
            return

        envelope = None
        try:
            envelope = json.loads(sample.payload.to_bytes())
            keys.parse_cmd_envelope(envelope)
            priority, actor = envelope["priority"], envelope["actor"]
            if not isinstance(actor, str):
                raise ValueError("cmd envelope actor is not a string")
        except (ValueError, KeyError):
            # A payload that never parsed has no id; one that parsed but is
            # not an envelope may still carry the id its publisher is
            # waiting on, and cmd_envelope_id takes either.
            session.health_event(
                "drop", reason="invalid-command", key=key, cmd_id=keys.cmd_envelope_id(envelope)
            )
            return
        cmd_id = keys.cmd_envelope_id(envelope)

        with changed:
            effects: list = []
            forward = resolver.command(
                (room, entity, aspect),
                priority,
                actor,
                params.hold_minutes * 60,
                effects,
                value=envelope.get("value"),
                hold=keys.cmd_envelope_hold(envelope),
                cmd_id=cmd_id,
            )
            if forward:
                effects.insert(0, ("forward", (room, entity, aspect), envelope))
            carry_out_locked(effects)

    cmd_sub = session.subscribe("home/cmd/**", cmd_handler)

    session.ready()

    stop = threading.Event()
    signal.signal(signal.SIGTERM, lambda *_: stop.set())
    signal.signal(signal.SIGINT, lambda *_: stop.set())
    expiry = threading.Thread(target=expiry_loop, args=(stop,), daemon=True)
    expiry.start()
    stop.wait()

    with changed:
        changed.notify_all()  # let the expiry thread see the stop flag
    expiry.join(timeout=2)
    cmd_sub.undeclare()
    hold_sub.undeclare()
    for sub in health_subs:
        sub.undeclare()
    session.close()


if __name__ == "__main__":
    main()
