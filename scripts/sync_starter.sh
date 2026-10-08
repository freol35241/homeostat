#!/usr/bin/env bash
# The starter house ships copies of the generic adapters, because a house
# repo is self-contained (its README says `cp -r examples/starter-house
# ~/house`). Copies kept by hand drift from the release they claim to be,
# so this script generates them.
#
#   scripts/sync_starter.sh          rewrite the copies
#   scripts/sync_starter.sh --check  fail if any copy is stale (CI)
#
# The only edit a copy needs is the SDK source. adapters/ pins the
# working-tree SDK so the tests exercise it. A shipped house pins the
# release (docs/design.md#sdk-distribution). The lockfile beside each
# adapter is copied with it and gets the same edit (pin_lock below), so a
# shipped unit resolves what the release's adapter was locked against.
# Adapter and SDK must come from the same commit. An adapter from main
# running against the previous tag's SDK raises AttributeError on anything
# the SDK has added since.
#
# The starter is therefore a snapshot of SDK_TAG rather than of main.
# --check compares against adapters/ as of that tag, which stays true on
# every main commit. scripts/release.sh bumps SDK_TAG with the other
# version strings and reruns this script. Tagging the result is the last
# step. Until the new tag exists, the check falls back to the working
# tree, which is the release commit itself. CI must check out with
# fetch-depth: 0, or the tag is not found.
set -euo pipefail

# Bumped with the starter's compose image at each release.
SDK_TAG="v0.16.1"

# The tag's version, and the same version as Python spells it. They differ
# only for a prerelease. Semver puts a hyphen before the label
# (0.14.0-rc1), and Cargo and the image tag use that form. Canonical PEP 440
# has no hyphen (0.14.0rc1), and uv uses that form for the wheel and in a
# lock.
# The difference matters. A wheel filename uses "-" to separate its fields,
# so homeostat-0.14.0-rc1-py3-none-any.whl parses as version 0.14.0 with
# build tag "rc1". uv then refuses the lock, and no unit in the house
# starts. PEP 440 normalisation drops the separator before a prerelease
# label. This project's tags use a hyphen as that separator.
VERSION="${SDK_TAG#v}"
PY_VERSION="$(printf '%s' "$VERSION" | sed -E 's/-(a|b|rc|alpha|beta)/\1/')"

REPO="$(cd "$(dirname "$0")/.." && pwd)"
UNITS="$REPO/examples/starter-house/units"

# adapters/ source -> starter unit name. Adapters the starter has no unit
# for (go2rtc, onvif, openwrt) are left out. evening_lights.py is the
# starter's own example automation. It has no adapters/ source, so it only
# gets the SDK pin.
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
assets/dashboard.css:assets/dashboard.css
assets/video-rtc.js:assets/video-rtc.js
assets/homeostat-mark.svg:assets/homeostat-mark.svg
"

# A shipped unit names `homeostat==VERSION` and has no [tool.uv.sources]
# block. The image bundles the wheel and points UV_FIND_LINKS at it. The
# rewrite is idempotent, so --check compares like with like.
pin_sdk() {
  # Matches an unpinned "homeostat" and an already-pinned
  # "homeostat==X.Y.Z". The starter-only units are rewritten in place. A
  # pattern that only matched the unpinned form would leave them on the
  # previous release, and --check would not notice, because the check
  # compares against this same transform.
  sed -e 's|^\(# *\)"homeostat[^"]*",|\1"homeostat=='"$PY_VERSION"'",|' \
      -e '/^# \[tool\.uv\.sources\]$/d' \
      -e '/^# homeostat = /d' \
    | awk '
      # Drop the empty "#" separator that preceded the sources block, but
      # only when the next line closes the PEP 723 header.
      /^#$/ { held = 1; next }
      held && !/^# \/\/\/$/ { print "#" }
      { held = 0; print }
    '
}

# Rewrites the lock's SDK entry from the path source the dev tree locks
# against to the wheel the image bundles. The output matches what `uv lock
# --script` writes when it finds the wheel through UV_FIND_LINKS, byte for
# byte. uv at first boot therefore sees a fresh lock and leaves it alone
# (smoke_starter.sh asserts that). The rest of the lock is unchanged, so
# the shipped unit runs the release's resolution. WHEELS is the image's
# UV_FIND_LINKS (Dockerfile). A path wheel has no hash, so the image is
# what the unit trusts for the SDK.
WHEELS=/opt/homeostat-wheels
pin_lock() {
  awk -v v="$PY_VERSION" -v w="$WHEELS" '
    { sub(/name = "homeostat", editable = "[^"]*"/, "name = \"homeostat\", specifier = \"==" v "\"") }
    /^\[\[package\]\]$/ { sdk = 0 }
    /^name = "homeostat"$/ { sdk = 1 }
    sdk && index($0, "source = { editable = ") == 1 {
      print "source = { registry = \"" w "\" }"; next
    }
    # The dependencies list closes; the wheel follows it, and the path
    # source'"'"'s [package.metadata] block (requires-dist) goes away.
    sdk && /^\]$/ {
      print
      print "wheels = ["
      print "    { path = \"" w "/homeostat-" v "-py3-none-any.whl\" },"
      print "]"
      sdk = 0; meta = 1; next
    }
    meta && /^$/ && !seen { next }
    meta && /^\[package\.metadata\]$/ { seen = 1; next }
    meta && /^requires-dist = / { next }
    meta { meta = 0; seen = 0 }
    { print }
  '
}

check=0
[ "${1:-}" = "--check" ] && check=1
stale=""
tmp="$(mktemp)"
trap 'rm -f "$tmp"' EXIT

# Read the release's adapters/ when the tag is in this clone, and the
# working tree otherwise. The working tree is only correct while that
# release is being prepared. A clone without the tag (a shallow checkout)
# silently compares against the working tree, which is why CI fetches full
# history.
if git -C "$REPO" rev-parse -q --verify "$SDK_TAG^{commit}" >/dev/null; then
  at_tag=1
else
  at_tag=0
fi
source_at_tag() {
  if [ "$at_tag" = 1 ]; then
    git -C "$REPO" show "$SDK_TAG:adapters/$1"
  else
    cat "$REPO/adapters/$1"
  fi
}
# Whether the release has the file at all. A tag cut before lockfiles
# existed has none.
have_at_tag() {
  if [ "$at_tag" = 1 ]; then
    git -C "$REPO" cat-file -e "$SDK_TAG:adapters/$1" 2>/dev/null
  else
    [ -f "$REPO/adapters/$1" ]
  fi
}

# Writes generated content to its place in the starter. Under --check, it
# records that the starter's copy differs instead.
place() {
  if [ "$check" = 1 ]; then
    cmp -s "$1" "$2" || stale="$stale $3"
  else
    mkdir -p "$(dirname "$2")"
    cp "$1" "$2"
  fi
}

for pair in $FILES; do
  src="${pair%%:*}"
  dst="$UNITS/${pair##*:}"
  [ -f "$REPO/adapters/$src" ] || { echo "missing adapters/$src" >&2; exit 1; }
  # A file added since the release is not in the snapshot. It reaches the
  # starter at the next tag, together with the page that references it.
  have_at_tag "$src" || continue
  case "$src" in
    # Only a unit script has an SDK source line. Assets are copied as-is.
    *.py) source_at_tag "$src" | pin_sdk > "$tmp" ;;
    *) source_at_tag "$src" > "$tmp" ;;
  esac
  place "$tmp" "$dst" "${pair##*:}"
  if [ "${src##*.}" = py ] && have_at_tag "$src.lock"; then
    source_at_tag "$src.lock" | pin_lock > "$tmp"
    place "$tmp" "$dst.lock" "${pair##*:}.lock"
  fi
done

# The dashboard page's modules. This copies every file under
# assets/dashboard/ in the release instead of listing each one above, so a
# new module cannot be left out of the starter. Older releases have no
# modules.
modules_at_tag() {
  if [ "$at_tag" = 1 ]; then
    git -C "$REPO" ls-tree -r --name-only "$SDK_TAG" -- adapters/assets/dashboard
  elif [ -d "$REPO/adapters/assets/dashboard" ]; then
    (cd "$REPO" && find adapters/assets/dashboard -type f | sort)
  fi
}
for src in $(modules_at_tag); do
  src="${src#adapters/}"
  source_at_tag "$src" > "$tmp"
  place "$tmp" "$UNITS/$src" "$src"
done

# Starter-only units (evening_lights.py) get the pin and nothing else.
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

# The release version appears in four places, and they must agree. A
# missed bump makes every published binary report the wrong version, so it
# is checked here.
bad=""
check_version() {
  grep -qF "$2" "$REPO/$1" || bad="$bad\n  $1: expected $2"
}
check_version Cargo.toml "version = \"$VERSION\""
check_version sdk/python/pyproject.toml "version = \"$PY_VERSION\""
check_version examples/starter-house/docker-compose.yml "homeostat:$VERSION}"
if [ -n "$bad" ]; then
  echo "version drift against SDK_TAG=$SDK_TAG:$(printf '%b' "$bad")" >&2
  echo "scripts/release.sh bumps all four together" >&2
  exit 1
fi
