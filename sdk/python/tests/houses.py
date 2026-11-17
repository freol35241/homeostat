"""Temporary house directories, for tests that build a real Context."""

import tempfile
from pathlib import Path

# A recorder is the unit that publishes under home/history/, which is how a
# Context tells that the house runs one.
RECORDER = """
schema = 1
[unit]
name = "recorder"
kind = "service"
[bus.publishes]
history = { key = "home/history/**" }
"""


def automation(name, body=""):
    """The manifest text of an automation named `name`, with `body` after its header."""
    return f'schema = 1\n[unit]\nname = "{name}"\nkind = "automation"\n{body}'


def entity(room, capability="switch", body=""):
    """The text of an entity file in `room`, with `body` after its tables."""
    return (
        f'schema = 1\n[entity]\ncapability = "{capability}"\nroom = "{room}"\n'
        f'[write_policy]\nmode = "exclusive"\n{body}'
    )


def house(test, files):
    """Write `files`, path to text, under a root that `test` removes when it ends."""
    tmp = tempfile.TemporaryDirectory()
    test.addCleanup(tmp.cleanup)
    root = Path(tmp.name)
    (root / "units").mkdir()
    for name, text in files.items():
        path = root / name
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(text)
    return root
