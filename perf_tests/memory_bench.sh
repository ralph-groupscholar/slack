#!/bin/bash
set -euo pipefail

cargo build --quiet

OUTPUT=$({ /usr/bin/time -l env RALPH_STARTUP_BENCH=1 target/debug/ralph >/dev/null; } 2>&1)
RSS_BYTES=$(echo "$OUTPUT" | awk '/maximum resident set size/ {print $1}')
if [[ -z "$RSS_BYTES" ]]; then
  echo "Failed to capture max RSS." >&2
  echo "$OUTPUT" >&2
  exit 1
fi
RSS_MB=$(awk -v bytes="$RSS_BYTES" 'BEGIN { printf "%.1f", bytes / 1024 / 1024 }')

echo "max_rss_bytes: $RSS_BYTES"
echo "max_rss_mb: $RSS_MB"
