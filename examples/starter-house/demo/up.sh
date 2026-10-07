#!/usr/bin/env bash
# Starts the starter house with simulated devices (see demo/README.md):
# copies the house out to its own directory and git repo the way the
# starter's README says, does the first-start steps, and brings up the
# broker, homeostat and the simulator. Run it again to pick up where it
# left off.
#
# With arguments, it is `docker compose` for the demo instead — the same
# project, files and environment — so nothing else has to repeat them:
#   demo/up.sh down             stop it (down -v also drops the uv cache)
#   demo/up.sh logs -f homeostat
#
#   DEMO_DIR    where the copy lives (default ~/homeostat-demo)
set -euo pipefail

STARTER="$(cd "$(dirname "$0")/.." && pwd)"
DEMO_DIR="${DEMO_DIR:-$HOME/homeostat-demo}"

compose() {
  docker compose --project-directory "$DEMO_DIR" -p homeostat-demo \
    -f "$DEMO_DIR/docker-compose.yml" -f "$DEMO_DIR/demo/docker-compose.demo.yml" "$@"
}

# The z2m frontend token the compose file insists on; the demo never
# starts zigbee2mqtt, so any value does.
export Z2M_FRONTEND_TOKEN="${Z2M_FRONTEND_TOKEN:-demo-not-used}"
export HOMEOSTAT_UID="${HOMEOSTAT_UID:-$(id -u)}" HOMEOSTAT_GID="${HOMEOSTAT_GID:-$(id -g)}"

if [ "$#" -gt 0 ]; then
  compose "$@"
  exit
fi

# In a fresh codespace the Docker daemon may still be starting.
for _ in $(seq 1 60); do
  docker info >/dev/null 2>&1 && break
  sleep 1
done
docker info >/dev/null 2>&1 || { echo "no Docker daemon answering" >&2; exit 1; }

if [ ! -d "$DEMO_DIR" ]; then
  cp -r "$STARTER" "$DEMO_DIR"
  cp "$DEMO_DIR/mosquitto.passwd.example" "$DEMO_DIR/mosquitto.passwd"
  git -C "$DEMO_DIR" init -q
  git -C "$DEMO_DIR" add -A
  git -C "$DEMO_DIR" -c user.name=demo -c user.email=demo@example.com commit -qm "demo house"
  echo "copied the starter house to $DEMO_DIR"
fi

compose up -d mosquitto homeostat simulator
echo
echo "The house is starting: units resolve their environments on the first"
echo "boot, which takes a minute or two. Watch it with"
echo "  $0 logs -f homeostat"
echo
echo "Dashboard: http://localhost:8600"
