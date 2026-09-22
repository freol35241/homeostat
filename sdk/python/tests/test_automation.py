"""Key-expression expansion in the automation Context: pure, no bus.

`_expand` is what makes a unit's subscriptions agree with what `plan`
printed for it (src/expand.rs), so these pin the four rules that matter —
templates per bound entity, zones per member room, the two never at once,
and no duplicate expression.

Run: uv run --no-project --with-editable sdk/python python -m unittest discover sdk/python/tests
"""

import types
import unittest
from pathlib import Path

from homeostat.automation import Context, _expand, _house_has_recorder
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

    def test_forecast_expands_like_state(self):
        # A forecast is the same series extended forward, so it is
        # addressed per entity and expands per entity. It did not, while
        # the core did: the mirror of the core's rule had been left behind
        # when the class was added, and a templated forecast publish that
        # `plan` accepted was unaddressable at runtime.
        self.assertEqual(
            _expand("home/forecast/{room}/{entity}/price", ZONES, LATCHES),
            [
                "home/forecast/global/night_mode/price",
                "home/forecast/hallway/motion_lighting/price",
            ],
        )

    def test_a_zone_expands_in_a_forecast_room_slot_too(self):
        self.assertEqual(
            _expand("home/forecast/downstairs/*/price", ZONES, []),
            ["home/forecast/livingroom/*/price", "home/forecast/hallway/*/price"],
        )


def bare_context(publishes, zones=None, entities=None):
    """A Context with only what `_concrete_key` reads. The constructor
    opens a bus session, and key resolution is pure, so it is exercised on
    an uninitialised instance rather than behind a live core."""
    ctx = Context.__new__(Context)
    ctx._publishes = publishes
    ctx._zones = zones if zones is not None else {}
    ctx._entities = entities if entities is not None else []
    return ctx


class ConcreteKeyTest(unittest.TestCase):
    """The manifest is the authority on what a unit publishes, so a
    binding must resolve to one key — for a forecast as for state. It did
    not: a forecast fell through to the branch that returns the expression
    verbatim and refuses slots, so a templated publish `plan` had accepted
    could not be addressed at all."""

    def test_a_literal_forecast_binding_resolves_to_its_key(self):
        ctx = bare_context(
            {"f": {"key": "home/forecast/global/spot_price/price/nordpool"}}
        )
        self.assertEqual(
            ctx._concrete_key("f", room=None, entity=None, aspect=None),
            "home/forecast/global/spot_price/price/nordpool",
        )

    def test_a_templated_forecast_binding_takes_slots(self):
        ctx = bare_context(
            {"f": {"key": "home/forecast/{room}/{entity}/price/planner"}},
            entities=LATCHES,
        )
        self.assertEqual(
            ctx._concrete_key("f", room="hallway", entity="motion_lighting", aspect=None),
            "home/forecast/hallway/motion_lighting/price/planner",
        )

    def test_a_forecast_binding_wildcarding_its_source_needs_one_named(self):
        # The source says WHO claims this future, so a binding that leaves
        # the slot open must have it filled at the call — exactly as an
        # open aspect must (docs/design.md, Sources).
        ctx = bare_context({"f": {"key": "home/forecast/global/spot_price/price/*"}})
        with self.assertRaises(ValueError):
            ctx._concrete_key("f", room=None, entity=None, aspect=None)
        self.assertEqual(
            ctx._concrete_key("f", room=None, entity=None, aspect=None, source="yr"),
            "home/forecast/global/spot_price/price/yr",
        )

    def test_only_a_forecast_publish_takes_a_source(self):
        ctx = bare_context({"s": {"key": "home/state/global/spot_price/price"}})
        with self.assertRaises(ValueError):
            ctx._concrete_key("s", room=None, entity=None, aspect=None, source="yr")

    def test_a_key_outside_the_declared_expression_is_refused(self):
        ctx = bare_context(
            {"f": {"key": "home/forecast/{room}/{entity}/price/planner"}},
            entities=LATCHES,
        )
        with self.assertRaises(ValueError):
            ctx._concrete_key("f", room="kitchen", entity="ghost", aspect=None)

    def test_an_unfilled_slot_is_refused_rather_than_published_wild(self):
        ctx = bare_context({"f": {"key": "home/forecast/{room}/{entity}/price"}})
        with self.assertRaises(ValueError):
            ctx._concrete_key("f", room=None, entity=None, aspect=None)


class RecorderPresenceTest(unittest.TestCase):
    """What `restore` asks before it waits for anything: a recorder-less
    house must start on its defaults at once, not after the timeout."""

    def test_a_house_running_a_recorder_is_recognised(self):
        self.assertTrue(_house_has_recorder(FIXTURES / "fixture_house_restore"))

    def test_a_house_without_one_is_too(self):
        self.assertFalse(_house_has_recorder(FIXTURES / "fixture_house_templates"))


if __name__ == "__main__":
    unittest.main()


class SourceUsedTest(unittest.TestCase):
    """Declared sources say what MAY contribute; `source_used` says what
    did. The SDK remembers the last answer per triple so "on transition"
    is its job and not the producer's — a stream that repeats on every
    tick is one no consumer can fold into intervals."""

    def context(self):
        ctx = Context.__new__(Context)
        ctx._sources_used = {}
        ctx.emitted = []
        ctx._session = types.SimpleNamespace(
            health_event=lambda kind, **f: ctx.emitted.append((kind, f))
        )
        return ctx

    def test_the_first_answer_always_reports(self):
        # A consumer starting mid-window must not read silence as
        # agreement, so the opening state is stated rather than assumed.
        ctx = self.context()
        ctx.source_used("fused", "temperature", "shed", True)
        self.assertEqual(
            ctx.emitted,
            [("source-restored", {"entity": "fused", "aspect": "temperature", "source": "shed"})],
        )

    def test_only_changes_are_reported(self):
        ctx = self.context()
        for used in (True, True, True):
            ctx.source_used("fused", "temperature", "shed", used)
        self.assertEqual(len(ctx.emitted), 1, ctx.emitted)
        ctx.source_used("fused", "temperature", "shed", False)
        ctx.source_used("fused", "temperature", "shed", False)
        self.assertEqual([k for k, _ in ctx.emitted], ["source-restored", "source-dropped"])
        # And back again, because a restored source is the other half of
        # the diagnosis.
        ctx.source_used("fused", "temperature", "shed", True)
        self.assertEqual(
            [k for k, _ in ctx.emitted],
            ["source-restored", "source-dropped", "source-restored"],
        )

    def test_each_triple_is_tracked_apart(self):
        # Two sources of one aspect, and one source of two aspects, are
        # three independent facts.
        ctx = self.context()
        ctx.source_used("fused", "temperature", "shed", True)
        ctx.source_used("fused", "temperature", "kitchen", True)
        ctx.source_used("fused", "humidity", "shed", True)
        self.assertEqual(len(ctx.emitted), 3, ctx.emitted)
        ctx.source_used("fused", "temperature", "shed", False)
        self.assertEqual(
            ctx.emitted[-1],
            ("source-dropped", {"entity": "fused", "aspect": "temperature", "source": "shed"}),
        )
