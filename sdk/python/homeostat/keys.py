"""Key builders for the homeostat key space (mirrors the Rust src/bus.rs).

Schema: home/{class}/{room}/{entity}/{aspect} for state and cmd;
home/health/{unit}[...] and home/meta/{unit}/... for supervision.
"""

ENV_UNIT = "HOMEOSTAT_UNIT"
ENV_BUS = "HOMEOSTAT_BUS"


def state_key(room: str, entity: str, aspect: str) -> str:
    return f"home/state/{room}/{entity}/{aspect}"


def cmd_key(room: str, entity: str, aspect: str) -> str:
    return f"home/cmd/{room}/{entity}/{aspect}"


def cmd_keyexpr(room: str, entity: str) -> str:
    """Key expression matching every command aspect of one entity."""
    return f"home/cmd/{room}/{entity}/**"


def config_key(unit: str, param: str) -> str:
    """Core-owned live parameter value (see docs/design.md, step 4)."""
    return f"home/config/{unit}/{param}"


def config_keyexpr(unit: str) -> str:
    """Key expression matching every parameter of one unit."""
    return f"home/config/{unit}/*"


CLOCK_MINUTE = "home/clock/minute"
CLOCK_DATE = "home/clock/date"


def history_key(space: str, entity: str, aspect: str) -> str:
    """History series key: entity-first (entity is the series identity,
    room is a tag carried per row). `space` is 'state' or 'cmd'."""
    return f"home/history/{space}/{entity}/{aspect}"


def discovery_key(unit: str) -> str:
    """An adapter's complete current view of its periphery: one JSON array
    of device records (see docs/design.md, Discovery)."""
    return f"home/discovery/{unit}"


def liveliness_key(unit: str) -> str:
    return f"home/health/{unit}/alive"


def health_event_key(unit: str) -> str:
    """Unit-published JSON events (e.g. dropped payloads); the parent key
    home/health/{unit} itself belongs to the supervisor."""
    return f"home/health/{unit}/event"
