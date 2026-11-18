"""The ESPHome translation, without a device: descriptors, states and commands."""

import threading
import unittest
from types import SimpleNamespace
from unittest import mock

import _adapters_path  # noqa: F401
from aioesphomeapi import (
    BinarySensorInfo,
    BinarySensorState,
    Event,
    EventInfo,
    LightInfo,
    LightState,
    NumberInfo,
    NumberState,
    SensorInfo,
    SensorState,
    SwitchInfo,
    SwitchState,
)
from esphome import aspect_descriptor, cmd_handler, state_values
from homeostat import keys
from homeostat.testing import FakeSample, FakeSession


def entity(name, capability, features=(), room="hall"):
    return SimpleNamespace(
        name=name, room=room, capability=capability, features=list(features), id=f"dev/{name}"
    )


def fields(entity_, info):
    return aspect_descriptor(entity_, info)["fields"]


class AspectDescriptorTest(unittest.TestCase):
    def test_a_switch_has_one_boolean_field(self):
        descriptor = aspect_descriptor(entity("pump", "switch"), SwitchInfo(object_id="pump"))
        self.assertEqual(
            descriptor,
            {
                "schema": 1,
                "groups": ["readings"],
                "fields": {"on": {"label": "on", "kind": "boolean", "group": "readings"}},
            },
        )

    def test_a_light_describes_the_features_it_declares(self):
        info = LightInfo(object_id="lamp")
        self.assertEqual(list(fields(entity("lamp", "light"), info)), ["on"])
        self.assertEqual(
            list(fields(entity("lamp", "light", ["brightness", "color_temp"]), info)),
            ["on", "brightness", "color_temp"],
        )

    def test_a_sensors_unit_picks_its_kind(self):
        sensor = entity("t", "sensor")
        for unit, kind in (("°C", "temperature"), ("%", "percent"), ("", "number")):
            with self.subTest(unit=unit):
                info = SensorInfo(object_id="t", device_class="temperature", unit_of_measurement=unit)
                field = fields(sensor, info)["temperature"]
                self.assertEqual(field["kind"], kind)
                self.assertNotIn("unit", field)
        info = SensorInfo(object_id="power", unit_of_measurement="W")
        self.assertEqual(fields(sensor, info)["power"]["unit"], "W")

    def test_the_devices_name_is_the_label_with_the_aspect_when_they_differ(self):
        sensor = entity("t", "sensor")
        info = SensorInfo(object_id="t", name="Outdoor Temperature", device_class="temperature")
        self.assertEqual(fields(sensor, info)["temperature"]["label"], "outdoor Temperature (temperature)")
        info = SensorInfo(object_id="t", name="Temperature", device_class="temperature")
        self.assertEqual(fields(sensor, info)["temperature"]["label"], "temperature")

    def test_an_alarm_like_binary_sensor_is_notable(self):
        sensor = entity("s", "binary_sensor")
        self.assertTrue(fields(sensor, BinarySensorInfo(object_id="s", device_class="smoke"))["smoke"]["notable"])
        self.assertNotIn("notable", fields(sensor, BinarySensorInfo(object_id="s", device_class="door"))["door"])

    def test_a_room_sensor_says_occupied_or_motion(self):
        room = entity("pir", "occupancy")
        self.assertIn("occupied", fields(room, BinarySensorInfo(object_id="p", device_class="presence")))
        self.assertIn("motion", fields(room, BinarySensorInfo(object_id="p", device_class="motion")))

    def test_a_buttons_actions_are_its_event_types(self):
        info = EventInfo(object_id="b", name="Button", event_types=["single", "double"])
        field = fields(entity("b", "button"), info)["action"]
        self.assertEqual((field["kind"], [v["value"] for v in field["values"]]), ("enum", ["single", "double"]))

    def test_an_info_that_does_not_fit_the_capability_has_no_descriptor(self):
        self.assertIsNone(aspect_descriptor(entity("pump", "switch"), SensorInfo(object_id="pump")))
        self.assertIsNone(aspect_descriptor(entity("x", "climate"), SwitchInfo(object_id="x")))


class StateValuesTest(unittest.TestCase):
    def values(self, entity_, state, info=None):
        return list(state_values(entity_, info, state))

    def test_a_switch_publishes_on(self):
        self.assertEqual(self.values(entity("pump", "switch"), SwitchState(state=True)), [("on", True)])

    def test_a_switchs_runtime_number_publishes_only_when_declared(self):
        state = NumberState(state=30.0)
        self.assertEqual(self.values(entity("pump", "switch", ["runtime_s"]), state), [("runtime_s", 30.0)])
        self.assertEqual(self.values(entity("pump", "switch"), state), [])

    def test_a_light_scales_brightness_to_254_and_rounds_color_temp(self):
        state = LightState(state=True, brightness=0.5, color_temperature=369.6)
        self.assertEqual(self.values(entity("lamp", "light"), state), [("on", True)])
        self.assertEqual(
            self.values(entity("lamp", "light", ["brightness", "color_temp"]), state),
            [("on", True), ("brightness", 127), ("color_temp", 370)],
        )

    def test_a_sensors_aspect_is_its_device_class_else_its_object_id(self):
        sensor = entity("t", "sensor")
        state = SensorState(state=21.5)
        info = SensorInfo(object_id="t", device_class="temperature")
        self.assertEqual(self.values(sensor, state, info), [("temperature", 21.5)])
        self.assertEqual(self.values(sensor, state, SensorInfo(object_id="lux")), [("lux", 21.5)])

    def test_binary_sensors_publish_by_capability(self):
        info = BinarySensorInfo(object_id="p", device_class="occupancy")
        state = BinarySensorState(state=True)
        self.assertEqual(self.values(entity("p", "occupancy"), state, info), [("occupied", True)])
        self.assertEqual(self.values(entity("p", "binary_sensor"), state, info), [("occupancy", True)])

    def test_a_button_publishes_the_event_type_as_its_action(self):
        self.assertEqual(self.values(entity("b", "button"), Event(event_type="double")), [("action", "double")])
        self.assertEqual(self.values(entity("b", "button"), Event(event_type="")), [])

    def test_a_missing_state_publishes_nothing(self):
        info = SensorInfo(object_id="t")
        self.assertEqual(self.values(entity("t", "sensor"), SensorState(missing_state=True), info), [])

    def test_a_state_that_does_not_fit_the_capability_publishes_nothing(self):
        self.assertEqual(self.values(entity("t", "sensor"), SwitchState(state=True)), [])


class ImmediateLoop:
    def call_soon_threadsafe(self, callback, *args):
        callback(*args)


class CmdHandlerTest(unittest.TestCase):
    def setUp(self):
        self.session = FakeSession("esphome")
        self.client = mock.Mock()
        self.attribution = mock.Mock()
        self.runtime = NumberInfo(object_id="pump_runtime_s", key=9, min_value=0, max_value=600)
        self.connected = {
            "pump": {"client": self.client, "key": 7, "runtime": self.runtime},
            "lamp": {"client": self.client, "key": 8},
            "temp": {"client": self.client, "key": 6},
        }

    def command(self, entity_, aspect, value):
        handler = cmd_handler(
            entity_, self.connected, threading.Lock(), ImmediateLoop(), self.session, self.attribution
        )
        payload = keys.cmd_envelope(value, "automation", "test", cmd_id="c0ffee01")
        handler(FakeSample(keys.cmd_key(entity_.room, entity_.name, aspect), payload))

    def assert_dropped(self, reason="invalid-command"):
        self.assertEqual(self.session.events[-1]["reason"], reason)
        self.assertEqual(self.session.events[-1]["cmd_id"], "c0ffee01")

    def test_a_switch_takes_a_bool_on(self):
        self.command(entity("pump", "switch"), "on", True)
        self.client.switch_command.assert_called_once_with(7, True)
        self.attribution.commanded.assert_called_once_with("pump", True)

    def test_a_switch_refuses_a_non_bool_or_another_aspect(self):
        for aspect, value in (("on", 1), ("brightness", 100)):
            with self.subTest(aspect=aspect, value=value):
                self.command(entity("pump", "switch"), aspect, value)
                self.assert_dropped()
        self.client.switch_command.assert_not_called()

    def test_runtime_is_taken_within_the_number_entitys_bounds(self):
        pump = entity("pump", "switch", ["runtime_s"])
        self.command(pump, "runtime_s", 600)
        self.client.number_command.assert_called_once_with(9, 600.0)
        for value in (601, -1, True, "30", 10**400):
            with self.subTest(value=value):
                self.command(pump, "runtime_s", value)
                self.assert_dropped()
        self.assertEqual(self.client.number_command.call_count, 1)

    def test_runtime_needs_the_feature_and_the_number_entity(self):
        self.command(entity("pump", "switch"), "runtime_s", 30)
        self.assert_dropped()
        del self.connected["pump"]["runtime"]
        self.command(entity("pump", "switch", ["runtime_s"]), "runtime_s", 30)
        self.assert_dropped()
        self.client.number_command.assert_not_called()

    def test_a_light_takes_on_brightness_and_color_temp(self):
        lamp = entity("lamp", "light", ["brightness", "color_temp"])
        self.command(lamp, "on", False)
        self.command(lamp, "brightness", 127)
        self.command(lamp, "brightness", 0)
        self.command(lamp, "color_temp", 370)
        self.assertEqual(
            self.client.light_command.call_args_list,
            [
                mock.call(8, state=False),
                mock.call(8, brightness=0.5),
                mock.call(8, brightness=0.0),
                mock.call(8, color_temperature=370.0),
            ],
        )
        # A brightness of 0 turns the light off, any other may turn it on.
        self.assertEqual(
            self.attribution.commanded.call_args_list,
            [mock.call("lamp", False), mock.call("lamp", True), mock.call("lamp", False)],
        )

    def test_brightness_outside_0_to_254_drops_and_is_not_clamped(self):
        lamp = entity("lamp", "light", ["brightness"])
        for value in (255, -1):
            with self.subTest(value=value):
                self.command(lamp, "brightness", value)
                self.assert_dropped()
        self.client.light_command.assert_not_called()

    def test_a_light_refuses_an_undeclared_feature(self):
        for aspect, value in (("brightness", 100), ("color_temp", 300), ("on", "yes")):
            with self.subTest(aspect=aspect):
                self.command(entity("lamp", "light"), aspect, value)
                self.assert_dropped()
        self.client.light_command.assert_not_called()

    def test_a_sensor_takes_no_commands(self):
        self.command(entity("temp", "sensor"), "temperature", 20)
        self.assert_dropped()

    def test_a_command_for_a_disconnected_device_drops_unavailable(self):
        self.command(entity("heater", "switch"), "on", True)
        self.assert_dropped("device-unavailable")
        self.client.switch_command.assert_not_called()


if __name__ == "__main__":
    unittest.main()
