# Build a High-Performance Slack Alternative

## Vision

Create a blazingly fast team communication app that prioritizes instant startup, minimal resource usage, and snappy UI interactions. Think "Ghostty for terminals" but for team chat.

## Latest Update (February 5, 2026)

- Initialized Rust build system with minimal binary entrypoint (`Cargo.toml`, `src/main.rs`).
- Added a minimal native window using winit to validate basic UI rendering (`Cargo.toml`, `src/main.rs`).
- Chose egui + wgpu rendering and drew a placeholder message list in the window (`Cargo.toml`, `src/main.rs`).
- Added a lightweight message model and rendered messages from structured data (`src/main.rs`).
- Added a local SQLite message store with schema + seed data, and load messages from the database (`Cargo.toml`, `src/main.rs`).
- Added a minimal message composer and persisted new messages into SQLite (`src/main.rs`).
- Improved composer UX: send on Enter while focused and keep focus after send (`src/main.rs`).
- Added channel/DM scaffolding with a sidebar, channel table, and per-channel message filtering (`src/main.rs`).
- Added per-channel composer metadata with placeholder text and a typing indicator stub (`src/main.rs`).
- Added per-channel typing indicator state updates keyed to recent local input (`src/main.rs`).
- Added WebSocket client scaffolding with a background worker, connection status UI, and connect/disconnect controls (`Cargo.toml`, `src/main.rs`).
- Hooked WebSocket send/receive into message flows with inbound persistence and UI updates (`src/main.rs`).
- Added a JSON-based realtime message protocol with legacy fallback parsing for WebSocket sync (`Cargo.toml`, `src/main.rs`).

## Core Requirements

### 1. Performance Targets (CRITICAL)

- **Cold startup**: < 200ms from launch to usable UI
- **Hot startup**: < 50ms
- **Message send latency**: < 100ms local echo
- **Memory footprint**: < 100MB with 10 channels loaded
- **CPU idle**: < 1% when not actively receiving messages

### 2. Essential Features

**Phase 1 - MVP:**

- Direct messages (1:1 chat)
- Channels (group chat)
- Real-time message sync
- Message history (local + cloud)
- Basic rich text (bold, italic, code blocks)
- File attachments (images, documents)
- Desktop notifications
- Search messages

**Phase 2 - Polish:**

- Threads/replies
- Reactions (emoji)
- User presence (online/away/offline)
- Typing indicators
- Message editing/deletion
- @mentions

### 3. Technical Stack

**Language Options (pick the best for performance):**

- **Rust** - Memory safety + speed (recommend: Tauri or native)
- **Zig** - Low-level control, fast compile times
- **C++** - Maximum performance if needed

**UI Framework (fast native rendering):**

- Tauri (Rust + web) - if acceptable performance
- Native macOS (Swift/AppKit) with Rust backend
- egui (immediate mode, Rust-native)
- Custom OpenGL/Metal rendering if needed

**Backend/Sync:**

- WebSocket or gRPC for real-time
- SQLite for local message cache
- Efficient binary protocol (protobuf/flatbuffers)

### 4. Architecture Principles

- **Zero-copy message parsing** where possible
- **Lazy loading**: Only render visible messages
- **Efficient data structures**: Arena allocation, object pools
- **Native threading**: Don't block the main thread
- **Minimal dependencies**: Every dep adds startup cost
- **AOT compilation**: No JIT startup penalty

## Quality Standards

### Performance Benchmarks

Create `perf_tests/` with:

- Startup time measurement (10 runs, report p50/p95)
- Message throughput test (1000 messages)
- Memory profiling script
- UI responsiveness test (60fps scroll test)

### Code Quality

- No panics in release builds
- Profile-guided optimization enabled
- Comprehensive error handling
- Clean separation: UI / business logic / network

### Deliverables

```
slack-alt/
├── src/           # Source code
├── perf_tests/    # Performance benchmarks
├── README.md      # Build instructions + architecture
├── BENCHMARKS.md  # Performance results
└── build/         # Compiled binary
```

## Implementation Strategy

### Iteration 1-3: Core Architecture

- Set up build system (Cargo/Zig/CMake)
- Implement basic UI window
- Local message storage (SQLite schema)
- Basic message list rendering

### Iteration 4-6: Real-time Sync

- WebSocket client
- Message protocol (send/receive)
- Efficient UI updates (incremental rendering)
- Background sync worker

### Iteration 7-10: Features & Polish

- File attachments
- Search implementation
- Notifications
- Settings/preferences

### Iteration 11-15: Performance Optimization

- Profile with instruments/flamegraph
- Optimize hot paths
- Reduce startup time
- Memory optimization

### Iteration 16-20: Testing & Documentation

- Run all benchmarks
- Document performance results
- Build instructions
- Demo video/screenshots

## Important Loop Instructions

**CRITICAL - Context Preservation:**

After EACH iteration, update `progress.txt` with:

- **What you built**: Specific features/files added
- **Performance wins**: Any optimizations made
- **Current metrics**: Startup time, memory usage (if measured)
- **Next priority**: What to tackle next iteration
- **Blockers**: Any issues encountered
- **Key decisions**: Framework choices, architecture decisions

**ALWAYS:**

1. Read `progress.txt` FIRST to see what's done
2. Check git log to understand recent changes
3. Build on existing code - don't restart from scratch
4. Prioritize performance in every decision
5. Test that it actually builds and runs on macOS

**Performance-First Decision Making:**

- When choosing between two approaches, pick the faster one
- Profile before optimizing, but optimize aggressively
- Measure startup time after major changes
- Question every dependency: "Do we really need this?"

## Progress Tracking

Use this format in `progress.txt`:

```markdown
## Iteration N - [Date]

**Built:** [What was implemented]
**Performance:** [Startup: Xms, Memory: YMB]
**Files changed:** [List key files]
**Next:** [What to do next]
```

## Completion Criteria

The project is DONE when ALL of these are true:

✅ **Functional:**

- Can send/receive messages in real-time
- Channels and DMs work
- Messages persist locally
- Search works
- File attachments work

✅ **Performance:**

- Startup < 200ms (cold)
- Memory < 100MB (with 10 channels)
- Runs smoothly on macOS

✅ **Quality:**

- Builds without errors
- No crashes during basic usage
- README with build instructions
- Performance benchmarks documented

When ALL criteria met, output:

<promise>DONE</promise>

## Fallback Strategy

If blocked after 10 iterations:

1. Document what's working
2. Document blockers
3. Suggest simplified approach
4. Output: `<promise>BLOCKED: [specific reason]</promise>`

## Notes

- **Focus on the experience**: Fast startup is more important than feature completeness
- **Iterate incrementally**: Working slow app → Working fast app → Featured fast app
- **Measure everything**: You can't improve what you don't measure
- **macOS native is OK**: Don't need cross-platform if it hurts performance
- **Ship something usable**: MVP > Perfect

Start with the simplest possible UI that can send a message. Then make it fast. Then add features.
