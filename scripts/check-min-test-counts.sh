#!/usr/bin/env bash
# PRODUCTION-READINESS-100.md goal 92: fails CI if any crate's test count
# drops below its recorded floor. This is a ratchet, not an absolute quality
# bar — floors below are simply each crate's test count at the time this
# script was written (2026-09-15), so the check exists to catch tests being
# silently deleted (or a crate's test file being excluded from the build)
# rather than to enforce a specific "enough tests" number. When you
# deliberately remove a test (e.g. replacing several small tests with one
# parametrized one), lower that crate's floor in the same commit with a
# one-line reason; when you add tests, floors do not need to be bumped —
# only decreases are checked.
set -euo pipefail

cd "$(dirname "$0")/.."

declare -A FLOORS=(
  [adapter-next]=19
  [adapter-pledgestack]=6
  [adapter-react]=2
  [adapter-solid]=5
  [adapter-tanstack]=6
  [cache]=17
  [cli]=22
  [core]=360
  [dev-server]=27
  [js-plugin-host]=24
  [optimizer]=24
  [resolver]=10
  [task-system]=173
  [task-system-macros]=1
  [wasm-plugin-host]=60
)

fail=0

for crate in "${!FLOORS[@]}"; do
  dir="crates/$crate"
  if [ ! -d "$dir" ]; then
    echo "SKIP: $crate (directory crates/$crate not found — floor entry is stale, remove it)"
    continue
  fi
  floor="${FLOORS[$crate]}"
  count=$(grep -rEc '#\[test\]|#\[tokio::test\]|#\[.*::test\]' "$dir" --include='*.rs' 2>/dev/null | awk -F: '{sum+=$2} END {print sum+0}')
  if [ "$count" -lt "$floor" ]; then
    echo "FAIL: $crate has $count tests, below its floor of $floor"
    fail=1
  else
    echo "OK: $crate has $count tests (floor $floor)"
  fi
done

if [ "$fail" -ne 0 ]; then
  echo ""
  echo "One or more crates dropped below their recorded test-count floor."
  echo "If this is a deliberate consolidation, lower the floor in"
  echo "scripts/check-min-test-counts.sh with a one-line reason; otherwise"
  echo "restore the missing test coverage."
  exit 1
fi

echo ""
echo "All crates meet their minimum test-count floor."
