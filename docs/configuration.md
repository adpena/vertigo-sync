# Configuration

vsync uses two configuration files: `default.project.json` for DataModel tree structure and `vsync.toml` for toolchain settings.

## vsync.toml

The unified configuration file for package metadata, dependencies, linting, formatting, and scripts.

### \[package\]

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `name` | string | `""` | Package name. Use `scope/name` format for registry publishing. |
| `version` | string | `""` | Semver version string (e.g., `"0.1.0"`). |
| `realm` | string | `""` | Package realm: `"shared"`, `"server"`, or `"dev"`. |
| `description` | string | `""` | Human-readable package description. |
| `license` | string | `""` | SPDX license identifier (e.g., `"MIT"`). |
| `authors` | string[] | `[]` | List of author names or email addresses. |
| `packages-dir` | string | `"Packages"` | Directory where installed packages are written. |

```toml
[package]
name = "acme/my-game"
version = "1.0.0"
realm = "shared"
description = "A Roblox experience"
license = "MIT"
authors = ["Alice <alice@example.com>"]
packages-dir = "Packages"
```

### \[registries\]

Named registries for package resolution. Keys are registry names, values are URLs.

```toml
[registries]
wally = "https://api.wally.run"
custom = "https://packages.example.com"
```

### \[dependencies\]

Shared dependencies available to all realms. Each key is an alias name, each value is a dependency specification.

**String format (registry dependency):**

```toml
[dependencies]
roact = "roblox/roact@^17.0.0"
promise = "evaera/promise@^4.0.0"
```

**Git dependency:**

```toml
[dependencies]
my-lib = { git = "https://github.com/user/repo", rev = "abc123" }
my-lib = { git = "https://github.com/user/repo", branch = "main" }
my-lib = { git = "https://github.com/user/repo", tag = "v1.0.0" }
```

**Path dependency:**

```toml
[dependencies]
my-lib = { path = "../my-lib" }
```

**Named registry dependency:**

```toml
[dependencies]
my-lib = { registry = "custom", name = "scope/my-lib" }
```

### \[server-dependencies\]

Dependencies available only in server-side scripts. Same format as `[dependencies]`.

```toml
[server-dependencies]
data-store-service = "acme/data-store@^1.0.0"
```

### \[dev-dependencies\]

Dependencies used during development and testing only. Same format as `[dependencies]`.

```toml
[dev-dependencies]
test-ez = "roblox/testez@^0.4.0"
```

### \[peer-dependencies\]

Dependencies expected to be provided by the consuming project. Same format as `[dependencies]`.

### \[lint\]

Controls the severity of built-in lint rules. Each key is a rule name, each value is one of:

- `"off"` -- disable the rule
- `"warn"` -- report as a warning (default for all rules)
- `"error"` -- report as an error

#### Configurable lint rules

| Rule | Default | Description |
|------|---------|-------------|
| `unused-variable` | `warn` | `local x = ...` where `x` is never referenced again. Variables prefixed with `_` are ignored. |
| `global-shadow` | `warn` | `local game = ...` shadowing a Roblox global (`game`, `workspace`, `Instance`, `Vector3`, `task`, etc.). |
| `wait-deprecated` | `warn` | Bare `wait()` call. Use `task.wait()` instead. |
| `spawn-deprecated` | `warn` | Bare `spawn()` call. Use `task.spawn()` instead. |
| `delay-deprecated` | `warn` | Bare `delay()` call. Use `task.delay()` instead. |
| `empty-block` | `warn` | Empty `then ... end` or `do ... end` block body. |
| `unreachable-code` | `warn` | Code after unconditional `return`, `break`, `continue`, or `error()`. |
| `parentheses-condition` | `warn` | Unnecessary parentheses around `if`/`while` conditions (e.g., `if (x) then`). |
| `comparison-order` | `warn` | Yoda conditions (e.g., `nil == x` instead of `x == nil`). |
| `function-length` | `warn` | Functions exceeding 100 lines. |
| `nesting-depth` | `warn` | Nesting deeper than 5 levels inside a function. |
| `cyclomatic-complexity` | `warn` | Functions with more than 10 branches (`if`, `elseif`, `and`, `or`, `while`, `for`, `repeat`). |

```toml
[lint]
unused-variable = "warn"
global-shadow = "error"
wait-deprecated = "error"
spawn-deprecated = "warn"
delay-deprecated = "warn"
empty-block = "off"
unreachable-code = "warn"
parentheses-condition = "warn"
comparison-order = "warn"
function-length = "warn"
nesting-depth = "warn"
cyclomatic-complexity = "warn"
```

#### Validation rules (always active)

These rules run during `vsync validate` and `vsync doctor` and are not configurable via `[lint]`:

| Rule | Severity | Description |
|------|----------|-------------|
| `strict-mode` | error | Missing `--!strict` directive at the top of a `.luau` file. |
| `cross-boundary-require` | error | Invalid `require()` path crossing service boundaries. |
| `deprecated-api` | warning | Usage of deprecated Roblox APIs. |
| `large-file` | warning | File exceeds 500 lines. |
| `tab-indent` | warning | File uses tab indentation (detected heuristically). |
| `instance-new-hot-path` | warning | `Instance.new()` inside a Heartbeat/RenderStepped callback. |
| `ncg-untyped-param` | warning | Function parameter without a type annotation in a `@native` function. |
| `ncg-closure-in-loop` | warning | Closure created inside a loop in a `@native` function. |
| `ncg-pattern-in-hot-path` | warning | `gmatch`/`gsub` in a hot-path function. |
| `perf-dynamic-array` | warning | Dynamic array growth pattern in a hot path. |
| `perf-unfrozen-constant` | warning | Module-level table that could be frozen with `table.freeze`. |
| `perf-missing-native` | warning | Hot-path function missing `@native` annotation. |
| `perf-pcall-in-native` | warning | `pcall`/`xpcall` inside a `@native` function (causes NCG deopt). |

### \[format\]

Controls the built-in StyLua-powered formatter. All fields are optional; unset fields use StyLua defaults.

| Field | Type | Default | Valid values |
|-------|------|---------|-------------|
| `indent-type` | string | StyLua default | `"tabs"`, `"spaces"` |
| `indent-width` | integer | StyLua default | Any positive integer (typically 2 or 4) |
| `line-width` | integer | StyLua default | Any positive integer (typically 80 or 120) |
| `quote-style` | string | StyLua default | `"single"`, `"double"`, `"auto"`, `"autoprefersingle"`, `"autopreferdouble"` |
| `call-parentheses` | string | StyLua default | `"always"`, `"nosinglestring"`, `"nosingletable"`, `"none"`, `"input"` |
| `collapse-simple-statement` | string | StyLua default | `"never"`, `"functiononly"`, `"conditionalonly"`, `"always"` |

```toml
[format]
indent-type = "tabs"
indent-width = 4
line-width = 120
quote-style = "double"
call-parentheses = "always"
collapse-simple-statement = "never"
```

### \[scripts\]

Named shell commands that can be run with `vsync run <name>`. Commands execute in a shell (`sh -c` on Unix, `cmd /C` on Windows) with the working directory set to the project root.

Two environment variables are injected:

- `VSYNC_PROJECT_ROOT` -- absolute path to the project root
- `VSYNC_PROJECT_NAME` -- the project name from `default.project.json`

```toml
[scripts]
test = "lune run tests"
lint = "vsync validate && vsync fmt --check"
deploy = "vsync build -o game.rbxl && rbxcloud publish game.rbxl"
```

### \[workspace\]

Multi-project workspace configuration.

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `members` | string[] | `[]` | Glob patterns matching workspace member directories. |

```toml
[workspace]
members = ["packages/*", "games/*"]
```

## default.project.json

Rojo-compatible project file that defines how source directories map to the Roblox DataModel. vsync reads this format directly.

### Top-level fields

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `name` | string | required | Project name shown in the Studio plugin. |
| `projectId` | string | -- | Stable project identifier for plugin binding. |
| `globIgnorePaths` | string[] | `[]` | Glob patterns for paths to exclude from sync. |
| `emitLegacyScripts` | boolean | `true` | Emit legacy script format for compatibility. |
| `servePort` | integer | `7575` | Port for the sync server. |
| `serveAddress` | string | `"127.0.0.1"` | Bind address for the sync server. |
| `tree` | object | required | DataModel tree definition. |

### Tree directives

| Directive | Description |
|-----------|-------------|
| `$className` | The Roblox class name for this tree node. |
| `$path` | Relative filesystem path to a source directory or file. |
| `$ignoreUnknownInstances` | When `true`, preserves instances not managed by sync. |
| `$properties` | Property overrides applied to the generated instance. |
| `$attributes` | Attribute overrides applied to the generated instance. |

### Example

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

### Vertigo Sync extensions

The `vertigoSync` key in `default.project.json` configures features specific to vsync.

#### vertigoSync.builders

Declares edit-mode preview builder entrypoints.

| Field | Type | Description |
|-------|------|-------------|
| `roots` | string[] | Filesystem prefixes containing builder `ModuleScript` entrypoints. |
| `dependencyRoots` | string[] | Filesystem prefixes that trigger builder re-execution on change. |

```json
{
  "vertigoSync": {
    "builders": {
      "roots": ["src/ServerScriptService/StudioPreview"],
      "dependencyRoots": ["src/ReplicatedStorage/Shared"]
    }
  }
}
```

#### vertigoSync.editPreview

Runs a project-specific preview builder from the Studio plugin.

| Field | Type | Description |
|-------|------|-------------|
| `enabled` | boolean | Enables the integrated edit-preview watcher. |
| `builderModulePath` | string | DataModel path to the preview builder `ModuleScript`. |
| `builderMethod` | string | Entry method name (e.g., `"Build"`). |
| `watchRoots` | string[] | DataModel roots watched for edits. |
| `debounceSeconds` | number | Debounce window before triggering a rebuild. |
| `rootRefreshSeconds` | number | How often the plugin re-resolves watch roots. |
| `mode` | string | Execution mode: `"edit_only"` or `"studio_server"`. |

### File type mapping

| File pattern | Roblox class |
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

### Meta files

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

## Studio plugin settings

The plugin persists these settings across Studio sessions:

| Setting key | Default | Description |
|-------------|---------|-------------|
| `VertigoSyncBinaryModels` | `false` | Enable binary model (`.rbxm`/`.rbxmx`) instance creation. |
| `VertigoSyncBuildersEnabled` | `false` | Enable builder execution in edit mode. |
| `VertigoSyncTimeTravelUI` | `true` | Show time-travel panel in the DockWidget. |
| `VertigoSyncHistoryBuffer` | `256` | Maximum history entries (16--1024). |

Settings can also be controlled via Workspace attributes:

```lua
workspace:SetAttribute("VertigoSyncBinaryModels", true)
workspace:SetAttribute("VertigoSyncBuildersEnabled", true)
workspace:SetAttribute("VertigoSyncServerUrl", "http://127.0.0.1:7575")
workspace:SetAttribute("VertigoSyncProjectId", "your-project-id")
```
