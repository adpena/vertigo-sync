# vertigo-sync

Fast, deterministic source sync and toolchain for Roblox Studio.

![version](https://img.shields.io/badge/version-0.1.0-blue)
![status](https://img.shields.io/badge/status-early%20release-orange)
![license](https://img.shields.io/badge/license-MIT-green)
![platform](https://img.shields.io/badge/platform-macOS%20%7C%20Linux%20%7C%20Windows-lightgrey)

> **Early release.** vsync is under active development. The core sync engine, formatter, and package installer are functional and tested against the real Wally registry. Some features are incomplete — see [Status](#status) below. APIs and config formats may change before 1.0. Bug reports and feedback are welcome.

## Features

- **Source sync** -- Sub-millisecond filesystem-to-Studio synchronization via WebSocket, SSE, or HTTP polling
- **Package management** -- Wally-compatible registry with lockfile, local cache, and `vsync add`/`remove`/`install`
- **Built-in linting** -- Configurable Luau lint rules (unused variables, deprecated APIs, NCG deopt, and more), no external tools needed
- **Built-in formatting** -- Powered by StyLua, integrated into the CLI with `vsync fmt`
- **Project scaffolding** -- `vsync init` creates a complete, ready-to-go project with `default.project.json` and `vsync.toml`
- **Migration** -- `vsync migrate` converts existing Rojo + Wally + Selene + StyLua configs into a single `vsync.toml`
- **Rojo compatible** -- Reads `default.project.json` directly; existing Rojo projects work without changes

## Quick start

```bash
# Install
cargo install --path .

# Create a new project
vsync init --name my-project
cd my-project

# Start the sync server
vsync serve --turbo

# In Roblox Studio, install the companion plugin
vsync plugin-install
```

Open Roblox Studio. On first use, click **Check Connection** once to trust the local `vsync` server. After that, the plugin reconnects automatically.

## CLI reference

| Command | Description |
|---------|-------------|
| `vsync serve` | Start the sync server (HTTP + WebSocket + SSE) |
| `vsync init` | Create a new project with standard directory structure |
| `vsync build -o place.rbxl` | Build a `.rbxl` place file from source |
| `vsync syncback --input place.rbxl` | Extract scripts from a place file back to the filesystem |
| `vsync sourcemap` | Generate a Rojo-compatible `sourcemap.json` for luau-lsp |
| `vsync validate` | Run built-in Luau source validation |
| `vsync fmt` | Format Luau source files (StyLua) |
| `vsync doctor` | Run determinism and health checks |
| `vsync install` | Install packages from `vsync.toml` |
| `vsync add <spec>` | Add a dependency to `vsync.toml` and install it |
| `vsync remove <name>` | Remove a dependency from `vsync.toml` |
| `vsync run <name>` | Run a project script defined in `vsync.toml` |
| `vsync migrate` | Convert Rojo ecosystem configs to `vsync.toml` |
| `vsync discover` | Print active project and server identity |
| `vsync plugin-install` | Install the Studio plugin |
| `vsync plugin-set-icon <id>` | Set the toolbar icon asset on the installed plugin |
| `vsync snapshot` | Print deterministic source tree snapshot |
| `vsync plugin-smoke-log` | Scan a Studio log for fatal plugin failures |

See [docs/cli.md](docs/cli.md) for the full CLI reference with flags and examples.

## Configuration

vsync uses two configuration files:

- **`default.project.json`** -- Rojo-compatible DataModel tree structure defining how source directories map to Studio services
- **`vsync.toml`** -- Package dependencies, lint rules, formatting options, and project scripts

```toml
# vsync.toml
[package]
name = "my-project"
version = "0.1.0"

[dependencies]
roact = "roblox/roact@^17.0.0"

[lint]
unused-variable = "warn"
global-shadow = "error"

[format]
indent-type = "tabs"
line-width = 120

[scripts]
test = "lune run tests"
```

See [docs/configuration.md](docs/configuration.md) for the full reference.

## Migration from Rojo

Existing `default.project.json` files work as-is:

```bash
# Before (Rojo + Wally + Selene + StyLua + Aftman)
rojo serve default.project.json

# After (vsync only)
vsync serve
```

To consolidate `wally.toml`, `selene.toml`, and `stylua.toml` into a single `vsync.toml`:

```bash
vsync migrate
```

See [docs/migration-from-rojo.md](docs/migration-from-rojo.md) for the full migration guide.

## Architecture

```
filesystem --> vsync server --> Studio plugin
                   |
                   +-- GET  /health
                   +-- GET  /snapshot
                   +-- GET  /diff?since=<hash>
                   +-- GET  /source/<path>
                   +-- GET  /sources/content?paths=<csv>
                   +-- GET  /events         (SSE)
                   +-- GET  /ws             (WebSocket)
                   +-- GET  /validate
                   +-- GET  /metrics        (Prometheus)
                   +-- GET  /history?limit=N
                   +-- GET  /config
                   +-- POST /sync/patch
```

1. `vsync` watches the source tree using native FSEvents (macOS) or inotify (Linux) with configurable coalescing.
2. On each change, it rebuilds a content-addressed snapshot using SHA-256 hashes and mtime/size caching.
3. The Studio plugin connects via WebSocket (primary), SSE (secondary), or HTTP polling (tertiary).
4. File changes are delivered as diffs, fetched concurrently, and applied within a frame-budgeted loop (4 ms/frame).
5. The snapshot is deterministic: the same source tree always produces the same fingerprint.

## Contributing

Feedback and contributions are welcome.

### Filing bug reports

Include:

1. What you expected and what actually happened
2. Steps to reproduce -- exact commands, project structure, Studio version
3. Environment -- OS, `vsync --version`, Studio version
4. Logs -- Studio Output (filter by `[VertigoSync]`) and terminal output from `vsync serve`
5. Output of `vsync doctor`

### Pull requests

1. Open an issue first to discuss the approach
2. Keep changes focused -- one feature or fix per PR
3. Include tests for new functionality
4. Run `cargo test` and `cargo clippy` before submitting

## Status

vsync 0.1 is an early release. Here is what works today and what is planned.

### Stable (tested, used in real projects)

- Live sync to Studio (WebSocket + SSE + HTTP polling)
- `vsync build` / `vsync syncback` / `vsync sourcemap`
- Two-way sync (Studio writes back to disk via `POST /sync/patch`)
- Rojo-compatible `default.project.json` (all standard features)
- Studio plugin with reconnection, frame budgeting, time travel, settings UI

### Functional (new in 0.1, tested against real Wally registry)

- `vsync install` / `vsync add` / `vsync remove` with transitive dependency resolution
- `vsync fmt` (StyLua-powered, parallel via rayon)
- `vsync validate` with 20 built-in lint rules (parallel via rayon)
- `vsync init` / `vsync migrate` / `vsync run`
- `vsync.toml` unified config, `vsync.lock` lockfile

### Not yet implemented

- `vsync publish` / `vsync login` (registry auth and package upload)
- `vsync update` (re-resolve dependencies to latest matching versions)
- Git and private registry dependencies
- Workspace / monorepo support
- `.styluaignore` equivalent
- Shell completion generation (`vsync completions <shell>`)
- Type checking integration (`luau-analyze`)
- Sourcemap auto-regeneration during `vsync serve`

### Out of scope (use alongside vsync)

- **darklua** -- Luau preprocessing/bundling. Use `vsync run` to orchestrate.
- **run-in-roblox** -- Headless test execution. Use `vsync run test` to orchestrate.
- **Tarmac** -- Asset pipeline (image uploads). Use `vsync run` to orchestrate.
- **luau-lsp** -- Language server. vsync generates sourcemaps for it; install separately.

## Vision

vsync is designed around the assumption that Roblox projects will continue to grow in scale and complexity. The architecture targets several capabilities that address pain points in large codebases:

- **Streaming edit preview** — Changes are delivered to Studio as incremental diffs over WebSocket, not full-file refreshes. The plugin applies updates within a frame-budgeted loop (4 ms/frame) with instance pooling, so even large batch changes do not cause Studio to hang or drop frames.
- **Time travel** — The sync server maintains a rolling history of snapshot states. The Studio plugin provides a scrubber UI for stepping backward and forward through recent changes, making it possible to visualize how a change propagated and revert to a prior state without leaving Studio.
- **Deterministic snapshots** — The same source tree always produces the same SHA-256 fingerprint. This is the foundation for reliable diffing, caching, and CI reproducibility.

These features are built on the same content-addressed snapshot system that powers `vsync serve`, `vsync build`, and `vsync doctor`.

## Acknowledgments

vsync builds on the work and ideas of the Roblox open-source community. This project would not exist without the foundations laid by others.

- **[Rojo](https://github.com/rojo-rbx/rojo)** pioneered the external-editor workflow for Roblox and defined the `default.project.json` conventions that vsync is fully compatible with. The Rojo project and its contributors fundamentally changed how Roblox developers work.
- **[rbx-dom](https://github.com/rojo-rbx/rbx-dom)** provides the binary and XML serialization libraries that vsync uses for `.rbxl`/`.rbxlx` and `.rbxm` file handling.
- **[Wally](https://github.com/UpliftGames/wally)** established the package registry and dependency model for the Roblox ecosystem. vsync's package manager is designed for full registry compatibility with Wally.
- **[Selene](https://github.com/Kampfkarren/selene)** and **[StyLua](https://github.com/JohnnyMorganz/StyLua)** set the standard for Luau linting and formatting. vsync embeds StyLua directly and draws from Selene's rule design.
- **[Luau LSP](https://github.com/JohnnyMorganz/luau-lsp)** provides the language server experience that vsync supports through sourcemap generation.
- **[Aftman](https://github.com/LPGhatguy/aftman)** and **[Foreman](https://github.com/Roblox/foreman)** demonstrated the value of toolchain management for Roblox projects.

The Roblox developer community — through open-source frameworks, shared packages, and honest feedback — continues to push these tools forward. vsync aims to contribute to that ecosystem, not replace it.

## License

MIT
