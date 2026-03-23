//! Unified toolchain configuration parser for `vsync.toml`.
//!
//! Reads and deserializes the project-level `vsync.toml` file that drives
//! package metadata, dependency resolution, linting, formatting, scripts,
//! and workspace configuration.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::Path;

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

/// Top-level `vsync.toml` configuration.
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

/// Package metadata section.
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

    #[serde(
        skip_serializing_if = "Option::is_none",
        default,
        rename = "packages-dir"
    )]
    pub packages_dir: Option<String>,
}

/// A dependency specification — supports multiple source kinds.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(untagged)]
pub enum DependencySpec {
    /// Short-hand string, e.g. `"roblox/roact@^17.0.0"`.
    Simple(String),

    /// Git dependency with optional pinning.
    Git {
        git: String,
        #[serde(skip_serializing_if = "Option::is_none", default)]
        rev: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none", default)]
        branch: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none", default)]
        tag: Option<String>,
    },

    /// Local filesystem path dependency.
    Path { path: String },

    /// Named dependency from a specific registry.
    Registry { registry: String, name: String },
}

/// Formatter configuration section.
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
pub struct FormatConfig {
    #[serde(
        skip_serializing_if = "Option::is_none",
        default,
        rename = "indent-type"
    )]
    pub indent_type: Option<String>,

    #[serde(
        skip_serializing_if = "Option::is_none",
        default,
        rename = "indent-width"
    )]
    pub indent_width: Option<u32>,

    #[serde(
        skip_serializing_if = "Option::is_none",
        default,
        rename = "line-width"
    )]
    pub line_width: Option<u32>,

    #[serde(
        skip_serializing_if = "Option::is_none",
        default,
        rename = "quote-style"
    )]
    pub quote_style: Option<String>,

    #[serde(
        skip_serializing_if = "Option::is_none",
        default,
        rename = "call-parentheses"
    )]
    pub call_parentheses: Option<String>,

    #[serde(
        skip_serializing_if = "Option::is_none",
        default,
        rename = "collapse-simple-statement"
    )]
    pub collapse_simple_statement: Option<String>,
}

/// Workspace configuration section.
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
pub struct WorkspaceConfig {
    #[serde(default)]
    pub members: Vec<String>,
}

// ---------------------------------------------------------------------------
// Loader
// ---------------------------------------------------------------------------

/// Reads and parses `vsync.toml` from the given project root directory.
///
/// Returns `Ok(None)` if the file does not exist, `Ok(Some(config))` on
/// success, or an error if the file exists but cannot be read or parsed.
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

/// Serialize and write a `VsyncConfig` back to `vsync.toml` in the given project root.
pub fn save_config(project_root: &Path, config: &VsyncConfig) -> Result<()> {
    let config_path = project_root.join("vsync.toml");
    let content = toml::to_string_pretty(config).context("failed to serialize vsync.toml")?;
    std::fs::write(&config_path, content)
        .with_context(|| format!("failed to write {}", config_path.display()))?;
    Ok(())
}

/// Load vsync.toml, falling back to wally.toml, falling back to defaults.
/// For now, only vsync.toml is supported; wally.toml fallback will be added in Task 3.
pub fn load_config_with_fallback(project_root: &Path) -> Result<VsyncConfig> {
    if let Some(config) = load_config(project_root)? {
        return Ok(config);
    }
    let wally_path = project_root.join("wally.toml");
    if wally_path.exists() {
        return crate::migrate::parse_wally_toml(&wally_path);
    }
    Ok(VsyncConfig::default())
}
