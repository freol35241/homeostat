# /// script
# requires-python = ">=3.11"
# dependencies = [
#     "homeostat",
# ]
#
# [tool.uv.sources]
# homeostat = { git = "https://github.com/freol35241/homeostat", subdirectory = "sdk/python", tag = "v0.4.0" }
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

hold_minutes is a family-editable parameter, seeded from the manifest
default and kept live by a config subscription — replicated minimally on
the session rather than routed through automation.Context, whose
Context.publish only knows the state/cmd key-slot shape and has no notion
of this service's arbitrary, per-wish home/arbiter/{room}/{entity}/{aspect}
forwarding keys.
"""

import json
import os
import signal
import threading
import time

import homeostat
from homeostat import house, keys

PARAM = "hold_minutes"


def main():
    unit = os.environ[keys.ENV_UNIT]
    model = house.load_house(".")
    arbitrated = {(e.room, e.name) for e in model.entities if e.write_mode == "arbitrated"}
    own = next(u for u in model.units if u.name == unit)
    hold_minutes = float(own.params[PARAM]["default"])

    session = homeostat.connect()
    lock = threading.Lock()
    leases: dict[tuple[str, str, str], dict] = {}

    def on_config(sample):
        nonlocal hold_minutes
        param = str(sample.key_expr).rsplit("/", 1)[1]
        if param != PARAM:
            return
        try:
            value = json.loads(sample.payload.to_bytes())
        except ValueError:
            return
        with lock:
            hold_minutes = float(value)

    # Subscribe, then get, merge: the get covers everything published
    # before this subscription, the subscriber everything after (the same
    # ordering automation.Context uses for [params.*]).
    config_sub = session.subscribe(keys.config_keyexpr(unit), on_config)
    served = dict(session.get_json(keys.config_keyexpr(unit)))
    seeded = served.get(keys.config_key(unit, PARAM))
    if seeded is not None:
        with lock:
            hold_minutes = float(seeded)

    def cmd_handler(sample):
        key = str(sample.key_expr)
        parts = key.split("/", 4)
        if len(parts) < 5:
            session.health_event("drop", reason="off-schema-key", key=key)
            return
        room, entity, aspect = parts[2:5]
        if (room, entity) not in arbitrated:
            return  # not arbitrated: its own adapter consumes this wish

        try:
            envelope = json.loads(sample.payload.to_bytes())
            keys.parse_cmd_envelope(envelope)
            priority, actor = envelope["priority"], envelope["actor"]
            if not isinstance(actor, str):
                raise ValueError("cmd envelope actor is not a string")
        except (ValueError, KeyError):
            session.health_event("drop", reason="invalid-command", key=key)
            return

        incoming = keys.CMD_PRIORITIES.index(priority)
        with lock:
            now = time.monotonic()
            lease = leases.get((room, entity, aspect))
            holder = lease if lease is not None and now < lease["deadline"] else None
            if holder is not None and incoming < keys.CMD_PRIORITIES.index(holder["priority"]):
                action = "refuse"
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
                    "deadline": now + hold_minutes * 60,
                }

        if action == "refuse":
            session.health_event(
                "refuse",
                room=room,
                entity=entity,
                aspect=aspect,
                priority=priority,
                actor=actor,
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

    session.ready()

    stop = threading.Event()
    signal.signal(signal.SIGTERM, lambda *_: stop.set())
    signal.signal(signal.SIGINT, lambda *_: stop.set())
    stop.wait()

    cmd_sub.undeclare()
    config_sub.undeclare()
    session.close()


if __name__ == "__main__":
    main()
