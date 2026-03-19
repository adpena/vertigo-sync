//! Rojo-compatible sourcemap generation for luau-lsp integration.
//!
//! Generates a `sourcemap.json` tree that mirrors the DataModel hierarchy
//! derived from the project file and filesystem layout. This enables
//! luau-lsp autocomplete, go-to-definition, and type checking across
//! the entire project without Rojo running.

use anyhow::{Context, Result};
use serde::Serialize;
use std::collections::BTreeMap;
use std::path::Path;

use crate::project::ProjectTree;

// ---------------------------------------------------------------------------
// Sourcemap types — exact Rojo schema that luau-lsp expects
// ---------------------------------------------------------------------------

/// A node in the Rojo-compatible sourcemap tree.
///
/// luau-lsp reads this format for require resolution, type checking,
/// and autocomplete across the entire DataModel.
#[derive(Debug, Clone, Serialize)]
pub struct SourcemapNode {
    /// Instance name in the DataModel (e.g. "Server", "DataService").
    pub name: String,
    /// Roblox class name (e.g. "Script", "ModuleScript", "Folder").
    #[serde(rename = "className")]
    pub class_name: String,
    /// Filesystem paths associated with this instance (relative to project root).
    #[serde(rename = "filePaths")]
    pub file_paths: Vec<String>,
    /// Child instances.
    pub children: Vec<SourcemapNode>,
}

// ---------------------------------------------------------------------------
// Well-known service class names (zero-allocation lookup)
// ---------------------------------------------------------------------------

/// Resolve the Roblox class name for a well-known service or container.
/// Returns the name itself as a fallback for unknown intermediate nodes.
fn service_class(name: &str) -> &str {
    match name {
        "Workspace" => "Workspace",
        "ServerScriptService" => "ServerScriptService",
        "ServerStorage" => "ServerStorage",
        "ReplicatedStorage" => "ReplicatedStorage",
        "ReplicatedFirst" => "ReplicatedFirst",
        "StarterPlayer" => "StarterPlayer",
        "StarterPlayerScripts" => "StarterPlayerScripts",
        "StarterCharacterScripts" => "StarterCharacterScripts",
        "StarterGui" => "StarterGui",
        "StarterPack" => "StarterPack",
        "Lighting" => "Lighting",
        "SoundService" => "SoundService",
        "Chat" => "Chat",
        "Teams" => "Teams",
        "TestService" => "TestService",
        other => other,
    }
}

// ---------------------------------------------------------------------------
// Init file detection (mirrors resolve_container_class in main.rs)
// ---------------------------------------------------------------------------

/// The set of init file names and their corresponding class.
const INIT_FILES: &[(&str, &str)] = &[
    ("init.server.luau", "Script"),
    ("init.server.lua", "Script"),
    ("init.client.luau", "LocalScript"),
    ("init.client.lua", "LocalScript"),
    ("init.luau", "ModuleScript"),
    ("init.lua", "ModuleScript"),
];

/// Determine the Roblox class for a directory. If it contains an init script,
/// the directory becomes that script type; otherwise it's a Folder.
fn dir_class(dir: &Path) -> &'static str {
    for &(init_name, class) in INIT_FILES {
        if dir.join(init_name).exists() {
            return class;
        }
    }
    "Folder"
}

/// Return the init file path (relative) if one exists in the directory.
fn dir_init_file(dir: &Path) -> Option<String> {
    for &(init_name, _) in INIT_FILES {
        let candidate = dir.join(init_name);
        if candidate.exists() {
            return Some(init_name.to_string());
        }
    }
    None
}

/// Strip Luau/Lua extensions to derive the instance name from a filename.
fn instance_name_from_file(name: &str) -> &str {
    name.strip_suffix(".server.luau")
        .or_else(|| name.strip_suffix(".server.lua"))
        .or_else(|| name.strip_suffix(".client.luau"))
        .or_else(|| name.strip_suffix(".client.lua"))
        .or_else(|| name.strip_suffix(".luau"))
        .or_else(|| name.strip_suffix(".lua"))
        .or_else(|| name.strip_suffix(".model.json"))
        .or_else(|| name.strip_suffix(".json"))
        .or_else(|| name.strip_suffix(".txt"))
        .or_else(|| name.strip_suffix(".csv"))
        .unwrap_or(name)
}

/// Determine the Roblox class for a file from its name.
fn file_class(name: &str) -> &'static str {
    if name.ends_with(".server.luau") || name.ends_with(".server.lua") {
        "Script"
    } else if name.ends_with(".client.luau") || name.ends_with(".client.lua") {
        "LocalScript"
    } else if name.ends_with(".luau") || name.ends_with(".lua") {
        "ModuleScript"
    } else if name.ends_with(".model.json") {
        // .model.json files define their own ClassName inside, but for
        // sourcemap purposes we use Folder as a reasonable default.
        "Folder"
    } else if name.ends_with(".json") {
        "ModuleScript"
    } else if name.ends_with(".txt") {
        "StringValue"
    } else if name.ends_with(".csv") {
        "LocalizationTable"
    } else {
        "ModuleScript"
    }
}

// ---------------------------------------------------------------------------
// Filesystem walking — builds sourcemap subtree for a directory
// ---------------------------------------------------------------------------

/// Recursively build a sourcemap tree from a filesystem directory.
/// `fs_prefix` is the path prefix relative to the project root (e.g. "src/Server").
fn walk_dir(dir: &Path, fs_prefix: &str, include_non_scripts: bool) -> Result<Vec<SourcemapNode>> {
    let mut children = Vec::new();

    let mut entries: Vec<_> = std::fs::read_dir(dir)
        .with_context(|| format!("failed to read directory {}", dir.display()))?
        .filter_map(|e| e.ok())
        .collect();

    // Sort for determinism.
    entries.sort_by_key(|e| e.file_name());

    for entry in &entries {
        let path = entry.path();
        let file_name = entry.file_name();
        let name_str = file_name.to_string_lossy();

        // Skip hidden files and .meta.json sidecars.
        if name_str.starts_with('.') || name_str.ends_with(".meta.json") {
            continue;
        }

        let child_fs_path = format!("{}/{}", fs_prefix, name_str);

        if path.is_dir() {
            // Check for init file — directory becomes the script instance.
            let class = dir_class(&path);
            let mut file_paths = vec![child_fs_path.clone()];
            if let Some(init) = dir_init_file(&path) {
                file_paths.push(format!("{}/{}", child_fs_path, init));
            }

            let grandchildren = walk_dir(&path, &child_fs_path, include_non_scripts)?;

            // Filter out init scripts from children (they are the directory itself).
            let filtered: Vec<SourcemapNode> = grandchildren
                .into_iter()
                .filter(|c| {
                    // Remove init.*.luau / init.*.lua children — they are the parent.
                    !c.name.starts_with("init.")
                        || (!c.name.ends_with(".luau") && !c.name.ends_with(".lua"))
                })
                .collect();

            children.push(SourcemapNode {
                name: name_str.to_string(),
                class_name: class.to_string(),
                file_paths,
                children: filtered,
            });
        } else if path.is_file() {
            let class = file_class(&name_str);

            // Skip non-Luau files unless include_non_scripts is set.
            // JSON/TXT/CSV files are non-script even when classified as ModuleScript.
            let is_luau_script = (name_str.ends_with(".luau")
                || name_str.ends_with(".lua")
                || name_str.ends_with(".server.luau")
                || name_str.ends_with(".server.lua")
                || name_str.ends_with(".client.luau")
                || name_str.ends_with(".client.lua"))
                && !name_str.ends_with(".model.json");
            if !is_luau_script && !include_non_scripts {
                continue;
            }

            // Init files are handled by the parent directory.
            if name_str.starts_with("init.") {
                continue;
            }

            let inst_name = instance_name_from_file(&name_str);

            children.push(SourcemapNode {
                name: inst_name.to_string(),
                class_name: class.to_string(),
                file_paths: vec![child_fs_path],
                children: vec![],
            });
        }
    }

    Ok(children)
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Generate a Rojo-compatible sourcemap tree from a project file.
///
/// The resulting `SourcemapNode` has `name: "game"`, `className: "DataModel"`,
/// and contains the full instance hierarchy derived from the project tree
/// and filesystem layout.
pub fn generate_sourcemap(
    root: &Path,
    project: &ProjectTree,
    include_non_scripts: bool,
) -> Result<SourcemapNode> {
    // Build the DataModel root.
    let mut service_children: BTreeMap<String, SourcemapNode> = BTreeMap::new();

    for mapping in &project.mappings {
        let segments: Vec<&str> = mapping.instance_path.split('.').collect();
        let fs_path = root.join(&mapping.fs_path);

        // Build the mapping's subtree from the filesystem.
        let mapping_children = if fs_path.is_dir() {
            walk_dir(&fs_path, &mapping.fs_path, include_non_scripts)?
        } else if fs_path.is_file() {
            // Single file mapping — no children.
            vec![]
        } else {
            // Path doesn't exist — skip silently.
            continue;
        };

        // Determine the leaf node's class and file_paths.
        let leaf_class = if fs_path.is_dir() {
            dir_class(&fs_path)
        } else {
            let name = fs_path
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_default();
            file_class(&name)
        };

        let leaf_file_paths = if fs_path.is_dir() {
            let mut paths = vec![mapping.fs_path.clone()];
            if let Some(init) = dir_init_file(&fs_path) {
                paths.push(format!("{}/{}", mapping.fs_path, init));
            }
            paths
        } else {
            vec![mapping.fs_path.clone()]
        };

        // Now insert into the tree, creating intermediate service nodes as needed.
        insert_into_tree(
            &mut service_children,
            &segments,
            0,
            leaf_class,
            leaf_file_paths,
            mapping_children,
        );
    }

    Ok(SourcemapNode {
        name: "game".to_string(),
        class_name: "DataModel".to_string(),
        file_paths: vec![],
        children: service_children.into_values().collect(),
    })
}

/// Insert a mapping into the tree at the correct position, creating
/// intermediate nodes as needed.
fn insert_into_tree(
    siblings: &mut BTreeMap<String, SourcemapNode>,
    segments: &[&str],
    depth: usize,
    leaf_class: &str,
    leaf_file_paths: Vec<String>,
    leaf_children: Vec<SourcemapNode>,
) {
    if depth >= segments.len() {
        return;
    }

    let name = segments[depth];
    let is_leaf = depth == segments.len() - 1;

    if is_leaf {
        // This is the final segment — set properties from the mapping.
        let node = siblings
            .entry(name.to_string())
            .or_insert_with(|| SourcemapNode {
                name: name.to_string(),
                class_name: leaf_class.to_string(),
                file_paths: vec![],
                children: vec![],
            });
        node.file_paths = leaf_file_paths;
        // Merge children (don't replace — multiple mappings might target same parent).
        node.children.extend(leaf_children);
    } else {
        // Intermediate node — always use service_class lookup.
        // mapping.class_name refers to the leaf node, not intermediates.
        let class = service_class(name);

        let node = siblings
            .entry(name.to_string())
            .or_insert_with(|| SourcemapNode {
                name: name.to_string(),
                class_name: class.to_string(),
                file_paths: vec![],
                children: vec![],
            });

        // Convert children vec into a BTreeMap for merging, then back.
        let mut child_map: BTreeMap<String, SourcemapNode> = BTreeMap::new();
        for child in node.children.drain(..) {
            child_map.insert(child.name.clone(), child);
        }

        insert_into_tree(
            &mut child_map,
            segments,
            depth + 1,
            leaf_class,
            leaf_file_paths,
            leaf_children,
        );

        node.children = child_map.into_values().collect();
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::project::PathMapping;
    use std::fs;
    use tempfile::tempdir;

    /// Helper: create a minimal project tree and filesystem for testing.
    fn setup_test_project() -> (tempfile::TempDir, ProjectTree) {
        let tmp = tempdir().expect("tempdir");
        let root = tmp.path();

        // Create directory structure.
        fs::create_dir_all(root.join("src/Server/Services")).unwrap();
        fs::create_dir_all(root.join("src/Client")).unwrap();
        fs::create_dir_all(root.join("src/Shared/Util")).unwrap();

        // Create files.
        fs::write(root.join("src/Server/init.server.luau"), "-- server").unwrap();
        fs::write(
            root.join("src/Server/Services/DataService.luau"),
            "return {}",
        )
        .unwrap();
        fs::write(root.join("src/Client/init.client.luau"), "-- client").unwrap();
        fs::write(root.join("src/Shared/Util/Types.luau"), "return nil").unwrap();

        let tree = ProjectTree {
            name: "test-project".to_string(),
            project_id: "test-project".to_string(),
            glob_ignore_paths: vec![],
            emit_legacy_scripts: true,
            serve_port: None,
            serve_address: None,
            vertigo_sync: None,
            mappings: vec![
                PathMapping {
                    fs_path: "src/Server".to_string(),
                    instance_path: "ServerScriptService.Server".to_string(),
                    class_name: "ServerScriptService".to_string(),
                    ignore_unknown: true,
                    properties: None,
                    attributes: None,
                },
                PathMapping {
                    fs_path: "src/Client".to_string(),
                    instance_path: "StarterPlayer.StarterPlayerScripts.Client".to_string(),
                    class_name: "StarterPlayer".to_string(),
                    ignore_unknown: true,
                    properties: None,
                    attributes: None,
                },
                PathMapping {
                    fs_path: "src/Shared".to_string(),
                    instance_path: "ReplicatedStorage.Shared".to_string(),
                    class_name: "ReplicatedStorage".to_string(),
                    ignore_unknown: true,
                    properties: None,
                    attributes: None,
                },
            ],
        };

        (tmp, tree)
    }

    #[test]
    fn sourcemap_root_is_game_datamodel() {
        let (tmp, tree) = setup_test_project();
        let sm = generate_sourcemap(tmp.path(), &tree, true).unwrap();
        assert_eq!(sm.name, "game");
        assert_eq!(sm.class_name, "DataModel");
        assert!(sm.file_paths.is_empty());
    }

    #[test]
    fn sourcemap_has_service_containers() {
        let (tmp, tree) = setup_test_project();
        let sm = generate_sourcemap(tmp.path(), &tree, true).unwrap();

        let names: Vec<&str> = sm.children.iter().map(|c| c.name.as_str()).collect();
        assert!(names.contains(&"ServerScriptService"));
        assert!(names.contains(&"ReplicatedStorage"));
        assert!(names.contains(&"StarterPlayer"));
    }

    #[test]
    fn sourcemap_server_has_init_file_path() {
        let (tmp, tree) = setup_test_project();
        let sm = generate_sourcemap(tmp.path(), &tree, true).unwrap();

        let sss = sm
            .children
            .iter()
            .find(|c| c.name == "ServerScriptService")
            .unwrap();
        let server = sss.children.iter().find(|c| c.name == "Server").unwrap();
        assert_eq!(server.class_name, "Script");
        assert!(server.file_paths.contains(&"src/Server".to_string()));
        assert!(
            server
                .file_paths
                .contains(&"src/Server/init.server.luau".to_string())
        );
    }

    #[test]
    fn sourcemap_includes_module_scripts() {
        let (tmp, tree) = setup_test_project();
        let sm = generate_sourcemap(tmp.path(), &tree, true).unwrap();

        let sss = sm
            .children
            .iter()
            .find(|c| c.name == "ServerScriptService")
            .unwrap();
        let server = sss.children.iter().find(|c| c.name == "Server").unwrap();
        let services = server
            .children
            .iter()
            .find(|c| c.name == "Services")
            .unwrap();
        let data_svc = services
            .children
            .iter()
            .find(|c| c.name == "DataService")
            .unwrap();
        assert_eq!(data_svc.class_name, "ModuleScript");
        assert!(
            data_svc
                .file_paths
                .contains(&"src/Server/Services/DataService.luau".to_string())
        );
    }

    #[test]
    fn sourcemap_json_roundtrip() {
        let (tmp, tree) = setup_test_project();
        let sm = generate_sourcemap(tmp.path(), &tree, true).unwrap();
        let json = serde_json::to_string_pretty(&sm).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed["name"], "game");
        assert_eq!(parsed["className"], "DataModel");
    }

    #[test]
    fn sourcemap_non_scripts_excluded_when_flag_false() {
        let tmp = tempdir().expect("tempdir");
        let root = tmp.path();
        fs::create_dir_all(root.join("src/Data")).unwrap();
        fs::write(root.join("src/Data/config.json"), "{}").unwrap();
        fs::write(root.join("src/Data/Module.luau"), "return {}").unwrap();

        let tree = ProjectTree {
            name: "test".to_string(),
            project_id: "test".to_string(),
            glob_ignore_paths: vec![],
            emit_legacy_scripts: true,
            serve_port: None,
            serve_address: None,
            vertigo_sync: None,
            mappings: vec![PathMapping {
                fs_path: "src/Data".to_string(),
                instance_path: "ReplicatedStorage.Data".to_string(),
                class_name: "ReplicatedStorage".to_string(),
                ignore_unknown: false,
                properties: None,
                attributes: None,
            }],
        };

        let sm = generate_sourcemap(root, &tree, false).unwrap();
        let rs = sm
            .children
            .iter()
            .find(|c| c.name == "ReplicatedStorage")
            .unwrap();
        let data = rs.children.iter().find(|c| c.name == "Data").unwrap();

        // Module.luau should be present, config.json should be excluded.
        let names: Vec<&str> = data.children.iter().map(|c| c.name.as_str()).collect();
        assert!(names.contains(&"Module"));
        assert!(!names.contains(&"config"));
    }

    #[test]
    fn instance_name_strips_extensions() {
        assert_eq!(instance_name_from_file("Foo.server.luau"), "Foo");
        assert_eq!(instance_name_from_file("Bar.client.lua"), "Bar");
        assert_eq!(instance_name_from_file("Baz.luau"), "Baz");
        assert_eq!(instance_name_from_file("Qux.model.json"), "Qux");
        assert_eq!(instance_name_from_file("NoExt"), "NoExt");
    }

    #[test]
    fn file_class_correct() {
        assert_eq!(file_class("foo.server.luau"), "Script");
        assert_eq!(file_class("bar.client.lua"), "LocalScript");
        assert_eq!(file_class("baz.luau"), "ModuleScript");
        assert_eq!(file_class("data.model.json"), "Folder");
        assert_eq!(file_class("config.json"), "ModuleScript");
        assert_eq!(file_class("text.txt"), "StringValue");
        assert_eq!(file_class("locale.csv"), "LocalizationTable");
    }

    #[test]
    fn model_json_detected_in_sourcemap() {
        let tmp = tempdir().expect("tempdir");
        let root = tmp.path();
        fs::create_dir_all(root.join("src/Net")).unwrap();
        fs::write(
            root.join("src/Net/Remotes.model.json"),
            r#"{"ClassName":"Folder"}"#,
        )
        .unwrap();

        let tree = ProjectTree {
            name: "test".to_string(),
            project_id: "test".to_string(),
            glob_ignore_paths: vec![],
            emit_legacy_scripts: true,
            serve_port: None,
            serve_address: None,
            vertigo_sync: None,
            mappings: vec![PathMapping {
                fs_path: "src/Net".to_string(),
                instance_path: "ReplicatedStorage.Net".to_string(),
                class_name: "ReplicatedStorage".to_string(),
                ignore_unknown: false,
                properties: None,
                attributes: None,
            }],
        };

        let sm = generate_sourcemap(root, &tree, true).unwrap();
        let rs = sm
            .children
            .iter()
            .find(|c| c.name == "ReplicatedStorage")
            .unwrap();
        let net = rs.children.iter().find(|c| c.name == "Net").unwrap();
        let remotes = net.children.iter().find(|c| c.name == "Remotes");
        assert!(remotes.is_some(), "Remotes.model.json should appear");
    }
}
