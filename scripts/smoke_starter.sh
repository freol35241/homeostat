#!/usr/bin/env bash
# Smoke test for examples/starter-house: boots the template with docker
# compose the way its README says to when there is no coordinator stick
# (mosquitto + homeostat only) and asserts every unit reaches `running`
# and the house shuts down cleanly. HOMEOSTAT_IMAGE points the compose
# file at the image under test instead of the published one.
#
# Usage: scripts/smoke_starter.sh <image>
set -euo pipefail

IMAGE="${1:?usage: scripts/smoke_starter.sh <image>}"
REPO="$(cd "$(dirname "$0")/.." && pwd)"
WORK="$(mktemp -d)"
PROJECT="homeostat-starter-$$"
export HOMEOSTAT_IMAGE="$IMAGE"

compose() {
  docker compose -p "$PROJECT" --project-directory "$WORK/house" "$@"
}

cleanup() {
  compose down -v --timeout 5 >/dev/null 2>&1 || true
  # The recorder wrote data/ as root inside the container; remove it the
  # same way so the host-side rm succeeds.
  docker run --rm --entrypoint /bin/rm -v "$WORK/house:/house" "$IMAGE" \
    -rf /house/data >/dev/null 2>&1 || true
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
git -C "$WORK/house" init -q
git -C "$WORK/house" -c user.name=smoke -c user.email=smoke@example.com \
  add -A
git -C "$WORK/house" -c user.name=smoke -c user.email=smoke@example.com \
  commit -qm "starter house"

compose up -d mosquitto homeostat >/dev/null 2>&1

# Four units resolve their uv environments on first boot (SDK from the
# release tag, eclipse-zenoh and paho-mqtt from PyPI): generous deadline.
echo "waiting for all starter units to reach running..."
deadline=$((SECONDS + 300))
for unit in clock recorder zigbee evening_lights; do
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

CID="$(compose ps -q homeostat)"
compose stop --timeout 20 homeostat >/dev/null 2>&1
exit_code="$(docker inspect -f '{{.State.ExitCode}}' "$CID")"
[ "$exit_code" = "0" ] || fail "homeostat exited $exit_code on compose stop"
echo "clean shutdown"

echo "STARTER SMOKE OK"
