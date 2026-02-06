#!/bin/bash
set -euo pipefail

cargo build --quiet

BIN=target/debug/ralph
OUTPUT=$(mktemp)

"$BIN" >/dev/null 2>&1 &
PID=$!

cleanup() {
  if kill -0 "$PID" 2>/dev/null; then
    kill "$PID" 2>/dev/null || true
  fi
  wait "$PID" 2>/dev/null || true
  rm -f "$OUTPUT"
}
trap cleanup EXIT

sleep 3

samples=10
interval=1
values=()

for _ in $(seq 1 "$samples"); do
  if ! kill -0 "$PID" 2>/dev/null; then
    echo "Process exited before sampling completed." >&2
    exit 1
  fi
  cpu=$(ps -p "$PID" -o %cpu= | awk '{print $1}')
  if [[ -z "$cpu" ]]; then
    echo "Failed to read CPU usage." >&2
    exit 1
  fi
  values+=("$cpu")
  sleep "$interval"
done

avg=$(printf "%s\n" "${values[@]}" | awk '{sum += $1; count += 1} END { if (count > 0) printf "%.2f", sum / count; }')

printf "avg_cpu_percent: %s\n" "$avg"
