# vsync Unified Roblox Toolchain Design

**Date:** 2026-03-20
**Status:** Approved
**Goal:** Replace the fragmented Rojo ecosystem (Rojo + Wally + Selene + StyLua + Aftman/Foreman) with a single fast Rust binary. The "uv" of Roblox development.

---

## 1. Vision

One install. One config file. Zero external dependencies. A Roblox developer runs `cargo install vertigo-sync` (or downloads a binary) and has everything: sync, packages, linting, formatting, project scaffolding.

The Rojo ecosystem today requires 6-7 separate tools that must be individually discovered, installed, version-pinned, and configured. vsync collapses them into one coherent CLI with a unified config surface (`vsync.toml`).

## 2. CLI Surface

### Existing commands (unchanged)

```
vsync serve [--turbo]                # live sync to Studio
vsync build -o <path>                # build .rbxl/.rbxlx
vsync validate                       # lint (expanded — see Section 5)
vsync sourcemap -o <path>            # generate sourcemap for luau-lsp
vsync doctor                         # health check (expanded)
vsync watch / watch-native           # filesystem watch
vsync diff / snapshot / event        # snapshot pipeline
vsync discover                       # project identity + server discovery
vsync plugin-install                 # install Studio plugin
vsync plugin-set-icon                # stamp plugin icon
vsync plugin-smoke-log               # scan Studio log for failures
vsync syncback                       # extract scripts from place file
```

### New commands

```
vsync init [--name] [--template]     # scaffold complete project
vsync fmt [--check] [<path>]         # format Luau source
vsync install                        # resolve & install packages
vsync add <package>                  # add dependency
vsync remove <package>               # remove dependency
vsync update [<package>]             # update dependencies
vsync publish                        # publish package to registry
vsync login                          # authenticate with registry
vsync run <script>                   # execute project scripts
vsync migrate                        # convert from Rojo ecosystem configs
```

### Modified commands

- `vsync init` — expanded from minimal to batteries-included (see Section 7)
- `vsync validate` — expanded with full linter rule set (see Section 5)
- `vsync doctor` — checks formatting consistency, package integrity, config health

## 3. Unified Configuration: `vsync.toml`

Single config file at project root. Replaces `wally.toml`, `selene.toml`, `stylua.toml`, `aftman.toml`/`foreman.toml`.

```toml
[package]
name = "studio-player/my-game"
version = "0.1.0"
realm = "shared"
description = "My Roblox game"
license = "MIT"
authors = ["developer"]

[registries]
default = "https://registry.wally.run"
# internal = "https://packages.our-studio.dev"

[dependencies]
roact = "roblox/roact@^17.0.0"
promise = "evaera/promise@^4.0.0"

[server-dependencies]
datastore2 = "kampfkarren/datastore2@^1.5.0"

[dev-dependencies]
testez = "roblox/testez@^0.4.0"

[peer-dependencies]
# roact = "roblox/roact@^17.0.0"

[lint]
unused-variable = "warn"
deprecated-api = "error"
global-shadow = "error"
strict-mode = "warn"
unreachable-code = "warn"

[format]
indent-type = "tabs"
indent-width = 4
line-width = 120
quote-style = "double"
call-parentheses = "always"

[scripts]
test = "vsync build -o test.rbxl && run-in-roblox --place test.rbxl --script tests/init.luau"
deploy = "vsync build -o game.rbxl && rbxcloud publish game.rbxl"
typecheck = "vsync sourcemap -o sourcemap.json && luau-lsp analyze src/"

[workspace]
# members = ["packages/*", "libs/*"]
```

### Relationship to `default.project.json`

`default.project.json` continues to define the Rojo-compatible DataModel tree structure (service hierarchy, `$path` mappings, include roots). `vsync.toml` owns everything else: packages, linting, formatting, scripts, workspace config. No overlap, no migration needed for the project file.

### Config loading semantics

1. **Locate project root:** Walk up from CWD looking for `default.project.json` (existing behavior)
2. **Load project tree:** Parse `default.project.json` → `ProjectTree` with include roots and mappings (existing behavior)
3. **Load vsync config:** Look for `vsync.toml` in the same directory as `default.project.json`
   - If `vsync.toml` exists: parse it, merge with project tree context
   - If absent: operate in backward-compatibility mode (read `wally.toml`, `selene.toml`, `stylua.toml` if present; use sensible defaults otherwise)
4. **Include roots:** Always derived from `default.project.json` `$path` mappings. `vsync.toml` does not redefine include roots. `vsync validate` and `vsync fmt` operate on the same file set that `vsync serve` syncs.
5. **Precedence:** CLI flags > `vsync.toml` > `default.project.json` defaults > built-in defaults

### `vsync validate --fix` boundary

`vsync validate --fix` applies lint auto-fixes only (unused import removal, deprecated API replacement, etc.). It does **not** run the formatter. Formatting is exclusively `vsync fmt`. The two commands are composable: `vsync validate --fix && vsync fmt` applies both. This avoids ambiguity and keeps each command's scope clean.

### Extended dependency syntax

```toml
[dependencies]
# Simple: resolve from default registry
roact = "roblox/roact@^17.0.0"

# Git source (for forks, pre-publish packages)
my-fork = { git = "https://github.com/user/lib", rev = "abc123" }
my-branch = { git = "https://github.com/user/lib", branch = "feature" }

# Path source (for monorepo local packages)
local-lib = { path = "../libs/my-lib" }

# Private registry
internal-lib = { registry = "internal", name = "studio/auth@^1.0.0" }
```

## 4. Package Management

### Registry compatibility

vsync speaks the Wally registry protocol natively:

1. **Index:** Git-based package index (clone/fetch from `registry.wally.run` index repo)
2. **Storage:** HTTP API for downloading package `.zip` archives
3. **Publish:** Authenticated HTTP API for uploading packages

Packages published with vsync are installable by Wally users and vice versa. Full interop.

### Resolution algorithm

- Semantic versioning with `^`, `~`, `>=`, `=` constraints
- Uses the `pubgrub` crate (same algorithm as Cargo and uv) rather than building a resolver from scratch
- Realm-aware resolution: `shared` packages cannot depend on `server`/`client`-only packages
- Peer dependency resolution (v2): consumer provides the dependency, resolver validates version compatibility
- Conflict detection at resolve time (not install time, unlike Wally)
- Deterministic output: same `vsync.toml` always produces same `vsync.lock`

### Lockfile: `vsync.lock`

Wally has no lockfile. vsync adds one:

```toml
# vsync.lock — auto-generated, do not edit
# This file ensures deterministic installs across machines.
lockfile-version = 1

[[package]]
name = "roblox/roact"
version = "17.1.0"
realm = "shared"
checksum = "sha256:abc123..."
source = "registry+https://registry.wally.run"

[[package]]
name = "evaera/promise"
version = "4.0.1"
realm = "shared"
checksum = "sha256:def456..."
source = "registry+https://registry.wally.run"
dependencies = ["roblox/roact@^17.0.0"]
```

The `lockfile-version` field enables forward-compatible schema evolution. vsync refuses to read a lockfile with a version higher than it supports, with a clear upgrade message.

### Error handling

Package operations fail explicitly with actionable messages:

| Failure | Behavior |
|---------|----------|
| Registry unreachable | Retry 3x with exponential backoff, then fail with "registry unreachable" + URL |
| Checksum mismatch | Fail hard, delete cached artifact, suggest `vsync install --force` to re-download |
| Git dependency branch deleted | Fail with "ref not found" + last known good rev from lockfile |
| Conflicting version requirements | Fail at resolve time with dependency chain trace showing both paths to the conflict |
| Corrupt cache | `vsync doctor` detects and repairs; `vsync cache clean` for manual reset |
| Partial download (network drop) | Resume via HTTP range if supported; restart otherwise. No partial state left on disk. |

All package errors use the existing `SyncError` type hierarchy extended with a `PackageError` variant.

### Install target

Packages install to `Packages/` by default (Wally-compatible layout). Configurable:

```toml
[package]
packages-dir = "Packages"   # default
```

### Performance: parallel + HTTP range downloads

Speed is the headline feature. vsync's package manager is built on async Rust (tokio):

- **Parallel resolution:** Dependency tree resolved concurrently, not sequentially
- **Parallel downloads:** All packages downloaded concurrently (bounded concurrency pool)
- **HTTP range chunking:** Large packages are split into N byte-range chunks (`Range: bytes=start-end`), downloaded in parallel, reassembled. The server must support `Accept-Ranges: bytes` (most CDNs and static hosts do). Fallback to single-stream if the server returns `Accept-Ranges: none` or no `Content-Length`.
- **Streaming extraction:** Packages are extracted as they arrive, not after all downloads complete
- **Local cache:** `~/.vsync/cache/` stores downloaded packages by checksum. `vsync install` on a warm cache is near-instant (verify checksums, symlink/copy).
- **Delta fetches:** When updating, only download packages whose versions changed in the lockfile

Target: 10-50x faster than `wally install` for a typical project. Sub-second installs on warm cache.

### Workspace support

For monorepos (framework authors like Quenty):

```toml
# root vsync.toml
[workspace]
members = ["packages/*", "libs/*"]
```

- Single `vsync.lock` at workspace root
- Members reference each other via `{ path = "../sibling" }`
- `vsync install` resolves the entire workspace graph at once
- `vsync publish --workspace` publishes all changed members in dependency order

### Type export generation

When `vsync install` completes, auto-generate Luau type stubs for installed packages:

- Parse each package's exported types from source
- Write `.d.luau` type definition files alongside the package source in `Packages/`
- luau-lsp picks these up automatically via the sourcemap
- Solves the "no autocomplete across package boundaries" pain point

## 5. Built-in Linter (Selene Replacement)

vsync's linter fully replaces Selene. No Selene passthrough, no selene.toml compatibility layer. Clean break.

### Rule categories

#### P0 — Correctness (ship blocker)

| Rule | Description |
|------|-------------|
| `unused-variable` | Local variable declared but never read |
| `unused-parameter` | Function parameter never read (configurable — some styles use `_` prefix) |
| `global-shadow` | Local shadows a Roblox global (e.g., `local game = ...`) |
| `unreachable-code` | Code after unconditional `return`/`break`/`continue`/`error()` |
| `empty-block` | Empty `if`/`else`/`for`/`while` body |
| `duplicate-key` | Duplicate keys in table constructors |
| `mismatched-arg-count` | Function called with wrong number of arguments (where statically determinable) |
| `undefined-variable` | Variable used without declaration or global definition |

#### P0 — Roblox-specific (partially exists, expand)

| Rule | Description |
|------|-------------|
| `deprecated-api` | Usage of removed/deprecated Roblox APIs (EXISTS — expand coverage) |
| `wait-deprecated` | `wait()` instead of `task.wait()` |
| `spawn-deprecated` | `spawn()` instead of `task.spawn()` |
| `delay-deprecated` | `delay()` instead of `task.delay()` |
| `roblox-incorrect-method` | Calling wrong method on Roblox type (e.g., `:Destroy()` vs `:destroy()`) |
| `require-path` | Invalid or suspicious require paths |

#### P0 — Performance (mostly exists)

| Rule | Description |
|------|-------------|
| `strict-mode` | Missing `--!strict` directive (EXISTS) |
| `instance-new-hot-path` | `Instance.new()` in Heartbeat/RenderStepped (EXISTS) |
| `ncg-untyped-param` | Missing type annotations blocking NCG optimization (EXISTS) |
| `ncg-closure-in-loop` | Closures created in loops blocking NCG (EXISTS) |
| `perf-pattern-in-hot-path` | `gmatch`/`gsub` in loops (EXISTS) |
| `perf-dynamic-array` | Table resizing in hot paths (EXISTS) |

#### P1 — Style (follow-up)

| Rule | Description |
|------|-------------|
| `parentheses-condition` | Unnecessary parentheses around `if`/`while` conditions |
| `string-format` | `..` concatenation where `string.format` is cleaner |
| `comparison-order` | Yoda conditions (`nil == x` instead of `x == nil`) |

#### P2 — Complexity (follow-up)

| Rule | Description |
|------|-------------|
| `function-length` | Functions exceeding configurable line count |
| `nesting-depth` | Excessive nesting depth |
| `cyclomatic-complexity` | Functions with too many branches |

### Configuration

All rules configured in `vsync.toml [lint]` section. Three severity levels: `"error"`, `"warn"`, `"off"`.

```toml
[lint]
unused-variable = "warn"
deprecated-api = "error"
global-shadow = "error"
# Per-rule overrides for specific paths:
# [lint.overrides."tests/**"]
# unused-variable = "off"
```

### Output

- Human-readable by default (file:line:col format, colored)
- `--json` for CI integration
- Exit code 1 on errors, 0 on warnings-only or clean
- `vsync validate --fix` auto-fixes where possible (unused imports, deprecated API replacements). Does not run formatting — use `vsync fmt` for that.

### Selene deprecation path

The existing `run_selene()` passthrough in `validate.rs` follows this removal schedule:

1. **v0.x (current):** Selene passthrough continues to work. `vsync validate` prints a deprecation notice: "Selene passthrough is deprecated; built-in rules will replace it in v1.0"
2. **v1.0:** `run_selene()` removed. All P0 rules implemented natively. If users rely on Selene-specific rules not yet in vsync, they can run selene independently — vsync just won't orchestrate it.
3. **Selene rule gap:** The P0 rule set covers Selene's most-used rules. Niche rules like `if_same_then_else`, `manual_table_clone`, `suspicious_reverse_loop` are tracked as P1/P2 and added incrementally post-v1.0. The Section 13 success criterion is updated to say "catches Selene's P0-equivalent rules" rather than "everything Selene catches."

## 6. Formatting (StyLua Integration)

### Approach

StyLua is compiled as a Rust library dependency (the `stylua_lib` crate). `vsync fmt` calls it in-process — no subprocess, no binary management. vsync pins a specific StyLua version; formatting output matches that version exactly. The pinned version is documented in `Cargo.toml` and noted in `vsync fmt --version` output.

### Commands

```
vsync fmt              # format all .luau/.lua files in project includes
vsync fmt src/Server   # format specific path
vsync fmt --check      # exit 1 if any file would change (CI mode)
vsync fmt --diff       # print unified diff of what would change
```

### Configuration

Formatting rules in `vsync.toml [format]` section:

```toml
[format]
indent-type = "tabs"       # "tabs" | "spaces"
indent-width = 4
line-width = 120
quote-style = "double"     # "double" | "single" | "auto"
call-parentheses = "always" # "always" | "no-single-string" | "no-single-table" | "none"
collapse-simple-statement = "never" # "never" | "function-only" | "always"
```

### StyLua compatibility

If `stylua.toml` exists and `vsync.toml` has no `[format]` section, vsync reads `stylua.toml` as a fallback. This eases migration but is not a long-term compatibility guarantee.

### Integration with other commands

- `vsync validate --fix && vsync fmt` is the idiomatic "fix everything" pipeline
- `vsync doctor` warns if files have inconsistent formatting
- `vsync init` scaffolds `[format]` with sensible defaults
- Pre-publish hook: `vsync publish` can optionally enforce `fmt --check` before upload

## 7. Project Scaffolding: `vsync init`

### Default scaffold (`vsync init my-game`)

```
my-game/
├── default.project.json
├── vsync.toml
├── .gitignore
├── src/
│   ├── Server/
│   │   └── init.server.luau
│   ├── Client/
│   │   └── init.client.luau
│   └── Shared/
│       └── init.luau
├── tests/
│   └── init.luau
├── .vscode/
│   └── settings.json
├── .github/
│   └── workflows/
│       └── ci.yml
└── README.md
```

**`.gitignore` contents:**
```
Packages/
*.rbxl
*.rbxlx
.vertigo-sync-state/
sourcemap.json
```

**`.vscode/settings.json`:**
```json
{
  "luau-lsp.sourcemap.enabled": true,
  "luau-lsp.sourcemap.rojoProjectFile": "default.project.json"
}
```

**`.github/workflows/ci.yml`:**
```yaml
name: CI
on: [push, pull_request]
jobs:
  check:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - uses: vertigo-sync/setup-vsync@v1
      - run: vsync install
      - run: vsync validate
      - run: vsync fmt --check
      - run: vsync build -o test.rbxl
```

### Templates (follow-up, not v1)

- `vsync init --template game` — default (above)
- `vsync init --template library` — adds `CHANGELOG.md`, `release.yml` workflow, `[package]` fields for publishing
- `vsync init --template plugin` — Studio plugin scaffold with toolbar setup

### `vsync migrate` (existing Rojo projects)

Converts an existing Rojo-ecosystem project to vsync:

1. Reads `wally.toml` → generates `vsync.toml [package]` + `[dependencies]`
2. Reads `selene.toml` → generates `vsync.toml [lint]` rule mappings
3. Reads `stylua.toml` → generates `vsync.toml [format]`
4. Reads `aftman.toml`/`foreman.toml` → prints message that these are no longer needed
5. Preserves `default.project.json` unchanged
6. Generates `vsync.lock` from existing `Packages/` state
7. Prints summary of what was migrated and what manual steps remain

## 8. `vsync run` — Project Scripts

Defined in `vsync.toml [scripts]`:

```toml
[scripts]
test = "vsync build -o test.rbxl && run-in-roblox --place test.rbxl --script tests/init.luau"
deploy = "vsync build -o game.rbxl && rbxcloud publish game.rbxl"
typecheck = "vsync sourcemap -o sourcemap.json && luau-lsp analyze src/"
lint = "vsync validate"
```

Executed via:
```
vsync run test
vsync run deploy
```

Scripts run in a shell with the project root as CWD. Environment variables available:
- `VSYNC_PROJECT_ROOT` — absolute path to project root
- `VSYNC_PROJECT_NAME` — from `vsync.toml [package].name`

## 9. Architecture Notes

### `vsync install` data flow

```
1. Read vsync.toml (or wally.toml fallback) → dependency requirements
2. Read vsync.lock (if exists) → previously resolved versions
3. Diff requirements vs lockfile → determine what needs resolution
4. Resolve new/changed deps via pubgrub (fetch index as needed)
5. Download missing packages (parallel, with range chunking for large ones)
6. Verify checksums (SHA-256 against resolved manifest)
7. Extract to ~/.vsync/cache/extracted/
8. Copy/link from cache → project Packages/ directory
9. Generate .d.luau type stubs (v2)
10. Write updated vsync.lock
```

Steps 4-6 are fully concurrent. Step 8 is atomic per-package (no partial install state).

### Module boundaries

The new features map to clean Rust modules:

| Module | Responsibility |
|--------|---------------|
| `src/package/` | Registry client, resolver, installer, lockfile, cache |
| `src/package/registry.rs` | Wally registry protocol (index + storage API) |
| `src/package/resolver.rs` | Dependency resolution with realm awareness (pubgrub crate) |
| `src/package/download.rs` | Parallel HTTP downloads with range chunking |
| `src/package/cache.rs` | Local checksum-addressed cache in `~/.vsync/cache/` |
| `src/package/lockfile.rs` | `vsync.lock` read/write |
| `src/package/workspace.rs` | Monorepo workspace graph resolution |
| `src/package/types.rs` | Type stub generation for installed packages |
| `src/lint/` | Expanded linter (absorbs current validate.rs rules) |
| `src/lint/rules/` | One file per rule category (correctness, roblox, perf, style) |
| `src/fmt.rs` | StyLua library integration |
| `src/config.rs` | `vsync.toml` parser (extends current project.rs) |
| `src/migrate.rs` | Wally/selene/stylua/aftman config migration |
| `src/init.rs` | Enhanced project scaffolding |
| `src/scripts.rs` | `vsync run` script executor |

### HTTP range download design

For packages above a configurable size threshold (default: 512KB):

1. **HEAD request** — get `Content-Length` and check `Accept-Ranges: bytes`
2. **Chunk planning** — split into N chunks (default: 4, configurable)
3. **Parallel GET requests** — each with `Range: bytes=start-end` header
4. **Reassembly** — ordered byte concatenation into final archive
5. **Checksum verification** — SHA-256 of reassembled archive against lockfile checksum
6. **Fallback** — if server doesn't support ranges, single-stream download

This is transparent to the user. The only visible effect is speed.

### Cache design

```
~/.vsync/
├── cache/
│   ├── packages/          # checksum-addressed package archives
│   │   ├── sha256-abc123.zip
│   │   └── sha256-def456.zip
│   ├── index/             # cloned registry index repo
│   │   └── wally.run/     # git bare repo
│   └── extracted/         # extracted package contents (keyed by checksum)
│       ├── sha256-abc123/
│       └── sha256-def456/
├── credentials.toml       # registry auth tokens
└── config.toml            # global vsync config (optional overrides)
```

`vsync install` on warm cache: verify checksums → symlink/copy from extracted cache → done. Target: <100ms for typical projects.

## 10. Migration Path for Existing Rojo Users

### Day 1: Drop-in compatible

- `vsync serve` already reads `default.project.json` (done)
- `vsync install` reads `wally.toml` if no `vsync.toml` exists
- `vsync validate` runs expanded rules without config (sensible defaults)
- `vsync fmt` reads `stylua.toml` if no `[format]` in `vsync.toml`

### Day 2: Migrate

- `vsync migrate` consolidates everything into `vsync.toml`
- Delete `wally.toml`, `selene.toml`, `stylua.toml`, `aftman.toml`/`foreman.toml`
- One config file, one tool

### Day 3: Leverage

- `vsync.lock` for deterministic CI
- Workspace support for monorepos
- Type generation for package autocomplete
- `vsync run` for project automation
- HTTP range downloads for blazing installs

## 11. Phasing

### v1.0 — Core toolchain (ship blocker)

- `vsync.toml` config parser and loading semantics
- `vsync init` (batteries-included, default template only)
- `vsync fmt` / `vsync fmt --check` (StyLua library integration)
- `vsync validate` expanded with P0 correctness + Roblox rules
- `vsync install` / `vsync add` / `vsync remove` / `vsync update` (Wally-compatible registry)
- `vsync.lock` lockfile with version field
- `vsync migrate` (wally.toml, selene.toml, stylua.toml)
- Local package cache (`~/.vsync/cache/`)
- Parallel resolution + parallel downloads
- Selene passthrough removal (with deprecation warning in v0.x)

### v1.1 — Ecosystem expansion

- `vsync publish` / `vsync login` (registry auth + upload)
- `vsync run` (project scripts)
- Private registry support
- Git and path dependencies
- HTTP range chunking for large packages
- P1 style lint rules
- `--template library` / `--template plugin` for `vsync init`

### v2.0 — Framework-scale

- Workspace / monorepo support
- Type export generation (`.d.luau` stubs)
- Peer dependency resolution
- P2 complexity lint rules
- Cross-platform script execution hardening

## 12. Non-Goals (for this design)

- **Replacing luau-lsp** — vsync generates sourcemaps and type stubs; the LSP is an editor concern
- **Replacing run-in-roblox** — test execution is available via `vsync run` scripts but vsync doesn't embed a Roblox runtime
- **Custom registry server implementation** — vsync is a client; hosting a private registry is out of scope (use existing Wally-compatible servers)
- **GUI/TUI package browser** — CLI-first; GUI is a follow-up if demand exists
- **Full Selene rule parity at v1.0** — P0 rules ship in v1.0; niche rules added incrementally

## 13. Success Criteria

- A new Roblox developer runs `cargo install vertigo-sync && vsync init my-game && vsync serve --turbo` and has a working, linted, formatted, editor-configured project
- An existing Rojo project runs `vsync migrate && vsync install` and everything works with no other tools installed
- `vsync install` on a 50-dependency project completes in under 2 seconds (warm cache: <100ms)
- `vsync validate` catches Selene's P0-equivalent rules plus Roblox performance rules, in a single pass
- `vsync fmt` produces identical output to StyLua at the pinned version for the same configuration
- Framework authors (Nevermore-scale) can manage 100+ packages in a workspace with one lockfile
