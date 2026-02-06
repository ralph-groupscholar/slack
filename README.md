# Ralph

A blazingly fast, native team communication app built in Rust. Think "Ghostty for terminals" but for team chat — instant startup, minimal resource usage, and snappy UI interactions.

## Features

- **Channels & Direct Messages** — create and switch between group channels and 1:1 DMs via a sidebar
- **Real-time Messaging** — WebSocket-based sync with a JSON message protocol, auth handshake, acks, and presence updates
- **Message Persistence** — local SQLite store for offline history and fast reads
- **Rich Text** — inline bold, italic, and code formatting in message bodies
- **File Attachments** — attach local files, persist metadata, and preview image thumbnails with async background decoding
- **Search** — SQLite-backed message search with per-channel scoping
- **Presence & Typing Indicators** — real-time online/away status and per-channel typing state
- **Mock Server** — bundled WebSocket echo/broadcast server for local development and integration testing

## Performance

Benchmarked on a macOS dev machine (debug build):

| Metric | Result |
| --- | --- |
| Cold startup (p50) | 209.63 ms |
| Cold startup (p95) | 329.22 ms |
| Memory (max RSS) | 97.7 MB |
| Idle CPU | 3.53% avg |

Key optimizations: deferred SQLite and background hydration, Metal-only wgpu backend, event-driven repaint loop, capped thumbnail caches with FIFO eviction, and low-power GPU adapter selection.

## Prerequisites

- **Rust toolchain** — `cargo` + `rustc` (install via [rustup](https://rustup.rs))
- **macOS** — the app targets Metal for GPU rendering

## Build & Run

```bash
cargo run
```

Release build (recommended for performance):

```bash
cargo run --release
```

### Mock WebSocket Server

Start the bundled mock server for local testing:

```bash
cargo run --bin mock_server
```

The server listens on `ws://127.0.0.1:9001` and echoes/broadcasts messages to all connected clients.

## Architecture

```
src/
├── main.rs             # App entrypoint, UI, state, and all client logic
└── bin/
    └── mock_server.rs  # Local WebSocket mock server
perf_tests/
├── startup_bench.py    # Startup time measurement (p50/p95 over N runs)
├── memory_bench.sh     # Max RSS via /usr/bin/time
└── idle_cpu_bench.sh   # Idle CPU sampling via ps
```

### Tech Stack

| Layer | Choice |
| --- | --- |
| Language | Rust |
| UI | egui (immediate mode) |
| Rendering | wgpu (Metal backend) |
| Windowing | winit |
| Local storage | SQLite (rusqlite, bundled) |
| Real-time | WebSocket (tungstenite) |
| Serialization | serde + serde_json |
| Image decoding | image crate (gif, jpeg, png, webp) |

### Design Principles

- **Deferred initialization** — SQLite, channel seeding, and message loading happen on background threads after the first frame renders
- **Event-driven rendering** — the UI only repaints when background workers signal changes, keeping idle CPU low
- **Capped caches** — thumbnail and error caches use FIFO eviction to bound memory growth
- **Minimal dependencies** — every dependency is justified; no JIT, no Electron, no web views

## Configuration

| Environment Variable | Description |
| --- | --- |
| `RALPH_STARTUP_BENCH` | Set to `1` to exit after the first frame (used by benchmark scripts) |

- The app uses a local SQLite file `ralph.db` in the repo root. If it cannot be opened, it falls back to an in-memory database.
- The WebSocket client defaults to `ws://127.0.0.1:9001`.

## Running Benchmarks

```bash
# Startup time (10 runs, reports p50/p95)
python3 perf_tests/startup_bench.py 10

# Memory usage (max RSS)
./perf_tests/memory_bench.sh

# Idle CPU (10 samples over 10s)
./perf_tests/idle_cpu_bench.sh
```

Detailed results are tracked in [BENCHMARKS.md](BENCHMARKS.md).

## License

This project is not currently published under an open-source license.
