# Migrating from Rojo

Vertigo Sync is a drop-in replacement for Rojo. Your existing `default.project.json` works without modification.

## Command Mapping

| Task | Rojo | Vertigo Sync |
|------|------|--------------|
| Start sync server | `rojo serve default.project.json` | `vertigo-sync serve --turbo` |
| Build place file | `rojo build default.project.json -o place.rbxl` | `vertigo-sync build -o place.rbxl` |
| Default port | `34872` | `7575` |
| Plugin install | Manual `.rbxm` install | `vertigo-sync plugin-install` |

## Step-by-Step Migration

### 1. Install Vertigo Sync

```bash
cargo install vertigo-sync
```

### 2. Stop Rojo

If Rojo is running, stop it. You can run both simultaneously on different ports, but only one Studio plugin should be active at a time.

### 3. Start Vertigo Sync

From the same project root where you ran `rojo serve`:

```bash
vertigo-sync serve --turbo
```

That is it. Vertigo Sync reads the same `default.project.json` and maps the same source paths.

### 4. Install the Studio Plugin

```bash
vertigo-sync plugin-install
```

If you had the Rojo plugin installed, disable it in Studio to avoid conflicts.

### 5. Open Studio

The Vertigo Sync plugin connects automatically. You should see a green "Connected" status within seconds.

## What Works Identically

- **`default.project.json` format** -- same schema, same `$path`/`$className` directives, same `$ignoreUnknownInstances`
- **Source file conventions** -- `init.server.luau`, `init.client.luau`, `init.luau`, `.server.luau`, `.client.luau`
- **Path mapping** -- `src/Server/` to ServerScriptService, `src/Client/` to StarterPlayerScripts, etc.
- **Extended file types** -- `.json` (ModuleScript), `.txt` (StringValue), `.csv` (LocalizationTable)
- **`.meta.json` files** -- instance properties and attributes

## What Is Different

### Transport

| Feature | Rojo | Vertigo Sync |
|---------|------|--------------|
| Primary transport | HTTP polling | WebSocket (real-time) |
| Fallback | None | SSE, then HTTP polling |
| Reconnection | Manual | Automatic with exponential backoff |
| Lag recovery | Manual resync | Automatic (server sends `lagged` message) |

### Validation

Rojo does not validate your Luau source. Vertigo Sync includes a 36-rule linter that runs on every snapshot:

```bash
vertigo-sync validate
```

Rules cover strict mode, NCG optimization, deprecated APIs, hot-path allocations, and cross-boundary requires.

### Observability

| Feature | Rojo | Vertigo Sync |
|---------|------|--------------|
| Prometheus metrics | No | `/metrics` endpoint |
| Workspace attributes | No | 15+ telemetry attributes |
| Apply throughput tracking | No | Adaptive frame budget with EWMA |
| Connection state machine | No | 5-state visual indicator |

### Plugin UX

| Feature | Rojo | Vertigo Sync |
|---------|------|--------------|
| Welcome screen | No | Yes (first-time setup guide) |
| Toast notifications | No | Yes (sync events, errors) |
| Time-travel | No | Yes (scrubber + history list) |
| Feature toggles | No | Yes (binary models, builders) |
| Instance pooling | No | Yes (128 per class, zero GC in hot path) |
| Adaptive frame budget | No | Yes (scales with Studio frame time) |

## What Is New

### Time-Travel

Navigate through your sync history with step/jump controls and a scrubber UI. Rewind to any previous snapshot to inspect the source tree at that point in time.

### MCP Tools

31 agent-native tools for reading, writing, validating, and observing your source tree. Designed for AI-assisted development workflows.

### Deterministic Snapshots

Same source tree always produces the same fingerprint hash. This enables CI-grade reproducibility and drift detection.

## Running Both Simultaneously

You can run Rojo and Vertigo Sync side by side during evaluation:

```bash
# Terminal 1: Rojo on default port
rojo serve default.project.json

# Terminal 2: Vertigo Sync on its default port
vertigo-sync serve --turbo
```

They use different ports (34872 vs 7575) and different Studio plugins, so there is no conflict. Enable only one plugin at a time in Studio.

## Rollback to Rojo

If you need to go back to Rojo:

1. Stop Vertigo Sync
2. Disable the Vertigo Sync plugin in Studio
3. Re-enable the Rojo plugin
4. Start `rojo serve`

No source files are modified. The migration is fully reversible.
