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

pub mod builder_codegen;
pub mod config;
pub mod credentials;
pub mod errors;
pub mod fmt;
pub mod init;
pub mod lint;
pub mod mcp;
pub mod migrate;
pub mod output;
pub mod package;
pub mod plugin_smoke;
pub mod project;
pub mod publish;
pub mod rbxl;
pub mod scripts;
pub mod serve_rbxl;
pub mod server;
pub mod sourcemap;
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
        let mut lock = self.last_event.lock().unwrap_or_else(|e| e.into_inner());
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
                let lock = self.last_event.lock().unwrap_or_else(|e| e.into_inner());
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

    /// Return current coalescer state for observability.
    pub fn status(&self) -> CoalescerStatus {
        let lock = self.last_event.lock().unwrap_or_else(|e| e.into_inner());
        let last_event_elapsed = lock.map(|ts| ts.elapsed());
        CoalescerStatus {
            pending: self.pending.load(Ordering::Acquire),
            last_event_elapsed,
            window: self.window,
        }
    }
}

/// Snapshot of coalescer state for observability endpoints.
#[derive(Debug, Clone)]
pub struct CoalescerStatus {
    pub pending: bool,
    pub last_event_elapsed: Option<Duration>,
    pub window: Duration,
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

// ---------------------------------------------------------------------------
// Glob ignore pattern matching
// ---------------------------------------------------------------------------

/// Pre-compiled set of glob patterns used to exclude paths during directory
/// traversal. Compiles once, matches O(1) per path via `globset`-style
/// sequential matching (using the `glob` crate's `Pattern`).
#[derive(Debug, Clone)]
pub struct GlobIgnoreSet {
    patterns: Vec<glob::Pattern>,
}

impl GlobIgnoreSet {
    /// Compile a set of glob pattern strings. Invalid patterns are silently
    /// skipped (logged to stderr).
    pub fn new(patterns: &[String]) -> Self {
        let compiled: Vec<glob::Pattern> = patterns
            .iter()
            .filter_map(|p| {
                glob::Pattern::new(p)
                    .map_err(|e| {
                        eprintln!("[vertigo-sync] invalid glob pattern '{}': {}", p, e);
                        e
                    })
                    .ok()
            })
            .collect();
        Self { patterns: compiled }
    }

    /// Returns an empty set that matches nothing.
    pub fn empty() -> Self {
        Self {
            patterns: Vec::new(),
        }
    }

    /// Test whether a relative path matches any of the ignore patterns.
    #[inline]
    pub fn is_ignored(&self, relative_path: &str) -> bool {
        self.patterns.iter().any(|p| p.matches(relative_path))
    }

    /// Test whether a directory path (or any child) could match any pattern.
    #[inline]
    pub fn could_match_under(&self, dir_prefix: &str) -> bool {
        if self.patterns.is_empty() {
            return false;
        }
        let _ = dir_prefix;
        true
    }

    pub fn is_empty(&self) -> bool {
        self.patterns.is_empty()
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
// Readiness contract
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReadinessTarget {
    EditSync,
    Preview,
    FullBakeStart,
    FullBakeResult,
}

impl ReadinessTarget {
    pub const ALL: [Self; 4] = [
        Self::EditSync,
        Self::Preview,
        Self::FullBakeStart,
        Self::FullBakeResult,
    ];
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReadinessStatusClass {
    Ready,
    Transient,
    Blocked,
    Failed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReadinessRecord {
    pub target: ReadinessTarget,
    pub ready: bool,
    pub epoch: u64,
    pub incarnation_id: String,
    pub status_class: ReadinessStatusClass,
    pub code: String,
    pub reason: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReadinessExpectation {
    pub target: ReadinessTarget,
    pub epoch: u64,
    pub incarnation_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommandReadinessExpectation {
    pub expected_target: ReadinessTarget,
    pub expected_epoch: u64,
    pub expected_incarnation_id: String,
}

impl CommandReadinessExpectation {
    pub fn to_readiness_expectation(&self) -> ReadinessExpectation {
        ReadinessExpectation {
            target: self.expected_target,
            epoch: self.expected_epoch,
            incarnation_id: self.expected_incarnation_id.clone(),
        }
    }
}

#[derive(Debug, Clone)]
struct ReadinessEvent {
    sequence: u64,
    record: ReadinessRecord,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ReadinessRejection {
    TargetMismatch {
        expected: ReadinessTarget,
        actual: ReadinessTarget,
    },
    EpochMismatch {
        target: ReadinessTarget,
        expected: u64,
        actual: u64,
    },
    IncarnationMismatch {
        target: ReadinessTarget,
        expected: String,
        actual: String,
    },
    NotReady {
        target: ReadinessTarget,
        status_class: ReadinessStatusClass,
        code: String,
    },
    DependencyViolation {
        target: ReadinessTarget,
        prerequisite: ReadinessTarget,
        message: String,
    },
    InvalidRecord {
        target: ReadinessTarget,
        message: String,
    },
}

#[derive(Debug, Clone)]
pub struct ReadinessState {
    incarnation_seq: u64,
    incarnation_id: String,
    full_bake_start_success_incarnation_id: Option<String>,
    event_seq: u64,
    records: BTreeMap<ReadinessTarget, ReadinessRecord>,
}

impl Default for ReadinessState {
    fn default() -> Self {
        Self::new()
    }
}

impl ReadinessState {
    pub fn new() -> Self {
        let incarnation_id = "inc-1".to_string();
        let mut records = BTreeMap::new();
        for target in ReadinessTarget::ALL {
            records.insert(target, Self::bootstrap_record(target, &incarnation_id));
        }
        Self {
            incarnation_seq: 1,
            incarnation_id,
            full_bake_start_success_incarnation_id: None,
            event_seq: 0,
            records,
        }
    }

    fn bootstrap_record(target: ReadinessTarget, incarnation_id: &str) -> ReadinessRecord {
        Self::non_ready_record(
            target,
            incarnation_id,
            0,
            ReadinessStatusClass::Transient,
            "plugin_unavailable",
            Some("plugin_unavailable".to_string()),
        )
    }

    fn non_ready_record(
        target: ReadinessTarget,
        incarnation_id: &str,
        epoch: u64,
        status_class: ReadinessStatusClass,
        code: &str,
        reason: Option<String>,
    ) -> ReadinessRecord {
        ReadinessRecord {
            target,
            ready: false,
            epoch,
            incarnation_id: incarnation_id.to_string(),
            status_class,
            code: code.to_string(),
            reason,
        }
    }

    fn read_record(&self, target: ReadinessTarget) -> &ReadinessRecord {
        self.records
            .get(&target)
            .unwrap_or_else(|| panic!("missing readiness record for {target:?}"))
    }

    fn read_record_mut(&mut self, target: ReadinessTarget) -> &mut ReadinessRecord {
        self.records
            .get_mut(&target)
            .unwrap_or_else(|| panic!("missing readiness record for {target:?}"))
    }

    fn validate_record_shape(record: &ReadinessRecord) -> Result<(), ReadinessRejection> {
        if record.ready {
            if record.status_class != ReadinessStatusClass::Ready {
                return Err(ReadinessRejection::InvalidRecord {
                    target: record.target,
                    message: "ready records must use status_class=ready".to_string(),
                });
            }
            if record.code != "ready" {
                return Err(ReadinessRejection::InvalidRecord {
                    target: record.target,
                    message: "ready records must use code=ready".to_string(),
                });
            }
            if record.reason.is_some() {
                return Err(ReadinessRejection::InvalidRecord {
                    target: record.target,
                    message: "ready records must not carry a reason".to_string(),
                });
            }
        } else if record.status_class == ReadinessStatusClass::Ready {
            return Err(ReadinessRejection::InvalidRecord {
                target: record.target,
                message: "status_class=ready requires ready=true".to_string(),
            });
        }
        Ok(())
    }

    fn demote_record(
        record: &mut ReadinessRecord,
        prerequisite: ReadinessTarget,
        reason: Option<String>,
    ) {
        record.ready = false;
        record.status_class = ReadinessStatusClass::Blocked;
        record.code = "prerequisite_invalidated".to_string();
        record.reason = Some(reason.unwrap_or_else(|| format!("{prerequisite:?} invalidated")));
    }

    fn demote_dependents(&mut self, prerequisite: ReadinessTarget, reason: Option<String>) {
        if matches!(
            prerequisite,
            ReadinessTarget::EditSync | ReadinessTarget::FullBakeStart
        ) {
            self.full_bake_start_success_incarnation_id = None;
        }

        let dependent_targets: &[ReadinessTarget] = match prerequisite {
            ReadinessTarget::EditSync => &[
                ReadinessTarget::Preview,
                ReadinessTarget::FullBakeStart,
                ReadinessTarget::FullBakeResult,
            ],
            ReadinessTarget::FullBakeStart => &[ReadinessTarget::FullBakeResult],
            ReadinessTarget::Preview | ReadinessTarget::FullBakeResult => &[],
        };

        for target in dependent_targets {
            let record = self.read_record_mut(*target);
            if record.ready {
                Self::demote_record(record, prerequisite, reason.clone());
            }
        }
    }

    pub fn current_readiness(&self, target: ReadinessTarget) -> ReadinessRecord {
        self.read_record(target).clone()
    }

    pub fn current_readiness_snapshot(&self, target: ReadinessTarget) -> (u64, ReadinessRecord) {
        (self.event_seq, self.current_readiness(target))
    }

    pub fn next_readiness_snapshot(&mut self) -> (u64, Vec<ReadinessRecord>) {
        self.event_seq = self.event_seq.saturating_add(1);
        let sequence = self.event_seq;
        let records = ReadinessTarget::ALL
            .into_iter()
            .map(|target| self.current_readiness(target))
            .collect();
        (sequence, records)
    }

    pub fn advance_epoch_if_invalidated(
        &mut self,
        target: ReadinessTarget,
        invalidated: bool,
    ) -> ReadinessRecord {
        if !invalidated {
            return self.current_readiness(target);
        }

        let epoch = {
            let record = self.read_record_mut(target);
            record.epoch = record.epoch.saturating_add(1);
            record.epoch
        };
        *self.read_record_mut(target) = Self::non_ready_record(
            target,
            &self.incarnation_id,
            epoch,
            ReadinessStatusClass::Transient,
            "target_invalidated",
            Some("target_invalidated".to_string()),
        );
        self.demote_dependents(target, Some(format!("{target:?} invalidated")));
        self.current_readiness(target)
    }

    pub fn rotate_incarnation(&mut self, reason: impl Into<String>) -> String {
        self.incarnation_seq = self.incarnation_seq.saturating_add(1);
        self.incarnation_id = format!("inc-{}", self.incarnation_seq);
        self.full_bake_start_success_incarnation_id = None;
        let reason = reason.into();
        for target in ReadinessTarget::ALL {
            let epoch = self.read_record(target).epoch;
            *self.read_record_mut(target) = Self::non_ready_record(
                target,
                &self.incarnation_id,
                epoch,
                ReadinessStatusClass::Transient,
                "incarnation_rotated",
                Some(reason.clone()),
            );
        }
        self.incarnation_id.clone()
    }

    pub fn update_readiness(&mut self, record: ReadinessRecord) -> Result<(), ReadinessRejection> {
        Self::validate_record_shape(&record)?;
        let target = record.target;
        if record.incarnation_id != self.incarnation_id {
            return Err(ReadinessRejection::IncarnationMismatch {
                target,
                expected: self.incarnation_id.clone(),
                actual: record.incarnation_id,
            });
        }

        let current_epoch = self.read_record(target).epoch;
        if record.epoch != current_epoch {
            return Err(ReadinessRejection::EpochMismatch {
                target,
                expected: record.epoch,
                actual: current_epoch,
            });
        }

        if record.ready {
            if matches!(
                target,
                ReadinessTarget::Preview | ReadinessTarget::FullBakeStart
            ) {
                let edit_sync = self.read_record(ReadinessTarget::EditSync);
                if !edit_sync.ready || edit_sync.incarnation_id != self.incarnation_id {
                    return Err(ReadinessRejection::DependencyViolation {
                        target,
                        prerequisite: ReadinessTarget::EditSync,
                        message:
                            "preview and full_bake_start require ready edit_sync for the same incarnation"
                                .to_string(),
                    });
                }
            }

            if target == ReadinessTarget::FullBakeResult {
                if self.full_bake_start_success_incarnation_id.as_deref()
                    != Some(self.incarnation_id.as_str())
                {
                    return Err(ReadinessRejection::DependencyViolation {
                        target: ReadinessTarget::FullBakeResult,
                        prerequisite: ReadinessTarget::FullBakeStart,
                        message:
                            "full_bake_result requires a successful full_bake_start for the same incarnation"
                                .to_string(),
                    });
                }
            }
        } else {
            if target == ReadinessTarget::FullBakeStart {
                self.full_bake_start_success_incarnation_id = None;
            }
            self.demote_dependents(target, record.reason.clone());
        }

        *self.read_record_mut(target) = record;
        Ok(())
    }

    pub fn validate_expectation(
        &self,
        expected_target: ReadinessTarget,
        expectation: &ReadinessExpectation,
    ) -> Result<(), ReadinessRejection> {
        if expectation.target != expected_target {
            return Err(ReadinessRejection::TargetMismatch {
                expected: expected_target,
                actual: expectation.target,
            });
        }

        let current = self.read_record(expected_target);
        if current.epoch != expectation.epoch {
            return Err(ReadinessRejection::EpochMismatch {
                target: expected_target,
                expected: expectation.epoch,
                actual: current.epoch,
            });
        }

        if current.incarnation_id != expectation.incarnation_id {
            return Err(ReadinessRejection::IncarnationMismatch {
                target: expected_target,
                expected: expectation.incarnation_id.clone(),
                actual: current.incarnation_id.clone(),
            });
        }

        if !current.ready {
            return Err(ReadinessRejection::NotReady {
                target: expected_target,
                status_class: current.status_class,
                code: current.code.clone(),
            });
        }

        Ok(())
    }

    pub fn record_successful_full_bake_start_for_current_incarnation(
        &mut self,
    ) -> Result<(), ReadinessRejection> {
        let edit_sync = self.read_record(ReadinessTarget::EditSync);
        let full_bake_start = self.read_record(ReadinessTarget::FullBakeStart);

        if !edit_sync.ready
            || !full_bake_start.ready
            || edit_sync.incarnation_id != self.incarnation_id
            || full_bake_start.incarnation_id != self.incarnation_id
        {
            return Err(ReadinessRejection::DependencyViolation {
                target: ReadinessTarget::FullBakeStart,
                prerequisite: ReadinessTarget::EditSync,
                message:
                    "full_bake_start success can only be recorded when edit_sync and full_bake_start are ready for the current incarnation"
                        .to_string(),
            });
        }

        self.full_bake_start_success_incarnation_id = Some(self.incarnation_id.clone());
        Ok(())
    }
}

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

    /// Remove entries for paths no longer present — borrows path refs to avoid cloning.
    pub fn retain_paths_ref(&mut self, live_paths: &HashSet<&str>) {
        self.entries
            .retain(|key, _| live_paths.contains(key.as_str()));
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

/// Optional instance metadata parsed from a sibling `.meta.json` file.
/// Follows the Rojo meta.json schema for property and attribute overrides.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct InstanceMeta {
    pub properties: BTreeMap<String, serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub attributes: Option<BTreeMap<String, serde_json::Value>>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SnapshotEntry {
    pub path: String,
    pub sha256: String,
    pub bytes: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub meta: Option<InstanceMeta>,
    /// File type hint: "luau", "json", "txt", "csv", "rbxm", "rbxmx", "meta_json", "lua", "other".
    #[serde(skip_serializing_if = "Option::is_none")]
    pub file_type: Option<String>,
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
    /// File type hint propagated from the current snapshot entry.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub file_type: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RenamedEntry {
    pub old_path: String,
    pub new_path: String,
    pub sha256: String,
    pub bytes: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SnapshotDiff {
    pub previous_fingerprint: String,
    pub current_fingerprint: String,
    pub added: Vec<SnapshotEntry>,
    pub modified: Vec<ModifiedEntry>,
    pub deleted: Vec<SnapshotEntry>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub renamed: Vec<RenamedEntry>,
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

/// A path rename event (old_path -> new_path) detected via content hash.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RenamedPathEvent {
    pub old_path: String,
    pub new_path: String,
}

/// SSE / watch diff event payload.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SyncDiffEvent {
    pub sequence_id: u64,
    pub source_hash: String,
    pub added_paths: Vec<String>,
    pub modified_paths: Vec<String>,
    pub deleted_paths: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub renamed_paths: Vec<RenamedPathEvent>,
    pub timestamp: String,
}

// ---------------------------------------------------------------------------
// Plugin command channel
// ---------------------------------------------------------------------------

/// A command queued for the Studio plugin to execute on its next poll.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PluginCommand {
    pub id: String,
    /// One of: "toggle_sync", "force_resync", "set_frame_budget", "run_builders", "set_log_level"
    pub command: String,
    #[serde(default)]
    pub params: serde_json::Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub readiness: Option<CommandReadinessExpectation>,
    /// Epoch seconds when the command was created (serialization-friendly).
    pub created_at_epoch: f64,
    /// Monotonic instant for GC — not serialized.
    #[serde(skip)]
    pub created_at: Option<Instant>,
}

/// Acknowledgment from the plugin after executing a command.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PluginCommandAck {
    pub command_id: String,
    pub success: bool,
    pub message: String,
}

/// Maximum number of pending plugin commands. Rejects new commands when full.
pub const PLUGIN_COMMAND_QUEUE_CAPACITY: usize = 32;

/// Commands older than this are garbage-collected.
pub const PLUGIN_COMMAND_TTL: Duration = Duration::from_secs(60);

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
    build_snapshot_with_ignores(root, includes, &GlobIgnoreSet::empty())
}

/// Build a snapshot with glob-based path exclusions applied during traversal.
pub fn build_snapshot_with_ignores(
    root: &Path,
    includes: &[String],
    ignores: &GlobIgnoreSet,
) -> Result<Snapshot> {
    let resolved_includes = resolve_includes(includes);
    let mut files = Vec::new();

    for include in &resolved_includes {
        let include_path = root.join(include);
        if !include_path.exists() {
            continue;
        }
        collect_files(root, &include_path, &mut files, ignores)?;
    }

    files.sort_by_key(|a| normalize_path(a));

    // Parallel hash using rayon — all CPU cores for initial/uncached builds.
    let entries: Result<Vec<SnapshotEntry>> = files
        .par_iter()
        .map(|relative| {
            let absolute = root.join(relative);
            let (sha256, bytes) = hash_file(&absolute)
                .with_context(|| format!("failed to hash file {}", absolute.display()))?;
            let normalized = normalize_path(relative);
            let file_type = Some(classify_file_type(&normalized).to_string());
            Ok(SnapshotEntry {
                path: normalized,
                sha256,
                bytes,
                meta: None,
                file_type,
            })
        })
        .collect();

    let mut entries = entries?;
    entries.sort_by(|a, b| a.path.cmp(&b.path));
    let fingerprint = fingerprint_entries(&entries);

    // Lazy meta.json attachment — only reads .meta.json files that exist as
    // siblings to Luau/Lua entries. Zero extra syscalls for projects without them.
    attach_meta_json(root, &mut entries);

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
    build_snapshot_cached_inner(root, includes, cache, None, &GlobIgnoreSet::empty())
}

/// Like `build_snapshot_cached` but also records cache hit/miss counts
/// into the provided `Metrics`.
pub fn build_snapshot_cached_with_metrics(
    root: &Path,
    includes: &[String],
    cache: &mut SnapshotCache,
    metrics: &Metrics,
) -> Result<Snapshot> {
    build_snapshot_cached_inner(
        root,
        includes,
        cache,
        Some(metrics),
        &GlobIgnoreSet::empty(),
    )
}

/// Cached snapshot build with glob ignore patterns.
pub fn build_snapshot_cached_with_ignores(
    root: &Path,
    includes: &[String],
    cache: &mut SnapshotCache,
    metrics: &Metrics,
    ignores: &GlobIgnoreSet,
) -> Result<Snapshot> {
    build_snapshot_cached_inner(root, includes, cache, Some(metrics), ignores)
}

/// Shared implementation for cached snapshot builds. When `metrics` is
/// `Some`, cache hit/miss counts are recorded atomically.
fn build_snapshot_cached_inner(
    root: &Path,
    includes: &[String],
    cache: &mut SnapshotCache,
    metrics: Option<&Metrics>,
    ignores: &GlobIgnoreSet,
) -> Result<Snapshot> {
    let resolved_includes = resolve_includes(includes);
    let mut files = Vec::new();

    for include in &resolved_includes {
        let include_path = root.join(include);
        if !include_path.exists() {
            continue;
        }
        collect_files(root, &include_path, &mut files, ignores)?;
    }

    files.sort_by_key(|a| normalize_path(a));

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

    // Record cache hit/miss metrics if a Metrics handle was provided.
    if let Some(m) = metrics {
        let mut hits = 0u64;
        let mut misses = 0u64;
        for fw in &work {
            if fw.cached_hash.is_some() {
                hits += 1;
            } else {
                misses += 1;
            }
        }
        m.cache_hits.fetch_add(hits, Ordering::Relaxed);
        m.cache_misses.fetch_add(misses, Ordering::Relaxed);
    }

    // Parallel hash only the cache misses.
    let entries: Result<Vec<SnapshotEntry>> = work
        .par_iter()
        .map(|fw| {
            if let Some(ref sha256) = fw.cached_hash {
                let file_type = Some(classify_file_type(&fw.normalized).to_string());
                Ok(SnapshotEntry {
                    path: fw.normalized.clone(),
                    sha256: sha256.clone(),
                    bytes: fw.size,
                    meta: None,
                    file_type,
                })
            } else {
                let (sha256, bytes) = hash_file(&fw.absolute)
                    .with_context(|| format!("failed to hash file {}", fw.absolute.display()))?;
                let file_type = Some(classify_file_type(&fw.normalized).to_string());
                Ok(SnapshotEntry {
                    path: fw.normalized.clone(),
                    sha256,
                    bytes,
                    meta: None,
                    file_type,
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

    // Prune deleted files from cache (borrow paths to avoid cloning).
    let live_paths: HashSet<&str> = entries.iter().map(|e| e.path.as_str()).collect();
    cache.retain_paths_ref(&live_paths);

    entries.sort_by(|a, b| a.path.cmp(&b.path));
    let fingerprint = fingerprint_entries(&entries);

    // Lazy meta.json attachment for cached builds too.
    attach_meta_json(root, &mut entries);

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

    // Stack-allocated buffer for u64 formatting — avoids heap allocation per entry.
    let mut itoa_buf = itoa::Buffer::new();
    for entry in entries {
        hasher.update(entry.path.as_bytes());
        hasher.update([0]);
        hasher.update(entry.sha256.as_bytes());
        hasher.update([0]);
        hasher.update(itoa_buf.format(entry.bytes).as_bytes());
        hasher.update(b"\n");
    }

    let digest = hasher.finalize();
    format!("{digest:x}")
}

// ---------------------------------------------------------------------------
// Diff engine
// ---------------------------------------------------------------------------

pub fn diff_snapshots(previous: &Snapshot, current: &Snapshot) -> SnapshotDiff {
    // Use borrowed keys (&str) to avoid cloning every path into the lookup maps.
    let mut previous_map: BTreeMap<&str, &SnapshotEntry> = BTreeMap::new();
    for entry in &previous.entries {
        previous_map.insert(&entry.path, entry);
    }

    let mut current_map: BTreeMap<&str, &SnapshotEntry> = BTreeMap::new();
    for entry in &current.entries {
        current_map.insert(&entry.path, entry);
    }

    // Pre-size based on typical diff proportions to reduce reallocation.
    let estimate = current_map.len().max(previous_map.len()) / 8 + 4;
    let mut added = Vec::with_capacity(estimate);
    let mut modified = Vec::with_capacity(estimate);
    let mut deleted = Vec::with_capacity(estimate);

    for (&path, &current_entry) in &current_map {
        match previous_map.get(path) {
            None => added.push(current_entry.clone()),
            Some(&previous_entry) => {
                if previous_entry.sha256 != current_entry.sha256
                    || previous_entry.bytes != current_entry.bytes
                {
                    modified.push(ModifiedEntry {
                        path: path.to_string(),
                        previous_sha256: previous_entry.sha256.clone(),
                        previous_bytes: previous_entry.bytes,
                        current_sha256: current_entry.sha256.clone(),
                        current_bytes: current_entry.bytes,
                        file_type: current_entry.file_type.clone(),
                    });
                }
            }
        }
    }

    for (&path, &previous_entry) in &previous_map {
        if !current_map.contains_key(path) {
            deleted.push(previous_entry.clone());
        }
    }

    // -----------------------------------------------------------------------
    // Rename detection: match deleted entries with added entries by content hash.
    // When the same SHA-256 appears in both sets, it's a rename, not delete+add.
    // -----------------------------------------------------------------------
    let mut renamed = Vec::new();
    let mut deleted_by_hash: HashMap<&str, usize> = HashMap::new();
    for (i, entry) in deleted.iter().enumerate() {
        deleted_by_hash.entry(entry.sha256.as_str()).or_insert(i);
    }

    let mut rename_added_indices = Vec::new();
    let mut rename_deleted_indices = Vec::new();

    for (ai, added_entry) in added.iter().enumerate() {
        if let Some(&di) = deleted_by_hash.get(added_entry.sha256.as_str()) {
            renamed.push(RenamedEntry {
                old_path: deleted[di].path.clone(),
                new_path: added_entry.path.clone(),
                sha256: added_entry.sha256.clone(),
                bytes: added_entry.bytes,
            });
            rename_added_indices.push(ai);
            rename_deleted_indices.push(di);
            // Remove from hash map so each deleted entry matches at most once.
            deleted_by_hash.remove(added_entry.sha256.as_str());
        }
    }

    // Remove matched entries from added/deleted (iterate in reverse to preserve indices).
    rename_added_indices.sort_unstable();
    rename_deleted_indices.sort_unstable();
    for &i in rename_added_indices.iter().rev() {
        added.remove(i);
    }
    for &i in rename_deleted_indices.iter().rev() {
        deleted.remove(i);
    }

    renamed.sort_by(|a, b| a.new_path.cmp(&b.new_path));

    added.sort_by(|a, b| a.path.cmp(&b.path));
    modified.sort_by(|a, b| a.path.cmp(&b.path));
    deleted.sort_by(|a, b| a.path.cmp(&b.path));

    SnapshotDiff {
        previous_fingerprint: previous.fingerprint.clone(),
        current_fingerprint: current.fingerprint.clone(),
        added,
        modified,
        deleted,
        renamed,
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
            renamed_paths: diff
                .renamed
                .iter()
                .map(|r| RenamedPathEvent {
                    old_path: r.old_path.clone(),
                    new_path: r.new_path.clone(),
                })
                .collect(),
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
    while rx.recv().is_ok() {
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
            renamed_paths: diff
                .renamed
                .iter()
                .map(|r| RenamedPathEvent {
                    old_path: r.old_path.clone(),
                    new_path: r.new_path.clone(),
                })
                .collect(),
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
const MAX_HISTORY_ENTRIES: usize = 256;

/// Cached .rbxl DOM state, shared between MCP tools and HTTP endpoints.
pub struct RbxlDomCache {
    pub dom: Option<rbx_dom_weak::WeakDom>,
    pub ref_map: HashMap<String, rbx_dom_weak::types::Ref>,
    pub loaded_path: Option<PathBuf>,
}

impl Default for RbxlDomCache {
    fn default() -> Self {
        Self::new()
    }
}

impl RbxlDomCache {
    pub fn new() -> Self {
        Self {
            dom: None,
            ref_map: HashMap::new(),
            loaded_path: None,
        }
    }
}

/// Thread-safe state for the serve command.
pub struct ServerState {
    pub server_id: String,
    pub root: PathBuf,
    pub canonical_root: PathBuf,
    pub project_path: PathBuf,
    pub includes: Vec<String>,
    pub current: Mutex<Arc<Snapshot>>,
    pub history: Mutex<BTreeMap<String, Arc<Snapshot>>>,
    pub history_order: Mutex<VecDeque<String>>,
    pub history_rows: Mutex<VecDeque<HistoryEntry>>,
    pub history_timestamps: Mutex<BTreeMap<String, String>>,
    pub tx: tokio::sync::broadcast::Sender<SyncDiffEvent>,
    pub patch_lock: tokio::sync::Mutex<()>,
    pub sequence: Mutex<u64>,
    pub cache: Mutex<SnapshotCache>,
    pub metrics: Arc<Metrics>,
    /// Cached .rbxl DOM for MCP tool access.
    pub rbxl: Mutex<RbxlDomCache>,
    /// Whether turbo mode is active (shorter coalesce window).
    pub turbo: bool,
    /// Coalesce window in milliseconds.
    pub coalesce_ms: u64,
    /// Whether binary model support is enabled.
    pub binary_models: bool,
    /// Authoritative readiness records for edit_sync, preview, and full-bake targets.
    pub readiness: Mutex<ReadinessState>,
    /// Broadcast readiness updates for SSE consumers.
    readiness_events_tx: tokio::sync::broadcast::Sender<ReadinessEvent>,
    /// Cached model manifests, keyed by content SHA-256.
    pub model_cache: Mutex<ModelManifestCache>,
    /// Server boot timestamp (set once at construction, never changes).
    pub boot_time: Instant,
    /// Latest plugin state reported via POST /plugin/state.
    pub plugin_state: Mutex<Option<serde_json::Value>>,
    /// Wall-clock instant of the last plugin state report.
    pub plugin_state_at: Mutex<Option<Instant>>,
    /// Latest plugin managed index reported via POST /plugin/managed.
    pub plugin_managed: Mutex<Option<serde_json::Value>>,
    /// Wall-clock instant of the last plugin managed index report.
    pub plugin_managed_at: Mutex<Option<Instant>>,
    /// Compiled glob ignore patterns from the project file.
    pub glob_ignores: GlobIgnoreSet,
    /// Pending commands for the Studio plugin (bounded FIFO).
    pub plugin_commands: Mutex<VecDeque<PluginCommand>>,
    /// Acknowledgments from the plugin, keyed by command ID.
    pub plugin_command_acks: Mutex<HashMap<String, PluginCommandAck>>,
    /// Reference to the filesystem event coalescer (set by the serve loop).
    pub coalescer: Mutex<Option<Arc<EventCoalescer>>>,
}

#[derive(Debug, Clone)]
pub struct ServerStateOptions {
    pub channel_capacity: usize,
    pub turbo: bool,
    pub coalesce_ms: u64,
    pub binary_models: bool,
    pub glob_ignores: GlobIgnoreSet,
    pub project_path: Option<PathBuf>,
}

impl ServerState {
    pub fn new(
        root: PathBuf,
        includes: Vec<String>,
        initial: Snapshot,
        channel_capacity: usize,
    ) -> Arc<Self> {
        Self::with_config(root, includes, initial, channel_capacity, false, 50, false)
    }

    pub fn with_config(
        root: PathBuf,
        includes: Vec<String>,
        initial: Snapshot,
        channel_capacity: usize,
        turbo: bool,
        coalesce_ms: u64,
        binary_models: bool,
    ) -> Arc<Self> {
        Self::with_full_config(
            root,
            includes,
            initial,
            ServerStateOptions {
                channel_capacity,
                turbo,
                coalesce_ms,
                binary_models,
                glob_ignores: GlobIgnoreSet::empty(),
                project_path: None,
            },
        )
    }

    pub fn with_full_config(
        root: PathBuf,
        includes: Vec<String>,
        initial: Snapshot,
        options: ServerStateOptions,
    ) -> Arc<Self> {
        let capacity = options.channel_capacity.clamp(32, 16_384);
        let (tx, _rx) = tokio::sync::broadcast::channel::<SyncDiffEvent>(capacity);
        let metrics = Arc::new(Metrics::new());
        let (readiness_events_tx, _readiness_events_rx) =
            tokio::sync::broadcast::channel::<ReadinessEvent>(capacity);
        metrics
            .entries
            .store(initial.entries.len() as u64, Ordering::Relaxed);
        let arc = Arc::new(initial);
        let mut history = BTreeMap::new();
        let mut history_order = VecDeque::new();
        let history_rows = VecDeque::new();
        let mut history_timestamps = BTreeMap::new();
        history.insert(arc.fingerprint.clone(), Arc::clone(&arc));
        history_order.push_back(arc.fingerprint.clone());
        history_timestamps.insert(arc.fingerprint.clone(), chrono::Utc::now().to_rfc3339());
        let canonical_root = fs::canonicalize(&root).unwrap_or_else(|_| root.clone());
        let project_path = options
            .project_path
            .unwrap_or_else(|| root.join("default.project.json"));

        Arc::new(Self {
            server_id: uuid::Uuid::new_v4().to_string(),
            root,
            canonical_root,
            project_path,
            includes,
            current: Mutex::new(arc),
            history: Mutex::new(history),
            history_order: Mutex::new(history_order),
            history_rows: Mutex::new(history_rows),
            history_timestamps: Mutex::new(history_timestamps),
            tx,
            patch_lock: tokio::sync::Mutex::new(()),
            sequence: Mutex::new(0),
            cache: Mutex::new(SnapshotCache::new()),
            metrics,
            rbxl: Mutex::new(RbxlDomCache::new()),
            turbo: options.turbo,
            coalesce_ms: options.coalesce_ms,
            binary_models: options.binary_models,
            readiness: Mutex::new(ReadinessState::new()),
            readiness_events_tx,
            model_cache: Mutex::new(ModelManifestCache::new()),
            boot_time: Instant::now(),
            plugin_state: Mutex::new(None),
            plugin_state_at: Mutex::new(None),
            plugin_managed: Mutex::new(None),
            plugin_managed_at: Mutex::new(None),
            glob_ignores: options.glob_ignores,
            plugin_commands: Mutex::new(VecDeque::new()),
            plugin_command_acks: Mutex::new(HashMap::new()),
            coalescer: Mutex::new(None),
        })
    }

    /// Garbage-collect expired plugin commands (older than TTL).
    pub fn gc_plugin_commands(&self) {
        let mut queue = self
            .plugin_commands
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        queue.retain(|cmd| {
            cmd.created_at
                .map(|t| t.elapsed() < PLUGIN_COMMAND_TTL)
                .unwrap_or(false)
        });
        // Also GC old acks (keep for 2x TTL at most).
        let ttl2 = PLUGIN_COMMAND_TTL * 2;
        let mut acks = self
            .plugin_command_acks
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        // Acks don't have timestamps, but we bound the map size.
        if acks.len() > PLUGIN_COMMAND_QUEUE_CAPACITY * 2 {
            // Remove oldest half (HashMap doesn't preserve order, so just truncate).
            let to_remove = acks.len() - PLUGIN_COMMAND_QUEUE_CAPACITY;
            let keys: Vec<String> = acks.keys().take(to_remove).cloned().collect();
            for k in keys {
                acks.remove(&k);
            }
        }
        let _ = ttl2; // suppress unused warning
    }

    /// Drain all pending plugin commands (for the POST /plugin/state response).
    /// Also runs GC first.
    pub fn drain_plugin_commands(&self) -> Vec<PluginCommand> {
        self.gc_plugin_commands();
        let mut queue = self
            .plugin_commands
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        queue.drain(..).collect()
    }

    fn record_plugin_command_rejection(
        &self,
        command_id: &str,
        rejection: ReadinessRejection,
    ) {
        let mut acks = self
            .plugin_command_acks
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        acks.insert(
            command_id.to_string(),
            PluginCommandAck {
                command_id: command_id.to_string(),
                success: false,
                message: format!("readiness precondition rejected: {rejection:?}"),
            },
        );
    }

    /// Drain pending plugin commands, dropping readiness-stale commands before
    /// they reach the Studio plugin and recording a rejection ack for them.
    pub fn drain_ready_plugin_commands(&self) -> Vec<PluginCommand> {
        self.gc_plugin_commands();
        let drained = {
            let mut queue = self
                .plugin_commands
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            queue.drain(..).collect::<Vec<_>>()
        };

        let mut accepted = Vec::with_capacity(drained.len());
        for command in drained {
            if let Some(readiness) = &command.readiness {
                let expectation = readiness.to_readiness_expectation();
                match self.validate_readiness_expectation(
                    readiness.expected_target,
                    &expectation,
                ) {
                    Ok(()) => accepted.push(command),
                    Err(rejection) => {
                        self.record_plugin_command_rejection(&command.id, rejection);
                    }
                }
            } else {
                accepted.push(command);
            }
        }

        accepted
    }

    pub fn current_readiness(&self, target: ReadinessTarget) -> ReadinessRecord {
        self.readiness
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .current_readiness(target)
    }

    pub fn current_readiness_snapshot(&self, target: ReadinessTarget) -> (u64, ReadinessRecord) {
        self.readiness
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .current_readiness_snapshot(target)
    }

    pub fn update_readiness(&self, record: ReadinessRecord) -> Result<(), ReadinessRejection> {
        let mut readiness = self.readiness.lock().unwrap_or_else(|e| e.into_inner());
        let result = readiness.update_readiness(record);
        if result.is_ok() {
            let (sequence, records) = readiness.next_readiness_snapshot();
            drop(readiness);
            self.broadcast_readiness_snapshot(sequence, records);
        }
        result
    }

    pub fn advance_readiness_epoch_if_invalidated(
        &self,
        target: ReadinessTarget,
        invalidated: bool,
    ) -> ReadinessRecord {
        let mut readiness = self.readiness.lock().unwrap_or_else(|e| e.into_inner());
        let record = readiness.advance_epoch_if_invalidated(target, invalidated);
        if invalidated {
            let (sequence, records) = readiness.next_readiness_snapshot();
            drop(readiness);
            self.broadcast_readiness_snapshot(sequence, records);
        }
        record
    }

    pub fn rotate_readiness_incarnation(&self, reason: impl Into<String>) -> String {
        let mut readiness = self.readiness.lock().unwrap_or_else(|e| e.into_inner());
        let incarnation_id = readiness.rotate_incarnation(reason);
        let (sequence, records) = readiness.next_readiness_snapshot();
        drop(readiness);
        self.broadcast_readiness_snapshot(sequence, records);
        incarnation_id
    }

    pub fn validate_readiness_expectation(
        &self,
        expected_target: ReadinessTarget,
        expectation: &ReadinessExpectation,
    ) -> Result<(), ReadinessRejection> {
        self.readiness
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .validate_expectation(expected_target, expectation)
    }

    pub fn record_successful_full_bake_start_for_current_incarnation(
        &self,
    ) -> Result<(), ReadinessRejection> {
        self.readiness
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .record_successful_full_bake_start_for_current_incarnation()
    }

    pub(crate) fn readiness_events(&self) -> tokio::sync::broadcast::Receiver<ReadinessEvent> {
        self.readiness_events_tx.subscribe()
    }

    fn broadcast_readiness_snapshot(&self, sequence: u64, records: Vec<ReadinessRecord>) {
        for record in records {
            let _ = self
                .readiness_events_tx
                .send(ReadinessEvent { sequence, record });
        }
    }

    /// Return up to `limit` cached history rows in chronological order
    /// (oldest to newest), without cloning the entire ring buffer.
    pub fn recent_history_rows(&self, limit: usize) -> Vec<HistoryEntry> {
        let rows = self.history_rows.lock().unwrap_or_else(|e| e.into_inner());
        let take = rows.len().min(limit);
        let mut selected: Vec<HistoryEntry> = rows.iter().rev().take(take).cloned().collect();
        selected.reverse();
        selected
    }

    /// Resolve the current snapshot, the `live` alias, or an exact historical
    /// snapshot by fingerprint.
    pub fn snapshot_ref_at(&self, at: Option<&str>) -> Option<Arc<Snapshot>> {
        match at {
            None | Some("live") => {
                let lock = self.current.lock().unwrap_or_else(|e| e.into_inner());
                Some(Arc::clone(&lock))
            }
            Some(fingerprint) => {
                let history = self.history.lock().unwrap_or_else(|e| e.into_inner());
                history.get(fingerprint).cloned()
            }
        }
    }

    /// Compute the diff from a historical snapshot to the current snapshot.
    pub fn diff_since_fingerprint(&self, fingerprint: &str) -> Option<SnapshotDiff> {
        let previous = {
            let history = self.history.lock().unwrap_or_else(|e| e.into_inner());
            history.get(fingerprint).cloned()
        }?;

        let current = {
            let lock = self.current.lock().unwrap_or_else(|e| e.into_inner());
            Arc::clone(&lock)
        };

        Some(diff_snapshots(&previous, &current))
    }

    /// Poll source tree and broadcast diff if changed.
    /// Uses incremental `SnapshotCache` so only changed files are re-hashed.
    pub fn poll_and_broadcast(&self) -> Result<()> {
        let start = Instant::now();
        let new_snapshot = {
            let mut cache_lock = self.cache.lock().unwrap_or_else(|e| e.into_inner());
            build_snapshot_cached_with_ignores(
                &self.root,
                &self.includes,
                &mut cache_lock,
                &self.metrics,
                &self.glob_ignores,
            )?
        };
        let elapsed = start.elapsed();

        // Record poll metrics.
        self.metrics.polls.fetch_add(1, Ordering::Relaxed);
        self.metrics
            .poll_duration_sum_us
            .fetch_add(elapsed.as_micros() as u64, Ordering::Relaxed);

        if elapsed.as_millis() > 50 {
            eprintln!(
                "[vertigo-sync] slow poll: {elapsed:?} ({} entries)",
                new_snapshot.entries.len()
            );
        }
        self.install_snapshot_and_broadcast(new_snapshot)?;
        Ok(())
    }

    /// Install a freshly built snapshot into shared state and broadcast a diff
    /// event when the fingerprint changes.
    pub fn install_snapshot_and_broadcast(&self, new_snapshot: Snapshot) -> Result<()> {
        let current = {
            let lock = self.current.lock().unwrap_or_else(|e| e.into_inner());
            Arc::clone(&lock)
        };

        if new_snapshot.fingerprint == current.fingerprint {
            self.metrics
                .entries
                .store(new_snapshot.entries.len() as u64, Ordering::Relaxed);
            return Ok(());
        }

        let diff = diff_snapshots(&current, &new_snapshot);
        let new_arc = Arc::new(new_snapshot);

        let seq = {
            let mut lock = self.sequence.lock().unwrap_or_else(|e| e.into_inner());
            *lock += 1;
            *lock
        };

        let event = SyncDiffEvent {
            sequence_id: seq,
            source_hash: new_arc.fingerprint.clone(),
            added_paths: diff.added.iter().map(|f| f.path.clone()).collect(),
            modified_paths: diff.modified.iter().map(|f| f.path.clone()).collect(),
            deleted_paths: diff.deleted.iter().map(|f| f.path.clone()).collect(),
            renamed_paths: diff
                .renamed
                .iter()
                .map(|r| RenamedPathEvent {
                    old_path: r.old_path.clone(),
                    new_path: r.new_path.clone(),
                })
                .collect(),
            timestamp: chrono::Utc::now().to_rfc3339(),
        };

        {
            let mut lock = self.current.lock().unwrap_or_else(|e| e.into_inner());
            *lock = Arc::clone(&new_arc);
        }
        {
            let mut hist_lock = self.history.lock().unwrap_or_else(|e| e.into_inner());
            let mut order_lock = self.history_order.lock().unwrap_or_else(|e| e.into_inner());
            let mut rows_lock = self.history_rows.lock().unwrap_or_else(|e| e.into_inner());
            let mut ts_lock = self
                .history_timestamps
                .lock()
                .unwrap_or_else(|e| e.into_inner());

            let fp = new_arc.fingerprint.clone();
            if !hist_lock.contains_key(&fp) {
                let timestamp = event.timestamp.clone();
                let history_seq = order_lock.len() as u64;
                let (scope, geometry_affecting) = classify_history_diff(&diff);
                hist_lock.insert(fp.clone(), Arc::clone(&new_arc));
                order_lock.push_back(fp.clone());
                rows_lock.push_back(HistoryEntry {
                    seq: history_seq,
                    fingerprint: fp.clone(),
                    timestamp: timestamp.clone(),
                    added: diff.added.len(),
                    modified: diff.modified.len(),
                    deleted: diff.deleted.len(),
                    geometry_affecting,
                    scope,
                });
                ts_lock.insert(fp, timestamp);

                // Evict oldest entries beyond the limit.
                while order_lock.len() > MAX_HISTORY_ENTRIES {
                    if let Some(oldest) = order_lock.pop_front() {
                        hist_lock.remove(&oldest);
                        ts_lock.remove(&oldest);
                        rows_lock.pop_front();
                    }
                }
            }
        }

        self.metrics
            .entries
            .store(new_arc.entries.len() as u64, Ordering::Relaxed);
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

fn collect_files(
    root: &Path,
    current: &Path,
    output: &mut Vec<PathBuf>,
    ignores: &GlobIgnoreSet,
) -> Result<()> {
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
        // Apply glob ignore check on the normalized relative path.
        if !ignores.is_empty() {
            let rel_str = normalize_path(relative);
            if ignores.is_ignored(&rel_str) {
                return Ok(());
            }
        }
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

        children.sort_by_key(|a| normalize_path(a));
        for child in children {
            collect_files(root, &child, output, ignores)?;
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

/// Classify a file's type from its path for the `file_type` field.
/// Zero heap allocation — uses byte-level ASCII-insensitive suffix matching.
fn classify_file_type(path: &str) -> &'static str {
    let bytes = path.as_bytes();
    let len = bytes.len();

    // Helper: case-insensitive suffix check without allocation.
    #[inline(always)]
    fn ends_with_ci(bytes: &[u8], suffix: &[u8]) -> bool {
        if bytes.len() < suffix.len() {
            return false;
        }
        let start = bytes.len() - suffix.len();
        bytes[start..]
            .iter()
            .zip(suffix.iter())
            .all(|(a, b)| a.to_ascii_lowercase() == *b)
    }

    // Check longest suffixes first to avoid false matches (.model.json / .meta.json before .json).
    if len >= 11 && ends_with_ci(bytes, b".model.json") {
        "model_json"
    } else if len >= 10 && ends_with_ci(bytes, b".meta.json") {
        "meta_json"
    } else if len >= 5 && ends_with_ci(bytes, b".luau") {
        "luau"
    } else if len >= 4 && ends_with_ci(bytes, b".lua") {
        "lua"
    } else if len >= 6 && ends_with_ci(bytes, b".rbxmx") {
        "rbxmx"
    } else if len >= 5 && ends_with_ci(bytes, b".rbxm") {
        "rbxm"
    } else if len >= 6 && ends_with_ci(bytes, b".jsonc") {
        "jsonc"
    } else if len >= 5 && ends_with_ci(bytes, b".json") {
        "json"
    } else if len >= 4 && ends_with_ci(bytes, b".txt") {
        "txt"
    } else if len >= 4 && ends_with_ci(bytes, b".csv") {
        "csv"
    } else if (len >= 5 && ends_with_ci(bytes, b".yaml"))
        || (len >= 4 && ends_with_ci(bytes, b".yml"))
    {
        "yaml"
    } else if len >= 5 && ends_with_ci(bytes, b".toml") {
        "toml"
    } else {
        "other"
    }
}

/// Strip `//` single-line and `/* */` block comments from JSONC content,
/// producing valid JSON. Correctly handles comments inside quoted strings
/// (does not strip them). Zero external dependencies.
pub fn strip_json_comments(input: &str) -> String {
    let mut output = String::with_capacity(input.len());
    let bytes = input.as_bytes();
    let len = bytes.len();
    let mut i = 0;
    let mut in_string = false;
    let mut escape_next = false;

    while i < len {
        if escape_next {
            output.push(bytes[i] as char);
            escape_next = false;
            i += 1;
            continue;
        }

        if in_string {
            if bytes[i] == b'\\' {
                escape_next = true;
                output.push('\\');
            } else if bytes[i] == b'"' {
                in_string = false;
                output.push('"');
            } else {
                output.push(bytes[i] as char);
            }
            i += 1;
            continue;
        }

        // Not in string
        if bytes[i] == b'"' {
            in_string = true;
            output.push('"');
            i += 1;
        } else if i + 1 < len && bytes[i] == b'/' && bytes[i + 1] == b'/' {
            // Single-line comment: skip to end of line
            i += 2;
            while i < len && bytes[i] != b'\n' {
                i += 1;
            }
        } else if i + 1 < len && bytes[i] == b'/' && bytes[i + 1] == b'*' {
            // Block comment: skip to */
            i += 2;
            while i + 1 < len && !(bytes[i] == b'*' && bytes[i + 1] == b'/') {
                i += 1;
            }
            if i + 1 < len {
                i += 2; // skip */
            }
        } else {
            output.push(bytes[i] as char);
            i += 1;
        }
    }

    output
}

/// Parse a `.meta.json` sidecar file into an `InstanceMeta`.
pub fn parse_meta_json(content: &str) -> Result<InstanceMeta> {
    let raw: serde_json::Value =
        serde_json::from_str(content).context("failed to parse .meta.json")?;
    let properties = match raw.get("properties") {
        Some(serde_json::Value::Object(map)) => {
            map.iter().map(|(k, v)| (k.clone(), v.clone())).collect()
        }
        _ => BTreeMap::new(),
    };
    let attributes = match raw.get("attributes") {
        Some(serde_json::Value::Object(map)) => {
            Some(map.iter().map(|(k, v)| (k.clone(), v.clone())).collect())
        }
        _ => None,
    };
    Ok(InstanceMeta {
        properties,
        attributes,
    })
}

/// Attach `.meta.json` sidecar metadata to snapshot entries.
/// For each entry ending in `.luau` or `.lua`, checks if a sibling `.meta.json`
/// exists in the snapshot and reads+parses it lazily from disk.
pub fn attach_meta_json(root: &Path, entries: &mut [SnapshotEntry]) {
    // Build a set of paths that are .meta.json files for quick lookup.
    let meta_paths: HashSet<String> = entries
        .iter()
        .filter(|e| e.path.ends_with(".meta.json"))
        .map(|e| e.path.clone())
        .collect();

    for entry in entries.iter_mut() {
        if entry.meta.is_some() {
            continue;
        }
        // For Luau/Lua files, check for a sibling .meta.json.
        // .server.luau and .client.luau are already covered by the .luau check.
        let ext_match = entry.path.ends_with(".luau") || entry.path.ends_with(".lua");
        if !ext_match {
            continue;
        }

        // Derive the expected .meta.json path: foo.luau -> foo.meta.json
        let meta_path = derive_meta_json_path(&entry.path);
        if let Some(meta_path) = meta_path
            && meta_paths.contains(&meta_path)
        {
            let abs = root.join(&meta_path);
            if let Ok(content) = fs::read_to_string(&abs)
                && let Ok(meta) = parse_meta_json(&content)
            {
                entry.meta = Some(meta);
            }
        }
    }
}

/// Derive the `.meta.json` sidecar path from a source file path.
/// Example: `src/Server/Foo.server.luau` -> `src/Server/Foo.server.luau.meta.json`
/// (Rojo convention: the meta file name is `{filename}.meta.json`)
fn derive_meta_json_path(source_path: &str) -> Option<String> {
    // Rojo meta convention: filename.meta.json sits next to the file.
    // For "init.server.luau" inside a directory, the meta is on the directory itself,
    // but we handle the simple case: {path}.meta.json
    // Actually Rojo convention for files is: `Foo.luau` -> `Foo.meta.json`
    // and for init scripts: `init.meta.json` in the same directory
    // Let's handle: strip extension, add .meta.json
    if let Some(stem) = source_path.strip_suffix(".server.luau") {
        Some(format!("{stem}.server.luau.meta.json"))
    } else if let Some(stem) = source_path.strip_suffix(".client.luau") {
        Some(format!("{stem}.client.luau.meta.json"))
    } else if let Some(stem) = source_path.strip_suffix(".luau") {
        Some(format!("{stem}.meta.json"))
    } else {
        source_path
            .strip_suffix(".lua")
            .map(|stem| format!("{stem}.meta.json"))
    }
}

// ---------------------------------------------------------------------------
// Model manifest cache — lazy deserialization of .rbxm/.rbxmx files
// ---------------------------------------------------------------------------

/// A single instance from a binary model file.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ModelInstance {
    pub index: usize,
    pub parent_index: Option<usize>,
    pub name: String,
    pub class_name: String,
    pub properties: BTreeMap<String, serde_json::Value>,
}

/// Manifest of instances within a binary model file.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ModelManifest {
    pub instances: Vec<ModelInstance>,
    pub root_count: usize,
}

/// Content-addressed cache for lazily deserialized binary model manifests.
/// Keyed by SHA-256 hash of the model file content — only re-parses when
/// the file actually changes.
pub struct ModelManifestCache {
    entries: HashMap<String, ModelManifest>,
}

impl ModelManifestCache {
    pub fn new() -> Self {
        Self {
            entries: HashMap::new(),
        }
    }

    /// Get or lazily deserialize a model manifest.
    /// `content_hash` is the SHA-256 of the file content (from snapshot).
    /// `path` is the absolute path to the .rbxm/.rbxmx file on disk.
    pub fn get_or_load(&mut self, content_hash: &str, path: &Path) -> Result<&ModelManifest> {
        if !self.entries.contains_key(content_hash) {
            let manifest = deserialize_model_manifest(path)?;
            self.entries.insert(content_hash.to_string(), manifest);
        }
        Ok(self.entries.get(content_hash).expect("just inserted"))
    }

    /// Number of cached manifests.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether the cache is empty.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

impl Default for ModelManifestCache {
    fn default() -> Self {
        Self::new()
    }
}

/// Deserialize a .rbxm/.rbxmx file into a `ModelManifest`.
pub fn deserialize_model_manifest(path: &Path) -> Result<ModelManifest> {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();

    let file = File::open(path)
        .with_context(|| format!("failed to open model file {}", path.display()))?;
    let reader = BufReader::new(file);

    let dom = match ext.as_str() {
        "rbxm" => rbx_binary::from_reader(reader)
            .with_context(|| format!("failed to parse binary model {}", path.display()))?,
        "rbxmx" => rbx_xml::from_reader_default(reader)
            .with_context(|| format!("failed to parse XML model {}", path.display()))?,
        _ => anyhow::bail!("unsupported model extension: {ext}"),
    };

    let mut instances = Vec::new();
    let mut ref_to_index: HashMap<rbx_dom_weak::types::Ref, usize> = HashMap::new();
    collect_model_instances(
        &dom,
        dom.root_ref(),
        None,
        &mut instances,
        &mut ref_to_index,
    );

    let root_count = dom.root().children().len();

    Ok(ModelManifest {
        instances,
        root_count,
    })
}

/// Recursively collect instances from a model DOM into flat list.
fn collect_model_instances(
    dom: &rbx_dom_weak::WeakDom,
    inst_ref: rbx_dom_weak::types::Ref,
    parent_index: Option<usize>,
    out: &mut Vec<ModelInstance>,
    ref_to_index: &mut HashMap<rbx_dom_weak::types::Ref, usize>,
) {
    let Some(inst) = dom.get_by_ref(inst_ref) else {
        return;
    };

    let index = out.len();
    ref_to_index.insert(inst_ref, index);

    let mut properties = BTreeMap::new();
    for (key, variant) in &inst.properties {
        // Convert simple property types to JSON values.
        let value = match variant {
            rbx_types::Variant::String(s) => serde_json::Value::String(s.clone()),
            rbx_types::Variant::Bool(b) => serde_json::Value::Bool(*b),
            rbx_types::Variant::Int32(n) => serde_json::json!(*n),
            rbx_types::Variant::Float32(n) => serde_json::json!(*n),
            rbx_types::Variant::Float64(n) => serde_json::json!(*n),
            rbx_types::Variant::Enum(e) => serde_json::json!(e.to_u32()),
            _ => serde_json::Value::String(format!("{:?}", std::mem::discriminant(variant))),
        };
        properties.insert(key.to_string(), value);
    }

    out.push(ModelInstance {
        index,
        parent_index,
        name: inst.name.clone(),
        class_name: inst.class.to_string(),
        properties,
    });

    for &child_ref in inst.children() {
        collect_model_instances(dom, child_ref, Some(index), out, ref_to_index);
    }
}

// ---------------------------------------------------------------------------
// History reading — parse events.jsonl in reverse order
// ---------------------------------------------------------------------------

/// A single entry from the event log, suitable for the /history endpoint.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct HistoryEntry {
    pub seq: u64,
    pub fingerprint: String,
    pub timestamp: String,
    pub added: usize,
    pub modified: usize,
    pub deleted: usize,
    #[serde(default)]
    pub geometry_affecting: bool,
    #[serde(default)]
    pub scope: String,
}

fn is_non_geometry_import_service_path(path: &str) -> bool {
    path.ends_with("/ImportService/DayNightCycle.lua")
        || path.ends_with("/ImportService/SceneAudit.lua")
}

fn is_non_geometry_preview_path(path: &str) -> bool {
    is_non_geometry_import_service_path(path)
        || path.contains("/src/Lighting/")
        || path.ends_with("/src/Lighting")
}

pub fn classify_history_path(path: &str) -> (&'static str, bool) {
    if path.contains("AustinManifest")
        || path.contains("AustinPreviewManifest")
        || path.contains("StudioPreview/AustinPreviewBuilder.lua")
    {
        return ("geometry", true);
    }
    if path.contains("/ImportService/") {
        if is_non_geometry_preview_path(path) {
            return ("code", false);
        }
        return ("geometry", true);
    }
    if is_non_geometry_preview_path(path) {
        return ("code", false);
    }
    if path.contains("assets/plugin_src/")
        || path.contains("assets/VertigoSyncPlugin.lua")
        || path.contains("/branding/")
    {
        return ("plugin", false);
    }
    if path.starts_with("docs/")
        || path == "README.md"
        || path == "CONTRIBUTING.md"
        || path == "LICENSE"
    {
        return ("docs", false);
    }
    ("code", false)
}

pub fn classify_history_diff(diff: &SnapshotDiff) -> (String, bool) {
    let mut saw_geometry = false;
    let mut saw_plugin = false;
    let mut saw_docs = false;
    let mut saw_code = false;

    let mut classify = |path: &str| {
        let (scope, geometry) = classify_history_path(path);
        saw_geometry |= geometry;
        match scope {
            "geometry" => {}
            "plugin" => saw_plugin = true,
            "docs" => saw_docs = true,
            _ => saw_code = true,
        }
    };

    for entry in &diff.added {
        classify(&entry.path);
    }
    for entry in &diff.modified {
        classify(&entry.path);
    }
    for entry in &diff.deleted {
        classify(&entry.path);
    }
    for entry in &diff.renamed {
        classify(&entry.old_path);
        classify(&entry.new_path);
    }

    let scope = if saw_geometry {
        "geometry"
    } else if saw_plugin && !(saw_docs || saw_code) {
        "plugin"
    } else if saw_docs && !(saw_plugin || saw_code) {
        "docs"
    } else if saw_plugin || saw_docs || saw_code {
        "mixed"
    } else {
        "unknown"
    };

    (scope.to_string(), saw_geometry)
}

/// Read the most recent `limit` entries from the event log (NDJSON).
/// Returns entries in reverse chronological order (newest first).
pub fn read_history(event_log_path: &Path, limit: usize) -> Result<Vec<HistoryEntry>> {
    if !event_log_path.exists() {
        return Ok(Vec::new());
    }
    let file = File::open(event_log_path)
        .with_context(|| format!("failed to open event log {}", event_log_path.display()))?;
    let reader = BufReader::new(file);

    let mut entries: Vec<HistoryEntry> = Vec::new();
    for line in reader.lines() {
        let line = line?;
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let value: serde_json::Value = match serde_json::from_str(trimmed) {
            Ok(v) => v,
            Err(_) => continue,
        };

        let seq = value
            .get("seq")
            .and_then(|v| v.as_u64())
            .or_else(|| value.get("sequence_id").and_then(|v| v.as_u64()))
            .unwrap_or(0);

        let fingerprint = value
            .get("snapshot_hash")
            .or_else(|| value.get("source_hash"))
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();

        let timestamp = value
            .get("timestamp_utc")
            .or_else(|| value.get("timestamp"))
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();

        let added = value
            .get("diff")
            .and_then(|d| d.get("added"))
            .and_then(|v| v.as_u64())
            .unwrap_or(0) as usize;
        let modified = value
            .get("diff")
            .and_then(|d| d.get("modified"))
            .and_then(|v| v.as_u64())
            .unwrap_or(0) as usize;
        let deleted = value
            .get("diff")
            .and_then(|d| d.get("deleted"))
            .and_then(|v| v.as_u64())
            .unwrap_or(0) as usize;

        entries.push(HistoryEntry {
            seq,
            fingerprint,
            timestamp,
            added,
            modified,
            deleted,
            geometry_affecting: false,
            scope: String::new(),
        });
    }

    // Return newest first, limited to `limit`.
    entries.reverse();
    entries.truncate(limit);
    Ok(entries)
}

// ---------------------------------------------------------------------------
// Reverse diff computation — swap adds/deletes for rewind support
// ---------------------------------------------------------------------------

/// Compute a reverse diff that undoes `diff`. Swaps added<->deleted and
/// inverts modified entry directions.
pub fn reverse_diff(diff: &SnapshotDiff) -> SnapshotDiff {
    let reversed_modified: Vec<ModifiedEntry> = diff
        .modified
        .iter()
        .map(|m| ModifiedEntry {
            path: m.path.clone(),
            previous_sha256: m.current_sha256.clone(),
            previous_bytes: m.current_bytes,
            current_sha256: m.previous_sha256.clone(),
            current_bytes: m.previous_bytes,
            file_type: m.file_type.clone(),
        })
        .collect();

    // Reverse renames: swap old_path <-> new_path
    let reversed_renamed: Vec<RenamedEntry> = diff
        .renamed
        .iter()
        .map(|r| RenamedEntry {
            old_path: r.new_path.clone(),
            new_path: r.old_path.clone(),
            sha256: r.sha256.clone(),
            bytes: r.bytes,
        })
        .collect();

    SnapshotDiff {
        previous_fingerprint: diff.current_fingerprint.clone(),
        current_fingerprint: diff.previous_fingerprint.clone(),
        added: diff.deleted.clone(),
        modified: reversed_modified,
        deleted: diff.added.clone(),
        renamed: reversed_renamed,
    }
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
                    meta: None,
                    file_type: None,
                },
                SnapshotEntry {
                    path: "src/b.lua".to_string(),
                    sha256: "hash_b_old".to_string(),
                    bytes: 2,
                    meta: None,
                    file_type: None,
                },
                SnapshotEntry {
                    path: "src/c.lua".to_string(),
                    sha256: "hash_c".to_string(),
                    bytes: 3,
                    meta: None,
                    file_type: None,
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
                    meta: None,
                    file_type: None,
                },
                SnapshotEntry {
                    path: "src/b.lua".to_string(),
                    sha256: "hash_b_new".to_string(),
                    bytes: 20,
                    meta: None,
                    file_type: None,
                },
                SnapshotEntry {
                    path: "src/d.lua".to_string(),
                    sha256: "hash_d".to_string(),
                    bytes: 4,
                    meta: None,
                    file_type: None,
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
                meta: None,
                file_type: None,
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
        fs::write(dir.join("bad.luau"), [0xFF, 0xFE, 0x00, 0x01]).expect("write");

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

    // -----------------------------------------------------------------------
    // InstanceMeta / meta.json tests
    // -----------------------------------------------------------------------

    #[test]
    fn parse_meta_json_full() {
        let content = r#"{
            "properties": {"Disabled": true, "RunContext": "Client"},
            "attributes": {"CustomTag": "value"}
        }"#;
        let meta = parse_meta_json(content).expect("parse");
        assert_eq!(meta.properties.len(), 2);
        assert_eq!(meta.properties["Disabled"], serde_json::json!(true));
        assert_eq!(meta.properties["RunContext"], serde_json::json!("Client"));
        assert!(meta.attributes.is_some());
        let attrs = meta.attributes.unwrap();
        assert_eq!(attrs["CustomTag"], serde_json::json!("value"));
    }

    #[test]
    fn parse_meta_json_properties_only() {
        let content = r#"{"properties": {"Disabled": false}}"#;
        let meta = parse_meta_json(content).expect("parse");
        assert_eq!(meta.properties.len(), 1);
        assert!(meta.attributes.is_none());
    }

    #[test]
    fn parse_meta_json_empty() {
        let content = r#"{}"#;
        let meta = parse_meta_json(content).expect("parse");
        assert!(meta.properties.is_empty());
        assert!(meta.attributes.is_none());
    }

    // -----------------------------------------------------------------------
    // File type classification tests
    // -----------------------------------------------------------------------

    #[test]
    fn classify_file_type_all_variants() {
        assert_eq!(classify_file_type("src/a.luau"), "luau");
        assert_eq!(classify_file_type("src/a.lua"), "lua");
        assert_eq!(classify_file_type("src/a.json"), "json");
        assert_eq!(classify_file_type("src/a.txt"), "txt");
        assert_eq!(classify_file_type("src/a.csv"), "csv");
        assert_eq!(classify_file_type("src/a.rbxm"), "rbxm");
        assert_eq!(classify_file_type("src/a.rbxmx"), "rbxmx");
        assert_eq!(classify_file_type("src/a.meta.json"), "meta_json");
        assert_eq!(classify_file_type("src/a.model.json"), "model_json");
        assert_eq!(classify_file_type("src/a.yaml"), "yaml");
        assert_eq!(classify_file_type("src/a.yml"), "yaml");
        assert_eq!(classify_file_type("src/a.YAML"), "yaml");
        assert_eq!(classify_file_type("src/a.YML"), "yaml");
        assert_eq!(classify_file_type("src/a.toml"), "toml");
        assert_eq!(classify_file_type("src/a.TOML"), "toml");
        assert_eq!(classify_file_type("src/a.py"), "other");
    }

    // -----------------------------------------------------------------------
    // History reading tests
    // -----------------------------------------------------------------------

    #[test]
    fn read_history_empty_file() {
        let root = tempdir().expect("tempdir");
        let log_path = root.path().join("events.jsonl");
        fs::write(&log_path, "").expect("write empty");
        let entries = read_history(&log_path, 50).expect("read");
        assert!(entries.is_empty());
    }

    #[test]
    fn read_history_returns_newest_first() {
        let root = tempdir().expect("tempdir");
        let log_path = root.path().join("events.jsonl");
        let content = r#"{"seq":1,"snapshot_hash":"aaa","timestamp_utc":"2026-01-01T00:00:00Z","diff":{"added":1,"modified":0,"deleted":0}}
{"seq":2,"snapshot_hash":"bbb","timestamp_utc":"2026-01-01T00:01:00Z","diff":{"added":0,"modified":1,"deleted":0}}
{"seq":3,"snapshot_hash":"ccc","timestamp_utc":"2026-01-01T00:02:00Z","diff":{"added":0,"modified":0,"deleted":1}}
"#;
        fs::write(&log_path, content).expect("write");
        let entries = read_history(&log_path, 50).expect("read");
        assert_eq!(entries.len(), 3);
        assert_eq!(entries[0].seq, 3);
        assert_eq!(entries[1].seq, 2);
        assert_eq!(entries[2].seq, 1);
        assert_eq!(entries[0].fingerprint, "ccc");
    }

    #[test]
    fn read_history_respects_limit() {
        let root = tempdir().expect("tempdir");
        let log_path = root.path().join("events.jsonl");
        let content = r#"{"seq":1,"snapshot_hash":"aaa","timestamp_utc":"t1","diff":{"added":1,"modified":0,"deleted":0}}
{"seq":2,"snapshot_hash":"bbb","timestamp_utc":"t2","diff":{"added":0,"modified":1,"deleted":0}}
{"seq":3,"snapshot_hash":"ccc","timestamp_utc":"t3","diff":{"added":0,"modified":0,"deleted":1}}
"#;
        fs::write(&log_path, content).expect("write");
        let entries = read_history(&log_path, 2).expect("read");
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].seq, 3);
        assert_eq!(entries[1].seq, 2);
    }

    #[test]
    fn read_history_nonexistent_file() {
        let root = tempdir().expect("tempdir");
        let log_path = root.path().join("no-such-file.jsonl");
        let entries = read_history(&log_path, 50).expect("read");
        assert!(entries.is_empty());
    }

    // -----------------------------------------------------------------------
    // Reverse diff tests
    // -----------------------------------------------------------------------

    #[test]
    fn reverse_diff_swaps_adds_and_deletes() {
        let diff = SnapshotDiff {
            previous_fingerprint: "old".to_string(),
            current_fingerprint: "new".to_string(),
            added: vec![SnapshotEntry {
                path: "src/new.lua".into(),
                sha256: "h1".into(),
                bytes: 10,
                meta: None,
                file_type: None,
            }],
            modified: vec![ModifiedEntry {
                path: "src/changed.lua".into(),
                previous_sha256: "old_h".into(),
                previous_bytes: 20,
                current_sha256: "new_h".into(),
                current_bytes: 25,
                file_type: Some("lua".to_string()),
            }],
            deleted: vec![SnapshotEntry {
                path: "src/removed.lua".into(),
                sha256: "h2".into(),
                bytes: 15,
                meta: None,
                file_type: None,
            }],
            renamed: vec![],
        };

        let rev = reverse_diff(&diff);
        assert_eq!(rev.previous_fingerprint, "new");
        assert_eq!(rev.current_fingerprint, "old");
        // Added in forward = deleted in reverse
        assert_eq!(rev.deleted.len(), 1);
        assert_eq!(rev.deleted[0].path, "src/new.lua");
        // Deleted in forward = added in reverse
        assert_eq!(rev.added.len(), 1);
        assert_eq!(rev.added[0].path, "src/removed.lua");
        // Modified directions are swapped
        assert_eq!(rev.modified.len(), 1);
        assert_eq!(rev.modified[0].previous_sha256, "new_h");
        assert_eq!(rev.modified[0].current_sha256, "old_h");
    }

    #[test]
    fn snapshot_ref_at_returns_live_and_historical_snapshots() {
        let state = ServerState::new(
            std::env::temp_dir(),
            vec!["src".to_string()],
            Snapshot {
                version: 1,
                include: vec!["src".to_string()],
                fingerprint: "fp-live-1".to_string(),
                entries: vec![],
            },
            32,
        );

        state
            .install_snapshot_and_broadcast(Snapshot {
                version: 1,
                include: vec!["src".to_string()],
                fingerprint: "fp-historical".to_string(),
                entries: vec![SnapshotEntry {
                    path: "src/a.luau".to_string(),
                    sha256: "sha-a".to_string(),
                    bytes: 10,
                    meta: None,
                    file_type: Some("luau".to_string()),
                }],
            })
            .expect("install historical snapshot");

        state
            .install_snapshot_and_broadcast(Snapshot {
                version: 1,
                include: vec!["src".to_string()],
                fingerprint: "fp-live-2".to_string(),
                entries: vec![SnapshotEntry {
                    path: "src/b.luau".to_string(),
                    sha256: "sha-b".to_string(),
                    bytes: 12,
                    meta: None,
                    file_type: Some("luau".to_string()),
                }],
            })
            .expect("install live snapshot");

        assert_eq!(
            state
                .snapshot_ref_at(None)
                .expect("current snapshot")
                .fingerprint,
            "fp-live-2"
        );
        assert_eq!(
            state
                .snapshot_ref_at(Some("live"))
                .expect("live alias snapshot")
                .fingerprint,
            "fp-live-2"
        );
        assert_eq!(
            state
                .snapshot_ref_at(Some("fp-historical"))
                .expect("historical snapshot")
                .fingerprint,
            "fp-historical"
        );
        assert!(state.snapshot_ref_at(Some("missing")).is_none());
    }

    // -----------------------------------------------------------------------
    // Snapshot backward compatibility (entries without meta/file_type)
    // -----------------------------------------------------------------------

    #[test]
    fn snapshot_entry_deserializes_without_meta_fields() {
        let json = r#"{"path":"src/a.lua","sha256":"abc","bytes":42}"#;
        let entry: SnapshotEntry = serde_json::from_str(json).expect("deserialize");
        assert_eq!(entry.path, "src/a.lua");
        assert!(entry.meta.is_none());
        assert!(entry.file_type.is_none());
    }

    #[test]
    fn snapshot_entry_serializes_without_none_fields() {
        let entry = SnapshotEntry {
            path: "src/a.lua".into(),
            sha256: "abc".into(),
            bytes: 42,
            meta: None,
            file_type: None,
        };
        let json = serde_json::to_string(&entry).expect("serialize");
        assert!(!json.contains("meta"));
        assert!(!json.contains("file_type"));
    }

    // -----------------------------------------------------------------------
    // Snapshot includes non-Luau file types
    // -----------------------------------------------------------------------

    #[test]
    fn snapshot_includes_json_txt_csv_files() {
        let root = tempdir().expect("tempdir");
        fs::create_dir_all(root.path().join("src")).expect("create src");
        fs::write(root.path().join("src/config.json"), r#"{"key":"val"}"#).expect("write json");
        fs::write(root.path().join("src/readme.txt"), "hello").expect("write txt");
        fs::write(root.path().join("src/locale.csv"), "key,en\nhello,Hello").expect("write csv");
        fs::write(root.path().join("src/main.luau"), "--!strict\nreturn {}").expect("write luau");

        let includes = vec!["src".to_string()];
        let snap = build_snapshot(root.path(), &includes).expect("snapshot");
        let paths: Vec<&str> = snap.entries.iter().map(|e| e.path.as_str()).collect();
        assert!(paths.contains(&"src/config.json"));
        assert!(paths.contains(&"src/readme.txt"));
        assert!(paths.contains(&"src/locale.csv"));
        assert!(paths.contains(&"src/main.luau"));
    }

    // -----------------------------------------------------------------------
    // Meta.json attachment tests
    // -----------------------------------------------------------------------

    #[test]
    fn meta_json_attaches_to_sibling_luau() {
        let root = tempdir().expect("tempdir");
        fs::create_dir_all(root.path().join("src")).expect("create src");
        fs::write(root.path().join("src/Foo.luau"), "return {}").expect("write luau");
        fs::write(
            root.path().join("src/Foo.meta.json"),
            r#"{"properties":{"Disabled":true}}"#,
        )
        .expect("write meta");

        let includes = vec!["src".to_string()];
        let snap = build_snapshot(root.path(), &includes).expect("snapshot");

        let foo_entry = snap.entries.iter().find(|e| e.path == "src/Foo.luau");
        assert!(foo_entry.is_some());
        let meta = &foo_entry.unwrap().meta;
        assert!(meta.is_some());
        assert_eq!(
            meta.as_ref().unwrap().properties["Disabled"],
            serde_json::json!(true)
        );
    }

    // -----------------------------------------------------------------------
    // Model manifest cache tests
    // -----------------------------------------------------------------------

    #[test]
    fn model_manifest_cache_empty() {
        let cache = ModelManifestCache::new();
        assert!(cache.is_empty());
        assert_eq!(cache.len(), 0);
    }

    // -----------------------------------------------------------------------
    // Extended resolve_instance_class tests (covered in project.rs tests)
    // -----------------------------------------------------------------------

    // -----------------------------------------------------------------------
    // globIgnorePaths filtering
    // -----------------------------------------------------------------------

    #[test]
    fn glob_ignore_paths_filters_files() {
        let root = tempdir().expect("tempdir");
        let src = root.path().join("src");
        fs::create_dir_all(src.join("tests")).expect("mkdir tests");
        fs::write(src.join("init.luau"), "return {}").expect("write init");
        fs::write(src.join("tests/foo.spec.luau"), "-- test").expect("write spec");
        fs::write(src.join("tests/bar.luau"), "-- bar").expect("write bar");
        fs::write(src.join("game.luau"), "-- game").expect("write game");

        let ignores = GlobIgnoreSet::new(&["**/*.spec.luau".to_string()]);
        let includes = vec!["src".to_string()];
        let snapshot =
            build_snapshot_with_ignores(root.path(), &includes, &ignores).expect("snapshot");

        let paths: Vec<&str> = snapshot.entries.iter().map(|e| e.path.as_str()).collect();
        assert!(
            !paths.iter().any(|p| p.contains("spec")),
            "spec files should be excluded, got: {:?}",
            paths
        );
        assert!(paths.iter().any(|p| p.contains("init.luau")));
        assert!(paths.iter().any(|p| p.contains("bar.luau")));
        assert!(paths.iter().any(|p| p.contains("game.luau")));
    }

    #[test]
    fn glob_ignore_paths_multiple_patterns() {
        let root = tempdir().expect("tempdir");
        let src = root.path().join("src");
        let vendor = src.join("vendor");
        fs::create_dir_all(&vendor).expect("mkdir vendor");
        fs::write(src.join("main.luau"), "-- main").expect("write");
        fs::write(src.join("test.spec.luau"), "-- test").expect("write");
        fs::write(vendor.join("lib.luau"), "-- vendor lib").expect("write");

        let ignores =
            GlobIgnoreSet::new(&["**/*.spec.luau".to_string(), "src/vendor/**".to_string()]);
        let includes = vec!["src".to_string()];
        let snapshot =
            build_snapshot_with_ignores(root.path(), &includes, &ignores).expect("snapshot");

        let paths: Vec<&str> = snapshot.entries.iter().map(|e| e.path.as_str()).collect();
        assert_eq!(paths, vec!["src/main.luau"]);
    }

    #[test]
    fn glob_ignore_set_empty_matches_nothing() {
        let ignores = GlobIgnoreSet::empty();
        assert!(!ignores.is_ignored("src/foo.spec.luau"));
        assert!(ignores.is_empty());
    }

    // -----------------------------------------------------------------------
    // .toml / .yaml file type classification
    // -----------------------------------------------------------------------

    #[test]
    fn classify_toml_file_type() {
        assert_eq!(classify_file_type("src/config.toml"), "toml");
        assert_eq!(classify_file_type("Packages/wally.toml"), "toml");
    }

    #[test]
    fn toml_files_included_in_snapshot() {
        let root = tempdir().expect("tempdir");
        let src = root.path().join("src");
        fs::create_dir_all(&src).expect("mkdir");
        fs::write(src.join("config.toml"), "[settings]\nkey = \"value\"").expect("write toml");
        fs::write(src.join("init.luau"), "return {}").expect("write luau");

        let includes = vec!["src".to_string()];
        let snapshot = build_snapshot(root.path(), &includes).expect("snapshot");

        let toml_entry = snapshot.entries.iter().find(|e| e.path.ends_with(".toml"));
        assert!(toml_entry.is_some(), "TOML file should be in snapshot");
        assert_eq!(toml_entry.unwrap().file_type.as_deref(), Some("toml"));
    }

    // -----------------------------------------------------------------------
    // JSONC support
    // -----------------------------------------------------------------------

    #[test]
    fn strip_json_comments_single_line() {
        assert_eq!(
            strip_json_comments("{\n  // comment\n  \"key\": 1\n}"),
            "{\n  \n  \"key\": 1\n}"
        );
    }

    #[test]
    fn strip_json_comments_block() {
        assert_eq!(
            strip_json_comments("{ /* block */ \"key\": 1 }"),
            "{  \"key\": 1 }"
        );
    }

    #[test]
    fn strip_json_comments_in_string() {
        // Comments inside strings should NOT be stripped
        assert_eq!(
            strip_json_comments("{\"url\": \"http://example.com\"}"),
            "{\"url\": \"http://example.com\"}"
        );
    }

    #[test]
    fn strip_json_comments_nested_block() {
        // Block comments do not nest — first */ ends the comment
        assert_eq!(
            strip_json_comments("{ /* a /* b */ \"k\": 1 }"),
            "{  \"k\": 1 }"
        );
    }

    #[test]
    fn strip_json_comments_escaped_quote_in_string() {
        // Escaped quotes inside strings should not end the string
        assert_eq!(
            strip_json_comments(r#"{"key": "val\"ue // not a comment"}"#),
            r#"{"key": "val\"ue // not a comment"}"#
        );
    }

    #[test]
    fn strip_json_comments_empty_input() {
        assert_eq!(strip_json_comments(""), "");
    }

    #[test]
    fn classify_jsonc_file_type() {
        assert_eq!(classify_file_type("src/config.jsonc"), "jsonc");
        assert_eq!(classify_file_type("settings.JSONC"), "jsonc");
    }

    #[test]
    fn jsonc_files_included_in_snapshot() {
        let root = tempdir().expect("tempdir");
        let src = root.path().join("src");
        fs::create_dir_all(&src).expect("mkdir");
        fs::write(
            src.join("config.jsonc"),
            "{\n  // this is a comment\n  \"key\": \"value\"\n}",
        )
        .expect("write jsonc");

        let includes = vec!["src".to_string()];
        let snapshot = build_snapshot(root.path(), &includes).expect("snapshot");

        let jsonc_entry = snapshot.entries.iter().find(|e| e.path.ends_with(".jsonc"));
        assert!(jsonc_entry.is_some(), "JSONC file should be in snapshot");
        assert_eq!(jsonc_entry.unwrap().file_type.as_deref(), Some("jsonc"));
    }

    // -----------------------------------------------------------------------
    // Rename detection
    // -----------------------------------------------------------------------

    fn make_entry(path: &str, sha: &str, bytes: u64) -> SnapshotEntry {
        SnapshotEntry {
            path: path.to_string(),
            sha256: sha.to_string(),
            bytes,
            meta: None,
            file_type: None,
        }
    }

    #[test]
    fn diff_detects_rename() {
        let previous = Snapshot {
            version: 1,
            include: vec!["src".to_string()],
            fingerprint: "fp1".to_string(),
            entries: vec![make_entry("src/old.luau", "abc123", 100)],
        };
        let current = Snapshot {
            version: 1,
            include: vec!["src".to_string()],
            fingerprint: "fp2".to_string(),
            entries: vec![make_entry("src/new.luau", "abc123", 100)],
        };
        let diff = diff_snapshots(&previous, &current);
        assert_eq!(diff.renamed.len(), 1);
        assert_eq!(diff.renamed[0].old_path, "src/old.luau");
        assert_eq!(diff.renamed[0].new_path, "src/new.luau");
        assert_eq!(diff.renamed[0].sha256, "abc123");
        assert!(diff.added.is_empty());
        assert!(diff.deleted.is_empty());
    }

    #[test]
    fn diff_rename_does_not_match_different_hashes() {
        let previous = Snapshot {
            version: 1,
            include: vec!["src".to_string()],
            fingerprint: "fp1".to_string(),
            entries: vec![make_entry("src/a.luau", "hash1", 100)],
        };
        let current = Snapshot {
            version: 1,
            include: vec!["src".to_string()],
            fingerprint: "fp2".to_string(),
            entries: vec![make_entry("src/b.luau", "hash2", 100)],
        };
        let diff = diff_snapshots(&previous, &current);
        assert!(diff.renamed.is_empty());
        assert_eq!(diff.added.len(), 1);
        assert_eq!(diff.deleted.len(), 1);
    }

    #[test]
    fn diff_rename_preserves_unrelated_changes() {
        let previous = Snapshot {
            version: 1,
            include: vec!["src".to_string()],
            fingerprint: "fp1".to_string(),
            entries: vec![
                make_entry("src/old.luau", "same_hash", 50),
                make_entry("src/keep.luau", "keep_hash", 30),
                make_entry("src/remove.luau", "remove_hash", 20),
            ],
        };
        let current = Snapshot {
            version: 1,
            include: vec!["src".to_string()],
            fingerprint: "fp2".to_string(),
            entries: vec![
                make_entry("src/new.luau", "same_hash", 50),
                make_entry("src/keep.luau", "keep_hash", 30),
                make_entry("src/fresh.luau", "fresh_hash", 10),
            ],
        };
        let diff = diff_snapshots(&previous, &current);
        assert_eq!(diff.renamed.len(), 1);
        assert_eq!(diff.renamed[0].old_path, "src/old.luau");
        assert_eq!(diff.renamed[0].new_path, "src/new.luau");
        assert_eq!(diff.added.len(), 1);
        assert_eq!(diff.added[0].path, "src/fresh.luau");
        assert_eq!(diff.deleted.len(), 1);
        assert_eq!(diff.deleted[0].path, "src/remove.luau");
        assert!(diff.modified.is_empty());
    }

    #[test]
    fn diff_rename_multiple_files() {
        let previous = Snapshot {
            version: 1,
            include: vec!["src".to_string()],
            fingerprint: "fp1".to_string(),
            entries: vec![
                make_entry("src/a.luau", "hash_a", 10),
                make_entry("src/b.luau", "hash_b", 20),
            ],
        };
        let current = Snapshot {
            version: 1,
            include: vec!["src".to_string()],
            fingerprint: "fp2".to_string(),
            entries: vec![
                make_entry("src/x.luau", "hash_a", 10),
                make_entry("src/y.luau", "hash_b", 20),
            ],
        };
        let diff = diff_snapshots(&previous, &current);
        assert_eq!(diff.renamed.len(), 2);
        assert!(diff.added.is_empty());
        assert!(diff.deleted.is_empty());
    }

    #[test]
    fn classify_history_path_treats_day_night_cycle_as_code_not_geometry() {
        assert_eq!(
            classify_history_path("roblox/src/ServerScriptService/ImportService/DayNightCycle.lua"),
            ("code", false)
        );
    }

    #[test]
    fn classify_history_path_treats_scene_audit_as_code_not_geometry() {
        assert_eq!(
            classify_history_path("roblox/src/ServerScriptService/ImportService/SceneAudit.lua"),
            ("code", false)
        );
    }

    #[test]
    fn classify_history_path_treats_lighting_root_as_code_not_geometry() {
        assert_eq!(
            classify_history_path("roblox/src/Lighting"),
            ("code", false)
        );
        assert_eq!(
            classify_history_path("roblox/src/Lighting/Atmosphere.json"),
            ("code", false)
        );
    }

    #[test]
    fn classify_history_path_keeps_builder_changes_geometry_affecting() {
        assert_eq!(
            classify_history_path(
                "roblox/src/ServerScriptService/ImportService/Builders/RoadBuilder.lua"
            ),
            ("geometry", true)
        );
    }

    #[test]
    fn classify_history_diff_does_not_mark_day_night_only_change_as_geometry() {
        let diff = SnapshotDiff {
            previous_fingerprint: "prev".to_string(),
            current_fingerprint: "curr".to_string(),
            added: vec![make_entry(
                "roblox/src/ServerScriptService/ImportService/DayNightCycle.lua",
                "sha",
                128,
            )],
            modified: Vec::new(),
            deleted: Vec::new(),
            renamed: Vec::new(),
        };

        let (scope, geometry_affecting) = classify_history_diff(&diff);
        assert_eq!(scope, "mixed");
        assert!(!geometry_affecting);
    }

    #[test]
    fn classify_history_diff_marks_builder_change_as_geometry() {
        let diff = SnapshotDiff {
            previous_fingerprint: "prev".to_string(),
            current_fingerprint: "curr".to_string(),
            added: vec![make_entry(
                "roblox/src/ServerScriptService/ImportService/Builders/BuildingBuilder.lua",
                "sha",
                128,
            )],
            modified: Vec::new(),
            deleted: Vec::new(),
            renamed: Vec::new(),
        };

        let (scope, geometry_affecting) = classify_history_diff(&diff);
        assert_eq!(scope, "geometry");
        assert!(geometry_affecting);
    }

    #[test]
    fn classify_history_diff_does_not_mark_lighting_only_change_as_geometry() {
        let diff = SnapshotDiff {
            previous_fingerprint: "prev".to_string(),
            current_fingerprint: "curr".to_string(),
            added: vec![make_entry("roblox/src/Lighting/Atmosphere.json", "sha", 64)],
            modified: Vec::new(),
            deleted: Vec::new(),
            renamed: Vec::new(),
        };

        let (scope, geometry_affecting) = classify_history_diff(&diff);
        assert_eq!(scope, "mixed");
        assert!(!geometry_affecting);
    }
}
