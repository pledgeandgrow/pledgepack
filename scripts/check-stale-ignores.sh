#!/usr/bin/env bash
# Fails if any `# review-by: YYYY-MM-DD` marker in .cargo/audit.toml / deny.toml /
# Cargo.toml has passed. Originally just the ignored-RUSTSEC-advisory markers
# (goal 6); also covers the lightningcss/noyalib tracked-risk dependency
# notes in Cargo.toml (goals 23-24) — same mechanism, same reasoning: an
# "accept this risk for now" decision can't silently become permanent. Run
# in CI's `audit` job.
set -euo pipefail

today=$(date -u +%Y-%m-%d)
stale=0

for file in .cargo/audit.toml deny.toml Cargo.toml; do
  while IFS= read -r line; do
    date_str=$(echo "$line" | sed -n 's/.*review-by: \([0-9]\{4\}-[0-9]\{2\}-[0-9]\{2\}\).*/\1/p')
    if [ -n "$date_str" ] && [[ "$date_str" < "$today" ]]; then
      echo "STALE: $file has a review-by date in the past: $date_str"
      echo "  -> $line"
      stale=1
    fi
  done < <(grep -n "review-by:" "$file" || true)
done

if [ "$stale" -ne 0 ]; then
  echo ""
  echo "One or more ignored advisories are past their review date."
  echo "Re-evaluate each one (cargo audit / cargo deny check advisories) and"
  echo "either bump its review-by date with a note on why it's still ignored,"
  echo "or remove the ignore now that it's fixable."
  exit 1
fi

echo "All review-by dates in .cargo/audit.toml/deny.toml/Cargo.toml are still current."
