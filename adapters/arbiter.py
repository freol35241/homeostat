# /// script
# requires-python = ">=3.11"
# dependencies = [
#     "homeostat",
# ]
#
# [tool.uv.sources]
# homeostat = { path = "../sdk/python", editable = true }
# ///
"""Arbiter service: holds the write lease for arbitrated entities.

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

A wish for an arbitrated entity is checked against the lease on its
(entity, aspect) (`Leases.wish`). If it wins, the envelope is forwarded
unchanged to home/arbiter/{room}/{entity}/{aspect} and the lease is taken
or refreshed for hold_minutes. The manual band is highest, so the family
wins over automations. Expiry reopens the aspect, so a forgotten override
clears itself.

A manual-band wish may carry a `hold` intent (docs/design.md#hold-intents):
"none" forwards without holding, an RFC3339 instant holds until then,
"report" holds without forwarding because the device already did it, and
"release" hands the aspect back without forwarding anything.

Each lease in force is published at home/hold/{unit}/{room}/{entity}/{aspect}
(`Leases.records`), mirrored by the core, and the key is deleted when the
lease ends. A restart takes back the leases the mirror still has.

Configuration: the live parameter hold_minutes (family-editable), seeded
from the manifest default.

Health events at home/health/{unit}/event: `drop` (off-schema-key,
invalid-command, unbound), `preempt` when a wish takes over from a lower
band, and `refuse` when a wish below the holder's band is turned away.
"""

import datetime
import json
import os
import signal
import threading
import time
from functools import partial

import homeostat
from homeostat import house, keys
from homeostat.params import LiveParams

PARAM = "hold_minutes"
GRANTS_KEY = "home/meta/system/grants"
PENDING_KEY = "home/meta/system/grants_pending"


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


class Bindings:
    """The arbitrated and bound entities, live from the grant tables.

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
        # Both sets in one attribute, replaced in one assignment, so a
        # reader never pairs sets from different tables.
        self._sets: tuple[frozenset, frozenset] = (frozenset(), frozenset())
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
                        self._sets = bindings(self._tables[GRANTS_KEY] + self._tables[PENDING_KEY])
                        seeded = True
        if seeded:
            with self._lock:
                self._on_change(self._sets[0])

    def _on_table(self, key: str, sample) -> None:
        with self._lock:
            self._live.add(key)
            self._tables[key] = json.loads(sample.payload.to_bytes())
            self._sets = bindings(self._tables[GRANTS_KEY] + self._tables[PENDING_KEY])
            self._on_change(self._sets[0])

    def route(self, target: tuple[str, str]) -> str:
        """Return "arbitrated", "bound" or "unbound" for a (room, entity)."""
        arbitrated, bound = self._sets
        if target in arbitrated:
            return "arbitrated"
        return "bound" if target in bound else "unbound"


def iso(epoch: float) -> str:
    """Format an epoch second as RFC3339 UTC.

    That is the spelling every other timestamp on this bus uses.
    """
    return (
        datetime.datetime.fromtimestamp(epoch, datetime.timezone.utc)
        .isoformat(timespec="seconds")
        .replace("+00:00", "Z")
    )


class Leases:
    """The holds in force, one per (room, entity, aspect), and the rule.

    A lease is per (entity, aspect), the granularity of the cmd key, and
    not per entity. One entity can carry independent controls: on the
    heat pump, the family's setpoint must not freeze an automation's
    outdoor_temperature_offset.

    The manual band's lease is the family hold. A manual command's `hold`
    intent decides what happens to it (docs/design.md#hold-intents). Lower
    bands take a lease for the hold time on every forward, as before.

    Enforcement uses time.monotonic(), which a clock step cannot shorten
    or stretch. The published `since` and `until` are wall-clock, fixed
    when the lease is taken, because another process, and this one after
    a restart, can only use a wall-clock deadline. If the clock is stepped
    the two disagree, and only the displayed countdown is off.

    Pure bookkeeping: no bus, no threads. The caller serialises access and
    does the publishing; `clock` and `wall` are time.monotonic and
    time.time in the unit and fakes in tests.
    """

    def __init__(self, clock=time.monotonic, wall=time.time):
        self._clock = clock
        self._wall = wall
        self._held: dict[tuple[str, str, str], dict] = {}

    def wish(
        self,
        target: tuple[str, str, str],
        priority: str,
        actor: str,
        hold_s: float,
        *,
        value=None,
        hold: str | None = None,
    ):
        """Arbitrate one wish for `target` at `priority` from `actor`.

        With no lease in force, or an incoming priority at or above the
        holder's band (in keys.CMD_PRIORITIES order), the wish wins. A
        priority strictly below the holder's is refused and counted against
        the holder.

        A winning wish takes the lease at its own band and actor for
        `hold_s` seconds, or until the instant `hold` names, and taking
        over from a strictly lower holder is a preempt. `hold` = "report"
        takes it the same way. "none" and "release" clear the family hold
        and take nothing, so a lower band's lease stays where it is. An
        instant already past clears it too. Whether the wish is forwarded
        is the caller's decision: "report" and "release" are not.

        Returns
        -------
        tuple of (str, dict or None)
            The action, "forward", "preempt" or "refuse", and the lease that
            was in force when the wish arrived (None if none was).
        """
        now = self._clock()
        lease = self._held.get(target)
        holder = dict(lease) if lease is not None and now < lease["deadline"] else None
        incoming = keys.CMD_PRIORITIES.index(priority)
        if holder is not None and incoming < keys.CMD_PRIORITIES.index(holder["priority"]):
            # The count lives on the hold, so a consumer can say "this
            # override has turned an automation away twice" without
            # replaying the event log.
            self._held[target]["refused"] += 1
            return "refuse", holder
        seconds = hold_s
        if hold not in (None, "none", "report", "release"):
            seconds = keys.hold_instant(hold).timestamp() - self._wall()
        if hold in ("none", "release") or seconds <= 0:
            if holder is not None and holder["priority"] == "manual":
                del self._held[target]
            return "forward", holder
        preempts = holder is not None and incoming > keys.CMD_PRIORITIES.index(holder["priority"])
        wall = self._wall()
        self._held[target] = {
            "priority": priority,
            "actor": actor,
            "value": value,
            "deadline": now + seconds,
            "since": iso(wall),
            "until": iso(wall + seconds),
            # A refreshed hold starts its count again, because the count
            # belongs to the hold in force.
            "refused": 0,
        }
        return ("preempt" if preempts else "forward"), holder

    def restore(self, records: list) -> None:
        """Take back the holds a previous run published and the mirror kept.

        A record whose `until` has passed, or that does not read as a hold,
        is skipped. The caller deletes what was not restored.

        Parameters
        ----------
        records : list of dict
            Hold records as `records` returns them.
        """
        now, wall = self._clock(), self._wall()
        for record in records:
            try:
                target = (record["room"], record["entity"], record["aspect"])
                until = datetime.datetime.fromisoformat(record["until"]).timestamp()
                if record["priority"] not in keys.CMD_PRIORITIES:
                    continue
                lease = {
                    "priority": record["priority"],
                    "actor": str(record["actor"]),
                    "value": record.get("value"),
                    "deadline": now + (until - wall),
                    "since": str(record["since"]),
                    "until": record["until"],
                    "refused": int(record.get("refused", 0)),
                }
            except (TypeError, KeyError, ValueError):
                continue
            if until > wall:
                self._held[target] = lease

    def prune(self) -> bool:
        """Drop expired leases; True when something went."""
        now = self._clock()
        expired = [k for k, lease in self._held.items() if lease["deadline"] <= now]
        for k in expired:
            del self._held[k]
        return bool(expired)

    def retain(self, arbitrated: frozenset) -> bool:
        """Drop leases on entities outside `arbitrated`; True when something went.

        An entity that stops being arbitrated is commanded by its adapter
        directly, so a hold on it no longer holds anything.
        """
        gone = [k for k in self._held if k[:2] not in arbitrated]
        for k in gone:
            del self._held[k]
        return bool(gone)

    def next_deadline(self) -> float | None:
        """Return the earliest deadline in force, or None with no leases."""
        return min((lease["deadline"] for lease in self._held.values()), default=None)

    def records(self) -> dict:
        """Return the hold record of each lease in force, by target.

        A record answers "is this aspect held right now?". The preempt and
        refuse events cannot: they are an audit trail, and a consumer that
        joins during a hold has missed them.
        """
        now = self._clock()
        out = {}
        for (room, entity, aspect), lease in self._held.items():
            if lease["deadline"] <= now:
                continue
            out[(room, entity, aspect)] = {
                "schema": 1,
                "room": room,
                "entity": entity,
                "aspect": aspect,
                "priority": lease["priority"],
                "actor": lease["actor"],
                # What the holder asked for, or what the device reported.
                "value": lease["value"],
                "since": lease["since"],
                # Arbitration enforces the monotonic deadline; a countdown
                # is drawn from this.
                "until": lease["until"],
                # How many wishes this hold has refused.
                "refused": lease["refused"],
            }
        return out


def main():
    unit = os.environ[keys.ENV_UNIT]
    own = next(u for u in house.load_house(".").units if u.name == unit)

    session = homeostat.connect()
    params = Params(session, {PARAM: float(own.params[PARAM]["default"])})
    leases = Leases()
    # Guards `leases` and `published`, and wakes the expiry thread when the
    # earliest deadline moves (a new lease, or one pruned) so it does not
    # sleep past a hold's end.
    changed = threading.Condition(threading.Lock())
    # hold key -> the record last put there.
    published: dict[str, dict] = {}

    def publish_holds_locked() -> None:
        # One key per held aspect. A record that changed is put again, and
        # the key of a hold that ended is deleted, so the mirror and every
        # subscriber forget it.
        current = {
            keys.hold_key(unit, *target): record for target, record in leases.records().items()
        }
        for key in published.keys() - current.keys():
            session.delete(key)
        for key, record in current.items():
            if published.get(key) != record:
                session.put_json(key, record)
        published.clear()
        published.update(current)

    # The family's holds outlive this process: the mirror kept what the
    # last run published. Those still in force are taken back, and the
    # first publish deletes the rest. This runs before Bindings, whose seed
    # drops a restored hold on an entity no longer arbitrated.
    try:
        kept = session.get_json(keys.hold_keyexpr(unit), timeout_s=5)
    except (homeostat.QueryTimeout, homeostat.QueryError):
        kept = []
    with changed:
        published.update((key, record) for key, record in kept)
        leases.restore([record for _key, record in kept])
        publish_holds_locked()

    def on_arbitrated(arbitrated: frozenset) -> None:
        with changed:
            if leases.retain(arbitrated):
                publish_holds_locked()
                changed.notify_all()

    routes = Bindings(session, on_arbitrated)

    def expiry_loop(stop: threading.Event) -> None:
        """One thread for every lease, waiting on the earliest deadline.

        Expiry is otherwise lazy, evaluated on the next wish for that key.
        That is enough for arbitration, but the published record would go
        on showing a hold that had ended.
        """
        while not stop.is_set():
            with changed:
                if leases.prune():
                    publish_holds_locked()
                deadline = leases.next_deadline()
                timeout = None if deadline is None else max(0.05, deadline - time.monotonic())
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
        hold = keys.cmd_envelope_hold(envelope)

        with changed:
            action, holder = leases.wish(
                (room, entity, aspect),
                priority,
                actor,
                params.hold_minutes * 60,
                value=envelope.get("value"),
                hold=hold,
            )
            publish_holds_locked()
            # The earliest deadline may have moved, and the expiry thread
            # must not sleep on the old one.
            changed.notify_all()

        if action == "refuse":
            # The command was well-formed and reached the arbiter, but a
            # higher band holds the aspect. `cmd_id` lets the publisher
            # report that at once instead of waiting for its timeout.
            session.health_event(
                "refuse",
                room=room,
                entity=entity,
                aspect=aspect,
                priority=priority,
                actor=actor,
                cmd_id=cmd_id,
                holder_priority=holder["priority"],
                holder_actor=holder["actor"],
            )
            return
        if action == "preempt":
            session.health_event(
                "preempt",
                room=room,
                entity=entity,
                aspect=aspect,
                from_priority=holder["priority"],
                from_actor=holder["actor"],
                to_priority=priority,
                to_actor=actor,
            )
        if hold in ("report", "release"):
            # The device already did it, or there is nothing to do: either
            # way only the hold changes.
            return
        session.put_json(keys.arbiter_key(room, entity, aspect), envelope)

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
    session.close()


if __name__ == "__main__":
    main()
