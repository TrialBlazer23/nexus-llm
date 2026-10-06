//! Minimal Hyper HTTP control-plane server for Nexus nodes.
//!
//! Serves state, model load/unload, and the model catalog on `control_port`
//! so it never fights llama-server on `api_port`.

use crate::control_plane::{
    self, build_model_catalog, ControlPlaneRequest, ControlPlaneState, ModelLoadRequest,
    ModelUnloadRequest, CONTROL_PLANE_VERSION,
};
use crate::discovery::NodeRole;
use crate::supervisor::SupervisorManager;
use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::body::Incoming;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Method, Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::net::TcpListener;
use tracing::{debug, error, info};
use uuid::Uuid;

#[derive(Clone)]
pub struct ControlPlaneServerState {
    pub node_id: Uuid,
    pub role: NodeRole,
    pub models_dir: PathBuf,
    pub api_host: String,
    pub api_port: u16,
    pub manager: SupervisorManager,
    pub allocatable_memory_mb: u64,
}

type BoxBody = Full<Bytes>;

fn json_response(status: StatusCode, body: impl serde::Serialize) -> Response<BoxBody> {
    let bytes = serde_json::to_vec(&body).unwrap_or_else(|_| b"{}".to_vec());
    Response::builder()
        .status(status)
        .header("content-type", "application/json")
        .body(Full::new(Bytes::from(bytes)))
        .unwrap_or_else(|_| Response::new(Full::new(Bytes::from_static(b"{}"))))
}

fn text_response(status: StatusCode, msg: &str) -> Response<BoxBody> {
    Response::builder()
        .status(status)
        .header("content-type", "text/plain")
        .body(Full::new(Bytes::from(msg.to_string())))
        .unwrap_or_else(|_| Response::new(Full::new(Bytes::from_static(b"error"))))
}

async fn read_json_body<T: serde::de::DeserializeOwned>(
    req: Request<Incoming>,
) -> Result<T, String> {
    let collected = req
        .collect()
        .await
        .map_err(|e| format!("body read failed: {e}"))?;
    let bytes = collected.to_bytes();
    if bytes.len() > control_plane::MAX_CONTROL_RESPONSE_BYTES {
        return Err("request body too large".to_string());
    }
    serde_json::from_slice(&bytes).map_err(|e| format!("invalid JSON: {e}"))
}

async fn handle_request(
    state: Arc<ControlPlaneServerState>,
    req: Request<Incoming>,
) -> Result<Response<BoxBody>, hyper::Error> {
    let path = req.uri().path().to_string();
    let method = req.method().clone();

    // Alias DESIGN_SPEC / IDENTIFIED_UPGRADES paths onto /nexus/control/v1/*
    let normalized = match path.as_str() {
        "/cluster/models" => "/nexus/control/v1/models".to_string(),
        "/cluster/state" => "/nexus/control/v1/state".to_string(),
        "/cluster/model/load" => "/nexus/control/v1/model/load".to_string(),
        "/cluster/model/unload" => "/nexus/control/v1/model/unload".to_string(),
        other => other.to_string(),
    };

    let response = match (method, normalized.as_str()) {
        (Method::GET, "/nexus/control/v1/models") => {
            let catalog = build_model_catalog(state.node_id, &state.models_dir);
            json_response(StatusCode::OK, catalog)
        }
        (Method::POST, "/nexus/control/v1/state") => match read_json_body::<ControlPlaneRequest>(req).await {
            Ok(_request) => {
                let active = state.manager.active_model().await;
                let ready = true;
                let inferring = state.manager.is_healthy().await;
                let body = ControlPlaneState {
                    node_id: state.node_id,
                    protocol_version: CONTROL_PLANE_VERSION,
                    role: state.role,
                    capabilities: vec!["inference".to_string(), "catalog".to_string()],
                    ready,
                    inferring,
                    rpc_ready: false,
                    allocatable_memory_mb: state.allocatable_memory_mb,
                    active_model: active,
                };
                json_response(StatusCode::OK, body)
            }
            Err(e) => text_response(StatusCode::BAD_REQUEST, &e),
        },
        (Method::POST, "/nexus/control/v1/model/load") => match read_json_body::<ModelLoadRequest>(req).await {
            Ok(request) => {
                let resp = control_plane::handle_load_model(
                    &state.manager,
                    &request,
                    &state.api_host,
                    state.api_port,
                    Some(state.models_dir.as_path()),
                )
                .await;
                let status = if resp.success {
                    StatusCode::OK
                } else {
                    StatusCode::INTERNAL_SERVER_ERROR
                };
                json_response(status, resp)
            }
            Err(e) => text_response(StatusCode::BAD_REQUEST, &e),
        },
        (Method::POST, "/nexus/control/v1/model/unload") => {
            match read_json_body::<ModelUnloadRequest>(req).await {
                Ok(request) => {
                    let resp = control_plane::handle_unload_model(&state.manager, &request).await;
                    let status = if resp.success {
                        StatusCode::OK
                    } else {
                        StatusCode::INTERNAL_SERVER_ERROR
                    };
                    json_response(status, resp)
                }
                Err(e) => text_response(StatusCode::BAD_REQUEST, &e),
            }
        }
        _ => text_response(StatusCode::NOT_FOUND, "not found"),
    };

    Ok(response)
}

/// Bind and serve the control plane on `bind_addr` until the task is aborted.
pub async fn serve_control_plane(
    bind_addr: SocketAddr,
    state: ControlPlaneServerState,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let listener = TcpListener::bind(bind_addr).await?;
    info!("Control-plane HTTP listening on {}", bind_addr);
    let state = Arc::new(state);

    loop {
        let (stream, peer) = listener.accept().await?;
        debug!("Control-plane connection from {}", peer);
        let io = TokioIo::new(stream);
        let state = state.clone();
        tokio::spawn(async move {
            let service = service_fn(move |req| handle_request(state.clone(), req));
            if let Err(err) = http1::Builder::new().serve_connection(io, service).await {
                error!("Control-plane connection error: {}", err);
            }
        });
    }
}

/// Spawn the control-plane server as a background task. Returns the JoinHandle.
pub fn spawn_control_plane(
    bind_addr: SocketAddr,
    state: ControlPlaneServerState,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        if let Err(e) = serve_control_plane(bind_addr, state).await {
            error!("Control-plane server exited: {}", e);
        }
    })
}
