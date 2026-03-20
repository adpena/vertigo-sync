# Migration from Rojo

vsync is a drop-in replacement for the Rojo ecosystem. It reads `default.project.json` directly, so existing projects work without changes.

## What replaces what

| Before | After | Notes |
|--------|-------|-------|
| Rojo (`rojo serve`) | `vsync serve` | Same project file format. |
| Wally (`wally.toml`) | `vsync.toml [dependencies]` | Same registry, compatible dependency format. |
| Selene (`selene.toml`) | `vsync.toml [lint]` + built-in rules | Built-in linter; selene is still supported alongside. |
| StyLua (`stylua.toml`) | `vsync.toml [format]` + `vsync fmt` | StyLua library embedded in the binary. |
| Aftman / Foreman | Not needed | vsync is a single binary with all tools built in. |

## Automatic migration

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

## Manual steps after migration

### 1. Verify the generated vsync.toml

Open `vsync.toml` and review the migrated settings:

```bash
cat vsync.toml
```

Confirm that dependency versions and lint/format settings match expectations.

### 2. Switch from Rojo to vsync

Replace `rojo serve` with `vsync serve`:

```bash
# Before
rojo serve default.project.json

# After
vsync serve
```

The `default.project.json` file is used as-is. vsync auto-detects `$path` entries from the project file, so `--include` flags are not usually needed.

### 3. Install the Studio plugin

```bash
vsync plugin-install
```

Uninstall the Rojo plugin from Studio to avoid conflicts. Both plugins can coexist on different ports, but running one at a time is simpler.

### 4. Install packages

If the project uses Wally packages, install them with vsync:

```bash
vsync install
```

This reads `[dependencies]` from `vsync.toml` (or falls back to `wally.toml` if `vsync.toml` does not exist) and installs packages into the `Packages/` directory.

### 5. Replace standalone tool invocations

| Before | After |
|--------|-------|
| `selene src/` | `vsync validate` |
| `stylua src/` | `vsync fmt` |
| `stylua --check src/` | `vsync fmt --check` |
| `rojo build -o game.rbxl` | `vsync build -o game.rbxl` |
| `rojo sourcemap` | `vsync sourcemap` |
| `wally install` | `vsync install` |

### 6. Update CI scripts

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

### 7. Clean up old config files

Once migration is confirmed working, remove the files that vsync replaces:

```bash
rm -f wally.toml wally.lock selene.toml stylua.toml aftman.toml foreman.toml
```

Keep `default.project.json` -- vsync uses it directly.

## Behavioral differences from Rojo

If switching from Rojo, be aware of these differences:

- **Port:** Rojo defaults to `34872`; vsync defaults to `7575`.
- **Apply timing:** Rojo applies all pending changes in a single frame. vsync distributes them across frames within a 4 ms budget.
- **Rename handling:** Rojo deletes the old instance and creates a new one. vsync detects renames via content hash matching and moves the instance in place, preserving references.
- **Snapshot model:** Rojo tracks instance-level changes in a DOM tree. vsync tracks file-level changes via content-addressed hashes.
- **Validation:** vsync runs its built-in linter on `validate` and `doctor` calls. Rojo does not lint source files.

## Keeping selene alongside vsync

vsync's built-in linter covers a different set of rules than selene. If a project uses selene rules that vsync does not implement, both tools can be used together. vsync runs selene automatically during `vsync validate` when selene is found on `PATH`.
