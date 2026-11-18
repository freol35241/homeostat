"""Put adapters/ on sys.path, so a test imports an adapter script by its name.

A test imports this module before any adapter. Discovery runs the tests as
top-level modules, so ``tests/adapters`` itself is on sys.path and this
import resolves.
"""

import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[2] / "adapters"))
