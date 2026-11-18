#!/usr/bin/env bash
# Resolves every Python unit's dependencies up front, so the first test run
# does not eat into test timeouts. Each script syncs against the lockfile
# committed next to it. With --locked, a script edited past its lock fails
# here instead of re-resolving.
#
#   scripts/prewarm.sh
set -euo pipefail
cd "$(dirname "$0")/.."

for script in adapters/*.py tests/fake_*.py tests/fixtures/house_*/units/*.py; do
  uv sync --locked --script "$script"
done
