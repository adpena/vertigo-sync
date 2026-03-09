//! MCP tool surface for vertigo-sync.
//!
//! Exposes vertigo-sync capabilities as MCP-compatible tool definitions via
//! REST endpoints that any MCP server can proxy to:
//!
//!   GET  /mcp/tools   — JSON array of tool definitions
//!   POST /mcp/execute — Execute a tool by name with arguments
//!
//! ## Agent DSL Philosophy
//!
//! Every tool is composable. Agents build workflows from atomic operations:
//!
//!   vsync_source → vsync_validate_content → vsync_safe_write → vsync_diff
//!
//! Read ops are side-effect-free. Write ops rebuild the snapshot atomically.
//! Validation ops work on content in memory without touching disk.

use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::time::{Instant, SystemTime};

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::Json;
use regex::Regex;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::project::parse_project;
use crate::rbxl::RbxlLoader;
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

fn tool_def(name: &str, description: &str, params: Vec<serde_json::Value>) -> serde_json::Value {
    let mut properties = serde_json::Map::new();
    let mut required_fields: Vec<String> = Vec::new();

    for p in &params {
        let param_name = p["name"].as_str().unwrap_or_default().to_string();
        let mut schema = serde_json::Map::new();
        schema.insert("type".to_string(), p["type"].clone());
        if let Some(desc) = p["description"].as_str() {
            schema.insert(
                "description".to_string(),
                serde_json::Value::String(desc.to_string()),
            );
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
        // ── Existing tools ──────────────────────────────────────────
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
                param("glob", "string", "File glob filter (e.g. *.luau)", false),
            ],
        ),
        tool_def(
            "vsync_stats",
            "Get source tree statistics (file count, total bytes, by extension)",
            vec![],
        ),
        // ── New: Read operations (composable) ───────────────────────
        tool_def(
            "vsync_ls",
            "List files in a directory within the source tree",
            vec![
                param(
                    "path",
                    "string",
                    "Relative directory path (e.g. src/Server/Services)",
                    true,
                ),
                param(
                    "recursive",
                    "boolean",
                    "Include subdirectories (default false)",
                    false,
                ),
            ],
        ),
        tool_def(
            "vsync_read_batch",
            "Read multiple source files in one call",
            vec![param(
                "paths",
                "array",
                "Array of relative file paths to read",
                true,
            )],
        ),
        tool_def(
            "vsync_file_info",
            "Get metadata for a file (hash, size, last modified) without content",
            vec![param("path", "string", "Relative file path", true)],
        ),
        tool_def(
            "vsync_grep",
            "Search source files with context lines (like rg -C)",
            vec![
                param("pattern", "string", "Regex pattern", true),
                param("glob", "string", "File glob filter", false),
                param(
                    "context",
                    "number",
                    "Lines of context before/after match (default 2)",
                    false,
                ),
                param(
                    "max_results",
                    "number",
                    "Maximum results (default 100)",
                    false,
                ),
            ],
        ),
        // ── New: Write operations (composable) ──────────────────────
        tool_def(
            "vsync_write",
            "Write content to a single source file (UTF-8 text, no base64)",
            vec![
                param("path", "string", "Relative file path", true),
                param("content", "string", "File content (UTF-8 text)", true),
            ],
        ),
        tool_def(
            "vsync_delete",
            "Delete a source file",
            vec![param(
                "path",
                "string",
                "Relative file path to delete",
                true,
            )],
        ),
        tool_def(
            "vsync_move",
            "Move/rename a source file",
            vec![
                param("from", "string", "Current relative path", true),
                param("to", "string", "New relative path", true),
            ],
        ),
        tool_def(
            "vsync_mkdir",
            "Create a directory in the source tree",
            vec![param(
                "path",
                "string",
                "Relative directory path to create",
                true,
            )],
        ),
        // ── New: Validation operations (composable) ─────────────────
        tool_def(
            "vsync_validate_content",
            "Validate Luau content without writing to disk",
            vec![
                param(
                    "path",
                    "string",
                    "Virtual file path (for lint context)",
                    true,
                ),
                param("content", "string", "Luau source content to validate", true),
            ],
        ),
        tool_def(
            "vsync_check_conflict",
            "Check if a path would conflict (case-insensitive collision, etc)",
            vec![param("path", "string", "Proposed file path", true)],
        ),
        // ── New: Pipeline operations (composable workflows) ─────────
        tool_def(
            "vsync_safe_write",
            "Write file only if validation passes (atomic validate+write)",
            vec![
                param("path", "string", "Relative file path", true),
                param("content", "string", "File content", true),
                param(
                    "require_strict",
                    "boolean",
                    "Require --!strict on line 1 (default true)",
                    false,
                ),
            ],
        ),
        tool_def(
            "vsync_describe_changes",
            "Describe current uncommitted changes in natural language",
            vec![param(
                "since_hash",
                "string",
                "Previous snapshot hash (optional, uses earliest known)",
                false,
            )],
        ),
        tool_def(
            "vsync_tree",
            "Get source tree structure as indented text",
            vec![
                param(
                    "path",
                    "string",
                    "Root path to start from (default: project root)",
                    false,
                ),
                param("depth", "number", "Maximum depth (default 3)", false),
            ],
        ),
        // ── New: Observability operations ───────────────────────────
        tool_def(
            "vsync_status",
            "Get comprehensive sync status (connections, last event, latency, snapshot age)",
            vec![],
        ),
        tool_def(
            "vsync_events",
            "Get recent sync events with sequence numbers",
            vec![param(
                "limit",
                "number",
                "Number of recent events (default 10)",
                false,
            )],
        ),
        // ── Pipeline orchestration ─────────────────────────────────
        tool_def(
            "vsync_pipeline",
            "Execute a composable pipeline of operations with dependency-aware scheduling",
            vec![
                param(
                    "steps",
                    "array",
                    "Array of pipeline steps: {tool, args, id, depends_on?, collect?}",
                    true,
                ),
                param(
                    "mode",
                    "string",
                    "Execution mode: 'sequential', 'parallel', 'auto' (default: 'auto')",
                    false,
                ),
                param(
                    "stop_on_error",
                    "boolean",
                    "Stop pipeline on first error (default: true)",
                    false,
                ),
            ],
        ),
        // ── RBXL file loading and querying ────────────────────────────
        tool_def(
            "vsync_rbxl_load",
            "Load and parse a .rbxl/.rbxlx file into the in-memory DOM cache",
            vec![param(
                "path",
                "string",
                "Absolute or relative path to the .rbxl/.rbxlx file",
                true,
            )],
        ),
        tool_def(
            "vsync_rbxl_tree",
            "Return the full instance tree of the loaded .rbxl file as a SceneGraph",
            vec![],
        ),
        tool_def(
            "vsync_rbxl_query",
            "Query instances in the loaded .rbxl file by class, tag, or name",
            vec![
                param("class", "string", "Filter by ClassName (exact match)", false),
                param("tag", "string", "Filter by CollectionService tag", false),
                param("name", "string", "Filter by Name (substring match)", false),
            ],
        ),
        tool_def(
            "vsync_rbxl_scripts",
            "Extract all Script/LocalScript/ModuleScript instances with their Source from the loaded .rbxl",
            vec![],
        ),
        tool_def(
            "vsync_rbxl_meshes",
            "Extract all MeshPart instances with their MeshId from the loaded .rbxl",
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
    // Pipeline is async — handle it before the synchronous match.
    if req.tool == "vsync_pipeline" {
        let result = exec_pipeline(&state, &req.arguments).await?;
        return Ok(Json(result));
    }

    let result = match req.tool.as_str() {
        // Existing tools
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
        // New: Read operations
        "vsync_ls" => exec_ls(&state, &req.arguments),
        "vsync_read_batch" => exec_read_batch(&state, &req.arguments),
        "vsync_file_info" => exec_file_info(&state, &req.arguments),
        "vsync_grep" => exec_grep(&state, &req.arguments),
        // New: Write operations
        "vsync_write" => exec_write(&state, &req.arguments),
        "vsync_delete" => exec_delete(&state, &req.arguments),
        "vsync_move" => exec_move(&state, &req.arguments),
        "vsync_mkdir" => exec_mkdir(&state, &req.arguments),
        // New: Validation operations
        "vsync_validate_content" => exec_validate_content(&req.arguments),
        "vsync_check_conflict" => exec_check_conflict(&state, &req.arguments),
        // New: Pipeline operations
        "vsync_safe_write" => exec_safe_write(&state, &req.arguments),
        "vsync_describe_changes" => exec_describe_changes(&state, &req.arguments),
        "vsync_tree" => exec_tree(&state, &req.arguments),
        // New: Observability operations
        "vsync_status" => exec_status(&state),
        "vsync_events" => exec_events(&state, &req.arguments),
        // RBXL file operations
        "vsync_rbxl_load" => exec_rbxl_load(&state, &req.arguments),
        "vsync_rbxl_tree" => exec_rbxl_tree(&state),
        "vsync_rbxl_query" => exec_rbxl_query(&state, &req.arguments),
        "vsync_rbxl_scripts" => exec_rbxl_scripts(&state),
        "vsync_rbxl_meshes" => exec_rbxl_meshes(&state),
        _ => Err((StatusCode::NOT_FOUND, format!("unknown tool: {}", req.tool))),
    }?;

    Ok(Json(result))
}

// ---------------------------------------------------------------------------
// Path validation helper — reused by all tools that accept relative paths.
// ---------------------------------------------------------------------------

/// Validate a relative path is safe (no traversal, no absolute). Returns
/// the resolved absolute path under `source_root`.
fn validate_path(source_root: &Path, raw_path: &str) -> Result<PathBuf, (StatusCode, String)> {
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
    Ok(source_root.join(candidate))
}

/// Canonicalize the project root for path safety checks.
fn canon_root(state: &ServerState) -> Result<PathBuf, (StatusCode, String)> {
    state
        .root
        .canonicalize()
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))
}

/// Rebuild the snapshot and update current + history in state. Returns the
/// new fingerprint.
fn rebuild_snapshot(state: &ServerState) -> Result<String, (StatusCode, String)> {
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

    Ok(new_hash)
}

/// Compute SHA-256 of bytes, return hex string.
fn sha256_hex(data: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(data);
    format!("{:x}", hasher.finalize())
}

// ---------------------------------------------------------------------------
// Existing tool implementations
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
    let since_hash = args["since_hash"].as_str().ok_or_else(|| {
        (
            StatusCode::BAD_REQUEST,
            "missing required param: since_hash".into(),
        )
    })?;

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
    let raw_path = args["path"].as_str().ok_or_else(|| {
        (
            StatusCode::BAD_REQUEST,
            "missing required param: path".into(),
        )
    })?;

    let source_root = canon_root(state)?;
    let target = validate_path(&source_root, raw_path)?;
    let resolved = target
        .canonicalize()
        .map_err(|_| (StatusCode::NOT_FOUND, format!("file not found: {raw_path}")))?;

    if !resolved.starts_with(&source_root) || !resolved.is_file() {
        return Err((StatusCode::NOT_FOUND, format!("file not found: {raw_path}")));
    }

    let content = std::fs::read_to_string(&resolved)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    let hash = sha256_hex(content.as_bytes());

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
    let patches = args["patches"].as_array().ok_or_else(|| {
        (
            StatusCode::BAD_REQUEST,
            "missing required param: patches (array)".into(),
        )
    })?;

    let source_root = canon_root(state)?;

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

        let target = match validate_path(&source_root, path_str) {
            Ok(t) => t,
            Err((_, msg)) => {
                errors.push(format!("{path_str}: {msg}"));
                continue;
            }
        };

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

    let new_hash = rebuild_snapshot(state)?;

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
    let pattern_str = args["pattern"].as_str().ok_or_else(|| {
        (
            StatusCode::BAD_REQUEST,
            "missing required param: pattern".into(),
        )
    })?;

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
// New tool implementations: Read operations
// ---------------------------------------------------------------------------

fn exec_ls(
    state: &ServerState,
    args: &serde_json::Value,
) -> Result<serde_json::Value, (StatusCode, String)> {
    let raw_path = args["path"].as_str().ok_or_else(|| {
        (
            StatusCode::BAD_REQUEST,
            "missing required param: path".into(),
        )
    })?;
    let recursive = args["recursive"].as_bool().unwrap_or(false);

    let source_root = canon_root(state)?;
    let target = validate_path(&source_root, raw_path)?;

    if !target.is_dir() {
        return Err((
            StatusCode::NOT_FOUND,
            format!("directory not found: {raw_path}"),
        ));
    }

    // Verify the resolved dir is inside the project root.
    let resolved = target.canonicalize().map_err(|_| {
        (
            StatusCode::NOT_FOUND,
            format!("directory not found: {raw_path}"),
        )
    })?;
    if !resolved.starts_with(&source_root) {
        return Err((StatusCode::BAD_REQUEST, "path traversal not allowed".into()));
    }

    let mut entries = Vec::new();
    ls_dir(&resolved, &source_root, recursive, &mut entries, 0, 5000);

    Ok(serde_json::json!({
        "path": raw_path,
        "recursive": recursive,
        "count": entries.len(),
        "entries": entries,
    }))
}

/// Collect directory entries for `vsync_ls`.
fn ls_dir(
    dir: &Path,
    root: &Path,
    recursive: bool,
    output: &mut Vec<serde_json::Value>,
    current_depth: usize,
    max_entries: usize,
) {
    let read = match std::fs::read_dir(dir) {
        Ok(r) => r,
        Err(_) => return,
    };

    let mut items: Vec<_> = read.flatten().collect();
    items.sort_by_key(|e| e.file_name());

    for entry in items {
        if output.len() >= max_entries {
            return;
        }

        let path = entry.path();
        let meta = match std::fs::metadata(&path) {
            Ok(m) => m,
            Err(_) => continue,
        };

        let rel = match path.strip_prefix(root) {
            Ok(r) => r.to_string_lossy().replace('\\', "/"),
            Err(_) => continue,
        };

        let name = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or_default()
            .to_string();

        let is_dir = meta.is_dir();
        let size = if is_dir { 0 } else { meta.len() };

        output.push(serde_json::json!({
            "name": name,
            "path": rel,
            "is_dir": is_dir,
            "size": size,
        }));

        if is_dir && recursive {
            // Skip noise directories.
            if matches!(
                name.as_str(),
                ".git" | "node_modules" | "target" | "__pycache__" | ".cache"
            ) {
                continue;
            }
            ls_dir(&path, root, true, output, current_depth + 1, max_entries);
        }
    }
}

fn exec_read_batch(
    state: &ServerState,
    args: &serde_json::Value,
) -> Result<serde_json::Value, (StatusCode, String)> {
    let paths = args["paths"].as_array().ok_or_else(|| {
        (
            StatusCode::BAD_REQUEST,
            "missing required param: paths (array)".into(),
        )
    })?;

    let source_root = canon_root(state)?;
    let mut results: Vec<serde_json::Value> = Vec::new();

    for path_val in paths {
        let raw_path = match path_val.as_str() {
            Some(p) => p,
            None => {
                results.push(serde_json::json!({
                    "path": path_val,
                    "error": "expected string path",
                }));
                continue;
            }
        };

        let target = match validate_path(&source_root, raw_path) {
            Ok(t) => t,
            Err((_, msg)) => {
                results.push(serde_json::json!({
                    "path": raw_path,
                    "error": msg,
                }));
                continue;
            }
        };

        match std::fs::read_to_string(&target) {
            Ok(content) => {
                let hash = sha256_hex(content.as_bytes());
                let bytes = content.len();
                results.push(serde_json::json!({
                    "path": raw_path,
                    "content": content,
                    "sha256": hash,
                    "bytes": bytes,
                }));
            }
            Err(e) => {
                results.push(serde_json::json!({
                    "path": raw_path,
                    "error": format!("read error: {e}"),
                }));
            }
        }
    }

    Ok(serde_json::json!({
        "count": results.len(),
        "files": results,
    }))
}

fn exec_file_info(
    state: &ServerState,
    args: &serde_json::Value,
) -> Result<serde_json::Value, (StatusCode, String)> {
    let raw_path = args["path"].as_str().ok_or_else(|| {
        (
            StatusCode::BAD_REQUEST,
            "missing required param: path".into(),
        )
    })?;

    let source_root = canon_root(state)?;
    let target = validate_path(&source_root, raw_path)?;

    let meta = std::fs::metadata(&target)
        .map_err(|_| (StatusCode::NOT_FOUND, format!("file not found: {raw_path}")))?;

    if !meta.is_file() {
        return Err((StatusCode::BAD_REQUEST, format!("not a file: {raw_path}")));
    }

    // Read content just for hash (don't return it — that's the point of file_info).
    let content =
        std::fs::read(&target).map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    let hash = sha256_hex(&content);

    let modified = meta
        .modified()
        .unwrap_or(SystemTime::UNIX_EPOCH)
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);

    let ext = Path::new(raw_path)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_string();

    Ok(serde_json::json!({
        "path": raw_path,
        "sha256": hash,
        "bytes": meta.len(),
        "modified_epoch": modified,
        "extension": ext,
    }))
}

fn exec_grep(
    state: &ServerState,
    args: &serde_json::Value,
) -> Result<serde_json::Value, (StatusCode, String)> {
    let pattern_str = args["pattern"].as_str().ok_or_else(|| {
        (
            StatusCode::BAD_REQUEST,
            "missing required param: pattern".into(),
        )
    })?;
    let glob_filter = args["glob"].as_str();
    let context = args["context"].as_u64().unwrap_or(2) as usize;
    let max_results = args["max_results"].as_u64().unwrap_or(100) as usize;

    let re = Regex::new(pattern_str)
        .map_err(|e| (StatusCode::BAD_REQUEST, format!("invalid regex: {e}")))?;

    let resolved_includes = crate::resolve_includes(&state.includes);
    let mut results: Vec<serde_json::Value> = Vec::new();

    for include in &resolved_includes {
        let include_path = state.root.join(include);
        if !include_path.exists() {
            continue;
        }
        grep_dir(
            &state.root,
            &include_path,
            &re,
            glob_filter,
            context,
            &mut results,
            max_results,
        );
        if results.len() >= max_results {
            break;
        }
    }

    Ok(serde_json::json!({
        "pattern": pattern_str,
        "glob": glob_filter,
        "context_lines": context,
        "matches": results.len(),
        "results": results,
    }))
}

/// Recursive grep with context lines.
fn grep_dir(
    root: &Path,
    dir: &Path,
    re: &Regex,
    glob_filter: Option<&str>,
    context: usize,
    results: &mut Vec<serde_json::Value>,
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
            if matches!(
                dir_name,
                ".git" | "node_modules" | "target" | "__pycache__" | ".cache"
            ) {
                continue;
            }
            grep_dir(root, &path, re, glob_filter, context, results, max);
            continue;
        }

        if !path.is_file() {
            continue;
        }

        let file_name = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or_default();

        if let Some(glob) = glob_filter {
            if !matches_glob(file_name, glob) {
                continue;
            }
        }

        let rel_path = match path.strip_prefix(root) {
            Ok(r) => r.to_string_lossy().replace('\\', "/"),
            Err(_) => continue,
        };

        let content = match std::fs::read_to_string(&path) {
            Ok(c) => c,
            Err(_) => continue,
        };

        let lines: Vec<&str> = content.lines().collect();
        for (line_idx, line) in lines.iter().enumerate() {
            if results.len() >= max {
                return;
            }
            if re.is_match(line) {
                let start = line_idx.saturating_sub(context);
                let end = (line_idx + context + 1).min(lines.len());
                let context_lines: Vec<serde_json::Value> = (start..end)
                    .map(|i| {
                        let l = lines[i];
                        let truncated = if l.len() > 500 {
                            format!("{}...", &l[..500])
                        } else {
                            l.to_string()
                        };
                        serde_json::json!({
                            "line_number": i + 1,
                            "text": truncated,
                            "is_match": i == line_idx,
                        })
                    })
                    .collect();

                results.push(serde_json::json!({
                    "path": rel_path,
                    "match_line": line_idx + 1,
                    "context": context_lines,
                }));
            }
        }
    }
}

// ---------------------------------------------------------------------------
// New tool implementations: Write operations
// ---------------------------------------------------------------------------

fn exec_write(
    state: &ServerState,
    args: &serde_json::Value,
) -> Result<serde_json::Value, (StatusCode, String)> {
    let raw_path = args["path"].as_str().ok_or_else(|| {
        (
            StatusCode::BAD_REQUEST,
            "missing required param: path".into(),
        )
    })?;
    let content = args["content"].as_str().ok_or_else(|| {
        (
            StatusCode::BAD_REQUEST,
            "missing required param: content".into(),
        )
    })?;

    let source_root = canon_root(state)?;
    let target = validate_path(&source_root, raw_path)?;

    // Ensure parent directory exists.
    if let Some(parent) = target.parent() {
        std::fs::create_dir_all(parent).map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("mkdir error: {e}"),
            )
        })?;
    }

    std::fs::write(&target, content.as_bytes()).map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("write error: {e}"),
        )
    })?;

    let hash = sha256_hex(content.as_bytes());
    let new_hash = rebuild_snapshot(state)?;

    Ok(serde_json::json!({
        "ok": true,
        "path": raw_path,
        "sha256": hash,
        "bytes": content.len(),
        "source_hash": new_hash,
    }))
}

fn exec_delete(
    state: &ServerState,
    args: &serde_json::Value,
) -> Result<serde_json::Value, (StatusCode, String)> {
    let raw_path = args["path"].as_str().ok_or_else(|| {
        (
            StatusCode::BAD_REQUEST,
            "missing required param: path".into(),
        )
    })?;

    let source_root = canon_root(state)?;
    let target = validate_path(&source_root, raw_path)?;

    let existed = target.exists();
    if existed {
        std::fs::remove_file(&target).map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("delete error: {e}"),
            )
        })?;
    }

    let new_hash = rebuild_snapshot(state)?;

    Ok(serde_json::json!({
        "ok": true,
        "path": raw_path,
        "existed": existed,
        "source_hash": new_hash,
    }))
}

fn exec_move(
    state: &ServerState,
    args: &serde_json::Value,
) -> Result<serde_json::Value, (StatusCode, String)> {
    let from = args["from"].as_str().ok_or_else(|| {
        (
            StatusCode::BAD_REQUEST,
            "missing required param: from".into(),
        )
    })?;
    let to = args["to"]
        .as_str()
        .ok_or_else(|| (StatusCode::BAD_REQUEST, "missing required param: to".into()))?;

    let source_root = canon_root(state)?;
    let from_target = validate_path(&source_root, from)?;
    let to_target = validate_path(&source_root, to)?;

    if !from_target.exists() {
        return Err((StatusCode::NOT_FOUND, format!("source not found: {from}")));
    }

    if to_target.exists() {
        return Err((
            StatusCode::CONFLICT,
            format!("destination already exists: {to}"),
        ));
    }

    // Ensure destination parent exists.
    if let Some(parent) = to_target.parent() {
        std::fs::create_dir_all(parent).map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("mkdir error: {e}"),
            )
        })?;
    }

    std::fs::rename(&from_target, &to_target).map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("rename error: {e}"),
        )
    })?;

    let new_hash = rebuild_snapshot(state)?;

    Ok(serde_json::json!({
        "ok": true,
        "from": from,
        "to": to,
        "source_hash": new_hash,
    }))
}

fn exec_mkdir(
    state: &ServerState,
    args: &serde_json::Value,
) -> Result<serde_json::Value, (StatusCode, String)> {
    let raw_path = args["path"].as_str().ok_or_else(|| {
        (
            StatusCode::BAD_REQUEST,
            "missing required param: path".into(),
        )
    })?;

    let source_root = canon_root(state)?;
    let target = validate_path(&source_root, raw_path)?;

    let already_existed = target.is_dir();
    std::fs::create_dir_all(&target).map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("mkdir error: {e}"),
        )
    })?;

    Ok(serde_json::json!({
        "ok": true,
        "path": raw_path,
        "created": !already_existed,
    }))
}

// ---------------------------------------------------------------------------
// New tool implementations: Validation operations
// ---------------------------------------------------------------------------

fn exec_validate_content(
    args: &serde_json::Value,
) -> Result<serde_json::Value, (StatusCode, String)> {
    let raw_path = args["path"].as_str().ok_or_else(|| {
        (
            StatusCode::BAD_REQUEST,
            "missing required param: path".into(),
        )
    })?;
    let content = args["content"].as_str().ok_or_else(|| {
        (
            StatusCode::BAD_REQUEST,
            "missing required param: content".into(),
        )
    })?;

    let issues = validate::validate_file_content(raw_path, content);
    let errors: Vec<_> = issues.iter().filter(|i| i.severity == "error").collect();
    let warnings: Vec<_> = issues.iter().filter(|i| i.severity == "warning").collect();

    Ok(serde_json::json!({
        "path": raw_path,
        "clean": errors.is_empty(),
        "errors": errors.len(),
        "warnings": warnings.len(),
        "issues": issues,
    }))
}

fn exec_check_conflict(
    state: &ServerState,
    args: &serde_json::Value,
) -> Result<serde_json::Value, (StatusCode, String)> {
    let raw_path = args["path"].as_str().ok_or_else(|| {
        (
            StatusCode::BAD_REQUEST,
            "missing required param: path".into(),
        )
    })?;

    let source_root = canon_root(state)?;
    let _target = validate_path(&source_root, raw_path)?;

    // Collect all existing paths from snapshot to check case-insensitive collisions.
    let lock = state
        .current
        .lock()
        .map_err(|_| (StatusCode::INTERNAL_SERVER_ERROR, "lock poisoned".into()))?;

    let proposed_lower = raw_path.to_lowercase();
    let mut conflicts: Vec<serde_json::Value> = Vec::new();

    for entry in &lock.entries {
        // Exact duplicate.
        if entry.path == raw_path {
            conflicts.push(serde_json::json!({
                "type": "exact_duplicate",
                "existing_path": entry.path,
            }));
        }
        // Case-insensitive collision.
        else if entry.path.to_lowercase() == proposed_lower {
            conflicts.push(serde_json::json!({
                "type": "case_collision",
                "existing_path": entry.path,
                "proposed_path": raw_path,
            }));
        }
    }

    // Check for Roblox naming conflicts: init.server.luau vs init.client.luau
    // in the same directory would confuse Rojo.
    let proposed_file = Path::new(raw_path)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or_default();
    if proposed_file.starts_with("init.") {
        let proposed_parent = Path::new(raw_path)
            .parent()
            .map(|p| p.to_string_lossy().replace('\\', "/"))
            .unwrap_or_default();
        for entry in &lock.entries {
            let entry_parent = Path::new(&entry.path)
                .parent()
                .map(|p| p.to_string_lossy().replace('\\', "/"))
                .unwrap_or_default();
            let entry_file = Path::new(&entry.path)
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or_default();
            if entry_parent == proposed_parent
                && entry_file.starts_with("init.")
                && entry.path != raw_path
            {
                conflicts.push(serde_json::json!({
                    "type": "rojo_init_conflict",
                    "existing_path": entry.path,
                    "proposed_path": raw_path,
                    "detail": "Multiple init.* files in the same directory confuse Rojo",
                }));
            }
        }
    }

    Ok(serde_json::json!({
        "path": raw_path,
        "has_conflict": !conflicts.is_empty(),
        "conflicts": conflicts,
    }))
}

// ---------------------------------------------------------------------------
// New tool implementations: Pipeline operations
// ---------------------------------------------------------------------------

fn exec_safe_write(
    state: &ServerState,
    args: &serde_json::Value,
) -> Result<serde_json::Value, (StatusCode, String)> {
    let raw_path = args["path"].as_str().ok_or_else(|| {
        (
            StatusCode::BAD_REQUEST,
            "missing required param: path".into(),
        )
    })?;
    let content = args["content"].as_str().ok_or_else(|| {
        (
            StatusCode::BAD_REQUEST,
            "missing required param: content".into(),
        )
    })?;
    let require_strict = args["require_strict"].as_bool().unwrap_or(true);

    // Phase 1: Validate content in memory.
    let issues = validate::validate_file_content(raw_path, content);
    let errors: Vec<_> = issues.iter().filter(|i| i.severity == "error").collect();

    // If require_strict is false, filter out the strict-mode error.
    let blocking_errors: Vec<_> = if require_strict {
        errors
    } else {
        errors
            .into_iter()
            .filter(|i| i.rule != "strict-mode")
            .collect()
    };

    if !blocking_errors.is_empty() {
        return Ok(serde_json::json!({
            "ok": false,
            "reason": "validation_failed",
            "path": raw_path,
            "errors": blocking_errors,
            "all_issues": issues,
        }));
    }

    // Phase 2: Check for path conflicts.
    let source_root = canon_root(state)?;
    let target = validate_path(&source_root, raw_path)?;

    // Phase 3: Write to disk.
    if let Some(parent) = target.parent() {
        std::fs::create_dir_all(parent).map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("mkdir error: {e}"),
            )
        })?;
    }

    std::fs::write(&target, content.as_bytes()).map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("write error: {e}"),
        )
    })?;

    // Phase 4: Rebuild snapshot.
    let hash = sha256_hex(content.as_bytes());
    let new_source_hash = rebuild_snapshot(state)?;

    let warnings: Vec<_> = issues.iter().filter(|i| i.severity == "warning").collect();

    Ok(serde_json::json!({
        "ok": true,
        "path": raw_path,
        "sha256": hash,
        "bytes": content.len(),
        "source_hash": new_source_hash,
        "warnings": warnings,
    }))
}

fn exec_describe_changes(
    state: &ServerState,
    args: &serde_json::Value,
) -> Result<serde_json::Value, (StatusCode, String)> {
    let since_hash = args["since_hash"].as_str();

    // Find the old snapshot — either the requested one, or the earliest in history.
    let old = {
        let lock = state
            .history
            .lock()
            .map_err(|_| (StatusCode::INTERNAL_SERVER_ERROR, "lock poisoned".into()))?;

        if let Some(hash) = since_hash {
            lock.get(hash).cloned()
        } else {
            // Use the history_order to get the earliest snapshot.
            let order = state
                .history_order
                .lock()
                .map_err(|_| (StatusCode::INTERNAL_SERVER_ERROR, "lock poisoned".into()))?;
            order.front().and_then(|h| lock.get(h).cloned())
        }
    };

    let old = match old {
        Some(o) => o,
        None => {
            return Ok(serde_json::json!({
                "description": "No previous snapshot available for comparison.",
                "since_hash": since_hash,
            }));
        }
    };

    let current = {
        let lock = state
            .current
            .lock()
            .map_err(|_| (StatusCode::INTERNAL_SERVER_ERROR, "lock poisoned".into()))?;
        Arc::clone(&lock)
    };

    if old.fingerprint == current.fingerprint {
        return Ok(serde_json::json!({
            "description": "No changes since the reference snapshot.",
            "since_hash": old.fingerprint,
            "current_hash": current.fingerprint,
        }));
    }

    let diff = diff_snapshots(&old, &current);

    let mut lines: Vec<String> = Vec::new();
    let total = diff.added.len() + diff.modified.len() + diff.deleted.len();
    lines.push(format!(
        "{} file(s) changed: {} added, {} modified, {} deleted.",
        total,
        diff.added.len(),
        diff.modified.len(),
        diff.deleted.len()
    ));

    for e in &diff.added {
        lines.push(format!("  + {} ({} bytes)", e.path, e.bytes));
    }
    for e in &diff.modified {
        let delta = e.current_bytes as i64 - e.previous_bytes as i64;
        let sign = if delta >= 0 { "+" } else { "" };
        lines.push(format!("  ~ {} ({}{} bytes)", e.path, sign, delta));
    }
    for e in &diff.deleted {
        lines.push(format!("  - {} ({} bytes removed)", e.path, e.bytes));
    }

    Ok(serde_json::json!({
        "description": lines.join("\n"),
        "since_hash": old.fingerprint,
        "current_hash": current.fingerprint,
        "added": diff.added.len(),
        "modified": diff.modified.len(),
        "deleted": diff.deleted.len(),
    }))
}

fn exec_tree(
    state: &ServerState,
    args: &serde_json::Value,
) -> Result<serde_json::Value, (StatusCode, String)> {
    let raw_path = args["path"].as_str().unwrap_or("");
    let max_depth = args["depth"].as_u64().unwrap_or(3) as usize;

    let lock = state
        .current
        .lock()
        .map_err(|_| (StatusCode::INTERNAL_SERVER_ERROR, "lock poisoned".into()))?;

    // Build a tree from the snapshot entries, filtered by prefix.
    let prefix = if raw_path.is_empty() {
        String::new()
    } else {
        let mut p = raw_path.to_string();
        if !p.ends_with('/') {
            p.push('/');
        }
        p
    };

    // Collect unique directories and files under prefix, capped by depth.
    let mut tree_nodes: BTreeMap<String, bool> = BTreeMap::new(); // path -> is_file

    for entry in &lock.entries {
        let path = if prefix.is_empty() {
            &entry.path
        } else if let Some(rest) = entry.path.strip_prefix(&prefix) {
            rest
        } else {
            continue;
        };

        let parts: Vec<&str> = path.split('/').collect();
        // Add each directory prefix up to max_depth, plus the file itself if within depth.
        for i in 0..parts.len().min(max_depth + 1) {
            let segment_path = if prefix.is_empty() {
                parts[..=i].join("/")
            } else {
                format!("{}{}", prefix, parts[..=i].join("/"))
            };
            let is_file = i == parts.len() - 1;
            tree_nodes.insert(segment_path, is_file);
        }
    }

    // Render as indented text.
    let mut lines: Vec<String> = Vec::new();
    for (path, is_file) in &tree_nodes {
        let display_path = if prefix.is_empty() {
            path.as_str()
        } else {
            path.strip_prefix(&prefix).unwrap_or(path)
        };
        let depth = display_path.matches('/').count();
        let indent = "  ".repeat(depth);
        let name = display_path.rsplit('/').next().unwrap_or(display_path);
        let suffix = if *is_file { "" } else { "/" };
        lines.push(format!("{indent}{name}{suffix}"));
    }

    Ok(serde_json::json!({
        "path": raw_path,
        "depth": max_depth,
        "tree": lines.join("\n"),
        "node_count": tree_nodes.len(),
    }))
}

// ---------------------------------------------------------------------------
// New tool implementations: Observability operations
// ---------------------------------------------------------------------------

fn exec_status(state: &ServerState) -> Result<serde_json::Value, (StatusCode, String)> {
    use std::sync::atomic::Ordering;

    let m = &state.metrics;
    let polls = m.polls.load(Ordering::Relaxed);
    let duration_sum_us = m.poll_duration_sum_us.load(Ordering::Relaxed);
    let avg_poll_ms = if polls > 0 {
        (duration_sum_us as f64 / polls as f64) / 1_000.0
    } else {
        0.0
    };
    let entries = m.entries.load(Ordering::Relaxed);
    let ws_connections = m.ws_connections.load(Ordering::Relaxed);
    let events_emitted = m.events_emitted.load(Ordering::Relaxed);
    let cache_hits = m.cache_hits.load(Ordering::Relaxed);
    let cache_misses = m.cache_misses.load(Ordering::Relaxed);
    let total = cache_hits + cache_misses;
    let cache_ratio = if total > 0 {
        cache_hits as f64 / total as f64
    } else {
        0.0
    };

    let current_hash = {
        let lock = state
            .current
            .lock()
            .map_err(|_| (StatusCode::INTERNAL_SERVER_ERROR, "lock poisoned".into()))?;
        lock.fingerprint.clone()
    };

    let history_count = {
        let lock = state
            .history
            .lock()
            .map_err(|_| (StatusCode::INTERNAL_SERVER_ERROR, "lock poisoned".into()))?;
        lock.len()
    };

    let sequence = {
        let lock = state
            .sequence
            .lock()
            .map_err(|_| (StatusCode::INTERNAL_SERVER_ERROR, "lock poisoned".into()))?;
        *lock
    };

    Ok(serde_json::json!({
        "status": "running",
        "version": env!("CARGO_PKG_VERSION"),
        "current_fingerprint": current_hash,
        "source_entries": entries,
        "ws_connections": ws_connections,
        "events_emitted": events_emitted,
        "sequence": sequence,
        "history_snapshots": history_count,
        "polls": polls,
        "avg_poll_ms": format!("{avg_poll_ms:.2}"),
        "cache_hit_ratio": format!("{cache_ratio:.3}"),
        "includes": state.includes,
    }))
}

fn exec_events(
    state: &ServerState,
    args: &serde_json::Value,
) -> Result<serde_json::Value, (StatusCode, String)> {
    let limit = args["limit"].as_u64().unwrap_or(10) as usize;

    // History order gives us the chronological sequence of snapshot hashes.
    let order = state
        .history_order
        .lock()
        .map_err(|_| (StatusCode::INTERNAL_SERVER_ERROR, "lock poisoned".into()))?;

    let history = state
        .history
        .lock()
        .map_err(|_| (StatusCode::INTERNAL_SERVER_ERROR, "lock poisoned".into()))?;

    // Take the last `limit + 1` entries so we can diff consecutive pairs.
    let total = order.len();
    let start = total.saturating_sub(limit + 1);
    let hashes: Vec<String> = order.iter().skip(start).cloned().collect();

    let mut events: Vec<serde_json::Value> = Vec::new();
    for i in 1..hashes.len() {
        let prev = history.get(&hashes[i - 1]);
        let curr = history.get(&hashes[i]);
        if let (Some(prev), Some(curr)) = (prev, curr) {
            let diff = diff_snapshots(prev, curr);
            events.push(serde_json::json!({
                "sequence": start + i,
                "from_hash": diff.previous_fingerprint,
                "to_hash": diff.current_fingerprint,
                "added": diff.added.len(),
                "modified": diff.modified.len(),
                "deleted": diff.deleted.len(),
                "added_paths": diff.added.iter().map(|e| &e.path).collect::<Vec<_>>(),
                "modified_paths": diff.modified.iter().map(|e| &e.path).collect::<Vec<_>>(),
                "deleted_paths": diff.deleted.iter().map(|e| &e.path).collect::<Vec<_>>(),
            }));
        }
    }

    Ok(serde_json::json!({
        "total_snapshots": total,
        "showing": events.len(),
        "events": events,
    }))
}

// ---------------------------------------------------------------------------
// Search implementation (existing)
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
        let dir = entry.path.split('/').take(2).collect::<Vec<_>>().join("/");
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

// ---------------------------------------------------------------------------
// Pipeline orchestration — vsync_pipeline
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Deserialize)]
struct PipelineStep {
    tool: String,
    #[serde(default)]
    args: serde_json::Value,
    id: String,
    #[serde(default)]
    depends_on: Vec<String>,
    #[serde(default)]
    collect: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
struct PipelineError {
    step_id: String,
    error: String,
}

#[derive(Debug, Clone, Serialize)]
struct PipelineResult {
    completed: usize,
    failed: usize,
    results: serde_json::Map<String, serde_json::Value>,
    errors: Vec<PipelineError>,
    execution_order: Vec<String>,
    total_ms: u64,
}

/// Execute a single pipeline step by dispatching to the appropriate tool.
fn execute_step(
    state: &ServerState,
    step: &PipelineStep,
    namespace: &HashMap<String, serde_json::Value>,
) -> Result<serde_json::Value, String> {
    // Interpolate variable references in args: "${step_id.field}" syntax.
    let args = interpolate_args(&step.args, namespace);

    match step.tool.as_str() {
        "vsync_health" => exec_health().map_err(|(_, e)| e),
        "vsync_snapshot" => exec_snapshot(state).map_err(|(_, e)| e),
        "vsync_diff" => exec_diff(state, &args).map_err(|(_, e)| e),
        "vsync_sources" => exec_sources(state).map_err(|(_, e)| e),
        "vsync_source" => exec_source(state, &args).map_err(|(_, e)| e),
        "vsync_validate" => exec_validate(state).map_err(|(_, e)| e),
        "vsync_metrics" => exec_metrics(state).map_err(|(_, e)| e),
        "vsync_patch" => exec_patch(state, &args).map_err(|(_, e)| e),
        "vsync_doctor" => exec_doctor(state).map_err(|(_, e)| e),
        "vsync_project" => exec_project(state).map_err(|(_, e)| e),
        "vsync_search" => exec_search(state, &args).map_err(|(_, e)| e),
        "vsync_stats" => exec_stats(state).map_err(|(_, e)| e),
        "vsync_ls" => exec_ls(state, &args).map_err(|(_, e)| e),
        "vsync_read_batch" => exec_read_batch(state, &args).map_err(|(_, e)| e),
        "vsync_file_info" => exec_file_info(state, &args).map_err(|(_, e)| e),
        "vsync_grep" => exec_grep(state, &args).map_err(|(_, e)| e),
        "vsync_write" => exec_write(state, &args).map_err(|(_, e)| e),
        "vsync_delete" => exec_delete(state, &args).map_err(|(_, e)| e),
        "vsync_move" => exec_move(state, &args).map_err(|(_, e)| e),
        "vsync_mkdir" => exec_mkdir(state, &args).map_err(|(_, e)| e),
        "vsync_validate_content" => exec_validate_content(&args).map_err(|(_, e)| e),
        "vsync_check_conflict" => exec_check_conflict(state, &args).map_err(|(_, e)| e),
        "vsync_safe_write" => exec_safe_write(state, &args).map_err(|(_, e)| e),
        "vsync_describe_changes" => exec_describe_changes(state, &args).map_err(|(_, e)| e),
        "vsync_tree" => exec_tree(state, &args).map_err(|(_, e)| e),
        "vsync_status" => exec_status(state).map_err(|(_, e)| e),
        "vsync_events" => exec_events(state, &args).map_err(|(_, e)| e),
        other => Err(format!("unknown tool in pipeline: {other}")),
    }
}

/// Interpolate `${step_id.field}` references in JSON args using the namespace.
fn interpolate_args(
    args: &serde_json::Value,
    namespace: &HashMap<String, serde_json::Value>,
) -> serde_json::Value {
    match args {
        serde_json::Value::String(s) => {
            // Full-value substitution: if the entire string is "${x.y}", replace
            // with the actual JSON value (preserving type).
            if s.starts_with("${") && s.ends_with('}') && s.matches("${").count() == 1 {
                let ref_path = &s[2..s.len() - 1];
                if let Some(val) = resolve_ref(ref_path, namespace) {
                    return val;
                }
            }
            // Partial interpolation: replace ${...} within the string.
            let mut result = s.clone();
            while let Some(start) = result.find("${") {
                let rest = &result[start + 2..];
                if let Some(end) = rest.find('}') {
                    let ref_path = &rest[..end];
                    let replacement = resolve_ref(ref_path, namespace)
                        .and_then(|v| match &v {
                            serde_json::Value::String(s) => Some(s.clone()),
                            other => Some(other.to_string()),
                        })
                        .unwrap_or_default();
                    result = format!(
                        "{}{}{}",
                        &result[..start],
                        replacement,
                        &result[start + 2 + end + 1..]
                    );
                } else {
                    break;
                }
            }
            serde_json::Value::String(result)
        }
        serde_json::Value::Object(map) => {
            let mut out = serde_json::Map::new();
            for (k, v) in map {
                out.insert(k.clone(), interpolate_args(v, namespace));
            }
            serde_json::Value::Object(out)
        }
        serde_json::Value::Array(arr) => {
            serde_json::Value::Array(arr.iter().map(|v| interpolate_args(v, namespace)).collect())
        }
        other => other.clone(),
    }
}

/// Resolve a dotted reference like "step1.content" from the namespace.
fn resolve_ref(
    ref_path: &str,
    namespace: &HashMap<String, serde_json::Value>,
) -> Option<serde_json::Value> {
    let mut parts = ref_path.splitn(2, '.');
    let step_id = parts.next()?;
    let field = parts.next();

    let value = namespace.get(step_id)?;
    match field {
        Some(f) => value.get(f).cloned(),
        None => Some(value.clone()),
    }
}

/// Topological sort of pipeline steps. Returns levels where each level
/// contains step indices that can execute in parallel.
fn topological_levels(steps: &[PipelineStep]) -> Result<Vec<Vec<usize>>, String> {
    let n = steps.len();
    let id_to_idx: HashMap<&str, usize> = steps
        .iter()
        .enumerate()
        .map(|(i, s)| (s.id.as_str(), i))
        .collect();

    // Validate all dependencies exist.
    for step in steps {
        for dep in &step.depends_on {
            if !id_to_idx.contains_key(dep.as_str()) {
                return Err(format!(
                    "step '{}' depends on unknown step '{}'",
                    step.id, dep
                ));
            }
        }
    }

    // Build in-degree map and adjacency list.
    let mut in_degree = vec![0usize; n];
    let mut dependents: Vec<Vec<usize>> = vec![Vec::new(); n];

    for (i, step) in steps.iter().enumerate() {
        for dep in &step.depends_on {
            if let Some(&dep_idx) = id_to_idx.get(dep.as_str()) {
                in_degree[i] += 1;
                dependents[dep_idx].push(i);
            }
        }
    }

    // Kahn's algorithm with level tracking.
    let mut levels: Vec<Vec<usize>> = Vec::new();
    let mut queue: VecDeque<usize> = VecDeque::new();

    for i in 0..n {
        if in_degree[i] == 0 {
            queue.push_back(i);
        }
    }

    let mut visited = 0usize;

    while !queue.is_empty() {
        let level: Vec<usize> = queue.drain(..).collect();
        let mut next_queue = VecDeque::new();

        for &idx in &level {
            visited += 1;
            for &dep_idx in &dependents[idx] {
                in_degree[dep_idx] -= 1;
                if in_degree[dep_idx] == 0 {
                    next_queue.push_back(dep_idx);
                }
            }
        }

        levels.push(level);
        queue = next_queue;
    }

    if visited != n {
        return Err("pipeline contains a dependency cycle".to_string());
    }

    Ok(levels)
}

/// Execute the pipeline in sequential mode: steps run one at a time in order.
async fn execute_sequential(
    steps: Vec<PipelineStep>,
    stop_on_error: bool,
    state: &Arc<ServerState>,
) -> Result<PipelineResult, (StatusCode, String)> {
    let start = Instant::now();
    let mut namespace: HashMap<String, serde_json::Value> = HashMap::new();
    let mut results = serde_json::Map::new();
    let mut errors: Vec<PipelineError> = Vec::new();
    let mut execution_order: Vec<String> = Vec::new();
    let mut completed = 0usize;

    for step in &steps {
        execution_order.push(step.id.clone());

        match execute_step(state, step, &namespace) {
            Ok(value) => {
                completed += 1;
                // Store in namespace under step id (always) and collect name (if set).
                namespace.insert(step.id.clone(), value.clone());
                if let Some(ref collect) = step.collect {
                    namespace.insert(collect.clone(), value.clone());
                }
                results.insert(step.id.clone(), value);
            }
            Err(error) => {
                errors.push(PipelineError {
                    step_id: step.id.clone(),
                    error,
                });
                if stop_on_error {
                    break;
                }
            }
        }
    }

    Ok(PipelineResult {
        completed,
        failed: errors.len(),
        results,
        errors,
        execution_order,
        total_ms: start.elapsed().as_millis() as u64,
    })
}

/// Execute the pipeline in parallel mode: all steps run concurrently (imap_unordered).
async fn execute_parallel(
    steps: Vec<PipelineStep>,
    stop_on_error: bool,
    state: &Arc<ServerState>,
) -> Result<PipelineResult, (StatusCode, String)> {
    let start = Instant::now();
    let state = Arc::clone(state);

    // Spawn all steps as concurrent tasks on the blocking pool (since the
    // underlying tool implementations are synchronous).
    let mut handles = Vec::with_capacity(steps.len());
    for step in steps.clone() {
        let state_clone = Arc::clone(&state);
        let handle = tokio::task::spawn_blocking(move || {
            let namespace = HashMap::new();
            let result = execute_step(&state_clone, &step, &namespace);
            (step.id.clone(), step.collect.clone(), result)
        });
        handles.push(handle);
    }

    let mut results = serde_json::Map::new();
    let mut errors: Vec<PipelineError> = Vec::new();
    let mut execution_order: Vec<String> = Vec::new();
    let mut completed = 0usize;

    for handle in handles {
        match handle.await {
            Ok((id, collect, Ok(value))) => {
                completed += 1;
                execution_order.push(id.clone());
                if let Some(ref collect_name) = collect {
                    results.insert(collect_name.clone(), value.clone());
                }
                results.insert(id, value);
            }
            Ok((id, _, Err(error))) => {
                execution_order.push(id.clone());
                errors.push(PipelineError { step_id: id, error });
                if stop_on_error {
                    break;
                }
            }
            Err(join_error) => {
                errors.push(PipelineError {
                    step_id: "unknown".to_string(),
                    error: format!("task join error: {join_error}"),
                });
                if stop_on_error {
                    break;
                }
            }
        }
    }

    Ok(PipelineResult {
        completed,
        failed: errors.len(),
        results,
        errors,
        execution_order,
        total_ms: start.elapsed().as_millis() as u64,
    })
}

/// Execute the pipeline in auto mode: topological sort by dependencies,
/// execute independent steps within each level concurrently.
async fn execute_auto(
    steps: Vec<PipelineStep>,
    stop_on_error: bool,
    state: &Arc<ServerState>,
) -> Result<PipelineResult, (StatusCode, String)> {
    let start = Instant::now();

    let levels = topological_levels(&steps).map_err(|e| (StatusCode::BAD_REQUEST, e))?;

    let namespace = Arc::new(std::sync::Mutex::new(
        HashMap::<String, serde_json::Value>::new(),
    ));
    let mut results = serde_json::Map::new();
    let mut errors: Vec<PipelineError> = Vec::new();
    let mut execution_order: Vec<String> = Vec::new();
    let mut completed = 0usize;
    let mut should_stop = false;

    for level in &levels {
        if should_stop {
            break;
        }

        if level.len() == 1 {
            // Single step — run inline, no spawn overhead.
            let step = &steps[level[0]];
            execution_order.push(step.id.clone());
            let ns = namespace.lock().unwrap().clone();

            match execute_step(state, step, &ns) {
                Ok(value) => {
                    completed += 1;
                    let mut ns = namespace.lock().unwrap();
                    ns.insert(step.id.clone(), value.clone());
                    if let Some(ref collect) = step.collect {
                        ns.insert(collect.clone(), value.clone());
                    }
                    results.insert(step.id.clone(), value);
                }
                Err(error) => {
                    errors.push(PipelineError {
                        step_id: step.id.clone(),
                        error,
                    });
                    if stop_on_error {
                        should_stop = true;
                    }
                }
            }
        } else {
            // Multiple steps — run concurrently on the blocking pool.
            let mut handles = Vec::with_capacity(level.len());
            for &idx in level {
                let step = steps[idx].clone();
                let state_clone = Arc::clone(state);
                let ns_clone = namespace.lock().unwrap().clone();
                let handle = tokio::task::spawn_blocking(move || {
                    let result = execute_step(&state_clone, &step, &ns_clone);
                    (step.id.clone(), step.collect.clone(), result)
                });
                handles.push(handle);
            }

            for handle in handles {
                match handle.await {
                    Ok((id, collect, Ok(value))) => {
                        completed += 1;
                        execution_order.push(id.clone());
                        let mut ns = namespace.lock().unwrap();
                        ns.insert(id.clone(), value.clone());
                        if let Some(ref collect_name) = collect {
                            ns.insert(collect_name.clone(), value.clone());
                        }
                        results.insert(id, value);
                    }
                    Ok((id, _, Err(error))) => {
                        execution_order.push(id.clone());
                        errors.push(PipelineError { step_id: id, error });
                        if stop_on_error {
                            should_stop = true;
                        }
                    }
                    Err(join_error) => {
                        errors.push(PipelineError {
                            step_id: "unknown".to_string(),
                            error: format!("task join error: {join_error}"),
                        });
                        if stop_on_error {
                            should_stop = true;
                        }
                    }
                }
            }
        }
    }

    Ok(PipelineResult {
        completed,
        failed: errors.len(),
        results,
        errors,
        execution_order,
        total_ms: start.elapsed().as_millis() as u64,
    })
}

/// Top-level pipeline executor: parse args, dispatch to the appropriate mode.
async fn exec_pipeline(
    state: &Arc<ServerState>,
    args: &serde_json::Value,
) -> Result<serde_json::Value, (StatusCode, String)> {
    let steps: Vec<PipelineStep> =
        serde_json::from_value(args.get("steps").cloned().ok_or_else(|| {
            (
                StatusCode::BAD_REQUEST,
                "missing required param: steps".into(),
            )
        })?)
        .map_err(|e| (StatusCode::BAD_REQUEST, format!("invalid steps: {e}")))?;

    if steps.is_empty() {
        return Ok(serde_json::json!({
            "completed": 0,
            "failed": 0,
            "results": {},
            "errors": [],
            "execution_order": [],
            "total_ms": 0,
        }));
    }

    // Validate step IDs are unique.
    let mut seen_ids = HashSet::new();
    for step in &steps {
        if !seen_ids.insert(&step.id) {
            return Err((
                StatusCode::BAD_REQUEST,
                format!("duplicate step id: {}", step.id),
            ));
        }
    }

    let mode = args.get("mode").and_then(|v| v.as_str()).unwrap_or("auto");
    let stop_on_error = args
        .get("stop_on_error")
        .and_then(|v| v.as_bool())
        .unwrap_or(true);

    let result = match mode {
        "sequential" => execute_sequential(steps, stop_on_error, state).await?,
        "parallel" => execute_parallel(steps, stop_on_error, state).await?,
        "auto" | _ => execute_auto(steps, stop_on_error, state).await?,
    };

    serde_json::to_value(&result).map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("serialize error: {e}"),
        )
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
        assert!(
            tools[1]["inputSchema"]["required"]
                .as_array()
                .unwrap()
                .iter()
                .all(|v| v != "count")
        );
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

    #[test]
    fn tool_count_matches_expected() {
        // Ensure we don't accidentally drop tools. 13 existing + 14 new + 1 pipeline + 5 rbxl = 33.
        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let tools = rt.block_on(async { handle_mcp_tools().await });
        assert_eq!(tools.0.len(), 33, "expected 33 MCP tools");
    }

    #[test]
    fn validate_path_rejects_traversal() {
        let root = PathBuf::from("/tmp/test");
        assert!(validate_path(&root, "../etc/passwd").is_err());
        assert!(validate_path(&root, "/absolute/path").is_err());
        assert!(validate_path(&root, "src/Server/ok.luau").is_ok());
    }

    #[test]
    fn sha256_hex_deterministic() {
        let a = sha256_hex(b"hello world");
        let b = sha256_hex(b"hello world");
        assert_eq!(a, b);
        assert_eq!(a.len(), 64); // 256 bits = 64 hex chars
    }

    #[test]
    fn topological_sort_linear() {
        let steps = vec![
            PipelineStep {
                tool: "vsync_health".into(),
                args: serde_json::Value::Null,
                id: "a".into(),
                depends_on: vec![],
                collect: None,
            },
            PipelineStep {
                tool: "vsync_health".into(),
                args: serde_json::Value::Null,
                id: "b".into(),
                depends_on: vec!["a".into()],
                collect: None,
            },
            PipelineStep {
                tool: "vsync_health".into(),
                args: serde_json::Value::Null,
                id: "c".into(),
                depends_on: vec!["b".into()],
                collect: None,
            },
        ];
        let levels = topological_levels(&steps).unwrap();
        assert_eq!(levels.len(), 3);
        assert_eq!(levels[0], vec![0]);
        assert_eq!(levels[1], vec![1]);
        assert_eq!(levels[2], vec![2]);
    }

    #[test]
    fn topological_sort_parallel() {
        let steps = vec![
            PipelineStep {
                tool: "vsync_health".into(),
                args: serde_json::Value::Null,
                id: "a".into(),
                depends_on: vec![],
                collect: None,
            },
            PipelineStep {
                tool: "vsync_health".into(),
                args: serde_json::Value::Null,
                id: "b".into(),
                depends_on: vec![],
                collect: None,
            },
            PipelineStep {
                tool: "vsync_health".into(),
                args: serde_json::Value::Null,
                id: "c".into(),
                depends_on: vec!["a".into(), "b".into()],
                collect: None,
            },
        ];
        let levels = topological_levels(&steps).unwrap();
        assert_eq!(levels.len(), 2);
        assert_eq!(levels[0].len(), 2); // a and b in parallel
        assert_eq!(levels[1], vec![2]); // c after both
    }

    #[test]
    fn topological_sort_cycle_detected() {
        let steps = vec![
            PipelineStep {
                tool: "vsync_health".into(),
                args: serde_json::Value::Null,
                id: "a".into(),
                depends_on: vec!["b".into()],
                collect: None,
            },
            PipelineStep {
                tool: "vsync_health".into(),
                args: serde_json::Value::Null,
                id: "b".into(),
                depends_on: vec!["a".into()],
                collect: None,
            },
        ];
        assert!(topological_levels(&steps).is_err());
    }

    #[test]
    fn interpolate_full_ref() {
        let mut ns = HashMap::new();
        ns.insert(
            "step1".to_string(),
            serde_json::json!({"path": "src/test.luau", "content": "hello"}),
        );
        let args = serde_json::json!({"path": "${step1.path}"});
        let result = interpolate_args(&args, &ns);
        assert_eq!(result["path"], "src/test.luau");
    }

    #[test]
    fn interpolate_partial_ref() {
        let mut ns = HashMap::new();
        ns.insert("s1".to_string(), serde_json::json!({"dir": "src/Server"}));
        let args = serde_json::json!({"path": "${s1.dir}/NewFile.luau"});
        let result = interpolate_args(&args, &ns);
        assert_eq!(result["path"], "src/Server/NewFile.luau");
    }
}

// ---------------------------------------------------------------------------
// RBXL MCP tool executors
// ---------------------------------------------------------------------------

fn exec_rbxl_load(
    state: &Arc<ServerState>,
    args: &serde_json::Value,
) -> Result<serde_json::Value, (StatusCode, String)> {
    let raw_path = args["path"]
        .as_str()
        .ok_or_else(|| (StatusCode::BAD_REQUEST, "missing 'path' argument".to_string()))?;

    let path = PathBuf::from(raw_path);
    let resolved = if path.is_absolute() {
        path
    } else {
        state.root.join(&path)
    };

    if !resolved.exists() {
        return Err((
            StatusCode::NOT_FOUND,
            format!("file not found: {}", resolved.display()),
        ));
    }

    let dom = RbxlLoader::load_file(&resolved).map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("parse error: {e}"),
        )
    })?;

    let ref_map = crate::rbxl::build_ref_map(&dom);
    let instance_count = ref_map.len();

    let mut lock = state
        .rbxl
        .lock()
        .map_err(|_| (StatusCode::INTERNAL_SERVER_ERROR, "lock".to_string()))?;
    lock.dom = Some(dom);
    lock.ref_map = ref_map;
    lock.loaded_path = Some(resolved.clone());

    Ok(serde_json::json!({
        "status": "loaded",
        "path": resolved.display().to_string(),
        "instance_count": instance_count,
    }))
}

fn exec_rbxl_tree(
    state: &Arc<ServerState>,
) -> Result<serde_json::Value, (StatusCode, String)> {
    let lock = state
        .rbxl
        .lock()
        .map_err(|_| (StatusCode::INTERNAL_SERVER_ERROR, "lock".to_string()))?;
    let dom = lock.dom.as_ref().ok_or_else(|| {
        (
            StatusCode::PRECONDITION_REQUIRED,
            "no .rbxl file loaded — call vsync_rbxl_load first".to_string(),
        )
    })?;

    let sg = RbxlLoader::to_scene_graph(dom);
    serde_json::to_value(&sg).map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))
}

fn exec_rbxl_query(
    state: &Arc<ServerState>,
    args: &serde_json::Value,
) -> Result<serde_json::Value, (StatusCode, String)> {
    let lock = state
        .rbxl
        .lock()
        .map_err(|_| (StatusCode::INTERNAL_SERVER_ERROR, "lock".to_string()))?;
    let dom = lock.dom.as_ref().ok_or_else(|| {
        (
            StatusCode::PRECONDITION_REQUIRED,
            "no .rbxl file loaded".to_string(),
        )
    })?;

    let class = args["class"].as_str();
    let tag = args["tag"].as_str();
    let name = args["name"].as_str();

    let results = RbxlLoader::query(dom, class, tag, name);
    serde_json::to_value(&results).map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))
}

fn exec_rbxl_scripts(
    state: &Arc<ServerState>,
) -> Result<serde_json::Value, (StatusCode, String)> {
    let lock = state
        .rbxl
        .lock()
        .map_err(|_| (StatusCode::INTERNAL_SERVER_ERROR, "lock".to_string()))?;
    let dom = lock.dom.as_ref().ok_or_else(|| {
        (
            StatusCode::PRECONDITION_REQUIRED,
            "no .rbxl file loaded".to_string(),
        )
    })?;

    let scripts = RbxlLoader::extract_scripts(dom);
    serde_json::to_value(&scripts).map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))
}

fn exec_rbxl_meshes(
    state: &Arc<ServerState>,
) -> Result<serde_json::Value, (StatusCode, String)> {
    let lock = state
        .rbxl
        .lock()
        .map_err(|_| (StatusCode::INTERNAL_SERVER_ERROR, "lock".to_string()))?;
    let dom = lock.dom.as_ref().ok_or_else(|| {
        (
            StatusCode::PRECONDITION_REQUIRED,
            "no .rbxl file loaded".to_string(),
        )
    })?;

    let meshes = RbxlLoader::extract_meshes(dom);
    serde_json::to_value(&meshes).map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))
}
