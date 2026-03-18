//! HTTP server for vertigo-sync serve mode.
//!
//! Endpoints:
//!   GET  /health             — status + version
//!   GET  /snapshot           — current snapshot JSON
//!   GET  /diff?since=<hash>  — diff from historical hash to current
//!   GET  /events             — SSE stream of SyncDiffEvent
//!   GET  /sources/content    — batched source fetch for high-rate hotload
//!   POST /sync/patch         — apply file patches, return ack
//!   GET  /validate            — run source validation, return report
//!   POST /plugin/state        — accept plugin state report
//!   GET  /plugin/state        — return latest plugin state (or 404)
//!   POST /plugin/managed      — accept plugin managed index report
//!   GET  /plugin/managed      — return latest plugin managed index (or 404)

use std::collections::{HashMap, HashSet};
use std::convert::Infallible;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, UNIX_EPOCH};

use axum::Router;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{DefaultBodyLimit, Path as AxumPath, Query, Request, State};
use axum::http::{HeaderMap, Method, StatusCode};
use axum::middleware::{self, Next};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Json, Response};
use axum::routing::{get, post};
use base64::Engine;
use serde::Deserialize;
use tokio_stream::StreamExt;
use tokio_stream::wrappers::BroadcastStream;
use tower_http::cors::{AllowOrigin, CorsLayer};

use sha2::{Digest, Sha256};

use crate::mcp::{handle_mcp_execute, handle_mcp_tools};
use crate::serve_rbxl::{new_shared_rbxl_state, rbxl_router};
use crate::{
    EventCoalescer, GlobIgnoreSet, ServerState, Snapshot, SnapshotDiff, build_snapshot,
    build_snapshot_with_ignores, diff_snapshots,
};
use std::sync::atomic::Ordering;

/// Patch request accepted by POST /sync/patch.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct SyncPatchRequest {
    pub source_hash: String,
    pub patches: Vec<PatchEntry>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct PatchEntry {
    pub path: String,
    pub action: String,
    #[serde(default)]
    pub content_base64: Option<String>,
    #[serde(default)]
    pub expected_sha256: Option<String>,
}

#[derive(Debug)]
enum PlannedPatch {
    Write {
        requested_path: String,
        normalized_path: String,
        target: PathBuf,
        bytes: Vec<u8>,
        expected_sha256: Option<String>,
    },
    Delete {
        requested_path: String,
        target: PathBuf,
        expected_sha256: Option<String>,
    },
}

#[derive(Debug)]
struct ExistingFileBackup {
    target: PathBuf,
    existed: bool,
    bytes: Vec<u8>,
}

impl PlannedPatch {
    fn requested_path(&self) -> &str {
        match self {
            Self::Write { requested_path, .. } | Self::Delete { requested_path, .. } => {
                requested_path
            }
        }
    }

    fn target(&self) -> &Path {
        match self {
            Self::Write { target, .. } | Self::Delete { target, .. } => target.as_path(),
        }
    }

    fn expected_sha256(&self) -> Option<&str> {
        match self {
            Self::Write {
                expected_sha256, ..
            }
            | Self::Delete {
                expected_sha256, ..
            } => expected_sha256.as_deref(),
        }
    }
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct SyncPatchAck {
    pub status: String,
    pub reason_code: String,
    #[serde(default)]
    pub reason_message: String,
    pub accepted: bool,
    pub new_source_hash: String,
    pub applied: usize,
    pub errors: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub validation: Vec<crate::validate::ValidationIssue>,
}

/// Maximum request body size (10 MiB). Prevents memory exhaustion from
/// oversized `/sync/patch` payloads while accommodating large Luau source files.
const MAX_BODY_BYTES: usize = 10 * 1024 * 1024;
/// Maximum number of paths accepted by one batched source request.
const MAX_BATCH_SOURCE_PATHS: usize = 256;
/// Maximum aggregate UTF-8 payload served in one batched source request.
const MAX_BATCH_SOURCE_BYTES: usize = 4 * 1024 * 1024;

/// Bearer token auth middleware for mutating endpoints.
///
/// If `VERTIGO_SYNC_API_TOKEN` is set, requires `Authorization: Bearer <token>`
/// on protected (mutating) routes. Returns 401 if missing or wrong.
async fn bearer_auth_layer(req: Request, next: Next) -> Response {
    // Token is stashed in a request extension by the outer layer closure.
    let expected = req.extensions().get::<ExpectedToken>().cloned();
    if let Some(ExpectedToken(ref token)) = expected {
        let auth_header = req
            .headers()
            .get(axum::http::header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok());
        match auth_header {
            Some(value) if value.strip_prefix("Bearer ").map_or(false, |t| t == token) => {}
            _ => {
                return (
                    StatusCode::UNAUTHORIZED,
                    Json(serde_json::json!({
                        "error": "Unauthorized",
                        "message": "VERTIGO_SYNC_API_TOKEN is set. Provide Authorization: Bearer <token> header."
                    })),
                )
                    .into_response();
            }
        }
    }
    next.run(req).await
}

/// Newtype wrapper so we can insert the expected token into request extensions.
#[derive(Clone)]
struct ExpectedToken(String);

/// Build the Axum router with all endpoints.
pub fn build_router(state: Arc<ServerState>) -> Router {
    let rbxl_state = new_shared_rbxl_state();

    // Optional bearer token auth for mutating endpoints.
    let api_token: Option<String> = std::env::var("VERTIGO_SYNC_API_TOKEN")
        .ok()
        .filter(|t| !t.is_empty());

    // CORS: allow browser clients on any localhost port (Vite dev, Strata, etc.)
    // and the deployed showcase site. Strata WASM needs GET + POST + WebSocket upgrade.
    let cors = CorsLayer::new()
        .allow_origin(AllowOrigin::predicate(|origin, _| {
            let origin = origin.as_bytes();
            // Allow localhost/127.0.0.1 origins with explicit port delimiter.
            // The `:` or end-of-string check prevents matching localhost.evil.com.
            fn is_loopback_origin(o: &[u8], prefix: &[u8]) -> bool {
                if !o.starts_with(prefix) {
                    return false;
                }
                let rest = &o[prefix.len()..];
                rest.is_empty() || rest.starts_with(b":")
            }
            is_loopback_origin(origin, b"http://localhost")
                || is_loopback_origin(origin, b"http://127.0.0.1")
                || is_loopback_origin(origin, b"https://localhost")
                || is_loopback_origin(origin, b"https://127.0.0.1")
        }))
        .allow_methods([Method::GET, Method::POST, Method::OPTIONS])
        .allow_headers([
            axum::http::header::CONTENT_TYPE,
            axum::http::header::AUTHORIZATION,
        ])
        .max_age(Duration::from_secs(86400));

    // Mutating routes that require bearer auth when VERTIGO_SYNC_API_TOKEN is set.
    let token_for_mutating = api_token.clone();
    let mutating_routes = Router::new()
        .route("/sync/patch", post(handle_patch))
        .route("/mcp/execute", post(handle_mcp_execute))
        .route("/plugin/state", post(handle_post_plugin_state))
        .route("/plugin/managed", post(handle_post_plugin_managed))
        .route("/plugin/command/ack", post(handle_plugin_command_ack))
        .layer(middleware::from_fn(move |mut req: Request, next: Next| {
            if let Some(ref tok) = token_for_mutating {
                req.extensions_mut().insert(ExpectedToken(tok.clone()));
            }
            bearer_auth_layer(req, next)
        }));

    // Read-only routes — no auth required.
    let read_routes = Router::new()
        .route("/health", get(handle_health))
        .route("/snapshot", get(handle_snapshot))
        .route("/diff", get(handle_diff))
        .route("/events", get(handle_events))
        .route("/ws", get(handle_ws))
        .route("/sources", get(handle_sources))
        .route("/sources/content", get(handle_sources_content))
        .route("/source/{*path}", get(handle_source))
        .route("/validate", get(handle_validate))
        .route("/metrics", get(handle_metrics))
        .route("/history", get(handle_history))
        .route("/rewind", get(handle_rewind))
        .route("/model/{*path}", get(handle_model))
        .route("/config", get(handle_config))
        .route("/sourcemap", get(handle_sourcemap))
        .route("/project", get(handle_project))
        .route("/mcp/tools", get(handle_mcp_tools))
        // GET handlers for plugin state/managed (read-only).
        .route("/plugin/state", get(handle_get_plugin_state))
        .route("/plugin/managed", get(handle_get_plugin_managed));

    read_routes
        .merge(mutating_routes)
        .with_state(state)
        .merge(rbxl_router(rbxl_state))
        .layer(cors)
        .layer(DefaultBodyLimit::max(MAX_BODY_BYTES))
}

/// Start the HTTP server. This blocks until the server exits.
pub async fn run_serve(
    root: std::path::PathBuf,
    project_path: std::path::PathBuf,
    includes: Vec<String>,
    port: u16,
    interval: Duration,
    channel_capacity: usize,
    coalesce_ms: u64,
    turbo: bool,
    address: String,
) -> anyhow::Result<()> {
    // Load glob ignore patterns from project file if available.
    let glob_ignores = if project_path.is_file() {
        if let Ok(tree) = crate::project::parse_project(&project_path) {
            GlobIgnoreSet::new(&tree.glob_ignore_paths)
        } else {
            GlobIgnoreSet::empty()
        }
    } else {
        GlobIgnoreSet::empty()
    };

    let snapshot = build_snapshot_with_ignores(&root, &includes, &glob_ignores)?;
    let state = ServerState::with_full_config(
        root,
        includes,
        snapshot,
        channel_capacity,
        turbo,
        coalesce_ms,
        false,
        glob_ignores,
        Some(project_path.clone()),
    );

    let coalescer = Arc::new(EventCoalescer::new(Duration::from_millis(coalesce_ms)));

    // Wire coalescer into state for observability (sync_watch_status tool).
    {
        let mut lock = state.coalescer.lock().unwrap_or_else(|e| e.into_inner());
        *lock = Some(Arc::clone(&coalescer));
    }

    // Background poller with coalescing.
    let poll_state = Arc::clone(&state);
    let poll_coalescer = Arc::clone(&coalescer);
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(interval).await;

            // Signal the coalescer — if a window is already open, skip
            // (the pending rebuild will pick up our changes).
            if !poll_coalescer.signal() {
                continue;
            }

            poll_coalescer.wait_for_quiescence().await;

            if let Err(error) = poll_state.poll_and_broadcast() {
                eprintln!("[vertigo-sync] poll error: {error}");
            }
        }
    });

    let app = build_router(state);

    let addr_str = format!("{address}:{port}");
    let addr: std::net::SocketAddr = addr_str
        .parse()
        .map_err(|e| anyhow::anyhow!("invalid serve address '{addr_str}': {e}"))?;

    // Warn if binding to a non-loopback address without bearer token auth.
    let is_loopback = addr.ip().is_loopback();
    let has_token = std::env::var("VERTIGO_SYNC_API_TOKEN")
        .ok()
        .filter(|t| !t.is_empty())
        .is_some();
    if !is_loopback && !has_token {
        eprintln!(
            "[vertigo-sync] WARNING: Binding to non-loopback address {addr} without \
             VERTIGO_SYNC_API_TOKEN set. Mutating endpoints (POST /sync/patch, /mcp/execute, \
             /plugin/*) are unprotected. Set VERTIGO_SYNC_API_TOKEN to require bearer auth."
        );
    }
    if has_token {
        eprintln!("[vertigo-sync] Bearer token auth enabled for mutating endpoints.");
    }

    eprintln!(
        "[vertigo-sync] serving on http://{addr} (coalesce={}ms, poll={}ms)",
        coalesce_ms,
        interval.as_millis()
    );

    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, app).await?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

async fn handle_health(State(state): State<Arc<ServerState>>) -> Json<serde_json::Value> {
    let boot_elapsed = state.boot_time.elapsed().as_secs();
    Json(serde_json::json!({
        "status": "ok",
        "version": "0.1.0",
        "server_boot_time": boot_elapsed
    }))
}

// ---------------------------------------------------------------------------
// Plugin state reporting endpoints
// ---------------------------------------------------------------------------

/// POST /plugin/state — plugin pushes its internal state periodically.
/// Returns 200 with pending commands if any, or 204 if none (backward compatible).
async fn handle_post_plugin_state(
    State(state): State<Arc<ServerState>>,
    Json(body): Json<serde_json::Value>,
) -> (StatusCode, axum::response::Response) {
    *state.plugin_state.lock().unwrap_or_else(|e| e.into_inner()) = Some(body);
    *state
        .plugin_state_at
        .lock()
        .unwrap_or_else(|e| e.into_inner()) = Some(std::time::Instant::now());

    let commands = state.drain_plugin_commands();
    if commands.is_empty() {
        (
            StatusCode::NO_CONTENT,
            axum::response::IntoResponse::into_response(StatusCode::NO_CONTENT),
        )
    } else {
        let json_body = serde_json::json!({ "commands": commands });
        (
            StatusCode::OK,
            axum::response::IntoResponse::into_response(Json(json_body)),
        )
    }
}

/// POST /plugin/command/ack — plugin acknowledges executed commands.
async fn handle_plugin_command_ack(
    State(state): State<Arc<ServerState>>,
    Json(body): Json<crate::PluginCommandAck>,
) -> StatusCode {
    let mut acks = state
        .plugin_command_acks
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    acks.insert(body.command_id.clone(), body);
    StatusCode::NO_CONTENT
}

/// GET /plugin/state — returns latest plugin state with staleness indicator.
async fn handle_get_plugin_state(
    State(state): State<Arc<ServerState>>,
) -> Result<Json<serde_json::Value>, StatusCode> {
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
            Ok(Json(value))
        }
        _ => Err(StatusCode::NOT_FOUND),
    }
}

/// POST /plugin/managed — plugin pushes its managed index summary periodically.
async fn handle_post_plugin_managed(
    State(state): State<Arc<ServerState>>,
    Json(body): Json<serde_json::Value>,
) -> StatusCode {
    *state
        .plugin_managed
        .lock()
        .unwrap_or_else(|e| e.into_inner()) = Some(body);
    *state
        .plugin_managed_at
        .lock()
        .unwrap_or_else(|e| e.into_inner()) = Some(std::time::Instant::now());
    StatusCode::NO_CONTENT
}

/// GET /plugin/managed — returns latest managed index with staleness indicator.
async fn handle_get_plugin_managed(
    State(state): State<Arc<ServerState>>,
) -> Result<Json<serde_json::Value>, StatusCode> {
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
            Ok(Json(value))
        }
        _ => Err(StatusCode::NOT_FOUND),
    }
}

// ---------------------------------------------------------------------------

async fn handle_snapshot(
    State(state): State<Arc<ServerState>>,
) -> Result<Json<Snapshot>, StatusCode> {
    let lock = state.current.lock().unwrap_or_else(|e| e.into_inner());
    Ok(Json((**lock).clone()))
}

#[derive(Deserialize)]
struct DiffQuery {
    since: String,
}

async fn handle_diff(
    State(state): State<Arc<ServerState>>,
    Query(query): Query<DiffQuery>,
) -> Result<Json<SnapshotDiff>, (StatusCode, String)> {
    let old = {
        let lock = state.history.lock().unwrap_or_else(|e| e.into_inner());
        lock.get(&query.since).cloned()
    };

    let old = old.ok_or_else(|| {
        (
            StatusCode::NOT_FOUND,
            format!("snapshot {} not found in history", query.since),
        )
    })?;

    let current = {
        let lock = state.current.lock().unwrap_or_else(|e| e.into_inner());
        Arc::clone(&lock)
    };

    Ok(Json(diff_snapshots(&old, &current)))
}

async fn handle_events(
    State(state): State<Arc<ServerState>>,
) -> Sse<impl tokio_stream::Stream<Item = Result<Event, Infallible>>> {
    let stream = BroadcastStream::new(state.tx.subscribe()).filter_map(|result| match result {
        Ok(event) => {
            let data = serde_json::to_string(&event).unwrap_or_else(|_| "{}".to_string());
            Some(Ok(Event::default().event("sync_diff").data(data)))
        }
        Err(_) => None,
    });

    Sse::new(stream).keep_alive(
        KeepAlive::new()
            .interval(Duration::from_secs(15))
            .text("ping"),
    )
}

// ---------------------------------------------------------------------------
// WebSocket endpoint — real-time bidirectional sync channel
// ---------------------------------------------------------------------------

/// Incoming messages from WebSocket clients.
#[derive(Debug, Deserialize)]
struct WsClientMessage {
    #[serde(rename = "type")]
    msg_type: String,
    #[serde(default)]
    path: Option<String>,
}

async fn handle_ws(
    ws: WebSocketUpgrade,
    State(state): State<Arc<ServerState>>,
) -> impl axum::response::IntoResponse {
    ws.on_upgrade(|socket| handle_ws_connection(socket, state))
}

async fn handle_ws_connection(mut socket: WebSocket, state: Arc<ServerState>) {
    state.metrics.ws_connections.fetch_add(1, Ordering::Relaxed);

    // Send initial connected message with current fingerprint and entry count.
    let (fingerprint, entries) = {
        let lock = state.current.lock().unwrap_or_else(|e| e.into_inner());
        (lock.fingerprint.clone(), lock.entries.len())
    };

    let connected_msg = serde_json::json!({
        "type": "connected",
        "fingerprint": fingerprint,
        "entries": entries,
    });
    if socket
        .send(Message::Text(connected_msg.to_string().into()))
        .await
        .is_err()
    {
        return;
    }

    // Subscribe to the broadcast channel for diff events.
    let mut rx = state.tx.subscribe();

    // Sequence counter for WebSocket-scoped messages.
    let mut ws_seq: u64 = 0;

    // Ping/pong keepalive state.
    let mut missed_pongs: u32 = 0;
    let mut ping_interval = tokio::time::interval(Duration::from_secs(15));
    // Skip the immediate first tick.
    ping_interval.tick().await;

    loop {
        tokio::select! {
            // Forward broadcast diff events to the client.
            result = rx.recv() => {
                match result {
                    Ok(event) => {
                        ws_seq += 1;
                        let mut msg = serde_json::json!({
                            "type": "sync_diff",
                            "seq": ws_seq,
                            "source_hash": event.source_hash,
                            "diff": {
                                "added": event.added_paths.len(),
                                "modified": event.modified_paths.len(),
                                "deleted": event.deleted_paths.len(),
                            },
                            "paths": {
                                "added": event.added_paths,
                                "modified": event.modified_paths,
                                "deleted": event.deleted_paths,
                            },
                            "timestamp": event.timestamp,
                        });
                        // Include renamed paths when present (backward-compatible).
                        if !event.renamed_paths.is_empty() {
                            msg["paths"]["renamed"] = serde_json::json!(event.renamed_paths);
                        }
                        if socket.send(Message::Text(msg.to_string().into())).await.is_err() {
                            break;
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                        // Notify client they missed events and should request a snapshot.
                        let msg = serde_json::json!({
                            "type": "lagged",
                            "missed": n,
                        });
                        let _ = socket.send(Message::Text(msg.to_string().into())).await;
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                }
            }

            // Accept incoming messages from the client.
            maybe_msg = socket.recv() => {
                match maybe_msg {
                    Some(Ok(Message::Text(text))) => {
                        if let Ok(client_msg) = serde_json::from_str::<WsClientMessage>(&text) {
                            match client_msg.msg_type.as_str() {
                                "request_snapshot" => {
                                    let snap = {
                                        let lock = state.current.lock().unwrap_or_else(|e| e.into_inner());
                                        (**lock).clone()
                                    };
                                    let resp = serde_json::json!({
                                        "type": "snapshot",
                                        "fingerprint": snap.fingerprint,
                                        "entries": snap.entries,
                                    });
                                    if socket.send(Message::Text(resp.to_string().into())).await.is_err() {
                                        break;
                                    }
                                }
                                "request_source" => {
                                    if let Some(ref path) = client_msg.path {
                                        let resp = serve_source_for_ws(&state, path);
                                        if socket.send(Message::Text(resp.to_string().into())).await.is_err() {
                                            break;
                                        }
                                    }
                                }
                                _ => {
                                    // Unknown message type — ignore silently.
                                }
                            }
                        }
                    }
                    Some(Ok(Message::Pong(_))) => {
                        missed_pongs = 0;
                    }
                    Some(Ok(Message::Close(_))) | None => break,
                    Some(Err(_)) => break,
                    _ => {}
                }
            }

            // Send periodic pings for keepalive.
            _ = ping_interval.tick() => {
                if missed_pongs >= 3 {
                    break;
                }
                if socket.send(Message::Ping(vec![].into())).await.is_err() {
                    break;
                }
                missed_pongs += 1;
            }
        }
    }

    // Decrement active WS connection count on drop.
    state.metrics.ws_connections.fetch_sub(1, Ordering::Relaxed);
}

/// Read a source file and return a JSON value for the WebSocket response.
fn serve_source_for_ws(state: &Arc<ServerState>, raw_path: &str) -> serde_json::Value {
    let source_root = state.canonical_root.clone();
    let normalized = match normalize_snapshot_lookup_path(raw_path) {
        Some(path) => path,
        None => {
            return serde_json::json!({
                "type": "source",
                "path": raw_path,
                "error": "path traversal not allowed",
            });
        }
    };
    let resolved = match resolve_source_file(&source_root, &normalized) {
        Ok(path) => path,
        Err((_, error)) => {
            return serde_json::json!({
                "type": "source",
                "path": raw_path,
                "error": error,
            });
        }
    };

    match std::fs::read_to_string(&resolved) {
        Ok(content) => {
            let content_bytes = content.len() as u64;
            let hash = match snapshot_metadata_for_path(state, &normalized) {
                Ok(Some((sha, expected_bytes))) if expected_bytes == content_bytes => sha,
                _ => sha256_hex(content.as_bytes()),
            };

            serde_json::json!({
                "type": "source",
                "path": normalized,
                "content": content,
                "sha256": hash,
            })
        }
        Err(e) => serde_json::json!({
            "type": "source",
            "path": raw_path,
            "error": e.to_string(),
        }),
    }
}

async fn handle_patch(
    State(state): State<Arc<ServerState>>,
    Json(req): Json<SyncPatchRequest>,
) -> Result<Json<SyncPatchAck>, (StatusCode, String)> {
    let _patch_guard = state.patch_lock.lock().await;

    let current_hash = {
        let lock = state.current.lock().unwrap_or_else(|e| e.into_inner());
        lock.fingerprint.clone()
    };

    if req.source_hash != current_hash {
        let errors = vec![format!(
            "hash mismatch: request targets {} but current is {}",
            req.source_hash, current_hash
        )];
        return Ok(Json(rejected_patch_ack(
            current_hash.clone(),
            "hash_mismatch",
            errors,
            Vec::new(),
        )));
    }

    if req.patches.is_empty() {
        return Ok(Json(rejected_patch_ack(
            current_hash,
            "empty_patch",
            vec!["patch request must include at least one patch entry".to_string()],
            Vec::new(),
        )));
    }

    let source_root = state.canonical_root.clone();
    let planned = match plan_patch_ops(&source_root, &req.patches) {
        Ok(planned) => planned,
        Err(errors) => {
            return Ok(Json(rejected_patch_ack(
                current_hash,
                "invalid_patch",
                errors,
                Vec::new(),
            )));
        }
    };

    let expectation_errors = verify_patch_expectations(&planned);
    if !expectation_errors.is_empty() {
        return Ok(Json(rejected_patch_ack(
            current_hash,
            "expected_sha256_mismatch",
            expectation_errors,
            Vec::new(),
        )));
    }

    let applied = match apply_planned_patches_atomically(&planned) {
        Ok(applied) => applied,
        Err(errors) => {
            return Ok(Json(rejected_patch_ack(
                current_hash,
                "apply_failed",
                errors,
                Vec::new(),
            )));
        }
    };

    // Rebuild snapshot after patches.
    let new_snapshot = build_snapshot(&state.root, &state.includes)
        .map_err(|error| (StatusCode::INTERNAL_SERVER_ERROR, error.to_string()))?;
    state
        .install_snapshot_and_broadcast(new_snapshot)
        .map_err(|error| (StatusCode::INTERNAL_SERVER_ERROR, error.to_string()))?;
    let new_hash = {
        let lock = state.current.lock().unwrap_or_else(|e| e.into_inner());
        lock.fingerprint.clone()
    };
    let validation_issues = collect_patch_validation_issues(&planned);

    Ok(Json(accepted_patch_ack(
        new_hash,
        applied,
        validation_issues,
    )))
}

fn accepted_patch_ack(
    new_source_hash: String,
    applied: usize,
    validation: Vec<crate::validate::ValidationIssue>,
) -> SyncPatchAck {
    SyncPatchAck {
        status: "accepted".to_string(),
        reason_code: "ok".to_string(),
        reason_message: String::new(),
        accepted: true,
        new_source_hash,
        applied,
        errors: Vec::new(),
        validation,
    }
}

fn rejected_patch_ack(
    new_source_hash: String,
    reason_code: &str,
    errors: Vec<String>,
    validation: Vec<crate::validate::ValidationIssue>,
) -> SyncPatchAck {
    let reason_message = errors
        .first()
        .cloned()
        .unwrap_or_else(|| "patch rejected".to_string());
    SyncPatchAck {
        status: "rejected".to_string(),
        reason_code: reason_code.to_string(),
        reason_message,
        accepted: false,
        new_source_hash,
        applied: 0,
        errors,
        validation,
    }
}

fn plan_patch_ops(
    source_root: &Path,
    patches: &[PatchEntry],
) -> Result<Vec<PlannedPatch>, Vec<String>> {
    let mut planned = Vec::with_capacity(patches.len());
    let mut seen = HashSet::with_capacity(patches.len());
    let mut errors = Vec::new();

    for patch in patches {
        let normalized = match normalize_snapshot_lookup_path(&patch.path) {
            Some(path) => path,
            None => {
                errors.push(format!("{}: invalid patch path", patch.path));
                continue;
            }
        };

        if !seen.insert(normalized.clone()) {
            errors.push(format!(
                "{}: duplicate patch target after normalization ({normalized})",
                patch.path
            ));
            continue;
        }

        let target = match resolve_patch_target(source_root, &patch.path) {
            Ok(path) => path,
            Err(error) => {
                errors.push(format!("{}: invalid patch path: {error}", patch.path));
                continue;
            }
        };

        let expected_sha256 = patch
            .expected_sha256
            .as_ref()
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty());

        match patch.action.as_str() {
            "write" => {
                let Some(content_b64) = patch.content_base64.as_ref() else {
                    errors.push(format!(
                        "{}: write action missing content_base64",
                        patch.path
                    ));
                    continue;
                };

                let bytes = match base64::engine::general_purpose::STANDARD.decode(content_b64) {
                    Ok(bytes) => bytes,
                    Err(error) => {
                        errors.push(format!("{}: base64 decode error: {error}", patch.path));
                        continue;
                    }
                };

                planned.push(PlannedPatch::Write {
                    requested_path: patch.path.clone(),
                    normalized_path: normalized,
                    target,
                    bytes,
                    expected_sha256,
                });
            }
            "delete" => {
                planned.push(PlannedPatch::Delete {
                    requested_path: patch.path.clone(),
                    target,
                    expected_sha256,
                });
            }
            other => {
                errors.push(format!("{}: unknown action '{other}'", patch.path));
            }
        }
    }

    if errors.is_empty() {
        Ok(planned)
    } else {
        Err(errors)
    }
}

fn verify_patch_expectations(planned: &[PlannedPatch]) -> Vec<String> {
    let mut errors = Vec::new();

    for op in planned {
        let Some(expected) = op.expected_sha256() else {
            continue;
        };

        let actual = match std::fs::read(op.target()) {
            Ok(bytes) => sha256_hex(&bytes),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => "<missing>".to_string(),
            Err(error) => {
                errors.push(format!(
                    "{}: failed to read current file for expected_sha256 check: {error}",
                    op.requested_path()
                ));
                continue;
            }
        };

        if !actual.eq_ignore_ascii_case(expected) {
            errors.push(format!(
                "{}: expected_sha256 mismatch (expected={}, actual={})",
                op.requested_path(),
                expected,
                actual
            ));
        }
    }

    errors
}

fn apply_planned_patches_atomically(planned: &[PlannedPatch]) -> Result<usize, Vec<String>> {
    let mut backups = Vec::with_capacity(planned.len());
    for op in planned {
        let target = op.target().to_path_buf();
        match std::fs::read(&target) {
            Ok(bytes) => backups.push(ExistingFileBackup {
                target,
                existed: true,
                bytes,
            }),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                backups.push(ExistingFileBackup {
                    target,
                    existed: false,
                    bytes: Vec::new(),
                });
            }
            Err(error) => {
                return Err(vec![format!(
                    "{}: failed to read existing file before apply: {error}",
                    op.requested_path()
                )]);
            }
        }
    }

    for op in planned {
        let apply_result = match op {
            PlannedPatch::Write { target, bytes, .. } => replace_file_contents(target, bytes),
            PlannedPatch::Delete { target, .. } => match std::fs::remove_file(target) {
                Ok(()) => Ok(()),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
                Err(error) => Err(error),
            },
        };

        if let Err(error) = apply_result {
            let mut errors = vec![format!("{}: apply failed: {error}", op.requested_path())];
            errors.extend(rollback_patch_backups(&backups));
            return Err(errors);
        }
    }

    Ok(planned.len())
}

fn rollback_patch_backups(backups: &[ExistingFileBackup]) -> Vec<String> {
    let mut errors = Vec::new();

    for backup in backups.iter().rev() {
        if backup.existed {
            if let Err(error) = replace_file_contents(&backup.target, &backup.bytes) {
                errors.push(format!(
                    "{}: rollback restore failed: {error}",
                    backup.target.display()
                ));
            }
        } else if backup.target.exists()
            && let Err(error) = std::fs::remove_file(&backup.target)
        {
            errors.push(format!(
                "{}: rollback cleanup failed: {error}",
                backup.target.display()
            ));
        }
    }

    errors
}

fn replace_file_contents(target: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let parent = target
        .parent()
        .ok_or_else(|| std::io::Error::other("target file has no parent directory"))?;
    std::fs::create_dir_all(parent)?;

    let tmp_suffix = std::time::SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let tmp_path = parent.join(format!(
        ".vertigo-sync.tmp.{}.{}",
        std::process::id(),
        tmp_suffix
    ));
    std::fs::write(&tmp_path, bytes)?;

    #[cfg(windows)]
    if target.exists() {
        std::fs::remove_file(target)?;
    }

    match std::fs::rename(&tmp_path, target) {
        Ok(()) => Ok(()),
        Err(error) => {
            let _ = std::fs::remove_file(&tmp_path);
            Err(error)
        }
    }
}

fn collect_patch_validation_issues(
    planned: &[PlannedPatch],
) -> Vec<crate::validate::ValidationIssue> {
    let mut issues = Vec::new();
    for op in planned {
        if let PlannedPatch::Write {
            normalized_path,
            bytes,
            ..
        } = op
            && let Ok(content) = std::str::from_utf8(bytes)
        {
            issues.extend(crate::validate::validate_file_content(
                normalized_path,
                content,
            ));
        }
    }
    issues
}

/// Source file entry returned by GET /sources.
#[derive(Debug, Clone, serde::Serialize)]
struct SourceEntry {
    path: String,
    sha256: String,
    bytes: u64,
}

async fn handle_sources(
    State(state): State<Arc<ServerState>>,
) -> Result<Json<Vec<SourceEntry>>, StatusCode> {
    let lock = state.current.lock().unwrap_or_else(|e| e.into_inner());
    let entries: Vec<SourceEntry> = lock
        .entries
        .iter()
        .map(|e| SourceEntry {
            path: e.path.clone(),
            sha256: e.sha256.clone(),
            bytes: e.bytes,
        })
        .collect();
    Ok(Json(entries))
}

#[derive(Debug, Deserialize)]
struct SourcesContentQuery {
    paths: String,
}

#[derive(Debug, Clone, serde::Serialize)]
struct SourceContentEntry {
    path: String,
    sha256: String,
    bytes: u64,
    content: String,
}

#[derive(Debug, Clone, serde::Serialize)]
struct SourcesContentResponse {
    entries: Vec<SourceContentEntry>,
    missing: Vec<String>,
}

async fn handle_sources_content(
    State(state): State<Arc<ServerState>>,
    Query(query): Query<SourcesContentQuery>,
) -> Result<Json<SourcesContentResponse>, (StatusCode, String)> {
    let source_root = state.canonical_root.clone();

    let mut dedupe = HashSet::new();
    let mut requested = Vec::new();
    for raw in query.paths.split(',') {
        let path = raw.trim();
        if path.is_empty() {
            continue;
        }
        let normalized = normalize_snapshot_lookup_path(path)
            .ok_or((StatusCode::BAD_REQUEST, "invalid relative path".to_string()))?;
        if dedupe.insert(normalized.clone()) {
            requested.push(normalized);
        }
    }

    if requested.is_empty() {
        return Err((
            StatusCode::BAD_REQUEST,
            "paths query must include at least one relative path".to_string(),
        ));
    }

    if requested.len() > MAX_BATCH_SOURCE_PATHS {
        return Err((
            StatusCode::PAYLOAD_TOO_LARGE,
            format!(
                "paths query exceeds max entries ({}/{MAX_BATCH_SOURCE_PATHS})",
                requested.len()
            ),
        ));
    }

    let mut entries: Vec<SourceContentEntry> = Vec::with_capacity(requested.len());
    let mut missing: Vec<String> = Vec::new();
    let mut total_bytes: usize = 0;
    let snapshot_meta = snapshot_metadata_for_paths(&state, &requested)?;

    for raw_path in requested {
        let resolved = match resolve_source_file(&source_root, &raw_path) {
            Ok(path) => path,
            Err((StatusCode::NOT_FOUND, _)) => {
                missing.push(raw_path);
                continue;
            }
            Err(error) => return Err(error),
        };

        let content = std::fs::read_to_string(&resolved)
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
        total_bytes += content.len();
        if total_bytes > MAX_BATCH_SOURCE_BYTES {
            return Err((
                StatusCode::PAYLOAD_TOO_LARGE,
                format!(
                    "batch source payload exceeded {} bytes; reduce request size",
                    MAX_BATCH_SOURCE_BYTES
                ),
            ));
        }

        let content_bytes = content.len() as u64;
        let sha256 = match snapshot_meta.get(&raw_path) {
            Some((sha, expected_bytes)) if *expected_bytes == content_bytes => sha.clone(),
            _ => sha256_hex(content.as_bytes()),
        };

        entries.push(SourceContentEntry {
            path: raw_path,
            sha256,
            bytes: content_bytes,
            content,
        });
    }

    Ok(Json(SourcesContentResponse { entries, missing }))
}

fn normalize_snapshot_lookup_path(raw_path: &str) -> Option<String> {
    let normalized = raw_path.replace('\\', "/");
    let candidate = Path::new(&normalized);
    let mut out = PathBuf::new();
    for component in candidate.components() {
        match component {
            Component::CurDir => {}
            Component::Normal(seg) => out.push(seg),
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => return None,
        }
    }
    if out.as_os_str().is_empty() {
        return None;
    }
    Some(out.to_string_lossy().replace('\\', "/"))
}

fn snapshot_metadata_for_paths(
    state: &Arc<ServerState>,
    requested: &[String],
) -> Result<HashMap<String, (String, u64)>, (StatusCode, String)> {
    let requested_set: HashSet<&str> = requested.iter().map(|path| path.as_str()).collect();
    let current = state.current.lock().unwrap_or_else(|e| e.into_inner());
    let mut metadata = HashMap::with_capacity(requested.len());
    for entry in &current.entries {
        if requested_set.contains(entry.path.as_str()) {
            metadata.insert(entry.path.clone(), (entry.sha256.clone(), entry.bytes));
        }
    }
    Ok(metadata)
}

fn snapshot_metadata_for_path(
    state: &Arc<ServerState>,
    path: &str,
) -> Result<Option<(String, u64)>, (StatusCode, String)> {
    let current = state.current.lock().unwrap_or_else(|e| e.into_inner());
    Ok(current
        .entries
        .iter()
        .find(|entry| entry.path == path)
        .map(|entry| (entry.sha256.clone(), entry.bytes)))
}

fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    format!("{:x}", hasher.finalize())
}

fn resolve_source_file(
    source_root: &Path,
    raw_path: &str,
) -> Result<PathBuf, (StatusCode, String)> {
    let normalized = normalize_snapshot_lookup_path(raw_path)
        .ok_or((StatusCode::BAD_REQUEST, "path traversal not allowed".into()))?;
    let target = source_root.join(&normalized);
    let resolved = target
        .canonicalize()
        .map_err(|_| (StatusCode::NOT_FOUND, format!("file not found: {raw_path}")))?;
    if !resolved.starts_with(source_root) {
        return Err((StatusCode::BAD_REQUEST, "path escapes source root".into()));
    }
    if !resolved.is_file() {
        return Err((StatusCode::NOT_FOUND, format!("not a file: {raw_path}")));
    }
    Ok(resolved)
}

async fn handle_source(
    State(state): State<Arc<ServerState>>,
    AxumPath(raw_path): AxumPath<String>,
) -> Result<(HeaderMap, String), (StatusCode, String)> {
    let source_root = state.canonical_root.clone();
    let resolved = resolve_source_file(&source_root, &raw_path)?;

    let content = std::fs::read_to_string(&resolved)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    let normalized = normalize_snapshot_lookup_path(&raw_path).ok_or((
        StatusCode::BAD_REQUEST,
        "path traversal not allowed".to_string(),
    ))?;
    let content_bytes = content.len() as u64;
    let hash = match snapshot_metadata_for_path(&state, &normalized)? {
        Some((sha, expected_bytes)) if expected_bytes == content_bytes => sha,
        _ => sha256_hex(content.as_bytes()),
    };

    let mut headers = HeaderMap::new();
    headers.insert("content-type", "text/plain; charset=utf-8".parse().unwrap());
    headers.insert("x-sha256", hash.parse().unwrap());

    Ok((headers, content))
}

async fn handle_validate(
    State(state): State<Arc<ServerState>>,
) -> Result<Json<crate::validate::ValidationReport>, (StatusCode, String)> {
    let report = crate::validate::validate_source(&state.root, &state.includes)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    Ok(Json(report))
}

async fn handle_metrics(State(state): State<Arc<ServerState>>) -> (HeaderMap, String) {
    let body = state.metrics.render();
    let mut headers = HeaderMap::new();
    headers.insert(
        "content-type",
        "text/plain; version=0.0.4; charset=utf-8".parse().unwrap(),
    );
    (headers, body)
}

// ---------------------------------------------------------------------------
// GET /history?limit=N — recent event log entries
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct HistoryQuery {
    #[serde(default = "default_history_limit")]
    limit: usize,
}

fn default_history_limit() -> usize {
    50
}

async fn handle_history(
    State(state): State<Arc<ServerState>>,
    Query(query): Query<HistoryQuery>,
) -> Result<Json<Vec<crate::HistoryEntry>>, (StatusCode, String)> {
    let limit = query.limit.min(500);
    let event_log_path = state.root.join(".vertigo-sync-state").join("events.jsonl");
    let entries = crate::read_history(&event_log_path, limit)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    Ok(Json(entries))
}

// ---------------------------------------------------------------------------
// GET /rewind?to=<fingerprint> — reverse diff to a historical snapshot
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct RewindQuery {
    to: String,
}

async fn handle_rewind(
    State(state): State<Arc<ServerState>>,
    Query(query): Query<RewindQuery>,
) -> Result<Json<crate::SnapshotDiff>, (StatusCode, String)> {
    let target = {
        let lock = state.history.lock().unwrap_or_else(|e| e.into_inner());
        lock.get(&query.to).cloned()
    };

    let target = target.ok_or_else(|| {
        (
            StatusCode::NOT_FOUND,
            format!("fingerprint {} not found in history ring buffer", query.to),
        )
    })?;

    let current = {
        let lock = state.current.lock().unwrap_or_else(|e| e.into_inner());
        std::sync::Arc::clone(&lock)
    };

    let forward = crate::diff_snapshots(&target, &current);
    let reversed = crate::reverse_diff(&forward);
    Ok(Json(reversed))
}

// ---------------------------------------------------------------------------
// GET /model/<path> — lazily deserialized model manifest
// ---------------------------------------------------------------------------

async fn handle_model(
    State(state): State<Arc<ServerState>>,
    AxumPath(path): AxumPath<String>,
) -> Result<Json<crate::ModelManifest>, (StatusCode, String)> {
    // Verify it's a model file.
    if !path.ends_with(".rbxm") && !path.ends_with(".rbxmx") {
        return Err((
            StatusCode::BAD_REQUEST,
            format!("not a binary model file: {path}"),
        ));
    }

    // Path traversal guard: resolve and verify the path stays inside canonical_root.
    let abs_path = state.canonical_root.join(&path);
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

    // Find the entry in the current snapshot to get the content hash.
    let content_hash = {
        let lock = state.current.lock().unwrap_or_else(|e| e.into_inner());
        let entry = lock.entries.iter().find(|e| e.path == path);
        match entry {
            Some(e) => e.sha256.clone(),
            None => {
                return Err((
                    StatusCode::NOT_FOUND,
                    format!("model path not found: {path}"),
                ));
            }
        }
    };

    // Use the ModelManifestCache for content-addressed caching.
    let mut cache_lock = state.model_cache.lock().unwrap_or_else(|e| e.into_inner());
    let manifest = cache_lock
        .get_or_load(&content_hash, &canonical)
        .map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                serde_json::json!({ "error": e.to_string() }).to_string(),
            )
        })?;

    Ok(Json(manifest.clone()))
}

// ---------------------------------------------------------------------------
// GET /config — current server configuration
// ---------------------------------------------------------------------------

async fn handle_config(State(state): State<Arc<ServerState>>) -> Json<serde_json::Value> {
    let history_size = {
        let lock = state.history.lock().unwrap_or_else(|e| e.into_inner());
        lock.len()
    };
    Json(serde_json::json!({
        "binary_models": state.binary_models,
        "model_pool_size": 128,
        "history_buffer_size": history_size,
        "history_buffer_capacity": 256,
        "turbo": state.turbo,
        "coalesce_ms": state.coalesce_ms,
    }))
}

/// Generate a Rojo-compatible sourcemap for luau-lsp integration.
async fn handle_sourcemap(
    State(state): State<Arc<ServerState>>,
    Query(params): Query<HashMap<String, String>>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let include_non_scripts = params
        .get("include_non_scripts")
        .map(|v| v == "true" || v == "1")
        .unwrap_or(true);

    let tree = match crate::project::parse_project(&state.project_path) {
        Ok(t) => t,
        Err(_) => return Err(StatusCode::INTERNAL_SERVER_ERROR),
    };

    let sourcemap =
        match crate::sourcemap::generate_sourcemap(&state.root, &tree, include_non_scripts) {
            Ok(s) => s,
            Err(_) => return Err(StatusCode::INTERNAL_SERVER_ERROR),
        };

    match serde_json::to_value(&sourcemap) {
        Ok(v) => Ok(Json(v)),
        Err(_) => Err(StatusCode::INTERNAL_SERVER_ERROR),
    }
}

/// Expose project-level properties and attributes for the plugin to apply.
async fn handle_project(
    State(state): State<Arc<ServerState>>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let tree = match crate::project::parse_project(&state.project_path) {
        Ok(t) => t,
        Err(_) => return Err(StatusCode::INTERNAL_SERVER_ERROR),
    };

    match serde_json::to_value(&tree) {
        Ok(v) => Ok(Json(v)),
        Err(_) => Err(StatusCode::INTERNAL_SERVER_ERROR),
    }
}

fn resolve_patch_target(source_root: &Path, raw_path: &str) -> anyhow::Result<PathBuf> {
    let normalized = raw_path.replace('\\', "/");
    let candidate = Path::new(&normalized);

    if candidate.is_absolute() {
        anyhow::bail!("absolute paths are not allowed")
    }

    for component in candidate.components() {
        match component {
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                anyhow::bail!("path traversal is not allowed")
            }
            _ => {}
        }
    }

    Ok(source_root.join(candidate))
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine;
    use serde_json::json;
    use std::fs;
    use std::path::Path;
    use tempfile::tempdir;

    fn write_project(path: &Path, name: &str, source_root: &str) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("create project parent");
        }
        fs::write(
            path,
            serde_json::to_vec_pretty(&json!({
                "name": name,
                "tree": {
                    "$className": "DataModel",
                    "ServerScriptService": {
                        "Server": {
                            "$path": source_root
                        }
                    }
                }
            }))
            .expect("serialize project"),
        )
        .expect("write project");
    }

    fn empty_snapshot(include: Vec<String>) -> crate::Snapshot {
        crate::Snapshot {
            version: 1,
            include,
            fingerprint: "test-fingerprint".to_string(),
            entries: Vec::new(),
        }
    }

    #[test]
    fn normalize_snapshot_lookup_path_canonicalizes_separators() {
        let path = normalize_snapshot_lookup_path(r".\src\Server\init.server.luau")
            .expect("normalize path");
        assert_eq!(path, "src/Server/init.server.luau");
    }

    #[test]
    fn normalize_snapshot_lookup_path_rejects_parent_dir() {
        assert!(normalize_snapshot_lookup_path("../secrets.txt").is_none());
        assert!(normalize_snapshot_lookup_path(r"..\secrets.txt").is_none());
    }

    #[test]
    fn resolve_source_file_accepts_backslash_relative_paths() {
        let root_dir = tempdir().expect("tempdir");
        let src_dir = root_dir.path().join("src").join("Server");
        fs::create_dir_all(&src_dir).expect("create src/Server");
        let file_path = src_dir.join("init.server.luau");
        fs::write(&file_path, "return {}\n").expect("write source");

        let canonical_root = root_dir.path().canonicalize().expect("canonical root");
        let resolved =
            resolve_source_file(&canonical_root, r"src\Server\init.server.luau").expect("resolve");

        assert_eq!(resolved, file_path.canonicalize().expect("canonical file"));
    }

    #[test]
    fn plan_patch_ops_rejects_duplicate_targets_after_normalization() {
        let root_dir = tempdir().expect("tempdir");
        let canonical_root = root_dir.path().canonicalize().expect("canonical root");
        let patch_a = PatchEntry {
            path: r"src\Shared\init.luau".to_string(),
            action: "write".to_string(),
            content_base64: Some(base64::engine::general_purpose::STANDARD.encode("return 1\n")),
            expected_sha256: None,
        };
        let patch_b = PatchEntry {
            path: "./src/Shared/init.luau".to_string(),
            action: "delete".to_string(),
            content_base64: None,
            expected_sha256: None,
        };

        let errors =
            plan_patch_ops(&canonical_root, &[patch_a, patch_b]).expect_err("duplicate target");

        assert!(
            errors
                .iter()
                .any(|line| line.contains("duplicate patch target")),
            "expected duplicate-target error, got: {errors:?}"
        );
    }

    #[test]
    fn verify_patch_expectations_detects_sha_mismatch() {
        let root_dir = tempdir().expect("tempdir");
        let src_dir = root_dir.path().join("src");
        fs::create_dir_all(&src_dir).expect("create src");
        let file_path = src_dir.join("init.server.luau");
        fs::write(&file_path, "return 1\n").expect("seed file");
        let canonical_root = root_dir.path().canonicalize().expect("canonical root");

        let patch = PatchEntry {
            path: "src/init.server.luau".to_string(),
            action: "write".to_string(),
            content_base64: Some(base64::engine::general_purpose::STANDARD.encode("return 2\n")),
            expected_sha256: Some("deadbeef".to_string()),
        };

        let planned = plan_patch_ops(&canonical_root, &[patch]).expect("plan");
        let errors = verify_patch_expectations(&planned);
        assert!(
            errors
                .iter()
                .any(|line| line.contains("expected_sha256 mismatch")),
            "expected expected_sha256 mismatch, got: {errors:?}"
        );
    }

    #[test]
    fn apply_planned_patches_atomically_preserves_existing_file_on_failure() {
        let root_dir = tempdir().expect("tempdir");
        let src_dir = root_dir.path().join("src");
        fs::create_dir_all(&src_dir).expect("create src");
        let stable_path = src_dir.join("stable.luau");
        fs::write(&stable_path, "return 'stable-old'\n").expect("seed stable file");

        // Create a file where a directory is expected so the second write fails.
        let blocking_file = root_dir.path().join("blocked");
        fs::write(&blocking_file, "not a directory").expect("seed blocking file");

        let canonical_root = root_dir.path().canonicalize().expect("canonical root");
        let patch_ok = PatchEntry {
            path: "src/stable.luau".to_string(),
            action: "write".to_string(),
            content_base64: Some(
                base64::engine::general_purpose::STANDARD.encode("return 'stable-new'\n"),
            ),
            expected_sha256: None,
        };
        let patch_fail = PatchEntry {
            path: "blocked/file.luau".to_string(),
            action: "write".to_string(),
            content_base64: Some(base64::engine::general_purpose::STANDARD.encode("return 0\n")),
            expected_sha256: None,
        };

        let planned = plan_patch_ops(&canonical_root, &[patch_ok, patch_fail]).expect("plan");
        let errors = apply_planned_patches_atomically(&planned).expect_err("apply should fail");
        assert!(
            errors.iter().any(|line| {
                line.contains("apply failed")
                    || line.contains("failed to read existing file before apply")
            }),
            "expected patch application failure, got: {errors:?}"
        );

        let stable_now = fs::read_to_string(&stable_path).expect("read stable after rollback");
        assert_eq!(stable_now, "return 'stable-old'\n");
    }

    #[test]
    fn rejected_patch_ack_sets_reason_fields() {
        let ack = rejected_patch_ack(
            "sha256:new".to_string(),
            "invalid_patch",
            vec!["bad patch".to_string()],
            Vec::new(),
        );
        assert!(!ack.accepted);
        assert_eq!(ack.status, "rejected");
        assert_eq!(ack.reason_code, "invalid_patch");
        assert_eq!(ack.reason_message, "bad patch");
    }

    #[test]
    fn accepted_patch_ack_sets_ok_reason() {
        let ack = accepted_patch_ack("sha256:new".to_string(), 3, Vec::new());
        assert!(ack.accepted);
        assert_eq!(ack.status, "accepted");
        assert_eq!(ack.reason_code, "ok");
        assert_eq!(ack.reason_message, "");
        assert_eq!(ack.applied, 3);
    }

    #[test]
    fn handle_project_uses_selected_project_path() {
        let workspace = tempdir().expect("tempdir");
        write_project(
            &workspace.path().join("default.project.json"),
            "RootGame",
            "root-src/Server",
        );

        let nested_root = workspace.path().join("apps/game");
        write_project(
            &nested_root.join("default.project.json"),
            "NestedGame",
            "nested-src/Server",
        );

        let state = crate::ServerState::with_full_config(
            nested_root.clone(),
            vec!["nested-src".to_string()],
            empty_snapshot(vec!["nested-src".to_string()]),
            32,
            false,
            50,
            false,
            crate::GlobIgnoreSet::empty(),
            Some(nested_root.join("default.project.json")),
        );

        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        let Json(value) = runtime
            .block_on(handle_project(State(state)))
            .expect("project response");

        assert_eq!(value["name"], "NestedGame");
    }

    #[test]
    fn handle_sourcemap_uses_selected_project_path_and_root() {
        let workspace = tempdir().expect("tempdir");
        write_project(
            &workspace.path().join("default.project.json"),
            "RootGame",
            "root-src/Server",
        );
        fs::create_dir_all(workspace.path().join("root-src/Server")).expect("create root src");
        fs::write(
            workspace.path().join("root-src/Server/init.server.luau"),
            "return 'root'\n",
        )
        .expect("write root source");

        let nested_root = workspace.path().join("apps/game");
        write_project(
            &nested_root.join("default.project.json"),
            "NestedGame",
            "nested-src/Server",
        );
        fs::create_dir_all(nested_root.join("nested-src/Server")).expect("create nested src");
        fs::write(
            nested_root.join("nested-src/Server/init.server.luau"),
            "return 'nested'\n",
        )
        .expect("write nested source");

        let state = crate::ServerState::with_full_config(
            nested_root.clone(),
            vec!["nested-src".to_string()],
            empty_snapshot(vec!["nested-src".to_string()]),
            32,
            false,
            50,
            false,
            crate::GlobIgnoreSet::empty(),
            Some(nested_root.join("default.project.json")),
        );

        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        let Json(value) = runtime
            .block_on(handle_sourcemap(
                State(state),
                Query(std::collections::HashMap::new()),
            ))
            .expect("sourcemap response");
        let serialized = serde_json::to_string(&value).expect("serialize sourcemap");

        assert!(serialized.contains("nested-src/Server/init.server.luau"));
        assert!(!serialized.contains("root-src/Server/init.server.luau"));
    }
}
