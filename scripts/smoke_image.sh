#!/usr/bin/env bash
# Smoke test for the container image: verifies the packaging, not the
# logic (cargo test covers that). Asserts that inside the image the
# supervisor boots, a real Python unit resolves its uv environment and
# reaches `running`, the bus is reachable from a second container, and
# SIGTERM shuts the house down cleanly.
#
# Usage: scripts/smoke_image.sh <image>
set -euo pipefail

IMAGE="${1:?usage: scripts/smoke_image.sh <image>}"
REPO="$(cd "$(dirname "$0")/.." && pwd)"
WORK="$(mktemp -d)"
NET="homeostat-smoke-$$"
SUP="homeostat-smoke-sup-$$"

cleanup() {
  docker rm -f "$SUP" >/dev/null 2>&1 || true
  docker network rm "$NET" >/dev/null 2>&1 || true
  rm -rf "$WORK"
}
trap cleanup EXIT

fail() {
  echo "SMOKE FAIL: $1" >&2
  echo "--- supervisor logs ---" >&2
  docker logs "$SUP" >&2 || true
  exit 1
}

# A minimal self-contained house: the clock adapter as its one unit, the
# Python SDK vendored at house/sdk/python so the script's relative
# `../sdk/python` uv source resolves inside the mount.
HOUSE="$WORK/house"
mkdir -p "$HOUSE/units" "$HOUSE/sdk"
cp -r "$REPO/sdk/python" "$HOUSE/sdk/python"
cp "$REPO/adapters/clock.py" "$HOUSE/units/clock.py"
cat > "$HOUSE/zones.toml" <<'EOF'
schema = 1

[zones]
EOF
cat > "$HOUSE/units/clock.toml" <<'EOF'
schema = 1

[unit]
name = "clock"
kind = "service"
description = "Civil time on the bus"

[runtime]
command = "uv run units/clock.py"
restart = "always"
shutdown_grace_s = 5

[bus.publishes]
minute = { key = "home/clock/minute" }
date = { key = "home/clock/date" }

[params.timezone]
type = "string"
default = "Europe/Stockholm"
editable_by = "owner"
EOF
git -C "$HOUSE" init -q
git -C "$HOUSE" -c user.name=smoke -c user.email=smoke@example.com \
  add -A
git -C "$HOUSE" -c user.name=smoke -c user.email=smoke@example.com \
  commit -qm "smoke house"

docker network create "$NET" >/dev/null
docker run -d --name "$SUP" --network "$NET" -v "$HOUSE:/house" "$IMAGE" >/dev/null

# The unit's first `uv run` resolves eclipse-zenoh from the network, so
# allow a generous deadline before requiring `running`.
echo "waiting for the clock unit to reach running..."
deadline=$((SECONDS + 180))
until docker logs "$SUP" 2>&1 | grep -q "\[homeostat\] clock: running"; do
  if [ "$(docker inspect -f '{{.State.Running}}' "$SUP")" != "true" ]; then
    fail "supervisor container exited before the clock unit ran"
  fi
  if [ "$SECONDS" -ge "$deadline" ]; then
    fail "clock unit not running within 180s"
  fi
  sleep 2
done
echo "clock unit is running"

# A second container reaches the bus over the container network and reads
# the live world; a clean boot of an unchanged repo must plan to nothing.
plan_out="$(docker run --rm --network "$NET" -v "$HOUSE:/house" "$IMAGE" \
  plan /house --bus "tcp/$SUP:7447")"
echo "$plan_out" | grep -q "No changes. The world matches the repo." \
  || fail "plan against the live house found a diff: $plan_out"
echo "plan from a second container matches the repo"

# SIGTERM through tini must land as a clean supervisor shutdown.
docker stop -t 20 "$SUP" >/dev/null
exit_code="$(docker inspect -f '{{.State.ExitCode}}' "$SUP")"
[ "$exit_code" = "0" ] || fail "supervisor exited $exit_code on SIGTERM"
docker logs "$SUP" 2>&1 | grep -q "\[homeostat\] shutting down" \
  || fail "no clean shutdown line in the logs"
echo "clean shutdown on SIGTERM"

echo "SMOKE OK"
