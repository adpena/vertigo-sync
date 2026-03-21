use anyhow::{Context, Result, bail};
use sha2::{Digest, Sha256};
use std::collections::HashSet;
use std::io::Read;
use std::path::Path;
use std::sync::Arc;
use std::time::Instant;

use crate::config::{DependencySpec, VsyncConfig};
use crate::output;
use super::cache;
use super::lockfile::{Lockfile, LockedPackage};
use super::registry::{IndexEntry, RegistryClient, parse_version_req};

/// Retry an async operation with exponential backoff.
async fn retry_async<F, Fut, T>(max_retries: u32, label: &str, f: F) -> Result<T>
where
    F: Fn() -> Fut,
    Fut: std::future::Future<Output = Result<T>>,
{
    let mut last_err = None;
    for attempt in 0..=max_retries {
        match f().await {
            Ok(val) => return Ok(val),
            Err(e) => {
                if attempt < max_retries {
                    let delay = std::time::Duration::from_millis(500 * 2u64.pow(attempt));
                    eprintln!("  \u{21bb} {label} failed (attempt {}), retrying in {delay:?}...", attempt + 1);
                    tokio::time::sleep(delay).await;
                }
                last_err = Some(e);
            }
        }
    }
    Err(last_err.unwrap())
}

/// Summary of what the install operation did.
pub struct InstallReport {
    pub installed: u32,
    pub cached: u32,
    pub total: u32,
    pub elapsed: std::time::Duration,
}

/// A package that needs to be resolved from the registry.
struct PendingResolve {
    scope: String,
    name: String,
    version_req: String,
    realm: String,
    full_name: String,
}

/// A resolved package that needs to be downloaded.
struct PendingDownload {
    scope: String,
    name: String,
    version: String,
    realm: String,
    full_name: String,
    entry: IndexEntry,
}

/// A downloaded package ready for extraction.
struct Downloaded {
    scope: String,
    name: String,
    version: String,
    realm: String,
    full_name: String,
    entry: IndexEntry,
    bytes: Vec<u8>,
    checksum: String,
}

/// Install all dependencies declared in the given config, writing `vsync.lock` and
/// populating the `Packages/` directory.
pub async fn install(project_root: &Path, config: &VsyncConfig) -> Result<InstallReport> {
    let start = Instant::now();
    let lock_path = project_root.join("vsync.lock");
    let mut lockfile = Lockfile::load(&lock_path)?
        .unwrap_or_else(Lockfile::new);

    // Collect all dependency maps with their realm tag.
    let dep_groups: Vec<(&str, &std::collections::BTreeMap<String, DependencySpec>)> = vec![
        ("shared", &config.dependencies),
        ("server", &config.server_dependencies),
        ("dev", &config.dev_dependencies),
    ];

    let registry = Arc::new(RegistryClient::default_wally()?);
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

    let mut total: u32 = 0;
    let mut cached: u32 = 0;
    let mut pending_resolve: Vec<PendingResolve> = Vec::new();

    // ── Phase 1: Collect and filter ──────────────────────────────────────
    // Check lockfile + cache BEFORE any network call.
    for (realm, deps) in &dep_groups {
        for spec in (*deps).values() {
            total += 1;
            match spec {
                DependencySpec::Simple(version_spec) => {
                    let (scope, name, version_req) = parse_version_req(version_spec)
                        .with_context(|| format!("bad dependency spec: {version_spec}"))?;

                    let full_name = format!("{scope}/{name}");

                    // Check lockfile + cache BEFORE any network call
                    if let Some(locked) = lockfile.packages.iter().find(|p| p.name == full_name) {
                        if cache::is_cached(&locked.checksum)? {
                            // Extract from cache, skip network entirely
                            cached += 1;
                            continue;
                        }
                    }

                    pending_resolve.push(PendingResolve {
                        scope,
                        name,
                        version_req,
                        realm: realm.to_string(),
                        full_name,
                    });
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

    // Fast path: everything is cached.
    if pending_resolve.is_empty() {
        // Warn about peer dependencies (not yet resolved).
        warn_peer_deps(config);

        // Write the updated lockfile (skip if no dependencies were processed).
        if !lockfile.packages.is_empty() || total > 0 {
            lockfile.save(&lock_path)?;
        }

        let elapsed = start.elapsed();
        if total > 0 {
            output::success(&format!(
                "{total} package{} up to date ({:.2}s)",
                if total == 1 { "" } else { "s" },
                elapsed.as_secs_f64()
            ));
        }

        return Ok(InstallReport {
            installed: 0,
            cached,
            total,
            elapsed,
        });
    }

    // ── Phase 2: Resolve concurrently (BFS for transitive deps) ─────────
    let mut resolved_names: HashSet<String> = HashSet::new();
    let mut pending_download: Vec<PendingDownload> = Vec::new();

    // Mark already-cached packages as resolved so we don't re-resolve them.
    for pkg in &lockfile.packages {
        if cache::is_cached(&pkg.checksum).unwrap_or(false) {
            resolved_names.insert(pkg.name.clone());
        }
    }

    // BFS loop: resolve current batch, then discover transitive deps, repeat.
    let mut current_batch = pending_resolve;
    while !current_batch.is_empty() {
        let resolve_count = current_batch.len();
        output::info(&format!(
            "Resolving {} package{}...",
            resolve_count,
            if resolve_count == 1 { "" } else { "s" }
        ));

        let mut resolve_set = tokio::task::JoinSet::new();
        let mut batch_resolved: Vec<PendingDownload> = Vec::with_capacity(resolve_count);

        for chunk in current_batch.chunks(8) {
            for pending in chunk {
                let reg = Arc::clone(&registry);
                let scope = pending.scope.clone();
                let name = pending.name.clone();
                let version_req = pending.version_req.clone();
                let realm = pending.realm.clone();
                let full_name = pending.full_name.clone();
                resolve_set.spawn(async move {
                    let versions = retry_async(3, &format!("fetch {scope}/{name}"), || {
                        let reg = reg.clone();
                        let scope = scope.clone();
                        let name = name.clone();
                        async move {
                            reg.fetch_versions(&scope, &name)
                                .await
                                .with_context(|| format!("failed to query {scope}/{name}"))
                        }
                    })
                    .await?;

                    let entry = select_version(&versions, &version_req)
                        .with_context(|| {
                            format!("no version of {scope}/{name} satisfies {version_req}")
                        })?
                        .clone();

                    Ok::<PendingDownload, anyhow::Error>(PendingDownload {
                        scope,
                        name,
                        version: entry.version.clone(),
                        realm,
                        full_name,
                        entry,
                    })
                });
            }
            while let Some(result) = resolve_set.join_next().await {
                let resolved = result.context("resolve task panicked")??;
                batch_resolved.push(resolved);
            }
        }

        // Collect transitive deps from newly resolved packages.
        let mut next_batch: Vec<PendingResolve> = Vec::new();
        for pd in &batch_resolved {
            resolved_names.insert(pd.full_name.clone());

            // Gather transitive deps from both dependencies and server_dependencies.
            let all_dep_specs = pd.entry.dependencies.values()
                .chain(pd.entry.server_dependencies.values());
            for dep_spec in all_dep_specs {
                if let Ok((scope, name, version_req)) = parse_version_req(dep_spec) {
                    let full_name = format!("{scope}/{name}");
                    if !resolved_names.contains(&full_name) {
                        resolved_names.insert(full_name.clone());
                        total += 1;
                        next_batch.push(PendingResolve {
                            scope,
                            name,
                            version_req,
                            realm: pd.realm.clone(),
                            full_name,
                        });
                    }
                }
            }
        }

        pending_download.extend(batch_resolved);
        current_batch = next_batch;
    }

    // ── Phase 3: Download concurrently ───────────────────────────────────
    let mut download_set = tokio::task::JoinSet::new();
    let mut downloaded: Vec<Downloaded> = Vec::with_capacity(pending_download.len());

    for chunk in pending_download.chunks(8) {
        for pd in chunk {
            let reg = Arc::clone(&registry);
            let scope = pd.scope.clone();
            let name = pd.name.clone();
            let version = pd.version.clone();
            let realm = pd.realm.clone();
            let full_name = pd.full_name.clone();
            let entry = pd.entry.clone();
            download_set.spawn(async move {
                let bytes = retry_async(3, &format!("download {scope}/{name}@{version}"), || {
                    let reg = reg.clone();
                    let scope = scope.clone();
                    let name = name.clone();
                    let version = version.clone();
                    async move {
                        reg.download_package(&scope, &name, &version)
                            .await
                            .with_context(|| {
                                format!("failed to download {scope}/{name}@{version}")
                            })
                    }
                })
                .await?;

                let checksum = hex_sha256(&bytes);
                let size_kb = bytes.len() / 1024;
                eprintln!("  \u{2193} {scope}/{name}@{version} ({size_kb} KB)");

                Ok::<Downloaded, anyhow::Error>(Downloaded {
                    scope,
                    name,
                    version,
                    realm,
                    full_name,
                    entry,
                    bytes,
                    checksum,
                })
            });
        }
        while let Some(result) = download_set.join_next().await {
            let dl = result.context("download task panicked")??;
            downloaded.push(dl);
        }
    }

    // ── Phase 4: Extract ─────────────────────────────────────────────────
    let mut installed: u32 = 0;

    for dl in downloaded {
        // Cache the zip — pass ownership of bytes to avoid cloning.
        let cache_path = cache::cached_package_path(&dl.checksum)?;
        let cache_path_for_err = cache_path.clone();
        let bytes = dl.bytes; // take ownership

        // Write to cache, then read back for extraction to avoid double clone.
        // Actually, we can write and extract from the same bytes by splitting:
        // 1) write cache (needs &[u8])
        // 2) extract (needs &[u8])
        // Both only need a reference, no clone needed.
        let dest = packages_dir.join(&dl.scope).join(&dl.name);
        // Validate dest is inside the packages directory.
        std::fs::create_dir_all(&dest)?;
        let canonical_packages = canon_pkg.clone();
        let canonical_dest = dest.canonicalize().unwrap_or_else(|_| dest.clone());
        if !canonical_dest.starts_with(&canonical_packages) {
            bail!("package path escapes Packages directory");
        }

        let dest_clone = dest.clone();
        let scope = dl.scope.clone();
        let name = dl.name.clone();
        let version = dl.version.clone();

        tokio::task::spawn_blocking(move || -> Result<()> {
            // Write to cache
            std::fs::write(&cache_path, &bytes)
                .with_context(|| {
                    format!("failed to write cache file {}", cache_path_for_err.display())
                })?;
            // Extract from same bytes — no extra clone
            extract_zip(&bytes, &dest_clone)
                .with_context(|| {
                    format!(
                        "failed to extract {scope}/{name}@{version} into {}",
                        dest_clone.display()
                    )
                })?;
            Ok(())
        })
        .await
        .context("extract task panicked")??;

        // Upsert lockfile entry.
        let full_name = dl.full_name;
        lockfile.packages.retain(|p| p.name != full_name);
        lockfile.packages.push(LockedPackage {
            name: full_name,
            version: dl.version.clone(),
            realm: dl.realm,
            checksum: dl.checksum,
            source: "wally".to_string(),
            dependencies: dl
                .entry
                .dependencies
                .iter()
                .map(|(k, v)| format!("{k}@{v}"))
                .collect(),
        });

        // Incremental save: persist lockfile after each successful extraction
        // so that partial failures don't lose progress.
        lockfile.save(&lock_path)?;

        installed += 1;
    }

    // ── Phase 5: Write lockfile ──────────────────────────────────────────
    // Warn about peer dependencies (not yet resolved).
    warn_peer_deps(config);

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

    let elapsed = start.elapsed();
    output::success(&format!(
        "{} package{} installed in {:.1}s",
        installed,
        if installed == 1 { "" } else { "s" },
        elapsed.as_secs_f64()
    ));

    Ok(InstallReport {
        installed,
        cached,
        total,
        elapsed,
    })
}

fn warn_peer_deps(config: &VsyncConfig) {
    if !config.peer_dependencies.is_empty() {
        eprintln!(
            "warning: {} peer dependenc{} declared but not yet resolved (coming in v2)",
            config.peer_dependencies.len(),
            if config.peer_dependencies.len() == 1 { "y" } else { "ies" }
        );
    }
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
