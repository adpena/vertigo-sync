//! Rojo-compatible project.json parser.
//!
//! Parses `default.project.json` and extracts the `tree` structure into a flat
//! list of filesystem-to-DataModel path mappings.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::Path;

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

/// Parsed project tree with resolved filesystem-to-instance mappings.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ProjectTree {
    pub name: String,
    pub mappings: Vec<PathMapping>,
}

/// A single filesystem path mapped to a Roblox DataModel instance path.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PathMapping {
    /// Filesystem path relative to project root, e.g. `"src/Server"`.
    pub fs_path: String,
    /// DataModel instance path, e.g. `"ServerScriptService.Server"`.
    pub instance_path: String,
    /// Roblox class name, e.g. `"ServerScriptService"`.
    pub class_name: String,
    /// Whether unknown instances should be ignored during sync.
    pub ignore_unknown: bool,
}

// ---------------------------------------------------------------------------
// Raw JSON schema (mirrors Rojo project file structure)
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct RawProject {
    name: String,
    tree: RawTreeNode,
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

    Ok(ProjectTree {
        name: raw.name,
        mappings,
    })
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

    if let Some(fs_path) = obj.get("$path").and_then(|v| v.as_str()) {
        mappings.push(PathMapping {
            fs_path: normalize_fs_path(fs_path),
            instance_path: instance_path.to_string(),
            class_name: class_name.clone(),
            ignore_unknown,
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
pub fn resolve_instance_class(file_path: &str) -> &str {
    let normalized = file_path.replace('\\', "/");
    let file_name = normalized.rsplit('/').next().unwrap_or(&normalized);

    // Skip .meta.json sidecars — they are not standalone instances.
    if file_name.ends_with(".meta.json") {
        return "Skip";
    }

    match file_name {
        "init.server.luau" | "init.server.lua" => "Script",
        "init.client.luau" | "init.client.lua" => "LocalScript",
        _ => {
            if file_name.ends_with(".server.luau") || file_name.ends_with(".server.lua") {
                "Script"
            } else if file_name.ends_with(".client.luau") || file_name.ends_with(".client.lua") {
                "LocalScript"
            } else if file_name.ends_with(".luau") || file_name.ends_with(".lua") {
                "ModuleScript"
            } else if file_name.ends_with(".json") {
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
    fn resolve_instance_class_txt_files() {
        assert_eq!(resolve_instance_class("src/readme.txt"), "StringValue");
    }

    #[test]
    fn resolve_instance_class_csv_files() {
        assert_eq!(resolve_instance_class("src/locale.csv"), "LocalizationTable");
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
}
