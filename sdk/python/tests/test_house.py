"""House loading over a temporary house directory.

Run: uv run --no-project --with-editable sdk/python python -m unittest discover sdk/python/tests
"""

import tempfile
import unittest
from pathlib import Path
from unittest import mock

from homeostat import house
from houses import automation
from houses import house as house_dir

MANIFEST = """
schema = 1
[unit]
name = "pump"
kind = "adapter"
[discovery]
mode = "static"
endpoint = "mqtt://127.0.0.1:1883"
[entities]
dir = "entities/pump/"
"""

SOURCE_MANIFEST = """
schema = 1
[unit]
name = "fusion"
kind = "adapter"
[entities]
dir = "entities/fusion/"
"""


def entity(room, capability="climate", inputs=""):
    return f"""
schema = 1
[entity]
id = "{room}_1"
capability = "{capability}"
room = "{room}"
[write_policy]
mode = "shared"
{inputs}
"""


class LoadAdapterTest(unittest.TestCase):
    def test_house_is_parsed_once_for_every_wired_input(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            (root / "units").mkdir()
            (root / "units" / "pump.toml").write_text(MANIFEST)
            (root / "units" / "fusion.toml").write_text(SOURCE_MANIFEST)
            (root / "entities" / "pump").mkdir(parents=True)
            (root / "entities" / "fusion").mkdir(parents=True)
            (root / "entities" / "fusion" / "indoor.toml").write_text(entity("hall", "sensor"))
            wired = '[inputs]\nindoor_temperature_actual = { entity = "indoor", aspect = "temperature" }'
            (root / "entities" / "pump" / "a.toml").write_text(entity("utility", inputs=wired))
            (root / "entities" / "pump" / "b.toml").write_text(entity("cellar", inputs=wired))

            with mock.patch.object(house, "load_house", wraps=house.load_house) as load_house:
                config = house.load_adapter("pump", root)
            self.assertEqual(load_house.call_count, 1)
            self.assertEqual(
                [e.inputs["indoor_temperature_actual"].room for e in config.entities],
                ["hall", "hall"],
            )


class LoadUnitTest(unittest.TestCase):
    def test_a_manifest_is_read_as_declared(self):
        body = '[bus.subscribes]\nlights = "home/state/hall/*/on"\n[bus.publishes]\nout = { key = "home/cmd/hall/lamp/on" }\n'
        root = house_dir(self, {"units/evening.toml": automation("evening", body)})
        unit = house.load_unit("evening", root)
        self.assertEqual((unit.name, unit.kind), ("evening", "automation"))
        self.assertEqual(unit.subscribes, {"lights": "home/state/hall/*/on"})
        self.assertEqual(unit.publishes, {"out": {"key": "home/cmd/hall/lamp/on"}})
        self.assertIn(house.ENABLED_PARAM, unit.params)

    def test_a_manifest_without_a_kind_is_refused(self):
        root = house_dir(self, {"units/odd.toml": 'schema = 1\n[unit]\nname = "odd"\n'})
        with self.assertRaises(KeyError):
            house.load_unit("odd", root)


class LoadZonesTest(unittest.TestCase):
    def test_zones_are_read_from_the_file(self):
        root = house_dir(self, {"zones.toml": '[zones]\ndownstairs = ["hall", "kitchen"]\n'})
        self.assertEqual(house.load_zones(root), {"downstairs": ["hall", "kitchen"]})

    def test_a_house_without_the_file_has_none(self):
        self.assertEqual(house.load_zones(house_dir(self, {})), {})


if __name__ == "__main__":
    unittest.main()
