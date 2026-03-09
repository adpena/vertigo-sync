//! HTTP server for vertigo-sync serve mode.
//!
//! Endpoints:
//!   GET  /health             — status + version
//!   GET  /snapshot           — current snapshot JSON
//!   GET  /diff?since=<hash>  — diff from historical hash to current
//!   GET  /events             — SSE stream of SyncDiffEvent
//!   POST /sync/patch         — apply file patches, return ack

use std::convert::Infallible;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::Json;
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::routing::{get, post};
use axum::Router;
use base64::Engine;
use serde::Deserialize;
use tokio_stream::StreamExt;
use tokio_stream::wrappers::BroadcastStream;

use crate::{ServerState, Snapshot, SnapshotDiff, build_snapshot, diff_snapshots};

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
}

/// Build the Axum router with all endpoints.
pub fn build_router(state: Arc<ServerState>) -> Router {
    Router::new()
        .route("/health", get(handle_health))
        .route("/snapshot", get(handle_snapshot))
        .route("/diff", get(handle_diff))
        .route("/events", get(handle_events))
        .route("/sync/patch", post(handle_patch))
        .with_state(state)
}

/// Start the HTTP server. This blocks until the server exits.
pub async fn run_serve(
    root: std::path::PathBuf,
    includes: Vec<String>,
    port: u16,
    interval: Duration,
    channel_capacity: usize,
) -> anyhow::Result<()> {
    let snapshot = build_snapshot(&root, &includes)?;
    let state = ServerState::new(root, includes, snapshot, channel_capacity);

    // Background poller.
    let poll_state = Arc::clone(&state);
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(interval).await;
            if let Err(error) = poll_state.poll_and_broadcast() {
                eprintln!("[vertigo-sync] poll error: {error}");
            }
        }
    });

    let app = build_router(state);

    let addr: std::net::SocketAddr = ([127, 0, 0, 1], port).into();
    eprintln!("[vertigo-sync] serving on http://{addr}");

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
    let stream = BroadcastStream::new(state.tx.subscribe()).filter_map(|result| {
        match result {
            Ok(event) => {
                let data = serde_json::to_string(&event).unwrap_or_else(|_| "{}".to_string());
                Some(Ok(Event::default().event("sync_diff").data(data)))
            }
            Err(_) => None,
        }
    });

    Sse::new(stream).keep_alive(KeepAlive::new().interval(Duration::from_secs(15)).text("ping"))
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

    Ok(Json(SyncPatchAck {
        accepted: errors.is_empty(),
        new_source_hash: new_hash,
        applied,
        errors,
    }))
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
