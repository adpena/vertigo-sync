# Unified Toolchain Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Transform vsync from a sync tool into a unified Roblox toolchain that replaces Rojo + Wally + Selene + StyLua + Aftman with a single binary.

**Architecture:** Five independent subsystems built on a shared `vsync.toml` config foundation. Each subsystem is a self-contained Rust module with its own tests, wired into the existing clap CLI. The config parser (`src/config.rs`) is the shared dependency; all other subsystems (formatting, linting, package management, scaffolding) depend on it but not on each other.

**Tech Stack:** Rust 1.85+, clap 4 (CLI), toml 0.8 (config parsing), stylua_lib (formatting), pubgrub (dependency resolution), reqwest (HTTP client), tokio (async runtime — already in use)

**Spec:** `docs/superpowers/specs/2026-03-20-unified-toolchain-design.md`

---

## Critical Implementation Notes

These corrections apply globally across all tasks. Read before starting any task.

### 1. `toml` crate aliasing

`Cargo.toml` declares `toml_crate = { package = "toml", version = "0.8" }`. All code snippets in this plan use `toml::` which **will not compile**. Before starting Task 1, change `Cargo.toml` to:
```toml
toml = "0.8"
```
Remove the `toml_crate` alias. Then find-and-replace any existing `toml_crate::` usage in the codebase to `toml::`. Commit this rename as a prerequisite.

### 2. `resolve_project_context` signature

The existing function signature is:
```rust
fn resolve_project_context(root: &Path, project: &Path, cli_includes: &[String]) -> Result<ProjectContext>
```

All code snippets that call `resolve_project_context(&cli)` are wrong. The correct pattern for commands that have a `--project` arg is:
```rust
let ctx = resolve_project_context(&root, &project, &cli.include)?;
```

For commands that don't take `--project` (like `Validate`, `Fmt`), either:
- Add a `--project` arg to those commands (recommended — matches `Build`, `Serve`, etc.), or
- Use `Path::new("default.project.json")` as the default.

When adding `vsync_config` to `ProjectContext`, also load it inside `resolve_project_context`:
```rust
let vsync_config = vertigo_sync::config::load_config_with_fallback(&project_root)?;
Ok(ProjectContext { project_path, project_root, includes, tree, vsync_config })
```

### 3. Async context — no nested tokio runtime

`main()` is `#[tokio::main] async fn main()`. Do NOT create `tokio::runtime::Runtime::new()` inside command handlers — this panics. Instead, make async command handlers use `.await` directly:
```rust
Command::Install { project } => {
    let ctx = resolve_project_context(&root, &project, &cli.include)?;
    let config = ctx.vsync_config.unwrap_or_default();
    let report = vertigo_sync::package::installer::install(&ctx.project_root, &config).await?;
    // ...
}
```

### 4. Missing P0 lint rules

Task 7 implements 7 new rules. The spec requires additional P0 rules. The existing `validate.rs` already implements `strict-mode`, `deprecated-api`, `instance-new-hot-path`, `ncg-*`, and `perf-*` rules. These must be wired into the configurable `[lint]` system. Add a **Task 8b** after Task 8: read existing `validate.rs` rules, expose them as configurable entries in `vsync.toml [lint]`, and delegate to the existing validation functions. Do not rewrite them — wrap them.

Additionally, add these rules to Task 7 (or a follow-up sub-task):
- `duplicate-key` — detect duplicate keys in table constructors
- `undefined-variable` — variable used without `local` or global declaration
- `roblox-incorrect-method` — wrong method casing on Roblox types

### 5. Parallel downloads

Task 12's installer is a sequential skeleton. Add a **Task 12b** that converts the download loop to `tokio::JoinSet` or `futures::stream::FuturesUnordered` with bounded concurrency (default: 8). This is a v1.0 requirement.

### 6. Selene deprecation

Add to Task 8: In the `command_validate` handler, after calling `run_selene()`, print a deprecation notice:
```rust
output::warn("Selene passthrough is deprecated and will be removed in v1.0. Built-in rules will replace it.");
```

### 7. `stylua_lib` API verification

The StyLua API in Task 5 is approximated. Before implementing, run `cargo doc -p stylua-lib --open` to see the actual API surface. Key things to verify: `Config` construction, `format_code` signature, enum variant names. Adjust the implementation to match.

### 8. Consolidation notes

- `write_if_missing` and `write_if_missing_with_dirs` in Task 13 are identical — use one function.
- `collect_luau_files` (Task 6) and `lint_directory` (Task 8) duplicate file-walking — extract a shared `walk_luau_files` utility in `src/lib.rs` or a small `src/fs_util.rs`.

---

## Phase 1: Config Foundation

### Task 1: `VsyncConfig` type definitions

**Files:**
- Create: `src/config.rs`
- Test: `tests/config_test.rs`

- [ ] **Step 1: Write test for parsing a minimal vsync.toml**

```rust
// tests/config_test.rs
use vertigo_sync::config::VsyncConfig;

#[test]
fn parse_minimal_config() {
    let toml = r#"
[package]
name = "studio-player/my-game"
version = "0.1.0"
realm = "shared"
"#;
    let config: VsyncConfig = toml::from_str(toml).unwrap();
    assert_eq!(config.package.name, "studio-player/my-game");
    assert_eq!(config.package.version, "0.1.0");
    assert_eq!(config.package.realm, "shared");
}

#[test]
fn parse_full_config() {
    let toml = r#"
[package]
name = "studio-player/my-game"
version = "0.1.0"
realm = "shared"

[dependencies]
roact = "roblox/roact@^17.0.0"

[server-dependencies]
datastore2 = "kampfkarren/datastore2@^1.5.0"

[dev-dependencies]
testez = "roblox/testez@^0.4.0"

[lint]
unused-variable = "warn"
deprecated-api = "error"
strict-mode = "off"

[format]
indent-type = "tabs"
indent-width = 4
line-width = 120
quote-style = "double"

[scripts]
test = "vsync build -o test.rbxl"
"#;
    let config: VsyncConfig = toml::from_str(toml).unwrap();
    assert_eq!(config.dependencies.len(), 1);
    assert_eq!(config.server_dependencies.len(), 1);
    assert_eq!(config.dev_dependencies.len(), 1);
    assert_eq!(
        config.lint.get("unused-variable").map(|s| s.as_str()),
        Some("warn")
    );
    assert_eq!(config.format.indent_type, Some("tabs".to_string()));
    assert_eq!(config.format.line_width, Some(120));
    assert_eq!(config.scripts.get("test").map(|s| s.as_str()), Some("vsync build -o test.rbxl"));
}

#[test]
fn empty_config_uses_defaults() {
    let toml = "";
    let config: VsyncConfig = toml::from_str(toml).unwrap();
    assert!(config.package.name.is_empty() || config.package == PackageConfig::default());
    assert!(config.dependencies.is_empty());
    assert!(config.lint.is_empty());
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test --test config_test`
Expected: FAIL — `config` module does not exist

- [ ] **Step 3: Implement VsyncConfig types**

```rust
// src/config.rs
//! vsync.toml configuration parser.
//!
//! Defines the unified config surface that replaces wally.toml, selene.toml,
//! stylua.toml, and aftman.toml with a single file.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::Path;
use anyhow::{Context, Result};

/// Top-level vsync.toml configuration.
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
pub struct VsyncConfig {
    #[serde(default)]
    pub package: PackageConfig,

    #[serde(default)]
    pub registries: BTreeMap<String, String>,

    #[serde(default)]
    pub dependencies: BTreeMap<String, DependencySpec>,

    #[serde(default, rename = "server-dependencies")]
    pub server_dependencies: BTreeMap<String, DependencySpec>,

    #[serde(default, rename = "dev-dependencies")]
    pub dev_dependencies: BTreeMap<String, DependencySpec>,

    #[serde(default, rename = "peer-dependencies")]
    pub peer_dependencies: BTreeMap<String, DependencySpec>,

    #[serde(default)]
    pub lint: BTreeMap<String, String>,

    #[serde(default)]
    pub format: FormatConfig,

    #[serde(default)]
    pub scripts: BTreeMap<String, String>,

    #[serde(default)]
    pub workspace: WorkspaceConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
pub struct PackageConfig {
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub version: String,
    #[serde(default)]
    pub realm: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub license: String,
    #[serde(default)]
    pub authors: Vec<String>,
    #[serde(default, rename = "packages-dir")]
    pub packages_dir: Option<String>,
}

/// A dependency can be a simple version string or a table with git/path/registry.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(untagged)]
pub enum DependencySpec {
    /// Simple version: `"roblox/roact@^17.0.0"`
    Simple(String),
    /// Git source: `{ git = "https://...", rev = "abc123" }`
    Git {
        git: String,
        #[serde(default)]
        rev: Option<String>,
        #[serde(default)]
        branch: Option<String>,
        #[serde(default)]
        tag: Option<String>,
    },
    /// Path source: `{ path = "../libs/my-lib" }`
    Path { path: String },
    /// Registry source: `{ registry = "internal", name = "studio/auth@^1.0.0" }`
    Registry { registry: String, name: String },
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
pub struct FormatConfig {
    #[serde(default, rename = "indent-type")]
    pub indent_type: Option<String>,
    #[serde(default, rename = "indent-width")]
    pub indent_width: Option<u32>,
    #[serde(default, rename = "line-width")]
    pub line_width: Option<u32>,
    #[serde(default, rename = "quote-style")]
    pub quote_style: Option<String>,
    #[serde(default, rename = "call-parentheses")]
    pub call_parentheses: Option<String>,
    #[serde(default, rename = "collapse-simple-statement")]
    pub collapse_simple_statement: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
pub struct WorkspaceConfig {
    #[serde(default)]
    pub members: Vec<String>,
}

/// Load and parse a vsync.toml file. Returns `Ok(None)` if the file doesn't exist.
pub fn load_config(project_root: &Path) -> Result<Option<VsyncConfig>> {
    let config_path = project_root.join("vsync.toml");
    if !config_path.exists() {
        return Ok(None);
    }
    let content = std::fs::read_to_string(&config_path)
        .with_context(|| format!("failed to read {}", config_path.display()))?;
    let config: VsyncConfig = toml::from_str(&content)
        .with_context(|| format!("failed to parse {}", config_path.display()))?;
    Ok(Some(config))
}
```

- [ ] **Step 4: Export the module from lib.rs**

Add to `src/lib.rs`:
```rust
pub mod config;
```

- [ ] **Step 5: Run tests to verify they pass**

Run: `cargo test --test config_test`
Expected: PASS (all 3 tests)

- [ ] **Step 6: Commit**

```bash
git add src/config.rs tests/config_test.rs src/lib.rs
git commit -m "feat(config): add vsync.toml parser with package, deps, lint, format, scripts"
```

---

### Task 2: Config loading integration with CLI

**Files:**
- Modify: `src/main.rs:136-141` (ProjectContext struct)
- Modify: `src/main.rs` (resolve_project_context helper)
- Test: `tests/config_integration_test.rs`

- [ ] **Step 1: Write integration test for config loading**

```rust
// tests/config_integration_test.rs
use std::fs;
use tempfile::TempDir;
use vertigo_sync::config::{load_config, VsyncConfig};

#[test]
fn load_config_returns_none_when_missing() {
    let dir = TempDir::new().unwrap();
    let result = load_config(dir.path()).unwrap();
    assert!(result.is_none());
}

#[test]
fn load_config_parses_existing_file() {
    let dir = TempDir::new().unwrap();
    let config_content = r#"
[package]
name = "test/project"
version = "0.1.0"
realm = "shared"

[lint]
unused-variable = "warn"
"#;
    fs::write(dir.path().join("vsync.toml"), config_content).unwrap();
    let result = load_config(dir.path()).unwrap();
    assert!(result.is_some());
    let config = result.unwrap();
    assert_eq!(config.package.name, "test/project");
    assert_eq!(config.lint.get("unused-variable").map(|s| s.as_str()), Some("warn"));
}

#[test]
fn load_config_returns_error_on_invalid_toml() {
    let dir = TempDir::new().unwrap();
    fs::write(dir.path().join("vsync.toml"), "not valid [[[toml").unwrap();
    let result = load_config(dir.path());
    assert!(result.is_err());
}
```

- [ ] **Step 2: Run tests to verify they pass** (load_config already implemented in Task 1)

Run: `cargo test --test config_integration_test`
Expected: PASS

- [ ] **Step 3: Add VsyncConfig to ProjectContext in main.rs**

Modify the `ProjectContext` struct in `src/main.rs:136`:

```rust
#[derive(Debug)]
struct ProjectContext {
    project_path: PathBuf,
    project_root: PathBuf,
    includes: Vec<String>,
    tree: vertigo_sync::project::ProjectTree,
    vsync_config: Option<vertigo_sync::config::VsyncConfig>,
}
```

- [ ] **Step 4: Wire config loading into project resolution**

Find the function in `main.rs` that constructs `ProjectContext` (the helper that calls `parse_project`). After `parse_project` succeeds, add:

```rust
let vsync_config = vertigo_sync::config::load_config(&project_root)?;
```

And include it in the `ProjectContext` return value.

- [ ] **Step 5: Verify existing commands still work**

Run: `cargo test`
Expected: All existing tests pass — config is `Option`, so `None` is harmless.

Run: `cargo build && ./target/debug/vsync --help`
Expected: CLI still works.

- [ ] **Step 6: Commit**

```bash
git add src/main.rs tests/config_integration_test.rs
git commit -m "feat(config): wire vsync.toml loading into CLI project context"
```

---

### Task 3: Wally.toml fallback reader

**Files:**
- Create: `src/migrate.rs` (wally parsing subset used for fallback)
- Modify: `src/config.rs` (add `load_config_with_fallback`)
- Test: `tests/wally_fallback_test.rs`

- [ ] **Step 1: Write test for wally.toml fallback**

```rust
// tests/wally_fallback_test.rs
use std::fs;
use tempfile::TempDir;
use vertigo_sync::config::load_config_with_fallback;

#[test]
fn falls_back_to_wally_toml_when_no_vsync_toml() {
    let dir = TempDir::new().unwrap();
    let wally_content = r#"
[package]
name = "studio-player/my-game"
version = "0.1.0"
realm = "shared"

[dependencies]
Roact = "roblox/roact@17.0.1"

[server-dependencies]
DataStore2 = "kampfkarren/datastore2@1.5.0"
"#;
    fs::write(dir.path().join("wally.toml"), wally_content).unwrap();
    let config = load_config_with_fallback(dir.path()).unwrap();
    assert_eq!(config.package.name, "studio-player/my-game");
    assert_eq!(config.dependencies.len(), 1);
    assert_eq!(config.server_dependencies.len(), 1);
}

#[test]
fn vsync_toml_takes_precedence_over_wally_toml() {
    let dir = TempDir::new().unwrap();
    fs::write(dir.path().join("wally.toml"), r#"
[package]
name = "from-wally"
version = "0.1.0"
realm = "shared"
"#).unwrap();
    fs::write(dir.path().join("vsync.toml"), r#"
[package]
name = "from-vsync"
version = "0.2.0"
realm = "shared"
"#).unwrap();
    let config = load_config_with_fallback(dir.path()).unwrap();
    assert_eq!(config.package.name, "from-vsync");
}

#[test]
fn no_config_files_returns_default() {
    let dir = TempDir::new().unwrap();
    let config = load_config_with_fallback(dir.path()).unwrap();
    assert!(config.dependencies.is_empty());
    assert!(config.package.name.is_empty());
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test --test wally_fallback_test`
Expected: FAIL — `load_config_with_fallback` doesn't exist

- [ ] **Step 3: Implement wally.toml parsing in migrate.rs**

```rust
// src/migrate.rs
//! Migration utilities for converting Rojo ecosystem configs to vsync.toml.

use crate::config::{DependencySpec, PackageConfig, VsyncConfig};
use anyhow::{Context, Result};
use serde::Deserialize;
use std::collections::BTreeMap;
use std::path::Path;

/// Raw wally.toml schema (subset needed for reading).
#[derive(Debug, Deserialize, Default)]
struct WallyManifest {
    #[serde(default)]
    package: WallyPackage,
    #[serde(default)]
    dependencies: BTreeMap<String, String>,
    #[serde(default, rename = "server-dependencies")]
    server_dependencies: BTreeMap<String, String>,
    #[serde(default, rename = "dev-dependencies")]
    dev_dependencies: BTreeMap<String, String>,
}

#[derive(Debug, Deserialize, Default)]
struct WallyPackage {
    #[serde(default)]
    name: String,
    #[serde(default)]
    version: String,
    #[serde(default)]
    realm: String,
    #[serde(default)]
    description: String,
    #[serde(default)]
    license: String,
    #[serde(default)]
    authors: Vec<String>,
}

/// Parse a wally.toml and convert it to a VsyncConfig.
pub fn parse_wally_toml(path: &Path) -> Result<VsyncConfig> {
    let content = std::fs::read_to_string(path)
        .with_context(|| format!("failed to read {}", path.display()))?;
    let wally: WallyManifest = toml::from_str(&content)
        .with_context(|| format!("failed to parse {}", path.display()))?;

    let to_deps = |map: BTreeMap<String, String>| -> BTreeMap<String, DependencySpec> {
        map.into_iter()
            .map(|(k, v)| (k, DependencySpec::Simple(v)))
            .collect()
    };

    Ok(VsyncConfig {
        package: PackageConfig {
            name: wally.package.name,
            version: wally.package.version,
            realm: wally.package.realm,
            description: wally.package.description,
            license: wally.package.license,
            authors: wally.package.authors,
            packages_dir: None,
        },
        dependencies: to_deps(wally.dependencies),
        server_dependencies: to_deps(wally.server_dependencies),
        dev_dependencies: to_deps(wally.dev_dependencies),
        ..Default::default()
    })
}
```

- [ ] **Step 4: Add `load_config_with_fallback` to config.rs**

```rust
/// Load vsync.toml, falling back to wally.toml, falling back to defaults.
///
/// Precedence: vsync.toml > wally.toml > empty default.
pub fn load_config_with_fallback(project_root: &Path) -> Result<VsyncConfig> {
    // Prefer vsync.toml
    if let Some(config) = load_config(project_root)? {
        return Ok(config);
    }
    // Fallback to wally.toml
    let wally_path = project_root.join("wally.toml");
    if wally_path.exists() {
        return crate::migrate::parse_wally_toml(&wally_path);
    }
    // No config — return defaults
    Ok(VsyncConfig::default())
}
```

- [ ] **Step 5: Export migrate module from lib.rs**

Add to `src/lib.rs`:
```rust
pub mod migrate;
```

- [ ] **Step 6: Run tests to verify they pass**

Run: `cargo test --test wally_fallback_test`
Expected: PASS (all 3 tests)

- [ ] **Step 7: Commit**

```bash
git add src/migrate.rs src/config.rs src/lib.rs tests/wally_fallback_test.rs
git commit -m "feat(config): add wally.toml fallback reader for backward compatibility"
```

---

## Phase 2: Formatting (`vsync fmt`)

### Task 4: Add stylua_lib dependency

**Files:**
- Modify: `Cargo.toml`

- [ ] **Step 1: Add stylua_lib to Cargo.toml**

Add under `[dependencies]`:
```toml
stylua_lib = { package = "stylua-lib", version = "2", features = ["luau"] }
```

Note: The crate is published as `stylua-lib` on crates.io. The `luau` feature enables Luau syntax support (as opposed to plain Lua).

- [ ] **Step 2: Verify it compiles**

Run: `cargo check`
Expected: Compiles successfully. If the crate name or version is wrong, check crates.io for the current published name and adjust.

- [ ] **Step 3: Commit**

```bash
git add Cargo.toml Cargo.lock
git commit -m "deps: add stylua-lib for built-in Luau formatting"
```

---

### Task 5: `vsync fmt` implementation

**Files:**
- Create: `src/fmt.rs`
- Modify: `src/main.rs` (add Fmt command + handler)
- Test: `tests/fmt_test.rs`

- [ ] **Step 1: Write test for formatting a Luau string**

```rust
// tests/fmt_test.rs
use vertigo_sync::fmt::format_source;

#[test]
fn format_normalizes_indentation() {
    let input = "--!strict\nlocal  x=1\nreturn   x\n";
    let result = format_source(input, &Default::default()).unwrap();
    // StyLua should normalize whitespace around = and between tokens
    assert!(result.contains("local x = 1"));
    assert!(result.contains("return x"));
}

#[test]
fn format_respects_config_indent_type() {
    use vertigo_sync::config::FormatConfig;
    let input = "--!strict\nif true then\nprint('hi')\nend\n";
    let config = FormatConfig {
        indent_type: Some("spaces".to_string()),
        indent_width: Some(2),
        ..Default::default()
    };
    let result = format_source(input, &config).unwrap();
    // Should use 2-space indent, not tabs
    assert!(result.contains("  print"));
    assert!(!result.contains('\t'));
}

#[test]
fn format_check_returns_diff_status() {
    use vertigo_sync::fmt::check_source;
    let formatted = "local x = 1\n";
    let unformatted = "local  x=1\n";
    assert!(!check_source(formatted, &Default::default()).unwrap());
    assert!(check_source(unformatted, &Default::default()).unwrap());
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test --test fmt_test`
Expected: FAIL — `fmt` module doesn't exist

- [ ] **Step 3: Implement src/fmt.rs**

```rust
// src/fmt.rs
//! Built-in Luau formatter powered by StyLua.

use crate::config::FormatConfig;
use anyhow::{Context, Result};
use std::path::Path;

/// Format a Luau source string using the given config.
pub fn format_source(source: &str, config: &FormatConfig) -> Result<String> {
    let stylua_config = build_stylua_config(config);
    stylua_lib::format_code(source, stylua_config, None, stylua_lib::OutputVerification::None)
        .context("stylua formatting failed")
}

/// Returns `true` if the source would change when formatted.
pub fn check_source(source: &str, config: &FormatConfig) -> Result<bool> {
    let formatted = format_source(source, config)?;
    Ok(formatted != source)
}

/// Format a file in-place. Returns `true` if the file was changed.
pub fn format_file(path: &Path, config: &FormatConfig) -> Result<bool> {
    let source = std::fs::read_to_string(path)
        .with_context(|| format!("failed to read {}", path.display()))?;
    let formatted = format_source(&source, config)?;
    if formatted == source {
        return Ok(false);
    }
    std::fs::write(path, &formatted)
        .with_context(|| format!("failed to write {}", path.display()))?;
    Ok(true)
}

fn build_stylua_config(config: &FormatConfig) -> stylua_lib::Config {
    let mut sc = stylua_lib::Config::default();
    sc.set_syntax(stylua_lib::LuaVersion::Luau);

    if let Some(ref indent_type) = config.indent_type {
        sc.set_indent_type(match indent_type.as_str() {
            "spaces" => stylua_lib::IndentType::Spaces,
            _ => stylua_lib::IndentType::Tabs,
        });
    }
    if let Some(width) = config.indent_width {
        sc.set_indent_width(width as usize);
    }
    if let Some(width) = config.line_width {
        sc.set_column_width(width as usize);
    }
    if let Some(ref style) = config.quote_style {
        sc.set_quote_style(match style.as_str() {
            "single" => stylua_lib::QuoteStyle::ForceSingle,
            "auto" => stylua_lib::QuoteStyle::AutoPreferDouble,
            _ => stylua_lib::QuoteStyle::ForceDouble,
        });
    }
    if let Some(ref parens) = config.call_parentheses {
        sc.set_call_parentheses(match parens.as_str() {
            "no-single-string" => stylua_lib::CallParenType::NoSingleString,
            "no-single-table" => stylua_lib::CallParenType::NoSingleTable,
            "none" => stylua_lib::CallParenType::Omit,
            _ => stylua_lib::CallParenType::Always,
        });
    }

    sc
}
```

Note: The exact StyLua API may differ from what's shown here. The implementer MUST check the actual `stylua_lib` crate API by reading `stylua_lib`'s docs or source. The key types to verify:
- `stylua_lib::Config` — builder pattern vs struct fields
- `stylua_lib::format_code` — exact signature (may take `Config` by ref or value)
- `stylua_lib::LuaVersion::Luau` — enum variant name
- `stylua_lib::IndentType`, `QuoteStyle`, `CallParenType` — exact names

Adjust the implementation to match the real API.

- [ ] **Step 4: Export the module from lib.rs**

Add to `src/lib.rs`:
```rust
pub mod fmt;
```

- [ ] **Step 5: Run tests to verify they pass**

Run: `cargo test --test fmt_test`
Expected: PASS (all 3 tests). If the StyLua API doesn't match, fix the implementation first.

- [ ] **Step 6: Commit**

```bash
git add src/fmt.rs src/lib.rs tests/fmt_test.rs
git commit -m "feat(fmt): add built-in Luau formatter powered by stylua-lib"
```

---

### Task 6: Wire `vsync fmt` into CLI

**Files:**
- Modify: `src/main.rs` (add Fmt command variant + handler)

- [ ] **Step 1: Add Fmt variant to Command enum**

In `src/main.rs`, add to the `Command` enum after the `Init` variant:

```rust
    /// Format Luau source files.
    #[command(display_order = 43)]
    Fmt {
        /// Check mode — exit 1 if any file would change, don't write.
        #[arg(long, default_value_t = false)]
        check: bool,
        /// Show unified diff of what would change.
        #[arg(long, default_value_t = false)]
        diff: bool,
        /// Specific path to format (default: all project includes).
        path: Option<PathBuf>,
    },
```

- [ ] **Step 2: Add the command handler function**

```rust
fn command_fmt(root: &Path, includes: &[String], config: &vertigo_sync::config::VsyncConfig, check: bool, show_diff: bool, path: Option<&Path>) -> Result<()> {
    let format_config = &config.format;

    // Collect .luau and .lua files from either the specified path or all includes
    let search_roots: Vec<PathBuf> = if let Some(p) = path {
        vec![root.join(p)]
    } else {
        includes.iter().map(|inc| root.join(inc)).collect()
    };

    let mut files: Vec<PathBuf> = Vec::new();
    for search_root in &search_roots {
        if search_root.is_file() {
            files.push(search_root.clone());
        } else if search_root.is_dir() {
            collect_luau_files(search_root, &mut files)?;
        }
    }

    let mut changed_count = 0u32;
    let mut error_count = 0u32;

    for file in &files {
        let source = std::fs::read_to_string(file)?;
        match vertigo_sync::fmt::format_source(&source, format_config) {
            Ok(formatted) => {
                if formatted != source {
                    changed_count += 1;
                    let rel = file.strip_prefix(root).unwrap_or(file);
                    if check {
                        output::warn(&format!("would reformat {}", rel.display()));
                    } else if show_diff {
                        output::info(&format!("--- {}", rel.display()));
                        // Simple line-by-line diff (not a full unified diff, but useful)
                        output::info("(file would be reformatted)");
                    } else {
                        std::fs::write(file, &formatted)?;
                        output::success(&format!("formatted {}", rel.display()));
                    }
                }
            }
            Err(e) => {
                error_count += 1;
                let rel = file.strip_prefix(root).unwrap_or(file);
                output::warn(&format!("failed to format {}: {e}", rel.display()));
            }
        }
    }

    let total = files.len();
    if check {
        if changed_count > 0 {
            bail!("{changed_count} file(s) would be reformatted (of {total} checked)");
        }
        output::success(&format!("All {total} file(s) formatted correctly"));
    } else {
        output::success(&format!("{changed_count} file(s) formatted, {total} checked, {error_count} error(s)"));
    }

    Ok(())
}

fn collect_luau_files(dir: &Path, out: &mut Vec<PathBuf>) -> Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.is_dir() {
            collect_luau_files(&path, out)?;
        } else if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
            if ext == "luau" || ext == "lua" {
                out.push(path);
            }
        }
    }
    Ok(())
}
```

- [ ] **Step 3: Wire the match arm in the main function**

In the main `match cli.command { ... }` block, add:

```rust
Command::Fmt { check, diff, path } => {
    let ctx = resolve_project_context(&cli)?;
    let config = ctx.vsync_config.unwrap_or_default();
    command_fmt(&ctx.project_root, &ctx.includes, &config, check, diff, path.as_deref())?;
}
```

- [ ] **Step 4: Verify it compiles and runs**

Run: `cargo build && ./target/debug/vsync fmt --help`
Expected: Shows fmt subcommand help with `--check` and `--diff` flags.

- [ ] **Step 5: Manual smoke test**

Run: `cd /tmp && mkdir fmt-test && cd fmt-test && /path/to/vsync init && /path/to/vsync fmt`
Expected: Formats the scaffolded .luau files (or reports "0 files formatted" if they're already clean).

- [ ] **Step 6: Commit**

```bash
git add src/main.rs
git commit -m "feat(cli): add vsync fmt command with --check and --diff flags"
```

---

## Phase 3: Expanded Linter

### Task 7: Refactor validate.rs into src/lint/ module

**Files:**
- Create: `src/lint/mod.rs` (re-exports, new rule infrastructure)
- Create: `src/lint/rules.rs` (new P0 correctness rules)
- Modify: `src/validate.rs` (re-export from lint module, keep backward compat)
- Test: `tests/lint_test.rs`

This task restructures the existing validation code to support the new rule system. Existing rules stay in `validate.rs` (they already work). New rules go in `src/lint/rules.rs`. The `src/lint/mod.rs` module unifies both.

- [ ] **Step 1: Write test for new correctness rules**

```rust
// tests/lint_test.rs
use vertigo_sync::lint::{LintRule, lint_source, LintSeverity};

#[test]
fn detects_unused_variable() {
    let source = r#"--!strict
local unused = 42
print("hello")
"#;
    let issues = lint_source(source, "test.luau", &default_rules());
    let unused_issues: Vec<_> = issues.iter()
        .filter(|i| i.rule == "unused-variable")
        .collect();
    assert!(!unused_issues.is_empty(), "should detect unused variable");
}

#[test]
fn no_false_positive_on_used_variable() {
    let source = r#"--!strict
local x = 42
print(x)
"#;
    let issues = lint_source(source, "test.luau", &default_rules());
    let unused_issues: Vec<_> = issues.iter()
        .filter(|i| i.rule == "unused-variable")
        .collect();
    assert!(unused_issues.is_empty(), "should not flag used variable");
}

#[test]
fn detects_global_shadow() {
    let source = r#"--!strict
local game = {}
print(game)
"#;
    let issues = lint_source(source, "test.luau", &default_rules());
    let shadow_issues: Vec<_> = issues.iter()
        .filter(|i| i.rule == "global-shadow")
        .collect();
    assert!(!shadow_issues.is_empty(), "should detect shadowed global 'game'");
}

#[test]
fn detects_deprecated_wait() {
    let source = r#"--!strict
wait(1)
"#;
    let issues = lint_source(source, "test.luau", &default_rules());
    let wait_issues: Vec<_> = issues.iter()
        .filter(|i| i.rule == "wait-deprecated")
        .collect();
    assert!(!wait_issues.is_empty(), "should detect deprecated wait()");
}

#[test]
fn respects_rule_config_off() {
    use std::collections::BTreeMap;
    let source = r#"--!strict
local unused = 42
print("hello")
"#;
    let mut rules = BTreeMap::new();
    rules.insert("unused-variable".to_string(), "off".to_string());
    let issues = lint_source(source, "test.luau", &rules);
    let unused_issues: Vec<_> = issues.iter()
        .filter(|i| i.rule == "unused-variable")
        .collect();
    assert!(unused_issues.is_empty(), "rule should be suppressed when 'off'");
}

fn default_rules() -> std::collections::BTreeMap<String, String> {
    let mut rules = std::collections::BTreeMap::new();
    rules.insert("unused-variable".to_string(), "warn".to_string());
    rules.insert("global-shadow".to_string(), "warn".to_string());
    rules.insert("wait-deprecated".to_string(), "warn".to_string());
    rules
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test --test lint_test`
Expected: FAIL — `lint` module doesn't exist

- [ ] **Step 3: Implement src/lint/mod.rs**

```rust
// src/lint/mod.rs
//! Unified Luau linter — replaces Selene with built-in rules.

pub mod rules;

use std::collections::BTreeMap;

/// A single lint diagnostic.
#[derive(Debug, Clone)]
pub struct LintIssue {
    pub rule: String,
    pub severity: LintSeverity,
    pub message: String,
    pub line: usize,
    pub column: usize,
    pub file: String,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum LintSeverity {
    Error,
    Warning,
}

/// Lint a source string using the provided rule configuration.
///
/// `rule_config` maps rule names to severity strings: "error", "warn", "off".
/// Rules not in the config default to "warn".
pub fn lint_source(source: &str, file: &str, rule_config: &BTreeMap<String, String>) -> Vec<LintIssue> {
    let mut issues = Vec::new();

    // Run each rule if not disabled
    let rule_fns: &[(&str, fn(&str, &str) -> Vec<LintIssue>)] = &[
        ("unused-variable", rules::check_unused_variables),
        ("global-shadow", rules::check_global_shadow),
        ("wait-deprecated", rules::check_deprecated_wait),
        ("spawn-deprecated", rules::check_deprecated_spawn),
        ("delay-deprecated", rules::check_deprecated_delay),
        ("empty-block", rules::check_empty_blocks),
        ("unreachable-code", rules::check_unreachable_code),
    ];

    for (rule_name, rule_fn) in rule_fns {
        let severity_str = rule_config
            .get(*rule_name)
            .map(|s| s.as_str())
            .unwrap_or("warn");
        if severity_str == "off" {
            continue;
        }
        let severity = if severity_str == "error" {
            LintSeverity::Error
        } else {
            LintSeverity::Warning
        };

        let mut rule_issues = rule_fn(source, file);
        for issue in &mut rule_issues {
            issue.severity = severity;
        }
        issues.extend(rule_issues);
    }

    issues
}
```

- [ ] **Step 4: Implement src/lint/rules.rs with P0 correctness rules**

```rust
// src/lint/rules.rs
//! Individual lint rule implementations.

use super::{LintIssue, LintSeverity};
use regex::Regex;
use std::collections::HashSet;
use std::sync::LazyLock;

/// Roblox globals that should not be shadowed.
static ROBLOX_GLOBALS: LazyLock<HashSet<&'static str>> = LazyLock::new(|| {
    [
        "game", "workspace", "script", "plugin", "tick", "time",
        "wait", "spawn", "delay", "print", "warn", "error",
        "typeof", "type", "require", "select", "unpack",
        "Instance", "Vector3", "Vector2", "CFrame", "Color3",
        "UDim", "UDim2", "Enum", "Ray", "Region3", "TweenInfo",
        "NumberSequence", "ColorSequence", "NumberRange",
        "BrickColor", "Rect", "PhysicalProperties",
        "math", "string", "table", "coroutine", "task", "debug",
        "os", "utf8", "buffer", "bit32",
    ].into_iter().collect()
});

/// Pattern: `local <name>` declarations.
static LOCAL_DECL_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?m)^\s*local\s+([a-zA-Z_]\w*)(?:\s*[=:,])").unwrap()
});

/// Check for local variables that are declared but never referenced again.
///
/// This is a simple text-based heuristic — not a full scope-aware analysis.
/// It catches the common case of `local x = ...` where `x` never appears
/// again in the file. Variables starting with `_` are excluded (convention
/// for intentionally unused).
pub fn check_unused_variables(source: &str, file: &str) -> Vec<LintIssue> {
    let mut issues = Vec::new();

    for (line_idx, line) in source.lines().enumerate() {
        if let Some(caps) = LOCAL_DECL_RE.captures(line) {
            let name = caps.get(1).unwrap().as_str();
            // Skip _ prefixed (conventional unused marker)
            if name.starts_with('_') {
                continue;
            }
            // Skip common patterns: function declarations, loop variables
            if line.contains("function") {
                continue;
            }
            // Count occurrences of the identifier in the rest of the file
            let rest = &source[source.lines().take(line_idx + 1).map(|l| l.len() + 1).sum::<usize>()..];
            let pattern = format!(r"\b{}\b", regex::escape(name));
            if let Ok(re) = Regex::new(&pattern) {
                if re.find(rest).is_none() {
                    issues.push(LintIssue {
                        rule: "unused-variable".to_string(),
                        severity: LintSeverity::Warning,
                        message: format!("variable '{name}' is declared but never used"),
                        line: line_idx + 1,
                        column: 0,
                        file: file.to_string(),
                    });
                }
            }
        }
    }

    issues
}

/// Check for locals that shadow Roblox globals.
pub fn check_global_shadow(source: &str, file: &str) -> Vec<LintIssue> {
    let mut issues = Vec::new();

    for (line_idx, line) in source.lines().enumerate() {
        if let Some(caps) = LOCAL_DECL_RE.captures(line) {
            let name = caps.get(1).unwrap().as_str();
            if ROBLOX_GLOBALS.contains(name) {
                issues.push(LintIssue {
                    rule: "global-shadow".to_string(),
                    severity: LintSeverity::Warning,
                    message: format!("local '{name}' shadows Roblox global"),
                    line: line_idx + 1,
                    column: 0,
                    file: file.to_string(),
                });
            }
        }
    }

    issues
}

/// Check for deprecated `wait()` — should use `task.wait()`.
pub fn check_deprecated_wait(source: &str, file: &str) -> Vec<LintIssue> {
    check_deprecated_call(source, file, "wait", "task.wait", "wait-deprecated")
}

/// Check for deprecated `spawn()` — should use `task.spawn()`.
pub fn check_deprecated_spawn(source: &str, file: &str) -> Vec<LintIssue> {
    check_deprecated_call(source, file, "spawn", "task.spawn", "spawn-deprecated")
}

/// Check for deprecated `delay()` — should use `task.delay()`.
pub fn check_deprecated_delay(source: &str, file: &str) -> Vec<LintIssue> {
    check_deprecated_call(source, file, "delay", "task.delay", "delay-deprecated")
}

fn check_deprecated_call(source: &str, file: &str, old: &str, new: &str, rule: &str) -> Vec<LintIssue> {
    let mut issues = Vec::new();
    // Match bare `wait(` but not `task.wait(` or `something.wait(`
    let pattern = format!(r"(?<![.\w]){}\s*\(", regex::escape(old));
    let re = Regex::new(&pattern).unwrap();

    for (line_idx, line) in source.lines().enumerate() {
        // Skip comments
        let trimmed = line.trim();
        if trimmed.starts_with("--") {
            continue;
        }
        if re.is_match(line) {
            issues.push(LintIssue {
                rule: rule.to_string(),
                severity: LintSeverity::Warning,
                message: format!("'{old}()' is deprecated, use '{new}()' instead"),
                line: line_idx + 1,
                column: 0,
                file: file.to_string(),
            });
        }
    }

    issues
}

/// Check for empty if/for/while blocks.
pub fn check_empty_blocks(source: &str, file: &str) -> Vec<LintIssue> {
    let mut issues = Vec::new();
    let re = Regex::new(r"(?m)(then|do)\s*\n\s*(end|else|elseif)").unwrap();

    for mat in re.find_iter(source) {
        let line_idx = source[..mat.start()].matches('\n').count() + 1;
        issues.push(LintIssue {
            rule: "empty-block".to_string(),
            severity: LintSeverity::Warning,
            message: "empty block body".to_string(),
            line: line_idx,
            column: 0,
            file: file.to_string(),
        });
    }

    issues
}

/// Check for code after unconditional return/break/continue.
pub fn check_unreachable_code(source: &str, file: &str) -> Vec<LintIssue> {
    let mut issues = Vec::new();
    let lines: Vec<&str> = source.lines().collect();

    for (i, line) in lines.iter().enumerate() {
        let trimmed = line.trim();
        // Skip comments
        if trimmed.starts_with("--") {
            continue;
        }
        // Check if this line is an unconditional return/break/continue/error()
        let is_terminal = trimmed == "return"
            || trimmed.starts_with("return ")
            || trimmed == "break"
            || trimmed == "continue"
            || trimmed.starts_with("error(");

        if is_terminal {
            // Check if the next non-empty, non-comment line is NOT end/else/elseif/until
            if let Some(next_line) = lines.get(i + 1) {
                let next_trimmed = next_line.trim();
                if !next_trimmed.is_empty()
                    && !next_trimmed.starts_with("--")
                    && !next_trimmed.starts_with("end")
                    && !next_trimmed.starts_with("else")
                    && !next_trimmed.starts_with("elseif")
                    && !next_trimmed.starts_with("until")
                {
                    issues.push(LintIssue {
                        rule: "unreachable-code".to_string(),
                        severity: LintSeverity::Warning,
                        message: "code after unconditional return/break/continue is unreachable".to_string(),
                        line: i + 2,
                        column: 0,
                        file: file.to_string(),
                    });
                }
            }
        }
    }

    issues
}
```

- [ ] **Step 5: Export the lint module from lib.rs**

Add to `src/lib.rs`:
```rust
pub mod lint;
```

- [ ] **Step 6: Run tests to verify they pass**

Run: `cargo test --test lint_test`
Expected: PASS (all 5 tests)

- [ ] **Step 7: Commit**

```bash
git add src/lint/ src/lib.rs tests/lint_test.rs
git commit -m "feat(lint): add built-in P0 correctness rules (unused-variable, global-shadow, deprecated-wait, empty-block, unreachable-code)"
```

---

### Task 8: Wire lint rules into `vsync validate` with config

**Files:**
- Modify: `src/main.rs` (validate command handler)
- Modify: `src/validate.rs` (integrate new lint module)

- [ ] **Step 1: Add lint integration call in the validate command handler**

Find the `Command::Validate` match arm in `main.rs`. After the existing `validate::validate_source()` call, add the new lint pass:

```rust
// Run expanded lint rules if vsync config has lint section
if let Some(ref config) = ctx.vsync_config {
    let lint_issues = vertigo_sync::lint::lint_source_tree(
        &ctx.project_root,
        &ctx.includes,
        &config.lint,
    );
    // Merge into the validation report or print separately
    for issue in &lint_issues {
        let severity_label = match issue.severity {
            vertigo_sync::lint::LintSeverity::Error => "error",
            vertigo_sync::lint::LintSeverity::Warning => "warning",
        };
        eprintln!("  {}:{}: {} [{}] {}", issue.file, issue.line, severity_label, issue.rule, issue.message);
    }
}
```

- [ ] **Step 2: Add `lint_source_tree` to src/lint/mod.rs**

```rust
/// Lint all Luau files in the given include roots.
pub fn lint_source_tree(
    root: &std::path::Path,
    includes: &[String],
    rule_config: &std::collections::BTreeMap<String, String>,
) -> Vec<LintIssue> {
    let mut all_issues = Vec::new();

    for include in includes {
        let include_path = root.join(include);
        if !include_path.is_dir() {
            continue;
        }
        lint_directory(&include_path, root, rule_config, &mut all_issues);
    }

    all_issues
}

fn lint_directory(
    dir: &std::path::Path,
    root: &std::path::Path,
    rule_config: &std::collections::BTreeMap<String, String>,
    issues: &mut Vec<LintIssue>,
) {
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return,
    };
    for entry in entries {
        let entry = match entry {
            Ok(e) => e,
            Err(_) => continue,
        };
        let path = entry.path();
        if path.is_dir() {
            lint_directory(&path, root, rule_config, issues);
        } else if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
            if ext == "luau" || ext == "lua" {
                if let Ok(source) = std::fs::read_to_string(&path) {
                    let rel = path.strip_prefix(root).unwrap_or(&path);
                    let file_str = rel.to_string_lossy().to_string();
                    let file_issues = lint_source(&source, &file_str, rule_config);
                    issues.extend(file_issues);
                }
            }
        }
    }
}
```

- [ ] **Step 3: Verify compilation and test**

Run: `cargo build && cargo test`
Expected: All pass. Existing validate behavior unchanged; new rules additive.

- [ ] **Step 4: Commit**

```bash
git add src/main.rs src/lint/mod.rs
git commit -m "feat(lint): integrate P0 rules into vsync validate command"
```

---

## Phase 4: Package Management

### Task 9: Add package management dependencies

**Files:**
- Modify: `Cargo.toml`

- [ ] **Step 1: Add reqwest and pubgrub to Cargo.toml**

```toml
reqwest = { version = "0.12", features = ["json", "stream"] }
pubgrub = "0.2"
flate2 = "1"
zip = "2"
dirs = "6"
```

Note: Check crates.io for the current `pubgrub` crate version. If no stable release exists or the API doesn't match, use a simpler backtracking resolver initially and add pubgrub later.

- [ ] **Step 2: Verify it compiles**

Run: `cargo check`
Expected: Compiles. Resolve any version conflicts.

- [ ] **Step 3: Commit**

```bash
git add Cargo.toml Cargo.lock
git commit -m "deps: add reqwest, pubgrub, zip, flate2, dirs for package management"
```

---

### Task 10: Package cache and lockfile types

**Files:**
- Create: `src/package/mod.rs`
- Create: `src/package/lockfile.rs`
- Create: `src/package/cache.rs`
- Test: `tests/lockfile_test.rs`

- [ ] **Step 1: Write test for lockfile round-trip**

```rust
// tests/lockfile_test.rs
use vertigo_sync::package::lockfile::{Lockfile, LockedPackage};

#[test]
fn lockfile_roundtrip() {
    let lockfile = Lockfile {
        lockfile_version: 1,
        packages: vec![
            LockedPackage {
                name: "roblox/roact".to_string(),
                version: "17.1.0".to_string(),
                realm: "shared".to_string(),
                checksum: "sha256:abc123".to_string(),
                source: "registry+https://registry.wally.run".to_string(),
                dependencies: vec![],
            },
            LockedPackage {
                name: "evaera/promise".to_string(),
                version: "4.0.1".to_string(),
                realm: "shared".to_string(),
                checksum: "sha256:def456".to_string(),
                source: "registry+https://registry.wally.run".to_string(),
                dependencies: vec!["roblox/roact@^17.0.0".to_string()],
            },
        ],
    };

    let serialized = lockfile.to_string();
    assert!(serialized.contains("lockfile-version = 1"));
    assert!(serialized.contains("roblox/roact"));

    let parsed = Lockfile::from_str(&serialized).unwrap();
    assert_eq!(parsed.lockfile_version, 1);
    assert_eq!(parsed.packages.len(), 2);
    assert_eq!(parsed.packages[0].name, "roblox/roact");
}

#[test]
fn lockfile_rejects_future_version() {
    let toml = r#"
lockfile-version = 99

[[package]]
name = "test/pkg"
version = "1.0.0"
realm = "shared"
checksum = "sha256:aaa"
source = "registry+https://registry.wally.run"
"#;
    let result = Lockfile::from_str(toml);
    assert!(result.is_err());
    let err_msg = result.unwrap_err().to_string();
    assert!(err_msg.contains("upgrade vsync") || err_msg.contains("unsupported lockfile version"));
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test --test lockfile_test`
Expected: FAIL — module doesn't exist

- [ ] **Step 3: Implement src/package/mod.rs**

```rust
// src/package/mod.rs
//! Package management: registry client, resolver, downloader, cache, lockfile.

pub mod cache;
pub mod lockfile;
```

- [ ] **Step 4: Implement src/package/lockfile.rs**

```rust
// src/package/lockfile.rs
//! vsync.lock file parser and serializer.

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::path::Path;

const CURRENT_LOCKFILE_VERSION: u32 = 1;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Lockfile {
    #[serde(rename = "lockfile-version")]
    pub lockfile_version: u32,
    #[serde(rename = "package", default)]
    pub packages: Vec<LockedPackage>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct LockedPackage {
    pub name: String,
    pub version: String,
    pub realm: String,
    pub checksum: String,
    pub source: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub dependencies: Vec<String>,
}

impl Lockfile {
    pub fn new() -> Self {
        Self {
            lockfile_version: CURRENT_LOCKFILE_VERSION,
            packages: Vec::new(),
        }
    }

    pub fn from_str(content: &str) -> Result<Self> {
        let lockfile: Lockfile = toml::from_str(content)
            .context("failed to parse vsync.lock")?;
        if lockfile.lockfile_version > CURRENT_LOCKFILE_VERSION {
            bail!(
                "unsupported lockfile version {} (this vsync supports up to {}). \
                 Please upgrade vsync.",
                lockfile.lockfile_version,
                CURRENT_LOCKFILE_VERSION
            );
        }
        Ok(lockfile)
    }

    pub fn load(path: &Path) -> Result<Option<Self>> {
        if !path.exists() {
            return Ok(None);
        }
        let content = std::fs::read_to_string(path)
            .with_context(|| format!("failed to read {}", path.display()))?;
        Self::from_str(&content).map(Some)
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        let content = self.to_string();
        std::fs::write(path, content.as_bytes())
            .with_context(|| format!("failed to write {}", path.display()))
    }
}

impl std::fmt::Display for Lockfile {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        writeln!(f, "# vsync.lock — auto-generated, do not edit")?;
        writeln!(f, "# This file ensures deterministic installs across machines.")?;
        let content = toml::to_string_pretty(self).map_err(|_| std::fmt::Error)?;
        write!(f, "{content}")
    }
}
```

- [ ] **Step 5: Implement src/package/cache.rs (skeleton)**

```rust
// src/package/cache.rs
//! Local package cache at ~/.vsync/cache/.

use anyhow::{Context, Result};
use std::path::{Path, PathBuf};

/// Returns the cache root directory, creating it if needed.
pub fn cache_root() -> Result<PathBuf> {
    let home = dirs::home_dir().context("could not determine home directory")?;
    let cache_dir = home.join(".vsync").join("cache").join("packages");
    std::fs::create_dir_all(&cache_dir)
        .with_context(|| format!("failed to create cache directory {}", cache_dir.display()))?;
    Ok(cache_dir)
}

/// Returns the path where a package with the given checksum would be cached.
pub fn cached_package_path(checksum: &str) -> Result<PathBuf> {
    let root = cache_root()?;
    Ok(root.join(format!("{checksum}.zip")))
}

/// Check if a package is already in the cache.
pub fn is_cached(checksum: &str) -> Result<bool> {
    let path = cached_package_path(checksum)?;
    Ok(path.exists())
}
```

- [ ] **Step 6: Export the package module from lib.rs**

Add to `src/lib.rs`:
```rust
pub mod package;
```

- [ ] **Step 7: Run tests to verify they pass**

Run: `cargo test --test lockfile_test`
Expected: PASS (both tests)

- [ ] **Step 8: Commit**

```bash
git add src/package/ src/lib.rs tests/lockfile_test.rs
git commit -m "feat(package): add lockfile parser and cache skeleton"
```

---

### Task 11: Registry client

**Files:**
- Create: `src/package/registry.rs`
- Test: `tests/registry_test.rs`

- [ ] **Step 1: Write test for parsing Wally registry index entries**

```rust
// tests/registry_test.rs
use vertigo_sync::package::registry::IndexEntry;

#[test]
fn parse_index_entry() {
    let json = r#"{"name":"roblox/roact","version":"17.1.0","realm":"shared","dependencies":{},"server-dependencies":{},"description":"A declarative UI library"}"#;
    let entry: IndexEntry = serde_json::from_str(json).unwrap();
    assert_eq!(entry.name, "roblox/roact");
    assert_eq!(entry.version, "17.1.0");
    assert_eq!(entry.realm, "shared");
}

#[test]
fn parse_version_requirement() {
    use vertigo_sync::package::registry::parse_version_req;
    let (scope, name, req) = parse_version_req("roblox/roact@^17.0.0").unwrap();
    assert_eq!(scope, "roblox");
    assert_eq!(name, "roact");
    assert_eq!(req, "^17.0.0");
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test --test registry_test`
Expected: FAIL

- [ ] **Step 3: Implement src/package/registry.rs**

```rust
// src/package/registry.rs
//! Wally-compatible registry client.

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// A single version entry from the registry index.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IndexEntry {
    pub name: String,
    pub version: String,
    pub realm: String,
    #[serde(default)]
    pub dependencies: BTreeMap<String, String>,
    #[serde(default, rename = "server-dependencies")]
    pub server_dependencies: BTreeMap<String, String>,
    #[serde(default)]
    pub description: String,
}

/// Parse a version requirement string like "roblox/roact@^17.0.0".
pub fn parse_version_req(spec: &str) -> Result<(String, String, String)> {
    // Format: "scope/name@version_req"
    let at_pos = spec.find('@')
        .context("dependency spec must contain '@' (e.g., roblox/roact@^17.0.0)")?;
    let path = &spec[..at_pos];
    let version_req = &spec[at_pos + 1..];

    let slash_pos = path.find('/')
        .context("dependency spec must contain '/' (e.g., roblox/roact@^17.0.0)")?;
    let scope = &path[..slash_pos];
    let name = &path[slash_pos + 1..];

    if scope.is_empty() || name.is_empty() || version_req.is_empty() {
        bail!("invalid dependency spec: '{spec}'");
    }

    Ok((scope.to_string(), name.to_string(), version_req.to_string()))
}

/// Registry API client for fetching package metadata and archives.
pub struct RegistryClient {
    pub base_url: String,
    pub api_url: String,
    client: reqwest::Client,
}

impl RegistryClient {
    /// Create a client for the default Wally registry.
    pub fn default_wally() -> Self {
        Self {
            base_url: "https://registry.wally.run".to_string(),
            api_url: "https://api.wally.run".to_string(),
            client: reqwest::Client::new(),
        }
    }

    /// Create a client for a custom registry.
    pub fn new(base_url: String, api_url: String) -> Self {
        Self {
            base_url,
            api_url,
            client: reqwest::Client::new(),
        }
    }

    /// Fetch all versions of a package from the registry API.
    pub async fn fetch_versions(&self, scope: &str, name: &str) -> Result<Vec<IndexEntry>> {
        let url = format!("{}/v1/package-versions/{scope}/{name}", self.api_url);
        let response = self.client.get(&url)
            .send()
            .await
            .with_context(|| format!("failed to fetch versions for {scope}/{name}"))?;

        if !response.status().is_success() {
            bail!("registry returned {} for {scope}/{name}", response.status());
        }

        let entries: Vec<IndexEntry> = response.json().await
            .with_context(|| format!("failed to parse versions for {scope}/{name}"))?;
        Ok(entries)
    }

    /// Download a package archive. Returns the raw bytes.
    pub async fn download_package(&self, scope: &str, name: &str, version: &str) -> Result<Vec<u8>> {
        let url = format!("{}/v1/package-contents/{scope}/{name}/{version}", self.api_url);
        let response = self.client.get(&url)
            .send()
            .await
            .with_context(|| format!("failed to download {scope}/{name}@{version}"))?;

        if !response.status().is_success() {
            bail!("registry returned {} for {scope}/{name}@{version}", response.status());
        }

        let bytes = response.bytes().await
            .with_context(|| format!("failed to read response for {scope}/{name}@{version}"))?;
        Ok(bytes.to_vec())
    }
}
```

Note: The Wally registry API URLs are approximated here. The implementer MUST verify the actual API endpoints by inspecting the Wally source code or making test requests. Key questions:
- What is the actual URL pattern for fetching versions?
- What is the URL for downloading package contents?
- Does it require authentication for public packages?

- [ ] **Step 4: Export from package/mod.rs**

```rust
pub mod registry;
```

- [ ] **Step 5: Run tests to verify they pass**

Run: `cargo test --test registry_test`
Expected: PASS (parsing tests only — no network calls)

- [ ] **Step 6: Commit**

```bash
git add src/package/registry.rs src/package/mod.rs tests/registry_test.rs
git commit -m "feat(package): add Wally-compatible registry client with version parsing"
```

---

### Task 12: `vsync install` command (basic flow)

**Files:**
- Create: `src/package/installer.rs`
- Modify: `src/main.rs` (add Install/Add/Remove/Update commands)
- Modify: `src/errors.rs` (add PackageError variant)

- [ ] **Step 1: Add PackageError to errors.rs**

```rust
// Add to the SyncError enum in src/errors.rs:

    /// Package registry unreachable after retries.
    RegistryUnreachable { url: String },
    /// Package checksum verification failed.
    ChecksumMismatch { package: String, expected: String, actual: String },
    /// Conflicting version requirements in dependency tree.
    DependencyConflict { package: String, message: String },
    /// Package not found in registry.
    PackageNotFound { name: String },
```

Add corresponding `title()`, `explanation()`, and `suggestion()` implementations for each variant.

- [ ] **Step 2: Create src/package/installer.rs**

```rust
// src/package/installer.rs
//! Package installation orchestrator.

use crate::config::{DependencySpec, VsyncConfig};
use crate::package::cache;
use crate::package::lockfile::{Lockfile, LockedPackage};
use crate::package::registry::{self, RegistryClient};
use anyhow::{Context, Result};
use std::path::Path;

/// Run the install flow: resolve → download → extract → write lockfile.
pub async fn install(project_root: &Path, config: &VsyncConfig) -> Result<InstallReport> {
    let lockfile_path = project_root.join("vsync.lock");
    let existing_lock = Lockfile::load(&lockfile_path)?;
    let packages_dir = project_root.join(
        config.package.packages_dir.as_deref().unwrap_or("Packages")
    );

    // Collect all dependencies
    let all_deps = collect_all_deps(config);
    if all_deps.is_empty() {
        return Ok(InstallReport { installed: 0, cached: 0, total: 0 });
    }

    let client = RegistryClient::default_wally();
    let mut new_lockfile = Lockfile::new();
    let mut installed = 0u32;
    let mut cached = 0u32;

    // Resolve and download each dependency
    // NOTE: This is a simplified sequential resolver. Replace with pubgrub for
    // proper transitive dependency resolution in a follow-up task.
    for (name, spec) in &all_deps {
        match spec {
            DependencySpec::Simple(version_str) => {
                let (scope, pkg_name, version_req) = registry::parse_version_req(version_str)?;

                // Fetch available versions
                let versions = client.fetch_versions(&scope, &pkg_name).await?;
                let selected = select_version(&versions, &version_req)?;

                // Check if already in cache
                let checksum = format!("sha256:{}", compute_package_id(&scope, &pkg_name, &selected.version));

                if cache::is_cached(&checksum)? {
                    cached += 1;
                } else {
                    // Download
                    let bytes = client.download_package(&scope, &pkg_name, &selected.version).await?;
                    let cache_path = cache::cached_package_path(&checksum)?;
                    std::fs::write(&cache_path, &bytes)?;
                    installed += 1;
                }

                // Extract to Packages/
                let pkg_dir = packages_dir.join(&scope).join(&pkg_name);
                std::fs::create_dir_all(&pkg_dir)?;
                // TODO: Extract zip archive to pkg_dir

                new_lockfile.packages.push(LockedPackage {
                    name: format!("{scope}/{pkg_name}"),
                    version: selected.version.clone(),
                    realm: selected.realm.clone(),
                    checksum,
                    source: format!("registry+{}", client.base_url),
                    dependencies: selected.dependencies.values().cloned().collect(),
                });
            }
            DependencySpec::Path { path } => {
                // Path dependencies don't need download — just verify they exist
                let dep_path = project_root.join(path);
                if !dep_path.exists() {
                    anyhow::bail!("path dependency '{}' does not exist at {}", name, dep_path.display());
                }
            }
            DependencySpec::Git { .. } => {
                // Git dependencies: clone/fetch — v1.1 feature, skip for now
                anyhow::bail!("git dependencies are not yet supported (coming in v1.1)");
            }
            DependencySpec::Registry { .. } => {
                // Private registry: v1.1 feature
                anyhow::bail!("private registries are not yet supported (coming in v1.1)");
            }
        }
    }

    // Write lockfile
    new_lockfile.save(&lockfile_path)?;

    let total = all_deps.len() as u32;
    Ok(InstallReport { installed, cached, total })
}

pub struct InstallReport {
    pub installed: u32,
    pub cached: u32,
    pub total: u32,
}

fn collect_all_deps(config: &VsyncConfig) -> Vec<(String, DependencySpec)> {
    let mut all = Vec::new();
    for (k, v) in &config.dependencies {
        all.push((k.clone(), v.clone()));
    }
    for (k, v) in &config.server_dependencies {
        all.push((k.clone(), v.clone()));
    }
    for (k, v) in &config.dev_dependencies {
        all.push((k.clone(), v.clone()));
    }
    all
}

fn select_version(versions: &[registry::IndexEntry], _version_req: &str) -> Result<&registry::IndexEntry> {
    // TODO: Proper semver matching. For now, pick the latest version.
    versions.last().context("no versions available for package")
}

fn compute_package_id(scope: &str, name: &str, version: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(format!("{scope}/{name}@{version}").as_bytes());
    format!("{:x}", hasher.finalize())
}
```

- [ ] **Step 3: Add Install command to CLI**

In `src/main.rs`, add to the Command enum:

```rust
    /// Install packages from vsync.toml / wally.toml.
    #[command(display_order = 44)]
    Install,

    /// Add a dependency.
    #[command(display_order = 45)]
    Add {
        /// Package spec, e.g. "roblox/roact@^17.0.0"
        package: String,
    },

    /// Remove a dependency.
    #[command(display_order = 46)]
    Remove {
        /// Package name, e.g. "roact"
        package: String,
    },

    /// Update dependencies.
    #[command(display_order = 47)]
    Update {
        /// Specific package to update (default: all).
        package: Option<String>,
    },
```

- [ ] **Step 4: Add the Install command handler**

```rust
Command::Install => {
    let ctx = resolve_project_context(&cli)?;
    let config = ctx.vsync_config.clone()
        .or_else(|| vertigo_sync::config::load_config_with_fallback(&ctx.project_root).ok())
        .unwrap_or_default();

    let rt = tokio::runtime::Runtime::new()?;
    let report = rt.block_on(vertigo_sync::package::installer::install(&ctx.project_root, &config))?;

    output::success(&format!(
        "Installed {} package(s) ({} from cache, {} total)",
        report.installed, report.cached, report.total
    ));
}
```

- [ ] **Step 5: Export installer from package/mod.rs**

```rust
pub mod installer;
```

- [ ] **Step 6: Verify it compiles**

Run: `cargo build`
Expected: Compiles. The install command exists but is a skeleton — actual registry calls need network.

- [ ] **Step 7: Commit**

```bash
git add src/package/installer.rs src/main.rs src/errors.rs src/package/mod.rs
git commit -m "feat(package): add vsync install command skeleton with registry client"
```

---

## Phase 5: Enhanced Init + Migrate

### Task 13: Expand `vsync init` with batteries-included scaffold

**Files:**
- Modify: `src/main.rs:1771` (command_init function)
- Test: `tests/init_test.rs`

- [ ] **Step 1: Write test for expanded init**

```rust
// tests/init_test.rs
use std::path::Path;
use tempfile::TempDir;

#[test]
fn init_creates_vsync_toml() {
    let dir = TempDir::new().unwrap();
    vertigo_sync::init::run_init(dir.path(), Some("test-game")).unwrap();
    let vsync_toml = dir.path().join("vsync.toml");
    assert!(vsync_toml.exists(), "vsync.toml should be created");
    let content = std::fs::read_to_string(&vsync_toml).unwrap();
    assert!(content.contains("[package]"));
    assert!(content.contains("test-game"));
    assert!(content.contains("[lint]"));
    assert!(content.contains("[format]"));
}

#[test]
fn init_creates_gitignore() {
    let dir = TempDir::new().unwrap();
    vertigo_sync::init::run_init(dir.path(), Some("test-game")).unwrap();
    let gitignore = dir.path().join(".gitignore");
    assert!(gitignore.exists());
    let content = std::fs::read_to_string(&gitignore).unwrap();
    assert!(content.contains("Packages/"));
    assert!(content.contains("*.rbxl"));
    assert!(content.contains(".vertigo-sync-state/"));
}

#[test]
fn init_creates_vscode_settings() {
    let dir = TempDir::new().unwrap();
    vertigo_sync::init::run_init(dir.path(), Some("test-game")).unwrap();
    let settings = dir.path().join(".vscode").join("settings.json");
    assert!(settings.exists());
    let content = std::fs::read_to_string(&settings).unwrap();
    assert!(content.contains("luau-lsp"));
}

#[test]
fn init_creates_ci_workflow() {
    let dir = TempDir::new().unwrap();
    vertigo_sync::init::run_init(dir.path(), Some("test-game")).unwrap();
    let ci = dir.path().join(".github").join("workflows").join("ci.yml");
    assert!(ci.exists());
    let content = std::fs::read_to_string(&ci).unwrap();
    assert!(content.contains("vsync validate"));
    assert!(content.contains("vsync fmt --check"));
}

#[test]
fn init_creates_tests_directory() {
    let dir = TempDir::new().unwrap();
    vertigo_sync::init::run_init(dir.path(), Some("test-game")).unwrap();
    let test_file = dir.path().join("tests").join("init.luau");
    assert!(test_file.exists());
}

#[test]
fn init_skips_existing_files() {
    let dir = TempDir::new().unwrap();
    // Create a pre-existing vsync.toml
    std::fs::write(dir.path().join("vsync.toml"), "# existing").unwrap();
    vertigo_sync::init::run_init(dir.path(), Some("test-game")).unwrap();
    // Should NOT overwrite
    let content = std::fs::read_to_string(dir.path().join("vsync.toml")).unwrap();
    assert_eq!(content, "# existing");
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test --test init_test`
Expected: FAIL — `init` module doesn't exist

- [ ] **Step 3: Create src/init.rs with expanded scaffolding**

```rust
// src/init.rs
//! Project scaffolding for `vsync init`.

use anyhow::{Context, Result};
use std::path::Path;

/// Run the full init scaffold. Returns Ok(()) on success.
pub fn run_init(root: &Path, name: Option<&str>) -> Result<()> {
    let project_name = name
        .map(|n| n.to_string())
        .or_else(|| {
            root.file_name()
                .and_then(|n| n.to_str())
                .map(|n| n.to_string())
        })
        .unwrap_or_else(|| "my-project".to_string());

    // default.project.json (existing logic)
    write_if_missing(root, "default.project.json", &project_json(&project_name))?;

    // vsync.toml
    write_if_missing(root, "vsync.toml", &vsync_toml(&project_name))?;

    // .gitignore
    write_if_missing(root, ".gitignore", GITIGNORE)?;

    // Source files
    write_if_missing_with_dirs(root, "src/Server/init.server.luau",
        "--!strict\nprint(\"[Server] Hello from Vertigo Sync!\")\n")?;
    write_if_missing_with_dirs(root, "src/Client/init.client.luau",
        "--!strict\nprint(\"[Client] Hello from Vertigo Sync!\")\n")?;
    write_if_missing_with_dirs(root, "src/Shared/init.luau",
        "--!strict\nreturn {}\n")?;

    // Tests
    write_if_missing_with_dirs(root, "tests/init.luau",
        "--!strict\n-- Add tests here\nreturn {}\n")?;

    // .vscode/settings.json
    write_if_missing_with_dirs(root, ".vscode/settings.json", VSCODE_SETTINGS)?;

    // .github/workflows/ci.yml
    write_if_missing_with_dirs(root, ".github/workflows/ci.yml", &ci_yml())?;

    // README.md
    write_if_missing(root, "README.md", &readme(&project_name))?;

    Ok(())
}

fn write_if_missing(root: &Path, rel_path: &str, content: &str) -> Result<()> {
    let path = root.join(rel_path);
    if path.exists() {
        return Ok(());
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&path, content)
        .with_context(|| format!("failed to write {}", path.display()))
}

fn write_if_missing_with_dirs(root: &Path, rel_path: &str, content: &str) -> Result<()> {
    let path = root.join(rel_path);
    if path.exists() {
        return Ok(());
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&path, content)
        .with_context(|| format!("failed to write {}", path.display()))
}

fn project_json(name: &str) -> String {
    let id = uuid::Uuid::new_v4().to_string();
    serde_json::to_string_pretty(&serde_json::json!({
        "name": name,
        "projectId": id,
        "tree": {
            "$className": "DataModel",
            "ServerScriptService": {
                "Server": { "$path": "src/Server" }
            },
            "StarterPlayer": {
                "StarterPlayerScripts": {
                    "Client": { "$path": "src/Client" }
                }
            },
            "ReplicatedStorage": {
                "Shared": { "$path": "src/Shared" }
            }
        }
    })).unwrap()
}

fn vsync_toml(name: &str) -> String {
    format!(r#"[package]
name = "studio-player/{name}"
version = "0.1.0"
realm = "shared"

[dependencies]

[server-dependencies]

[dev-dependencies]

[lint]
unused-variable = "warn"
deprecated-api = "error"
global-shadow = "error"
strict-mode = "warn"
wait-deprecated = "warn"

[format]
indent-type = "tabs"
indent-width = 4
line-width = 120
quote-style = "double"
"#)
}

const GITIGNORE: &str = r#"# Dependencies
Packages/

# Build output
*.rbxl
*.rbxlx

# vsync state
.vertigo-sync-state/
sourcemap.json
"#;

const VSCODE_SETTINGS: &str = r#"{
  "luau-lsp.sourcemap.enabled": true,
  "luau-lsp.sourcemap.rojoProjectFile": "default.project.json"
}
"#;

fn ci_yml() -> String {
    r#"name: CI
on: [push, pull_request]
jobs:
  check:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - name: Install vsync
        run: cargo install vertigo-sync
      - run: vsync install
      - run: vsync validate
      - run: vsync fmt --check
      - run: vsync build -o test.rbxl
"#.to_string()
}

fn readme(name: &str) -> String {
    format!(r#"# {name}

Built with [Vertigo Sync](https://github.com/vertigo-sync/vertigo-sync).

## Getting Started

```bash
vsync serve --turbo
```
"#)
}
```

- [ ] **Step 4: Export from lib.rs**

```rust
pub mod init;
```

- [ ] **Step 5: Update command_init in main.rs to call the new module**

Replace the body of `command_init` with:

```rust
fn command_init(root: &Path, name: Option<&str>) -> Result<()> {
    let project_name = name
        .map(|n| n.to_string())
        .or_else(|| root.file_name().and_then(|n| n.to_str()).map(|n| n.to_string()))
        .unwrap_or_else(|| "my-project".to_string());

    output::header(&format!("Initializing project: {project_name}"));
    eprintln!();

    vertigo_sync::init::run_init(root, Some(&project_name))?;

    eprintln!();
    output::success(&format!("Project '{project_name}' initialized"));
    eprintln!();
    output::info("Next steps:");
    output::info("  vsync serve --turbo             Start syncing");
    output::info("  vsync install                   Install packages");
    output::info("  vsync validate                  Lint your code");
    output::info("  vsync fmt                       Format your code");
    output::info("  vsync doctor                    Verify project health");

    Ok(())
}
```

- [ ] **Step 6: Run tests to verify they pass**

Run: `cargo test --test init_test`
Expected: PASS (all 6 tests)

- [ ] **Step 7: Commit**

```bash
git add src/init.rs src/lib.rs src/main.rs tests/init_test.rs
git commit -m "feat(init): expand vsync init with vsync.toml, gitignore, vscode, ci, tests scaffold"
```

---

### Task 14: `vsync migrate` command

**Files:**
- Modify: `src/migrate.rs` (add selene.toml and stylua.toml readers + full migrate flow)
- Modify: `src/main.rs` (add Migrate command)
- Test: `tests/migrate_test.rs`

- [ ] **Step 1: Write test for full migration**

```rust
// tests/migrate_test.rs
use std::fs;
use tempfile::TempDir;
use vertigo_sync::migrate::run_migrate;

#[test]
fn migrate_converts_wally_toml() {
    let dir = TempDir::new().unwrap();
    fs::write(dir.path().join("wally.toml"), r#"
[package]
name = "studio/game"
version = "0.1.0"
realm = "shared"

[dependencies]
Roact = "roblox/roact@17.0.1"
"#).unwrap();

    let report = run_migrate(dir.path()).unwrap();
    assert!(report.wally_migrated);

    let vsync_toml = fs::read_to_string(dir.path().join("vsync.toml")).unwrap();
    assert!(vsync_toml.contains("studio/game"));
    assert!(vsync_toml.contains("[dependencies]"));
}

#[test]
fn migrate_converts_selene_toml() {
    let dir = TempDir::new().unwrap();
    fs::write(dir.path().join("selene.toml"), r#"
std = "roblox"
"#).unwrap();

    let report = run_migrate(dir.path()).unwrap();
    assert!(report.selene_migrated);
}

#[test]
fn migrate_converts_stylua_toml() {
    let dir = TempDir::new().unwrap();
    fs::write(dir.path().join("stylua.toml"), r#"
indent_type = "Tabs"
indent_width = 4
column_width = 120
quote_style = "AutoPreferDouble"
"#).unwrap();

    let report = run_migrate(dir.path()).unwrap();
    assert!(report.stylua_migrated);

    let vsync_toml = fs::read_to_string(dir.path().join("vsync.toml")).unwrap();
    assert!(vsync_toml.contains("[format]"));
    assert!(vsync_toml.contains("tabs"));
}

#[test]
fn migrate_skips_if_vsync_toml_exists() {
    let dir = TempDir::new().unwrap();
    fs::write(dir.path().join("vsync.toml"), "# existing config").unwrap();
    fs::write(dir.path().join("wally.toml"), r#"
[package]
name = "test/pkg"
version = "0.1.0"
realm = "shared"
"#).unwrap();

    let report = run_migrate(dir.path()).unwrap();
    assert!(!report.wally_migrated, "should not overwrite existing vsync.toml");
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test --test migrate_test`
Expected: FAIL — `run_migrate` doesn't exist

- [ ] **Step 3: Implement full migration in src/migrate.rs**

Add to the existing `src/migrate.rs`:

```rust
use crate::config::{FormatConfig, VsyncConfig};
use std::collections::BTreeMap;

pub struct MigrateReport {
    pub wally_migrated: bool,
    pub selene_migrated: bool,
    pub stylua_migrated: bool,
    pub aftman_found: bool,
}

/// Run the full migration: wally.toml + selene.toml + stylua.toml → vsync.toml.
pub fn run_migrate(root: &Path) -> Result<MigrateReport> {
    let vsync_path = root.join("vsync.toml");
    if vsync_path.exists() {
        return Ok(MigrateReport {
            wally_migrated: false,
            selene_migrated: false,
            stylua_migrated: false,
            aftman_found: false,
        });
    }

    let mut config = VsyncConfig::default();
    let mut report = MigrateReport {
        wally_migrated: false,
        selene_migrated: false,
        stylua_migrated: false,
        aftman_found: false,
    };

    // Wally
    let wally_path = root.join("wally.toml");
    if wally_path.exists() {
        let wally_config = parse_wally_toml(&wally_path)?;
        config.package = wally_config.package;
        config.dependencies = wally_config.dependencies;
        config.server_dependencies = wally_config.server_dependencies;
        config.dev_dependencies = wally_config.dev_dependencies;
        report.wally_migrated = true;
    }

    // Selene
    let selene_path = root.join("selene.toml");
    if selene_path.exists() {
        config.lint = parse_selene_to_lint_config(&selene_path)?;
        report.selene_migrated = true;
    }

    // StyLua
    let stylua_path = root.join("stylua.toml");
    if stylua_path.exists() {
        config.format = parse_stylua_to_format_config(&stylua_path)?;
        report.stylua_migrated = true;
    }

    // Aftman/Foreman detection
    if root.join("aftman.toml").exists() || root.join("foreman.toml").exists() {
        report.aftman_found = true;
    }

    // Apply defaults for lint if not set by selene
    if config.lint.is_empty() {
        config.lint.insert("unused-variable".to_string(), "warn".to_string());
        config.lint.insert("deprecated-api".to_string(), "error".to_string());
        config.lint.insert("global-shadow".to_string(), "error".to_string());
        config.lint.insert("strict-mode".to_string(), "warn".to_string());
    }

    // Apply defaults for format if not set by stylua
    if config.format == FormatConfig::default() {
        config.format = FormatConfig {
            indent_type: Some("tabs".to_string()),
            indent_width: Some(4),
            line_width: Some(120),
            quote_style: Some("double".to_string()),
            ..Default::default()
        };
    }

    // Serialize and write
    let toml_str = toml::to_string_pretty(&config)
        .context("failed to serialize vsync.toml")?;
    std::fs::write(&vsync_path, toml_str)
        .with_context(|| format!("failed to write {}", vsync_path.display()))?;

    Ok(report)
}

fn parse_selene_to_lint_config(path: &Path) -> Result<BTreeMap<String, String>> {
    // Selene's config is mostly about setting the standard library (roblox vs lua51).
    // Individual rule config is in selene.toml as [rules.<name>] sections.
    // For migration, we map to vsync defaults since selene rules != vsync rules.
    let mut lint = BTreeMap::new();
    lint.insert("unused-variable".to_string(), "warn".to_string());
    lint.insert("deprecated-api".to_string(), "error".to_string());
    lint.insert("global-shadow".to_string(), "error".to_string());
    lint.insert("strict-mode".to_string(), "warn".to_string());
    Ok(lint)
}

fn parse_stylua_to_format_config(path: &Path) -> Result<FormatConfig> {
    #[derive(Deserialize, Default)]
    struct StyluaToml {
        #[serde(default)]
        indent_type: Option<String>,
        #[serde(default)]
        indent_width: Option<u32>,
        #[serde(default)]
        column_width: Option<u32>,
        #[serde(default)]
        quote_style: Option<String>,
        #[serde(default)]
        call_parentheses: Option<String>,
        #[serde(default)]
        collapse_simple_statement: Option<String>,
    }

    let content = std::fs::read_to_string(path)?;
    let stylua: StyluaToml = toml::from_str(&content)?;

    Ok(FormatConfig {
        indent_type: stylua.indent_type.map(|s| s.to_lowercase().replace("tabs", "tabs").replace("spaces", "spaces")),
        indent_width: stylua.indent_width,
        line_width: stylua.column_width,
        quote_style: stylua.quote_style.map(|s| {
            match s.as_str() {
                "ForceSingle" => "single".to_string(),
                "ForceDouble" => "double".to_string(),
                _ => "double".to_string(),
            }
        }),
        call_parentheses: stylua.call_parentheses.map(|s| s.to_lowercase()),
        collapse_simple_statement: stylua.collapse_simple_statement.map(|s| s.to_lowercase()),
    })
}
```

- [ ] **Step 4: Add Migrate command to CLI**

In `src/main.rs`, add to Command enum:

```rust
    /// Migrate from Rojo ecosystem (wally.toml, selene.toml, stylua.toml) to vsync.toml.
    #[command(display_order = 48)]
    Migrate,
```

And the handler:

```rust
Command::Migrate => {
    output::header("Migrating to vsync.toml");
    let report = vertigo_sync::migrate::run_migrate(&root)?;

    if report.wally_migrated {
        output::success("Migrated wally.toml → vsync.toml [package] + [dependencies]");
    }
    if report.selene_migrated {
        output::success("Migrated selene.toml → vsync.toml [lint]");
    }
    if report.stylua_migrated {
        output::success("Migrated stylua.toml → vsync.toml [format]");
    }
    if report.aftman_found {
        output::info("Found aftman.toml/foreman.toml — these are no longer needed with vsync");
    }
    if !report.wally_migrated && !report.selene_migrated && !report.stylua_migrated {
        output::info("Nothing to migrate (vsync.toml already exists or no ecosystem configs found)");
    }

    eprintln!();
    output::info("You can now delete wally.toml, selene.toml, stylua.toml, and aftman.toml/foreman.toml");
}
```

- [ ] **Step 5: Run tests to verify they pass**

Run: `cargo test --test migrate_test`
Expected: PASS (all 4 tests)

- [ ] **Step 6: Commit**

```bash
git add src/migrate.rs src/main.rs tests/migrate_test.rs
git commit -m "feat(migrate): add vsync migrate command for wally/selene/stylua conversion"
```

---

### Task 15: `vsync run` command

**Files:**
- Create: `src/scripts.rs`
- Modify: `src/main.rs` (add Run command)
- Test: `tests/scripts_test.rs`

- [ ] **Step 1: Write test for script resolution**

```rust
// tests/scripts_test.rs
use std::collections::BTreeMap;
use vertigo_sync::scripts::resolve_script;

#[test]
fn resolves_defined_script() {
    let mut scripts = BTreeMap::new();
    scripts.insert("test".to_string(), "echo hello".to_string());
    let result = resolve_script("test", &scripts);
    assert_eq!(result, Some("echo hello".to_string()));
}

#[test]
fn returns_none_for_undefined_script() {
    let scripts = BTreeMap::new();
    let result = resolve_script("missing", &scripts);
    assert!(result.is_none());
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test --test scripts_test`
Expected: FAIL

- [ ] **Step 3: Implement src/scripts.rs**

```rust
// src/scripts.rs
//! Script runner for `vsync run`.

use anyhow::{bail, Context, Result};
use std::collections::BTreeMap;
use std::path::Path;
use std::process::Command;

/// Look up a script by name.
pub fn resolve_script(name: &str, scripts: &BTreeMap<String, String>) -> Option<String> {
    scripts.get(name).cloned()
}

/// Execute a script in a shell with the project root as CWD.
pub fn run_script(name: &str, command: &str, project_root: &Path, project_name: &str) -> Result<i32> {
    let shell = if cfg!(target_os = "windows") { "cmd" } else { "sh" };
    let flag = if cfg!(target_os = "windows") { "/C" } else { "-c" };

    let status = Command::new(shell)
        .arg(flag)
        .arg(command)
        .current_dir(project_root)
        .env("VSYNC_PROJECT_ROOT", project_root.to_string_lossy().as_ref())
        .env("VSYNC_PROJECT_NAME", project_name)
        .status()
        .with_context(|| format!("failed to execute script '{name}'"))?;

    Ok(status.code().unwrap_or(1))
}
```

- [ ] **Step 4: Export from lib.rs and add CLI command**

Add to `src/lib.rs`:
```rust
pub mod scripts;
```

Add to Command enum in `src/main.rs`:
```rust
    /// Run a project script defined in vsync.toml [scripts].
    #[command(display_order = 49)]
    Run {
        /// Script name (defined in vsync.toml [scripts]).
        name: String,
    },
```

Handler:
```rust
Command::Run { name } => {
    let ctx = resolve_project_context(&cli)?;
    let config = ctx.vsync_config.unwrap_or_default();
    match vertigo_sync::scripts::resolve_script(&name, &config.scripts) {
        Some(command) => {
            output::info(&format!("Running script '{name}': {command}"));
            let exit_code = vertigo_sync::scripts::run_script(
                &name, &command, &ctx.project_root, &config.package.name
            )?;
            if exit_code != 0 {
                bail!("script '{name}' exited with code {exit_code}");
            }
        }
        None => {
            let available: Vec<&str> = config.scripts.keys().map(|k| k.as_str()).collect();
            if available.is_empty() {
                bail!("no scripts defined in vsync.toml [scripts]");
            } else {
                bail!("unknown script '{name}'. Available: {}", available.join(", "));
            }
        }
    }
}
```

- [ ] **Step 5: Run tests to verify they pass**

Run: `cargo test --test scripts_test`
Expected: PASS

- [ ] **Step 6: Commit**

```bash
git add src/scripts.rs src/lib.rs src/main.rs tests/scripts_test.rs
git commit -m "feat(scripts): add vsync run command for project scripts"
```

---

## Summary

| Task | Phase | What it builds | Depends on |
|------|-------|---------------|------------|
| 0 | Prereq | Rename `toml_crate` → `toml` in Cargo.toml | — |
| 1 | Config | `VsyncConfig` types + parser | 0 |
| 2 | Config | CLI integration (add `vsync_config` to `ProjectContext`) | 1 |
| 3 | Config | Wally.toml fallback reader | 1 |
| 4 | Format | StyLua dependency | 0 |
| 5 | Format | `format_source` + `check_source` | 1, 4 |
| 6 | Format | `vsync fmt` CLI command | 2, 5 |
| 7 | Lint | P0 correctness rules (new rules module) | 1 |
| 8 | Lint | Wire new rules into `vsync validate` + Selene deprecation notice | 2, 7 |
| 8b | Lint | Wrap existing `validate.rs` rules as configurable `[lint]` entries | 8 |
| 9 | Package | reqwest/pubgrub dependencies | 0 |
| 10 | Package | Lockfile + cache types | 1, 9 |
| 11 | Package | Registry client | 9 |
| 12 | Package | `vsync install` command (sequential skeleton) | 2, 3, 10, 11 |
| 12b | Package | Parallel downloads with bounded concurrency | 12 |
| 13 | Init | Expanded scaffold with vsync.toml, gitignore, CI, vscode | 1 |
| 14 | Migrate | `vsync migrate` command (wally + selene + stylua) | 3 |
| 15 | Scripts | `vsync run` command (v1.1 scope, included for simplicity) | 2 |

**Dependency graph:**
```
0 (toml rename)
├── 1 (config types)
│   ├── 2 (CLI wiring)
│   │   ├── 6 (vsync fmt CLI) ← also needs 5
│   │   ├── 8 (lint → validate) ← also needs 7
│   │   │   └── 8b (wrap existing rules)
│   │   ├── 12 (vsync install) ← also needs 3, 10, 11
│   │   │   └── 12b (parallel downloads)
│   │   └── 15 (vsync run)
│   ├── 3 (wally fallback)
│   │   └── 14 (vsync migrate)
│   ├── 5 (fmt core) ← also needs 4
│   ├── 7 (lint rules)
│   ├── 10 (lockfile) ← also needs 9
│   └── 13 (init scaffold)
├── 4 (stylua dep)
└── 9 (reqwest/pubgrub dep)
```

**Parallelism:** After Tasks 0-3, the following chains can run concurrently:
- Chain A: 4 → 5 → 6
- Chain B: 7 → 8 → 8b
- Chain C: 9 → 10 → 11 → 12 → 12b
- Chain D: 13
- Chain E: 14
- Chain F: 15
