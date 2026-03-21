# Migrating from Rojo

Vertigo Sync reads the same `default.project.json` format that Rojo established. Your existing project file works without modification. The migration is fully reversible -- you can switch back to Rojo at any time without changing your source files.

## Command Mapping

| Task | Rojo | Vertigo Sync |
|------|------|--------------|
| Start sync server | `rojo serve default.project.json` | `vsync serve` |
| Build place file | `rojo build default.project.json -o place.rbxl` | `vsync build -o place.rbxl` |
| Default port | `34872` | `7575` |
| Plugin install | Manual `.rbxm` install | `vsync plugin-install` |

## Step-by-Step Migration

### 1. Install Vertigo Sync

```bash
cargo install --path .
```

### 2. Stop Rojo

If Rojo is running, stop it. You can run both simultaneously on different ports, but only one Studio plugin should be active at a time.

### 3. Start Vertigo Sync

From the same project root where you ran `rojo serve`:

```bash
vsync serve
```

That is it. Vertigo Sync reads the same `default.project.json` and maps the same source paths.

### 4. Install the Studio Plugin

```bash
vsync plugin-install
```

If you had the Rojo plugin installed, disable it in Studio to avoid conflicts.

### 5. Open Studio

The Vertigo Sync plugin connects automatically. You should see a green "Connected" status within seconds.

## Automatic Migration

Run `vsync migrate` in a project directory that has Rojo ecosystem config files:

```bash
vsync migrate
```

This reads the following files (if present) and merges them into a single `vsync.toml`:

| Source file | Migrated to |
|-------------|-------------|
| `wally.toml` | `[package]`, `[dependencies]`, `[server-dependencies]`, `[dev-dependencies]` |
| `selene.toml` | `[lint]` (carries over the `std` hint and default rule severities) |
| `stylua.toml` | `[format]` (indent type, width, line width, quote style, call parentheses) |

If `aftman.toml` or `foreman.toml` is detected, vsync reports their presence. Those files can be removed since vsync bundles all the tools they managed.

`vsync migrate` does not overwrite an existing `vsync.toml`. It is safe to run repeatedly.

## Tool Replacement Reference

| Before | After |
|--------|-------|
| `rojo serve` | `vsync serve` |
| `rojo build -o game.rbxl` | `vsync build -o game.rbxl` |
| `rojo sourcemap` | `vsync sourcemap` |
| `selene src/` | `vsync validate` |
| `stylua src/` | `vsync fmt` |
| `stylua --check src/` | `vsync fmt --check` |
| `wally install` | `vsync install` |

## Update CI Scripts

A typical CI pipeline migration:

```yaml
# Before
- run: aftman install
- run: wally install
- run: selene src/
- run: stylua --check src/
- run: rojo build -o game.rbxl

# After
- run: cargo install vertigo-sync
- run: vsync install
- run: vsync validate
- run: vsync fmt --check
- run: vsync build -o game.rbxl
```

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

Rojo intentionally defers linting to dedicated tools like Selene, which is a reasonable design choice for a sync-focused tool. Vertigo Sync takes a different approach and bundles a built-in linter for convenience:

```bash
vsync validate
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
| Feature toggles | No | Yes (binary models, builders; builders stay off until enabled) |
| Instance pooling | No | Yes (128 per class, zero GC in hot path) |
| Adaptive frame budget | No | Yes (scales with Studio frame time) |
| Project identity binding | No | Yes (plugin remembers last good project and fails closed on ambiguous discovery) |

## What Is New

### Time-Travel

Navigate through your sync history with step/jump controls and a scrubber UI. Rewind to any previous snapshot to inspect the source tree at that point in time.

### MCP Tools

Agent-native tools for reading, writing, validating, and observing your source tree. Designed for AI-assisted development workflows.

### Deterministic Snapshots

Same source tree always produces the same fingerprint hash. This enables CI-grade reproducibility and drift detection.

## Running Both Simultaneously

You can run Rojo and Vertigo Sync side by side during evaluation:

```bash
# Terminal 1: Rojo on default port
rojo serve default.project.json

# Terminal 2: Vertigo Sync on its default port
vsync serve
```

They use different ports (34872 vs 7575) and different Studio plugins, so there is no conflict. Enable only one plugin at a time in Studio.

## Rollback to Rojo

If you need to go back to Rojo:

1. Stop Vertigo Sync
2. Disable the Vertigo Sync plugin in Studio
3. Re-enable the Rojo plugin
4. Start `rojo serve`

No source files are modified. The migration is fully reversible.

## Clean Up Old Config Files

Once migration is confirmed working, remove the files that vsync replaces:

```bash
rm -f wally.toml wally.lock selene.toml stylua.toml aftman.toml foreman.toml
```

Keep `default.project.json` -- vsync uses it directly.

## Keeping Selene Alongside vsync

vsync's built-in linter covers a different set of rules than selene. If a project uses selene rules that vsync does not implement, both tools can be used together. vsync runs selene automatically during `vsync validate` when selene is found on `PATH`.
