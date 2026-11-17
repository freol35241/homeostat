"""Key builders for the homeostat key space (mirrors the Rust src/bus.rs).

Schema: home/{class}/{room}/{entity}/{aspect} for state, cmd, arbiter and
forecast; home/health/{unit}[...] and home/meta/{unit}/... for supervision.
"""

import datetime
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

# The words a manual-band command's `hold` may carry besides an instant
# (docs/design.md#hold-intents). "report" and "release" are for the
# arbiter alone: neither is forwarded to the device.
HOLD_WORDS = ("none", "report", "release")

# The aspect and value each capability's device is safe at, where it has
# one (VOCABULARY's `rest` in src/manifest.rs, which a test there checks this
# against). Between two holds at one band, the one for the rest value wins
# (docs/design.md#which-hold-wins).
REST_VALUES = {
    "burner": ("on", False),
}

# The aspect each commandable capability's commands target by default
# (VOCABULARY's `base` in src/manifest.rs, which a test there checks this
# against). A gesture action that releases a target hands back this one
# (docs/design.md#gesture-actions).
BASE_ASPECTS = {
    "burner": "on",
    "climate": "setpoint",
    "light": "on",
    "lock": "locked",
    "notifier": "message",
    "switch": "on",
}

# The furthest a hold may name. A family hold is for tonight or a holiday,
# and an end years out is a typo or a hold nobody will remember.
HOLD_HORIZON = datetime.timedelta(days=366)


def cmd_envelope(
    value: Any,
    priority: str,
    actor: str,
    *,
    cmd_id: str | None = None,
    options: dict | None = None,
    hold: str | None = None,
) -> dict:
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
        The command's value, JSON-encodable. None with ``hold="release"``,
        which carries no value.
    priority : str
        One of `CMD_PRIORITIES`.
    actor : str
        The publishing unit's name.
    cmd_id : str or None, optional
        The correlation id. A random one is generated when not given.
    options : dict or None, optional
        Command options (docs/design.md#command-options), such as
        ``{"transition": 1.0}``. Left out of the envelope when empty.
    hold : str or None, optional
        What the arbiter does with the family hold on the aspect
        (docs/design.md#hold-intents): one of `HOLD_WORDS`, an RFC3339
        instant with an offset to hold until, at most `HOLD_HORIZON` ahead,
        or `"HH:MM"` to hold until the house clock next shows it.
        Only a manual-band command takes one. Left out of the envelope
        when None.

    Returns
    -------
    dict
        The envelope, with `value`, `priority`, `actor`, `id` and, when
        given, `options` and `hold`. A release has no `value`.

    Raises
    ------
    ValueError
        If `options` is not an object with string keys, `hold` is not a
        hold intent or is given below the manual band, or a release
        carries a value.
    """
    envelope = {
        "value": value,
        "priority": priority,
        "actor": actor,
        "id": cmd_id or secrets.token_hex(4),
    }
    if options is not None:
        _check_options(options)
    if options:
        envelope["options"] = options
    if hold is not None:
        _check_hold(hold, priority)
        envelope["hold"] = hold
        if hold == "release":
            if value is not None:
                raise ValueError("a release carries no value")
            del envelope["value"]
    return envelope


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
        The envelope's `value`, or None for a release, which carries none.

    Raises
    ------
    ValueError
        On anything malformed: not an object, missing value or priority, an
        unknown priority, options that are not an object, or a `hold` that
        is not a hold intent or sits below the manual band.
    """
    if not isinstance(payload, dict):
        raise ValueError("cmd payload is not a JSON object")
    if "priority" not in payload:
        raise ValueError("cmd envelope missing priority")
    if payload["priority"] not in CMD_PRIORITIES:
        raise ValueError(f"unknown priority {payload['priority']!r}")
    if "options" in payload:
        _check_options(payload["options"])
    if "hold" in payload:
        _check_hold(payload["hold"], payload["priority"])
    if payload.get("hold") == "release":
        return None
    if "value" not in payload:
        raise ValueError("cmd envelope missing value")
    return payload["value"]


def cmd_envelope_hold(payload: Any) -> str | None:
    """Return the hold intent of a cmd envelope, or None if it has none.

    Call it on an envelope `parse_cmd_envelope` accepted, which has checked
    the intent.

    Parameters
    ----------
    payload : Any
        A decoded cmd payload that passed `parse_cmd_envelope`.

    Returns
    -------
    str or None
        The envelope's `hold`: one of `HOLD_WORDS`, an RFC3339 instant or
        `"HH:MM"`.
    """
    return payload.get("hold")


def is_time_of_day(hold: Any) -> bool:
    """Return whether a hold intent is a time of day, `"HH:MM"` on a 24-hour clock.

    The arbiter holds until the house clock next shows that time
    (docs/design.md#hold-intents), so the sender needs no time zone.

    Parameters
    ----------
    hold : Any
        A hold intent, or anything else.

    Returns
    -------
    bool
        True for `"00:00"` to `"23:59"`.
    """
    # ASCII digits only, as the core reads them (tests/fixtures/time_of_day.json).
    return isinstance(hold, str) and re.fullmatch(r"([01]\d|2[0-3]):[0-5]\d", hold, re.ASCII) is not None


def hold_instant(hold: str) -> datetime.datetime | None:
    """Return the instant a hold intent holds until, or None for a word or a time of day.

    Parameters
    ----------
    hold : str
        A hold intent that `parse_cmd_envelope` or `cmd_envelope` accepted.

    Returns
    -------
    datetime.datetime or None
        The instant, timezone-aware, or None for one of `HOLD_WORDS` or a
        time of day.
    """
    if hold in HOLD_WORDS or is_time_of_day(hold):
        return None
    return datetime.datetime.fromisoformat(hold)


def cmd_envelope_options(payload: Any) -> dict:
    """Return the options of a cmd envelope (docs/design.md#command-options).

    Call it on an envelope `parse_cmd_envelope` accepted, which has checked
    their shape.

    Parameters
    ----------
    payload : Any
        A decoded cmd payload that passed `parse_cmd_envelope`.

    Returns
    -------
    dict
        The envelope's `options`, or an empty dict.
    """
    return payload.get("options", {})


def _check_options(options: Any) -> None:
    if not isinstance(options, dict) or not all(isinstance(k, str) for k in options):
        raise ValueError("cmd envelope options is not an object with string keys")


def _check_hold(hold: Any, priority: Any) -> None:
    # The family hold belongs to the manual band, the band family surfaces
    # command at. A lower band asking to hold or release it is a bug in
    # its publisher, so it is refused rather than ignored.
    if priority != "manual":
        raise ValueError(f"a {priority} command takes no hold")
    if hold in HOLD_WORDS or is_time_of_day(hold):
        return
    if not isinstance(hold, str):
        raise ValueError(f"hold {hold!r} is not a hold intent")
    try:
        instant = datetime.datetime.fromisoformat(hold)
    except ValueError:
        raise ValueError(f"hold {hold!r} is not a hold intent") from None
    # A local time without an offset would be read in the arbiter's zone,
    # which need not be the sender's.
    if instant.tzinfo is None:
        raise ValueError(f"hold {hold!r} has no UTC offset")
    if instant - datetime.datetime.now(datetime.timezone.utc) > HOLD_HORIZON:
        raise ValueError(f"hold {hold!r} is more than {HOLD_HORIZON.days} days ahead")


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
        'state', 'cmd' or 'forecast'. A forecast series key also ends in
        its source.
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


def hold_key(unit: str, room: str, entity: str, aspect: str) -> str:
    """Return the key of what an arbiter holds on one aspect.

    The arbiter publishes one record per held aspect, under its own name,
    and deletes it when the hold ends. The core mirrors them. The preempt
    and refuse events record what happened. These keys answer whether an
    aspect is held right now (docs/design.md#arbitrated-mode).

    Parameters
    ----------
    unit : str
        The arbiter unit's name.
    room : str
        The room the entity is in.
    entity : str
        The entity's name.
    aspect : str
        The held aspect.

    Returns
    -------
    str
        home/hold/{unit}/{room}/{entity}/{aspect}.

    Raises
    ------
    ValueError
        If a part is not a valid key segment.
    """
    return "home/hold/" + _segments(unit, room, entity, aspect)


def hold_keyexpr(unit: str) -> str:
    """Return the key expression matching every hold one arbiter publishes.

    Parameters
    ----------
    unit : str
        The arbiter unit's name.

    Returns
    -------
    str
        home/hold/{unit}/**.

    Raises
    ------
    ValueError
        If a part is not a valid key segment.
    """
    return "home/hold/" + _segments(unit) + "/**"


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
