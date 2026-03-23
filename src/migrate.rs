//! Migration helpers for converting Rojo ecosystem configs to `vsync.toml`.
//!
//! Parses `wally.toml`, `selene.toml`, and `stylua.toml` manifests and merges
//! them into a single [`VsyncConfig`] so that projects can be migrated without
//! manual config rewriting.

use anyhow::{Context, Result};
use serde::Deserialize;
use std::collections::BTreeMap;
use std::path::Path;

use crate::config::{DependencySpec, FormatConfig, PackageConfig, VsyncConfig};

// ---------------------------------------------------------------------------
// Public report type
// ---------------------------------------------------------------------------

/// Summary of what was migrated by [`run_migrate`].
pub struct MigrateReport {
    pub wally_migrated: bool,
    pub selene_migrated: bool,
    pub stylua_migrated: bool,
    pub aftman_found: bool,
    pub dep_count: usize,
    pub server_dep_count: usize,
    pub dev_dep_count: usize,
}

// ---------------------------------------------------------------------------
// Internal Wally types (Deserialize only)
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
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

// ---------------------------------------------------------------------------
// Internal StyLua types (Deserialize only)
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize, Default)]
struct StyluaManifest {
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

// ---------------------------------------------------------------------------
// Internal Selene types (Deserialize only)
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize, Default)]
struct SeleneManifest {
    #[serde(default)]
    std: Option<String>,

    #[serde(default)]
    #[allow(dead_code)]
    lints: BTreeMap<String, toml::Value>,
}

// ---------------------------------------------------------------------------
// Conversion helpers
// ---------------------------------------------------------------------------

fn convert_deps(wally_deps: BTreeMap<String, String>) -> BTreeMap<String, DependencySpec> {
    wally_deps
        .into_iter()
        .map(|(key, value)| (key, DependencySpec::Simple(value)))
        .collect()
}

/// Parse a `wally.toml` file and convert it into a [`VsyncConfig`].
pub fn parse_wally_toml(path: &Path) -> Result<VsyncConfig> {
    let content = std::fs::read_to_string(path)
        .with_context(|| format!("failed to read {}", path.display()))?;

    let manifest: WallyManifest =
        toml::from_str(&content).with_context(|| format!("failed to parse {}", path.display()))?;

    Ok(VsyncConfig {
        package: PackageConfig {
            name: manifest.package.name,
            version: manifest.package.version,
            realm: manifest.package.realm,
            description: manifest.package.description,
            license: manifest.package.license,
            authors: manifest.package.authors,
            ..PackageConfig::default()
        },
        dependencies: convert_deps(manifest.dependencies),
        server_dependencies: convert_deps(manifest.server_dependencies),
        dev_dependencies: convert_deps(manifest.dev_dependencies),
        ..VsyncConfig::default()
    })
}

/// Parse `stylua.toml` and map it to a [`FormatConfig`].
fn parse_stylua_to_format_config(path: &Path) -> Result<FormatConfig> {
    let content = std::fs::read_to_string(path)
        .with_context(|| format!("failed to read {}", path.display()))?;

    let manifest: StyluaManifest =
        toml::from_str(&content).with_context(|| format!("failed to parse {}", path.display()))?;

    let indent_type = manifest.indent_type.map(|v| match v.as_str() {
        "Tabs" => "tabs".to_string(),
        "Spaces" => "spaces".to_string(),
        other => other.to_lowercase(),
    });

    let quote_style = manifest.quote_style.map(|v| match v.as_str() {
        "ForceSingle" => "single".to_string(),
        "ForceDouble" => "double".to_string(),
        other => other.to_lowercase(),
    });

    Ok(FormatConfig {
        indent_type,
        indent_width: manifest.indent_width,
        line_width: manifest.column_width,
        quote_style,
        call_parentheses: manifest.call_parentheses,
        collapse_simple_statement: manifest.collapse_simple_statement,
    })
}

/// Parse `selene.toml` and extract lint-related configuration.
///
/// Since selene rules don't map 1:1, this returns sensible defaults with
/// the selene `std` carried over as a hint.
fn parse_selene_to_lint_config(path: &Path) -> Result<BTreeMap<String, String>> {
    let content = std::fs::read_to_string(path)
        .with_context(|| format!("failed to read {}", path.display()))?;

    let manifest: SeleneManifest =
        toml::from_str(&content).with_context(|| format!("failed to parse {}", path.display()))?;

    let mut lint = BTreeMap::new();

    // Carry over the std hint (e.g. "roblox", "luau", "lua51").
    if let Some(std) = manifest.std {
        lint.insert("std".to_string(), std);
    }

    // Hardcoded sensible defaults for migrated projects.
    lint.entry("unused-variable".to_string())
        .or_insert_with(|| "warn".to_string());
    lint.entry("global-shadow".to_string())
        .or_insert_with(|| "warn".to_string());
    lint.entry("wait-deprecated".to_string())
        .or_insert_with(|| "warn".to_string());

    Ok(lint)
}

/// Default lint config applied when no selene.toml is found.
fn default_lint_config() -> BTreeMap<String, String> {
    let mut lint = BTreeMap::new();
    lint.insert("unused-variable".to_string(), "warn".to_string());
    lint.insert("global-shadow".to_string(), "warn".to_string());
    lint.insert("wait-deprecated".to_string(), "warn".to_string());
    lint
}

/// Default format config applied when no stylua.toml is found.
fn default_format_config() -> FormatConfig {
    FormatConfig {
        indent_type: Some("tabs".to_string()),
        indent_width: Some(4),
        line_width: Some(120),
        quote_style: Some("double".to_string()),
        call_parentheses: None,
        collapse_simple_statement: None,
    }
}

// ---------------------------------------------------------------------------
// Main migration entry point
// ---------------------------------------------------------------------------

/// Migrate Rojo ecosystem config files (`wally.toml`, `selene.toml`,
/// `stylua.toml`) into a single `vsync.toml`.
///
/// If `vsync.toml` already exists, this is a no-op and all report fields
/// are `false`.
pub fn run_migrate(root: &Path) -> Result<MigrateReport> {
    let vsync_path = root.join("vsync.toml");

    // Don't overwrite an existing vsync.toml.
    if vsync_path.exists() {
        return Ok(MigrateReport {
            wally_migrated: false,
            selene_migrated: false,
            stylua_migrated: false,
            aftman_found: false,
            dep_count: 0,
            server_dep_count: 0,
            dev_dep_count: 0,
        });
    }

    let mut config = VsyncConfig::default();
    let mut report = MigrateReport {
        wally_migrated: false,
        selene_migrated: false,
        stylua_migrated: false,
        aftman_found: false,
        dep_count: 0,
        server_dep_count: 0,
        dev_dep_count: 0,
    };

    // 1. Wally
    let wally_path = root.join("wally.toml");
    if wally_path.exists() {
        let wally_config = parse_wally_toml(&wally_path)?;
        config.package = wally_config.package;
        report.dep_count = wally_config.dependencies.len();
        report.server_dep_count = wally_config.server_dependencies.len();
        report.dev_dep_count = wally_config.dev_dependencies.len();
        config.dependencies = wally_config.dependencies;
        config.server_dependencies = wally_config.server_dependencies;
        config.dev_dependencies = wally_config.dev_dependencies;
        report.wally_migrated = true;
    }

    // 2. Selene
    let selene_path = root.join("selene.toml");
    if selene_path.exists() {
        config.lint = parse_selene_to_lint_config(&selene_path)?;
        report.selene_migrated = true;
    } else {
        config.lint = default_lint_config();
    }

    // 3. StyLua
    let stylua_path = root.join("stylua.toml");
    if stylua_path.exists() {
        config.format = parse_stylua_to_format_config(&stylua_path)?;
        report.stylua_migrated = true;
    } else {
        config.format = default_format_config();
    }

    // 4. Aftman / Foreman detection
    if root.join("aftman.toml").exists() || root.join("foreman.toml").exists() {
        report.aftman_found = true;
    }

    // 5. Write vsync.toml (atomic create to avoid TOCTOU race)
    let content = toml::to_string_pretty(&config).context("failed to serialize vsync.toml")?;
    match std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&vsync_path)
    {
        Ok(mut f) => {
            use std::io::Write;
            f.write_all(content.as_bytes())
                .with_context(|| format!("failed to write {}", vsync_path.display()))?;
        }
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            // Another process created vsync.toml between our check and write — no-op.
            return Ok(MigrateReport {
                wally_migrated: false,
                selene_migrated: false,
                stylua_migrated: false,
                aftman_found: false,
                dep_count: 0,
                server_dep_count: 0,
                dev_dep_count: 0,
            });
        }
        Err(e) => {
            return Err(
                anyhow::anyhow!(e).context(format!("failed to create {}", vsync_path.display()))
            );
        }
    }

    Ok(report)
}
