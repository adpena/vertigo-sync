# Vertigo Sync

Fast, deterministic source sync for Roblox Studio.

![version](https://img.shields.io/badge/version-0.1.0-blue)
![license](https://img.shields.io/badge/license-MIT-green)
![platform](https://img.shields.io/badge/platform-macOS%20%7C%20Linux%20%7C%20Windows-lightgrey)

## Why Vertigo Sync?

Vertigo Sync is a next-generation source synchronization engine for Roblox Studio built in Rust. It provides sub-millisecond cached sync, built-in Luau validation with 36 lint rules, and a full agent-native MCP tool surface -- all in a 5.5 MB binary. If you use Rojo today, Vertigo Sync is a drop-in replacement that is faster, more observable, and designed for the age of AI-assisted development.

| Dimension | Rojo 7.4.4 | Vertigo Sync |
|-----------|-----------|--------------|
| **Cold snapshot (529 files)** | ~50-150 ms | **13.5 ms** |
| **Cached snapshot (0 changes)** | N/A | **9.2 ms** |
| **Diff computation** | Instance-level | **194 us** (file-level) |
| **Transport** | HTTP polling | **WebSocket + SSE + HTTP** |
| **Built-in validation** | None | **36-rule Luau linter** |
| **MCP tools** | None | **31 tools** |
| **Time-travel** | No | **Yes** |
| **Prometheus metrics** | No | **Yes** |
| **Binary size** | ~8-12 MB | **5.5 MB** |

## Quickstart

```bash
# Install
cargo install vertigo-sync

# Start syncing
vertigo-sync serve --turbo

# Install the Studio plugin
vertigo-sync plugin-install
```

Open Roblox Studio -- the plugin connects automatically.

## Features

- **Sub-millisecond sync** -- 13.5 ms cold snapshot, 9.2 ms cached, 194 us diff
- **Frame-budgeted Studio plugin** -- never stalls Studio, 4 ms/frame budget with adaptive scaling
- **Built-in Luau validation** -- 36 rules catching NCG deopt, strict mode violations, deprecated APIs, hot-path allocations
- **WebSocket + SSE + HTTP** -- triple-transport with automatic fallback and lag recovery
- **Time-travel** -- rewind and fast-forward through your sync history with a scrubber UI
- **31 MCP tools** -- full agent-native read/write/validate surface for AI-assisted development
- **Prometheus metrics** -- production observability at `/metrics`
- **Instance pooling** -- pre-allocated instance pool eliminates GC pressure during sync
- **Rojo-compatible** -- works with existing `default.project.json` files, no migration required

## Installation

### From source (recommended)

```bash
cargo install vertigo-sync
```

### Pre-built binaries

**macOS / Linux:**

```bash
curl -fsSL https://github.com/vertigo-sync/vertigo-sync/releases/latest/download/install.sh | sh
```

**Windows (PowerShell):**

```powershell
irm https://github.com/vertigo-sync/vertigo-sync/releases/latest/download/install.ps1 | iex
```

### Homebrew (macOS / Linux)

```bash
brew tap vertigo-sync/tap
brew install vertigo-sync
```

## Studio Plugin

```bash
vertigo-sync plugin-install
```

The plugin provides:

- **Real-time sync status** with connection health, transport mode, and throughput metrics
- **Welcome screen** with setup instructions for first-time users
- **Toast notifications** for sync events (file counts, errors, reconnections)
- **Connection state machine** with clear visual states (waiting, connecting, connected, reconnecting, error)
- **Time-travel scrubber** for navigating sync history with step/jump controls
- **Feature toggles** for binary models, builders, and time-travel UI
- **Persistent settings** across Studio sessions via `plugin:GetSetting()`
- **Instance pooling** -- 128 pre-allocated instances per class, zero `Instance.new()` in the hot path
- **Adaptive frame budget** -- dynamically scales apply rate and fetch concurrency based on Studio frame time

### Plugin Architecture

The plugin runs a 4-stage pipeline on every Heartbeat:

1. **Sync Manager** -- health checks, snapshot reconciliation, WebSocket/poll transport
2. **Fetch Queue** -- concurrent source fetching with batch requests and retry logic
3. **Apply Queue** -- frame-budgeted instance creation/update/deletion with coalescing
4. **Metrics Flush** -- workspace attribute telemetry for external monitoring

## Commands

| Command | Description |
|---------|-------------|
| `vertigo-sync serve` | Start the sync server (default port 7575) |
| `vertigo-sync serve --turbo` | Start with 10 ms FSEvents coalescing (faster sync) |
| `vertigo-sync snapshot` | Print deterministic source tree snapshot |
| `vertigo-sync doctor` | Run determinism and health validation |
| `vertigo-sync validate` | Run Luau source validation (36 rules) |
| `vertigo-sync build -o place.rbxl` | Build a place file from source |
| `vertigo-sync plugin-install` | Install the Studio plugin |

## Migrating from Rojo

Your existing `default.project.json` works as-is. Just change the command:

```bash
# Before (Rojo)
rojo serve default.project.json

# After (Vertigo Sync)
vertigo-sync serve --turbo
```

See [Migration Guide](docs/migration-from-rojo.md) for the full walkthrough.

## Configuration

### CLI Flags

| Flag | Default | Description |
|------|---------|-------------|
| `--root <path>` | `.` | Project root directory |
| `--port <port>` | `7575` | HTTP server port |
| `--turbo` | `false` | Enable turbo mode (10 ms coalescing) |
| `-o <path>` | - | Output path for build command |

### Environment Variables

| Variable | Default | Description |
|----------|---------|-------------|
| `VERTIGO_SYNC_PORT` | `7575` | Override server port |
| `VERTIGO_SYNC_ROOT` | `.` | Override project root |
| `VERTIGO_SYNC_LOG` | `info` | Log level (trace, debug, info, warn, error) |
| `VERTIGO_SYNC_TURBO` | `false` | Enable turbo mode |

### Project Configuration

Vertigo Sync reads `default.project.json` in Rojo-compatible format:

```json
{
  "name": "MyProject",
  "tree": {
    "$className": "DataModel",
    "ServerScriptService": {
      "$className": "ServerScriptService",
      "Server": {
        "$path": "src/Server"
      }
    },
    "StarterPlayer": {
      "$className": "StarterPlayer",
      "StarterPlayerScripts": {
        "$className": "StarterPlayerScripts",
        "Client": {
          "$path": "src/Client"
        }
      }
    },
    "ReplicatedStorage": {
      "$className": "ReplicatedStorage",
      "Shared": {
        "$path": "src/Shared"
      },
      "Packages": {
        "$path": "Packages"
      }
    }
  }
}
```

## Architecture

```
filesystem ──► vertigo-sync server ──► Studio plugin
                   │
                   ├── GET /health
                   ├── GET /snapshot
                   ├── GET /diff?since=<hash>
                   ├── GET /source/<path>
                   ├── GET /sources
                   ├── GET /sources/content?paths=<csv>
                   ├── GET /events (SSE)
                   ├── GET /ws (WebSocket)
                   ├── GET /validate
                   ├── GET /metrics
                   ├── GET /history?limit=N
                   ├── GET /rewind?to=<hash>
                   ├── GET /config
                   └── POST /sync/patch
```

**How it works:**

1. `vertigo-sync` watches your source tree using native FSEvents (macOS) or inotify (Linux) with configurable coalescing
2. On each change, it rebuilds a content-addressed snapshot using SHA-256 hashes and mtime/size caching
3. The Studio plugin connects via WebSocket (primary), SSE (secondary), or HTTP polling (tertiary)
4. File changes are delivered as diffs, fetched concurrently, and applied within a frame-budgeted loop
5. The snapshot is deterministic: same source tree always produces the same fingerprint

## Performance

All benchmarks on Apple Silicon, 529-file synthetic Vertigo project:

| Operation | Time |
|-----------|------|
| Cold snapshot (529 files) | 13.5 ms |
| Cached snapshot (0 changes) | 9.2 ms |
| Cached snapshot (1 change) | 9.9 ms |
| Cached snapshot (10 changes) | 14.9 ms |
| Diff computation (20 modified) | 194 us |
| JSON serialization (529 entries) | 50 us |
| Source validation (529 files) | 30.6 ms |
| Health doctor (determinism check) | 60.3 ms |

### Scaling

| Files | Cold Snapshot |
|-------|---------------|
| 100 | 3.6 ms |
| 250 | 6.4 ms |
| 529 | 10.6 ms |
| 1000 | 290 ms |

Scaling is roughly linear at ~20 us/file up to the ~500 file range.

## API Reference

### HTTP Endpoints

| Endpoint | Method | Description |
|----------|--------|-------------|
| `/health` | GET | Server health probe |
| `/snapshot` | GET | Full deterministic snapshot with file hashes |
| `/diff?since=<hash>` | GET | Incremental diff since a previous snapshot |
| `/source/<path>` | GET | Raw source file content (SHA-256 in `X-SHA256` header) |
| `/sources` | GET | File listing with paths and hashes |
| `/sources/content?paths=<csv>` | GET | Batch source content fetch |
| `/events` | GET | SSE stream of sync events |
| `/ws` | GET | WebSocket (bidirectional, lag recovery) |
| `/validate` | GET | Luau lint report |
| `/metrics` | GET | Prometheus metrics |
| `/history?limit=N` | GET | Recent sync history entries |
| `/rewind?to=<hash>` | GET | Rewind to a previous snapshot |
| `/config` | GET | Server configuration |
| `/sync/patch` | POST | Apply patches (Studio to disk) |

### WebSocket Messages

| Type | Direction | Description |
|------|-----------|-------------|
| `connected` | Server to Client | Initial connection with current fingerprint |
| `sync_diff` | Server to Client | File changes with paths and hashes |
| `lagged` | Server to Client | Client fell behind; triggers snapshot resync |

### Patch Ack/Reject

All patch operations return deterministic ack/reject envelopes with structured reason codes:

`ok`, `hash_mismatch`, `sequence_gap`, `path_out_of_scope`, `class_mismatch`, `missing_source`, `apply_budget_exceeded`, `stale_snapshot`, `auth_failed`, `transport_unavailable`, `internal_error`

## MCP Tools

Vertigo Sync exposes 31 MCP tools for agent-native source manipulation. See the [Agent DSL Reference](../../docs/vertigo-sync-agent-dsl.md) for the full catalog.

### Categories

| Category | Tools | Description |
|----------|-------|-------------|
| **Read** | 13 | `vsync_health`, `vsync_snapshot`, `vsync_diff`, `vsync_source`, `vsync_grep`, ... |
| **Write** | 5 | `vsync_write`, `vsync_patch`, `vsync_delete`, `vsync_move`, `vsync_mkdir` |
| **Validate** | 3 | `vsync_validate`, `vsync_validate_content`, `vsync_check_conflict` |
| **Pipeline** | 3 | `vsync_safe_write`, `vsync_describe_changes`, `vsync_pipeline` |
| **Observe** | 4 | `vsync_metrics`, `vsync_doctor`, `vsync_status`, `vsync_events` |
| **Bridge** | 3 | `vsync_bridge_manifest`, `vsync_bridge_execute`, `vsync_bridge_batch` |

### Example: Agent Workflow

```
vsync_source("src/Server/Services/DataService.luau")   # Read
  -> vsync_validate_content(path, new_content)          # Lint in-memory
  -> vsync_safe_write(path, new_content)                # Atomic write
  -> vsync_describe_changes(since_hash)                 # Summary
```

## Validation Rules

The built-in Luau validator checks 36 rules across these categories:

- **Strict mode** -- missing `--!strict` directive
- **NCG optimization** -- missing `@native` on hot-path functions, closures in loops
- **Deprecated APIs** -- usage of deprecated Roblox APIs
- **Hot-path allocations** -- `Instance.new()` in Heartbeat/RenderStepped callbacks
- **Cross-boundary requires** -- invalid require paths across service boundaries
- **Performance patterns** -- `gmatch`/`gsub` in hot paths, non-SIMD vector math

## License

MIT
