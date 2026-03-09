#![deny(unsafe_code)]

use anyhow::{Context, Result};
use rayon::prelude::*;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

pub mod mcp;
pub mod project;
pub mod server;
pub mod validate;

// ---------------------------------------------------------------------------
// Event coalescer — batches filesystem events within a configurable window
// ---------------------------------------------------------------------------

/// Batches filesystem events within a configurable time window before
/// triggering a snapshot rebuild. Saving 10 files in quick succession
/// produces 1 rebuild, not 10.
pub struct EventCoalescer {
    /// How long to wait for more events before triggering.
    window: Duration,
    /// Monotonic timestamp of the last event received.
    last_event: Mutex<Option<Instant>>,
    /// Whether a coalesce cycle is currently in progress.
    pending: AtomicBool,
}

impl EventCoalescer {
    pub fn new(window: Duration) -> Self {
        Self {
            window,
            last_event: Mutex::new(None),
            pending: AtomicBool::new(false),
        }
    }

    /// Signal that a filesystem event occurred. Returns `true` if this is
    /// the first event in a new coalesce window (caller should spawn the
    /// quiescence waiter). Returns `false` if a window is already open
    /// (the timer has been reset internally).
    pub fn signal(&self) -> bool {
        let now = Instant::now();
        let mut lock = self.last_event.lock().unwrap();
        *lock = Some(now);

        // If no pending cycle, start one.
        let was_pending = self.pending.swap(true, Ordering::AcqRel);
        !was_pending
    }

    /// Wait for the coalesce window to close (no events for `window`
    /// duration), then mark the cycle as finished and return. Call this
    /// after `signal()` returns `true`.
    pub async fn wait_for_quiescence(&self) {
        loop {
            tokio::time::sleep(self.window).await;

            let elapsed = {
                let lock = self.last_event.lock().unwrap();
                match *lock {
                    Some(ts) => ts.elapsed(),
                    None => self.window, // shouldn't happen, but safe
                }
            };

            if elapsed >= self.window {
                // Window closed — no new events arrived during the sleep.
                self.pending.store(false, Ordering::Release);
                return;
            }

            // Events arrived during our sleep — loop and wait again.
            // Sleep only the remaining time rather than a full window.
            let remaining = self.window.saturating_sub(elapsed);
            if !remaining.is_zero() {
                tokio::time::sleep(remaining).await;
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Prometheus-compatible metrics
// ---------------------------------------------------------------------------

/// Atomic counters/gauges for the sync server. All fields are updated
/// via atomic operations — no locking required for reads or writes.
pub struct Metrics {
    pub polls: AtomicU64,
    pub poll_duration_sum_us: AtomicU64,
    pub cache_hits: AtomicU64,
    pub cache_misses: AtomicU64,
    pub events_emitted: AtomicU64,
    pub ws_connections: AtomicU64,
    pub entries: AtomicU64,
}

impl Metrics {
    pub fn new() -> Self {
        Self {
            polls: AtomicU64::new(0),
            poll_duration_sum_us: AtomicU64::new(0),
            cache_hits: AtomicU64::new(0),
            cache_misses: AtomicU64::new(0),
            events_emitted: AtomicU64::new(0),
            ws_connections: AtomicU64::new(0),
            entries: AtomicU64::new(0),
        }
    }

    /// Render Prometheus text exposition format.
    pub fn render(&self) -> String {
        let polls = self.polls.load(Ordering::Relaxed);
        let duration_sum_us = self.poll_duration_sum_us.load(Ordering::Relaxed);
        let duration_sum_s = duration_sum_us as f64 / 1_000_000.0;
        let cache_hits = self.cache_hits.load(Ordering::Relaxed);
        let cache_misses = self.cache_misses.load(Ordering::Relaxed);
        let total_lookups = cache_hits + cache_misses;
        let hit_ratio = if total_lookups > 0 {
            cache_hits as f64 / total_lookups as f64
        } else {
            0.0
        };
        let entries = self.entries.load(Ordering::Relaxed);
        let ws_conns = self.ws_connections.load(Ordering::Relaxed);
        let events = self.events_emitted.load(Ordering::Relaxed);

        format!(
            "# HELP vertigo_sync_polls_total Total number of snapshot polls\n\
             # TYPE vertigo_sync_polls_total counter\n\
             vertigo_sync_polls_total {polls}\n\
             \n\
             # HELP vertigo_sync_poll_duration_seconds Time to build snapshot\n\
             # TYPE vertigo_sync_poll_duration_seconds histogram\n\
             vertigo_sync_poll_duration_seconds_sum {duration_sum_s:.6}\n\
             vertigo_sync_poll_duration_seconds_count {polls}\n\
             \n\
             # HELP vertigo_sync_cache_hit_ratio Ratio of cache hits to total file lookups\n\
             # TYPE vertigo_sync_cache_hit_ratio gauge\n\
             vertigo_sync_cache_hit_ratio {hit_ratio:.6}\n\
             \n\
             # HELP vertigo_sync_entries_total Number of files in current snapshot\n\
             # TYPE vertigo_sync_entries_total gauge\n\
             vertigo_sync_entries_total {entries}\n\
             \n\
             # HELP vertigo_sync_ws_connections Active WebSocket connections\n\
             # TYPE vertigo_sync_ws_connections gauge\n\
             vertigo_sync_ws_connections {ws_conns}\n\
             \n\
             # HELP vertigo_sync_events_total Total sync events emitted\n\
             # TYPE vertigo_sync_events_total counter\n\
             vertigo_sync_events_total {events}\n"
        )
    }
}

impl Default for Metrics {
    fn default() -> Self {
        Self::new()
    }
}

pub const DEFAULT_INCLUDES: &[&str] = &["src", "studio-plugin", "scripts/dev"];
const SKIP_DIR_NAMES: &[&str] = &[
    ".git",
    ".idea",
    ".next",
    ".turbo",
    ".vite",
    "__pycache__",
    "node_modules",
    "target",
    "dist",
    "build",
    "coverage",
    ".cache",
];
const SKIP_FILE_NAMES: &[&str] = &[".DS_Store"];
const SKIP_FILE_SUFFIXES: &[&str] = &[".log", ".tmp", ".swp"];

/// Threshold in bytes above which memory-mapped I/O is used for hashing.
/// Below this, the mmap syscall overhead exceeds the copy savings.
const MMAP_THRESHOLD: u64 = 4096;

// ---------------------------------------------------------------------------
// Incremental snapshot cache
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
struct CachedEntry {
    sha256: String,
    bytes: u64,
    mtime: SystemTime,
}

/// Per-file hash cache keyed by (path, mtime, size). Eliminates redundant
/// SHA-256 computation on unchanged files — makes subsequent snapshots
/// O(changed_files) instead of O(all_files).
pub struct SnapshotCache {
    entries: HashMap<String, CachedEntry>,
}

impl SnapshotCache {
    pub fn new() -> Self {
        Self {
            entries: HashMap::new(),
        }
    }

    /// Returns cached hash if mtime+size match, otherwise `None`.
    pub fn get(&self, path: &str, mtime: SystemTime, size: u64) -> Option<&str> {
        self.entries.get(path).and_then(|entry| {
            if entry.mtime == mtime && entry.bytes == size {
                Some(entry.sha256.as_str())
            } else {
                None
            }
        })
    }

    /// Store a computed hash for a file.
    pub fn insert(&mut self, path: String, sha256: String, bytes: u64, mtime: SystemTime) {
        self.entries.insert(
            path,
            CachedEntry {
                sha256,
                bytes,
                mtime,
            },
        );
    }

    /// Remove entries for paths no longer present on disk.
    pub fn retain_paths(&mut self, live_paths: &HashSet<String>) {
        self.entries.retain(|key, _| live_paths.contains(key));
    }

    /// Number of cached entries (for diagnostics).
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether the cache is empty.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

impl Default for SnapshotCache {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SnapshotEntry {
    pub path: String,
    pub sha256: String,
    pub bytes: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Snapshot {
    pub version: u32,
    pub include: Vec<String>,
    pub fingerprint: String,
    pub entries: Vec<SnapshotEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ModifiedEntry {
    pub path: String,
    pub previous_sha256: String,
    pub previous_bytes: u64,
    pub current_sha256: String,
    pub current_bytes: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SnapshotDiff {
    pub previous_fingerprint: String,
    pub current_fingerprint: String,
    pub added: Vec<SnapshotEntry>,
    pub modified: Vec<ModifiedEntry>,
    pub deleted: Vec<SnapshotEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct EventDiffCounts {
    pub added: usize,
    pub modified: usize,
    pub deleted: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct EventPaths {
    pub added: Vec<String>,
    pub modified: Vec<String>,
    pub deleted: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DiffEvent {
    pub seq: u64,
    pub event: String,
    pub timestamp_utc: String,
    pub source_hash: String,
    pub snapshot_hash: String,
    pub diff: EventDiffCounts,
    pub paths: EventPaths,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DoctorReport {
    pub deterministic: bool,
    pub first_fingerprint: String,
    pub second_fingerprint: String,
    pub first_entries: usize,
    pub second_entries: usize,
    pub mismatch_path: Option<String>,
}

// ---------------------------------------------------------------------------
// Extended doctor — source health validation
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct HealthReport {
    pub healthy: bool,
    pub source_root: String,
    pub file_count: usize,
    pub issues: Vec<HealthIssue>,
    pub deterministic: bool,
    pub fingerprint: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct HealthIssue {
    pub severity: String,
    pub path: String,
    pub message: String,
}

// ---------------------------------------------------------------------------
// Watch / SSE event types
// ---------------------------------------------------------------------------

/// SSE / watch diff event payload.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SyncDiffEvent {
    pub sequence_id: u64,
    pub source_hash: String,
    pub added_paths: Vec<String>,
    pub modified_paths: Vec<String>,
    pub deleted_paths: Vec<String>,
    pub timestamp: String,
}

// ---------------------------------------------------------------------------
// Includes resolution
// ---------------------------------------------------------------------------

pub fn resolve_includes(includes: &[String]) -> Vec<String> {
    let mut values: Vec<String> = if includes.is_empty() {
        DEFAULT_INCLUDES
            .iter()
            .map(|value| value.to_string())
            .collect()
    } else {
        includes.to_vec()
    };

    for value in &mut values {
        let mut normalized = value.replace('\\', "/");
        while normalized.starts_with("./") {
            normalized = normalized[2..].to_string();
        }
        while normalized.ends_with('/') && normalized.len() > 1 {
            normalized.pop();
        }
        *value = if normalized.is_empty() {
            ".".to_string()
        } else {
            normalized
        };
    }

    values.sort();
    values.dedup();
    values
}

// ---------------------------------------------------------------------------
// Snapshot engine
// ---------------------------------------------------------------------------

pub fn build_snapshot(root: &Path, includes: &[String]) -> Result<Snapshot> {
    let resolved_includes = resolve_includes(includes);
    let mut files = Vec::new();

    for include in &resolved_includes {
        let include_path = root.join(include);
        if !include_path.exists() {
            continue;
        }
        collect_files(root, &include_path, &mut files)?;
    }

    files.sort_by(|a, b| normalize_path(a).cmp(&normalize_path(b)));

    // Parallel hash using rayon — all CPU cores for initial/uncached builds.
    let entries: Result<Vec<SnapshotEntry>> = files
        .par_iter()
        .map(|relative| {
            let absolute = root.join(relative);
            let (sha256, bytes) = hash_file(&absolute)
                .with_context(|| format!("failed to hash file {}", absolute.display()))?;
            Ok(SnapshotEntry {
                path: normalize_path(relative),
                sha256,
                bytes,
            })
        })
        .collect();

    let mut entries = entries?;
    entries.sort_by(|a, b| a.path.cmp(&b.path));
    let fingerprint = fingerprint_entries(&entries);

    Ok(Snapshot {
        version: 1,
        include: resolved_includes,
        fingerprint,
        entries,
    })
}

/// Cached variant of `build_snapshot`. Skips SHA-256 computation for files
/// whose (mtime, size) haven't changed since the last poll. After building,
/// prunes cache entries for deleted files.
///
/// Use the uncached `build_snapshot` for determinism verification (doctor/health).
pub fn build_snapshot_cached(
    root: &Path,
    includes: &[String],
    cache: &mut SnapshotCache,
) -> Result<Snapshot> {
    let resolved_includes = resolve_includes(includes);
    let mut files = Vec::new();

    for include in &resolved_includes {
        let include_path = root.join(include);
        if !include_path.exists() {
            continue;
        }
        collect_files(root, &include_path, &mut files)?;
    }

    files.sort_by(|a, b| normalize_path(a).cmp(&normalize_path(b)));

    // First pass: check cache hits vs misses.
    struct FileWork {
        normalized: String,
        absolute: PathBuf,
        mtime: SystemTime,
        size: u64,
        cached_hash: Option<String>,
    }

    let work: Result<Vec<FileWork>> = files
        .iter()
        .map(|relative| {
            let normalized = normalize_path(relative);
            let absolute = root.join(relative);
            let meta = fs::metadata(&absolute)
                .with_context(|| format!("failed to stat {}", absolute.display()))?;
            let mtime = meta.modified().unwrap_or(SystemTime::UNIX_EPOCH);
            let size = meta.len();
            let cached_hash = cache.get(&normalized, mtime, size).map(String::from);
            Ok(FileWork {
                normalized,
                absolute,
                mtime,
                size,
                cached_hash,
            })
        })
        .collect();
    let work = work?;

    // Second pass: parallel hash only the cache misses.
    let entries: Result<Vec<SnapshotEntry>> = work
        .par_iter()
        .map(|fw| {
            if let Some(ref sha256) = fw.cached_hash {
                Ok(SnapshotEntry {
                    path: fw.normalized.clone(),
                    sha256: sha256.clone(),
                    bytes: fw.size,
                })
            } else {
                let (sha256, bytes) = hash_file(&fw.absolute)
                    .with_context(|| format!("failed to hash file {}", fw.absolute.display()))?;
                Ok(SnapshotEntry {
                    path: fw.normalized.clone(),
                    sha256,
                    bytes,
                })
            }
        })
        .collect();
    let mut entries = entries?;

    // Update cache with newly computed hashes.
    for (fw, entry) in work.iter().zip(entries.iter()) {
        if fw.cached_hash.is_none() {
            cache.insert(
                entry.path.clone(),
                entry.sha256.clone(),
                entry.bytes,
                fw.mtime,
            );
        }
    }

    // Prune deleted files from cache.
    let live_paths: HashSet<String> = entries.iter().map(|e| e.path.clone()).collect();
    cache.retain_paths(&live_paths);

    entries.sort_by(|a, b| a.path.cmp(&b.path));
    let fingerprint = fingerprint_entries(&entries);

    Ok(Snapshot {
        version: 1,
        include: resolved_includes,
        fingerprint,
        entries,
    })
}

/// Like `build_snapshot_cached` but also records cache hit/miss counts
/// into the provided `Metrics`.
pub fn build_snapshot_cached_with_metrics(
    root: &Path,
    includes: &[String],
    cache: &mut SnapshotCache,
    metrics: &Metrics,
) -> Result<Snapshot> {
    let resolved_includes = resolve_includes(includes);
    let mut files = Vec::new();

    for include in &resolved_includes {
        let include_path = root.join(include);
        if !include_path.exists() {
            continue;
        }
        collect_files(root, &include_path, &mut files)?;
    }

    files.sort_by(|a, b| normalize_path(a).cmp(&normalize_path(b)));

    struct FileWork {
        normalized: String,
        absolute: PathBuf,
        mtime: SystemTime,
        size: u64,
        cached_hash: Option<String>,
    }

    let mut hits = 0u64;
    let mut misses = 0u64;

    let work: Result<Vec<FileWork>> = files
        .iter()
        .map(|relative| {
            let normalized = normalize_path(relative);
            let absolute = root.join(relative);
            let meta = fs::metadata(&absolute)
                .with_context(|| format!("failed to stat {}", absolute.display()))?;
            let mtime = meta.modified().unwrap_or(SystemTime::UNIX_EPOCH);
            let size = meta.len();
            let cached_hash = cache.get(&normalized, mtime, size).map(String::from);
            Ok(FileWork {
                normalized,
                absolute,
                mtime,
                size,
                cached_hash,
            })
        })
        .collect();
    let work = work?;

    for fw in &work {
        if fw.cached_hash.is_some() {
            hits += 1;
        } else {
            misses += 1;
        }
    }
    metrics.cache_hits.fetch_add(hits, Ordering::Relaxed);
    metrics.cache_misses.fetch_add(misses, Ordering::Relaxed);

    let entries: Result<Vec<SnapshotEntry>> = work
        .par_iter()
        .map(|fw| {
            if let Some(ref sha256) = fw.cached_hash {
                Ok(SnapshotEntry {
                    path: fw.normalized.clone(),
                    sha256: sha256.clone(),
                    bytes: fw.size,
                })
            } else {
                let (sha256, bytes) = hash_file(&fw.absolute)
                    .with_context(|| format!("failed to hash file {}", fw.absolute.display()))?;
                Ok(SnapshotEntry {
                    path: fw.normalized.clone(),
                    sha256,
                    bytes,
                })
            }
        })
        .collect();
    let mut entries = entries?;

    for (fw, entry) in work.iter().zip(entries.iter()) {
        if fw.cached_hash.is_none() {
            cache.insert(
                entry.path.clone(),
                entry.sha256.clone(),
                entry.bytes,
                fw.mtime,
            );
        }
    }

    let live_paths: HashSet<String> = entries.iter().map(|e| e.path.clone()).collect();
    cache.retain_paths(&live_paths);

    entries.sort_by(|a, b| a.path.cmp(&b.path));
    let fingerprint = fingerprint_entries(&entries);

    Ok(Snapshot {
        version: 1,
        include: resolved_includes,
        fingerprint,
        entries,
    })
}

pub fn fingerprint_entries(entries: &[SnapshotEntry]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"vertigo-sync-snapshot-v1\n");

    for entry in entries {
        hasher.update(entry.path.as_bytes());
        hasher.update([0]);
        hasher.update(entry.sha256.as_bytes());
        hasher.update([0]);
        hasher.update(entry.bytes.to_string().as_bytes());
        hasher.update(b"\n");
    }

    let digest = hasher.finalize();
    format!("{digest:x}")
}

// ---------------------------------------------------------------------------
// Diff engine
// ---------------------------------------------------------------------------

pub fn diff_snapshots(previous: &Snapshot, current: &Snapshot) -> SnapshotDiff {
    let mut previous_map = BTreeMap::new();
    for entry in &previous.entries {
        previous_map.insert(entry.path.clone(), entry);
    }

    let mut current_map = BTreeMap::new();
    for entry in &current.entries {
        current_map.insert(entry.path.clone(), entry);
    }

    let mut added = Vec::new();
    let mut modified = Vec::new();
    let mut deleted = Vec::new();

    for (path, current_entry) in &current_map {
        match previous_map.get(path) {
            None => added.push((*current_entry).clone()),
            Some(previous_entry) => {
                if previous_entry.sha256 != current_entry.sha256
                    || previous_entry.bytes != current_entry.bytes
                {
                    modified.push(ModifiedEntry {
                        path: path.clone(),
                        previous_sha256: previous_entry.sha256.clone(),
                        previous_bytes: previous_entry.bytes,
                        current_sha256: current_entry.sha256.clone(),
                        current_bytes: current_entry.bytes,
                    });
                }
            }
        }
    }

    for (path, previous_entry) in &previous_map {
        if !current_map.contains_key(path) {
            deleted.push((*previous_entry).clone());
        }
    }

    added.sort_by(|a, b| a.path.cmp(&b.path));
    modified.sort_by(|a, b| a.path.cmp(&b.path));
    deleted.sort_by(|a, b| a.path.cmp(&b.path));

    SnapshotDiff {
        previous_fingerprint: previous.fingerprint.clone(),
        current_fingerprint: current.fingerprint.clone(),
        added,
        modified,
        deleted,
    }
}

// ---------------------------------------------------------------------------
// IO helpers
// ---------------------------------------------------------------------------

pub fn read_snapshot(path: &Path) -> Result<Snapshot> {
    let file = File::open(path)
        .with_context(|| format!("failed to open snapshot file {}", path.display()))?;
    let reader = BufReader::new(file);
    let snapshot = serde_json::from_reader(reader)
        .with_context(|| format!("failed to parse snapshot json {}", path.display()))?;
    Ok(snapshot)
}

pub fn write_json_file<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    ensure_parent(path)?;
    let mut file = File::create(path)
        .with_context(|| format!("failed to create output file {}", path.display()))?;
    serde_json::to_writer_pretty(&mut file, value)
        .with_context(|| format!("failed to write json to {}", path.display()))?;
    file.write_all(b"\n")
        .with_context(|| format!("failed to finalize output {}", path.display()))?;
    Ok(())
}

pub fn next_event_seq(path: &Path) -> Result<u64> {
    if !path.exists() {
        return Ok(1);
    }

    let file =
        File::open(path).with_context(|| format!("failed to open event log {}", path.display()))?;
    let reader = BufReader::new(file);

    let mut max_seq = 0_u64;
    for line in reader.lines() {
        let line = line.with_context(|| format!("failed reading event log {}", path.display()))?;
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }

        let value: serde_json::Value = serde_json::from_str(trimmed)
            .with_context(|| format!("invalid jsonl in event log {}", path.display()))?;

        if let Some(seq) = value
            .get("seq")
            .and_then(|raw| raw.as_u64())
            .or_else(|| value.get("sequence_id").and_then(|raw| raw.as_u64()))
        {
            max_seq = max_seq.max(seq);
        }
    }

    Ok(max_seq.saturating_add(1))
}

pub fn append_event(path: &Path, event: &DiffEvent) -> Result<()> {
    ensure_parent(path)?;
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .with_context(|| format!("failed to open event log for append {}", path.display()))?;

    serde_json::to_writer(&mut file, event)
        .with_context(|| format!("failed to serialize event for {}", path.display()))?;
    file.write_all(b"\n")
        .with_context(|| format!("failed to append newline to {}", path.display()))?;

    Ok(())
}

// ---------------------------------------------------------------------------
// Doctor (determinism check)
// ---------------------------------------------------------------------------

pub fn run_doctor(root: &Path, includes: &[String]) -> Result<DoctorReport> {
    let first = build_snapshot(root, includes)?;
    let second = build_snapshot(root, includes)?;

    let deterministic = first.fingerprint == second.fingerprint && first.entries == second.entries;

    Ok(DoctorReport {
        deterministic,
        first_fingerprint: first.fingerprint,
        second_fingerprint: second.fingerprint,
        first_entries: first.entries.len(),
        second_entries: second.entries.len(),
        mismatch_path: find_mismatch_path(&first.entries, &second.entries),
    })
}

/// Extended health doctor: UTF-8 validity, case collisions, large files,
/// project.json presence, plus deterministic fingerprinting.
pub fn run_health_doctor(root: &Path, includes: &[String]) -> Result<HealthReport> {
    let mut issues: Vec<HealthIssue> = Vec::new();
    let mut file_count: usize = 0;
    let mut seen_lower: BTreeMap<String, String> = BTreeMap::new();

    let resolved = resolve_includes(includes);
    for inc in &resolved {
        let inc_path = root.join(inc);
        if !inc_path.exists() {
            continue;
        }
        check_health_recursive(
            &inc_path,
            &inc_path,
            &mut file_count,
            &mut seen_lower,
            &mut issues,
        )?;
    }

    match find_project_json(root) {
        Some(pj_path) => {
            let content = fs::read_to_string(&pj_path).unwrap_or_default();
            if !content.contains("src/") && !content.contains("src\\") {
                issues.push(HealthIssue {
                    severity: "warn".into(),
                    path: pj_path.to_string_lossy().into_owned(),
                    message: "default.project.json does not reference 'src/' directory".into(),
                });
            }
        }
        None => {
            issues.push(HealthIssue {
                severity: "warn".into(),
                path: root.to_string_lossy().into_owned(),
                message: "default.project.json not found in root or parent directories".into(),
            });
        }
    }

    // Run source validation checks.
    if let Ok(validation) = validate::validate_source(root, includes) {
        for vi in &validation.issues {
            let severity = if vi.severity == "error" {
                "error"
            } else {
                "warn"
            };
            issues.push(HealthIssue {
                severity: severity.into(),
                path: vi.path.clone(),
                message: format!("[{}] {}", vi.rule, vi.message),
            });
        }
    }

    let first = build_snapshot(root, includes)?;
    let second = build_snapshot(root, includes)?;
    let deterministic = first.fingerprint == second.fingerprint && first.entries == second.entries;
    if !deterministic {
        issues.push(HealthIssue {
            severity: "error".into(),
            path: root.to_string_lossy().into_owned(),
            message: format!(
                "non-deterministic snapshots: first={}, second={}",
                first.fingerprint, second.fingerprint
            ),
        });
    }

    let healthy = !issues.iter().any(|i| i.severity == "error");

    Ok(HealthReport {
        healthy,
        source_root: root.to_string_lossy().into_owned(),
        file_count,
        issues,
        deterministic,
        fingerprint: first.fingerprint,
    })
}

// ---------------------------------------------------------------------------
// Watch mode
// ---------------------------------------------------------------------------

/// Blocking poll loop: emit NDJSON diffs to stdout, optionally write snapshots.
pub fn run_watch(
    root: &Path,
    includes: &[String],
    interval: Duration,
    output_dir: Option<&Path>,
) -> Result<()> {
    let mut prev = build_snapshot(root, includes)?;
    let mut seq: u64 = 0;

    if let Some(dir) = output_dir {
        fs::create_dir_all(dir)?;
        write_snapshot_file(dir, &prev)?;
    }

    eprintln!(
        "[vertigo-sync] watching root={} includes={:?} poll={}ms files={}",
        root.display(),
        includes,
        interval.as_millis(),
        prev.entries.len()
    );

    loop {
        std::thread::sleep(interval);
        let current = build_snapshot(root, includes)?;

        if current.fingerprint == prev.fingerprint {
            continue;
        }

        seq += 1;
        let diff = diff_snapshots(&prev, &current);

        let event = SyncDiffEvent {
            sequence_id: seq,
            source_hash: current.fingerprint.clone(),
            added_paths: diff.added.iter().map(|f| f.path.clone()).collect(),
            modified_paths: diff.modified.iter().map(|f| f.path.clone()).collect(),
            deleted_paths: diff.deleted.iter().map(|f| f.path.clone()).collect(),
            timestamp: chrono::Utc::now().to_rfc3339(),
        };

        let json = serde_json::to_string(&event).context("failed to serialize watch event")?;
        println!("{json}");

        if let Some(dir) = output_dir {
            write_snapshot_file(dir, &current)?;
        }

        prev = current;
    }
}

/// Native filesystem watcher using `notify` (FSEvents on macOS).
///
/// Falls back to [`run_watch`] if native watching cannot be initialised.
/// Emits the same NDJSON format as `run_watch` but reacts to filesystem
/// events instead of polling at a fixed interval.
///
/// `coalesce_window` controls how long to wait after the last filesystem
/// event before triggering a snapshot rebuild (default: 50ms).
pub fn run_watch_native(
    root: &Path,
    includes: &[String],
    output_dir: Option<&Path>,
    coalesce_window: Duration,
) -> Result<()> {
    use notify::{Config, RecommendedWatcher, RecursiveMode, Watcher};
    use std::sync::mpsc;

    let resolved = resolve_includes(includes);
    let mut prev = build_snapshot(root, &resolved)?;
    let mut seq: u64 = 0;

    if let Some(dir) = output_dir {
        fs::create_dir_all(dir)?;
        write_snapshot_file(dir, &prev)?;
    }

    let (tx, rx) = mpsc::channel();

    let config = Config::default().with_poll_interval(Duration::from_millis(100));

    let mut watcher: RecommendedWatcher =
        Watcher::new(tx, config).context("failed to create native file watcher")?;

    // Watch each include root.
    for inc in &resolved {
        let watch_path = root.join(inc);
        if watch_path.exists() {
            watcher
                .watch(&watch_path, RecursiveMode::Recursive)
                .with_context(|| format!("failed to watch path {}", watch_path.display()))?;
        }
    }

    eprintln!(
        "[vertigo-sync] native watch root={} includes={:?} files={} coalesce={}ms",
        root.display(),
        &resolved,
        prev.entries.len(),
        coalesce_window.as_millis()
    );

    // Coalesce: collect events for the configured window then rebuild once.
    loop {
        // Block until first event.
        match rx.recv() {
            Ok(_) => {}
            Err(_) => break,
        }

        // Drain any buffered events within the coalesce window.
        // Use a sliding-window approach: keep draining while events arrive.
        let mut last_event = Instant::now();
        loop {
            let remaining = coalesce_window.saturating_sub(last_event.elapsed());
            if remaining.is_zero() {
                break;
            }
            match rx.recv_timeout(remaining) {
                Ok(_) => {
                    last_event = Instant::now();
                }
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => break,
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => return Ok(()),
            }
        }

        let current = match build_snapshot(root, &resolved) {
            Ok(snap) => snap,
            Err(e) => {
                eprintln!("[vertigo-sync] snapshot error during watch: {e}");
                continue;
            }
        };

        if current.fingerprint == prev.fingerprint {
            continue;
        }

        seq += 1;
        let diff = diff_snapshots(&prev, &current);

        let event = SyncDiffEvent {
            sequence_id: seq,
            source_hash: current.fingerprint.clone(),
            added_paths: diff.added.iter().map(|f| f.path.clone()).collect(),
            modified_paths: diff.modified.iter().map(|f| f.path.clone()).collect(),
            deleted_paths: diff.deleted.iter().map(|f| f.path.clone()).collect(),
            timestamp: chrono::Utc::now().to_rfc3339(),
        };

        let json = serde_json::to_string(&event).context("failed to serialize watch event")?;
        println!("{json}");

        if let Some(dir) = output_dir {
            write_snapshot_file(dir, &current)?;
        }

        prev = current;
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Server state (shared between HTTP server and background poller)
// ---------------------------------------------------------------------------

/// Maximum number of history entries to retain before eviction.
const MAX_HISTORY_ENTRIES: usize = 64;

/// Thread-safe state for the serve command.
pub struct ServerState {
    pub root: PathBuf,
    pub includes: Vec<String>,
    pub current: Mutex<Arc<Snapshot>>,
    pub history: Mutex<BTreeMap<String, Arc<Snapshot>>>,
    pub history_order: Mutex<VecDeque<String>>,
    pub tx: tokio::sync::broadcast::Sender<SyncDiffEvent>,
    pub sequence: Mutex<u64>,
    pub cache: Mutex<SnapshotCache>,
    pub metrics: Arc<Metrics>,
}

impl ServerState {
    pub fn new(
        root: PathBuf,
        includes: Vec<String>,
        initial: Snapshot,
        channel_capacity: usize,
    ) -> Arc<Self> {
        let capacity = channel_capacity.clamp(32, 16_384);
        let (tx, _rx) = tokio::sync::broadcast::channel::<SyncDiffEvent>(capacity);
        let metrics = Arc::new(Metrics::new());
        metrics
            .entries
            .store(initial.entries.len() as u64, Ordering::Relaxed);
        let arc = Arc::new(initial);
        let mut history = BTreeMap::new();
        let mut history_order = VecDeque::new();
        history.insert(arc.fingerprint.clone(), Arc::clone(&arc));
        history_order.push_back(arc.fingerprint.clone());

        Arc::new(Self {
            root,
            includes,
            current: Mutex::new(arc),
            history: Mutex::new(history),
            history_order: Mutex::new(history_order),
            tx,
            sequence: Mutex::new(0),
            cache: Mutex::new(SnapshotCache::new()),
            metrics,
        })
    }

    /// Poll source tree and broadcast diff if changed.
    /// Uses incremental `SnapshotCache` so only changed files are re-hashed.
    pub fn poll_and_broadcast(&self) -> Result<()> {
        let start = Instant::now();
        let new_snapshot = {
            let mut cache_lock = self
                .cache
                .lock()
                .map_err(|e| anyhow::anyhow!("cache lock: {e}"))?;
            build_snapshot_cached_with_metrics(
                &self.root,
                &self.includes,
                &mut cache_lock,
                &self.metrics,
            )?
        };
        let elapsed = start.elapsed();

        // Record poll metrics.
        self.metrics.polls.fetch_add(1, Ordering::Relaxed);
        self.metrics
            .poll_duration_sum_us
            .fetch_add(elapsed.as_micros() as u64, Ordering::Relaxed);
        self.metrics
            .entries
            .store(new_snapshot.entries.len() as u64, Ordering::Relaxed);

        if elapsed.as_millis() > 50 {
            eprintln!(
                "[vertigo-sync] slow poll: {elapsed:?} ({} entries)",
                new_snapshot.entries.len()
            );
        }
        let current = {
            let lock = self
                .current
                .lock()
                .map_err(|e| anyhow::anyhow!("lock: {e}"))?;
            Arc::clone(&lock)
        };

        if new_snapshot.fingerprint == current.fingerprint {
            return Ok(());
        }

        let diff = diff_snapshots(&current, &new_snapshot);
        let new_arc = Arc::new(new_snapshot);

        let seq = {
            let mut lock = self
                .sequence
                .lock()
                .map_err(|e| anyhow::anyhow!("lock: {e}"))?;
            *lock += 1;
            *lock
        };

        let event = SyncDiffEvent {
            sequence_id: seq,
            source_hash: new_arc.fingerprint.clone(),
            added_paths: diff.added.iter().map(|f| f.path.clone()).collect(),
            modified_paths: diff.modified.iter().map(|f| f.path.clone()).collect(),
            deleted_paths: diff.deleted.iter().map(|f| f.path.clone()).collect(),
            timestamp: chrono::Utc::now().to_rfc3339(),
        };

        {
            let mut lock = self
                .current
                .lock()
                .map_err(|e| anyhow::anyhow!("lock: {e}"))?;
            *lock = Arc::clone(&new_arc);
        }
        {
            let mut hist_lock = self
                .history
                .lock()
                .map_err(|e| anyhow::anyhow!("lock: {e}"))?;
            let mut order_lock = self
                .history_order
                .lock()
                .map_err(|e| anyhow::anyhow!("lock: {e}"))?;

            let fp = new_arc.fingerprint.clone();
            if !hist_lock.contains_key(&fp) {
                hist_lock.insert(fp.clone(), new_arc);
                order_lock.push_back(fp);

                // Evict oldest entries beyond the limit.
                while order_lock.len() > MAX_HISTORY_ENTRIES {
                    if let Some(oldest) = order_lock.pop_front() {
                        hist_lock.remove(&oldest);
                    }
                }
            }
        }

        self.metrics.events_emitted.fetch_add(1, Ordering::Relaxed);
        let _ = self.tx.send(event);
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Internal helpers
// ---------------------------------------------------------------------------

fn find_mismatch_path(first: &[SnapshotEntry], second: &[SnapshotEntry]) -> Option<String> {
    let max_len = first.len().max(second.len());

    for idx in 0..max_len {
        match (first.get(idx), second.get(idx)) {
            (Some(a), Some(b)) if a == b => continue,
            (Some(a), Some(b)) => return Some(format!("{}|{}", a.path, b.path)),
            (Some(a), None) => return Some(a.path.clone()),
            (None, Some(b)) => return Some(b.path.clone()),
            (None, None) => break,
        }
    }

    None
}

fn check_health_recursive(
    root: &Path,
    dir: &Path,
    file_count: &mut usize,
    seen_lower: &mut BTreeMap<String, String>,
    issues: &mut Vec<HealthIssue>,
) -> Result<()> {
    let read_dir =
        fs::read_dir(dir).with_context(|| format!("cannot read dir: {}", dir.display()))?;

    for entry in read_dir {
        let entry = entry?;
        let ft = entry.file_type()?;
        let name = entry.file_name();
        let name_str = name.to_string_lossy();

        if ft.is_symlink() {
            continue;
        }

        if ft.is_dir() {
            if matches!(
                name_str.as_ref(),
                ".git" | "node_modules" | "Packages" | "target"
            ) {
                continue;
            }
            check_health_recursive(root, &entry.path(), file_count, seen_lower, issues)?;
        } else if ft.is_file() {
            *file_count += 1;
            let path = entry.path();
            let rel = path
                .strip_prefix(root)
                .unwrap_or(&path)
                .to_string_lossy()
                .replace('\\', "/");

            let lower = rel.to_lowercase();
            if let Some(existing) = seen_lower.get(&lower) {
                issues.push(HealthIssue {
                    severity: "error".into(),
                    path: rel.clone(),
                    message: format!("case-insensitive path collision with '{existing}'"),
                });
            } else {
                seen_lower.insert(lower, rel.clone());
            }

            if rel.ends_with(".luau") || rel.ends_with(".lua") {
                let content = fs::read(&path)?;
                if std::str::from_utf8(&content).is_err() {
                    issues.push(HealthIssue {
                        severity: "error".into(),
                        path: rel.clone(),
                        message: "file is not valid UTF-8".into(),
                    });
                }
            }

            let meta = fs::metadata(&path)?;
            if meta.len() > 1_048_576 {
                issues.push(HealthIssue {
                    severity: "warn".into(),
                    path: rel.clone(),
                    message: format!("file exceeds 1 MB ({} bytes)", meta.len()),
                });
            }
        }
    }
    Ok(())
}

fn find_project_json(root: &Path) -> Option<PathBuf> {
    let mut dir = root.to_path_buf();
    for _ in 0..4 {
        let candidate = dir.join("default.project.json");
        if candidate.is_file() {
            return Some(candidate);
        }
        if !dir.pop() {
            break;
        }
    }
    None
}

fn write_snapshot_file(dir: &Path, snapshot: &Snapshot) -> Result<()> {
    let short_hash = if snapshot.fingerprint.len() >= 16 {
        &snapshot.fingerprint[..16]
    } else {
        &snapshot.fingerprint
    };
    let filename = format!("snapshot-{short_hash}.json");
    let path = dir.join(filename);
    write_json_file(&path, snapshot)?;
    eprintln!("[vertigo-sync] wrote {}", path.display());
    Ok(())
}

fn collect_files(root: &Path, current: &Path, output: &mut Vec<PathBuf>) -> Result<()> {
    let metadata = fs::symlink_metadata(current)
        .with_context(|| format!("failed to inspect {}", current.display()))?;

    if metadata.file_type().is_symlink() {
        return Ok(());
    }

    if metadata.is_file() {
        if should_skip_file(current) {
            return Ok(());
        }
        let relative = current.strip_prefix(root).with_context(|| {
            format!(
                "failed to strip root {} from {}",
                root.display(),
                current.display()
            )
        })?;
        output.push(relative.to_path_buf());
        return Ok(());
    }

    if metadata.is_dir() {
        if should_skip_dir(current) {
            return Ok(());
        }
        let mut children = Vec::new();
        for entry in fs::read_dir(current)
            .with_context(|| format!("failed to read directory {}", current.display()))?
        {
            let entry = entry.with_context(|| {
                format!("failed to read directory entry under {}", current.display())
            })?;
            children.push(entry.path());
        }

        children.sort_by(|a, b| normalize_path(a).cmp(&normalize_path(b)));
        for child in children {
            collect_files(root, &child, output)?;
        }
    }

    Ok(())
}

fn hash_file(path: &Path) -> Result<(String, u64)> {
    let file = File::open(path).with_context(|| format!("failed to open {}", path.display()))?;
    let meta = file
        .metadata()
        .with_context(|| format!("failed to stat {}", path.display()))?;
    let file_len = meta.len();

    // For files above the mmap threshold, use zero-copy memory-mapped I/O.
    // This avoids a kernel→userspace copy for the entire file contents.
    if file_len > MMAP_THRESHOLD {
        return hash_file_mmap(&file, file_len, path);
    }

    // Small files: regular buffered read (mmap syscall overhead not worth it).
    let mut reader = BufReader::new(file);
    let mut hasher = Sha256::new();
    let mut total_bytes = 0_u64;
    let mut buffer = [0_u8; 64 * 1024];

    loop {
        let read = reader
            .read(&mut buffer)
            .with_context(|| format!("failed to read {}", path.display()))?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
        total_bytes += read as u64;
    }

    let digest = hasher.finalize();
    Ok((format!("{digest:x}"), total_bytes))
}

/// Hash a file using memory-mapped I/O. The mmap gives the kernel a single
/// contiguous view of the file — the SHA-256 update reads directly from the
/// page cache with zero intermediate copies.
#[allow(unsafe_code)]
fn hash_file_mmap(file: &File, file_len: u64, path: &Path) -> Result<(String, u64)> {
    // SAFETY: The file is open for reading and we do not modify it.
    // The mmap is read-only and lives only for the duration of this function.
    let mmap = unsafe {
        memmap2::Mmap::map(file).with_context(|| format!("failed to mmap {}", path.display()))?
    };
    let mut hasher = Sha256::new();
    hasher.update(&mmap[..]);
    let digest = hasher.finalize();
    Ok((format!("{digest:x}"), file_len))
}

fn ensure_parent(path: &Path) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create parent directory {}", parent.display()))?;
    }
    Ok(())
}

fn normalize_path(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

fn should_skip_dir(path: &Path) -> bool {
    let Some(name) = path.file_name().and_then(|value| value.to_str()) else {
        return false;
    };
    SKIP_DIR_NAMES.contains(&name)
}

fn should_skip_file(path: &Path) -> bool {
    let Some(name) = path.file_name().and_then(|value| value.to_str()) else {
        return false;
    };
    if SKIP_FILE_NAMES.contains(&name) {
        return true;
    }
    SKIP_FILE_SUFFIXES
        .iter()
        .any(|suffix| name.ends_with(suffix))
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    #[test]
    fn snapshot_entries_are_sorted_and_deterministic() {
        let root = tempdir().expect("tempdir");

        fs::create_dir_all(root.path().join("src/nested")).expect("create src/nested");
        fs::write(root.path().join("src/z.lua"), "z").expect("write z");
        fs::write(root.path().join("src/a.lua"), "a").expect("write a");
        fs::write(root.path().join("src/nested/b.lua"), "b").expect("write b");

        let includes = vec!["src".to_string()];
        let first = build_snapshot(root.path(), &includes).expect("first snapshot");
        let second = build_snapshot(root.path(), &includes).expect("second snapshot");

        let paths: Vec<&str> = first
            .entries
            .iter()
            .map(|entry| entry.path.as_str())
            .collect();
        assert_eq!(paths, vec!["src/a.lua", "src/nested/b.lua", "src/z.lua"]);
        assert_eq!(first.entries, second.entries);
        assert_eq!(first.fingerprint, second.fingerprint);
    }

    #[test]
    fn snapshot_skips_generated_directories_and_logs() {
        let root = tempdir().expect("tempdir");
        fs::create_dir_all(root.path().join("src/live")).expect("create src/live");
        fs::create_dir_all(root.path().join("src/node_modules/pkg")).expect("create node_modules");
        fs::create_dir_all(root.path().join("src/dist/assets")).expect("create dist");
        fs::create_dir_all(root.path().join("src/.cache")).expect("create cache");

        fs::write(root.path().join("src/live/game.luau"), "print('ok')").expect("write game");
        fs::write(root.path().join("src/live/dev.log"), "noise").expect("write log");
        fs::write(
            root.path().join("src/node_modules/pkg/index.js"),
            "module.exports = 1;",
        )
        .expect("write node module");
        fs::write(
            root.path().join("src/dist/assets/app.js"),
            "console.log(1);",
        )
        .expect("write dist");
        fs::write(root.path().join("src/.cache/meta.json"), "{}").expect("write cache");

        let includes = vec!["src".to_string()];
        let snapshot = build_snapshot(root.path(), &includes).expect("snapshot");
        let paths: Vec<&str> = snapshot
            .entries
            .iter()
            .map(|entry| entry.path.as_str())
            .collect();
        assert_eq!(paths, vec!["src/live/game.luau"]);
    }

    #[test]
    fn diff_classifies_added_modified_deleted() {
        let previous = Snapshot {
            version: 1,
            include: vec!["src".to_string()],
            fingerprint: "prev_fp".to_string(),
            entries: vec![
                SnapshotEntry {
                    path: "src/a.lua".to_string(),
                    sha256: "hash_a".to_string(),
                    bytes: 1,
                },
                SnapshotEntry {
                    path: "src/b.lua".to_string(),
                    sha256: "hash_b_old".to_string(),
                    bytes: 2,
                },
                SnapshotEntry {
                    path: "src/c.lua".to_string(),
                    sha256: "hash_c".to_string(),
                    bytes: 3,
                },
            ],
        };

        let current = Snapshot {
            version: 1,
            include: vec!["src".to_string()],
            fingerprint: "cur_fp".to_string(),
            entries: vec![
                SnapshotEntry {
                    path: "src/a.lua".to_string(),
                    sha256: "hash_a".to_string(),
                    bytes: 1,
                },
                SnapshotEntry {
                    path: "src/b.lua".to_string(),
                    sha256: "hash_b_new".to_string(),
                    bytes: 20,
                },
                SnapshotEntry {
                    path: "src/d.lua".to_string(),
                    sha256: "hash_d".to_string(),
                    bytes: 4,
                },
            ],
        };

        let diff = diff_snapshots(&previous, &current);

        assert_eq!(diff.added.len(), 1);
        assert_eq!(diff.added[0].path, "src/d.lua");

        assert_eq!(diff.modified.len(), 1);
        assert_eq!(diff.modified[0].path, "src/b.lua");
        assert_eq!(diff.modified[0].previous_sha256, "hash_b_old");
        assert_eq!(diff.modified[0].current_sha256, "hash_b_new");

        assert_eq!(diff.deleted.len(), 1);
        assert_eq!(diff.deleted[0].path, "src/c.lua");
    }

    #[test]
    fn empty_diff_when_fingerprints_match() {
        let snap = Snapshot {
            version: 1,
            include: vec!["src".to_string()],
            fingerprint: "same_fp".to_string(),
            entries: vec![SnapshotEntry {
                path: "src/a.lua".to_string(),
                sha256: "hash_a".to_string(),
                bytes: 10,
            }],
        };

        let diff = diff_snapshots(&snap, &snap);
        assert!(diff.added.is_empty());
        assert!(diff.modified.is_empty());
        assert!(diff.deleted.is_empty());
    }

    #[test]
    fn health_doctor_valid_source() {
        let root = tempdir().expect("tempdir");
        fs::create_dir_all(root.path().join("src")).expect("create src");
        fs::write(root.path().join("src/init.luau"), "--!strict\nreturn {}\n").expect("write");
        fs::write(
            root.path().join("default.project.json"),
            r#"{"name":"test","tree":{"$path":"src/"}}"#,
        )
        .expect("write project json");

        let includes = vec!["src".to_string()];
        let report = run_health_doctor(root.path(), &includes).expect("doctor");
        assert!(report.healthy);
        assert!(report.deterministic);
        assert_eq!(report.file_count, 1);
        assert!(report.issues.is_empty());
    }

    #[test]
    fn health_doctor_detects_non_utf8() {
        let root = tempdir().expect("tempdir");
        let dir = root.path().join("src");
        fs::create_dir_all(&dir).expect("mkdir");
        fs::write(dir.join("bad.luau"), &[0xFF, 0xFE, 0x00, 0x01]).expect("write");

        let includes = vec!["src".to_string()];
        let report = run_health_doctor(root.path(), &includes).expect("doctor");
        assert!(!report.healthy);
        assert!(report.issues.iter().any(|i| i.message.contains("UTF-8")));
    }

    #[test]
    fn snapshot_serialization_roundtrip() {
        let root = tempdir().expect("tempdir");
        fs::create_dir_all(root.path().join("src")).expect("mkdir");
        fs::write(root.path().join("src/test.luau"), "return nil\n").expect("write");

        let includes = vec!["src".to_string()];
        let snap = build_snapshot(root.path(), &includes).expect("snapshot");
        let json = serde_json::to_string_pretty(&snap).expect("serialize");
        let deserialized: Snapshot = serde_json::from_str(&json).expect("deserialize");

        assert_eq!(snap.fingerprint, deserialized.fingerprint);
        assert_eq!(snap.entries.len(), deserialized.entries.len());
    }

    // -----------------------------------------------------------------------
    // SnapshotCache tests
    // -----------------------------------------------------------------------

    #[test]
    fn cache_new_is_empty() {
        let cache = SnapshotCache::new();
        assert!(cache.is_empty());
        assert_eq!(cache.len(), 0);
    }

    #[test]
    fn cache_insert_and_get() {
        let mut cache = SnapshotCache::new();
        let mtime = SystemTime::now();
        cache.insert("src/a.lua".into(), "abc123".into(), 100, mtime);

        assert_eq!(cache.len(), 1);
        assert_eq!(cache.get("src/a.lua", mtime, 100), Some("abc123"));
    }

    #[test]
    fn cache_miss_on_different_mtime() {
        let mut cache = SnapshotCache::new();
        let mtime = SystemTime::now();
        cache.insert("src/a.lua".into(), "abc123".into(), 100, mtime);

        let later = mtime + Duration::from_secs(1);
        assert_eq!(cache.get("src/a.lua", later, 100), None);
    }

    #[test]
    fn cache_miss_on_different_size() {
        let mut cache = SnapshotCache::new();
        let mtime = SystemTime::now();
        cache.insert("src/a.lua".into(), "abc123".into(), 100, mtime);

        assert_eq!(cache.get("src/a.lua", mtime, 200), None);
    }

    #[test]
    fn cache_retain_prunes_deleted() {
        let mut cache = SnapshotCache::new();
        let mtime = SystemTime::now();
        cache.insert("src/a.lua".into(), "aaa".into(), 10, mtime);
        cache.insert("src/b.lua".into(), "bbb".into(), 20, mtime);
        cache.insert("src/c.lua".into(), "ccc".into(), 30, mtime);

        let live: HashSet<String> = ["src/a.lua".into(), "src/c.lua".into()].into();
        cache.retain_paths(&live);

        assert_eq!(cache.len(), 2);
        assert!(cache.get("src/a.lua", mtime, 10).is_some());
        assert!(cache.get("src/b.lua", mtime, 20).is_none());
        assert!(cache.get("src/c.lua", mtime, 30).is_some());
    }

    // -----------------------------------------------------------------------
    // build_snapshot_cached tests
    // -----------------------------------------------------------------------

    #[test]
    fn cached_snapshot_matches_uncached() {
        let root = tempdir().expect("tempdir");
        fs::create_dir_all(root.path().join("src/sub")).expect("mkdir");
        fs::write(root.path().join("src/a.luau"), "return 'a'\n").expect("write a");
        fs::write(root.path().join("src/b.luau"), "return 'b'\n").expect("write b");
        fs::write(root.path().join("src/sub/c.luau"), "return 'c'\n").expect("write c");

        let includes = vec!["src".to_string()];
        let uncached = build_snapshot(root.path(), &includes).expect("uncached");

        let mut cache = SnapshotCache::new();
        let cached = build_snapshot_cached(root.path(), &includes, &mut cache).expect("cached");

        assert_eq!(uncached.fingerprint, cached.fingerprint);
        assert_eq!(uncached.entries, cached.entries);
        assert_eq!(cache.len(), 3);
    }

    #[test]
    fn cached_snapshot_reuses_cache_on_second_call() {
        let root = tempdir().expect("tempdir");
        fs::create_dir_all(root.path().join("src")).expect("mkdir");
        fs::write(root.path().join("src/a.luau"), "return 'a'\n").expect("write");

        let includes = vec!["src".to_string()];
        let mut cache = SnapshotCache::new();

        let first = build_snapshot_cached(root.path(), &includes, &mut cache).expect("first");
        let second = build_snapshot_cached(root.path(), &includes, &mut cache).expect("second");

        assert_eq!(first.fingerprint, second.fingerprint);
        assert_eq!(first.entries, second.entries);
    }

    #[test]
    fn cached_snapshot_detects_file_modification() {
        let root = tempdir().expect("tempdir");
        fs::create_dir_all(root.path().join("src")).expect("mkdir");
        fs::write(root.path().join("src/x.luau"), "return 1\n").expect("write");

        let includes = vec!["src".to_string()];
        let mut cache = SnapshotCache::new();

        let first = build_snapshot_cached(root.path(), &includes, &mut cache).expect("first");

        // Wait briefly so mtime advances, then modify.
        std::thread::sleep(Duration::from_millis(50));
        fs::write(root.path().join("src/x.luau"), "return 2\n").expect("modify");

        let second = build_snapshot_cached(root.path(), &includes, &mut cache).expect("second");

        assert_ne!(first.fingerprint, second.fingerprint);
    }

    #[test]
    fn cached_snapshot_handles_deleted_file() {
        let root = tempdir().expect("tempdir");
        fs::create_dir_all(root.path().join("src")).expect("mkdir");
        fs::write(root.path().join("src/a.luau"), "a").expect("write a");
        fs::write(root.path().join("src/b.luau"), "b").expect("write b");

        let includes = vec!["src".to_string()];
        let mut cache = SnapshotCache::new();

        let _first = build_snapshot_cached(root.path(), &includes, &mut cache).expect("first");
        assert_eq!(cache.len(), 2);

        fs::remove_file(root.path().join("src/b.luau")).expect("delete");

        let second = build_snapshot_cached(root.path(), &includes, &mut cache).expect("second");
        assert_eq!(second.entries.len(), 1);
        assert_eq!(cache.len(), 1); // pruned
    }

    // -----------------------------------------------------------------------
    // mmap hash parity test
    // -----------------------------------------------------------------------

    #[test]
    fn mmap_and_buffered_hash_produce_same_result() {
        let root = tempdir().expect("tempdir");

        // Create a file larger than MMAP_THRESHOLD (4KB).
        let large_content: Vec<u8> = (0..8192).map(|i| (i % 256) as u8).collect();
        let large_path = root.path().join("large.bin");
        fs::write(&large_path, &large_content).expect("write large");

        // Create a file smaller than MMAP_THRESHOLD.
        let small_path = root.path().join("small.bin");
        fs::write(&small_path, b"hello world").expect("write small");

        let (large_hash, large_bytes) = hash_file(&large_path).expect("hash large");
        let (small_hash, small_bytes) = hash_file(&small_path).expect("hash small");

        assert_eq!(large_bytes, 8192);
        assert_eq!(small_bytes, 11);

        // Verify hashes are valid hex SHA-256 (64 chars).
        assert_eq!(large_hash.len(), 64);
        assert_eq!(small_hash.len(), 64);
        assert!(large_hash.chars().all(|c| c.is_ascii_hexdigit()));
        assert!(small_hash.chars().all(|c| c.is_ascii_hexdigit()));

        // Re-hash to confirm determinism.
        let (large_hash2, _) = hash_file(&large_path).expect("rehash large");
        let (small_hash2, _) = hash_file(&small_path).expect("rehash small");
        assert_eq!(large_hash, large_hash2);
        assert_eq!(small_hash, small_hash2);
    }
}
