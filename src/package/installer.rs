use anyhow::{Context, Result, bail};
use sha2::{Digest, Sha256};
use std::io::Read;
use std::path::Path;

use crate::config::{DependencySpec, VsyncConfig};
use super::cache;
use super::lockfile::{Lockfile, LockedPackage};
use super::registry::{IndexEntry, RegistryClient, parse_version_req};

/// Summary of what the install operation did.
pub struct InstallReport {
    pub installed: u32,
    pub cached: u32,
    pub total: u32,
}

/// Install all dependencies declared in the given config, writing `vsync.lock` and
/// populating the `Packages/` directory.
pub async fn install(project_root: &Path, config: &VsyncConfig) -> Result<InstallReport> {
    let lock_path = project_root.join("vsync.lock");
    let mut lockfile = Lockfile::load(&lock_path)?
        .unwrap_or_else(Lockfile::new);

    // Collect all dependency maps with their realm tag.
    let dep_groups: Vec<(&str, &std::collections::BTreeMap<String, DependencySpec>)> = vec![
        ("shared", &config.dependencies),
        ("server", &config.server_dependencies),
        ("dev", &config.dev_dependencies),
    ];

    let registry = RegistryClient::default_wally()?;
    let packages_dir = project_root.join(
        config
            .package
            .packages_dir
            .as_deref()
            .unwrap_or("Packages"),
    );

    // Validate packages-dir is inside the project root.
    std::fs::create_dir_all(&packages_dir)?;
    let canon_root = project_root.canonicalize()?;
    let canon_pkg = packages_dir.canonicalize()?;
    if !canon_pkg.starts_with(&canon_root) {
        bail!("packages-dir must be inside the project root");
    }

    let mut installed: u32 = 0;
    let mut cached: u32 = 0;
    let mut total: u32 = 0;

    for (realm, deps) in &dep_groups {
        for spec in (*deps).values() {
            total += 1;
            match spec {
                DependencySpec::Simple(version_spec) => {
                    let (scope, name, version_req) = parse_version_req(version_spec)
                        .with_context(|| format!("bad dependency spec: {version_spec}"))?;

                    // Fetch available versions from the registry.
                    let versions = registry
                        .fetch_versions(&scope, &name)
                        .await
                        .with_context(|| format!("failed to query {scope}/{name}"))?;

                    let entry = select_version(&versions, &version_req)
                        .with_context(|| {
                            format!("no version of {scope}/{name} satisfies {version_req}")
                        })?;

                    // Check if already locked at this version.
                    let full_name = format!("{scope}/{name}");
                    let already_locked = lockfile.packages.iter().any(|p| {
                        p.name == full_name && p.version == entry.version
                    });

                    if already_locked {
                        // Check cache.
                        let existing = lockfile
                            .packages
                            .iter()
                            .find(|p| p.name == full_name && p.version == entry.version)
                            .unwrap();
                        if cache::is_cached(&existing.checksum)? {
                            cached += 1;
                            continue;
                        }
                    }

                    // Download the package.
                    let bytes = registry
                        .download_package(&scope, &name, &entry.version)
                        .await
                        .with_context(|| {
                            format!("failed to download {scope}/{name}@{}", entry.version)
                        })?;

                    // Compute checksum.
                    let checksum = hex_sha256(&bytes);

                    // Cache the zip (offload blocking I/O).
                    let cache_path = cache::cached_package_path(&checksum)?;
                    let cache_bytes = bytes.clone();
                    let cache_path_clone = cache_path.clone();
                    tokio::task::spawn_blocking(move || std::fs::write(&cache_path_clone, &cache_bytes))
                        .await
                        .context("cache write task panicked")?
                        .with_context(|| {
                            format!("failed to write cache file {}", cache_path.display())
                        })?;

                    // Extract the zip into Packages/{scope}/{name}/.
                    let dest = packages_dir.join(&scope).join(&name);
                    // Validate dest is inside the packages directory.
                    std::fs::create_dir_all(&dest)?;
                    let canonical_packages = canon_pkg.clone();
                    let canonical_dest = dest.canonicalize().unwrap_or_else(|_| dest.clone());
                    if !canonical_dest.starts_with(&canonical_packages) {
                        bail!("package path escapes Packages directory");
                    }
                    let bytes_clone = bytes.clone();
                    let dest_clone = dest.clone();
                    tokio::task::spawn_blocking(move || extract_zip(&bytes_clone, &dest_clone))
                        .await
                        .context("extract task panicked")?
                        .with_context(|| {
                            format!(
                                "failed to extract {scope}/{name}@{} into {}",
                                entry.version,
                                dest.display()
                            )
                        })?;

                    // Upsert lockfile entry.
                    lockfile.packages.retain(|p| p.name != full_name);
                    lockfile.packages.push(LockedPackage {
                        name: full_name.clone(),
                        version: entry.version.clone(),
                        realm: realm.to_string(),
                        checksum,
                        source: "wally".to_string(),
                        dependencies: entry
                            .dependencies
                            .iter()
                            .map(|(k, v)| format!("{k}@{v}"))
                            .collect(),
                    });

                    installed += 1;
                }
                DependencySpec::Path { path } => {
                    let dep_path = project_root.join(path);
                    if !dep_path.exists() {
                        bail!(
                            "path dependency '{}' does not exist (resolved to {})",
                            path,
                            dep_path.display()
                        );
                    }
                    // Validate path doesn't escape project root.
                    let canon_dep = dep_path.canonicalize()?;
                    if !canon_dep.starts_with(&canon_root) {
                        bail!("path dependency '{}' must be inside the project root", path);
                    }
                    // Path deps are not cached or locked — they're used directly.
                }
                DependencySpec::Git { .. } => {
                    bail!("git dependencies are not yet supported (coming in v1.1)");
                }
                DependencySpec::Registry { .. } => {
                    bail!("custom registry dependencies are not yet supported (coming in v1.1)");
                }
            }
        }
    }

    // Warn about peer dependencies (not yet resolved).
    if !config.peer_dependencies.is_empty() {
        eprintln!(
            "warning: {} peer dependenc{} declared but not yet resolved (coming in v2)",
            config.peer_dependencies.len(),
            if config.peer_dependencies.len() == 1 { "y" } else { "ies" }
        );
    }

    // Write the updated lockfile (skip if no dependencies were processed).
    if !lockfile.packages.is_empty() || total > 0 {
        lockfile.save(&lock_path)?;
    }

    // Ensure the Packages directory exists.
    if !packages_dir.exists() {
        std::fs::create_dir_all(&packages_dir).with_context(|| {
            format!(
                "failed to create packages directory {}",
                packages_dir.display()
            )
        })?;
    }

    Ok(InstallReport {
        installed,
        cached,
        total,
    })
}

fn hex_sha256(data: &[u8]) -> String {
    use std::fmt::Write;
    let result = Sha256::new().chain_update(data).finalize();
    let mut s = String::with_capacity(64);
    for b in result.iter() {
        write!(s, "{b:02x}").unwrap();
    }
    s
}

/// Select the highest version from `versions` that satisfies the semver `req_str`.
fn select_version<'a>(versions: &'a [IndexEntry], req_str: &str) -> Result<&'a IndexEntry> {
    let req = semver::VersionReq::parse(req_str)
        .with_context(|| format!("invalid version requirement: {req_str}"))?;

    versions
        .iter()
        .filter_map(|entry| {
            let ver = semver::Version::parse(&entry.version).ok()?;
            if req.matches(&ver) {
                Some((ver, entry))
            } else {
                None
            }
        })
        .max_by(|(a, _), (b, _)| a.cmp(b))
        .map(|(_, entry)| entry)
        .with_context(|| format!("no version satisfies {req_str}"))
}

/// Extract a zip archive from `bytes` into `dest`, creating directories as needed.
fn extract_zip(bytes: &[u8], dest: &Path) -> Result<()> {
    const MAX_ZIP_ENTRIES: usize = 2_000;
    const MAX_FILE_BYTES: u64 = 50 * 1024 * 1024; // 50 MiB per file
    const MAX_TOTAL_BYTES: u64 = 200 * 1024 * 1024; // 200 MiB total

    let cursor = std::io::Cursor::new(bytes);
    let mut archive = zip::ZipArchive::new(cursor).context("failed to open zip archive")?;

    if archive.len() > MAX_ZIP_ENTRIES {
        bail!(
            "zip archive has {} entries, exceeding limit of {}",
            archive.len(),
            MAX_ZIP_ENTRIES
        );
    }

    if dest.exists() {
        std::fs::remove_dir_all(dest)
            .with_context(|| format!("failed to clean existing directory {}", dest.display()))?;
    }
    std::fs::create_dir_all(dest)
        .with_context(|| format!("failed to create directory {}", dest.display()))?;

    let mut total_written: u64 = 0;

    for i in 0..archive.len() {
        let mut file = archive.by_index(i).context("failed to read zip entry")?;
        let name = file.name().to_string();
        let file_size = file.size();

        if file_size > MAX_FILE_BYTES {
            bail!(
                "zip entry '{}' is {} bytes, exceeding {} byte limit",
                name,
                file_size,
                MAX_FILE_BYTES
            );
        }
        total_written += file_size;
        if total_written > MAX_TOTAL_BYTES {
            bail!("zip total uncompressed size exceeds {} byte limit", MAX_TOTAL_BYTES);
        }

        let Some(enclosed_name) = file.enclosed_name() else {
            // Skip entries with unsafe paths (path traversal, absolute paths, etc.)
            continue;
        };
        let out_path = dest.join(enclosed_name);

        if file.is_dir() {
            std::fs::create_dir_all(&out_path).with_context(|| {
                format!("failed to create directory {}", out_path.display())
            })?;
        } else {
            if let Some(parent) = out_path.parent() {
                if !parent.exists() {
                    std::fs::create_dir_all(parent).with_context(|| {
                        format!("failed to create directory {}", parent.display())
                    })?;
                }
            }
            let mut outfile = std::fs::File::create(&out_path).with_context(|| {
                format!("failed to create file {}", out_path.display())
            })?;
            let mut limited = (&mut file).take(MAX_FILE_BYTES);
            std::io::copy(&mut limited, &mut outfile).with_context(|| {
                format!("failed to write file {}", out_path.display())
            })?;
        }
    }

    Ok(())
}
