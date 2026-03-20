//! Project scaffolding for `vsync init`.

use anyhow::{Context, Result};
use std::fs;
use std::path::Path;

/// Write `contents` to `path` only when the file does not already exist.
/// Parent directories are created automatically.
/// Returns `true` if the file was written, `false` if it was skipped.
///
/// Uses `create_new(true)` for atomic check-and-create to avoid TOCTOU races.
fn write_if_missing(path: &Path, contents: &str) -> Result<bool> {
    use std::fs::OpenOptions;
    use std::io::Write;

    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create directory {}", parent.display()))?;
    }
    match OpenOptions::new().write(true).create_new(true).open(path) {
        Ok(mut f) => {
            f.write_all(contents.as_bytes())
                .with_context(|| format!("failed to write {}", path.display()))?;
            Ok(true)
        }
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Ok(false),
        Err(e) => Err(anyhow::anyhow!(e).context(format!("failed to create {}", path.display()))),
    }
}

/// Create a batteries-included project scaffold under `root`.
///
/// Every file is created only when absent — existing files are never overwritten.
pub fn run_init(root: &Path, name: Option<&str>) -> Result<()> {
    let project_name = name
        .map(|n| n.to_string())
        .or_else(|| {
            root.file_name()
                .and_then(|n| n.to_str())
                .map(|n| n.to_string())
        })
        .unwrap_or_else(|| "my-project".to_string());

    // -- default.project.json ------------------------------------------------
    let project_path = root.join("default.project.json");
    if !project_path.exists() {
        let project_json = serde_json::json!({
            "name": project_name,
            "projectId": uuid::Uuid::new_v4().to_string(),
            "tree": {
                "$className": "DataModel",
                "ServerScriptService": {
                    "Server": {
                        "$path": "src/Server"
                    }
                },
                "StarterPlayer": {
                    "StarterPlayerScripts": {
                        "Client": {
                            "$path": "src/Client"
                        }
                    }
                },
                "ReplicatedStorage": {
                    "Shared": {
                        "$path": "src/Shared"
                    }
                }
            }
        });
        let formatted = serde_json::to_string_pretty(&project_json)?;
        write_if_missing(&project_path, &formatted)?;
    }

    // -- vsync.toml ----------------------------------------------------------
    {
        use crate::config::{VsyncConfig, PackageConfig, FormatConfig};

        let lint = std::collections::BTreeMap::from([
            ("unused-variable".to_string(), "warn".to_string()),
            ("deprecated-api".to_string(), "error".to_string()),
            ("global-shadow".to_string(), "error".to_string()),
            ("strict-mode".to_string(), "warn".to_string()),
            ("wait-deprecated".to_string(), "warn".to_string()),
        ]);
        let config = VsyncConfig {
            package: PackageConfig {
                name: project_name.clone(),
                version: "0.1.0".to_string(),
                ..PackageConfig::default()
            },
            format: FormatConfig {
                indent_type: Some("tabs".to_string()),
                indent_width: Some(4),
                line_width: Some(120),
                quote_style: Some("double".to_string()),
                ..FormatConfig::default()
            },
            lint,
            ..VsyncConfig::default()
        };
        let vsync_toml = toml::to_string_pretty(&config)
            .context("failed to serialize vsync.toml")?;
        write_if_missing(&root.join("vsync.toml"), &vsync_toml)?;
    }

    // -- .gitignore ----------------------------------------------------------
    let gitignore = "\
Packages/
*.rbxl
*.rbxlx
.vertigo-sync-state/
sourcemap.json
";
    write_if_missing(&root.join(".gitignore"), gitignore)?;

    // -- Source files ---------------------------------------------------------
    write_if_missing(
        &root.join("src/Server/init.server.luau"),
        "--!strict\nprint(\"[Server] Hello from Vertigo Sync!\")\n",
    )?;
    write_if_missing(
        &root.join("src/Client/init.client.luau"),
        "--!strict\nprint(\"[Client] Hello from Vertigo Sync!\")\n",
    )?;
    write_if_missing(
        &root.join("src/Shared/init.luau"),
        "--!strict\nreturn {}\n",
    )?;

    // -- tests/init.luau -----------------------------------------------------
    write_if_missing(
        &root.join("tests/init.luau"),
        "--!strict\n-- Add tests here\nreturn {}\n",
    )?;

    // -- .vscode/settings.json -----------------------------------------------
    let vscode_settings = serde_json::json!({
        "luau-lsp.sourcemap.enabled": true,
        "luau-lsp.sourcemap.autogenerate": false,
        "luau-lsp.sourcemap.rojoProjectFile": "default.project.json"
    });
    let vscode_str = serde_json::to_string_pretty(&vscode_settings)?;
    write_if_missing(&root.join(".vscode/settings.json"), &vscode_str)?;

    // -- .github/workflows/ci.yml --------------------------------------------
    let ci_yml = r#"name: CI

on:
  push:
    branches: [main]
  pull_request:
    branches: [main]

jobs:
  check:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4

      - name: Install vsync
        run: cargo install vertigo-sync

      - name: Validate
        run: vsync validate

      - name: Format check
        run: vsync fmt --check

      - name: Build
        run: vsync build
"#;
    write_if_missing(&root.join(".github/workflows/ci.yml"), ci_yml)?;

    // -- README.md -----------------------------------------------------------
    let readme = format!(
        r#"# {project_name}

Built with [Vertigo Sync](https://github.com/adpena/vertigo-sync).

## Getting Started

```bash
vsync install        # install dependencies
vsync validate       # check project health
vsync serve --turbo  # start syncing to Roblox Studio
```
"#
    );
    write_if_missing(&root.join("README.md"), &readme)?;

    Ok(())
}
