#!/usr/bin/env bash
# PRODUCTION-READINESS-100.md goal 97: fails the release if docs/CHANGELOG.md
# has no entry for the version being released — a release tag can't be cut
# without a corresponding changelog entry. Run from release.yml's
# security-gate job, which already gates the whole release pipeline.
#
# Determines the version being released the same way release.yml's other
# steps do: strip the leading "v" off the pushed tag (falls back to the
# workspace Cargo.toml version for a local/manual run, e.g. testing this
# script before a tag exists).
set -euo pipefail

cd "$(dirname "$0")/.."

if [ -n "${GITHUB_REF:-}" ] && [[ "$GITHUB_REF" == refs/tags/* ]]; then
  version="${GITHUB_REF#refs/tags/}"
  version="${version#v}"
else
  version=$(grep -m1 '^version = ' Cargo.toml | sed -E 's/version = "(.*)"/\1/')
  echo "No tag in GITHUB_REF — falling back to workspace Cargo.toml version: $version"
fi

if grep -qE "^## \[${version//./\\.}\]" docs/CHANGELOG.md; then
  echo "OK: docs/CHANGELOG.md has an entry for $version."
else
  echo "FAIL: docs/CHANGELOG.md has no '## [$version]' heading."
  echo ""
  echo "Add a changelog entry for this release before tagging — see the"
  echo "existing '## [0.3.2] - YYYY-MM-DD' entries for the expected format."
  exit 1
fi
