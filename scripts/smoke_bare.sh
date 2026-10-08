#!/usr/bin/env bash
# Smoke test for running without Docker. It installs the release binary
# and the SDK wheel on a plain host with git and uv. Like smoke_image.sh,
# it tests packaging and leaves the logic to cargo test. It asserts that:
#   - the supervisor boots;
#   - a real Python unit resolves the SDK from the wheel directory and
#     reaches `running`;
#   - `plan` finds the live house through HOMEOSTAT_BUS;
#   - SIGTERM shuts the house down cleanly.
#
# Usage: scripts/smoke_bare.sh <homeostat-binary> <wheel-dir>
set -euo pipefail

BIN="$(realpath "${1:?usage: scripts/smoke_bare.sh <homeostat-binary> <wheel-dir>}")"
WHEELS="$(realpath "${2:?usage: scripts/smoke_bare.sh <homeostat-binary> <wheel-dir>}")"
REPO="$(cd "$(dirname "$0")/.." && pwd)"
WORK="$(mktemp -d)"
LOG="$WORK/supervisor.log"
SUP_PID=""

cleanup() {
  [ -n "$SUP_PID" ] && kill -KILL "$SUP_PID" 2>/dev/null || true
  rm -rf "$WORK"
}
trap cleanup EXIT

fail() {
  echo "SMOKE FAIL: $1" >&2
  echo "--- supervisor logs ---" >&2
  cat "$LOG" >&2 || true
  exit 1
}

SDK_VERSION="$(sed -n 's/^version = "\(.*\)"/\1/p' "$REPO/Cargo.toml" | head -1)"
ls "$WHEELS"/homeostat-"$SDK_VERSION"-*.whl >/dev/null 2>&1 \
  || { echo "SMOKE FAIL: no homeostat $SDK_VERSION wheel in $WHEELS" >&2; exit 1; }

HOUSE="$WORK/house"
"$REPO/scripts/smoke_house.sh" "$HOUSE" "$SDK_VERSION"

# Use a fresh uv cache. The SDK is not on any package index, so the unit
# can only resolve it from the wheel directory. The README tells a bare
# host to point UV_FIND_LINKS at that directory.
export UV_CACHE_DIR="$WORK/uv-cache"
export UV_FIND_LINKS="$WHEELS"
PORT="$(python3 -c 'import socket; s = socket.socket(); s.bind(("127.0.0.1", 0)); print(s.getsockname()[1])')"
export HOMEOSTAT_BUS="tcp/127.0.0.1:$PORT"

"$BIN" up "$HOUSE" --listen "$HOMEOSTAT_BUS" >"$LOG" 2>&1 &
SUP_PID=$!

# The unit's first `uv run` resolves eclipse-zenoh from PyPI, so allow a
# generous deadline before requiring `running`.
echo "waiting for the clock unit to reach running..."
deadline=$((SECONDS + 180))
until grep -q "\[homeostat\] clock: running" "$LOG"; do
  kill -0 "$SUP_PID" 2>/dev/null || fail "supervisor exited before the clock unit ran"
  [ "$SECONDS" -lt "$deadline" ] || fail "clock unit not running within 180s"
  sleep 2
done
echo "clock unit is running"

# Run plan as an operator would, with no --bus, so the endpoint comes from
# the environment. A clean boot of an unchanged repo must plan no changes.
plan_out="$("$BIN" plan "$HOUSE")"
echo "$plan_out" | grep -q "No changes. The world matches the repo." \
  || fail "plan against the live house found a diff: $plan_out"
echo "plan through HOMEOSTAT_BUS matches the repo"

# Send SIGTERM to the supervisor alone, as systemd's KillMode=mixed does.
# The units and then the supervisor must shut down cleanly.
kill -TERM "$SUP_PID"
exit_code=0
wait "$SUP_PID" || exit_code=$?
SUP_PID=""
[ "$exit_code" = "0" ] || fail "supervisor exited $exit_code on SIGTERM"
grep -q "\[homeostat\] shutting down" "$LOG" || fail "no clean shutdown line in the logs"
echo "clean shutdown on SIGTERM"

echo "SMOKE OK"
