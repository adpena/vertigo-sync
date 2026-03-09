//! HTTP endpoints for serving parsed .rbxl / .rbxlx data.
//!
//! All endpoints share a cached `WeakDom` behind an `Arc<RwLock<...>>` so
//! repeated queries are O(1) after the initial parse. The DOM is loaded
//! lazily on the first `GET /api/rbxl/load` request.
//!
//! Endpoints:
//!   GET  /api/rbxl/load?path=<path>         — parse file and cache the DOM
//!   GET  /api/rbxl/tree                     — return full instance tree JSON
//!   GET  /api/rbxl/instance/{id}            — single instance with all props
//!   GET  /api/rbxl/query?class=X&tag=Y&name=Z — query instances
//!   GET  /api/rbxl/scripts                  — all scripts with Source
//!   GET  /api/rbxl/meshes                   — all MeshPart instances

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use axum::Router;
use axum::extract::{Path as AxumPath, Query, State};
use axum::http::StatusCode;
use axum::response::Json;
use axum::routing::get;
use rbx_dom_weak::WeakDom;
use rbx_dom_weak::types::Ref;
use serde::Deserialize;
use tokio::sync::RwLock;

use crate::rbxl::{InstanceNode, MeshEntry, RbxlLoader, SceneGraph, ScriptEntry, build_ref_map};

// ---------------------------------------------------------------------------
// Shared state for the RBXL cache layer
// ---------------------------------------------------------------------------

/// Cached state for a loaded .rbxl file. Kept behind `Arc<RwLock<...>>` so
/// the DOM is parsed once and served to many concurrent readers.
pub struct RbxlState {
    /// The parsed DOM (None until first load).
    dom: Option<WeakDom>,
    /// Lookup map from string IDs to DOM refs.
    ref_map: HashMap<String, Ref>,
    /// Path of the currently loaded file (for diagnostics).
    loaded_path: Option<PathBuf>,
}

impl RbxlState {
    pub fn new() -> Self {
        Self {
            dom: None,
            ref_map: HashMap::new(),
            loaded_path: None,
        }
    }
}

pub type SharedRbxlState = Arc<RwLock<RbxlState>>;

pub fn new_shared_rbxl_state() -> SharedRbxlState {
    Arc::new(RwLock::new(RbxlState::new()))
}

// ---------------------------------------------------------------------------
// Router
// ---------------------------------------------------------------------------

/// Build the `/api/rbxl/*` sub-router. Caller merges this into the main app.
pub fn rbxl_router(rbxl_state: SharedRbxlState) -> Router {
    Router::new()
        .route("/api/rbxl/load", get(handle_load))
        .route("/api/rbxl/tree", get(handle_tree))
        .route("/api/rbxl/instance/{id}", get(handle_instance))
        .route("/api/rbxl/query", get(handle_query))
        .route("/api/rbxl/scripts", get(handle_scripts))
        .route("/api/rbxl/meshes", get(handle_meshes))
        .with_state(rbxl_state)
}

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct LoadQuery {
    path: String,
}

/// `GET /api/rbxl/load?path=<path>` — parse an .rbxl/.rbxlx file and cache it.
async fn handle_load(
    State(state): State<SharedRbxlState>,
    Query(query): Query<LoadQuery>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let path = PathBuf::from(&query.path);

    if !path.exists() {
        return Err((
            StatusCode::NOT_FOUND,
            format!("file not found: {}", path.display()),
        ));
    }

    // Parse the file (this is the expensive part — done once).
    let dom = RbxlLoader::load_file(&path).map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("parse error: {e}"),
        )
    })?;

    let ref_map = build_ref_map(&dom);
    let instance_count = ref_map.len();

    // Cache the DOM.
    let mut lock = state.write().await;
    lock.dom = Some(dom);
    lock.ref_map = ref_map;
    lock.loaded_path = Some(path.clone());

    Ok(Json(serde_json::json!({
        "status": "loaded",
        "path": path.display().to_string(),
        "instance_count": instance_count,
    })))
}

/// `GET /api/rbxl/tree` — return the full instance tree as JSON.
async fn handle_tree(
    State(state): State<SharedRbxlState>,
) -> Result<Json<SceneGraph>, (StatusCode, String)> {
    let lock = state.read().await;
    let dom = lock.dom.as_ref().ok_or_else(|| {
        (
            StatusCode::PRECONDITION_REQUIRED,
            "no file loaded — call GET /api/rbxl/load?path=... first".to_string(),
        )
    })?;

    Ok(Json(RbxlLoader::to_scene_graph(dom)))
}

/// `GET /api/rbxl/instance/{id}` — return a single instance with full properties.
async fn handle_instance(
    State(state): State<SharedRbxlState>,
    AxumPath(id): AxumPath<String>,
) -> Result<Json<InstanceNode>, (StatusCode, String)> {
    let lock = state.read().await;
    let dom = lock.dom.as_ref().ok_or_else(|| {
        (
            StatusCode::PRECONDITION_REQUIRED,
            "no file loaded".to_string(),
        )
    })?;

    // Look up the Ref from our map.
    let inst_ref = lock
        .ref_map
        .get(&id)
        .ok_or_else(|| (StatusCode::NOT_FOUND, format!("instance not found: {id}")))?;

    RbxlLoader::get_instance_full(dom, *inst_ref)
        .ok_or_else(|| (StatusCode::NOT_FOUND, format!("ref dangling: {id}")))
        .map(Json)
}

#[derive(Deserialize)]
struct QueryParams {
    class: Option<String>,
    tag: Option<String>,
    name: Option<String>,
}

/// `GET /api/rbxl/query?class=X&tag=Y&name=Z` — query instances.
async fn handle_query(
    State(state): State<SharedRbxlState>,
    Query(params): Query<QueryParams>,
) -> Result<Json<Vec<InstanceNode>>, (StatusCode, String)> {
    let lock = state.read().await;
    let dom = lock.dom.as_ref().ok_or_else(|| {
        (
            StatusCode::PRECONDITION_REQUIRED,
            "no file loaded".to_string(),
        )
    })?;

    let results = RbxlLoader::query(
        dom,
        params.class.as_deref(),
        params.tag.as_deref(),
        params.name.as_deref(),
    );

    Ok(Json(results))
}

/// `GET /api/rbxl/scripts` — all Script/LocalScript/ModuleScript with Source.
async fn handle_scripts(
    State(state): State<SharedRbxlState>,
) -> Result<Json<Vec<ScriptEntry>>, (StatusCode, String)> {
    let lock = state.read().await;
    let dom = lock.dom.as_ref().ok_or_else(|| {
        (
            StatusCode::PRECONDITION_REQUIRED,
            "no file loaded".to_string(),
        )
    })?;

    Ok(Json(RbxlLoader::extract_scripts(dom)))
}

/// `GET /api/rbxl/meshes` — all MeshPart instances with MeshId.
async fn handle_meshes(
    State(state): State<SharedRbxlState>,
) -> Result<Json<Vec<MeshEntry>>, (StatusCode, String)> {
    let lock = state.read().await;
    let dom = lock.dom.as_ref().ok_or_else(|| {
        (
            StatusCode::PRECONDITION_REQUIRED,
            "no file loaded".to_string(),
        )
    })?;

    Ok(Json(RbxlLoader::extract_meshes(dom)))
}
