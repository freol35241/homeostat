#!/usr/bin/env bash
# Prepare a release commit: bump every version string, relock, regenerate
# the starter house.
#
#   scripts/release.sh 0.17.0
#   scripts/release.sh 0.17.0-rc1
#
# It edits the working tree and stops there. Review the diff, commit it as
# "Release X.Y.Z", merge, then tag the merge commit vX.Y.Z; the tag is what
# triggers the release workflow.
set -euo pipefail

VERSION="${1:-}"
if ! [[ "$VERSION" =~ ^[0-9]+\.[0-9]+\.[0-9]+(-(a|b|rc|alpha|beta)[0-9]*)?$ ]]; then
  echo "usage: scripts/release.sh X.Y.Z[-rcN]" >&2
  exit 2
fi
# Python spells a prerelease without the hyphen (see sync_starter.sh).
PY_VERSION="$(printf '%s' "$VERSION" | sed -E 's/-(a|b|rc|alpha|beta)/\1/')"

REPO="$(cd "$(dirname "$0")/.." && pwd)"
cd "$REPO"

if [ -n "$(git status --porcelain)" ]; then
  echo "the working tree has changes; commit or stash them first" >&2
  exit 1
fi

# perl rather than sed -i, which takes different arguments on macOS.
export VERSION PY_VERSION
perl -pi -e 's/^SDK_TAG="v[^"]*"/SDK_TAG="v$ENV{VERSION}"/' scripts/sync_starter.sh
perl -pi -e 's/^version = "[^"]*"/version = "$ENV{VERSION}"/' Cargo.toml
perl -pi -e 's/^version = "[^"]*"/version = "$ENV{PY_VERSION}"/' sdk/python/pyproject.toml
perl -pi -e 's/(ghcr\.io\/freol35241\/homeostat:)[^}]*\}/$1$ENV{VERSION}}/' \
  examples/starter-house/docker-compose.yml

# Cargo.lock records the crate's own version.
cargo metadata --format-version 1 >/dev/null

# Every script locked against the working-tree SDK records its version too.
# The starter's locks are not among them: sync_starter.sh writes those.
git ls-files '*.py.lock' | while read -r lock; do
  grep -q '^source = { editable = ' "$lock" || continue
  uv lock --quiet --script "${lock%.lock}"
done

scripts/sync_starter.sh
scripts/sync_starter.sh --check

git status --short
echo
echo "Release $VERSION prepared. Review the diff, then commit it as \"Release $VERSION\"."
