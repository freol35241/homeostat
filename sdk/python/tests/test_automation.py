"""Key-expression expansion in the automation Context: pure, no bus.

`_expand` is what makes a unit's subscriptions agree with what `plan`
printed for it (src/expand.rs), so these pin the four rules that matter —
templates per bound entity, zones per member room, the two never at once,
and no duplicate expression.

Run: uv run --no-project --with-editable sdk/python python -m unittest discover sdk/python/tests
"""

import unittest
from pathlib import Path

from homeostat.automation import _expand, _house_has_recorder
from homeostat.house import Entity

FIXTURES = Path(__file__).resolve().parents[3] / "tests"

ZONES = {"downstairs": ["livingroom", "hallway"]}


def entity(name, room, write_mode="exclusive"):
    return Entity(
        name=name, id=name, capability="switch", room=room, write_mode=write_mode
    )


LATCHES = [entity("night_mode", "global"), entity("motion_lighting", "hallway")]


class TemplateTest(unittest.TestCase):
    def test_both_slots_expand_per_bound_entity(self):
        self.assertEqual(
            _expand("home/state/{room}/{entity}/on", ZONES, LATCHES),
            ["home/state/global/night_mode/on", "home/state/hallway/motion_lighting/on"],
        )

    def test_a_subscribe_expression_keeps_its_wildcard_tail(self):
        """The half that failed silently: zenoh takes `{room}` as a literal
        chunk, so the unexpanded expression matches no key that exists."""
        self.assertEqual(
            _expand("home/cmd/{room}/{entity}/**", ZONES, LATCHES),
            ["home/cmd/global/night_mode/**", "home/cmd/hallway/motion_lighting/**"],
        )

    def test_a_room_only_template_does_not_repeat_a_shared_room(self):
        upstairs = [entity("a", "bedroom"), entity("b", "bedroom")]
        self.assertEqual(
            _expand("home/state/{room}/thermo/temperature", ZONES, upstairs),
            ["home/state/bedroom/thermo/temperature"],
        )

    def test_a_templated_cmd_skips_arbitrated_entities(self):
        """Their cmd path belongs to the arbiter, as in the core."""
        mixed = [entity("latch", "global"), entity("pump", "boiler", "arbitrated")]
        self.assertEqual(
            _expand("home/cmd/{room}/{entity}/**", ZONES, mixed),
            ["home/cmd/global/latch/**"],
        )
        self.assertEqual(
            _expand("home/state/{room}/{entity}/**", ZONES, mixed),
            ["home/state/global/latch/**", "home/state/boiler/pump/**"],
        )

    def test_binding_nothing_expands_to_nothing(self):
        self.assertEqual(_expand("home/state/{room}/{entity}/on", ZONES, []), [])


class ZoneTest(unittest.TestCase):
    def test_a_zone_in_the_room_slot_expands_per_member_room(self):
        self.assertEqual(
            _expand("home/state/downstairs/*/temperature", ZONES, []),
            [
                "home/state/livingroom/*/temperature",
                "home/state/hallway/*/temperature",
            ],
        )

    def test_a_plain_room_is_left_alone(self):
        expr = "home/state/office/thermo/temperature"
        self.assertEqual(_expand(expr, ZONES, []), [expr])

    def test_a_non_entity_class_never_expands(self):
        expr = "home/discovery/z2m"
        self.assertEqual(_expand(expr, ZONES, LATCHES), [expr])


class RecorderPresenceTest(unittest.TestCase):
    """What `restore` asks before it waits for anything: a recorder-less
    house must start on its defaults at once, not after the timeout."""

    def test_a_house_running_a_recorder_is_recognised(self):
        self.assertTrue(_house_has_recorder(FIXTURES / "fixture_house_restore"))

    def test_a_house_without_one_is_too(self):
        self.assertFalse(_house_has_recorder(FIXTURES / "fixture_house_templates"))


if __name__ == "__main__":
    unittest.main()
