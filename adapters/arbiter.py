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
would go stale.

A wish for an arbitrated entity is checked against the lease on its
(entity, aspect) (`Leases.wish`). If it wins, the envelope is forwarded
unchanged to home/arbiter/{room}/{entity}/{aspect} and the lease is taken
or refreshed for hold_minutes. The manual band is highest, so the family
wins over automations. Expiry reopens the aspect, so a forgotten override
clears itself.

The leases in force are published as one document at home/hold/{unit}
(`Leases.document`), mirrored by the core.

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

import homeostat
from homeostat import house, keys
from homeostat.params import LiveParams

PARAM = "hold_minutes"
GRANTS_KEY = "home/meta/system/grants"


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
    """The arbitrated and bound entities, live from the grant table.

    The core puts the table again when an apply completes. This follows the
    LiveParams read pattern: subscribe, then seed with a get, and a table
    the subscription has already delivered wins over the seed.
    """

    def __init__(self, session):
        # Both sets in one attribute, replaced in one assignment, so a
        # reader never pairs sets from different tables.
        self._sets: tuple[frozenset, frozenset] = (frozenset(), frozenset())
        self._live = False
        self._sub = session.subscribe(GRANTS_KEY, self._on_grants)
        for _key, value in session.get_json(GRANTS_KEY):
            if not self._live:
                self._sets = bindings(value)

    def _on_grants(self, sample) -> None:
        self._live = True
        self._sets = bindings(json.loads(sample.payload.to_bytes()))

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

    Enforcement uses time.monotonic(), which a clock step cannot shorten
    or stretch. The published `until` is its wall-clock equivalent,
    because another process can only use a wall-clock deadline. If the
    clock is stepped the two disagree, and only the displayed countdown
    is off.

    Pure bookkeeping: no bus, no threads. The caller serialises access and
    does the publishing; `clock` is time.monotonic in the unit and a fake
    in tests.
    """

    def __init__(self, clock=time.monotonic):
        self._clock = clock
        self._held: dict[tuple[str, str, str], dict] = {}

    def wish(self, target: tuple[str, str, str], priority: str, actor: str, hold_s: float):
        """Arbitrate one wish for `target` at `priority` from `actor`.

        With no lease in force, or an incoming priority at or above the
        holder's band (in keys.CMD_PRIORITIES order), the wish is forwarded
        and takes the lease at its own band and actor. Taking over from a
        strictly lower holder is a preempt. A priority strictly below the
        holder's is refused and not forwarded.

        Returns
        -------
        tuple of (str, dict or None)
            The action, "forward", "preempt" or "refuse", and the lease that
            was in force when the wish arrived (None if none was). Forward
            and preempt take or refresh the lease for `hold_s` seconds; a
            refusal counts against the holder instead.
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
        preempts = holder is not None and incoming > keys.CMD_PRIORITIES.index(holder["priority"])
        self._held[target] = {
            "priority": priority,
            "actor": actor,
            "deadline": now + hold_s,
            "taken": now,
            # A refreshed hold starts its count again, because the count
            # belongs to the hold in force.
            "refused": 0,
        }
        return ("preempt" if preempts else "forward"), holder

    def prune(self) -> bool:
        """Drop expired leases; True when something went."""
        now = self._clock()
        expired = [k for k, lease in self._held.items() if lease["deadline"] <= now]
        for k in expired:
            del self._held[k]
        return bool(expired)

    def next_deadline(self) -> float | None:
        """Return the earliest deadline in force, or None with no leases."""
        return min((lease["deadline"] for lease in self._held.values()), default=None)

    def document(self, wall: float) -> dict:
        """Return the holds document, ordered by room, entity and aspect.

        The document answers "is this aspect held right now?". The
        preempt and refuse events cannot: they are an audit trail, and a
        consumer that joins during a hold has missed them. It is one
        document per arbiter, shaped like home/discovery/{unit}, and not a
        key per lease. A restart then publishes one empty list, expiry
        needs one timer, and a reader always sees a consistent set.

        `wall` is time.time() at the same moment the clock is read, so the
        published `since` and `until` are the monotonic lease in wall time.
        """
        now = self._clock()
        holds = []
        for (room, entity, aspect), lease in sorted(self._held.items()):
            if lease["deadline"] <= now:
                continue
            holds.append(
                {
                    "room": room,
                    "entity": entity,
                    "aspect": aspect,
                    "priority": lease["priority"],
                    "actor": lease["actor"],
                    "since": iso(wall - (now - lease["taken"])),
                    # Arbitration enforces the monotonic deadline; a
                    # countdown is drawn from this.
                    "until": iso(wall + (lease["deadline"] - now)),
                    # How many wishes this hold has refused.
                    "refused": lease["refused"],
                }
            )
        return {"schema": 1, "holds": holds}


def main():
    unit = os.environ[keys.ENV_UNIT]
    own = next(u for u in house.load_house(".").units if u.name == unit)

    session = homeostat.connect()
    params = Params(session, {PARAM: float(own.params[PARAM]["default"])})
    routes = Bindings(session)
    leases = Leases()
    hold_key = keys.hold_key(unit)
    # Guards `leases`, and wakes the expiry thread when the earliest
    # deadline moves (a new lease, or one pruned) so it does not sleep past
    # a hold's end.
    changed = threading.Condition(threading.Lock())

    def publish_holds_locked() -> None:
        # Each document replaces the previous one, so a consumer does not
        # merge two.
        session.put_json(hold_key, leases.document(time.time()))

    def expiry_loop(stop: threading.Event) -> None:
        """One thread for every lease, waiting on the earliest deadline.

        Expiry is otherwise lazy, evaluated on the next wish for that key.
        That is enough for arbitration, but the published document would
        go on showing a hold that had ended.
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

        with changed:
            action, holder = leases.wish(
                (room, entity, aspect), priority, actor, params.hold_minutes * 60
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
        session.put_json(keys.arbiter_key(room, entity, aspect), envelope)

    cmd_sub = session.subscribe("home/cmd/**", cmd_handler)

    # Leases are in memory, so a restart holds nothing, while the mirror
    # still has what the last process published. One empty document
    # replaces it; with one key per hold there would be a set to find and
    # clear.
    with changed:
        publish_holds_locked()

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
