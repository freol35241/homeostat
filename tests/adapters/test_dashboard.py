"""The dashboard's model and command checks, without a bus or a browser."""

import os
import sys
import unittest
from pathlib import Path
from unittest import mock

sys.path.insert(0, str(Path(__file__).resolve().parents[2] / "adapters"))

from dashboard import (
    ENV_HOSTS,
    MAX_TEXT_CHARS,
    build_model,
    command_value_ok,
    commandable_aspects,
    descriptor_command,
    driven_aspects,
    host_allowed,
    text_ok,
    unit_relations,
)
from homeostat import house


def entity(name, capability, features=(), room="hall"):
    return {"name": name, "capability": capability, "features": list(features), "room": room}


def grant(unit, names, grant_keys, priority="automation", hold=False, room="hall"):
    return {
        "unit": unit,
        "capability": "light",
        "priority": priority,
        "keys": list(grant_keys),
        "entities": [{"name": name, "room": room} for name in names],
        "hold": hold,
    }


def described(**command):
    """A descriptor with one field, `level`, whose command is `command`."""
    return {"fields": {"level": {"command": dict({"editable_by": "family"}, **command)}}}


LAMP = entity("lamp", "light", ["brightness"])


class BuildModelTest(unittest.TestCase):
    def setUp(self):
        self.house = house.HouseModel(
            zones={"downstairs": ["hall"]},
            units=[
                house.UnitInfo(
                    name="evening_lights",
                    kind="automation",
                    params={"lux": {"default": 50}},
                    subscribes={"motion": "home/state/hall/*/motion"},
                ),
                house.UnitInfo(name="z2m", kind="adapter", naming={"en": "Zigbee"}),
            ],
            entities=[
                house.Entity(
                    name="hall_lamp",
                    id="0x01",
                    capability="light",
                    room="hall",
                    features=["brightness"],
                    owner="z2m",
                    intervals={"energy": 3600},
                ),
                house.Entity(
                    name="hall_temp",
                    id="0x02",
                    capability="sensor",
                    room="hall",
                    naming={"en": "Hall temperature"},
                    sources={
                        "raw": house.SourceRef(entity="probe", aspect="temperature", note="n"),
                    },
                ),
            ],
            views=[{"title": "Home"}],
            controls=[{"control": "setpoint", "step": 0.5}],
        )

    def test_an_entity_is_commandable_when_its_capability_is_granted(self):
        model = build_model(self.house, {"light"})
        self.assertEqual(
            {e["name"]: e["commandable"] for e in model["entities"]},
            {"hall_lamp": True, "hall_temp": False},
        )

    def test_a_label_is_the_english_name_else_the_name_with_spaces(self):
        model = build_model(self.house, set())
        self.assertEqual(
            [e["label"] for e in model["entities"]], ["hall lamp", "Hall temperature"]
        )
        self.assertEqual([u["label"] for u in model["units"]], ["evening lights", "Zigbee"])

    def test_sources_and_intervals_travel_with_their_entity(self):
        lamp, temp = build_model(self.house, set())["entities"]
        self.assertEqual((lamp["sources"], lamp["intervals"]), ({}, {"energy": 3600}))
        self.assertEqual(
            temp["sources"],
            {"raw": {"entity": "probe", "aspect": "temperature", "note": "n", "precision": None}},
        )

    def test_the_house_level_declarations_pass_through(self):
        model = build_model(self.house, set())
        self.assertEqual(model["zones"], {"downstairs": ["hall"]})
        self.assertEqual(model["views"], [{"title": "Home"}])
        self.assertEqual(model["controls"], [{"control": "setpoint", "step": 0.5}])
        self.assertEqual(model["units"][0]["params"], {"lux": {"default": 50}})
        self.assertEqual(model["units"][0]["subscribes"], {"motion": "home/state/hall/*/motion"})


class CommandableAspectsTest(unittest.TestCase):
    def test_the_base_aspect_and_the_declared_vocabulary_features_are_commandable(self):
        self.assertEqual(commandable_aspects(LAMP, None), {"on", "brightness"})
        self.assertEqual(
            commandable_aspects(entity("lamp", "light", ["power"]), None), {"on"}
        )

    def test_a_capability_outside_the_vocabulary_has_none(self):
        self.assertEqual(commandable_aspects(entity("t", "sensor", ["on"]), None), set())

    def test_a_descriptor_adds_the_fields_that_declare_a_command(self):
        descriptor = {"fields": {"mode": {"command": {"type": "enum"}}, "temp": {"label": "t"}}}
        self.assertEqual(
            commandable_aspects(entity("t", "sensor"), descriptor), {"mode"}
        )

    def test_a_malformed_descriptor_adds_nothing(self):
        for descriptor in (
            {},
            {"fields": ["mode"]},
            {"fields": {"mode": "enum"}},
            {"fields": {"mode": {"command": None}}},
        ):
            with self.subTest(descriptor=descriptor):
                self.assertEqual(commandable_aspects(LAMP, descriptor), {"on", "brightness"})


class DrivenAspectsTest(unittest.TestCase):
    def setUp(self):
        self.model = {"entities": [LAMP]}

    def test_an_aspect_maps_to_the_lowest_band_that_reaches_it(self):
        grants = [
            grant("dashboard", ["lamp"], ["home/cmd/hall/lamp/**"], priority="manual"),
            grant("evening", ["lamp"], ["home/cmd/hall/lamp/on"], priority="automation"),
        ]
        self.assertEqual(
            driven_aspects(self.model, grants, {}),
            {"hall/lamp/on": "automation", "hall/lamp/brightness": "manual"},
        )

    def test_a_hold_row_reaches_its_aspects_through_the_units_hold_keys(self):
        grants = [grant("heating", ["lamp"], ["home/hold/heating/hall/lamp/on"], hold=True)]
        self.assertEqual(driven_aspects(self.model, grants, {}), {"hall/lamp/on": "automation"})

    def test_a_descriptor_command_counts_as_an_aspect(self):
        grants = [grant("evening", ["lamp"], ["home/cmd/hall/lamp/**"])]
        descriptors = {"lamp": {"fields": {"effect": {"command": {"type": "enum"}}}}}
        self.assertIn("hall/lamp/effect", driven_aspects(self.model, grants, descriptors))

    def test_malformed_grants_and_rows_are_skipped(self):
        good = grant("evening", ["lamp"], ["home/cmd/hall/lamp/on"])
        malformed = [
            "lamp",
            dict(good, capability=None),
            dict(good, priority=None),
            dict(good, priority=3),
            dict(good, priority="urgent"),
            dict(good, keys=["home/**/**", "home/cmd/$x"]),
            dict(good, entities=["lamp", {"name": 3}, {"name": "ghost", "room": "hall"}]),
            dict(good, entities=[{"name": "lamp"}]),
        ]
        for row in malformed:
            with self.subTest(row=row):
                self.assertEqual(driven_aspects(self.model, [row], {}), {})
        self.assertEqual(
            driven_aspects(self.model, [*malformed, good], {}), {"hall/lamp/on": "automation"}
        )


class UnitRelationsTest(unittest.TestCase):
    def relations(self, subscribes=None, grants=(), state_keys=(), descriptors=None):
        model = {
            "zones": {"downstairs": ["hall", "kitchen"]},
            "entities": [LAMP, entity("pir", "occupancy"), entity("kt", "sensor", room="kitchen")],
            "units": [{"name": "evening", "subscribes": subscribes or {}}],
        }
        return unit_relations(model, list(grants), list(state_keys), descriptors or {})["evening"]

    def test_drives_lists_each_aspect_the_units_grant_reaches(self):
        drives = self.relations(grants=[grant("evening", ["lamp"], ["home/cmd/hall/lamp/on"])])
        self.assertEqual(drives["drives"], [{"entity": "lamp", "aspect": "on"}])

    def test_another_units_grant_is_not_this_units(self):
        drives = self.relations(grants=[grant("night", ["lamp"], ["home/cmd/hall/lamp/on"])])
        self.assertEqual(drives["drives"], [])

    def test_an_entity_with_no_known_aspects_keeps_a_bare_row(self):
        drives = self.relations(grants=[grant("evening", ["ghost"], ["home/cmd/hall/ghost/**"])])
        self.assertEqual(drives["drives"], [{"entity": "ghost", "aspect": None}])

    def test_sources_are_the_state_keys_a_subscription_reaches(self):
        relations = self.relations(
            subscribes={"motion": "home/state/hall/*/motion"},
            state_keys=["home/state/hall/pir/motion", "home/state/hall/pir/battery"],
        )
        self.assertEqual(relations["sources"], [{"entity": "pir", "aspect": "motion"}])

    def test_a_zone_in_the_room_slot_expands_to_its_rooms(self):
        relations = self.relations(
            subscribes={"t": "home/state/downstairs/*/temperature"},
            state_keys=[
                "home/state/hall/pir/temperature",
                "home/state/kitchen/kt/temperature",
                "home/state/attic/at/temperature",
            ],
        )
        self.assertEqual(
            relations["sources"],
            [{"entity": "kt", "aspect": "temperature"}, {"entity": "pir", "aspect": "temperature"}],
        )

    def test_a_template_or_a_non_state_subscription_is_no_source(self):
        relations = self.relations(
            subscribes={
                "own": "home/state/{room}/{entity}/motion",
                "cmd": "home/cmd/hall/pir/motion",
                "short": "home/state/hall",
                "bad": "home/state/hall/**/**",
                "odd": 3,
            },
            state_keys=["home/state/hall/pir/motion"],
        )
        self.assertEqual(relations["sources"], [])

    def test_malformed_grants_and_rows_are_skipped(self):
        good = grant("evening", ["lamp"], ["home/cmd/hall/lamp/on"])
        relations = self.relations(
            grants=[
                "lamp",
                dict(good, capability=None),
                dict(good, entities=["lamp", {"name": 3}]),
                good,
            ]
        )
        self.assertEqual(relations["drives"], [{"entity": "lamp", "aspect": "on"}])


class DescriptorCommandTest(unittest.TestCase):
    def test_a_family_command_is_returned_with_the_fields_values(self):
        values = [{"value": "eco", "label": "eco"}]
        descriptor = {
            "fields": {
                "mode": {
                    "values": values,
                    "command": {"type": "enum", "editable_by": "family"},
                }
            }
        }
        self.assertEqual(
            descriptor_command(descriptor, "mode"),
            {"type": "enum", "editable_by": "family", "values": values},
        )
        self.assertEqual(descriptor_command(described(type="float"), "level")["values"], [])

    def test_no_command_for_an_undescribed_field_or_another_tier(self):
        for descriptor, aspect in (
            (None, "level"),
            ({}, "level"),
            (described(type="float"), "other"),
            ({"fields": {"level": "float"}}, "level"),
            ({"fields": {"level": {"command": "float"}}}, "level"),
            ({"fields": {"level": {"command": {"type": "float"}}}}, "level"),
            (described(type="float", editable_by="owner"), "level"),
        ):
            with self.subTest(descriptor=descriptor, aspect=aspect):
                self.assertIsNone(descriptor_command(descriptor, aspect))

    def test_a_non_numeric_bound_means_no_command(self):
        for command in (
            {"step": "1"},
            {"step": True},
            {"constraint": {"min": "0"}},
            {"constraint": {"max": [10]}},
        ):
            with self.subTest(command=command):
                self.assertIsNone(descriptor_command(described(type="float", **command), "level"))
        self.assertIsNotNone(
            descriptor_command(
                described(type="float", step=0.5, constraint={"min": 0, "max": 10}), "level"
            )
        )

    # descriptor_command reads `fields` with .get without checking it is an
    # object, so a list raises where commandable_aspects reads no fields.
    @unittest.expectedFailure
    def test_a_descriptor_whose_fields_are_not_an_object_has_no_command(self):
        self.assertIsNone(descriptor_command({"fields": ["level"]}, "level"))

    # descriptor_command passes a non-object constraint through, and
    # command_value_ok raises on it: the 500 its bound check exists to avoid.
    @unittest.expectedFailure
    def test_no_command_it_returns_makes_the_value_check_raise(self):
        for constraint in ([0, 10], "0-10"):
            with self.subTest(constraint=constraint):
                command = descriptor_command(described(type="float", constraint=constraint), "level")
                self.assertIsInstance(command is None or command_value_ok(command, 5), bool)


class CommandValueOkTest(unittest.TestCase):
    def test_an_enum_value_must_be_one_of_its_values(self):
        command = {"type": "enum", "values": ["eco", {"value": "eco"}, {"value": 2}]}
        self.assertTrue(command_value_ok(command, "eco"))
        self.assertTrue(command_value_ok(command, 2))
        self.assertFalse(command_value_ok(command, "boost"))
        self.assertFalse(command_value_ok({"type": "enum", "values": ["eco"]}, "eco"))

    def test_a_bool_must_be_a_bool(self):
        self.assertTrue(command_value_ok({"type": "bool"}, False))
        for value in (0, 1, "true", None):
            with self.subTest(value=value):
                self.assertFalse(command_value_ok({"type": "bool"}, value))

    def test_a_number_must_be_finite_and_within_its_constraint(self):
        command = {"type": "float", "constraint": {"min": 0, "max": 10}}
        for value in (0, 2.5, 10):
            with self.subTest(value=value):
                self.assertTrue(command_value_ok(command, value))
        for value in (-1, 10.5, True, "5", None, float("nan"), float("inf")):
            with self.subTest(value=value):
                self.assertFalse(command_value_ok(command, value))
        self.assertTrue(command_value_ok({"type": "float"}, -1e9))
        self.assertTrue(command_value_ok({"type": "float", "constraint": None}, 3))

    def test_an_int_must_be_integral(self):
        self.assertTrue(command_value_ok({"type": "int"}, 3))
        self.assertTrue(command_value_ok({"type": "int"}, 3.0))
        self.assertFalse(command_value_ok({"type": "int"}, 3.5))

    def test_any_other_type_admits_nothing(self):
        for command in ({"type": "text"}, {"type": "json"}, {}):
            with self.subTest(command=command):
                self.assertFalse(command_value_ok(command, "hello"))


class TextOkTest(unittest.TestCase):
    def test_text_is_a_string_that_is_not_blank_and_not_too_long(self):
        self.assertTrue(text_ok("Dinner is ready"))
        self.assertTrue(text_ok("x" * MAX_TEXT_CHARS))
        for value in ("", "  \n", "x" * (MAX_TEXT_CHARS + 1), 5, None, ["hi"]):
            with self.subTest(value=value):
                self.assertFalse(text_ok(value))


class HostAllowedTest(unittest.TestCase):
    def setUp(self):
        patcher = mock.patch.dict(os.environ)
        patcher.start()
        self.addCleanup(patcher.stop)
        os.environ.pop(ENV_HOSTS, None)

    def test_a_known_name_is_allowed_with_or_without_a_port(self):
        for host in ("localhost", "homeostat:8600", "homeostat.lan", "homeostat.local:80"):
            with self.subTest(host=host):
                self.assertTrue(host_allowed(host))

    def test_a_house_network_address_is_allowed(self):
        for host in ("10.8.0.1", "192.168.1.20:8600", "127.0.0.1", "[::1]:8600", "[fd12::1]", "[fe80::1]"):
            with self.subTest(host=host):
                self.assertTrue(host_allowed(host))

    def test_a_public_address_or_another_name_is_refused(self):
        for host in ("", "8.8.8.8", "100.64.0.1", "[2001:db8::1]:8600", "homeostat.example.com"):
            with self.subTest(host=host):
                self.assertFalse(host_allowed(host))

    def test_a_port_is_stripped_only_when_it_is_digits(self):
        self.assertFalse(host_allowed("localhost:http"))

    def test_the_extra_hosts_variable_adds_names(self):
        os.environ[ENV_HOSTS] = " home.example.com , ,proxy "
        self.assertTrue(host_allowed("home.example.com:443"))
        self.assertTrue(host_allowed("proxy"))
        self.assertFalse(host_allowed("example.com"))
        self.assertFalse(host_allowed(""))


if __name__ == "__main__":
    unittest.main()
