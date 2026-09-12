"""Key builders for the homeostat key space (mirrors the Rust src/bus.rs).

Schema: home/{class}/{room}/{entity}/{aspect} for state, cmd, and arbiter;
home/health/{unit}[...] and home/meta/{unit}/... for supervision.
"""

import re
from typing import Any

ENV_UNIT = "HOMEOSTAT_UNIT"
ENV_BUS = "HOMEOSTAT_BUS"

# The core's rule for a name that becomes one key segment (src/validate.rs,
# valid_segment): anything else breaks the fixed key schema ("/"), is
# meaningful to the bus ("*", "$", "?", "#") or invites encoding surprises.
_SEGMENT = re.compile(r"[A-Za-z0-9_.-]+")


def valid_segment(name: str) -> bool:
    """Whether `name` is usable as exactly one bus key segment."""
    return isinstance(name, str) and _SEGMENT.fullmatch(name) is not None and name not in (".", "..")


def _segments(*names: str) -> str:
    """Joins validated segments with "/". Raises ValueError on a segment
    the core would refuse — a device-chosen field name is the usual
    offender (a "**" would put on a wildcard and fan out to every aspect
    subscriber; a "#" or "" raises inside the zenoh put); callers drop the
    field with a "malformed-payload" health event."""
    for name in names:
        if not valid_segment(name):
            raise ValueError(f"{name!r} is not a valid key segment")
    return "/".join(names)


def state_key(room: str, entity: str, aspect: str) -> str:
    return "home/state/" + _segments(room, entity, aspect)


def state_keyexpr(room: str, entity: str) -> str:
    """Key expression matching every direct state aspect of one entity —
    for subscribing to a fed input's whole state, room/entity coming from
    the house's own manifests, not a device (see `_segments`)."""
    return "home/state/" + _segments(room, entity) + "/*"


def cmd_key(room: str, entity: str, aspect: str) -> str:
    return "home/cmd/" + _segments(room, entity, aspect)


def cmd_keyexpr(room: str, entity: str) -> str:
    """Key expression matching every command aspect of one entity."""
    return "home/cmd/" + _segments(room, entity) + "/**"


def arbiter_key(room: str, entity: str, aspect: str) -> str:
    """The arbiter's grant output for an arbitrated entity (docs/design.md,
    Arbitrated mode): the cmd shape, its own reserved class."""
    return "home/arbiter/" + _segments(room, entity, aspect)


def arbiter_keyexpr(room: str, entity: str) -> str:
    """Key expression matching every arbiter aspect of one entity."""
    return "home/arbiter/" + _segments(room, entity) + "/**"


def command_keyexprs(entity) -> list[str]:
    """The key expressions on which one bound entity receives commands:
    home/cmd for plain entities. An arbitrated entity gets no home/cmd
    subscription at all — not subscribing IS the structural enforcement —
    and instead receives the arbiter's forwarded envelope."""
    if entity.write_mode == "arbitrated":
        return [arbiter_keyexpr(entity.room, entity.name)]
    return [cmd_keyexpr(entity.room, entity.name)]


CMD_PRIORITIES = ("automation", "agent", "family", "manual")


def cmd_envelope(value: Any, priority: str, actor: str) -> dict:
    """Builds a home/cmd/** payload (docs/design.md, Arbitrated mode): every
    cmd payload is an envelope, priority stamped from the publishing unit's
    manifest declaration, actor the unit name."""
    return {"value": value, "priority": priority, "actor": actor}


def parse_cmd_envelope(payload: Any) -> Any:
    """Validates a cmd envelope, returning its value. Raises ValueError on
    anything malformed: not an object, missing value or priority, or an
    unknown priority — callers drop these with an "invalid-command" health
    event."""
    if not isinstance(payload, dict):
        raise ValueError("cmd payload is not a JSON object")
    if "value" not in payload or "priority" not in payload:
        raise ValueError("cmd envelope missing value or priority")
    if payload["priority"] not in CMD_PRIORITIES:
        raise ValueError(f"unknown priority {payload['priority']!r}")
    return payload["value"]


def config_key(unit: str, param: str) -> str:
    """Core-owned live parameter value (see docs/design.md, step 4)."""
    return "home/config/" + _segments(unit, param)


def config_keyexpr(unit: str) -> str:
    """Key expression matching every parameter of one unit."""
    return "home/config/" + _segments(unit) + "/*"


def history_key(space: str, entity: str, aspect: str) -> str:
    """History series key: entity-first (entity is the series identity,
    room is a tag carried per row). `space` is 'state' or 'cmd'."""
    return "home/history/" + _segments(space, entity, aspect)


def discovery_key(unit: str) -> str:
    """An adapter's complete current view of its periphery: one JSON array
    of device records (see docs/design.md, Discovery)."""
    return "home/discovery/" + _segments(unit)


def liveliness_key(unit: str) -> str:
    return "home/health/" + _segments(unit) + "/alive"


def health_event_key(unit: str) -> str:
    """Unit-published JSON events (e.g. dropped payloads); the parent key
    home/health/{unit} itself belongs to the supervisor."""
    return "home/health/" + _segments(unit) + "/event"
