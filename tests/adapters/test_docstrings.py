"""Each adapter's module docstring lists the health events it emits.

The events the SDK emits itself are listed in one table in docs/adapters.md.
"""

import ast
import re
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
ADAPTERS = ROOT / "adapters"
SDK = ROOT / "sdk" / "python" / "homeostat"
SDK_TABLE = "### Events the SDK emits"


def emitted(tree: ast.Module) -> set[str]:
    """Return the reason, or the kind when there is no reason, of each literal event."""
    names = set()
    for node in ast.walk(tree):
        if not (isinstance(node, ast.Call) and getattr(node.func, "attr", None) == "health_event"):
            continue
        if not (node.args and isinstance(node.args[0], ast.Constant)):
            continue
        # A `**fields` argument may carry the reason, so it counts as one.
        reason = next((k.value for k in node.keywords if k.arg in ("reason", None)), None)
        if reason is None:
            names.add(node.args[0].value)
        elif isinstance(reason, ast.Constant):
            names.add(reason.value)
    return names


def sdk_events(tree: ast.Module) -> set[tuple[str, str | None]]:
    """Return the (kind, reason) of each event with a literal kind.

    The reason is None when there is none or it is not a literal. `mqtt.guard`
    calls its `health` argument, which is a Session's `health_event`.
    """
    events = set()
    for node in ast.walk(tree):
        if not isinstance(node, ast.Call) or not node.args:
            continue
        if getattr(node.func, "attr", getattr(node.func, "id", None)) not in ("health_event", "health"):
            continue
        kind = node.args[0]
        # `"paused" if paused else "resumed"` emits both.
        kinds = [kind.body, kind.orelse] if isinstance(kind, ast.IfExp) else [kind]
        reason = next((k.value for k in node.keywords if k.arg == "reason"), None)
        reason = reason.value if isinstance(reason, ast.Constant) else None
        events |= {(k.value, reason) for k in kinds if isinstance(k, ast.Constant)}
    return events


def table_events(markdown: str) -> set[tuple[str, str | None]]:
    """Return the (kind, reason) of each row of the SDK's table in adapters.md."""
    section = markdown.split(SDK_TABLE, 1)[1].split("\n#", 1)[0]
    events = set()
    for row in section.splitlines():
        cells = row.split("|")
        if len(cells) < 3 or not cells[1].strip().startswith("`"):
            continue
        reason = re.search(r'reason = "([^"]+)"', cells[2])
        for kind in re.findall(r"`([^`]+)`", cells[1]):
            events.add((kind, reason and reason.group(1)))
    return events


class HealthEventDocstringTest(unittest.TestCase):
    def test_every_literal_event_is_in_the_module_docstring(self):
        gaps = []
        for path in sorted(ADAPTERS.glob("*.py")):
            tree = ast.parse(path.read_text(), filename=str(path))
            doc = ast.get_docstring(tree) or ""
            for name in sorted(emitted(tree)):
                # A whole name: `purge` must not match inside `purge-failed`.
                if not re.search(rf"(?<![\w-]){re.escape(name)}(?![\w-])", doc):
                    gaps.append(f"{path.name}: {name}")
        self.assertEqual(gaps, [])

    def test_the_sdk_table_lists_every_literal_event_the_sdk_emits(self):
        emitted_by_sdk = set()
        for path in sorted(SDK.glob("*.py")):
            emitted_by_sdk |= sdk_events(ast.parse(path.read_text(), filename=str(path)))
        listed = table_events((ROOT / "docs" / "adapters.md").read_text())
        self.assertEqual(sorted(emitted_by_sdk, key=str), sorted(listed, key=str))


if __name__ == "__main__":
    unittest.main()
