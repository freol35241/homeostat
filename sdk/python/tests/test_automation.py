"""Key-expression expansion in the automation Context: pure, no bus.

`_expand` is what makes a unit's subscriptions agree with what `plan`
printed for it (src/expand.rs), so these pin the four rules that matter —
templates per bound entity, zones per member room, the two never at once,
and no duplicate expression.

Run: uv run --no-project --with-editable sdk/python python -m unittest discover sdk/python/tests
"""

import datetime
import threading
import types
import unittest
from pathlib import Path

from homeostat import house
from homeostat.automation import (
    Context,
    HistoryUnavailable,
    _expand,
    _house_has_recorder,
)
from homeostat.house import Entity
from homeostat.session import QueryError, QueryTimeout

FIXTURES = Path(__file__).resolve().parents[3] / "tests" / "fixtures"

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
        """Unexpanded, this fails silently: zenoh takes `{room}` as a
        literal chunk, so the expression matches no key that exists."""
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
        # addressed per entity and expands per entity, as the core does: a
        # templated forecast publish that `plan` accepts must be
        # addressable at runtime.
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
    binding must resolve to one key — for a forecast as for state. A
    forecast that fell through to the branch that returns the expression
    verbatim and refuses slots would leave a templated publish `plan` had
    accepted unaddressable."""

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
        # open aspect must (docs/design.md#forecasts).
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


class PublishOptionsTest(unittest.TestCase):
    """Command options ride in the envelope beside the value
    (docs/design.md#command-options). Only a command carries them."""

    def context(self):
        ctx = bare_context(
            {
                "lamp": {"key": "home/cmd/hall/lamp/brightness", "priority": "automation"},
                "level": {"key": "home/state/hall/dimmer/level"},
            }
        )
        ctx.unit = "pir_dim"
        ctx._lock = threading.Lock()
        ctx._param_specs = {}
        ctx.puts = []
        ctx._session = types.SimpleNamespace(put_json=lambda key, value: ctx.puts.append((key, value)))
        return ctx

    def test_a_command_carries_its_options(self):
        ctx = self.context()
        ctx.publish("lamp", 127, options={"transition": 1.0})
        [(key, envelope)] = ctx.puts
        self.assertEqual(key, "home/cmd/hall/lamp/brightness")
        self.assertEqual(envelope["value"], 127)
        self.assertEqual(envelope["options"], {"transition": 1.0})

    def test_a_command_without_options_has_no_options_field(self):
        ctx = self.context()
        ctx.publish("lamp", 127)
        self.assertNotIn("options", ctx.puts[0][1])

    def test_options_on_a_state_publish_are_refused(self):
        ctx = self.context()
        with self.assertRaises(ValueError):
            ctx.publish("level", 3, options={"transition": 1.0})
        self.assertEqual(ctx.puts, [])

    def test_options_must_be_an_object_with_string_keys(self):
        ctx = self.context()
        for options in ([("transition", 1)], {1: 1.0}, [], ""):
            with self.assertRaises(ValueError, msg=repr(options)):
                ctx.publish("lamp", 127, options=options)
        self.assertEqual(ctx.puts, [])

    def test_bad_options_raise_while_paused(self):
        # The mistake is in the automation's code, so it shows at once,
        # not on the first publish after the automation is resumed.
        ctx = self.context()
        ctx._param_specs = {house.ENABLED_PARAM: {}}
        ctx._param_values = {house.ENABLED_PARAM: False}
        with self.assertRaises(ValueError):
            ctx.publish("lamp", 127, options=[1])
        ctx.publish("lamp", 127, options={"transition": 1.0})
        self.assertEqual(ctx.puts, [])


class RecorderPresenceTest(unittest.TestCase):
    """What `restore` asks before it waits for anything: a recorder-less
    house must start on its defaults at once, not after the timeout."""

    def test_a_house_running_a_recorder_is_recognised(self):
        self.assertTrue(_house_has_recorder(FIXTURES / "house_restore"))

    def test_a_house_without_one_is_too(self):
        self.assertFalse(_house_has_recorder(FIXTURES / "house_templates"))


class RestoreWaitTest(unittest.TestCase):
    """How `restore` waits for the recorder. The recorder answers one
    query at a time, so a get that gives up and asks again queues a second
    answer behind the first: once one answer takes longer than the get, no
    asker is ever still there for a reply. `restore` must ask once and
    wait, and say which of the two ways the recorder failed it."""

    KEY = "home/state/global/night_mode/on"

    def context(self, get_json):
        ctx = Context.__new__(Context)
        ctx.emitted = []
        ctx.asked = []
        ctx._concrete_key = lambda binding, **slots: self.KEY
        ctx._has_recorder = lambda: True

        def get(selector, timeout_s=None):
            ctx.asked.append((selector, timeout_s))
            return get_json(selector, timeout_s)

        ctx._session = types.SimpleNamespace(
            get_json=get,
            health_event=lambda kind, **f: ctx.emitted.append((kind, f)),
        )
        return ctx

    def test_a_slow_answer_is_waited_for_not_asked_again(self):
        # A stats answer slower than a short 2 s get would wait for.
        answer_s = 2.5

        def recorder(selector, timeout_s):
            if selector == "home/history/stats":
                if timeout_s < answer_s:
                    raise QueryTimeout("Timeout")
                return [(selector, {})]
            return [(selector, [{"ts": "2026-01-01T00:00:00+00:00", "value": True}])]

        ctx = self.context(recorder)
        restored = ctx.restore("state", timeout_s=30.0)
        self.assertIsNotNone(restored)
        self.assertIs(restored[0], True)
        stats = [t for sel, t in ctx.asked if sel == "home/history/stats"]
        self.assertEqual(len(stats), 1, "one question, not a queue of them")
        self.assertEqual(ctx.emitted, [])

    def test_nobody_answering_is_asked_again_until_the_deadline(self):
        # No recorder up yet: a get nobody serves returns at once, empty.
        ctx = self.context(lambda selector, timeout_s: [])
        self.assertIsNone(ctx.restore("state", timeout_s=0.5))
        self.assertGreater(len(ctx.asked), 1)
        self.assertEqual(
            ctx.emitted,
            [
                (
                    "restore-failed",
                    {"key": self.KEY, "reason": "no recorder answered within 0.5s"},
                )
            ],
        )

    def test_a_recorder_that_took_the_query_and_never_answered_is_named(self):
        def silent(selector, timeout_s):
            raise QueryTimeout("Timeout")

        ctx = self.context(silent)
        self.assertIsNone(ctx.restore("state", timeout_s=0.5))
        self.assertEqual(len(ctx.asked), 1)
        self.assertEqual(
            ctx.emitted,
            [
                (
                    "restore-failed",
                    {
                        "key": self.KEY,
                        "reason": "recorder took the query but did not answer within 0.5s",
                    },
                )
            ],
        )


ROW = {"ts": "2026-11-06T07:00:00.000000+00:00", "room": "garden", "value": 0.4}


class HistoryTest(unittest.TestCase):
    """`history` reads the past of a subscribed state key. It goes through
    the same recorder wait as `restore`, but it raises where `restore`
    returns None: an empty window must mean that nothing was recorded."""

    UTC = datetime.timezone.utc
    START = datetime.datetime(2026, 11, 6, 6, 0, tzinfo=UTC)
    END = datetime.datetime(2026, 11, 7, 6, 0, tzinfo=UTC)

    def context(self, answer, recorder=True, stats=lambda: True):
        ctx = Context.__new__(Context)
        ctx.asked = []
        ctx._subscribes = {
            "rain": "home/state/garden/rain_gauge/precipitation",
            "temperatures": "home/state/downstairs/*/temperature",
            "commands": "home/cmd/global/night_mode/**",
        }
        ctx._zones = ZONES
        ctx._entities = []
        ctx._has_recorder = lambda: recorder

        def get(selector, timeout_s=None):
            ctx.asked.append(selector)
            if selector == "home/history/stats":
                return [(selector, {})] if stats() else []
            return answer(selector)

        ctx._session = types.SimpleNamespace(get_json=get)
        return ctx

    def test_a_bound_keeps_its_microseconds(self):
        ctx = self.context(lambda selector: [(selector, [])])
        ctx.history("rain", self.START.replace(microsecond=123456), self.END)
        self.assertIn("from=2026-11-06T06:00:00.123456+00:00;", ctx.asked[-1])

    def test_the_window_is_read_in_utc_and_its_stamps_parsed(self):
        ctx = self.context(lambda selector: [(selector, [ROW])])
        local = datetime.timezone(datetime.timedelta(hours=1))
        rows = ctx.history("rain", self.START.astimezone(local), self.END, bucket_s=3600)
        self.assertEqual(
            ctx.asked[-1],
            "home/history/state/rain_gauge/precipitation"
            "?from=2026-11-06T06:00:00+00:00;to=2026-11-07T06:00:00+00:00"
            ";limit=10000;bucket=3600",
        )
        self.assertEqual(
            rows,
            [{**ROW, "ts": datetime.datetime(2026, 11, 6, 7, 0, tzinfo=self.UTC)}],
        )

    def test_a_series_with_nothing_recorded_is_empty(self):
        ctx = self.context(lambda selector: [])
        self.assertEqual(ctx.history("rain", self.START, self.END), [])

    def test_the_slots_must_land_inside_the_subscription(self):
        ctx = self.context(lambda selector: [(selector, [])])
        ctx.history("temperatures", self.START, self.END, room="hallway", entity="hall_temp")
        self.assertTrue(ctx.asked[-1].startswith("home/history/state/hall_temp/temperature?"))
        for binding, slots, error in (
            ("temperatures", {"entity": "hall_temp"}, ValueError),
            ("temperatures", {"room": "attic", "entity": "attic_temp"}, ValueError),
            ("commands", {"aspect": "on"}, ValueError),
            ("lamp", {}, KeyError),
        ):
            with self.subTest(binding=binding, slots=slots), self.assertRaises(error):
                ctx.history(binding, self.START, self.END, **slots)

    def test_bad_bounds_and_buckets_are_refused_before_asking(self):
        ctx = self.context(lambda selector: [])
        naive = self.START.replace(tzinfo=None)
        cases = (
            (naive, None),
            (self.END + datetime.timedelta(seconds=1), None),
            (self.START, 0),
            (self.START, True),
            (self.START, 1.5),
        )
        for start, bucket_s in cases:
            with self.subTest(start=start, bucket_s=bucket_s), self.assertRaises(ValueError):
                ctx.history("rain", start, self.END, bucket_s=bucket_s)
        self.assertEqual(ctx.asked, [])

    def test_what_cannot_be_read_raises(self):
        def refused(selector):
            raise QueryError("store unavailable: disk I/O error")

        def slow(selector):
            raise QueryTimeout("Timeout")

        # Up for the wait, gone for the read and the second look.
        answers = iter([True, False])
        stopped = self.context(lambda selector: [], stats=lambda: next(answers))
        cases = (
            (self.context(lambda selector: [], recorder=False), "runs no recorder"),
            (self.context(refused), "store unavailable"),
            (self.context(slow), "Timeout"),
            (stopped, "stopped answering"),
            (self.context(lambda selector: [(selector, [ROW] * 10_000)]), "pass bucket_s"),
        )
        for ctx, reason in cases:
            with self.subTest(reason=reason), self.assertRaisesRegex(HistoryUnavailable, reason):
                ctx.history("rain", self.START, self.END)

    def test_a_recorder_that_never_answers_raises_with_its_reason(self):
        ctx = self.context(lambda selector: [])
        ctx._session.get_json = lambda selector, timeout_s=None: []
        with self.assertRaisesRegex(HistoryUnavailable, "no recorder answered within 0.3s"):
            ctx.history("rain", self.START, self.END, timeout_s=0.3)

    def test_a_full_set_of_buckets_is_not_a_cut_window(self):
        bucket = {**ROW, "sum": 0.4, "n": 1}
        ctx = self.context(lambda selector: [(selector, [bucket] * 10_000)])
        self.assertEqual(len(ctx.history("rain", self.START, self.END, bucket_s=60)), 10_000)


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


if __name__ == "__main__":
    unittest.main()
