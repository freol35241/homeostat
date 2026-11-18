"""The core's vocabulary tables, as the SDK and the adapters read them.

Generated from src/manifest.rs and src/keyspace.rs by `sdk_vocabulary` in
src/schema.rs; do not edit (a test refuses a stale copy). Regenerate with
`HOMEOSTAT_BLESS=1 cargo test --lib sdk_vocabulary`. docs/manifest.md
renders the same vocabulary for people.
"""

# Command priority bands, lowest to highest (`Priority::ALL`).
CMD_PRIORITIES = ("automation", "agent", "family", "manual")

# The words a manual-band command's `hold` may carry besides an instant or
# "HH:MM" (`HOLD_WORDS`).
HOLD_WORDS = ("none", "report", "release")

# The classes addressed per entity, as home/{class}/{room}/{entity}/{aspect}
# (`ENTITY_ADDRESSED` in src/keyspace.rs).
ENTITY_ADDRESSED = ("state", "cmd", "arbiter", "forecast")

# The aspect each capability's commands target by default (`base`).
BASE_ASPECTS = {
    "burner": "on",
    "climate": "setpoint",
    "light": "on",
    "lock": "locked",
    "notifier": "message",
    "switch": "on",
}

# The aspects that take a command, per capability, with the type of value
# each carries (`commands`, `ValueType`). A feature takes commands only on
# an entity that declares it.
COMMANDS = {
    "burner": {"on": "bool", "power_level": "number"},
    "climate": {"setpoint": "number"},
    "light": {"on": "bool", "brightness": "number", "color_temp": "number"},
    "lock": {"locked": "bool"},
    "notifier": {"message": "text", "alert": "text"},
    "switch": {"on": "bool", "runtime_s": "number"},
}

# The aspect and value each capability's device is safe at (`rest`).
REST_VALUES = {
    "burner": ("on", False),
}

# The command options per capability, with the aspects whose commands take
# each (`options`).
OPTIONS = {
    "light": {"transition": ("on", "brightness", "color_temp")},
}

# The aspects a room sensor publishes, whichever its device observes
# (the `occupancy` row's `aspects`).
OCCUPANCY_ASPECTS = ("motion", "occupied")

# A light's lowest and highest `brightness` (`BRIGHTNESS_RANGE`).
BRIGHTNESS_RANGE = (0, 254)

# The parameter that pauses an automation, and its spec when the manifest
# does not declare it (`ENABLED_PARAM`, `implicit_enabled`).
ENABLED_PARAM = "enabled"
IMPLICIT_ENABLED = {"type": "bool", "default": True, "editable_by": "family"}
