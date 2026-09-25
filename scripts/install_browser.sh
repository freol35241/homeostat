#!/usr/bin/env bash
# Fetches the Chromium that tests/browser is locked against, with the
# system libraries it needs. Installs only — it runs no tests.
#
# Through the test script rather than `playwright install` directly, so the
# browser is always the version tests/browser/run.py.lock resolved: a
# mismatch between the two is a "Executable doesn't exist" that costs half
# an hour to read.
#
#   scripts/install_browser.sh
set -euo pipefail
REPO="$(cd "$(dirname "$0")/.." && pwd)"
exec uv run --script "$REPO/tests/browser/run.py" --install-browser
