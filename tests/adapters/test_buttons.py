"""What one gesture action sends (docs/design.md#gesture-actions), without a bus."""

import types
import unittest

import _adapters_path  # noqa: F401
import buttons
from buttons import UnknownState, commands
from homeostat import house as house_mod


class CommandsTest(unittest.TestCase):
    def test_a_toggle_flips_the_known_state(self):
        self.assertEqual(commands({"toggle": "on"}, "light", {"on": False}.get), [("on", True, None)])
        self.assertEqual(commands({"toggle": "on", "hold": "none"}, "light", {"on": True}.get), [("on", False, "none")])

    def test_a_toggle_without_a_boolean_state_sends_nothing(self):
        for state in ({}, {"on": "unknown"}, {"on": 1}):
            with self.subTest(state=state), self.assertRaises(UnknownState):
                commands({"toggle": "on"}, "light", state.get)

    def test_a_set_sends_one_command_per_aspect_with_the_hold(self):
        sent = commands({"set": {"on": True, "brightness": 120}, "hold": "06:00"}, "light", {}.get)
        self.assertEqual(sorted(sent), [("brightness", 120, "06:00"), ("on", True, "06:00")])

    def test_a_release_hands_back_the_base_aspect(self):
        self.assertEqual(commands({"hold": "release"}, "lock", {}.get), [("locked", None, "release")])



class PressTest(unittest.TestCase):
    """The service's press handler, with a fake context."""

    def run_press(self, gesture, publishes, light_state=False):
        light = house_mod.Entity(name="lamp", id="1", capability="light", room="hall", features=["brightness"])
        remote = house_mod.Entity(
            name="remote", id="2", capability="button", room="hall",
            actions={"single": {"lamp": {"set": {"on": True, "brightness": 120}}}},
        )
        model = house_mod.HouseModel(
            zones={}, entities=[light, remote],
            units=[types.SimpleNamespace(name="buttons", publishes=publishes)],
        )
        handlers, sent, events = {}, [], []

        def publish(binding, value, **kw):
            if value == 120:
                raise ValueError("refused")
            sent.append((binding, kw["aspect"], value))

        ctx = types.SimpleNamespace(
            unit="buttons",
            subscribe=lambda binding, handler: handlers.__setitem__(binding, handler),
            publish=publish,
            health_event=lambda kind, **fields: events.append(fields["reason"]),
            ready=lambda: None, run=lambda: None,
        )
        original = (buttons.automation.context, buttons.house.load_house)
        buttons.automation.context, buttons.house.load_house = (lambda: ctx), (lambda root: model)
        try:
            buttons.main()
        finally:
            buttons.automation.context, buttons.house.load_house = original
        handlers["state"]("home/state/hall/lamp/on", light_state)
        handlers["presses"]("home/state/hall/remote/action", gesture)
        return sent, events

    def test_a_refused_send_is_reported_and_the_rest_still_go(self):
        lights = {"lights": {"key": "home/cmd/**", "capability": "light", "priority": "manual"}}
        sent, events = self.run_press("single", lights)
        self.assertEqual(sent, [("lights", "on", True)])
        self.assertEqual(events, ["invalid-action"])

    def test_the_publish_whose_key_covers_the_target_sends(self):
        publishes = {
            "kitchen": {"key": "home/cmd/kitchen/**", "capability": "light", "priority": "manual"},
            "hall": {"key": "home/cmd/hall/lamp/on", "capability": "light", "priority": "manual"},
        }
        sent, events = self.run_press("single", publishes)
        self.assertEqual(sent, [("hall", "on", True)])
        self.assertEqual(events, ["ungranted"], "nothing covers the brightness")

    def test_a_press_that_is_not_a_word_sends_nothing(self):
        lights = {"lights": {"key": "home/cmd/**", "capability": "light", "priority": "manual"}}
        self.assertEqual(self.run_press({"x": 1}, lights), ([], []))


if __name__ == "__main__":
    unittest.main()
