"""House loading over a temporary house directory.

Run: uv run --no-project --with-editable sdk/python python -m unittest discover sdk/python/tests
"""

import tempfile
import unittest
from pathlib import Path
from unittest import mock

from homeostat import house

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


if __name__ == "__main__":
    unittest.main()
