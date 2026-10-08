# /// script
# requires-python = ">=3.11"
# dependencies = [
#     "homeostat",
# ]
#
# [tool.uv.sources]
# homeostat = { path = "../../../../sdk/python", editable = true }
# ///
"""Latches: commandable virtual switches bound through templates.

See docs/design.md#commandable-virtual-entities. The unit binds through
`{room}`/`{entity}` templates and exists to test the SDK's template
expansion. It binds two entities in two rooms through one subscribe and
one publish expression. A test that commands either latch and sees its
state come back has shown the SDK expanded both directions the way the
core's plan did. The failure this fixture catches is the subscribe half
matching nothing without an error.

A latch holds what it was told: the command's value becomes the state.
"""

from homeostat import automation, keys


def main():
    ctx = automation.context()

    def on_command(key, value):
        _, _, room, entity, aspect = key.split("/")
        try:
            commanded = keys.parse_cmd_envelope(value)
        except ValueError as exc:
            ctx.health_event("invalid-command", key=key, reason=str(exc))
            return
        ctx.publish("state", commanded, room=room, entity=entity, aspect=aspect)

    ctx.subscribe("commands", on_command)
    ctx.ready()
    ctx.run()


if __name__ == "__main__":
    main()
