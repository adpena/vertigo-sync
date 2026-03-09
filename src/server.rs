//! HTTP server for vertigo-sync serve mode.
//!
//! Endpoints:
//!   GET  /health             — status + version
//!   GET  /snapshot           — current snapshot JSON
//!   GET  /diff?since=<hash>  — diff from historical hash to current
//!   GET  /events             — SSE stream of SyncDiffEvent
//!   POST /sync/patch         — apply file patches, return ack
//!   GET  /validate            — run source validation, return report

use std::convert::Infallible;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{DefaultBodyLimit, Path as AxumPath, Query, State};
use axum::http::{HeaderMap, Method, StatusCode};
use axum::response::Json;
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::routing::{get, post};
use base64::Engine;
use serde::Deserialize;
use tokio_stream::StreamExt;
use tokio_stream::wrappers::BroadcastStream;
use tower_http::cors::{AllowOrigin, CorsLayer};

use sha2::{Digest, Sha256};

use crate::mcp::{handle_mcp_execute, handle_mcp_tools};
use crate::serve_rbxl::{new_shared_rbxl_state, rbxl_router};
use crate::{EventCoalescer, ServerState, Snapshot, SnapshotDiff, build_snapshot, diff_snapshots};
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
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct SyncPatchAck {
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

/// Build the Axum router with all endpoints.
pub fn build_router(state: Arc<ServerState>) -> Router {
    let rbxl_state = new_shared_rbxl_state();

    // CORS: allow browser clients on any localhost port (Vite dev, Strata, etc.)
    // and the deployed showcase site. Strata WASM needs GET + POST + WebSocket upgrade.
    let cors = CorsLayer::new()
        .allow_origin(AllowOrigin::predicate(|origin, _| {
            let origin = origin.as_bytes();
            // Allow any localhost/127.0.0.1 origin (any port).
            origin.starts_with(b"http://localhost")
                || origin.starts_with(b"http://127.0.0.1")
                || origin.starts_with(b"https://localhost")
                || origin.starts_with(b"https://127.0.0.1")
        }))
        .allow_methods([Method::GET, Method::POST, Method::OPTIONS])
        .allow_headers([axum::http::header::CONTENT_TYPE])
        .max_age(Duration::from_secs(86400));

    Router::new()
        .route("/health", get(handle_health))
        .route("/snapshot", get(handle_snapshot))
        .route("/diff", get(handle_diff))
        .route("/events", get(handle_events))
        .route("/ws", get(handle_ws))
        .route("/sources", get(handle_sources))
        .route("/source/{*path}", get(handle_source))
        .route("/sync/patch", post(handle_patch))
        .route("/validate", get(handle_validate))
        .route("/metrics", get(handle_metrics))
        .route("/mcp/tools", get(handle_mcp_tools))
        .route("/mcp/execute", post(handle_mcp_execute))
        .with_state(state)
        .merge(rbxl_router(rbxl_state))
        .layer(cors)
        .layer(DefaultBodyLimit::max(MAX_BODY_BYTES))
}

/// Start the HTTP server. This blocks until the server exits.
pub async fn run_serve(
    root: std::path::PathBuf,
    includes: Vec<String>,
    port: u16,
    interval: Duration,
    channel_capacity: usize,
    coalesce_ms: u64,
) -> anyhow::Result<()> {
    let snapshot = build_snapshot(&root, &includes)?;
    let state = ServerState::new(root, includes, snapshot, channel_capacity);

    let coalescer = Arc::new(EventCoalescer::new(Duration::from_millis(coalesce_ms)));

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

    let addr: std::net::SocketAddr = ([127, 0, 0, 1], port).into();
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

async fn handle_health() -> Json<serde_json::Value> {
    Json(serde_json::json!({
        "status": "ok",
        "version": "0.1.0"
    }))
}

async fn handle_snapshot(
    State(state): State<Arc<ServerState>>,
) -> Result<Json<Snapshot>, StatusCode> {
    let lock = state
        .current
        .lock()
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
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
        let lock = state
            .history
            .lock()
            .map_err(|_| (StatusCode::INTERNAL_SERVER_ERROR, "lock".into()))?;
        lock.get(&query.since).cloned()
    };

    let old = old.ok_or_else(|| {
        (
            StatusCode::NOT_FOUND,
            format!("snapshot {} not found in history", query.since),
        )
    })?;

    let current = {
        let lock = state
            .current
            .lock()
            .map_err(|_| (StatusCode::INTERNAL_SERVER_ERROR, "lock".into()))?;
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
        let lock = match state.current.lock() {
            Ok(lock) => lock,
            Err(_) => return,
        };
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
                        let msg = serde_json::json!({
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
                                        let lock = match state.current.lock() {
                                            Ok(l) => l,
                                            Err(_) => break,
                                        };
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
    let candidate = Path::new(raw_path);
    if candidate.is_absolute() {
        return serde_json::json!({
            "type": "source",
            "path": raw_path,
            "error": "absolute paths not allowed",
        });
    }

    for component in candidate.components() {
        if matches!(
            component,
            Component::ParentDir | Component::RootDir | Component::Prefix(_)
        ) {
            return serde_json::json!({
                "type": "source",
                "path": raw_path,
                "error": "path traversal not allowed",
            });
        }
    }

    let source_root = match state.root.canonicalize() {
        Ok(r) => r,
        Err(e) => {
            return serde_json::json!({
                "type": "source",
                "path": raw_path,
                "error": e.to_string(),
            });
        }
    };

    let target = source_root.join(candidate);
    let resolved = match target.canonicalize() {
        Ok(r) if r.starts_with(&source_root) && r.is_file() => r,
        _ => {
            return serde_json::json!({
                "type": "source",
                "path": raw_path,
                "error": "file not found",
            });
        }
    };

    match std::fs::read_to_string(&resolved) {
        Ok(content) => {
            let mut hasher = Sha256::new();
            hasher.update(content.as_bytes());
            let hash = format!("{:x}", hasher.finalize());

            serde_json::json!({
                "type": "source",
                "path": raw_path,
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
    let current_hash = {
        let lock = state
            .current
            .lock()
            .map_err(|_| (StatusCode::INTERNAL_SERVER_ERROR, "lock".into()))?;
        lock.fingerprint.clone()
    };

    if req.source_hash != current_hash {
        return Ok(Json(SyncPatchAck {
            accepted: false,
            new_source_hash: current_hash.clone(),
            applied: 0,
            errors: vec![format!(
                "hash mismatch: request targets {} but current is {}",
                req.source_hash, current_hash
            )],
            validation: Vec::new(),
        }));
    }

    let source_root = state
        .root
        .canonicalize()
        .map_err(|error| (StatusCode::INTERNAL_SERVER_ERROR, error.to_string()))?;

    let mut applied = 0usize;
    let mut errors: Vec<String> = Vec::new();

    for patch in &req.patches {
        let target = match resolve_patch_target(&source_root, &patch.path) {
            Ok(target) => target,
            Err(error) => {
                errors.push(format!("{}: invalid patch path: {error}", patch.path));
                continue;
            }
        };

        match patch.action.as_str() {
            "write" => {
                if let Some(content) = patch.content_base64.as_ref() {
                    match base64::engine::general_purpose::STANDARD.decode(content) {
                        Ok(bytes) => {
                            if let Some(parent) = target.parent() {
                                let _ = std::fs::create_dir_all(parent);
                            }
                            match std::fs::write(&target, &bytes) {
                                Ok(()) => applied += 1,
                                Err(error) => {
                                    errors.push(format!("{}: write error: {error}", patch.path));
                                }
                            }
                        }
                        Err(error) => {
                            errors.push(format!("{}: base64 decode error: {error}", patch.path));
                        }
                    }
                } else {
                    errors.push(format!(
                        "{}: write action missing content_base64",
                        patch.path
                    ));
                }
            }
            "delete" => match std::fs::remove_file(&target) {
                Ok(()) => applied += 1,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => applied += 1,
                Err(error) => errors.push(format!("{}: delete error: {error}", patch.path)),
            },
            other => errors.push(format!("{}: unknown action '{other}'", patch.path)),
        }
    }

    // Rebuild snapshot after patches.
    let new_snapshot = build_snapshot(&state.root, &state.includes)
        .map_err(|error| (StatusCode::INTERNAL_SERVER_ERROR, error.to_string()))?;

    let new_hash = new_snapshot.fingerprint.clone();
    let new_arc = Arc::new(new_snapshot);

    {
        let mut lock = state
            .current
            .lock()
            .map_err(|_| (StatusCode::INTERNAL_SERVER_ERROR, "lock".into()))?;
        *lock = Arc::clone(&new_arc);
    }
    {
        let mut lock = state
            .history
            .lock()
            .map_err(|_| (StatusCode::INTERNAL_SERVER_ERROR, "lock".into()))?;
        lock.insert(new_hash.clone(), new_arc);
    }

    // Validate changed files only.
    let mut validation_issues = Vec::new();
    for patch in &req.patches {
        if patch.action == "write" {
            if let Some(content_b64) = patch.content_base64.as_ref() {
                if let Ok(bytes) = base64::engine::general_purpose::STANDARD.decode(content_b64) {
                    if let Ok(content) = String::from_utf8(bytes) {
                        let file_issues =
                            crate::validate::validate_file_content(&patch.path, &content);
                        validation_issues.extend(file_issues);
                    }
                }
            }
        }
    }

    Ok(Json(SyncPatchAck {
        accepted: errors.is_empty(),
        new_source_hash: new_hash,
        applied,
        errors,
        validation: validation_issues,
    }))
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
    let lock = state
        .current
        .lock()
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
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

async fn handle_source(
    State(state): State<Arc<ServerState>>,
    AxumPath(raw_path): AxumPath<String>,
) -> Result<(HeaderMap, String), (StatusCode, String)> {
    // Validate: no traversal, must be within include roots.
    let candidate = Path::new(&raw_path);
    if candidate.is_absolute() {
        return Err((StatusCode::BAD_REQUEST, "absolute paths not allowed".into()));
    }
    for component in candidate.components() {
        match component {
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                return Err((StatusCode::BAD_REQUEST, "path traversal not allowed".into()));
            }
            _ => {}
        }
    }

    let source_root = state
        .root
        .canonicalize()
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    let target = source_root.join(candidate);

    // Verify the resolved path is still under the source root.
    let resolved = target
        .canonicalize()
        .map_err(|_| (StatusCode::NOT_FOUND, format!("file not found: {raw_path}")))?;
    if !resolved.starts_with(&source_root) {
        return Err((StatusCode::BAD_REQUEST, "path escapes source root".into()));
    }

    if !resolved.is_file() {
        return Err((StatusCode::NOT_FOUND, format!("not a file: {raw_path}")));
    }

    let content = std::fs::read_to_string(&resolved)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    // Compute SHA-256 hash.
    let mut hasher = Sha256::new();
    hasher.update(content.as_bytes());
    let hash = format!("{:x}", hasher.finalize());

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

fn resolve_patch_target(source_root: &Path, raw_path: &str) -> anyhow::Result<PathBuf> {
    let candidate = Path::new(raw_path);

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
