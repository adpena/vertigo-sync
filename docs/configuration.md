# Configuration Reference

## CLI Commands

### `vertigo-sync serve`

Start the sync server.

```bash
vertigo-sync [OPTIONS] serve [SERVE_OPTIONS]
```

| Flag | Default | Description |
|------|---------|-------------|
| `--root <path>` | `.` | Project root directory (must contain `default.project.json`) |
| `--port <port>` | `7575` | HTTP/WebSocket server port |
| `--turbo` | `false` | Enable turbo mode: reduces FSEvents coalescing from 50 ms to 10 ms |

**Examples:**

```bash
# Default: serve current directory on port 7575
vertigo-sync serve

# Turbo mode with custom root
vertigo-sync --root /path/to/project serve --turbo

# Custom port
vertigo-sync serve --port 8080
```

### `vertigo-sync snapshot`

Print the current source tree snapshot to stdout as JSON.

```bash
vertigo-sync [OPTIONS] snapshot
```

| Flag | Default | Description |
|------|---------|-------------|
| `--root <path>` | `.` | Project root directory |

The snapshot includes every source file with its path, SHA-256 hash, byte size, and file type. The output is deterministic: same source tree always produces the same fingerprint.

### `vertigo-sync doctor`

Run determinism and health validation.

```bash
vertigo-sync [OPTIONS] doctor
```

| Flag | Default | Description |
|------|---------|-------------|
| `--root <path>` | `.` | Project root directory |

Doctor runs two full snapshot passes and verifies that both produce the same fingerprint. It also checks source tree analysis, file integrity, and project configuration.

### `vertigo-sync validate`

Run Luau source validation with 36 built-in rules.

```bash
vertigo-sync [OPTIONS] validate
```

| Flag | Default | Description |
|------|---------|-------------|
| `--root <path>` | `.` | Project root directory |

Returns a JSON report with file paths, line numbers, severity levels, messages, and rule names.

### `vertigo-sync build`

Build a place file from source.

```bash
vertigo-sync [OPTIONS] build -o <output>
```

| Flag | Default | Description |
|------|---------|-------------|
| `--root <path>` | `.` | Project root directory |
| `-o <path>` | (required) | Output file path (`.rbxl` or `.rbxlx`) |

### `vertigo-sync plugin-install`

Install the Studio plugin to the Roblox plugins directory.

```bash
vertigo-sync plugin-install
```

Copies the plugin file to `~/Documents/Roblox/Plugins/VertigoSyncPlugin.lua` on macOS/Linux or the equivalent Windows path.

## Environment Variables

| Variable | Default | Description |
|----------|---------|-------------|
| `VERTIGO_SYNC_PORT` | `7575` | Override the server port (equivalent to `--port`) |
| `VERTIGO_SYNC_ROOT` | `.` | Override the project root (equivalent to `--root`) |
| `VERTIGO_SYNC_LOG` | `info` | Log level: `trace`, `debug`, `info`, `warn`, `error` |
| `VERTIGO_SYNC_TURBO` | `false` | Set to `true` to enable turbo mode (equivalent to `--turbo`) |

CLI flags take precedence over environment variables.

## Project Configuration (`default.project.json`)

Vertigo Sync reads `default.project.json` in the Rojo-compatible format. The project file defines how source directories map to Roblox DataModel locations.

### Schema

```json
{
  "name": "ProjectName",
  "servePlaceIds": [123456789],
  "placeId": 123456789,
  "gameId": 987654321,
  "tree": {
    "$className": "DataModel",
    "ServiceName": {
      "$className": "ServiceClassName",
      "ChildName": {
        "$path": "relative/path/to/source"
      }
    }
  }
}
```

### Directives

| Directive | Description |
|-----------|-------------|
| `$className` | The Roblox class name for this tree node |
| `$path` | Relative path to a source directory or file |
| `$ignoreUnknownInstances` | When `true`, preserves instances not managed by sync |

### File Type Mapping

| File Pattern | Roblox Class |
|-------------|--------------|
| `*.server.luau` / `init.server.luau` | `Script` |
| `*.client.luau` / `init.client.luau` | `LocalScript` |
| `*.luau` / `init.luau` | `ModuleScript` |
| `*.server.lua` / `init.server.lua` | `Script` |
| `*.client.lua` / `init.client.lua` | `LocalScript` |
| `*.lua` / `init.lua` | `ModuleScript` |
| `*.json` | `ModuleScript` (source = JSON content) |
| `*.txt` | `StringValue` (Value = file content) |
| `*.csv` | `LocalizationTable` |
| `*.rbxm` / `*.rbxmx` | Binary/XML model (feature-gated) |

### Meta Files (`.meta.json`)

Place a `.meta.json` file next to any source file to set instance properties and attributes:

```json
{
  "properties": {
    "Disabled": true
  },
  "attributes": {
    "Version": 2,
    "Category": "combat"
  }
}
```

## Studio Plugin Settings

The plugin persists these settings across Studio sessions using `plugin:GetSetting()` / `plugin:SetSetting()`:

| Setting Key | Default | Description |
|-------------|---------|-------------|
| `VertigoSyncBinaryModels` | `false` | Enable binary model (`.rbxm`/`.rbxmx`) instance creation |
| `VertigoSyncBuildersEnabled` | `true` | Enable builder execution in edit mode |
| `VertigoSyncTimeTravelUI` | `true` | Show time-travel panel in the DockWidget |
| `VertigoSyncHistoryBuffer` | `256` | Maximum history entries (16-1024) |

These settings can also be toggled via Workspace attributes for external control:

```lua
workspace:SetAttribute("VertigoSyncBinaryModels", true)
workspace:SetAttribute("VertigoSyncBuildersEnabled", false)
```

## Plugin Telemetry Attributes

The plugin writes these Workspace attributes for external monitoring:

| Attribute | Type | Description |
|-----------|------|-------------|
| `VertigoSyncStatus` | string | `connected`, `disconnected`, or `error` |
| `VertigoSyncHash` | string | Current snapshot fingerprint |
| `VertigoSyncLastUpdate` | string | ISO 8601 timestamp of last update |
| `VertigoSyncTransport` | string | `ws`, `poll`, or `idle` |
| `VertigoSyncQueueDepth` | number | Pending apply operations |
| `VertigoSyncFetchQueueDepth` | number | Pending source fetches |
| `VertigoSyncFetchInFlight` | number | Active concurrent fetches |
| `VertigoSyncLaggedEvents` | number | WebSocket lag recovery count |
| `VertigoSyncDroppedUpdates` | number | Dropped updates (queue overflow) |
| `VertigoSyncReconnects` | number | WebSocket reconnection count |
| `VertigoSyncAppliedPerSecond` | number | Apply throughput |
| `VertigoSyncApplyBudgetMs` | number | Current adaptive frame budget (ms) |
| `VertigoSyncApplyMaxPerTick` | number | Current max applies per tick |
| `VertigoSyncFetchConcurrency` | number | Current fetch worker count |
| `VertigoSyncApplyCostUs` | number | EWMA apply cost per operation (us) |
| `VertigoSyncPluginVersion` | string | Plugin version string |
| `VertigoSyncRealtimeDefault` | boolean | Always `true` |
| `VertigoSyncBinaryModels` | boolean | Binary models feature gate |
| `VertigoSyncBuildersEnabled` | boolean | Builders feature gate |
| `VertigoSyncTimeTravel` | boolean | Time-travel mode active |

## Plugin Tuning Constants

These constants are defined at the top of `VertigoSyncPlugin.lua` and can be adjusted for different workload profiles:

| Constant | Default | Description |
|----------|---------|-------------|
| `HEALTH_POLL_SECONDS` | `5` | Health check interval |
| `POLL_INTERVAL_FAST` | `0.10` | Initial poll interval (seconds) |
| `POLL_INTERVAL_MAX` | `1.50` | Maximum poll backoff (seconds) |
| `APPLY_FRAME_BUDGET_SECONDS` | `0.004` | Target frame budget for apply loop (4 ms) |
| `MAX_APPLIES_PER_TICK` | `96` | Maximum instance operations per Heartbeat |
| `MAX_FETCH_CONCURRENCY` | `24` | Maximum concurrent HTTP source fetches |
| `MAX_SOURCE_BATCH_SIZE` | `24` | Maximum files per batch content request |
| `MAX_SOURCE_FETCH_RETRIES` | `3` | Retry count for failed source fetches |
| `POOL_SIZE` | `128` | Pre-allocated instances per class in the pool |
| `APPLY_QUEUE_HARD_CAP` | `2048` | Queue overflow threshold (triggers resync) |
| `WS_RECONNECT_MIN_SECONDS` | `0.25` | Minimum WebSocket reconnect backoff |
| `WS_RECONNECT_MAX_SECONDS` | `5.0` | Maximum WebSocket reconnect backoff |
| `TIME_TRAVEL_HISTORY_LIMIT` | `256` | Default history buffer size |
