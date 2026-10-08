#!/usr/bin/env bash
# Fetches the Chromium that tests/browser is locked against, with the
# system libraries it needs. It only installs and runs no tests.
#
# It goes through the test script instead of calling `playwright install`
# directly, so the browser matches the Playwright version that
# tests/browser/run.py.lock resolved. A mismatch shows up as an unhelpful
# "Executable doesn't exist" error.
#
#   scripts/install_browser.sh
set -euo pipefail
REPO="$(cd "$(dirname "$0")/.." && pwd)"
exec uv run --script "$REPO/tests/browser/run.py" --install-browser
