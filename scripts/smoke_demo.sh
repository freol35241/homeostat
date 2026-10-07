#!/usr/bin/env bash
# Smoke test for the starter house's demo (examples/starter-house/demo):
# boots it the way demo/up.sh does, against the image under test, and
# asserts that every simulated device's state reaches the recorder through
# its real adapter and that a dashboard command round-trips to a device.
# The demo is the first thing a newcomer runs; this is what stops an
# adapter change from quietly leaving it half-dark.
#
# Usage: scripts/smoke_demo.sh <image>
set -euo pipefail

IMAGE="${1:?usage: scripts/smoke_demo.sh <image>}"
REPO="$(cd "$(dirname "$0")/.." && pwd)"
WORK="$(mktemp -d)"
export HOMEOSTAT_IMAGE="$IMAGE"
export DEMO_DIR="$WORK/house"
UP="$REPO/examples/starter-house/demo/up.sh"

# up.sh with arguments is `docker compose` for the demo: the project and
# files are named in one place.
cleanup() {
  "$UP" down -v --timeout 5 >/dev/null 2>&1 || true
  rm -rf "$WORK"
}
trap cleanup EXIT

fail() {
  echo "SMOKE FAIL: $1" >&2
  echo "--- compose logs ---" >&2
  "$UP" logs >&2 || true
  exit 1
}

"$UP" >/dev/null

# The latest recorded value of each aspect, or nothing yet.
latest() {
  python3 - "$DEMO_DIR/data/history.db" "$1" "$2" <<'PY'
import sqlite3, sys
try:
    conn = sqlite3.connect(f"file:{sys.argv[1]}?mode=ro", uri=True)
    row = conn.execute(
        "SELECT value FROM history WHERE class = 'state' AND entity = ? AND aspect = ?"
        " ORDER BY ts DESC LIMIT 1", (sys.argv[2], sys.argv[3])).fetchone()
    print("" if row is None else row[0])
except sqlite3.Error:
    print("")
PY
}

# Units resolve their environments on first boot: a generous deadline.
echo "waiting for every simulated device to reach the store..."
deadline=$((SECONDS + 420))
for pair in livingroom_lamp:on front_door:locked hallway_motion:occupancy \
            heatpump:indoor_temperature heatpump:setpoint alice:lat porch_switch:on; do
  entity="${pair%%:*}" aspect="${pair##*:}"
  until [ -n "$(latest "$entity" "$aspect")" ]; do
    [ -n "$("$UP" ps -q homeostat)" ] || fail "homeostat container exited"
    [ "$SECONDS" -lt "$deadline" ] || fail "no $entity/$aspect in the store"
    sleep 3
  done
  echo "$entity/$aspect = $(latest "$entity" "$aspect")"
done

# The other direction: the dashboard's command reaches the simulated
# ESPHome switch, and the device's readback comes home.
[ "$(latest porch_switch on)" = "0" ] || fail "the porch switch did not start off"
curl -sf -m 10 -X POST http://127.0.0.1:8600/api/cmd \
  -H 'Content-Type: application/json' -H 'X-Homeostat: family' \
  -d '{"room":"porch","entity":"porch_switch","aspect":"on","value":true}' >/dev/null \
  || fail "the dashboard refused the command"
deadline=$((SECONDS + 60))
until [ "$(latest porch_switch on)" = "1" ]; do
  [ "$SECONDS" -lt "$deadline" ] || fail "the porch switch never reported on"
  sleep 2
done
echo "porch_switch/on = 1 after a dashboard command"

echo "DEMO SMOKE OK"
