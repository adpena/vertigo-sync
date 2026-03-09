//! MCP tool surface for vertigo-sync.
//!
//! Exposes vertigo-sync capabilities as MCP-compatible tool definitions via
//! REST endpoints that any MCP server can proxy to:
//!
//!   GET  /mcp/tools   — JSON array of tool definitions
//!   POST /mcp/execute — Execute a tool by name with arguments

use std::collections::BTreeMap;
use std::path::{Component, Path};
use std::sync::Arc;

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::Json;
use regex::Regex;
use serde::Deserialize;

use crate::project::parse_project;
use crate::validate;
use crate::{ServerState, build_snapshot, diff_snapshots, run_doctor, run_health_doctor};

// ---------------------------------------------------------------------------
// Tool definition helpers
// ---------------------------------------------------------------------------

fn param(name: &str, typ: &str, description: &str, required: bool) -> serde_json::Value {
    serde_json::json!({
        "name": name,
        "type": typ,
        "description": description,
        "required": required,
    })
}

fn tool_def(
    name: &str,
    description: &str,
    params: Vec<serde_json::Value>,
) -> serde_json::Value {
    let mut properties = serde_json::Map::new();
    let mut required_fields: Vec<String> = Vec::new();

    for p in &params {
        let param_name = p["name"].as_str().unwrap_or_default().to_string();
        let mut schema = serde_json::Map::new();
        schema.insert(
            "type".to_string(),
            p["type"].clone(),
        );
        if let Some(desc) = p["description"].as_str() {
            schema.insert("description".to_string(), serde_json::Value::String(desc.to_string()));
        }
        if p["required"].as_bool().unwrap_or(false) {
            required_fields.push(param_name.clone());
        }
        properties.insert(param_name, serde_json::Value::Object(schema));
    }

    let mut input_schema = serde_json::json!({
        "type": "object",
        "properties": properties,
    });

    if !required_fields.is_empty() {
        input_schema["required"] = serde_json::json!(required_fields);
    }

    serde_json::json!({
        "name": name,
        "description": description,
        "inputSchema": input_schema,
    })
}

// ---------------------------------------------------------------------------
// GET /mcp/tools
// ---------------------------------------------------------------------------

pub async fn handle_mcp_tools() -> Json<Vec<serde_json::Value>> {
    Json(vec![
        tool_def(
            "vsync_health",
            "Check vertigo-sync server health and version",
            vec![],
        ),
        tool_def(
            "vsync_snapshot",
            "Get current source tree snapshot with file hashes",
            vec![],
        ),
        tool_def(
            "vsync_diff",
            "Get diff between current and a previous snapshot hash",
            vec![param(
                "since_hash",
                "string",
                "Previous snapshot fingerprint to diff from",
                true,
            )],
        ),
        tool_def(
            "vsync_sources",
            "List all source files with paths and SHA-256 hashes",
            vec![],
        ),
        tool_def(
            "vsync_source",
            "Read a specific source file content",
            vec![param(
                "path",
                "string",
                "Relative file path (e.g. src/Server/Services/DataService.luau)",
                true,
            )],
        ),
        tool_def(
            "vsync_validate",
            "Run Luau source validation and NCG performance lints",
            vec![],
        ),
        tool_def(
            "vsync_metrics",
            "Get Prometheus metrics (polls, cache ratio, latency)",
            vec![],
        ),
        tool_def(
            "vsync_patch",
            "Apply file patches to the source tree",
            vec![param(
                "patches",
                "array",
                "Array of {path, action, content_base64} objects",
                true,
            )],
        ),
        tool_def(
            "vsync_doctor",
            "Run determinism and health checks on source tree",
            vec![],
        ),
        tool_def(
            "vsync_project",
            "Get parsed project.json mappings (fs path -> DataModel path)",
            vec![],
        ),
        tool_def(
            "vsync_search",
            "Search source files by content pattern (ripgrep-style)",
            vec![
                param("pattern", "string", "Regex pattern to search for", true),
                param(
                    "glob",
                    "string",
                    "File glob filter (e.g. *.luau)",
                    false,
                ),
            ],
        ),
        tool_def(
            "vsync_stats",
            "Get source tree statistics (file count, total bytes, by extension)",
            vec![],
        ),
    ])
}

// ---------------------------------------------------------------------------
// POST /mcp/execute
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct McpExecuteRequest {
    pub tool: String,
    #[serde(default)]
    pub arguments: serde_json::Value,
}

pub async fn handle_mcp_execute(
    State(state): State<Arc<ServerState>>,
    Json(req): Json<McpExecuteRequest>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let result = match req.tool.as_str() {
        "vsync_health" => exec_health(),
        "vsync_snapshot" => exec_snapshot(&state),
        "vsync_diff" => exec_diff(&state, &req.arguments),
        "vsync_sources" => exec_sources(&state),
        "vsync_source" => exec_source(&state, &req.arguments),
        "vsync_validate" => exec_validate(&state),
        "vsync_metrics" => exec_metrics(&state),
        "vsync_patch" => exec_patch(&state, &req.arguments),
        "vsync_doctor" => exec_doctor(&state),
        "vsync_project" => exec_project(&state),
        "vsync_search" => exec_search(&state, &req.arguments),
        "vsync_stats" => exec_stats(&state),
        _ => Err((
            StatusCode::NOT_FOUND,
            format!("unknown tool: {}", req.tool),
        )),
    }?;

    Ok(Json(result))
}

// ---------------------------------------------------------------------------
// Tool implementations
// ---------------------------------------------------------------------------

fn exec_health() -> Result<serde_json::Value, (StatusCode, String)> {
    Ok(serde_json::json!({
        "status": "ok",
        "version": env!("CARGO_PKG_VERSION"),
        "server": "vertigo-sync",
    }))
}

fn exec_snapshot(state: &ServerState) -> Result<serde_json::Value, (StatusCode, String)> {
    let lock = state
        .current
        .lock()
        .map_err(|_| (StatusCode::INTERNAL_SERVER_ERROR, "lock poisoned".into()))?;
    serde_json::to_value(&**lock).map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))
}

fn exec_diff(
    state: &ServerState,
    args: &serde_json::Value,
) -> Result<serde_json::Value, (StatusCode, String)> {
    let since_hash = args["since_hash"]
        .as_str()
        .ok_or_else(|| (StatusCode::BAD_REQUEST, "missing required param: since_hash".into()))?;

    let old = {
        let lock = state
            .history
            .lock()
            .map_err(|_| (StatusCode::INTERNAL_SERVER_ERROR, "lock poisoned".into()))?;
        lock.get(since_hash).cloned()
    };

    let old = old.ok_or_else(|| {
        (
            StatusCode::NOT_FOUND,
            format!("snapshot {} not found in history", since_hash),
        )
    })?;

    let current = {
        let lock = state
            .current
            .lock()
            .map_err(|_| (StatusCode::INTERNAL_SERVER_ERROR, "lock poisoned".into()))?;
        Arc::clone(&lock)
    };

    let diff = diff_snapshots(&old, &current);
    serde_json::to_value(&diff).map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))
}

fn exec_sources(state: &ServerState) -> Result<serde_json::Value, (StatusCode, String)> {
    let lock = state
        .current
        .lock()
        .map_err(|_| (StatusCode::INTERNAL_SERVER_ERROR, "lock poisoned".into()))?;

    let entries: Vec<serde_json::Value> = lock
        .entries
        .iter()
        .map(|e| {
            serde_json::json!({
                "path": e.path,
                "sha256": e.sha256,
                "bytes": e.bytes,
            })
        })
        .collect();

    Ok(serde_json::json!(entries))
}

fn exec_source(
    state: &ServerState,
    args: &serde_json::Value,
) -> Result<serde_json::Value, (StatusCode, String)> {
    let raw_path = args["path"]
        .as_str()
        .ok_or_else(|| (StatusCode::BAD_REQUEST, "missing required param: path".into()))?;

    let candidate = Path::new(raw_path);
    if candidate.is_absolute() {
        return Err((StatusCode::BAD_REQUEST, "absolute paths not allowed".into()));
    }
    for component in candidate.components() {
        if matches!(
            component,
            Component::ParentDir | Component::RootDir | Component::Prefix(_)
        ) {
            return Err((StatusCode::BAD_REQUEST, "path traversal not allowed".into()));
        }
    }

    let source_root = state
        .root
        .canonicalize()
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    let target = source_root.join(candidate);
    let resolved = target
        .canonicalize()
        .map_err(|_| (StatusCode::NOT_FOUND, format!("file not found: {raw_path}")))?;

    if !resolved.starts_with(&source_root) || !resolved.is_file() {
        return Err((StatusCode::NOT_FOUND, format!("file not found: {raw_path}")));
    }

    let content = std::fs::read_to_string(&resolved)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(content.as_bytes());
    let hash = format!("{:x}", hasher.finalize());

    Ok(serde_json::json!({
        "path": raw_path,
        "content": content,
        "sha256": hash,
        "bytes": content.len(),
    }))
}

fn exec_validate(state: &ServerState) -> Result<serde_json::Value, (StatusCode, String)> {
    let report = validate::validate_source(&state.root, &state.includes)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    serde_json::to_value(&report).map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))
}

fn exec_metrics(state: &ServerState) -> Result<serde_json::Value, (StatusCode, String)> {
    use std::sync::atomic::Ordering;

    let m = &state.metrics;
    let polls = m.polls.load(Ordering::Relaxed);
    let duration_sum_us = m.poll_duration_sum_us.load(Ordering::Relaxed);
    let cache_hits = m.cache_hits.load(Ordering::Relaxed);
    let cache_misses = m.cache_misses.load(Ordering::Relaxed);
    let total_lookups = cache_hits + cache_misses;
    let hit_ratio = if total_lookups > 0 {
        cache_hits as f64 / total_lookups as f64
    } else {
        0.0
    };

    Ok(serde_json::json!({
        "polls": polls,
        "poll_duration_seconds_sum": duration_sum_us as f64 / 1_000_000.0,
        "cache_hits": cache_hits,
        "cache_misses": cache_misses,
        "cache_hit_ratio": hit_ratio,
        "entries": m.entries.load(Ordering::Relaxed),
        "ws_connections": m.ws_connections.load(Ordering::Relaxed),
        "events_emitted": m.events_emitted.load(Ordering::Relaxed),
    }))
}

fn exec_patch(
    state: &ServerState,
    args: &serde_json::Value,
) -> Result<serde_json::Value, (StatusCode, String)> {
    let patches = args["patches"]
        .as_array()
        .ok_or_else(|| (StatusCode::BAD_REQUEST, "missing required param: patches (array)".into()))?;

    let source_root = state
        .root
        .canonicalize()
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    let mut applied = 0usize;
    let mut errors: Vec<String> = Vec::new();

    for patch_val in patches {
        let path_str = match patch_val["path"].as_str() {
            Some(p) => p,
            None => {
                errors.push("patch entry missing 'path' field".into());
                continue;
            }
        };
        let action = match patch_val["action"].as_str() {
            Some(a) => a,
            None => {
                errors.push(format!("{path_str}: missing 'action' field"));
                continue;
            }
        };

        // Validate path safety.
        let candidate = Path::new(path_str);
        if candidate.is_absolute() {
            errors.push(format!("{path_str}: absolute paths not allowed"));
            continue;
        }
        let mut traversal = false;
        for component in candidate.components() {
            if matches!(
                component,
                Component::ParentDir | Component::RootDir | Component::Prefix(_)
            ) {
                traversal = true;
                break;
            }
        }
        if traversal {
            errors.push(format!("{path_str}: path traversal not allowed"));
            continue;
        }

        let target = source_root.join(candidate);

        match action {
            "write" => {
                let content_b64 = match patch_val["content_base64"].as_str() {
                    Some(c) => c,
                    None => {
                        errors.push(format!("{path_str}: write action missing content_base64"));
                        continue;
                    }
                };
                use base64::Engine;
                match base64::engine::general_purpose::STANDARD.decode(content_b64) {
                    Ok(bytes) => {
                        if let Some(parent) = target.parent() {
                            let _ = std::fs::create_dir_all(parent);
                        }
                        match std::fs::write(&target, &bytes) {
                            Ok(()) => applied += 1,
                            Err(e) => errors.push(format!("{path_str}: write error: {e}")),
                        }
                    }
                    Err(e) => errors.push(format!("{path_str}: base64 decode error: {e}")),
                }
            }
            "delete" => match std::fs::remove_file(&target) {
                Ok(()) => applied += 1,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => applied += 1,
                Err(e) => errors.push(format!("{path_str}: delete error: {e}")),
            },
            other => errors.push(format!("{path_str}: unknown action '{other}'")),
        }
    }

    // Rebuild snapshot after patches.
    let new_snapshot = build_snapshot(&state.root, &state.includes)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    let new_hash = new_snapshot.fingerprint.clone();
    let new_arc = Arc::new(new_snapshot);

    {
        let mut lock = state
            .current
            .lock()
            .map_err(|_| (StatusCode::INTERNAL_SERVER_ERROR, "lock poisoned".into()))?;
        *lock = Arc::clone(&new_arc);
    }
    {
        let mut lock = state
            .history
            .lock()
            .map_err(|_| (StatusCode::INTERNAL_SERVER_ERROR, "lock poisoned".into()))?;
        lock.insert(new_hash.clone(), new_arc);
    }

    Ok(serde_json::json!({
        "accepted": errors.is_empty(),
        "new_source_hash": new_hash,
        "applied": applied,
        "errors": errors,
    }))
}

fn exec_doctor(state: &ServerState) -> Result<serde_json::Value, (StatusCode, String)> {
    let determinism = run_doctor(&state.root, &state.includes)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    let health = run_health_doctor(&state.root, &state.includes)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    Ok(serde_json::json!({
        "determinism": determinism,
        "health": health,
    }))
}

fn exec_project(state: &ServerState) -> Result<serde_json::Value, (StatusCode, String)> {
    let project_path = state.root.join("default.project.json");
    if !project_path.exists() {
        return Ok(serde_json::json!({
            "error": "default.project.json not found",
            "path": project_path.display().to_string(),
        }));
    }

    let tree = parse_project(&project_path)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    serde_json::to_value(&tree).map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))
}

fn exec_search(
    state: &ServerState,
    args: &serde_json::Value,
) -> Result<serde_json::Value, (StatusCode, String)> {
    let pattern_str = args["pattern"]
        .as_str()
        .ok_or_else(|| (StatusCode::BAD_REQUEST, "missing required param: pattern".into()))?;

    let glob_filter = args["glob"].as_str();

    let results = search_sources(&state.root, &state.includes, pattern_str, glob_filter)
        .map_err(|e| (StatusCode::BAD_REQUEST, e.to_string()))?;

    Ok(serde_json::json!({
        "pattern": pattern_str,
        "glob": glob_filter,
        "matches": results.len(),
        "results": results,
    }))
}

fn exec_stats(state: &ServerState) -> Result<serde_json::Value, (StatusCode, String)> {
    let lock = state
        .current
        .lock()
        .map_err(|_| (StatusCode::INTERNAL_SERVER_ERROR, "lock poisoned".into()))?;

    Ok(compute_stats(&lock))
}

// ---------------------------------------------------------------------------
// Search implementation
// ---------------------------------------------------------------------------

#[derive(Debug, serde::Serialize)]
struct SearchResult {
    path: String,
    line_number: usize,
    line: String,
}

fn search_sources(
    root: &Path,
    includes: &[String],
    pattern: &str,
    glob_filter: Option<&str>,
) -> Result<Vec<SearchResult>, String> {
    let re = Regex::new(pattern).map_err(|e| format!("invalid regex: {e}"))?;

    let resolved_includes = crate::resolve_includes(includes);
    let mut results = Vec::new();

    // Cap results to prevent unbounded output.
    const MAX_RESULTS: usize = 500;

    for include in &resolved_includes {
        let include_path = root.join(include);
        if !include_path.exists() {
            continue;
        }
        search_dir(
            root,
            &include_path,
            &re,
            glob_filter,
            &mut results,
            MAX_RESULTS,
        );
        if results.len() >= MAX_RESULTS {
            break;
        }
    }

    Ok(results)
}

fn search_dir(
    root: &Path,
    dir: &Path,
    re: &Regex,
    glob_filter: Option<&str>,
    results: &mut Vec<SearchResult>,
    max: usize,
) {
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return,
    };

    for entry in entries.flatten() {
        if results.len() >= max {
            return;
        }

        let path = entry.path();
        if path.is_dir() {
            let dir_name = path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or_default();
            // Skip known noise directories.
            if matches!(
                dir_name,
                ".git" | "node_modules" | "target" | "__pycache__" | ".cache"
            ) {
                continue;
            }
            search_dir(root, &path, re, glob_filter, results, max);
            continue;
        }

        if !path.is_file() {
            continue;
        }

        let file_name = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or_default();

        // Apply glob filter (simple suffix/extension matching).
        if let Some(glob) = glob_filter {
            if !matches_glob(file_name, glob) {
                continue;
            }
        }

        let rel_path = match path.strip_prefix(root) {
            Ok(r) => r.to_string_lossy().replace('\\', "/"),
            Err(_) => continue,
        };

        // Read file and search lines.
        let content = match std::fs::read_to_string(&path) {
            Ok(c) => c,
            Err(_) => continue, // Skip binary/unreadable files.
        };

        for (line_idx, line) in content.lines().enumerate() {
            if results.len() >= max {
                return;
            }
            if re.is_match(line) {
                results.push(SearchResult {
                    path: rel_path.clone(),
                    line_number: line_idx + 1,
                    line: if line.len() > 500 {
                        format!("{}...", &line[..500])
                    } else {
                        line.to_string()
                    },
                });
            }
        }
    }
}

/// Simple glob matching: supports `*.ext`, `*.{ext1,ext2}`, and bare `ext`.
fn matches_glob(file_name: &str, glob: &str) -> bool {
    if glob.starts_with("*.") {
        let suffix = &glob[1..]; // e.g. ".luau"
        file_name.ends_with(suffix)
    } else if glob.contains('*') {
        // Fallback: just check if the extension part matches.
        let ext_part = glob.trim_start_matches('*').trim_start_matches('.');
        file_name.ends_with(&format!(".{ext_part}"))
    } else if glob.starts_with('.') {
        file_name.ends_with(glob)
    } else {
        file_name.ends_with(&format!(".{glob}"))
    }
}

// ---------------------------------------------------------------------------
// Stats computation
// ---------------------------------------------------------------------------

fn compute_stats(snapshot: &crate::Snapshot) -> serde_json::Value {
    let total_files = snapshot.entries.len();
    let total_bytes: u64 = snapshot.entries.iter().map(|e| e.bytes).sum();

    // Files by extension.
    let mut by_extension: BTreeMap<String, (usize, u64)> = BTreeMap::new();
    // Files by top-level directory.
    let mut by_directory: BTreeMap<String, (usize, u64)> = BTreeMap::new();
    // Track largest files.
    let mut largest: Vec<(&str, u64)> = Vec::new();

    for entry in &snapshot.entries {
        // Extension.
        let ext = entry
            .path
            .rsplit('.')
            .next()
            .map(|e| format!(".{e}"))
            .unwrap_or_else(|| "(none)".to_string());
        let ext_entry = by_extension.entry(ext).or_insert((0, 0));
        ext_entry.0 += 1;
        ext_entry.1 += entry.bytes;

        // Top-level directory (first path segment).
        let dir = entry
            .path
            .split('/')
            .take(2)
            .collect::<Vec<_>>()
            .join("/");
        let dir_entry = by_directory.entry(dir).or_insert((0, 0));
        dir_entry.0 += 1;
        dir_entry.1 += entry.bytes;

        largest.push((&entry.path, entry.bytes));
    }

    // Sort largest files descending, take top 10.
    largest.sort_by(|a, b| b.1.cmp(&a.1));
    largest.truncate(10);

    let avg_bytes = if total_files > 0 {
        total_bytes / total_files as u64
    } else {
        0
    };

    serde_json::json!({
        "total_files": total_files,
        "total_bytes": total_bytes,
        "average_bytes": avg_bytes,
        "fingerprint": snapshot.fingerprint,
        "by_extension": by_extension.iter().map(|(ext, (count, bytes))| {
            serde_json::json!({ "extension": ext, "files": count, "bytes": bytes })
        }).collect::<Vec<_>>(),
        "by_directory": by_directory.iter().map(|(dir, (count, bytes))| {
            serde_json::json!({ "directory": dir, "files": count, "bytes": bytes })
        }).collect::<Vec<_>>(),
        "largest_files": largest.iter().map(|(path, bytes)| {
            serde_json::json!({ "path": path, "bytes": bytes })
        }).collect::<Vec<_>>(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_definitions_valid_json() {
        let tools = vec![
            tool_def("test_tool", "A test tool", vec![]),
            tool_def(
                "test_with_params",
                "Tool with params",
                vec![
                    param("name", "string", "A name", true),
                    param("count", "number", "A count", false),
                ],
            ),
        ];

        assert_eq!(tools.len(), 2);
        assert_eq!(tools[0]["name"], "test_tool");
        assert_eq!(tools[1]["inputSchema"]["required"][0], "name");
        assert!(tools[1]["inputSchema"]["required"]
            .as_array()
            .unwrap()
            .iter()
            .all(|v| v != "count"));
    }

    #[test]
    fn glob_matching() {
        assert!(matches_glob("foo.luau", "*.luau"));
        assert!(!matches_glob("foo.lua", "*.luau"));
        assert!(matches_glob("bar.json", "*.json"));
        assert!(matches_glob("baz.ts", ".ts"));
        assert!(matches_glob("test.rs", "rs"));
    }

    #[test]
    fn compute_stats_empty_snapshot() {
        let snapshot = crate::Snapshot {
            version: 1,
            include: vec![],
            fingerprint: "abc123".into(),
            entries: vec![],
        };
        let stats = compute_stats(&snapshot);
        assert_eq!(stats["total_files"], 0);
        assert_eq!(stats["total_bytes"], 0);
    }

    #[test]
    fn compute_stats_with_entries() {
        let snapshot = crate::Snapshot {
            version: 1,
            include: vec!["src".into()],
            fingerprint: "abc123".into(),
            entries: vec![
                crate::SnapshotEntry {
                    path: "src/Server/init.server.luau".into(),
                    sha256: "aaa".into(),
                    bytes: 1000,
                },
                crate::SnapshotEntry {
                    path: "src/Client/init.client.luau".into(),
                    sha256: "bbb".into(),
                    bytes: 2000,
                },
                crate::SnapshotEntry {
                    path: "src/Shared/Config/Abilities.luau".into(),
                    sha256: "ccc".into(),
                    bytes: 500,
                },
            ],
        };
        let stats = compute_stats(&snapshot);
        assert_eq!(stats["total_files"], 3);
        assert_eq!(stats["total_bytes"], 3500);
        assert_eq!(stats["average_bytes"], 1166); // 3500 / 3
    }
}
