# Configuration Reference

## CLI Commands

Global options are passed before the subcommand.

```bash
vsync serve
vsync --root /path/to/project --turbo serve --project roblox/default.project.json
vsync --root /path/to/project snapshot
vsync --root /path/to/project build -o place.rbxl --project default.project.json
```

### Global Flags

| Flag | Default | Description |
|------|---------|-------------|
| `--root <path>` | `.` | Workspace root used to resolve relative paths |
| `--state-dir <path>` | `.vertigo-sync-state` | Directory for default snapshot, diff, and event files |
| `--snapshot <path>` | - | Current snapshot JSON path used by `snapshot`, `diff`, and `event` |
| `--previous <path>` | - | Previous snapshot JSON path used by `diff` and `event` |
| `--diff <path>` | - | Diff JSON output path used by `diff` and `event` |
| `--output <path>` | - | Primary output JSON path for `snapshot`, `event`, `doctor`, and `health` |
| `--event-log <path>` | - | JSONL event log path used by `event` |
| `--include <path>` | auto-detected or `src` | Include roots to sync |
| `--interval-seconds <n>` | `2` | Polling interval for `watch` and `serve` modes |
| `--port <port>` | `7575` | HTTP/WebSocket port for `serve` |
| `--address <addr>` | `127.0.0.1` | HTTP bind address for `serve` |
| `--channel-capacity <n>` | `1024` | Broadcast channel capacity for `serve` and `event` fanout |
| `--coalesce-ms <n>` | `50` | Event coalescing window in milliseconds |
| `--turbo` | `false` | Use 10 ms coalescing, 100 ms polling, and native filesystem watch |
| `--json` | `false` | Emit machine-readable JSON instead of human-readable text |

### `serve`

Serve snapshot, diff, and event data over HTTP + SSE.

```bash
vsync --turbo serve --project default.project.json
```

| Flag | Default | Description |
|------|---------|-------------|
| `--project <path>` | `default.project.json` | Project file path |

The server resolves `servePort` and `serveAddress` from the project file when present, falling back to the global `--port` and `--address` flags, then to `7575` and `127.0.0.1`.

### `snapshot`, `diff`, `event`, `doctor`, `health`, `validate`, `watch`, `watch-native`

These commands use the global flags above and do not add command-specific flags.

### `build`

Build a place file from source.

```bash
vsync build -o <output>
```

| Flag | Default | Description |
|------|---------|-------------|
| `-o, --output <path>` | required | Output `.rbxl` or `.rbxlx` file |
| `--project <path>` | `default.project.json` | Project file path |
| `--binary-models` | `false` | Enable binary model (`.rbxm`/`.rbxmx`) processing |

### `syncback`

Extract scripts from a place file back to the filesystem.

```bash
vsync syncback --input place.rbxl
```

| Flag | Default | Description |
|------|---------|-------------|
| `--input <path>` | required | Input `.rbxl` or `.rbxlx` file |
| `--project <path>` | `default.project.json` | Project file used for path mapping |
| `--dry-run` | `false` | Show what would be written without writing |

### `sourcemap`

Generate a Rojo-compatible `sourcemap.json` for luau-lsp.

```bash
vsync sourcemap
```

| Flag | Default | Description |
|------|---------|-------------|
| `--output <path>` | `sourcemap.json` | Output path |
| `--project <path>` | `default.project.json` | Project file path |
| `--include-non-scripts` | `true` | Include non-script instances in the sourcemap |
| `--watch` | `false` | Regenerate automatically when files change |

### `init`

Create a new Vertigo Sync project with a standard directory structure.

```bash
vsync init --name MyProject
```

| Flag | Default | Description |
|------|---------|-------------|
| `--name <name>` | current directory name | Project name |

### `plugin-install`

Install the Studio plugin to the Roblox plugins directory.

```bash
vsync plugin-install
```

The command copies the plugin file to the Roblox user plugins directory for the current platform.

## Project Configuration (`default.project.json`)

Vertigo Sync reads `default.project.json` in the Rojo-compatible format. The project file defines how source directories map to Roblox DataModel locations.

### Schema

```json
{
  "name": "ProjectName",
  "globIgnorePaths": ["generated/**"],
  "emitLegacyScripts": true,
  "servePort": 7575,
  "serveAddress": "127.0.0.1",
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
| `$properties` | Property overrides applied to the generated instance |
| `$attributes` | Attribute overrides applied to the generated instance |

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
