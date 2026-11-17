# /// script
# requires-python = ">=3.11"
# dependencies = [
#     "homeostat",
# ]
#
# [tool.uv.sources]
# homeostat = { path = "../sdk/python", editable = true }
# ///
"""Buttons service: a button's gestures as family commands.

See docs/design.md#gesture-actions. A `button` entity's file maps
gestures to commands under `[actions.<gesture>]`, keyed by the target
entity. On a press this service sends them at the manual band, as the
dashboard sends a tap:

- `toggle` flips a boolean aspect, from the target's state in the core's
  mirror;
- `set` sends the values given, one command per aspect;
- `hold` rides each command as its hold intent, and `hold = "release"`
  alone hands back the target's base aspect.

A gesture without a table sends nothing. Only presses made while the
service runs count: the SDK's subscribe hands over no mirrored or replayed
press (docs/design.md#buttons), so a restart sends nothing.

The actions are read from the house's files at start. The manifest
watches the house, so an edit restarts the service. Its cmd publishes,
one per capability, say which targets it may command.

Health events at home/health/{unit}/event: `drop` with reason
`ungranted` for a target whose capability no cmd publish covers, and
`unknown-state` for a toggle whose target state is not a known boolean.
"""

from homeostat import automation, house, keys


class UnknownState(LookupError):
    """A toggle's target has no boolean state to flip."""


def commands(action: dict, capability: str, state) -> list[tuple[str, object, str | None]]:
    """Return what one action sends, as (aspect, value, hold) per command.

    `state(aspect)` returns the target's current value for an aspect, or
    None when it is not known. Plan has checked the action's shape.

    Raises
    ------
    UnknownState
        If the action toggles an aspect whose state is not a boolean.
    """
    hold = action.get("hold")
    if "toggle" in action:
        aspect = action["toggle"]
        current = state(aspect)
        if not isinstance(current, bool):
            raise UnknownState(aspect)
        return [(aspect, not current, hold)]
    if "set" in action:
        return [(aspect, value, hold) for aspect, value in action["set"].items()]
    return [(keys.BASE_ASPECTS[capability], None, "release")]


def main():
    ctx = automation.context()
    model = house.load_house(".")
    entities = {e.name: e for e in model.entities}
    own = next(u for u in model.units if u.name == ctx.unit)
    # Target capability -> the cmd publish that may command it.
    bindings = {
        spec["capability"]: name
        for name, spec in own.publishes.items()
        if spec.get("key", "").startswith("home/cmd/") and "capability" in spec
    }
    # (room, button) -> gesture -> target -> action.
    actions = {(e.room, e.name): e.actions for e in model.entities if e.capability == "button" and e.actions}
    states: dict[str, object] = {}

    def on_state(key, value):
        states[key] = value

    def on_press(key, gesture):
        _, _, room, button, _ = key.split("/")
        for name, action in actions.get((room, button), {}).get(gesture, {}).items():
            target = entities.get(name)
            if target is None:
                continue
            binding = bindings.get(target.capability)
            if binding is None:
                ctx.health_event("drop", reason="ungranted", button=button, gesture=gesture, target=name)
                continue
            prefix = f"home/state/{target.room}/{target.name}/"
            try:
                sends = commands(action, target.capability, lambda aspect, prefix=prefix: states.get(prefix + aspect))
            except UnknownState as missing:
                ctx.health_event(
                    "drop", reason="unknown-state", button=button, gesture=gesture,
                    target=name, aspect=str(missing),
                )
                continue
            for aspect, value, hold in sends:
                ctx.publish(binding, value, room=target.room, entity=target.name, aspect=aspect, hold=hold)

    ctx.subscribe("state", on_state)
    ctx.subscribe("presses", on_press)
    ctx.ready()
    ctx.run()


if __name__ == "__main__":
    main()
