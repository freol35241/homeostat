# /// script
# requires-python = ">=3.11"
# dependencies = [
#     "homeostat",
# ]
#
# [tool.uv.sources]
# homeostat = { path = "../sdk/python", editable = true }
# ///
"""Arbiter service: the write-token holder for arbitrated entities (see
docs/design.md, Arbitrated mode, "Settled 2026-07-16").

By plan-time construction an adapter's templated cmd subscription excludes
its arbitrated entities, so wishes for them never reach an owner adapter
directly; this service subscribes every wish on home/cmd/** instead. A
wish for a non-arbitrated entity is ignored — its own adapter consumes
home/cmd directly. A wish for an arbitrated entity holds a lease per
(entity, aspect) — the granularity of the cmd key itself, amended from
per-entity when the heat pump showed orthogonal control dimensions
sharing one entity (the family's setpoint must not freeze an
automation's outdoor_temperature_offset; see docs/design.md, Arbitrated
mode): {priority, actor, deadline}, deadline
`time.monotonic() + hold_minutes * 60`. No active lease, an expired one,
or an incoming priority at or above the holder's band (band order
keys.CMD_PRIORITIES, manual highest — THE FAMILY ALWAYS WINS OVER
AUTOMATIONS) forwards the envelope unchanged to
home/arbiter/{room}/{entity}/{aspect} (keys.arbiter_key) and takes or
refreshes the lease at the incoming band/actor; a takeover from a
strictly lower active holder additionally publishes a "preempt" event; an
incoming priority strictly below the holder is refused with a "refuse"
event and no forward. Expiry reopens the aspect to automations, so a
forgotten override self-heals. A malformed envelope drops with an
"invalid-command" event, like any adapter. Events land at
home/health/arbiter/event, recorded like any health event.

What it is holding is published as one document per arbiter unit at
home/hold/{unit}, the home/discovery/{unit} shape, mirrored by the core.
A lease is the answer to "is this aspect held right now?", which the
preempt/refuse events cannot give: they are an audit trail, and a consumer
that joins mid-hold has missed them. One document rather than a key per
lease, because that is what makes the awkward cases trivial — a restart
publishes an empty list (the leases were memory and are gone), expiry needs
one timer rather than one per lease, and a reader sees a consistent set.

Enforcement stays on time.monotonic(), which no clock step can shorten or
stretch; the published `until` is its wall-clock twin, because a countdown
is the only kind of deadline that means anything in another process. The
two can disagree if the clock is stepped, and only the countdown suffers.

hold_minutes is a family-editable parameter, kept live by the SDK's
LiveParams (subscribe-then-get, seeded from the manifest default) rather
than routed through automation.Context, whose Context.publish only knows
the state/cmd key-slot shape and has no notion of this service's
arbitrary, per-wish home/arbiter/{room}/{entity}/{aspect} forwarding keys.
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


class Params(LiveParams):
    """hold_minutes from home/config/{unit}/*, live."""

    @property
    def hold_minutes(self) -> float:
        return self.get(PARAM)


def iso(epoch: float) -> str:
    """An epoch second as RFC3339 UTC, the spelling every other timestamp
    on this bus uses."""
    return (
        datetime.datetime.fromtimestamp(epoch, datetime.timezone.utc)
        .isoformat(timespec="seconds")
        .replace("+00:00", "Z")
    )


def main():
    unit = os.environ[keys.ENV_UNIT]
    model = house.load_house(".")
    arbitrated = {(e.room, e.name) for e in model.entities if e.write_mode == "arbitrated"}
    own = next(u for u in model.units if u.name == unit)

    session = homeostat.connect()
    params = Params(session, {PARAM: float(own.params[PARAM]["default"])})
    lock = threading.Lock()
    leases: dict[tuple[str, str, str], dict] = {}
    hold_key = keys.hold_key(unit)
    # Wakes the expiry thread when the earliest deadline moves — a new
    # lease, or one pruned — so it never sleeps past a hold's end.
    changed = threading.Condition(lock)

    def publish_holds_locked() -> None:
        """The leases still in force, oldest first. Called with the lock
        held, from whoever changed them; the document replaces its
        predecessor, so a consumer never merges two of them."""
        now = time.monotonic()
        wall = time.time()
        holds = []
        for (room, entity, aspect), lease in sorted(leases.items()):
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
                    # The monotonic deadline is what arbitration enforces;
                    # this is what a countdown can be drawn from.
                    "until": iso(wall + (lease["deadline"] - now)),
                    # How many wishes this hold has actually refused: not
                    # what makes it a hold, but what says it cost something.
                    "refused": lease["refused"],
                }
            )
        session.put_json(hold_key, {"schema": 1, "holds": holds})

    def prune_locked() -> bool:
        """Drops expired leases. True when something went, so the caller
        knows whether the document changed."""
        now = time.monotonic()
        expired = [k for k, lease in leases.items() if lease["deadline"] <= now]
        for k in expired:
            del leases[k]
        return bool(expired)

    def expiry_loop(stop: threading.Event) -> None:
        """One thread for every lease, waiting on the earliest deadline.
        Expiry is otherwise lazy — evaluated on the next wish for that key —
        which is correct for arbitration and wrong for a published
        document, which would go on claiming a hold that had ended."""
        while not stop.is_set():
            with changed:
                if prune_locked():
                    publish_holds_locked()
                deadlines = [lease["deadline"] for lease in leases.values()]
                now = time.monotonic()
                timeout = max(0.05, min(deadlines) - now) if deadlines else None
                changed.wait(timeout=timeout)

    def cmd_handler(sample):
        key = str(sample.key_expr)
        parts = key.split("/", 4)
        if len(parts) < 5:
            session.health_event("drop", reason="off-schema-key", key=key)
            return
        room, entity, aspect = parts[2:5]
        if (room, entity) not in arbitrated:
            return  # not arbitrated: its own adapter consumes this wish

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

        incoming = keys.CMD_PRIORITIES.index(priority)
        with changed:
            now = time.monotonic()
            lease = leases.get((room, entity, aspect))
            holder = lease if lease is not None and now < lease["deadline"] else None
            if holder is not None and incoming < keys.CMD_PRIORITIES.index(holder["priority"]):
                action = "refuse"
                # What the hold cost, carried on the hold itself: a
                # consumer can say "this override has turned an automation
                # away twice" without replaying the event log.
                holder["refused"] += 1
            else:
                action = (
                    "preempt"
                    if holder is not None
                    and incoming > keys.CMD_PRIORITIES.index(holder["priority"])
                    else "forward"
                )
                leases[(room, entity, aspect)] = {
                    "priority": priority,
                    "actor": actor,
                    "deadline": now + params.hold_minutes * 60,
                    "taken": now,
                    # A refreshed hold starts its tally again: the count
                    # belongs to the hold in force, not to the aspect.
                    "refused": 0,
                }
            publish_holds_locked()
            # The earliest deadline has moved either way — a fresh lease
            # pushes it out, and the expiry thread must not sleep on the
            # old one.
            changed.notify_all()

        if action == "refuse":
            # The one outcome that is neither success nor failure: the
            # command was well-formed and reached the arbiter, and a higher
            # band simply holds the aspect. `cmd_id` is what lets whoever
            # published it say so, instead of waiting out a timeout it was
            # never going to win.
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

    # Leases are memory, so a restart holds nothing — and the mirror still
    # carries whatever the last process published. One empty document says
    # so exactly, which is the whole reason this is one key and not one per
    # hold: there is no set to enumerate and clear.
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
