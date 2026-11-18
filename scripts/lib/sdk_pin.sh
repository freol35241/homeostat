# shellcheck shell=bash
# How a shipped unit names the SDK. Sourced by sync_starter.sh, release.sh
# and smoke_house.sh, so the starter house, a release and the smoke tests
# pin the SDK the same way.

# Prints a semver version as Python spells it. They differ only for a
# prerelease. Semver puts a hyphen before the label (0.14.0-rc1), and
# Cargo and the image tag use that form. Canonical PEP 440 has no hyphen
# (0.14.0rc1), and uv uses that form for the wheel and in a lock.
# The difference matters. A wheel filename uses "-" to separate its fields,
# so homeostat-0.14.0-rc1-py3-none-any.whl parses as version 0.14.0 with
# build tag "rc1". uv then refuses the lock, and no unit in the house
# starts. PEP 440 normalisation drops the separator before a prerelease
# label. This project's tags use a hyphen as that separator.
py_version() {
  printf '%s' "$1" | sed -E 's/-(a|b|rc|alpha|beta)/\1/'
}

# Rewrites a unit script's PEP 723 header, read from stdin, to name
# `homeostat==$1` with no [tool.uv.sources] block. A deployed unit has
# that form (docs/design.md#sdk-distribution): the image bundles the wheel
# and points UV_FIND_LINKS at it. The rewrite is idempotent, so
# sync_starter.sh --check compares like with like.
pin_sdk() {
  # Matches an unpinned "homeostat" and an already-pinned
  # "homeostat==X.Y.Z". sync_starter.sh rewrites the starter-only units in
  # place. A pattern that only matched the unpinned form would leave them
  # on the previous release, and --check would not notice, because the
  # check compares against this same transform.
  sed -e 's|^\(# *\)"homeostat[^"]*",|\1"homeostat=='"$1"'",|' \
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
