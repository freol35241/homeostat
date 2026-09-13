# /// script
# requires-python = ">=3.11"
# dependencies = [
#     "homeostat",
# ]
#
# [tool.uv.sources]
# homeostat = { path = "../../../sdk/python", editable = true }
# ///
"""Latches: commandable virtual switches (docs/design.md, Commandable
virtual entities), bound through `{room}`/`{entity}` templates.

The unit exists for the SDK's template expansion. It binds two entities in
two rooms through ONE subscribe and ONE publish expression, so a test that
commands either latch and sees its state come back has proved the SDK
expanded both directions the way the core's plan did — the subscribe half
silently matching nothing being the failure this fixture exists to catch.

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
