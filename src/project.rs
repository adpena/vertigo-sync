//! Rojo-compatible project.json parser.
//!
//! Parses `default.project.json` and extracts the `tree` structure into a flat
//! list of filesystem-to-DataModel path mappings.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

/// Parsed project tree with resolved filesystem-to-instance mappings.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ProjectTree {
    pub name: String,
    pub project_id: String,
    pub mappings: Vec<PathMapping>,
    /// Glob patterns for paths to exclude from the snapshot (Rojo-compatible).
    #[serde(default)]
    pub glob_ignore_paths: Vec<String>,
    /// When `false`, all scripts emit as `Script` with `RunContext` instead of
    /// `LocalScript` / `Script` based on filename suffix. Defaults to `true`.
    #[serde(default = "default_true")]
    pub emit_legacy_scripts: bool,
    /// Optional serve port from project file (Rojo `servePort` parity).
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub serve_port: Option<u16>,
    /// Optional serve address from project file (Rojo `serveAddress` parity).
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub serve_address: Option<String>,
    /// Optional vertigo-sync project-local configuration.
    #[serde(
        skip_serializing_if = "Option::is_none",
        default,
        rename = "vertigoSync"
    )]
    pub vertigo_sync: Option<VertigoSyncConfig>,
}

fn default_true() -> bool {
    true
}

/// A single filesystem path mapped to a Roblox DataModel instance path.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PathMapping {
    /// Filesystem path relative to project root, e.g. `"src/Server"`.
    pub fs_path: String,
    /// DataModel instance path, e.g. `"ServerScriptService.Server"`.
    pub instance_path: String,
    /// Roblox class name, e.g. `"ServerScriptService"`.
    pub class_name: String,
    /// Whether unknown instances should be ignored during sync.
    pub ignore_unknown: bool,
    /// `$properties` from the project tree node — applied to the instance.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub properties: Option<BTreeMap<String, serde_json::Value>>,
    /// `$attributes` from the project tree node — applied as instance attributes.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub attributes: Option<BTreeMap<String, serde_json::Value>>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct VertigoSyncConfig {
    #[serde(default)]
    pub builders: VertigoSyncBuildersConfig,
    #[serde(
        skip_serializing_if = "Option::is_none",
        default,
        rename = "editPreview"
    )]
    pub edit_preview: Option<VertigoSyncEditPreviewConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct VertigoSyncBuildersConfig {
    #[serde(default)]
    pub roots: Vec<String>,
    #[serde(rename = "dependencyRoots", default)]
    pub dependency_roots: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct VertigoSyncEditPreviewConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(rename = "builderModulePath")]
    pub builder_module_path: String,
    #[serde(
        skip_serializing_if = "Option::is_none",
        default,
        rename = "builderMethod"
    )]
    pub builder_method: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty", default, rename = "watchRoots")]
    pub watch_roots: Vec<String>,
    #[serde(
        skip_serializing_if = "Option::is_none",
        default,
        rename = "debounceSeconds"
    )]
    pub debounce_seconds: Option<f64>,
    #[serde(
        skip_serializing_if = "Option::is_none",
        default,
        rename = "rootRefreshSeconds"
    )]
    pub root_refresh_seconds: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub mode: Option<String>,
}

// ---------------------------------------------------------------------------
// Raw JSON schema (mirrors Rojo project file structure)
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct RawProject {
    name: String,
    #[serde(rename = "projectId", default)]
    project_id: Option<String>,
    tree: RawTreeNode,
    /// Optional JSON Schema URL — ignored but must not create a phantom instance.
    #[serde(rename = "$schema", default)]
    #[allow(dead_code)]
    schema: Option<String>,
    #[serde(rename = "globIgnorePaths", default)]
    glob_ignore_paths: Vec<String>,
    #[serde(rename = "emitLegacyScripts", default = "default_true")]
    emit_legacy_scripts: bool,
    /// Optional serve port from project file (Rojo parity).
    #[serde(rename = "servePort", default)]
    pub serve_port: Option<u16>,
    /// Optional serve address from project file (Rojo parity).
    #[serde(rename = "serveAddress", default)]
    pub serve_address: Option<String>,
    #[serde(rename = "vertigoSync", default)]
    vertigo_sync: Option<RawVertigoSyncConfig>,
}

#[derive(Debug, Deserialize)]
struct RawTreeNode {
    #[serde(rename = "$className", default)]
    class_name: Option<String>,
    #[serde(rename = "$path", default)]
    path: Option<String>,
    #[serde(rename = "$ignoreUnknownInstances", default)]
    ignore_unknown_instances: Option<bool>,
    #[serde(flatten)]
    children: BTreeMap<String, serde_json::Value>,
}

#[derive(Debug, Deserialize)]
struct RawVertigoSyncConfig {
    #[serde(default)]
    builders: RawVertigoSyncBuildersConfig,
    #[serde(rename = "editPreview", default)]
    edit_preview: Option<RawVertigoSyncEditPreviewConfig>,
}

#[derive(Debug, Deserialize, Default)]
struct RawVertigoSyncBuildersConfig {
    #[serde(default)]
    roots: Vec<String>,
    #[serde(rename = "dependencyRoots", default)]
    dependency_roots: Vec<String>,
}

#[derive(Debug, Deserialize, Default)]
struct RawVertigoSyncEditPreviewConfig {
    #[serde(default)]
    enabled: bool,
    #[serde(rename = "builderModulePath", default)]
    builder_module_path: Option<String>,
    #[serde(rename = "builderMethod", default)]
    builder_method: Option<String>,
    #[serde(rename = "watchRoots", default)]
    watch_roots: Vec<String>,
    #[serde(rename = "debounceSeconds", default)]
    debounce_seconds: Option<f64>,
    #[serde(rename = "rootRefreshSeconds", default)]
    root_refresh_seconds: Option<f64>,
    #[serde(default)]
    mode: Option<String>,
}

// ---------------------------------------------------------------------------
// Parsing
// ---------------------------------------------------------------------------

/// Parse a Rojo-compatible project file and return a `ProjectTree`.
pub fn parse_project(path: &Path) -> Result<ProjectTree> {
    let content = std::fs::read_to_string(path)
        .with_context(|| format!("failed to read project file {}", path.display()))?;
    parse_project_str(&content, path)
}

/// Parse project JSON from a string (testable without filesystem).
fn parse_project_str(content: &str, source_path: &Path) -> Result<ProjectTree> {
    let raw: RawProject = serde_json::from_str(content)
        .with_context(|| format!("failed to parse project json {}", source_path.display()))?;

    let mut mappings = Vec::new();
    let root_class = raw
        .tree
        .class_name
        .as_deref()
        .unwrap_or("DataModel")
        .to_string();
    let root_ignore = raw.tree.ignore_unknown_instances.unwrap_or(false);

    // If the root tree node itself has a $path, record it.
    if let Some(ref fs_path) = raw.tree.path {
        mappings.push(PathMapping {
            fs_path: normalize_fs_path(fs_path),
            instance_path: raw.name.clone(),
            class_name: root_class.clone(),
            ignore_unknown: root_ignore,
            properties: None,
            attributes: None,
        });
    }

    // Walk children.
    for (child_name, child_value) in &raw.tree.children {
        walk_tree_node(
            child_name,
            child_value,
            &child_name.clone(),
            root_ignore,
            &mut mappings,
        );
    }

    let project_id = resolve_project_id(&raw, source_path, &mappings);

    Ok(ProjectTree {
        name: raw.name,
        project_id,
        mappings,
        glob_ignore_paths: raw.glob_ignore_paths,
        emit_legacy_scripts: raw.emit_legacy_scripts,
        serve_port: raw.serve_port,
        serve_address: raw.serve_address,
        vertigo_sync: raw.vertigo_sync.map(|config| VertigoSyncConfig {
            builders: VertigoSyncBuildersConfig {
                roots: config
                    .builders
                    .roots
                    .into_iter()
                    .map(|path| normalize_fs_path(&path))
                    .collect(),
                dependency_roots: config
                    .builders
                    .dependency_roots
                    .into_iter()
                    .map(|path| normalize_fs_path(&path))
                    .collect(),
            },
            edit_preview: config.edit_preview.and_then(|edit_preview| {
                match edit_preview.builder_module_path {
                    Some(builder_module_path) if !builder_module_path.trim().is_empty() => {
                        Some(VertigoSyncEditPreviewConfig {
                            enabled: edit_preview.enabled,
                            builder_module_path: builder_module_path.trim().to_string(),
                            builder_method: edit_preview
                                .builder_method
                                .map(|value| value.trim().to_string())
                                .filter(|value| !value.is_empty()),
                            watch_roots: edit_preview
                                .watch_roots
                                .into_iter()
                                .map(|path| path.trim().replace('\\', "/"))
                                .filter(|path| !path.is_empty())
                                .collect(),
                            debounce_seconds: edit_preview.debounce_seconds,
                            root_refresh_seconds: edit_preview.root_refresh_seconds,
                            mode: edit_preview
                                .mode
                                .map(|value| value.trim().to_string())
                                .filter(|value| !value.is_empty()),
                        })
                    }
                    _ => None,
                }
            }),
        }),
    })
}

fn resolve_project_id(raw: &RawProject, source_path: &Path, mappings: &[PathMapping]) -> String {
    let explicit = raw
        .project_id
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty());
    if let Some(project_id) = explicit {
        return project_id.to_string();
    }

    let canonical_source = source_path
        .canonicalize()
        .unwrap_or_else(|_| source_path.to_path_buf());
    let mut hasher = Sha256::new();
    hasher.update(b"vertigo-sync-project-id-v1\0");
    hasher.update(canonical_source.to_string_lossy().as_bytes());
    hasher.update(b"\0");
    hasher.update(raw.name.as_bytes());
    hasher.update(b"\0");
    hasher.update(if raw.emit_legacy_scripts { b"1" } else { b"0" });
    hasher.update(b"\0");
    hasher.update(raw.serve_port.unwrap_or_default().to_string().as_bytes());
    hasher.update(b"\0");
    if let Some(address) = &raw.serve_address {
        hasher.update(address.as_bytes());
    }
    hasher.update(b"\0");
    for mapping in mappings {
        hasher.update(mapping.fs_path.as_bytes());
        hasher.update(b"\0");
        hasher.update(mapping.instance_path.as_bytes());
        hasher.update(b"\0");
        hasher.update(mapping.class_name.as_bytes());
        hasher.update(b"\0");
        hasher.update(if mapping.ignore_unknown { b"1" } else { b"0" });
        hasher.update(b"\0");
    }
    format!("{:x}", hasher.finalize())
}

fn walk_tree_node(
    node_name: &str,
    value: &serde_json::Value,
    instance_path: &str,
    parent_ignore: bool,
    mappings: &mut Vec<PathMapping>,
) {
    let Some(obj) = value.as_object() else {
        return;
    };

    let class_name = obj
        .get("$className")
        .and_then(|v| v.as_str())
        .unwrap_or(node_name)
        .to_string();

    let ignore_unknown = obj
        .get("$ignoreUnknownInstances")
        .and_then(|v| v.as_bool())
        .unwrap_or(parent_ignore);

    // Extract $properties if present.
    let properties = obj
        .get("$properties")
        .and_then(|v| v.as_object())
        .map(|map| map.iter().map(|(k, v)| (k.clone(), v.clone())).collect());

    // Extract $attributes if present.
    let attributes = obj
        .get("$attributes")
        .and_then(|v| v.as_object())
        .map(|map| map.iter().map(|(k, v)| (k.clone(), v.clone())).collect());

    if let Some(fs_path) = obj.get("$path").and_then(|v| v.as_str()) {
        mappings.push(PathMapping {
            fs_path: normalize_fs_path(fs_path),
            instance_path: instance_path.to_string(),
            class_name: class_name.clone(),
            ignore_unknown,
            properties,
            attributes,
        });
    }

    // Recurse into children (skip $-prefixed keys).
    for (key, child_value) in obj {
        if key.starts_with('$') {
            continue;
        }
        let child_instance_path = format!("{instance_path}.{key}");
        walk_tree_node(
            key,
            child_value,
            &child_instance_path,
            ignore_unknown,
            mappings,
        );
    }
}

// ---------------------------------------------------------------------------
// Instance class resolution
// ---------------------------------------------------------------------------

/// Given a filesystem path, determine the Roblox instance class.
///
/// Extended to handle non-Luau file types:
/// - `.json` -> "ModuleScript" (source is JSON content as a string)
/// - `.txt` -> "StringValue"
/// - `.csv` -> "LocalizationTable"
/// - `.rbxm` / `.rbxmx` -> "Model" (marker — actual instances come from manifest)
/// - `.meta.json` -> skip (sidecar, not a standalone entry)
pub fn resolve_instance_class(file_path: &str) -> &'static str {
    let normalized = file_path.replace('\\', "/");
    let file_name = normalized.rsplit('/').next().unwrap_or(&normalized);

    // Skip .meta.json sidecars — they are not standalone instances.
    if file_name.ends_with(".meta.json") {
        return "Skip";
    }

    // .model.json files define instance trees — treat as ModuleScript for
    // classification purposes (actual class comes from the JSON content).
    if file_name.ends_with(".model.json") {
        return "ModuleScript";
    }

    match file_name {
        "init.server.luau" | "init.server.lua" => "Script",
        "init.client.luau" | "init.client.lua" => "LocalScript",
        _ => {
            if file_name.ends_with(".server.luau") || file_name.ends_with(".server.lua") {
                "Script"
            } else if file_name.ends_with(".client.luau") || file_name.ends_with(".client.lua") {
                "LocalScript"
            } else if file_name.ends_with(".luau")
                || file_name.ends_with(".lua")
                || file_name.ends_with(".json")
                || file_name.ends_with(".jsonc")
                || file_name.ends_with(".yaml")
                || file_name.ends_with(".yml")
                || file_name.ends_with(".toml")
            {
                "ModuleScript"
            } else if file_name.ends_with(".txt") {
                "StringValue"
            } else if file_name.ends_with(".csv") {
                "LocalizationTable"
            } else if file_name.ends_with(".rbxm") || file_name.ends_with(".rbxmx") {
                "Model"
            } else if !file_name.contains('.') {
                // Directory (no extension).
                "Folder"
            } else {
                // Unknown non-Luau file.
                "ModuleScript"
            }
        }
    }
}

// ---------------------------------------------------------------------------
// emitLegacyScripts support
// ---------------------------------------------------------------------------

/// The RunContext property value for scripts when `emitLegacyScripts` is false.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum RunContext {
    Server,
    Client,
}

/// Resolved instance class with optional RunContext for non-legacy script mode.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedClass {
    pub class_name: &'static str,
    pub run_context: Option<RunContext>,
}

/// Like `resolve_instance_class`, but when `emit_legacy_scripts` is false,
/// both server and client scripts are emitted as `"Script"` with a `RunContext`.
/// Module scripts remain `"ModuleScript"` in all modes.
pub fn resolve_instance_class_with_context(
    file_path: &str,
    emit_legacy_scripts: bool,
) -> ResolvedClass {
    let base = resolve_instance_class(file_path);

    if emit_legacy_scripts || base == "ModuleScript" || base == "Skip" || base == "Folder" {
        return ResolvedClass {
            class_name: base,
            run_context: None,
        };
    }

    let normalized = file_path.replace('\\', "/");
    let file_name = normalized.rsplit('/').next().unwrap_or(&normalized);

    if file_name.ends_with(".server.luau")
        || file_name.ends_with(".server.lua")
        || file_name == "init.server.luau"
        || file_name == "init.server.lua"
    {
        ResolvedClass {
            class_name: "Script",
            run_context: Some(RunContext::Server),
        }
    } else if file_name.ends_with(".client.luau")
        || file_name.ends_with(".client.lua")
        || file_name == "init.client.luau"
        || file_name == "init.client.lua"
    {
        ResolvedClass {
            class_name: "Script",
            run_context: Some(RunContext::Client),
        }
    } else {
        ResolvedClass {
            class_name: base,
            run_context: None,
        }
    }
}

// ---------------------------------------------------------------------------
// Nested project inclusion
// ---------------------------------------------------------------------------

/// Discover nested `default.project.json` files within a directory and parse
/// them as sub-projects.
pub fn discover_nested_projects(
    base_dir: &Path,
    parent_ignore_unknown: bool,
    visited: &mut std::collections::HashSet<PathBuf>,
) -> Result<Vec<ProjectTree>> {
    let canonical = base_dir
        .canonicalize()
        .unwrap_or_else(|_| base_dir.to_path_buf());

    if !visited.insert(canonical.clone()) {
        anyhow::bail!(
            "circular nested project reference detected at {}",
            base_dir.display()
        );
    }

    let project_file = base_dir.join("default.project.json");
    if !project_file.is_file() {
        return Ok(Vec::new());
    }

    let mut tree = parse_project(&project_file)?;

    for mapping in &mut tree.mappings {
        if !mapping.ignore_unknown {
            mapping.ignore_unknown = parent_ignore_unknown;
        }
    }

    Ok(vec![tree])
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn normalize_fs_path(raw: &str) -> String {
    let mut s = raw.replace('\\', "/");
    while s.starts_with("./") {
        s = s[2..].to_string();
    }
    while s.ends_with('/') && s.len() > 1 {
        s.pop();
    }
    s
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn test_project_json() -> &'static str {
        r#"{
            "name": "vertigo",
            "tree": {
                "$className": "DataModel",
                "$ignoreUnknownInstances": true,
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
        }"#
    }

    #[test]
    fn parse_project_extracts_mappings() {
        let tree =
            parse_project_str(test_project_json(), &PathBuf::from("test.project.json")).unwrap();
        assert_eq!(tree.name, "vertigo");
        assert!(tree.mappings.len() >= 4);

        let server = tree
            .mappings
            .iter()
            .find(|m| m.fs_path == "src/Server")
            .expect("server mapping");
        assert_eq!(server.instance_path, "ServerScriptService.Server");

        let client = tree
            .mappings
            .iter()
            .find(|m| m.fs_path == "src/Client")
            .expect("client mapping");
        assert_eq!(
            client.instance_path,
            "StarterPlayer.StarterPlayerScripts.Client"
        );

        let shared = tree
            .mappings
            .iter()
            .find(|m| m.fs_path == "src/Shared")
            .expect("shared mapping");
        assert_eq!(shared.instance_path, "ReplicatedStorage.Shared");
    }

    #[test]
    fn parse_project_extracts_vertigo_sync_builder_config() {
        let tree = parse_project_str(
            r#"{
                "name": "ArnisRoblox",
                "vertigoSync": {
                    "builders": {
                        "roots": ["src\\ServerScriptService\\StudioPreview"],
                        "dependencyRoots": ["src/ServerScriptService/ImportService", "src/ReplicatedStorage/Shared"]
                    }
                },
                "tree": {
                    "$className": "DataModel",
                    "ServerScriptService": {
                        "$path": "src/ServerScriptService"
                    }
                }
            }"#,
            &PathBuf::from("test.project.json"),
        )
        .unwrap();

        let config = tree.vertigo_sync.expect("vertigo sync config");
        assert_eq!(
            config.builders.roots,
            vec!["src/ServerScriptService/StudioPreview"]
        );
        assert_eq!(
            config.builders.dependency_roots,
            vec![
                "src/ServerScriptService/ImportService",
                "src/ReplicatedStorage/Shared"
            ]
        );
    }

    #[test]
    fn parse_project_extracts_edit_preview_config() {
        let tree = parse_project_str(
            r#"{
                "name": "ArnisRoblox",
                "vertigoSync": {
                    "builders": {
                        "roots": ["src/ServerScriptService/StudioPreview/AustinPreviewBuilder.lua"],
                        "dependencyRoots": ["src/ServerScriptService/StudioPreview"]
                    },
                    "editPreview": {
                        "enabled": true,
                        "builderModulePath": "ServerScriptService.StudioPreview.AustinPreviewBuilder",
                        "builderMethod": "Build",
                        "watchRoots": [
                            "ServerScriptService.StudioPreview",
                            "ReplicatedStorage.Shared"
                        ],
                        "debounceSeconds": 0.25,
                        "rootRefreshSeconds": 1.0,
                        "mode": "edit_only"
                    }
                },
                "tree": {
                    "$className": "DataModel",
                    "ServerScriptService": {
                        "$path": "src/ServerScriptService"
                    }
                }
            }"#,
            &PathBuf::from("test.project.json"),
        )
        .unwrap();

        let config = tree.vertigo_sync.expect("vertigo sync config");
        let edit_preview = config.edit_preview.expect("edit preview config");
        assert!(edit_preview.enabled);
        assert_eq!(
            edit_preview.builder_module_path,
            "ServerScriptService.StudioPreview.AustinPreviewBuilder"
        );
        assert_eq!(edit_preview.builder_method.as_deref(), Some("Build"));
        assert_eq!(
            edit_preview.watch_roots,
            vec![
                "ServerScriptService.StudioPreview",
                "ReplicatedStorage.Shared"
            ]
        );
        assert_eq!(edit_preview.debounce_seconds, Some(0.25));
        assert_eq!(edit_preview.root_refresh_seconds, Some(1.0));
        assert_eq!(edit_preview.mode.as_deref(), Some("edit_only"));
    }

    #[test]
    fn resolve_instance_class_init_scripts() {
        assert_eq!(
            resolve_instance_class("src/Server/init.server.luau"),
            "Script"
        );
        assert_eq!(
            resolve_instance_class("src/Client/init.client.luau"),
            "LocalScript"
        );
        assert_eq!(
            resolve_instance_class("src/Shared/Util/Types.luau"),
            "ModuleScript"
        );
        assert_eq!(resolve_instance_class("src/Server/Services"), "Folder");
    }

    #[test]
    fn resolve_instance_class_suffixed_scripts() {
        assert_eq!(resolve_instance_class("foo.server.luau"), "Script");
        assert_eq!(resolve_instance_class("bar.client.lua"), "LocalScript");
        assert_eq!(resolve_instance_class("baz.luau"), "ModuleScript");
    }

    #[test]
    fn resolve_instance_class_json_files() {
        assert_eq!(resolve_instance_class("src/config.json"), "ModuleScript");
    }

    #[test]
    fn resolve_instance_class_jsonc_files() {
        assert_eq!(resolve_instance_class("src/config.jsonc"), "ModuleScript");
    }

    #[test]
    fn resolve_instance_class_txt_files() {
        assert_eq!(resolve_instance_class("src/readme.txt"), "StringValue");
    }

    #[test]
    fn resolve_instance_class_csv_files() {
        assert_eq!(
            resolve_instance_class("src/locale.csv"),
            "LocalizationTable"
        );
    }

    #[test]
    fn resolve_instance_class_binary_models() {
        assert_eq!(resolve_instance_class("src/model.rbxm"), "Model");
        assert_eq!(resolve_instance_class("src/model.rbxmx"), "Model");
    }

    #[test]
    fn resolve_instance_class_meta_json_skipped() {
        assert_eq!(resolve_instance_class("src/Foo.meta.json"), "Skip");
    }

    #[test]
    fn ignore_unknown_inherits_from_parent() {
        let tree =
            parse_project_str(test_project_json(), &PathBuf::from("test.project.json")).unwrap();
        // Root has $ignoreUnknownInstances = true, so children inherit it.
        for mapping in &tree.mappings {
            assert!(
                mapping.ignore_unknown,
                "mapping {} should inherit ignore_unknown",
                mapping.fs_path
            );
        }
    }

    // P0: $properties/$attributes
    #[test]
    fn parse_project_extracts_properties_and_attributes() {
        let json = r#"{
            "name": "test",
            "tree": {
                "$className": "DataModel",
                "Lighting": {
                    "$className": "Lighting",
                    "$properties": {
                        "Ambient": [0.3, 0.3, 0.3],
                        "Brightness": 2.0,
                        "ClockTime": 14.5
                    },
                    "$attributes": {
                        "ZoneName": "overworld",
                        "FogEnabled": true
                    },
                    "$path": "src/Lighting"
                }
            }
        }"#;
        let tree = parse_project_str(json, &PathBuf::from("test.project.json")).unwrap();
        let lighting = tree
            .mappings
            .iter()
            .find(|m| m.instance_path == "Lighting")
            .expect("lighting mapping");
        let props = lighting
            .properties
            .as_ref()
            .expect("should have properties");
        assert!(props.contains_key("Brightness"));
        assert_eq!(props["Brightness"], serde_json::json!(2.0));
        let attrs = lighting
            .attributes
            .as_ref()
            .expect("should have attributes");
        assert!(attrs.contains_key("ZoneName"));
        assert_eq!(attrs["ZoneName"], serde_json::json!("overworld"));
    }

    #[test]
    fn parse_project_no_properties_returns_none() {
        let tree =
            parse_project_str(test_project_json(), &PathBuf::from("test.project.json")).unwrap();
        let server = tree
            .mappings
            .iter()
            .find(|m| m.fs_path == "src/Server")
            .expect("server mapping");
        assert!(server.properties.is_none());
        assert!(server.attributes.is_none());
    }

    #[test]
    fn resolve_instance_class_model_json() {
        assert_eq!(
            resolve_instance_class("src/Net/Remotes.model.json"),
            "ModuleScript"
        );
    }

    // P1-A: globIgnorePaths
    #[test]
    fn glob_ignore_paths_parsed() {
        let json = r#"{
            "name": "test",
            "globIgnorePaths": ["**/*.spec.luau", "src/vendor/**"],
            "tree": {
                "$className": "DataModel",
                "ServerScriptService": {
                    "$className": "ServerScriptService",
                    "Server": { "$path": "src/Server" }
                }
            }
        }"#;
        let tree = parse_project_str(json, &PathBuf::from("test.project.json")).unwrap();
        assert_eq!(
            tree.glob_ignore_paths,
            vec!["**/*.spec.luau", "src/vendor/**"]
        );
    }

    #[test]
    fn glob_ignore_paths_defaults_empty() {
        let tree =
            parse_project_str(test_project_json(), &PathBuf::from("test.project.json")).unwrap();
        assert!(tree.glob_ignore_paths.is_empty());
    }

    // P1-A: emitLegacyScripts
    #[test]
    fn emit_legacy_scripts_defaults_true() {
        let tree =
            parse_project_str(test_project_json(), &PathBuf::from("test.project.json")).unwrap();
        assert!(tree.emit_legacy_scripts);
    }

    #[test]
    fn emit_legacy_scripts_false_parsed() {
        let json = r#"{
            "name": "test",
            "emitLegacyScripts": false,
            "tree": {
                "$className": "DataModel",
                "ServerScriptService": {
                    "$className": "ServerScriptService",
                    "Server": { "$path": "src/Server" }
                }
            }
        }"#;
        let tree = parse_project_str(json, &PathBuf::from("test.project.json")).unwrap();
        assert!(!tree.emit_legacy_scripts);
    }

    #[test]
    fn resolve_class_with_context_legacy_mode() {
        let r = resolve_instance_class_with_context("foo.server.luau", true);
        assert_eq!(r.class_name, "Script");
        assert_eq!(r.run_context, None);
        let r = resolve_instance_class_with_context("bar.client.luau", true);
        assert_eq!(r.class_name, "LocalScript");
        assert_eq!(r.run_context, None);
    }

    #[test]
    fn resolve_class_with_context_non_legacy_mode() {
        let r = resolve_instance_class_with_context("foo.server.luau", false);
        assert_eq!(r.class_name, "Script");
        assert_eq!(r.run_context, Some(RunContext::Server));
        let r = resolve_instance_class_with_context("bar.client.luau", false);
        assert_eq!(r.class_name, "Script");
        assert_eq!(r.run_context, Some(RunContext::Client));
        let r = resolve_instance_class_with_context("baz.luau", false);
        assert_eq!(r.class_name, "ModuleScript");
        assert_eq!(r.run_context, None);
    }

    #[test]
    fn resolve_instance_class_toml_files() {
        assert_eq!(resolve_instance_class("src/config.toml"), "ModuleScript");
    }

    // P2: yaml/yml
    #[test]
    fn resolve_instance_class_yaml_files() {
        assert_eq!(resolve_instance_class("src/config.yaml"), "ModuleScript");
        assert_eq!(resolve_instance_class("src/config.yml"), "ModuleScript");
    }

    // P2: $schema
    #[test]
    fn schema_field_does_not_create_phantom_instance() {
        let json = r#"{
            "$schema": "https://raw.githubusercontent.com/rojo-rbx/vscode-rojo/main/schemas/project.json",
            "name": "test-schema",
            "tree": {
                "$className": "DataModel",
                "ReplicatedStorage": {
                    "$className": "ReplicatedStorage",
                    "Shared": { "$path": "src/Shared" }
                }
            }
        }"#;
        let tree = parse_project_str(json, &PathBuf::from("schema.project.json")).unwrap();
        assert_eq!(tree.name, "test-schema");
        assert_eq!(tree.mappings.len(), 1);
        assert_eq!(tree.mappings[0].fs_path, "src/Shared");
    }

    // P2: servePort/Address
    #[test]
    fn serve_port_and_address_from_project() {
        let json = r#"{
            "name": "test-serve",
            "projectId": "test-serve-id",
            "servePort": 8080,
            "serveAddress": "0.0.0.0",
            "tree": {
                "$className": "DataModel",
                "ServerScriptService": {
                    "Server": { "$path": "src/Server" }
                }
            }
        }"#;
        let tree = parse_project_str(json, &PathBuf::from("serve.project.json")).unwrap();
        assert_eq!(tree.serve_port, Some(8080));
        assert_eq!(tree.serve_address, Some("0.0.0.0".to_string()));
        assert_eq!(tree.project_id, "test-serve-id");
    }

    #[test]
    fn serve_port_absent_returns_none() {
        let tree =
            parse_project_str(test_project_json(), &PathBuf::from("test.project.json")).unwrap();
        assert_eq!(tree.serve_port, None);
        assert_eq!(tree.serve_address, None);
        assert!(!tree.project_id.is_empty());
    }

    #[test]
    fn derived_project_id_is_stable_for_same_source_path() {
        let path = PathBuf::from("nested/default.project.json");
        let left = parse_project_str(test_project_json(), &path).unwrap();
        let right = parse_project_str(test_project_json(), &path).unwrap();
        assert_eq!(left.project_id, right.project_id);
    }

    #[test]
    fn derived_project_id_changes_with_source_path() {
        let left = parse_project_str(
            test_project_json(),
            &PathBuf::from("a/default.project.json"),
        )
        .unwrap();
        let right = parse_project_str(
            test_project_json(),
            &PathBuf::from("b/default.project.json"),
        )
        .unwrap();
        assert_ne!(left.project_id, right.project_id);
    }

    // P1-A: nested projects
    #[test]
    fn discover_nested_project_basic() {
        let dir = tempfile::tempdir().expect("tempdir");
        let sub = dir.path().join("Packages/Signal");
        std::fs::create_dir_all(&sub).expect("mkdir");
        std::fs::write(
            sub.join("default.project.json"),
            r#"{"name":"Signal","tree":{"$className":"ModuleScript","$path":"src"}}"#,
        )
        .expect("write");
        std::fs::create_dir_all(sub.join("src")).expect("mkdir src");
        std::fs::write(sub.join("src/init.luau"), "return {}").expect("write init");

        let mut visited = std::collections::HashSet::new();
        let trees = discover_nested_projects(&sub, true, &mut visited).unwrap();
        assert_eq!(trees.len(), 1);
        assert_eq!(trees[0].name, "Signal");
        for m in &trees[0].mappings {
            assert!(m.ignore_unknown);
        }
    }

    #[test]
    fn discover_nested_project_circular_detection() {
        let dir = tempfile::tempdir().expect("tempdir");
        let sub = dir.path().join("pkg");
        std::fs::create_dir_all(&sub).expect("mkdir");
        std::fs::write(
            sub.join("default.project.json"),
            r#"{"name":"loop","tree":{"$className":"Folder"}}"#,
        )
        .expect("write");

        let mut visited = std::collections::HashSet::new();
        let _ = discover_nested_projects(&sub, false, &mut visited).unwrap();
        let result = discover_nested_projects(&sub, false, &mut visited);
        assert!(result.is_err());
    }
}
