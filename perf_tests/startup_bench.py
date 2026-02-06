#!/usr/bin/env python3
import math
import os
import re
import subprocess
import sys
from statistics import median

FIRST_FRAME_RE = re.compile(r"first_frame_ms=([0-9]+\.[0-9]+)")

def run_once(bin_path: str) -> float:
    env = os.environ.copy()
    env["RALPH_STARTUP_BENCH"] = "1"
    result = subprocess.run(
        [bin_path],
        env=env,
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
        text=True,
        timeout=15,
    )
    match = FIRST_FRAME_RE.search(result.stdout)
    if not match:
        raise RuntimeError(f"missing first_frame_ms in output:\n{result.stdout}")
    return float(match.group(1))


def percentile(sorted_values, pct: float) -> float:
    if not sorted_values:
        raise ValueError("no values")
    idx = max(0, math.ceil(pct * len(sorted_values)) - 1)
    return sorted_values[idx]


def main() -> int:
    runs = 10
    if len(sys.argv) > 1:
        runs = int(sys.argv[1])
    subprocess.run(["cargo", "build", "--quiet"], check=True)
    bin_path = os.path.join("target", "debug", "ralph")
    values = []
    for i in range(runs):
        value = run_once(bin_path)
        values.append(value)
        print(f"run {i + 1}/{runs}: {value:.2f} ms")
    values.sort()
    p50 = median(values)
    p95 = percentile(values, 0.95)
    print(f"p50: {p50:.2f} ms")
    print(f"p95: {p95:.2f} ms")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
