//! Package publishing to a Wally-compatible registry.
//!
//! Validates metadata, runs pre-publish checks, builds a zip archive,
//! and POSTs it to the registry's publish endpoint.

use anyhow::{Context, Result, bail};
use std::io::Write;
use std::path::Path;

use crate::config::VsyncConfig;
use crate::credentials;

/// Build a zip archive of the project source suitable for publishing.
///
/// Includes all `.luau`, `.lua`, `.json`, `.toml`, and `.md` files at the
/// project root and under `src/`, excluding build artifacts and metadata.
pub fn build_package_zip(project_root: &Path) -> Result<Vec<u8>> {
    let buf: Vec<u8> = Vec::new();
    let cursor = std::io::Cursor::new(buf);
    let mut zip = zip::ZipWriter::new(cursor);

    let options = zip::write::SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated);

    // Collect files to include
    let mut files: Vec<std::path::PathBuf> = Vec::new();
    collect_publishable_files(project_root, project_root, &mut files)?;

    for file_path in &files {
        let rel = file_path
            .strip_prefix(project_root)
            .unwrap_or(file_path);
        let rel_str = rel.to_string_lossy().replace('\\', "/");

        let content = std::fs::read(file_path)
            .with_context(|| format!("failed to read {}", file_path.display()))?;

        zip.start_file(&rel_str, options)
            .with_context(|| format!("failed to add {} to zip", rel_str))?;
        zip.write_all(&content)?;
    }

    let cursor = zip.finish()?;
    Ok(cursor.into_inner())
}

/// Recursively collect files suitable for publishing.
#[allow(clippy::only_used_in_recursion)]
fn collect_publishable_files(
    root: &Path,
    dir: &Path,
    out: &mut Vec<std::path::PathBuf>,
) -> Result<()> {
    let entries = std::fs::read_dir(dir)
        .with_context(|| format!("failed to read directory {}", dir.display()))?;

    for entry in entries.flatten() {
        let path = entry.path();
        let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");

        if path.is_dir() {
            // Skip build artifacts and metadata directories
            if matches!(
                name,
                ".git"
                    | "node_modules"
                    | "Packages"
                    | "target"
                    | "dist"
                    | "build"
                    | ".vertigo-sync-state"
                    | ".vscode"
                    | ".github"
            ) || name.starts_with('.')
            {
                continue;
            }
            collect_publishable_files(root, &path, out)?;
        } else if path.is_file() {
            let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
            if matches!(
                ext,
                "luau" | "lua" | "json" | "toml" | "md" | "txt" | "yml" | "yaml"
            ) {
                out.push(path);
            }
        }
    }

    Ok(())
}

/// Validate that the config has all required fields for publishing.
pub fn validate_publish_metadata(config: &VsyncConfig) -> Result<()> {
    if config.package.name.is_empty() {
        bail!("package.name is required for publishing");
    }
    if config.package.version.is_empty() {
        bail!("package.version is required for publishing");
    }
    if config.package.realm.is_empty() {
        bail!("package.realm is required for publishing (shared | server)");
    }

    // Validate version is valid semver
    semver::Version::parse(&config.package.version)
        .with_context(|| {
            format!(
                "package.version '{}' is not valid semver",
                config.package.version
            )
        })?;

    // Validate name contains a scope separator
    if !config.package.name.contains('/') {
        bail!(
            "package.name '{}' must include a scope (e.g. 'myscope/mypackage')",
            config.package.name
        );
    }

    Ok(())
}

/// Publish a package to the registry.
///
/// Returns the published version string on success.
pub async fn publish_package(
    project_root: &Path,
    config: &VsyncConfig,
    registry_url: &str,
) -> Result<String> {
    // Enforce HTTPS for registry tokens (localhost exempt for dev)
    if !registry_url.starts_with("https://") && !registry_url.starts_with("http://127.0.0.1") && !registry_url.starts_with("http://localhost") {
        bail!("registry URL must use HTTPS to protect credentials: {registry_url}");
    }

    // Get auth token
    let token = credentials::get_token(registry_url)?
        .with_context(|| {
            format!(
                "not authenticated with {registry_url} — run `vsync login` first"
            )
        })?;

    // Build the package zip
    let zip_bytes = build_package_zip(project_root)?;
    let size_kb = zip_bytes.len() / 1024;

    // POST to the publish endpoint
    let url = format!("{}/v1/publish", registry_url.trim_end_matches('/'));
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(60))
        .build()
        .context("failed to build HTTP client")?;

    let resp = client
        .post(&url)
        .header("Authorization", format!("Bearer {token}"))
        .header("Wally-Version", "0.3.2")
        .header("Content-Type", "application/gzip")
        .body(zip_bytes)
        .send()
        .await
        .with_context(|| format!("failed to publish to {url}"))?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        bail!(
            "registry returned {status} when publishing: {body}"
        );
    }

    let version = config.package.version.clone();
    crate::output::success(&format!(
        "Published {}@{version} ({size_kb} KB)",
        config.package.name
    ));

    Ok(version)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{PackageConfig, VsyncConfig};

    #[test]
    fn validate_metadata_rejects_empty_name() {
        let config = VsyncConfig {
            package: PackageConfig {
                name: "".to_string(),
                version: "1.0.0".to_string(),
                realm: "shared".to_string(),
                ..Default::default()
            },
            ..Default::default()
        };
        let err = validate_publish_metadata(&config).unwrap_err();
        assert!(err.to_string().contains("name"));
    }

    #[test]
    fn validate_metadata_rejects_empty_version() {
        let config = VsyncConfig {
            package: PackageConfig {
                name: "scope/pkg".to_string(),
                version: "".to_string(),
                realm: "shared".to_string(),
                ..Default::default()
            },
            ..Default::default()
        };
        let err = validate_publish_metadata(&config).unwrap_err();
        assert!(err.to_string().contains("version"));
    }

    #[test]
    fn validate_metadata_rejects_empty_realm() {
        let config = VsyncConfig {
            package: PackageConfig {
                name: "scope/pkg".to_string(),
                version: "1.0.0".to_string(),
                realm: "".to_string(),
                ..Default::default()
            },
            ..Default::default()
        };
        let err = validate_publish_metadata(&config).unwrap_err();
        assert!(err.to_string().contains("realm"));
    }

    #[test]
    fn validate_metadata_rejects_no_scope() {
        let config = VsyncConfig {
            package: PackageConfig {
                name: "pkg".to_string(),
                version: "1.0.0".to_string(),
                realm: "shared".to_string(),
                ..Default::default()
            },
            ..Default::default()
        };
        let err = validate_publish_metadata(&config).unwrap_err();
        assert!(err.to_string().contains("scope"));
    }

    #[test]
    fn validate_metadata_rejects_invalid_semver() {
        let config = VsyncConfig {
            package: PackageConfig {
                name: "scope/pkg".to_string(),
                version: "not-a-version".to_string(),
                realm: "shared".to_string(),
                ..Default::default()
            },
            ..Default::default()
        };
        let err = validate_publish_metadata(&config).unwrap_err();
        assert!(err.to_string().contains("semver"));
    }

    #[test]
    fn validate_metadata_accepts_valid() {
        let config = VsyncConfig {
            package: PackageConfig {
                name: "scope/pkg".to_string(),
                version: "1.0.0".to_string(),
                realm: "shared".to_string(),
                ..Default::default()
            },
            ..Default::default()
        };
        assert!(validate_publish_metadata(&config).is_ok());
    }

    #[test]
    fn build_package_zip_creates_archive() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();

        std::fs::write(root.join("vsync.toml"), "[package]\nname = \"test\"").unwrap();
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("src/init.luau"), "return {}").unwrap();

        let zip_bytes = build_package_zip(root).unwrap();
        assert!(!zip_bytes.is_empty());

        // Verify the zip is valid
        let cursor = std::io::Cursor::new(&zip_bytes);
        let archive = zip::ZipArchive::new(cursor).unwrap();
        assert!(archive.len() >= 2); // vsync.toml + src/init.luau
    }
}
