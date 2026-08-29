#!/usr/bin/env bash
# The starter house ships copies of the generic adapters, because a house
# repo is self-contained (its README says `cp -r examples/starter-house
# ~/house`). Hand-maintained copies drift silently — three of them had
# already been edited past the release they claim — so they are generated.
#
#   scripts/sync_starter.sh          rewrite the copies
#   scripts/sync_starter.sh --check  fail if any copy is stale (CI)
#
# The one edit a copy needs is the SDK source: adapters/ pins the
# working-tree SDK so the tests exercise it, a shipped house pins the
# release (docs/design.md, SDK distribution). Adapter and SDK must come
# from the SAME commit — an adapter from main against the previous tag's
# SDK raises AttributeError on whatever the SDK grew since.
#
# The starter is therefore a snapshot of SDK_TAG, not of main: --check
# compares against adapters/ AS OF that tag, which is the invariant that
# holds continuously on main. Cutting a release means bumping SDK_TAG,
# sdk/python/pyproject.toml and the compose image, rerunning this, and
# tagging the result. While the new tag does not exist yet the check
# falls back to the working tree — that is the release commit itself.
# (CI must check out with fetch-depth: 0 or the tag is never found.)
set -euo pipefail

# Bumped with the starter's compose image at each release.
SDK_TAG="v0.8.0"

REPO="$(cd "$(dirname "$0")/.." && pwd)"
UNITS="$REPO/examples/starter-house/units"

# adapters/ source -> starter unit name. Adapters the starter has no unit
# for (go2rtc, onvif, openwrt) are absent by design; evening_lights.py is
# the starter's own example automation and has no adapters/ source, so it
# only takes the SDK pin.
FILES="
zigbee2mqtt.py:zigbee.py
esphome.py:esphome.py
ivt490.py:ivt490.py
owntracks.py:owntracks.py
clock.py:clock.py
recorder.py:recorder.py
arbiter.py:arbiter.py
dashboard.py:dashboard.py
dashboard.html:dashboard.html
assets/leaflet.css:assets/leaflet.css
assets/leaflet.js:assets/leaflet.js
assets/protomaps-leaflet.js:assets/protomaps-leaflet.js
assets/dashboard-logic.js:assets/dashboard-logic.js
assets/video-rtc.js:assets/video-rtc.js
"

# A shipped unit names `homeostat==VERSION` and carries no
# [tool.uv.sources] block at all: the image bundles the wheel and points
# UV_FIND_LINKS at it. Idempotent, so --check compares like with like.
pin_sdk() {
  sed -e 's|^#     "homeostat".*|#     "homeostat=='"${SDK_TAG#v}"'",|' \
      -e '/^# \[tool\.uv\.sources\]$/d' \
      -e '/^# homeostat = /d' \
    | awk '
      # Drop the now-empty "#" separator that preceded the sources block:
      # only when the next line closes the PEP 723 header.
      /^#$/ { held = 1; next }
      held && !/^# \/\/\/$/ { print "#" }
      { held = 0; print }
    '
}

check=0
[ "${1:-}" = "--check" ] && check=1
stale=""
tmp="$(mktemp)"
trap 'rm -f "$tmp"' EXIT

# The release's adapters/, or the working tree while that release is being
# prepared.
source_at_tag() {
  git -C "$REPO" show "$SDK_TAG:adapters/$1" 2>/dev/null || cat "$REPO/adapters/$1"
}

for pair in $FILES; do
  src="${pair%%:*}"
  dst="$UNITS/${pair##*:}"
  [ -f "$REPO/adapters/$src" ] || { echo "missing adapters/$src" >&2; exit 1; }
  case "$src" in
    # Only a unit script carries an SDK source line; assets copy verbatim.
    *.py) source_at_tag "$src" | pin_sdk > "$tmp" ;;
    *) source_at_tag "$src" > "$tmp" ;;
  esac
  if [ "$check" = 1 ]; then
    cmp -s "$tmp" "$dst" || stale="$stale ${pair##*:}"
  else
    mkdir -p "$(dirname "$dst")"
    cp "$tmp" "$dst"
  fi
done

# Starter-only units (evening_lights.py) take the pin and nothing else.
for unit in "$UNITS"/*.py; do
  grep -q '^# *"homeostat' "$unit" || continue
  pin_sdk < "$unit" > "$tmp"
  if [ "$check" = 1 ]; then
    cmp -s "$tmp" "$unit" || stale="$stale $(basename "$unit"):sdk-pin"
  else
    cp "$tmp" "$unit"
  fi
done

if [ -n "$stale" ]; then
  echo "starter-house copies are stale:$stale" >&2
  echo "run scripts/sync_starter.sh and commit the result" >&2
  exit 1
fi

# The release version lives in four places and they must agree. Cargo's was
# left at 0.1.0 through eight releases, so every published binary reported
# 0.1.0 -- a habit is not enough, and a version nobody can trust is worse
# than no version at all.
version="${SDK_TAG#v}"
bad=""
check_version() {
  grep -qF "$2" "$REPO/$1" || bad="$bad\n  $1: expected $2"
}
check_version Cargo.toml "version = \"$version\""
check_version sdk/python/pyproject.toml "version = \"$version\""
check_version examples/starter-house/docker-compose.yml "homeostat:$version}"
if [ -n "$bad" ]; then
  echo "version drift against SDK_TAG=$SDK_TAG:$(printf "$bad")" >&2
  echo "cutting a release bumps all four together" >&2
  exit 1
fi
