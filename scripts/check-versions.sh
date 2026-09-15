#!/usr/bin/env bash
# Verifies package.json's version, Cargo.toml's [workspace.package] version,
# and (when run against a git tag) the tag itself all agree. Previously only
# tag-vs-package.json was checked (in release.yml), so package.json and
# Cargo.toml could silently drift — see PRODUCTION-READINESS-100.md goal 7.
#
# Usage:
#   scripts/check-versions.sh            # checks package.json == Cargo.toml
#   scripts/check-versions.sh v0.3.2      # also checks both against this tag
set -euo pipefail

cd "$(dirname "$0")/.."

pkg_version=$(node -p "require('./package.json').version")
cargo_version=$(grep -m1 '^version = ' Cargo.toml | sed -E 's/version = "([^"]+)"/\1/')

echo "package.json version: $pkg_version"
echo "Cargo.toml [workspace.package] version: $cargo_version"

status=0

if [ "$pkg_version" != "$cargo_version" ]; then
  echo "ERROR: package.json version ($pkg_version) does not match Cargo.toml workspace version ($cargo_version)"
  status=1
fi

if [ "${1:-}" != "" ]; then
  tag="$1"
  tag_version="${tag#v}"
  echo "git tag version: $tag_version"
  if [ "$tag_version" != "$pkg_version" ]; then
    echo "ERROR: tag version ($tag_version) does not match package.json version ($pkg_version)"
    status=1
  fi
  if [ "$tag_version" != "$cargo_version" ]; then
    echo "ERROR: tag version ($tag_version) does not match Cargo.toml workspace version ($cargo_version)"
    status=1
  fi
fi

if [ "$status" -eq 0 ]; then
  echo "Versions are consistent."
fi

exit "$status"
