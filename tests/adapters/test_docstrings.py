"""Each adapter's module docstring lists the health events it emits."""

import ast
import re
import unittest
from pathlib import Path

ADAPTERS = Path(__file__).resolve().parents[2] / "adapters"


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


if __name__ == "__main__":
    unittest.main()
