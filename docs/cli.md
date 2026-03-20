# CLI Reference

## Global flags

Global flags are passed before the subcommand.

```bash
vsync [global flags] <command> [command flags]
```

| Flag | Default | Description |
|------|---------|-------------|
| `--root <path>` | `.` | Workspace root used to resolve include paths and relative output paths. |
| `--state-dir <path>` | `.vertigo-sync-state` | Directory for snapshot, diff, and event state files. |
| `--snapshot <path>` | -- | Override path for the current snapshot JSON file. |
| `--previous <path>` | -- | Override path for the previous snapshot JSON file. |
| `--diff <path>` | -- | Override path for diff JSON output. |
| `--output <path>` | -- | Override path for primary output JSON (used by `snapshot`, `event`, `doctor`, `health`). |
| `--event-log <path>` | -- | Override path for JSONL event log. |
| `--include <path>` | auto-detected | Include roots to sync. Auto-detected from `$path` entries in the project file, or defaults to `src`. Comma-separated or repeated. |
| `--interval-seconds <n>` | `2` | Polling interval in seconds for `watch` and `serve` modes. |
| `--port <port>` | `7575` | HTTP/WebSocket port for `serve`. Overrides `servePort` in the project file. |
| `--address <addr>` | `127.0.0.1` | HTTP bind address for `serve`. Overrides `serveAddress` in the project file. |
| `--channel-capacity <n>` | `1024` | Broadcast channel capacity for `serve` and `event` fanout. |
| `--coalesce-ms <n>` | `50` | Event coalescing window in milliseconds. |
| `--turbo` | `false` | Shortcut for 10 ms coalescing, 100 ms polling, and native filesystem watch. |
| `--json` | `false` | Emit machine-readable JSON instead of human-readable text. |

## Commands

### serve

Start the sync server. Watches the source tree and serves snapshot, diff, and event data over HTTP, WebSocket, and SSE.

```bash
vsync serve
vsync --turbo serve
vsync serve --project path/to/default.project.json
```

**Flags:**

| Flag | Default | Description |
|------|---------|-------------|
| `--project <path>` | `default.project.json` | Project file path. |

The server resolves port and address from: CLI `--port`/`--address` flags, then `servePort`/`serveAddress` from the project file, then defaults (`7575` / `127.0.0.1`).

**Example:**

```bash
# Serve a nested project from a monorepo root
vsync --root . --turbo serve --project roblox/default.project.json
```

### init

Create a new project with standard directory structure. Generates `default.project.json`, `vsync.toml`, starter scripts, and a `.gitignore`.

```bash
vsync init
vsync init --name my-game
```

**Flags:**

| Flag | Default | Description |
|------|---------|-------------|
| `--name <name>` | current directory name | Project name. |

Existing files are never overwritten.

### build

Build a `.rbxl` or `.rbxlx` place file from the source tree.

```bash
vsync build -o game.rbxl
vsync build -o game.rbxl --project default.project.json --binary-models
```

**Flags:**

| Flag | Default | Description |
|------|---------|-------------|
| `-o, --output <path>` | required | Output file path (`.rbxl` or `.rbxlx`). |
| `--project <path>` | `default.project.json` | Project file path. |
| `--binary-models` | `false` | Enable binary model (`.rbxm`/`.rbxmx`) processing. |

### syncback

Extract scripts from a place file back to the filesystem. The inverse of `build`.

```bash
vsync syncback --input game.rbxl
vsync syncback --input game.rbxl --dry-run
```

**Flags:**

| Flag | Default | Description |
|------|---------|-------------|
| `-i, --input <path>` | required | Input `.rbxl` or `.rbxlx` file. |
| `--project <path>` | `default.project.json` | Project file used for path mapping. |
| `--dry-run` | `false` | Show what would be written without writing. |

### sourcemap

Generate a Rojo-compatible `sourcemap.json` for luau-lsp. Provides require resolution, type checking, and autocomplete without Rojo running.

```bash
vsync sourcemap
vsync sourcemap --watch
vsync sourcemap --output custom-sourcemap.json
```

**Flags:**

| Flag | Default | Description |
|------|---------|-------------|
| `-o, --output <path>` | `sourcemap.json` | Output path. |
| `--project <path>` | `default.project.json` | Project file path. |
| `--include-non-scripts` | `true` | Include non-script instances (Folders, StringValues, etc.) in the sourcemap. |
| `--watch` | `false` | Regenerate automatically when files change. |

### validate

Run built-in Luau source validation on all files in the project.

```bash
vsync validate
vsync validate --project path/to/default.project.json
```

**Flags:**

| Flag | Default | Description |
|------|---------|-------------|
| `--project <path>` | `default.project.json` | Project file path. |

Reports include file paths, line numbers, severity, and rule names. Also runs plugin safety validation on the generated Studio plugin.

**Example output:**

```
src/Server/Services/MyService.luau:1: error: missing --!strict directive [strict-mode]
src/Client/Controllers/Input.luau:42: warning: Instance.new in hot path [instance-new-hot-path]
```

### fmt

Format Luau source files using the built-in StyLua-powered formatter. Reads formatting options from `[format]` in `vsync.toml`.

```bash
vsync fmt
vsync fmt --check
vsync fmt --diff
vsync fmt src/Server/Services/MyService.luau
```

**Flags:**

| Flag | Default | Description |
|------|---------|-------------|
| `--check` | `false` | Check formatting without writing changes. Exits with code 1 if any file is unformatted. |
| `--diff` | `false` | Print a unified diff for each file that would change. |
| `--project <path>` | `default.project.json` | Project file path. |
| `<path>` | all project includes | Specific file or directory to format. |

### install

Install packages declared in `vsync.toml` into the `Packages/` directory (or the directory specified by `packages-dir`).

```bash
vsync install
```

**Flags:**

| Flag | Default | Description |
|------|---------|-------------|
| `--project <path>` | `default.project.json` | Project file path. |

### add

Add a dependency to `vsync.toml` and install it.

```bash
vsync add roblox/roact@^17.0.0
vsync add Roact roblox/roact@^17.0.0      # with custom alias
vsync add roblox/roact@^17.0.0 --server   # server dependency
vsync add roblox/testez@^0.4.0 --dev      # dev dependency
```

**Positional arguments:**

- `<spec>` -- package specification in `scope/name@version-req` format
- `<alias> <spec>` -- optional alias followed by package specification

**Flags:**

| Flag | Default | Description |
|------|---------|-------------|
| `--server` | `false` | Add to `[server-dependencies]` instead of `[dependencies]`. |
| `--dev` | `false` | Add to `[dev-dependencies]` instead of `[dependencies]`. |
| `--project <path>` | `default.project.json` | Project file path. |

### remove

Remove a dependency from `vsync.toml`, delete it from `Packages/`, and update the lockfile.

```bash
vsync remove roact
```

**Positional arguments:**

- `<package>` -- alias name of the package to remove

**Flags:**

| Flag | Default | Description |
|------|---------|-------------|
| `--project <path>` | `default.project.json` | Project file path. |

### run

Run a named script defined in the `[scripts]` section of `vsync.toml`.

```bash
vsync run test
vsync run deploy
```

**Positional arguments:**

- `<name>` -- script name as defined in `vsync.toml`

Scripts execute in a shell with `VSYNC_PROJECT_ROOT` and `VSYNC_PROJECT_NAME` environment variables set.

### migrate

Convert Rojo ecosystem configuration files into a single `vsync.toml`. Reads:

- `wally.toml` -- package metadata and dependencies
- `selene.toml` -- lint configuration
- `stylua.toml` -- formatting configuration

Detects `aftman.toml`/`foreman.toml` and reports their presence.

```bash
vsync migrate
```

Does nothing if `vsync.toml` already exists.

### discover

Print the active project identity and, if reachable, the bound server identity. Useful for debugging connection issues.

```bash
vsync discover
vsync discover --project path/to/default.project.json
vsync discover --server-url http://127.0.0.1:8080
```

**Flags:**

| Flag | Default | Description |
|------|---------|-------------|
| `--project <path>` | `default.project.json` | Project file path. |
| `--server-url <url>` | derived from project | Server URL to inspect. Defaults to the URL derived from `serveAddress`/`servePort` in the project file. |

### doctor

Run determinism and health checks on the project. Builds the snapshot twice and verifies the fingerprints match.

```bash
vsync doctor
```

### snapshot

Walk include roots and write a deterministic snapshot JSON file.

```bash
vsync snapshot
```

### diff

Compare the previous snapshot against the current state and write a diff JSON file.

```bash
vsync diff
```

### event

Compute a diff and append it to the JSONL event log with a monotonic sequence number.

```bash
vsync event
```

### health

Run source-tree health checks.

```bash
vsync health
```

### watch

Blocking watch loop that emits NDJSON diff events to stdout on each filesystem change.

```bash
vsync watch
vsync watch --project path/to/default.project.json
```

**Flags:**

| Flag | Default | Description |
|------|---------|-------------|
| `--project <path>` | `default.project.json` | Project file path. |

### watch-native

Native filesystem watch using FSEvents (macOS) or inotify (Linux). Replaces polling-based watch.

```bash
vsync watch-native
```

**Flags:**

| Flag | Default | Description |
|------|---------|-------------|
| `--project <path>` | `default.project.json` | Project file path. |

### plugin-install

Install the Vertigo Sync companion plugin to the Roblox Studio plugins directory.

```bash
vsync plugin-install
```

Runs generated-plugin safety validation before writing. Fails if the plugin exceeds the top-level symbol budget or any function exceeds the register risk threshold.

### plugin-set-icon

Set the toolbar icon asset on the installed Studio plugin file.

```bash
vsync plugin-set-icon rbxassetid://1234567890
vsync plugin-set-icon 1234567890
```

**Positional arguments:**

- `<asset_id>` -- Roblox image asset ID (e.g., `rbxassetid://1234567890` or just the numeric ID)

### plugin-smoke-log

Scan a Roblox Studio log file for fatal plugin and runtime failure signatures.

```bash
vsync plugin-smoke-log --log ~/Library/Logs/Roblox/latest.log
vsync plugin-smoke-log --log studio.log --allow-plugin user_VertigoSyncPlugin.lua
vsync plugin-smoke-log --log studio.log --ignore-cloud-plugins --allow-plugin user_VertigoSyncPlugin.lua
```

**Flags:**

| Flag | Default | Description |
|------|---------|-------------|
| `--log <path>` | required | Path to a Roblox Studio log file. |
| `--allow-plugin <name>` | `[]` | External `user_`/`cloud_` plugins permitted during this run. Repeatable. |
| `--ignore-cloud-plugins` | `false` | Ignore Roblox-managed `cloud_` plugin loads. |

Exits with a non-zero code if fatal patterns are found, such as:

- `Out of local registers`
- `attempt to call a nil value`
- `Write apply permanently failed`
- `Snapshot sync failed`

When `--allow-plugin` is specified, any unlisted `user_` or `cloud_` plugin in the log also causes failure.

## Exit codes

| Code | Meaning |
|------|---------|
| `0` | Success. |
| `1` | General error (invalid arguments, runtime failure, validation errors, unformatted files with `--check`). |
| `101` | Panic (unexpected internal error). |
