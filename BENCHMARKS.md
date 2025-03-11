# Ralph Performance Benchmarks

## Baseline (February 6, 2026)

Environment: local macOS dev machine, debug build.

### Startup (first frame)

Command:

```bash
perf_tests/startup_bench.py 10
```

Results:
- p50: 276.08 ms
- p95: 928.01 ms

### Memory (max RSS)

Command:

```bash
perf_tests/memory_bench.sh
```

Results:
- max_rss_mb: 101.2 MB
- max_rss_bytes: 106119168
