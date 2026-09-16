# /// script
# requires-python = ">=3.11"
# dependencies = [
#     "homeostat",
# ]
#
# [tool.uv.sources]
# homeostat = { path = "../../../sdk/python", editable = true }
# ///
"""A latch that restores its decision (docs/design.md, Restoring a unit's
own last value).

The fixture for `ctx.restore`. A latch holds what a person decided, so
after a core restart there is nothing to recompute and nothing in the
mirror to replay — the recorder is the only record that the decision was
ever made. The unit reads its own last published value back and republishes
it, at whatever age, before declaring itself ready; with no row to find it
starts on the code default, which is what a house with no history does.
"""

from homeostat import automation, keys

ROOM = "global"
ENTITY = "night_mode"
DEFAULT = False


def main():
    ctx = automation.context()

    def publish(value):
        ctx.publish("state", value, room=ROOM, entity=ENTITY, aspect="on")

    def on_command(key, value):
        try:
            commanded = keys.parse_cmd_envelope(value)
        except ValueError as exc:
            ctx.health_event("invalid-command", key=key, reason=str(exc))
            return
        publish(commanded)

    ctx.subscribe("commands", on_command)
    # Age is irrelevant to a decision: whatever was last decided still is.
    restored = ctx.restore("state", room=ROOM, entity=ENTITY, aspect="on")
    publish(DEFAULT if restored is None else restored[0])
    ctx.ready()
    ctx.run()


if __name__ == "__main__":
    main()
