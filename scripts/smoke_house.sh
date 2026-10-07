#!/usr/bin/env bash
# Writes the minimal house the packaging smoke tests boot (smoke_image.sh,
# smoke_bare.sh): the clock adapter as its one unit, committed to git.
#
# Its SDK dependency is rewritten from the in-repo path source to
# `homeostat==VERSION` with no sources block — the shape a deployed unit
# has (docs/design.md, SDK distribution), so a smoke test resolves the SDK
# from the bundled wheel the way a real house does. The rewrite mirrors
# pin_sdk in scripts/sync_starter.sh.
#
# Usage: scripts/smoke_house.sh <dir> <sdk-version>
set -euo pipefail

HOUSE="${1:?usage: scripts/smoke_house.sh <dir> <sdk-version>}"
SDK_VERSION="${2:?usage: scripts/smoke_house.sh <dir> <sdk-version>}"
REPO="$(cd "$(dirname "$0")/.." && pwd)"

mkdir -p "$HOUSE/units"
sed -e 's|^\(# *\)"homeostat[^"]*",|\1"homeostat=='"$SDK_VERSION"'",|' \
    -e '/^# \[tool\.uv\.sources\]$/d' \
    -e '/^# homeostat = /d' \
    "$REPO/adapters/clock.py" \
  | awk '
    /^#$/ { held = 1; next }
    held && !/^# \/\/\/$/ { print "#" }
    { held = 0; print }
  ' > "$HOUSE/units/clock.py"
grep -q "\"homeostat==$SDK_VERSION\"" "$HOUSE/units/clock.py" \
  && ! grep -q 'tool.uv.sources' "$HOUSE/units/clock.py" \
  || { echo "SMOKE FAIL: clock.py SDK dependency line drifted; rewrite missed" >&2; exit 1; }
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
