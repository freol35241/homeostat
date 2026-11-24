#!/usr/bin/env bash
# Runs the checks CI runs, in CI's order. The tool versions live here, and
# .github/workflows/ci.yml calls this script once per check.
#
#   scripts/check.sh                 scripts/prewarm.sh, then every check
#   scripts/check.sh adapters ruff   only the named checks
#
# The checks, in order: fmt clippy cargo-test sdk pyright adapters ruff js
# starter smoke-bare browser. cargo-test needs mosquitto, and browser needs
# scripts/install_browser.sh first.
set -euo pipefail
cd "$(dirname "$0")/.."

CHECKS="fmt clippy cargo-test sdk pyright adapters ruff js starter smoke-bare browser"

# The Python checks run against the working-tree SDK. paho is not an SDK
# dependency. homeostat.mqtt is an optional module, and the MQTT adapters
# bring their own paho for it. Its message guard is tested in sdk, so the
# runner needs paho.
SDK=(uv run --no-project --with-editable sdk/python --with 'paho-mqtt>=2,<3')

check_fmt() { cargo fmt --check; }

check_clippy() { cargo clippy --all-targets -- -D warnings; }

check_cargo_test() { cargo test; }

# The SDK's bus-free helpers, with stdlib unittest.
check_sdk() { "${SDK[@]}" python -m unittest discover sdk/python/tests; }

# The SDK ships py.typed, so units type-check against it. This checks that
# its annotations hold.
check_pyright() { "${SDK[@]}" --with pyright==1.1.414 pyright sdk/python/homeostat; }

# The adapters' logic, unit by unit, without a bus or a process. The tested
# scripts import their own libraries at module level, so the runner has
# them, in the ranges the scripts' headers declare.
check_adapters() {
  "${SDK[@]}" --with 'aiohttp>=3.12.14,<4' --with 'aioesphomeapi>=45,<46' \
    --with 'zeroconf>=0.130,<1' python -m unittest discover tests/adapters
}

check_ruff() { uvx ruff@0.16.7 check adapters sdk scripts tests/adapters tests/browser; }

# The dashboard's decision logic (adapters/assets/dashboard/logic/) and
# its html tag, with Node's built-in runner and no packages. The shell
# expands the glob, because newer Node treats a bare directory argument as
# a module path and fails with MODULE_NOT_FOUND.
check_js() { node --test tests/js/*.test.js; }

# examples/starter-house ships generated copies of adapters/. This fails
# when a copy is edited by hand or a release does not regenerate them.
check_starter() { scripts/sync_starter.sh --check; }

# Installs the binary and the SDK wheel without Docker, the way a release's
# tarball and wheel are installed. scripts/smoke_bare.sh lists what it
# asserts. The build is a no-op after cargo-test, and lets this check run
# on its own.
check_smoke_bare() {
  WHEELS="$(mktemp -d)"
  trap 'rm -rf "$WHEELS"' EXIT
  uv build --wheel sdk/python -o "$WHEELS"
  cargo build --bin homeostat
  scripts/smoke_bare.sh target/debug/homeostat "$WHEELS"
}

# The dashboard page in a real browser against canned fixtures
# (tests/browser/README.md).
check_browser() { uv run --script tests/browser/run.py; }

if [ "$#" = 0 ]; then
  scripts/prewarm.sh
  # shellcheck disable=SC2086 # CHECKS splits into names
  set -- $CHECKS
fi
for name in "$@"; do
  # A name with a space in it would match across two names below.
  case " $CHECKS " in
    *" $name "*) [[ "$name" != *" "* ]] && continue ;;
  esac
  echo "unknown check: $name (the checks: $CHECKS)" >&2
  exit 2
done
for name in "$@"; do
  echo "==> $name"
  "check_${name//-/_}"
done
