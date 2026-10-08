"""Key builders for the homeostat key space (mirrors the Rust src/bus.rs).

Schema: home/{class}/{room}/{entity}/{aspect} for state, cmd, arbiter and
forecast; home/health/{unit}[...] and home/meta/{unit}/... for supervision.
"""

import re
import secrets
from typing import Any

ENV_UNIT = "HOMEOSTAT_UNIT"
ENV_BUS = "HOMEOSTAT_BUS"

# The core's rule for a name that becomes one key segment (src/validate.rs,
# valid_segment). Other characters either break the fixed key schema ("/"),
# have a meaning on the bus ("*", "$", "?", "#"), or risk encoding
# problems.
_SEGMENT = re.compile(r"[A-Za-z0-9_.-]+")


def valid_segment(name: str) -> bool:
    """Return whether `name` is usable as a single bus key segment.

    Parameters
    ----------
    name : str
        The candidate segment. A non-string is not valid.

    Returns
    -------
    bool
        True if `name` is a valid key segment.
    """
    return isinstance(name, str) and _SEGMENT.fullmatch(name) is not None and name not in (".", "..")


def _segments(*names: str) -> str:
    """Join validated segments with "/".

    Raises ValueError on a segment the core would refuse. The usual cause
    is a field name chosen by a device. A "**" would put on a wildcard and
    reach every aspect subscriber. A "#" or "" raises inside the zenoh put.
    Callers drop the field with a "malformed-payload" health event.
    """
    for name in names:
        if not valid_segment(name):
            raise ValueError(f"{name!r} is not a valid key segment")
    return "/".join(names)


def state_key(room: str, entity: str, aspect: str) -> str:
    """Return the state key of one aspect of one entity.

    Parameters
    ----------
    room : str
        The entity's room.
    entity : str
        The entity's name.
    aspect : str
        The aspect.

    Returns
    -------
    str
        home/state/{room}/{entity}/{aspect}.

    Raises
    ------
    ValueError
        If a part is not a valid key segment.
    """
    return "home/state/" + _segments(room, entity, aspect)


def state_keyexpr(room: str, entity: str) -> str:
    """Return the key expression matching every direct state aspect of one entity.

    Used to subscribe to a fed input's whole state. The room and entity
    come from the house's own manifests rather than from a device (see
    `_segments`).

    Parameters
    ----------
    room : str
        The entity's room.
    entity : str
        The entity's name.

    Returns
    -------
    str
        home/state/{room}/{entity}/*.

    Raises
    ------
    ValueError
        If a part is not a valid key segment.
    """
    return "home/state/" + _segments(room, entity) + "/*"


def forecast_key(room: str, entity: str, aspect: str, source: str) -> str:
    """Return the key of a series' forecast: its state key plus the source.

    See docs/design.md#forecasts. The key has the same room/entity/aspect as
    `state_key`, followed by the source. A forecast is therefore the same
    series extended forward, and the entity's aspect descriptor already
    labels it. Several sources may forecast one series, such as two weather
    providers, or a controller publishing the trajectory it plans to cause.
    The source segment is therefore required.

    Parameters
    ----------
    room : str
        The entity's room.
    entity : str
        The entity's name.
    aspect : str
        The aspect.
    source : str
        Who says so, e.g. a weather provider or a controller.

    Returns
    -------
    str
        home/forecast/{room}/{entity}/{aspect}/{source}.

    Raises
    ------
    ValueError
        If a part is not a valid key segment.
    """
    return "home/forecast/" + _segments(room, entity, aspect, source)


def forecast_keyexpr(room: str, entity: str) -> str:
    """Return the key expression matching every forecast aspect of one entity.

    It matches every source.

    Parameters
    ----------
    room : str
        The entity's room.
    entity : str
        The entity's name.

    Returns
    -------
    str
        home/forecast/{room}/{entity}/*/*.

    Raises
    ------
    ValueError
        If a part is not a valid key segment.
    """
    return "home/forecast/" + _segments(room, entity) + "/*/*"


def cmd_key(room: str, entity: str, aspect: str) -> str:
    """Return the command key of one aspect of one entity.

    Parameters
    ----------
    room : str
        The entity's room.
    entity : str
        The entity's name.
    aspect : str
        The aspect.

    Returns
    -------
    str
        home/cmd/{room}/{entity}/{aspect}.

    Raises
    ------
    ValueError
        If a part is not a valid key segment.
    """
    return "home/cmd/" + _segments(room, entity, aspect)


def cmd_keyexpr(room: str, entity: str) -> str:
    """Return the key expression matching every command aspect of one entity.

    Parameters
    ----------
    room : str
        The entity's room.
    entity : str
        The entity's name.

    Returns
    -------
    str
        home/cmd/{room}/{entity}/**.

    Raises
    ------
    ValueError
        If a part is not a valid key segment.
    """
    return "home/cmd/" + _segments(room, entity) + "/**"


def arbiter_key(room: str, entity: str, aspect: str) -> str:
    """Return the key of the arbiter's grant output for an arbitrated entity.

    It has the cmd shape, in its own reserved class
    (docs/design.md#arbitrated-mode).

    Parameters
    ----------
    room : str
        The entity's room.
    entity : str
        The entity's name.
    aspect : str
        The aspect.

    Returns
    -------
    str
        home/arbiter/{room}/{entity}/{aspect}.

    Raises
    ------
    ValueError
        If a part is not a valid key segment.
    """
    return "home/arbiter/" + _segments(room, entity, aspect)


def arbiter_keyexpr(room: str, entity: str) -> str:
    """Return the key expression matching every arbiter aspect of one entity.

    Parameters
    ----------
    room : str
        The entity's room.
    entity : str
        The entity's name.

    Returns
    -------
    str
        home/arbiter/{room}/{entity}/**.

    Raises
    ------
    ValueError
        If a part is not a valid key segment.
    """
    return "home/arbiter/" + _segments(room, entity) + "/**"


def command_keyexprs(entity) -> list[str]:
    """Return the key expressions on which one bound entity receives commands.

    That is home/cmd for plain entities. An arbitrated entity gets no
    home/cmd subscription at all, which is how arbitration is enforced. It
    receives the arbiter's forwarded envelope instead.

    Parameters
    ----------
    entity : house.Entity
        The bound entity; its `room`, `name` and `write_mode` are read.

    Returns
    -------
    list of str
        The key expressions to subscribe to for the entity's commands.

    Raises
    ------
    ValueError
        If the entity's room or name is not a valid key segment.
    """
    if entity.write_mode == "arbitrated":
        return [arbiter_keyexpr(entity.room, entity.name)]
    return [cmd_keyexpr(entity.room, entity.name)]


CMD_PRIORITIES = ("automation", "agent", "family", "manual")


def cmd_envelope(value: Any, priority: str, actor: str, *, cmd_id: str | None = None) -> dict:
    """Build a home/cmd/** payload (docs/design.md#cmd-envelopes).

    Every cmd payload is an envelope. Its priority comes from the
    publishing unit's manifest, and its actor is the unit name.

    `id` links one command to whatever ends it. A command passes through
    arbitration, adapter validation and device readback, and any of these
    stages can end it before it reaches the device. Without an id, a
    publisher watching for the outcome can only match on key and value,
    which goes wrong when two commands to one aspect overlap. An id is
    generated here when the caller does not supply one, so every envelope
    the SDK builds has one.

    Parameters
    ----------
    value : Any
        The command's value, JSON-encodable.
    priority : str
        One of `CMD_PRIORITIES`.
    actor : str
        The publishing unit's name.
    cmd_id : str or None, optional
        The correlation id. A random one is generated when not given.

    Returns
    -------
    dict
        The envelope, with `value`, `priority`, `actor` and `id`.
    """
    return {
        "value": value,
        "priority": priority,
        "actor": actor,
        "id": cmd_id or secrets.token_hex(4),
    }


def cmd_envelope_id(payload: Any) -> str | None:
    """Return the correlation id of a cmd payload, or None if it has none.

    Returns None for an envelope from a publisher that does not set an id.
    The id is optional on the wire, so a command without one is not
    refused.

    Parameters
    ----------
    payload : Any
        A decoded cmd payload. A non-object has no id.

    Returns
    -------
    str or None
        The envelope's `id`, or None.
    """
    return payload.get("id") if isinstance(payload, dict) else None


def parse_cmd_envelope(payload: Any) -> Any:
    """Validate a cmd envelope and return its value.

    Callers drop a malformed envelope with an "invalid-command" health
    event.

    Parameters
    ----------
    payload : Any
        A decoded cmd payload.

    Returns
    -------
    Any
        The envelope's `value`.

    Raises
    ------
    ValueError
        On anything malformed: not an object, missing value or priority, or
        an unknown priority.
    """
    if not isinstance(payload, dict):
        raise ValueError("cmd payload is not a JSON object")
    if "value" not in payload or "priority" not in payload:
        raise ValueError("cmd envelope missing value or priority")
    if payload["priority"] not in CMD_PRIORITIES:
        raise ValueError(f"unknown priority {payload['priority']!r}")
    return payload["value"]


def config_key(unit: str, param: str) -> str:
    """Return the key of a core-owned live parameter value (docs/design.md#live-parameters).

    Parameters
    ----------
    unit : str
        The unit's name.
    param : str
        The parameter's name.

    Returns
    -------
    str
        home/config/{unit}/{param}.

    Raises
    ------
    ValueError
        If a part is not a valid key segment.
    """
    return "home/config/" + _segments(unit, param)


def config_keyexpr(unit: str) -> str:
    """Return the key expression matching every parameter of one unit.

    Parameters
    ----------
    unit : str
        The unit's name.

    Returns
    -------
    str
        home/config/{unit}/*.

    Raises
    ------
    ValueError
        If a part is not a valid key segment.
    """
    return "home/config/" + _segments(unit) + "/*"


def history_key(space: str, entity: str, aspect: str) -> str:
    """Return a history series key, entity-first.

    The entity identifies the series. The room is a tag stored on each row.

    Parameters
    ----------
    space : str
        'state' or 'cmd'.
    entity : str
        The entity's name.
    aspect : str
        The aspect.

    Returns
    -------
    str
        home/history/{space}/{entity}/{aspect}.

    Raises
    ------
    ValueError
        If a part is not a valid key segment.
    """
    return "home/history/" + _segments(space, entity, aspect)


def hold_key(unit: str) -> str:
    """Return the key of what an arbiter is currently holding.

    There is one document per arbiter unit, in the discovery shape, and
    the core mirrors it. The preempt and refuse events record what
    happened. This key answers whether an aspect is held right now
    (docs/design.md#arbitrated-mode).

    Parameters
    ----------
    unit : str
        The arbiter unit's name.

    Returns
    -------
    str
        home/hold/{unit}.

    Raises
    ------
    ValueError
        If a part is not a valid key segment.
    """
    return "home/hold/" + _segments(unit)


def discovery_key(unit: str) -> str:
    """Return the key of an adapter's complete current view of its periphery.

    It carries one JSON array of device records (see
    docs/design.md#discovery).

    Parameters
    ----------
    unit : str
        The adapter unit's name.

    Returns
    -------
    str
        home/discovery/{unit}.

    Raises
    ------
    ValueError
        If a part is not a valid key segment.
    """
    return "home/discovery/" + _segments(unit)


def liveliness_key(unit: str) -> str:
    """Return the key of a unit's liveliness token.

    Parameters
    ----------
    unit : str
        The unit's name.

    Returns
    -------
    str
        home/health/{unit}/alive.

    Raises
    ------
    ValueError
        If a part is not a valid key segment.
    """
    return "home/health/" + _segments(unit) + "/alive"


def health_event_key(unit: str) -> str:
    """Return the key of unit-published JSON events (e.g. dropped payloads).

    The parent key home/health/{unit} itself belongs to the supervisor.

    Parameters
    ----------
    unit : str
        The unit's name.

    Returns
    -------
    str
        home/health/{unit}/event.

    Raises
    ------
    ValueError
        If a part is not a valid key segment.
    """
    return "home/health/" + _segments(unit) + "/event"
