#!/usr/bin/env bash
# Smoke test for examples/starter-house. It boots the template with docker
# compose as its README describes for a house without a coordinator stick
# (mosquitto and homeostat only). It asserts that every unit reaches
# `running` and that the house shuts down cleanly. HOMEOSTAT_IMAGE points
# the compose file at the image under test instead of the published one.
#
# Usage: scripts/smoke_starter.sh <image>
set -euo pipefail

IMAGE="${1:?usage: scripts/smoke_starter.sh <image>}"
REPO="$(cd "$(dirname "$0")/.." && pwd)"
WORK="$(mktemp -d)"
PROJECT="homeostat-starter-$$"
export HOMEOSTAT_IMAGE="$IMAGE"
# The values the README has a house set in .env. The container runs as
# the checkout's owner, so data/ is owned by the host user and a plain rm
# cleans it up. The compose file refuses to start without a z2m frontend
# token.
export HOMEOSTAT_UID="$(id -u)" HOMEOSTAT_GID="$(id -g)"
export Z2M_FRONTEND_TOKEN="smoke"

compose() {
  docker compose -p "$PROJECT" --project-directory "$WORK/house" "$@"
}

cleanup() {
  compose down -v --timeout 5 >/dev/null 2>&1 || true
  rm -rf "$WORK"
}
trap cleanup EXIT

fail() {
  echo "SMOKE FAIL: $1" >&2
  echo "--- compose logs ---" >&2
  compose logs >&2 || true
  exit 1
}

cp -r "$REPO/examples/starter-house" "$WORK/house"
# The README's first-start step.
cp "$WORK/house/mosquitto.passwd.example" "$WORK/house/mosquitto.passwd"
# The example maps a fixed MCP port, which the README relies on. The smoke
# run swaps in a free host port so that parallel runs, or another process
# on 8642, do not collide.
MCP_PORT="$(python3 -c 'import socket; s = socket.socket(); s.bind(("127.0.0.1", 0)); print(s.getsockname()[1])')"
sed -i "s/\"8642:8642\"/\"127.0.0.1:${MCP_PORT}:8642\"/" "$WORK/house/docker-compose.yml"
grep -q "${MCP_PORT}:8642" "$WORK/house/docker-compose.yml" \
  || { echo "SMOKE FAIL: could not rewrite the MCP port mapping" >&2; exit 1; }
git -C "$WORK/house" init -q
git -C "$WORK/house" -c user.name=smoke -c user.email=smoke@example.com \
  add -A
git -C "$WORK/house" -c user.name=smoke -c user.email=smoke@example.com \
  commit -qm "starter house"

compose up -d mosquitto homeostat >/dev/null 2>&1

# Four units resolve their uv environments on first boot (the SDK from the
# release tag, eclipse-zenoh and paho-mqtt from PyPI), so the deadline is
# generous.
echo "waiting for all starter units to reach running..."
deadline=$((SECONDS + 300))
for unit in clock recorder zigbee evening_lights mcp; do
  until compose logs homeostat 2>&1 | grep -q "\[homeostat\] $unit: running"; do
    if [ -z "$(compose ps -q homeostat)" ]; then
      fail "homeostat container exited before $unit ran"
    fi
    if [ "$SECONDS" -ge "$deadline" ]; then
      fail "$unit not running within deadline"
    fi
    sleep 2
  done
  echo "$unit is running"
done

# The agent surface refuses a request without the write header. A
# cross-origin browser POST can produce such a request
# (docs/design.md#local-only-access). This is checked before the normal
# request, so a surface that answered everything would fail here.
refused="$(curl -s -m 10 -o /dev/null -w '%{http_code}' -X POST "http://127.0.0.1:${MCP_PORT}" \
  -H 'Content-Type: application/json' \
  -d '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18"}}')"
[ "$refused" = "403" ] \
  || fail "MCP answered a request with no X-Homeostat header: HTTP $refused"

# It answers a client that sends the header.
init="$(curl -s -m 10 -X POST "http://127.0.0.1:${MCP_PORT}" \
  -H 'Content-Type: application/json' -H 'X-Homeostat: 1' \
  -d '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18"}}')"
echo "$init" | grep -q '"name":"homeostat"' \
  || fail "MCP initialize did not answer over HTTP: $init"
echo "agent surface answers on :${MCP_PORT}, and refuses an un-headered POST"

# The owner tool the image ships reads the recorder's stats over the bus.
profile="$(docker exec "$(compose ps -q homeostat)" uv run /opt/homeostat/store_profile.py 2>&1)" \
  || fail "store_profile.py failed in the image: $profile"
echo "$profile" | grep -q "^store " || fail "store_profile.py printed no store line: $profile"
echo "store_profile.py reads the recorder's stats"

# Every unit must resolve from its committed lockfile. uv rewrites a lock
# it finds stale. If booting leaves the house repo dirty, a lock shipped by
# sync_starter.sh does not match the script beside it.
dirty="$(git -C "$WORK/house" status --porcelain -- 'units/*.lock')"
[ -z "$dirty" ] || fail "a unit rewrote its lockfile at boot:
$dirty"

CID="$(compose ps -q homeostat)"
compose stop --timeout 20 homeostat >/dev/null 2>&1
exit_code="$(docker inspect -f '{{.State.ExitCode}}' "$CID")"
[ "$exit_code" = "0" ] || fail "homeostat exited $exit_code on compose stop"
echo "clean shutdown"

echo "STARTER SMOKE OK"
