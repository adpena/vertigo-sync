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
        // Validate project_name for TOML safety
        let safe_name: String = project_name
            .chars()
            .filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_' || *c == '/')
            .collect();

        let vsync_toml = format!(r#"# vsync.toml — unified project configuration
# Documentation: https://github.com/vertigo-sync/vertigo-sync/blob/main/docs/configuration.md

[package]
name = "{safe_name}"
version = "0.1.0"
# realm = "shared"  # shared | server

[dependencies]
# promise = "evaera/promise@^4.0.0"

[server-dependencies]

[dev-dependencies]

# Lint rule configuration. Values: "error", "warn", "off"
# Full rule list: https://github.com/vertigo-sync/vertigo-sync/blob/main/docs/configuration.md#lint
[lint]
unused-variable = "warn"
global-shadow = "error"
wait-deprecated = "warn"
empty-block = "warn"
unreachable-code = "warn"

# Formatting options (powered by StyLua)
# Full options: https://github.com/vertigo-sync/vertigo-sync/blob/main/docs/configuration.md#format
[format]
indent-type = "tabs"
indent-width = 4
line-width = 120
quote-style = "double"

# Project scripts — run with: vsync run <name>
# [scripts]
# test = "vsync build -o test.rbxl"
"#);
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

Built with [Vertigo Sync](https://github.com/vertigo-sync/vertigo-sync).

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

/// Apply the "library" template on top of the default scaffold.
///
/// Adds: CHANGELOG.md, `.github/workflows/release.yml`, enriched `vsync.toml`
/// metadata (description, license, authors).
pub fn apply_library_template(root: &Path, project_name: &str) -> Result<()> {
    // CHANGELOG.md
    let changelog = format!(
        r#"# Changelog

All notable changes to **{project_name}** will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/).

## [Unreleased]

### Added
- Initial release
"#
    );
    write_if_missing(&root.join("CHANGELOG.md"), &changelog)?;

    // .github/workflows/release.yml — publish on tag push
    let release_yml = r#"name: Release

on:
  push:
    tags:
      - "v*"

jobs:
  publish:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4

      - name: Install vsync
        run: cargo install vertigo-sync

      - name: Validate
        run: vsync validate

      - name: Format check
        run: vsync fmt --check

      - name: Publish
        run: vsync publish
        env:
          VSYNC_TOKEN: ${{ secrets.VSYNC_TOKEN }}
"#;
    write_if_missing(
        &root.join(".github/workflows/release.yml"),
        release_yml,
    )?;

    // Enrich vsync.toml with library metadata if not already present.
    // We re-read the file to patch in additional fields.
    let toml_path = root.join("vsync.toml");
    if toml_path.exists() {
        let content = std::fs::read_to_string(&toml_path)
            .with_context(|| format!("failed to read {}", toml_path.display()))?;

        // Only add fields if they are not already present
        let mut additions = String::new();

        if !content.contains("description") {
            additions.push_str("\n# description = \"A Luau library for Roblox\"\n");
        }
        if !content.contains("license") {
            additions.push_str("# license = \"MIT\"\n");
        }
        if !content.contains("authors") {
            additions.push_str("# authors = [\"Your Name\"]\n");
        }
        if !content.contains("realm") || content.contains("# realm") {
            // Uncomment realm for libraries — they are typically shared
            let new_content = content.replace(
                "# realm = \"shared\"  # shared | server",
                "realm = \"shared\"",
            );
            if new_content != content {
                std::fs::write(&toml_path, &new_content)
                    .with_context(|| format!("failed to write {}", toml_path.display()))?;
            }
        }

        if !additions.is_empty() {
            // Append after the [package] section header
            let updated = std::fs::read_to_string(&toml_path)?;
            if let Some(pos) = updated.find("# realm") {
                // Find the end of that line
                let line_end = updated[pos..].find('\n').map(|i| pos + i + 1).unwrap_or(updated.len());
                let mut patched = String::with_capacity(updated.len() + additions.len());
                patched.push_str(&updated[..line_end]);
                patched.push_str(&additions);
                patched.push_str(&updated[line_end..]);
                std::fs::write(&toml_path, &patched)?;
            } else {
                // Just append at the end of the [package] section
                let mut f = std::fs::OpenOptions::new()
                    .append(true)
                    .open(&toml_path)?;
                use std::io::Write;
                write!(f, "{additions}")?;
            }
        }
    }

    Ok(())
}

/// Apply the "plugin" template on top of the default scaffold.
///
/// Replaces the standard Server/Client/Shared layout with a single
/// plugin entry point.
pub fn apply_plugin_template(root: &Path, project_name: &str) -> Result<()> {
    // Override default.project.json with plugin-specific tree
    let project_path = root.join("default.project.json");
    let project_json = serde_json::json!({
        "name": project_name,
        "projectId": uuid::Uuid::new_v4().to_string(),
        "tree": {
            "$className": "DataModel",
            "ServerStorage": {
                project_name: {
                    "$path": "src/Plugin"
                }
            }
        }
    });
    let formatted = serde_json::to_string_pretty(&project_json)?;
    // Overwrite (plugin template replaces default)
    std::fs::write(&project_path, &formatted)?;

    // Create plugin entry point
    let plugin_init = format!(
        r#"--!strict
-- {project_name} Plugin Entry Point

local toolbar = plugin:CreateToolbar("{project_name}")
local button = toolbar:CreateButton(
    "{project_name}",
    "Launch {project_name}",
    "rbxassetid://0"
)

local widgetInfo = DockWidgetPluginGuiInfo.new(
    Enum.InitialDockState.Float,
    false,
    false,
    400,
    300,
    200,
    150
)

local widget = plugin:CreateDockWidgetPluginGui("{project_name}", widgetInfo)
widget.Title = "{project_name}"

button.Click:Connect(function()
    widget.Enabled = not widget.Enabled
end)

print("[{project_name}] Plugin loaded")
"#
    );

    write_if_missing(
        &root.join("src/Plugin/init.server.luau"),
        &plugin_init,
    )?;

    // Update vsync.toml realm to server for plugins
    let toml_path = root.join("vsync.toml");
    if toml_path.exists() {
        let content = std::fs::read_to_string(&toml_path)?;
        let updated = content.replace(
            "# realm = \"shared\"  # shared | server",
            "realm = \"server\"",
        );
        if updated != content {
            std::fs::write(&toml_path, &updated)?;
        }
    }

    Ok(())
}
