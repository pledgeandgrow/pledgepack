#!/usr/bin/env bash
# PRODUCTION-READINESS-100.md goal 100: regenerates a production-readiness
# scorecard from the actual per-goal status recorded in that document, rather
# than hand-maintained prose. Run locally (prints to stdout) or in CI
# (`--out <path>` writes it to a file to upload as a build artifact).
#
# Scope note, stated honestly: this reads PRODUCTION-READINESS-100.md's own
# ✅/🟡/🔴 markers, which that document's own governing rule requires to be
# backed by a passing, CI-enforced regression test before a goal may be
# marked ✅ — so this is one level of indirection from raw CI job results,
# not from hand-typed claims disconnected from any check. A tighter version
# that queries the GitHub Actions API directly for job pass/fail per goal
# would be more airtight but needs a mapping from goal number to specific CI
# job/step that doesn't exist yet — real follow-up work, not done here.
set -euo pipefail

cd "$(dirname "$0")/.."

DOC="docs/PRODUCTION-READINESS-100.md"
OUT=""

while [ $# -gt 0 ]; do
  case "$1" in
    --out) OUT="$2"; shift 2 ;;
    *) echo "Unknown argument: $1" >&2; exit 1 ;;
  esac
done

if [ ! -f "$DOC" ]; then
  echo "FAIL: $DOC not found." >&2
  exit 1
fi

count_marker() {
  local marker="$1"
  grep -cE "^[0-9]+\. ${marker}" "$DOC" || true
}

resolved=$(count_marker "✅")
partial=$(count_marker "🟡")
open=$(count_marker "🔴")
critical=$(count_marker "🚨")
total=$((resolved + partial + open + critical))

pct() {
  if [ "$total" -eq 0 ]; then echo "0.0"; else
    awk -v n="$1" -v d="$total" 'BEGIN { printf "%.1f", (n / d) * 100 }'
  fi
}

phase_lines=$(grep -n "^## Phase" "$DOC")

{
  echo "# PledgePack Production-Readiness Scorecard"
  echo ""
  echo "_Generated $(date -u +%Y-%m-%dT%H:%MZ) from \`$DOC\`'s per-goal status markers._"
  echo ""
  echo "**Governing rule (inherited from the source document):** a goal counts"
  echo "as ✅ only once backed by a passing, CI-enforced regression test — not"
  echo "a self-report. See that document for the evidence behind each mark."
  echo ""
  echo "## Overall"
  echo ""
  echo "| Status | Count | % of $total |"
  echo "|--------|-------|-----|"
  echo "| ✅ Resolved | $resolved | $(pct "$resolved")% |"
  echo "| 🟡 Partial | $partial | $(pct "$partial")% |"
  echo "| 🔴 Open | $open | $(pct "$open")% |"
  echo "| 🚨 Open + reproduced bug | $critical | $(pct "$critical")% |"
  echo ""
  echo "## By phase"
  echo ""
  echo "| Phase | ✅ | 🟡 | 🔴 | 🚨 | Total |"
  echo "|-------|----|----|----|----|----|"

  while IFS= read -r line; do
    line_no="${line%%:*}"
    title="${line#*:}"
    title="${title#\#\# }"
    next_phase_line=$(echo "$phase_lines" | awk -F: -v cur="$line_no" '$1 > cur { print $1; exit }')
    if [ -n "$next_phase_line" ]; then
      chunk=$(sed -n "${line_no},$((next_phase_line - 1))p" "$DOC")
    else
      chunk=$(sed -n "${line_no},\$p" "$DOC")
    fi
    p_res=$(echo "$chunk" | grep -cE "^[0-9]+\. ✅" || true)
    p_par=$(echo "$chunk" | grep -cE "^[0-9]+\. 🟡" || true)
    p_open=$(echo "$chunk" | grep -cE "^[0-9]+\. 🔴" || true)
    p_crit=$(echo "$chunk" | grep -cE "^[0-9]+\. 🚨" || true)
    p_total=$((p_res + p_par + p_open + p_crit))
    if [ "$p_total" -gt 0 ]; then
      echo "| $title | $p_res | $p_par | $p_open | $p_crit | $p_total |"
    fi
  done <<< "$phase_lines"

  echo ""
  echo "## Known critical blockers"
  echo ""
  echo "Extracted from the \"Known blockers, not yet resolved\" section of the source document:"
  echo ""
  awk '/^\*\*Known blockers, not yet resolved:\*\*$/{flag=1; next} /^---$/{if(flag) exit} flag' "$DOC"
} > "${OUT:-/dev/stdout}"

if [ -n "$OUT" ]; then
  echo "Scorecard written to $OUT" >&2
  echo "$resolved/$total goals resolved ($(pct "$resolved")%)." >&2
fi
