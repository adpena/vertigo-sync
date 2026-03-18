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
use std::time::{Duration, Instant, SystemTime};

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
            vec![
                param(
                    "limit",
                    "number",
                    "Number of recent events (default 10)",
                    false,
                ),
                param(
                    "detail",
                    "boolean",
                    "Include full file path lists in each event (default false, counts only)",
                    false,
                ),
            ],
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
        // ── Bridge compatibility layer ────────────────────────────
        tool_def(
            "vsync_bridge_manifest",
            "Describe the agent-first bridge.v1 method catalog and transport endpoints",
            vec![],
        ),
        tool_def(
            "vsync_bridge_execute",
            "Execute a single bridge.v1 method via {method, params, id?} and return normalized envelope",
            vec![
                param(
                    "method",
                    "string",
                    "Bridge method name (e.g. bridge.hello, source.read, sync.validate)",
                    true,
                ),
                param("params", "object", "Method parameters object", false),
                param(
                    "id",
                    "string",
                    "Optional request id echoed in the response envelope",
                    false,
                ),
            ],
        ),
        tool_def(
            "vsync_bridge_batch",
            "Execute multiple bridge.v1 method calls in one request for lower round-trip overhead",
            vec![
                param(
                    "calls",
                    "array",
                    "Array of {method, params?, id?} bridge method calls",
                    true,
                ),
                param(
                    "stop_on_error",
                    "boolean",
                    "Stop execution after first failure (default true)",
                    false,
                ),
                param(
                    "max_retries",
                    "number",
                    "Per-call retry attempts for retryable bridge errors (default 0)",
                    false,
                ),
                param(
                    "retry_backoff_ms",
                    "number",
                    "Base retry backoff in milliseconds (default 50, exponential)",
                    false,
                ),
                param(
                    "retry_on_codes",
                    "array",
                    "Optional bridge error codes eligible for retry (default retryable transport/internal codes)",
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
                param(
                    "class",
                    "string",
                    "Filter by ClassName (exact match)",
                    false,
                ),
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
        // ── New: History, rewind, model, config ──────────────────────
        tool_def(
            "sync_history",
            "Get recent sync event history from the event log",
            vec![param(
                "limit",
                "number",
                "Maximum entries to return (default 50, max 500)",
                false,
            )],
        ),
        tool_def(
            "sync_rewind",
            "Compute reverse diff to rewind to a historical snapshot fingerprint",
            vec![param(
                "fingerprint",
                "string",
                "Target snapshot fingerprint to rewind to",
                true,
            )],
        ),
        tool_def(
            "sync_model_manifest",
            "Get lazily-deserialized model manifest for a .rbxm/.rbxmx file",
            vec![param(
                "path",
                "string",
                "Relative path to the .rbxm/.rbxmx model file",
                true,
            )],
        ),
        tool_def(
            "sync_config",
            "Get current server configuration including feature flags",
            vec![],
        ),
        // ── Plugin state reporting ───────────────────────────────
        tool_def(
            "sync_plugin_state",
            "Get Studio plugin internal state (connection, transport, queue depths, throughput, version). Includes staleness indicator.",
            vec![],
        ),
        tool_def(
            "sync_plugin_managed",
            "Get Studio plugin managed instance index (paths, hashes, classes). Includes staleness indicator.",
            vec![],
        ),
        // ── Plugin command channel ──────────────────────────────────
        tool_def(
            "sync_plugin_command",
            "Send a command to the Studio plugin (toggle sync, force resync, adjust frame budget, run builders, set log level, time travel)",
            vec![
                param(
                    "command",
                    "string",
                    "Command: toggle_sync | force_resync | set_frame_budget | run_builders | set_log_level | time_travel",
                    true,
                ),
                param(
                    "params",
                    "object",
                    "Command parameters (e.g. {\"budget_ms\": 8} for set_frame_budget, {\"level\": \"verbose\"} for set_log_level, {\"action\": \"rewind\", \"fingerprint\": \"abc123\"} for time_travel — actions: rewind, step_back, step_forward, jump_oldest, resume_live)",
                    false,
                ),
                param(
                    "wait",
                    "boolean",
                    "Wait for plugin acknowledgment (default false, max 10s)",
                    false,
                ),
            ],
        ),
        // ── Filesystem watcher health ───────────────────────────────
        tool_def(
            "sync_watch_status",
            "Get filesystem watcher health: coalesce state, last event age, pending rebuild status",
            vec![],
        ),
        // ── File change history ─────────────────────────────────────
        tool_def(
            "sync_file_history",
            "Get change history for a specific file path across snapshots",
            vec![
                param(
                    "path",
                    "string",
                    "File path to trace (e.g. src/Server/Services/DataService.luau)",
                    true,
                ),
                param(
                    "limit",
                    "integer",
                    "Max events to return (default 20)",
                    false,
                ),
            ],
        ),
        // ── Builder codegen tools ────────────────────────────────────
        tool_def(
            "sync_scaffold_builder",
            "Create a new builder module from template. The builder generates geometry procedurally in Edit mode.",
            vec![
                param(
                    "name",
                    "string",
                    "Builder name (e.g. CoralCaveBuilder)",
                    true,
                ),
                param(
                    "zone",
                    "string",
                    "Zone name (e.g. Coral Cave, Hub, Abyss)",
                    true,
                ),
                param(
                    "y_range",
                    "string",
                    "Vertical range (e.g. '-50 to -20')",
                    false,
                ),
                param("description", "string", "What this builder creates", false),
            ],
        ),
        tool_def(
            "sync_convert_to_builder",
            "Convert a .model.json instance tree into a builder .luau module that generates the same geometry procedurally",
            vec![
                param(
                    "input_path",
                    "string",
                    "Path to .model.json file to convert",
                    true,
                ),
                param(
                    "output_path",
                    "string",
                    "Path for the generated builder .luau (default: same directory, .luau extension)",
                    false,
                ),
                param(
                    "builder_name",
                    "string",
                    "Module name for the builder (default: derived from filename)",
                    false,
                ),
            ],
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
        // Bridge compatibility layer
        "vsync_bridge_manifest" => exec_bridge_manifest(&state),
        "vsync_bridge_execute" => exec_bridge_execute(&state, &req.arguments),
        "vsync_bridge_batch" => exec_bridge_batch(&state, &req.arguments),
        // RBXL file operations
        "vsync_rbxl_load" => exec_rbxl_load(&state, &req.arguments),
        "vsync_rbxl_tree" => exec_rbxl_tree(&state),
        "vsync_rbxl_query" => exec_rbxl_query(&state, &req.arguments),
        "vsync_rbxl_scripts" => exec_rbxl_scripts(&state),
        "vsync_rbxl_meshes" => exec_rbxl_meshes(&state),
        // New: History, rewind, model, config
        "sync_history" => exec_sync_history(&state, &req.arguments),
        "sync_rewind" => exec_sync_rewind(&state, &req.arguments),
        "sync_model_manifest" => exec_sync_model_manifest(&state, &req.arguments),
        "sync_config" => exec_sync_config(&state),
        // Plugin state reporting
        "sync_plugin_state" => exec_sync_plugin_state(&state),
        "sync_plugin_managed" => exec_sync_plugin_managed(&state),
        // Plugin command channel
        "sync_plugin_command" => {
            let result = exec_sync_plugin_command(&state, &req.arguments).await;
            return result.map(Json);
        }
        // Filesystem watcher health
        "sync_watch_status" => exec_sync_watch_status(&state),
        // File change history
        "sync_file_history" => exec_sync_file_history(&state, &req.arguments),
        // Builder codegen tools
        "sync_scaffold_builder" => exec_scaffold_builder(&state, &req.arguments),
        "sync_convert_to_builder" => exec_convert_to_builder(&state, &req.arguments),
        _ => Err((StatusCode::NOT_FOUND, format!("unknown tool: {}", req.tool))),
    }?;

    Ok(Json(result))
}

const BRIDGE_PROTOCOL_VERSION: &str = "bridge.v1";

fn bridge_method_catalog() -> &'static [(&'static str, &'static str)] {
    &[
        (
            "bridge.hello",
            "Return protocol/version/server metadata and current workspace fingerprint",
        ),
        (
            "bridge.capabilities",
            "List supported bridge methods and concise descriptions",
        ),
        ("sync.health", "Health/status probe for vertigo-sync"),
        ("sync.snapshot", "Return current source snapshot"),
        ("sync.diff", "Diff current snapshot against since_hash"),
        ("sync.status", "Sync observability status report"),
        ("sync.events", "Recent sync events with sequence metadata"),
        ("sync.validate", "Run Luau + NCG validation pass"),
        ("sync.doctor", "Run determinism and sync doctor checks"),
        (
            "source.index",
            "List all source entries with path/hash/bytes",
        ),
        ("source.list", "List source directory entries"),
        ("source.read", "Read one source file by relative path"),
        (
            "source.read_batch",
            "Read multiple source files in one request",
        ),
        ("source.search", "Regex search across source content"),
        (
            "source.grep",
            "Regex search with context and bounded results",
        ),
        ("source.info", "Read source metadata for a single path"),
        ("source.tree", "Render source tree as indented text"),
        ("source.write", "Write a UTF-8 source file"),
        ("source.delete", "Delete a source file"),
        ("source.move", "Move/rename a source file"),
        ("source.mkdir", "Create a source directory"),
        (
            "source.validate_content",
            "Validate source content without writing to disk",
        ),
        (
            "source.safe_write",
            "Validate then write source file atomically",
        ),
        ("source.patch", "Apply write/delete patch batch"),
        (
            "source.describe_changes",
            "Summarize source diffs in natural language",
        ),
        (
            "source.conflict_check",
            "Check if a path collides with existing source paths",
        ),
        (
            "source.check_conflict",
            "Alias of source.conflict_check for compatibility",
        ),
        ("sync.project", "Alias of project.mappings"),
        ("project.mappings", "Return parsed project tree mappings"),
        (
            "metrics.get",
            "Return sync metrics and performance counters",
        ),
        (
            "sync.history",
            "Get recent sync event history from the event log",
        ),
        (
            "sync.rewind",
            "Compute reverse diff to rewind to a historical snapshot fingerprint",
        ),
        (
            "sync.model_manifest",
            "Get lazily-deserialized model manifest for a .rbxm/.rbxmx file",
        ),
        (
            "sync.config",
            "Get current server configuration including feature flags",
        ),
        (
            "sync.plugin_state",
            "Get Studio plugin internal state with staleness indicator",
        ),
        (
            "sync.plugin_managed",
            "Get Studio plugin managed instance index with staleness indicator",
        ),
        (
            "sync.plugin_command",
            "Send a command to the Studio plugin (toggle sync, force resync, etc.)",
        ),
        (
            "sync.watch_status",
            "Get filesystem watcher health: coalesce state, last event age, pending rebuild",
        ),
        (
            "sync.file_history",
            "Get change history for a specific file path across snapshots",
        ),
        (
            "sync.scaffold_builder",
            "Create a new builder module from template for procedural geometry generation",
        ),
        (
            "sync.convert_to_builder",
            "Convert a .model.json instance tree into a builder .luau module",
        ),
    ]
}

fn bridge_capabilities_json() -> Vec<serde_json::Value> {
    bridge_method_catalog()
        .iter()
        .map(|(name, description)| {
            serde_json::json!({
                "name": name,
                "description": description,
            })
        })
        .collect()
}

fn bridge_error_code(status: StatusCode) -> &'static str {
    match status {
        StatusCode::BAD_REQUEST => "BAD_PARAMS",
        StatusCode::NOT_FOUND => "NOT_FOUND",
        StatusCode::CONFLICT => "EDIT_CONFLICT",
        StatusCode::PAYLOAD_TOO_LARGE => "PAYLOAD_TOO_LARGE",
        StatusCode::TOO_MANY_REQUESTS => "RATE_LIMITED",
        StatusCode::SERVICE_UNAVAILABLE => "TRANSPORT_UNAVAILABLE",
        StatusCode::REQUEST_TIMEOUT | StatusCode::GATEWAY_TIMEOUT => "TIMEOUT",
        _ => "INTERNAL_ERROR",
    }
}

fn bridge_error_retryable(status: StatusCode) -> bool {
    matches!(
        status,
        StatusCode::REQUEST_TIMEOUT
            | StatusCode::TOO_MANY_REQUESTS
            | StatusCode::INTERNAL_SERVER_ERROR
            | StatusCode::BAD_GATEWAY
            | StatusCode::SERVICE_UNAVAILABLE
            | StatusCode::GATEWAY_TIMEOUT
    )
}

const BRIDGE_DEFAULT_RETRY_CODES: &[&str] = &[
    "TIMEOUT",
    "RATE_LIMITED",
    "TRANSPORT_UNAVAILABLE",
    "INTERNAL_ERROR",
];

fn parse_bridge_retry_codes(args: &serde_json::Value) -> HashSet<String> {
    if let Some(values) = args
        .get("retry_on_codes")
        .and_then(serde_json::Value::as_array)
    {
        let codes: HashSet<String> = values
            .iter()
            .filter_map(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(|value| value.to_ascii_uppercase())
            .collect();
        if !codes.is_empty() {
            return codes;
        }
    }
    BRIDGE_DEFAULT_RETRY_CODES
        .iter()
        .map(|value| (*value).to_string())
        .collect()
}

fn parse_bridge_max_retries(args: &serde_json::Value) -> usize {
    args.get("max_retries")
        .and_then(serde_json::Value::as_u64)
        .map(|value| value.min(5) as usize)
        .unwrap_or(0)
}

fn parse_bridge_retry_backoff_ms(args: &serde_json::Value) -> u64 {
    args.get("retry_backoff_ms")
        .and_then(serde_json::Value::as_u64)
        .map(|value| value.min(5_000))
        .unwrap_or(50)
}

fn should_retry_bridge_error(
    status: StatusCode,
    code: &str,
    allowed_codes: &HashSet<String>,
) -> bool {
    if allowed_codes.contains(code) {
        return true;
    }
    bridge_error_retryable(status) && allowed_codes.contains(bridge_error_code(status))
}

fn annotate_bridge_attempts(
    mut response: serde_json::Value,
    attempts: usize,
    retries: usize,
) -> serde_json::Value {
    if let Some(obj) = response.as_object_mut() {
        obj.insert("attempts".to_string(), serde_json::json!(attempts));
        obj.insert("retries".to_string(), serde_json::json!(retries));
    }
    response
}

fn bridge_ok_response(id: Option<&str>, result: serde_json::Value) -> serde_json::Value {
    serde_json::json!({
        "v": BRIDGE_PROTOCOL_VERSION,
        "kind": "response",
        "id": id,
        "ok": true,
        "result": result,
    })
}

fn bridge_error_response(
    id: Option<&str>,
    status: StatusCode,
    message: impl Into<String>,
) -> serde_json::Value {
    serde_json::json!({
        "v": BRIDGE_PROTOCOL_VERSION,
        "kind": "response",
        "id": id,
        "ok": false,
        "error": {
            "code": bridge_error_code(status),
            "message": message.into(),
            "retryable": bridge_error_retryable(status),
        }
    })
}

fn exec_bridge_manifest(state: &ServerState) -> Result<serde_json::Value, (StatusCode, String)> {
    let (source_hash, entry_count) = {
        let lock = state.current.lock().unwrap_or_else(|e| e.into_inner());
        (lock.fingerprint.clone(), lock.entries.len())
    };

    Ok(serde_json::json!({
        "protocol": BRIDGE_PROTOCOL_VERSION,
        "server": "vertigo-sync",
        "version": env!("CARGO_PKG_VERSION"),
        "workspace": {
            "source_hash": source_hash,
            "entry_count": entry_count,
        },
        "transport": {
            "mcp_tool": "vsync_bridge_execute",
            "batch_tool": "vsync_bridge_batch",
            "http": {
                "tools_path": "/mcp/tools",
                "execute_path": "/mcp/execute",
            },
            "ws": {
                "path": "/ws",
                "note": "legacy sync events plus bridge-compatible request envelopes via MCP bridge tools",
            }
        },
        "methods": bridge_capabilities_json(),
    }))
}

fn exec_bridge_method(
    state: &ServerState,
    method: &str,
    params: &serde_json::Value,
) -> Result<serde_json::Value, (StatusCode, String)> {
    match method {
        "bridge.hello" => {
            let (source_hash, entry_count) = {
                let lock = state.current.lock().unwrap_or_else(|e| e.into_inner());
                (lock.fingerprint.clone(), lock.entries.len())
            };
            Ok(serde_json::json!({
                "protocol": BRIDGE_PROTOCOL_VERSION,
                "server": "vertigo-sync",
                "version": env!("CARGO_PKG_VERSION"),
                "source_hash": source_hash,
                "entry_count": entry_count,
            }))
        }
        "bridge.capabilities" => Ok(serde_json::json!({
            "methods": bridge_capabilities_json(),
        })),
        "sync.health" => exec_health(),
        "sync.snapshot" => exec_snapshot(state),
        "sync.diff" => exec_diff(state, params),
        "sync.status" => exec_status(state),
        "sync.events" => exec_events(state, params),
        "sync.validate" => exec_validate(state),
        "sync.doctor" => exec_doctor(state),
        "source.index" => exec_sources(state),
        "source.list" => exec_ls(state, params),
        "source.read" => exec_source(state, params),
        "source.read_batch" => exec_read_batch(state, params),
        "source.search" => exec_search(state, params),
        "source.grep" => exec_grep(state, params),
        "source.info" => exec_file_info(state, params),
        "source.tree" => exec_tree(state, params),
        "source.write" => exec_write(state, params),
        "source.delete" => exec_delete(state, params),
        "source.move" => exec_move(state, params),
        "source.mkdir" => exec_mkdir(state, params),
        "source.validate_content" => exec_validate_content(params),
        "source.safe_write" => exec_safe_write(state, params),
        "source.patch" => exec_patch(state, params),
        "source.describe_changes" => exec_describe_changes(state, params),
        "source.conflict_check" => exec_check_conflict(state, params),
        "source.check_conflict" => exec_check_conflict(state, params),
        "sync.project" => exec_project(state),
        "project.mappings" => exec_project(state),
        "metrics.get" => exec_metrics(state),
        "sync.history" => exec_sync_history(state, params),
        "sync.rewind" => exec_sync_rewind(state, params),
        "sync.model_manifest" => exec_sync_model_manifest(state, params),
        "sync.config" => exec_sync_config(state),
        "sync.plugin_state" => exec_sync_plugin_state(state),
        "sync.plugin_managed" => exec_sync_plugin_managed(state),
        // Note: sync.plugin_command is async so it's not available via bridge (sync only).
        // Use the MCP tool directly for plugin commands that need wait support.
        "sync.watch_status" => exec_sync_watch_status(state),
        "sync.file_history" => exec_sync_file_history(state, params),
        "sync.scaffold_builder" => exec_scaffold_builder(state, params),
        "sync.convert_to_builder" => exec_convert_to_builder(state, params),
        _ => Err((
            StatusCode::NOT_FOUND,
            format!("unknown bridge method: {method}"),
        )),
    }
}

fn exec_bridge_execute(
    state: &ServerState,
    args: &serde_json::Value,
) -> Result<serde_json::Value, (StatusCode, String)> {
    let id = args["id"].as_str();
    let method = match args["method"].as_str() {
        Some(value) if !value.trim().is_empty() => value.trim(),
        _ => {
            return Ok(bridge_error_response(
                id,
                StatusCode::BAD_REQUEST,
                "missing required param: method",
            ));
        }
    };

    let params = match args.get("params") {
        None | Some(serde_json::Value::Null) => serde_json::json!({}),
        Some(value) if value.is_object() => value.clone(),
        Some(_) => {
            return Ok(bridge_error_response(
                id,
                StatusCode::BAD_REQUEST,
                "params must be an object when provided",
            ));
        }
    };

    match exec_bridge_method(state, method, &params) {
        Ok(result) => Ok(bridge_ok_response(id, result)),
        Err((status, message)) => Ok(bridge_error_response(id, status, message)),
    }
}

fn exec_bridge_batch(
    state: &ServerState,
    args: &serde_json::Value,
) -> Result<serde_json::Value, (StatusCode, String)> {
    let calls = args["calls"].as_array().ok_or_else(|| {
        (
            StatusCode::BAD_REQUEST,
            "missing required param: calls (array)".to_string(),
        )
    })?;
    if calls.is_empty() {
        return Err((
            StatusCode::BAD_REQUEST,
            "calls must include at least one item".to_string(),
        ));
    }

    let stop_on_error = args["stop_on_error"].as_bool().unwrap_or(true);
    let max_retries = parse_bridge_max_retries(args);
    let retry_backoff_ms = parse_bridge_retry_backoff_ms(args);
    let retry_codes = parse_bridge_retry_codes(args);
    let mut retry_codes_sorted: Vec<String> = retry_codes.iter().cloned().collect();
    retry_codes_sorted.sort();
    let mut responses = Vec::with_capacity(calls.len());
    let mut success_count = 0usize;
    let mut failure_count = 0usize;
    let mut retry_count = 0usize;
    let mut retried_calls = 0usize;

    for (index, call) in calls.iter().enumerate() {
        let id = call["id"]
            .as_str()
            .map(str::to_string)
            .unwrap_or_else(|| format!("batch-{}", index + 1));
        let method = call["method"].as_str().map(str::trim).unwrap_or_default();
        let params = match call.get("params") {
            None | Some(serde_json::Value::Null) => serde_json::json!({}),
            Some(value) if value.is_object() => value.clone(),
            Some(_) => {
                failure_count += 1;
                responses.push(annotate_bridge_attempts(
                    bridge_error_response(
                        Some(&id),
                        StatusCode::BAD_REQUEST,
                        "params must be an object when provided",
                    ),
                    1,
                    0,
                ));
                if stop_on_error {
                    break;
                }
                continue;
            }
        };

        if method.is_empty() {
            failure_count += 1;
            responses.push(annotate_bridge_attempts(
                bridge_error_response(
                    Some(&id),
                    StatusCode::BAD_REQUEST,
                    "missing required field calls[].method",
                ),
                1,
                0,
            ));
            if stop_on_error {
                break;
            }
            continue;
        }

        let mut attempt = 0usize;
        let mut call_retries = 0usize;
        loop {
            attempt += 1;
            match exec_bridge_method(state, method, &params) {
                Ok(result) => {
                    success_count += 1;
                    if call_retries > 0 {
                        retried_calls += 1;
                        retry_count += call_retries;
                    }
                    responses.push(annotate_bridge_attempts(
                        bridge_ok_response(Some(&id), result),
                        attempt,
                        call_retries,
                    ));
                    break;
                }
                Err((status, message)) => {
                    let code = bridge_error_code(status).to_string();
                    let can_retry = call_retries < max_retries
                        && should_retry_bridge_error(status, &code, &retry_codes);
                    if can_retry {
                        call_retries += 1;
                        let sleep_ms = retry_backoff_ms
                            .saturating_mul(2u64.saturating_pow(call_retries as u32 - 1));
                        if sleep_ms > 0 {
                            std::thread::sleep(Duration::from_millis(sleep_ms));
                        }
                        continue;
                    }

                    failure_count += 1;
                    if call_retries > 0 {
                        retried_calls += 1;
                        retry_count += call_retries;
                    }
                    responses.push(annotate_bridge_attempts(
                        bridge_error_response(Some(&id), status, message),
                        attempt,
                        call_retries,
                    ));
                    if stop_on_error {
                        break;
                    }
                    break;
                }
            }
        }

        if stop_on_error
            && let Some(last) = responses.last()
            && last["ok"].as_bool() == Some(false)
        {
            break;
        }
    }

    Ok(serde_json::json!({
        "v": BRIDGE_PROTOCOL_VERSION,
        "kind": "batch_response",
        "ok": failure_count == 0,
        "summary": {
            "total": calls.len(),
            "completed": responses.len(),
            "succeeded": success_count,
            "failed": failure_count,
            "stop_on_error": stop_on_error,
            "max_retries": max_retries,
            "retry_backoff_ms": retry_backoff_ms,
            "retry_on_codes": retry_codes_sorted,
            "retried_calls": retried_calls,
            "retry_attempts": retry_count,
        },
        "responses": responses,
    }))
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
        let mut lock = state.current.lock().unwrap_or_else(|e| e.into_inner());
        *lock = Arc::clone(&new_arc);
    }
    {
        let mut lock = state.history.lock().unwrap_or_else(|e| e.into_inner());
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
    let lock = state.current.lock().unwrap_or_else(|e| e.into_inner());
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
        let lock = state.history.lock().unwrap_or_else(|e| e.into_inner());
        lock.get(since_hash).cloned()
    };

    let old = old.ok_or_else(|| {
        (
            StatusCode::NOT_FOUND,
            format!("snapshot {} not found in history", since_hash),
        )
    })?;

    let current = {
        let lock = state.current.lock().unwrap_or_else(|e| e.into_inner());
        Arc::clone(&lock)
    };

    let diff = diff_snapshots(&old, &current);
    serde_json::to_value(&diff).map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))
}

fn exec_sources(state: &ServerState) -> Result<serde_json::Value, (StatusCode, String)> {
    let lock = state.current.lock().unwrap_or_else(|e| e.into_inner());

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
    let lock = state.current.lock().unwrap_or_else(|e| e.into_inner());

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

        if let Some(glob) = glob_filter
            && !matches_glob(file_name, glob)
        {
            continue;
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
    let lock = state.current.lock().unwrap_or_else(|e| e.into_inner());

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
        let lock = state.history.lock().unwrap_or_else(|e| e.into_inner());

        if let Some(hash) = since_hash {
            lock.get(hash).cloned()
        } else {
            // Use the history_order to get the earliest snapshot.
            let order = state
                .history_order
                .lock()
                .unwrap_or_else(|e| e.into_inner());
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
        let lock = state.current.lock().unwrap_or_else(|e| e.into_inner());
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

    let lock = state.current.lock().unwrap_or_else(|e| e.into_inner());

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
        let lock = state.current.lock().unwrap_or_else(|e| e.into_inner());
        lock.fingerprint.clone()
    };

    let history_count = {
        let lock = state.history.lock().unwrap_or_else(|e| e.into_inner());
        lock.len()
    };

    let sequence = {
        let lock = state.sequence.lock().unwrap_or_else(|e| e.into_inner());
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
    let detail = args["detail"].as_bool().unwrap_or(false);

    // History order gives us the chronological sequence of snapshot hashes.
    let order = state
        .history_order
        .lock()
        .unwrap_or_else(|e| e.into_inner());

    let history = state.history.lock().unwrap_or_else(|e| e.into_inner());

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
            let mut event = serde_json::json!({
                "sequence": start + i,
                "from_hash": diff.previous_fingerprint,
                "to_hash": diff.current_fingerprint,
                "added": diff.added.len(),
                "modified": diff.modified.len(),
                "deleted": diff.deleted.len(),
            });
            if detail {
                event["added_paths"] =
                    serde_json::json!(diff.added.iter().map(|e| &e.path).collect::<Vec<_>>());
                event["modified_paths"] =
                    serde_json::json!(diff.modified.iter().map(|e| &e.path).collect::<Vec<_>>());
                event["deleted_paths"] =
                    serde_json::json!(diff.deleted.iter().map(|e| &e.path).collect::<Vec<_>>());
            }
            events.push(event);
        }
    }

    Ok(serde_json::json!({
        "total_snapshots": total,
        "showing": events.len(),
        "detail": detail,
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
        if let Some(glob) = glob_filter
            && !matches_glob(file_name, glob)
        {
            continue;
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
        "vsync_bridge_manifest" => exec_bridge_manifest(state).map_err(|(_, e)| e),
        "vsync_bridge_execute" => exec_bridge_execute(state, &args).map_err(|(_, e)| e),
        "vsync_bridge_batch" => exec_bridge_batch(state, &args).map_err(|(_, e)| e),
        "sync_history" => exec_sync_history(state, &args).map_err(|(_, e)| e),
        "sync_rewind" => exec_sync_rewind(state, &args).map_err(|(_, e)| e),
        "sync_model_manifest" => exec_sync_model_manifest(state, &args).map_err(|(_, e)| e),
        "sync_config" => exec_sync_config(state).map_err(|(_, e)| e),
        "sync_plugin_state" => exec_sync_plugin_state(state).map_err(|(_, e)| e),
        "sync_plugin_managed" => exec_sync_plugin_managed(state).map_err(|(_, e)| e),
        // Note: sync_plugin_command is async and not supported in synchronous pipeline steps.
        "sync_watch_status" => exec_sync_watch_status(state).map_err(|(_, e)| e),
        "sync_file_history" => exec_sync_file_history(state, &args).map_err(|(_, e)| e),
        "sync_scaffold_builder" => exec_scaffold_builder(state, &args).map_err(|(_, e)| e),
        "sync_convert_to_builder" => exec_convert_to_builder(state, &args).map_err(|(_, e)| e),
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
                        .map(|v| match &v {
                            serde_json::Value::String(s) => s.clone(),
                            other => other.to_string(),
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
            let ns = namespace.lock().unwrap_or_else(|e| e.into_inner()).clone();

            match execute_step(state, step, &ns) {
                Ok(value) => {
                    completed += 1;
                    let mut ns = namespace.lock().unwrap_or_else(|e| e.into_inner());
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
                let ns_clone = namespace.lock().unwrap_or_else(|e| e.into_inner()).clone();
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
                        let mut ns = namespace.lock().unwrap_or_else(|e| e.into_inner());
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

// ---------------------------------------------------------------------------
// Builder codegen tool executors
// ---------------------------------------------------------------------------

use crate::builder_codegen;

fn exec_scaffold_builder(
    state: &ServerState,
    args: &serde_json::Value,
) -> Result<serde_json::Value, (StatusCode, String)> {
    let name = args["name"].as_str().ok_or_else(|| {
        (
            StatusCode::BAD_REQUEST,
            "missing required param: name".into(),
        )
    })?;
    let zone = args["zone"].as_str().ok_or_else(|| {
        (
            StatusCode::BAD_REQUEST,
            "missing required param: zone".into(),
        )
    })?;
    let y_range = args["y_range"].as_str();
    let description = args["description"].as_str();

    // Generate the builder code.
    let code = builder_codegen::scaffold_builder(name, zone, y_range, description)
        .map_err(|e| (StatusCode::BAD_REQUEST, e))?;

    // Write via the safe_write path (validate + write + rebuild snapshot).
    let rel_path = format!("src/Server/World/Builders/{name}.luau");

    // Check that file doesn't already exist.
    let source_root = canon_root(state)?;
    let target = validate_path(&source_root, &rel_path)?;
    if target.exists() {
        return Err((
            StatusCode::CONFLICT,
            format!("builder already exists: {rel_path}"),
        ));
    }

    // Delegate to safe_write for validation + write + snapshot rebuild.
    let safe_write_args = serde_json::json!({
        "path": rel_path,
        "content": code,
        "require_strict": true,
    });
    let write_result = exec_safe_write(state, &safe_write_args)?;

    // Augment the result with builder-specific metadata.
    let mut result = write_result;
    if let Some(obj) = result.as_object_mut() {
        obj.insert("builder_name".into(), serde_json::json!(name));
        obj.insert("zone".into(), serde_json::json!(zone));
        if let Some(yr) = y_range {
            obj.insert("y_range".into(), serde_json::json!(yr));
        }
    }

    Ok(result)
}

fn exec_convert_to_builder(
    state: &ServerState,
    args: &serde_json::Value,
) -> Result<serde_json::Value, (StatusCode, String)> {
    let input_path = args["input_path"].as_str().ok_or_else(|| {
        (
            StatusCode::BAD_REQUEST,
            "missing required param: input_path".into(),
        )
    })?;

    // Resolve the input path relative to the project root.
    let source_root = canon_root(state)?;
    let input_resolved = if Path::new(input_path).is_absolute() {
        PathBuf::from(input_path)
    } else {
        source_root.join(input_path)
    };

    if !input_resolved.exists() {
        return Err((
            StatusCode::NOT_FOUND,
            format!("input file not found: {input_path}"),
        ));
    }

    // Read and parse the model.json.
    let content = std::fs::read_to_string(&input_resolved).map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("read error: {e}"),
        )
    })?;
    let model: serde_json::Value = serde_json::from_str(&content).map_err(|e| {
        (
            StatusCode::BAD_REQUEST,
            format!("invalid JSON in {input_path}: {e}"),
        )
    })?;

    // Derive builder name from filename or explicit param.
    let builder_name = if let Some(explicit) = args["builder_name"].as_str() {
        explicit.to_string()
    } else {
        // Derive from filename: "CoralCave.model.json" -> "CoralCaveBuilder"
        let stem = Path::new(input_path)
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("Generated");
        // Strip ".model" suffix if present.
        let base = stem.strip_suffix(".model").unwrap_or(stem);
        let sanitized = builder_codegen::sanitize_var_name(base);
        if sanitized.ends_with("Builder") {
            sanitized
        } else {
            format!("{sanitized}Builder")
        }
    };

    // Generate the builder Luau code.
    let luau_code = builder_codegen::generate_builder_luau(&model, &builder_name)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e))?;

    // Determine output path.
    let output_path = if let Some(explicit) = args["output_path"].as_str() {
        explicit.to_string()
    } else {
        // Default: same directory as input, with .luau extension.
        format!("src/Server/World/Builders/{builder_name}.luau")
    };

    // Write via safe_write path.
    let safe_write_args = serde_json::json!({
        "path": output_path,
        "content": luau_code,
        "require_strict": true,
    });
    let write_result = exec_safe_write(state, &safe_write_args)?;

    // Augment result.
    let mut result = write_result;
    if let Some(obj) = result.as_object_mut() {
        obj.insert("builder_name".into(), serde_json::json!(builder_name));
        obj.insert("input_path".into(), serde_json::json!(input_path));
        obj.insert("output_path".into(), serde_json::json!(output_path));
    }

    Ok(result)
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
                    meta: None,
                    file_type: None,
                },
                crate::SnapshotEntry {
                    path: "src/Client/init.client.luau".into(),
                    sha256: "bbb".into(),
                    bytes: 2000,
                    meta: None,
                    file_type: None,
                },
                crate::SnapshotEntry {
                    path: "src/Shared/Config/Abilities.luau".into(),
                    sha256: "ccc".into(),
                    bytes: 500,
                    meta: None,
                    file_type: None,
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
        // Ensure we don't accidentally drop tools. 13 existing + 14 new + 1 pipeline + 3 bridge + 5 rbxl = 36.
        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let tools = rt.block_on(async { handle_mcp_tools().await });
        assert_eq!(
            tools.0.len(),
            47,
            "expected 47 MCP tools (45 existing + 2 new: scaffold_builder, convert_to_builder)"
        );
    }

    #[test]
    fn bridge_manifest_exposes_protocol_and_methods() {
        let methods = bridge_capabilities_json();
        assert!(
            methods
                .iter()
                .any(|method| method["name"] == "bridge.hello")
        );
        assert!(
            methods
                .iter()
                .any(|method| method["name"] == "source.safe_write")
        );
        assert!(
            methods
                .iter()
                .any(|method| method["name"] == "source.validate_content")
        );
    }

    #[test]
    fn bridge_error_code_mapping_is_stable() {
        assert_eq!(bridge_error_code(StatusCode::BAD_REQUEST), "BAD_PARAMS");
        assert_eq!(bridge_error_code(StatusCode::NOT_FOUND), "NOT_FOUND");
        assert_eq!(
            bridge_error_code(StatusCode::SERVICE_UNAVAILABLE),
            "TRANSPORT_UNAVAILABLE"
        );
    }

    #[test]
    fn bridge_execute_rejects_non_object_params() {
        let snapshot = crate::Snapshot {
            version: 1,
            include: vec![],
            fingerprint: "abc123".into(),
            entries: vec![],
        };
        let state = crate::ServerState::new(std::env::temp_dir(), vec![], snapshot, 32);
        let result = exec_bridge_execute(
            &state,
            &serde_json::json!({
                "id": "req-1",
                "method": "bridge.hello",
                "params": ["invalid"]
            }),
        )
        .expect("bridge execute should return structured envelope");
        assert_eq!(result["ok"], false);
        assert_eq!(result["error"]["code"], "BAD_PARAMS");
    }

    #[test]
    fn bridge_batch_stops_after_first_error() {
        let snapshot = crate::Snapshot {
            version: 1,
            include: vec![],
            fingerprint: "abc123".into(),
            entries: vec![],
        };
        let state = crate::ServerState::new(std::env::temp_dir(), vec![], snapshot, 32);
        let result = exec_bridge_batch(
            &state,
            &serde_json::json!({
                "stop_on_error": true,
                "calls": [
                    {"id": "a", "method": "bridge.hello"},
                    {"id": "b", "method": "does.not.exist"},
                    {"id": "c", "method": "bridge.capabilities"}
                ]
            }),
        )
        .expect("bridge batch should return summary envelope");
        assert_eq!(result["ok"], false);
        assert_eq!(result["summary"]["completed"], 2);
    }

    #[test]
    fn bridge_batch_retries_when_code_allowlisted() {
        let snapshot = crate::Snapshot {
            version: 1,
            include: vec![],
            fingerprint: "abc123".into(),
            entries: vec![],
        };
        let state = crate::ServerState::new(std::env::temp_dir(), vec![], snapshot, 32);
        let result = exec_bridge_batch(
            &state,
            &serde_json::json!({
                "stop_on_error": false,
                "max_retries": 2,
                "retry_backoff_ms": 0,
                "retry_on_codes": ["NOT_FOUND"],
                "calls": [
                    {"id": "a", "method": "does.not.exist"}
                ]
            }),
        )
        .expect("bridge batch should return summary envelope");
        assert_eq!(result["ok"], false);
        assert_eq!(result["summary"]["retry_attempts"], 2);
        assert_eq!(result["summary"]["retried_calls"], 1);
        assert_eq!(result["responses"][0]["attempts"], 3);
        assert_eq!(result["responses"][0]["retries"], 2);
    }

    #[test]
    fn bridge_execute_supports_source_alias_methods() {
        let snapshot = crate::Snapshot {
            version: 1,
            include: vec![],
            fingerprint: "abc123".into(),
            entries: vec![],
        };
        let state = crate::ServerState::new(std::env::temp_dir(), vec![], snapshot, 32);
        let result = exec_bridge_execute(
            &state,
            &serde_json::json!({
                "id": "req-alias",
                "method": "source.validate_content",
                "params": {
                    "path": "src/Server/Test.luau",
                    "content": "--!strict\nreturn {}\n"
                }
            }),
        )
        .expect("bridge execute should return structured envelope");
        assert_eq!(result["ok"], true);
        assert!(result["result"]["issues"].is_array());
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

    // ── Plugin command channel tests ─────────────────────────────

    fn make_test_state() -> Arc<crate::ServerState> {
        let snapshot = crate::Snapshot {
            version: 1,
            include: vec![],
            fingerprint: "test000".into(),
            entries: vec![],
        };
        crate::ServerState::new(std::env::temp_dir(), vec![], snapshot, 32)
    }

    #[test]
    fn plugin_command_enqueue_and_drain() {
        let state = make_test_state();
        let cmd = crate::PluginCommand {
            id: "cmd-1".into(),
            command: "toggle_sync".into(),
            params: serde_json::Value::Null,
            created_at_epoch: 0.0,
            created_at: Some(std::time::Instant::now()),
        };
        {
            let mut queue = state.plugin_commands.lock().unwrap();
            queue.push_back(cmd);
        }
        let drained = state.drain_plugin_commands();
        assert_eq!(drained.len(), 1);
        assert_eq!(drained[0].id, "cmd-1");
        // Queue should be empty after drain.
        let drained2 = state.drain_plugin_commands();
        assert!(drained2.is_empty());
    }

    #[test]
    fn plugin_command_gc_expires_old() {
        let state = make_test_state();
        // Insert a command with a created_at far in the past.
        let old_cmd = crate::PluginCommand {
            id: "old-cmd".into(),
            command: "force_resync".into(),
            params: serde_json::Value::Null,
            created_at_epoch: 0.0,
            created_at: Some(std::time::Instant::now() - std::time::Duration::from_secs(120)),
        };
        let fresh_cmd = crate::PluginCommand {
            id: "fresh-cmd".into(),
            command: "toggle_sync".into(),
            params: serde_json::Value::Null,
            created_at_epoch: 0.0,
            created_at: Some(std::time::Instant::now()),
        };
        {
            let mut queue = state.plugin_commands.lock().unwrap();
            queue.push_back(old_cmd);
            queue.push_back(fresh_cmd);
        }
        let drained = state.drain_plugin_commands();
        assert_eq!(drained.len(), 1, "old command should be GC'd");
        assert_eq!(drained[0].id, "fresh-cmd");
    }

    #[test]
    fn plugin_command_ack_stored() {
        let state = make_test_state();
        let ack = crate::PluginCommandAck {
            command_id: "cmd-42".into(),
            success: true,
            message: "ok".into(),
        };
        {
            let mut acks = state.plugin_command_acks.lock().unwrap();
            acks.insert(ack.command_id.clone(), ack);
        }
        let acks = state.plugin_command_acks.lock().unwrap();
        assert!(acks.contains_key("cmd-42"));
        assert_eq!(acks["cmd-42"].success, true);
    }

    #[test]
    fn plugin_command_queue_overflow_rejected() {
        let state = make_test_state();
        {
            let mut queue = state.plugin_commands.lock().unwrap();
            for i in 0..crate::PLUGIN_COMMAND_QUEUE_CAPACITY {
                queue.push_back(crate::PluginCommand {
                    id: format!("cmd-{i}"),
                    command: "toggle_sync".into(),
                    params: serde_json::Value::Null,
                    created_at_epoch: 0.0,
                    created_at: Some(std::time::Instant::now()),
                });
            }
            assert_eq!(queue.len(), crate::PLUGIN_COMMAND_QUEUE_CAPACITY);
        }
        // Verify we can detect overflow condition (actual rejection is in the async handler).
        let queue = state.plugin_commands.lock().unwrap();
        assert!(queue.len() >= crate::PLUGIN_COMMAND_QUEUE_CAPACITY);
    }

    #[test]
    fn watch_status_returns_without_coalescer() {
        let state = make_test_state();
        let result = exec_sync_watch_status(&state).unwrap();
        assert!(result["watcher_type"].is_string());
        assert_eq!(result["pending_rebuild"], false);
        assert!(result["note"].is_string()); // "coalescer not attached"
    }

    #[test]
    fn watch_status_returns_with_coalescer() {
        let state = make_test_state();
        let coalescer = Arc::new(crate::EventCoalescer::new(
            std::time::Duration::from_millis(50),
        ));
        {
            let mut lock = state.coalescer.lock().unwrap();
            *lock = Some(Arc::clone(&coalescer));
        }
        let result = exec_sync_watch_status(&state).unwrap();
        assert_eq!(result["coalesce_window_ms"], 50);
        assert_eq!(result["pending_rebuild"], false);
        assert!(result.get("note").is_none());
    }

    #[test]
    fn file_history_empty_when_no_changes() {
        let state = make_test_state();
        let result = exec_sync_file_history(
            &state,
            &serde_json::json!({"path": "src/Server/init.server.luau"}),
        )
        .unwrap();
        assert_eq!(result["history_events"], 0);
        assert_eq!(result["current"]["exists"], false);
    }

    #[test]
    fn file_history_across_snapshot_transitions() {
        // Create state with initial snapshot containing one file.
        let entry = crate::SnapshotEntry {
            path: "src/test.luau".into(),
            sha256: "aaa".into(),
            bytes: 100,
            meta: None,
            file_type: None,
        };
        let snap1 = crate::Snapshot {
            version: 1,
            include: vec!["src".into()],
            fingerprint: "snap1".into(),
            entries: vec![entry.clone()],
        };
        let state = crate::ServerState::new(std::env::temp_dir(), vec!["src".into()], snap1, 32);

        // Simulate a second snapshot with modified file.
        let mut entry2 = entry.clone();
        entry2.sha256 = "bbb".into();
        entry2.bytes = 200;
        let snap2 = Arc::new(crate::Snapshot {
            version: 1,
            include: vec!["src".into()],
            fingerprint: "snap2".into(),
            entries: vec![entry2],
        });
        {
            let mut hist = state.history.lock().unwrap();
            hist.insert("snap2".into(), Arc::clone(&snap2));
            let mut order = state.history_order.lock().unwrap();
            order.push_back("snap2".into());
            *state.current.lock().unwrap() = snap2;
        }

        let result =
            exec_sync_file_history(&state, &serde_json::json!({"path": "src/test.luau"})).unwrap();
        assert_eq!(result["history_events"], 1);
        assert_eq!(result["events"][0]["action"], "modified");
        assert_eq!(result["events"][0]["previous_sha256"], "aaa");
        assert_eq!(result["events"][0]["current_sha256"], "bbb");
        assert_eq!(result["current"]["exists"], true);
    }

    #[test]
    fn events_detail_mode_includes_paths() {
        // Build state with two snapshots to create an event.
        let entry = crate::SnapshotEntry {
            path: "src/test.luau".into(),
            sha256: "aaa".into(),
            bytes: 100,
            meta: None,
            file_type: None,
        };
        let snap1 = crate::Snapshot {
            version: 1,
            include: vec!["src".into()],
            fingerprint: "snap-d1".into(),
            entries: vec![],
        };
        let state = crate::ServerState::new(std::env::temp_dir(), vec!["src".into()], snap1, 32);
        let snap2 = Arc::new(crate::Snapshot {
            version: 1,
            include: vec!["src".into()],
            fingerprint: "snap-d2".into(),
            entries: vec![entry],
        });
        {
            let mut hist = state.history.lock().unwrap();
            hist.insert("snap-d2".into(), Arc::clone(&snap2));
            let mut order = state.history_order.lock().unwrap();
            order.push_back("snap-d2".into());
            *state.current.lock().unwrap() = snap2;
        }

        // Without detail.
        let result = exec_events(&state, &serde_json::json!({"limit": 10})).unwrap();
        assert_eq!(result["detail"], false);
        let first_event = &result["events"][0];
        assert!(first_event.get("added_paths").is_none());

        // With detail.
        let result =
            exec_events(&state, &serde_json::json!({"limit": 10, "detail": true})).unwrap();
        assert_eq!(result["detail"], true);
        let first_event = &result["events"][0];
        assert!(first_event["added_paths"].is_array());
        assert_eq!(first_event["added_paths"][0], "src/test.luau");
    }
}

// ---------------------------------------------------------------------------
// RBXL MCP tool executors
// ---------------------------------------------------------------------------

fn exec_rbxl_load(
    state: &Arc<ServerState>,
    args: &serde_json::Value,
) -> Result<serde_json::Value, (StatusCode, String)> {
    let raw_path = args["path"].as_str().ok_or_else(|| {
        (
            StatusCode::BAD_REQUEST,
            "missing 'path' argument".to_string(),
        )
    })?;

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

    let mut lock = state.rbxl.lock().unwrap_or_else(|e| e.into_inner());
    lock.dom = Some(dom);
    lock.ref_map = ref_map;
    lock.loaded_path = Some(resolved.clone());

    Ok(serde_json::json!({
        "status": "loaded",
        "path": resolved.display().to_string(),
        "instance_count": instance_count,
    }))
}

fn exec_rbxl_tree(state: &Arc<ServerState>) -> Result<serde_json::Value, (StatusCode, String)> {
    let lock = state.rbxl.lock().unwrap_or_else(|e| e.into_inner());
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
    let lock = state.rbxl.lock().unwrap_or_else(|e| e.into_inner());
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

fn exec_rbxl_scripts(state: &Arc<ServerState>) -> Result<serde_json::Value, (StatusCode, String)> {
    let lock = state.rbxl.lock().unwrap_or_else(|e| e.into_inner());
    let dom = lock.dom.as_ref().ok_or_else(|| {
        (
            StatusCode::PRECONDITION_REQUIRED,
            "no .rbxl file loaded".to_string(),
        )
    })?;

    let scripts = RbxlLoader::extract_scripts(dom);
    serde_json::to_value(&scripts).map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))
}

fn exec_rbxl_meshes(state: &Arc<ServerState>) -> Result<serde_json::Value, (StatusCode, String)> {
    let lock = state.rbxl.lock().unwrap_or_else(|e| e.into_inner());
    let dom = lock.dom.as_ref().ok_or_else(|| {
        (
            StatusCode::PRECONDITION_REQUIRED,
            "no .rbxl file loaded".to_string(),
        )
    })?;

    let meshes = RbxlLoader::extract_meshes(dom);
    serde_json::to_value(&meshes).map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))
}

// ---------------------------------------------------------------------------
// New MCP tool implementations: history, rewind, model, config
// ---------------------------------------------------------------------------

fn exec_sync_history(
    state: &ServerState,
    args: &serde_json::Value,
) -> Result<serde_json::Value, (StatusCode, String)> {
    let limit = args.get("limit").and_then(|v| v.as_u64()).unwrap_or(50) as usize;
    let limit = limit.min(500);

    let event_log_path = state.root.join(".vertigo-sync-state").join("events.jsonl");
    let entries = crate::read_history(&event_log_path, limit)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    serde_json::to_value(&entries).map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))
}

fn exec_sync_rewind(
    state: &ServerState,
    args: &serde_json::Value,
) -> Result<serde_json::Value, (StatusCode, String)> {
    let fingerprint = args
        .get("fingerprint")
        .and_then(|v| v.as_str())
        .ok_or_else(|| {
            (
                StatusCode::BAD_REQUEST,
                "missing required parameter: fingerprint".to_string(),
            )
        })?;

    let target = {
        let lock = state.history.lock().unwrap_or_else(|e| e.into_inner());
        lock.get(fingerprint).cloned()
    };

    let target = target.ok_or_else(|| {
        (
            StatusCode::NOT_FOUND,
            format!(
                "fingerprint {} not found in history ring buffer",
                fingerprint
            ),
        )
    })?;

    let current = {
        let lock = state.current.lock().unwrap_or_else(|e| e.into_inner());
        std::sync::Arc::clone(&lock)
    };

    let forward = crate::diff_snapshots(&target, &current);
    let reversed = crate::reverse_diff(&forward);

    serde_json::to_value(&reversed).map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))
}

fn exec_sync_model_manifest(
    state: &ServerState,
    args: &serde_json::Value,
) -> Result<serde_json::Value, (StatusCode, String)> {
    let path = args.get("path").and_then(|v| v.as_str()).ok_or_else(|| {
        (
            StatusCode::BAD_REQUEST,
            "missing required parameter: path".to_string(),
        )
    })?;

    if !path.ends_with(".rbxm") && !path.ends_with(".rbxmx") {
        return Err((
            StatusCode::BAD_REQUEST,
            format!("not a binary model file: {path}"),
        ));
    }

    // Path traversal guard: resolve and verify the path stays inside canonical_root.
    let abs_path = state.canonical_root.join(path);
    let canonical = abs_path.canonicalize().map_err(|_| {
        (
            StatusCode::NOT_FOUND,
            format!("model path not found: {path}"),
        )
    })?;
    if !canonical.starts_with(&state.canonical_root) {
        return Err((
            StatusCode::BAD_REQUEST,
            "path traversal detected".to_string(),
        ));
    }

    // Look up content hash from snapshot for cache key.
    let content_hash = {
        let lock = state.current.lock().unwrap_or_else(|e| e.into_inner());
        lock.entries
            .iter()
            .find(|e| e.path == path)
            .map(|e| e.sha256.clone())
    };

    let mut cache_lock = state.model_cache.lock().unwrap_or_else(|e| e.into_inner());

    let manifest = if let Some(hash) = content_hash {
        cache_lock.get_or_load(&hash, &canonical)
    } else {
        // File not in snapshot — deserialize directly without caching.
        drop(cache_lock);
        let m = crate::deserialize_model_manifest(&canonical)
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
        return serde_json::to_value(&m)
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()));
    };

    let manifest = manifest.map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    serde_json::to_value(manifest).map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))
}

fn exec_sync_config(state: &ServerState) -> Result<serde_json::Value, (StatusCode, String)> {
    let history_size = {
        let lock = state.history.lock().unwrap_or_else(|e| e.into_inner());
        lock.len()
    };
    Ok(serde_json::json!({
        "binary_models": state.binary_models,
        "model_pool_size": 128,
        "history_buffer_size": history_size,
        "history_buffer_capacity": 256,
        "turbo": state.turbo,
        "coalesce_ms": state.coalesce_ms,
    }))
}

fn exec_sync_plugin_state(state: &ServerState) -> Result<serde_json::Value, (StatusCode, String)> {
    let data = state
        .plugin_state
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone();
    let at = *state
        .plugin_state_at
        .lock()
        .unwrap_or_else(|e| e.into_inner());

    match (data, at) {
        (Some(mut value), Some(instant)) => {
            let age_secs = instant.elapsed().as_secs_f64();
            let stale = age_secs > 10.0;
            if let Some(obj) = value.as_object_mut() {
                obj.insert("_stale".to_string(), serde_json::json!(stale));
                obj.insert(
                    "_age_seconds".to_string(),
                    serde_json::json!(age_secs.round() as u64),
                );
            }
            Ok(value)
        }
        _ => Err((
            StatusCode::NOT_FOUND,
            "no plugin state reported yet".to_string(),
        )),
    }
}

fn exec_sync_plugin_managed(
    state: &ServerState,
) -> Result<serde_json::Value, (StatusCode, String)> {
    let data = state
        .plugin_managed
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone();
    let at = *state
        .plugin_managed_at
        .lock()
        .unwrap_or_else(|e| e.into_inner());

    match (data, at) {
        (Some(mut value), Some(instant)) => {
            let age_secs = instant.elapsed().as_secs_f64();
            let stale = age_secs > 60.0;
            if let Some(obj) = value.as_object_mut() {
                obj.insert("_stale".to_string(), serde_json::json!(stale));
                obj.insert(
                    "_age_seconds".to_string(),
                    serde_json::json!(age_secs.round() as u64),
                );
            }
            Ok(value)
        }
        _ => Err((
            StatusCode::NOT_FOUND,
            "no plugin managed index reported yet".to_string(),
        )),
    }
}

// ---------------------------------------------------------------------------
// Plugin command channel
// ---------------------------------------------------------------------------

async fn exec_sync_plugin_command(
    state: &Arc<ServerState>,
    args: &serde_json::Value,
) -> Result<serde_json::Value, (StatusCode, String)> {
    use crate::{PLUGIN_COMMAND_QUEUE_CAPACITY, PluginCommand};

    let command = args["command"]
        .as_str()
        .ok_or_else(|| {
            (
                StatusCode::BAD_REQUEST,
                "missing required param: command".to_string(),
            )
        })?
        .to_string();

    // Validate command name.
    let valid_commands = [
        "toggle_sync",
        "force_resync",
        "set_frame_budget",
        "run_builders",
        "set_log_level",
    ];
    if !valid_commands.contains(&command.as_str()) {
        return Err((
            StatusCode::BAD_REQUEST,
            format!(
                "invalid command '{}'. Valid: {}",
                command,
                valid_commands.join(", ")
            ),
        ));
    }

    let params = args
        .get("params")
        .cloned()
        .unwrap_or(serde_json::Value::Null);
    let wait = args["wait"].as_bool().unwrap_or(false);

    // GC expired commands first.
    state.gc_plugin_commands();

    let cmd_id = uuid::Uuid::new_v4().to_string();
    let now = Instant::now();
    let epoch = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs_f64();

    let cmd = PluginCommand {
        id: cmd_id.clone(),
        command: command.clone(),
        params,
        created_at_epoch: epoch,
        created_at: Some(now),
    };

    // Enqueue — reject if full.
    {
        let mut queue = state
            .plugin_commands
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if queue.len() >= PLUGIN_COMMAND_QUEUE_CAPACITY {
            return Err((
                StatusCode::SERVICE_UNAVAILABLE,
                format!(
                    "plugin command queue full ({} pending). Try again later.",
                    PLUGIN_COMMAND_QUEUE_CAPACITY
                ),
            ));
        }
        queue.push_back(cmd);
    }

    if !wait {
        return Ok(serde_json::json!({
            "command_id": cmd_id,
            "command": command,
            "queued": true,
            "wait": false,
        }));
    }

    // Poll for ack with 200ms intervals, max 10s.
    let deadline = now + Duration::from_secs(10);
    loop {
        tokio::time::sleep(Duration::from_millis(200)).await;

        let ack = {
            let acks = state
                .plugin_command_acks
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            acks.get(&cmd_id).cloned()
        };

        if let Some(ack) = ack {
            return Ok(serde_json::json!({
                "command_id": cmd_id,
                "command": command,
                "acknowledged": true,
                "success": ack.success,
                "message": ack.message,
            }));
        }

        if Instant::now() >= deadline {
            return Ok(serde_json::json!({
                "command_id": cmd_id,
                "command": command,
                "acknowledged": false,
                "timeout": true,
                "message": "plugin did not acknowledge within 10s",
            }));
        }
    }
}

// ---------------------------------------------------------------------------
// Filesystem watcher health
// ---------------------------------------------------------------------------

fn exec_sync_watch_status(state: &ServerState) -> Result<serde_json::Value, (StatusCode, String)> {
    let coalescer_info = {
        let lock = state.coalescer.lock().unwrap_or_else(|e| e.into_inner());
        lock.as_ref().map(|c| c.status())
    };

    let watcher_type = if cfg!(target_os = "macos") {
        "FSEvents"
    } else {
        "polling"
    };

    match coalescer_info {
        Some(status) => {
            let last_event_ms = status.last_event_elapsed.map(|d| d.as_millis() as u64);
            Ok(serde_json::json!({
                "watcher_type": watcher_type,
                "coalesce_window_ms": status.window.as_millis() as u64,
                "pending_rebuild": status.pending,
                "last_event_elapsed_ms": last_event_ms,
                "turbo": state.turbo,
            }))
        }
        None => Ok(serde_json::json!({
            "watcher_type": watcher_type,
            "coalesce_window_ms": state.coalesce_ms,
            "pending_rebuild": false,
            "last_event_elapsed_ms": null,
            "turbo": state.turbo,
            "note": "coalescer not attached (serve mode may not be active)",
        })),
    }
}

// ---------------------------------------------------------------------------
// File change history
// ---------------------------------------------------------------------------

fn exec_sync_file_history(
    state: &ServerState,
    args: &serde_json::Value,
) -> Result<serde_json::Value, (StatusCode, String)> {
    let path = args["path"]
        .as_str()
        .ok_or_else(|| {
            (
                StatusCode::BAD_REQUEST,
                "missing required param: path".to_string(),
            )
        })?
        .to_string();
    let limit = args["limit"].as_u64().unwrap_or(20) as usize;

    let order = state
        .history_order
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let history = state.history.lock().unwrap_or_else(|e| e.into_inner());

    // Walk consecutive snapshot pairs looking for changes to this file.
    let mut events: Vec<serde_json::Value> = Vec::new();
    let hashes: Vec<String> = order.iter().cloned().collect();

    for i in 1..hashes.len() {
        if events.len() >= limit {
            break;
        }

        let prev = history.get(&hashes[i - 1]);
        let curr = history.get(&hashes[i]);
        if let (Some(prev_snap), Some(curr_snap)) = (prev, curr) {
            let prev_entry = prev_snap.entries.iter().find(|e| e.path == path);
            let curr_entry = curr_snap.entries.iter().find(|e| e.path == path);

            match (prev_entry, curr_entry) {
                (None, Some(entry)) => {
                    events.push(serde_json::json!({
                        "sequence": i,
                        "action": "added",
                        "snapshot": curr_snap.fingerprint,
                        "sha256": entry.sha256,
                        "bytes": entry.bytes,
                    }));
                }
                (Some(prev_e), Some(curr_e)) if prev_e.sha256 != curr_e.sha256 => {
                    events.push(serde_json::json!({
                        "sequence": i,
                        "action": "modified",
                        "snapshot": curr_snap.fingerprint,
                        "previous_sha256": prev_e.sha256,
                        "current_sha256": curr_e.sha256,
                        "previous_bytes": prev_e.bytes,
                        "current_bytes": curr_e.bytes,
                    }));
                }
                (Some(entry), None) => {
                    events.push(serde_json::json!({
                        "sequence": i,
                        "action": "deleted",
                        "snapshot": curr_snap.fingerprint,
                        "sha256": entry.sha256,
                        "bytes": entry.bytes,
                    }));
                }
                _ => {
                    // No change for this file in this transition.
                }
            }
        }
    }

    // Current state of the file.
    let current_state = {
        let current = state.current.lock().unwrap_or_else(|e| e.into_inner());
        current
            .entries
            .iter()
            .find(|e| e.path == path)
            .map(|e| {
                serde_json::json!({
                    "exists": true,
                    "sha256": e.sha256,
                    "bytes": e.bytes,
                })
            })
            .unwrap_or_else(|| serde_json::json!({ "exists": false }))
    };

    Ok(serde_json::json!({
        "path": path,
        "current": current_state,
        "history_events": events.len(),
        "snapshots_searched": hashes.len().saturating_sub(1),
        "events": events,
    }))
}
