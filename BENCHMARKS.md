# Ralph Performance Benchmarks

## Baseline (February 6, 2026)

Environment: local macOS dev machine, debug build.

### Startup (first frame)

Command:

```bash
perf_tests/startup_bench.py 10
```

Results:
- p50: 218.37 ms
- p95: 411.36 ms

### Memory (max RSS)

Command:

```bash
perf_tests/memory_bench.sh
```

Results:
- max_rss_mb: 97.7 MB
- max_rss_bytes: 102449152
