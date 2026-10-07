//! Minimal HTTP control-plane listener for remote model orchestration.
//!
//! Binds a dedicated `network.control_port` (default 9998) so it never collides
//! with llama-server on `api_port` (8080). Routes match the existing reqwest
//! clients in `control_plane.rs`.

use crate::config::NexusConfig;
use crate::control_plane::{
    build_control_plane_state, build_model_catalog, handle_load_model, handle_unload_model,
    ControlPlaneRequest, ModelLoadRequest, ModelUnloadRequest, PairRequest, PairResponse,
    CONTROL_PLANE_VERSION, MAX_CONTROL_RESPONSE_BYTES,
};
use crate::discovery::{DiscoveryService, NodeRole, StatusFlags};
use crate::node_identity::NodeIdentity;
use crate::supervisor::SupervisorManager;
use crate::sysinfo::SystemProfile;
use crate::trust_auth::{
    authorize_privileged_signer, verify_control_request, AuthError, NonceCache,
};
use bytes::Bytes;
use http_body_util::{BodyExt, Full, Limited};
use hyper::body::Incoming;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Method, Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use std::collections::HashMap;
use std::convert::Infallible;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::net::TcpListener;
use tracing::{error, info, warn};
use uuid::Uuid;

/// Shared runtime state for the control-plane HTTP server.
#[derive(Clone)]
pub struct ControlPlaneContext {
    pub node_id: Uuid,
    pub role: NodeRole,
    pub capabilities: Vec<String>,
    pub supervisor: SupervisorManager,
    pub api_host: String,
    pub api_port: u16,
    pub binary_path: PathBuf,
    pub discovery: Option<Arc<DiscoveryService>>,
    pub rpc_ready: bool,
    pub use_mmap: bool,
    pub memory_budget_percent: u8,
    pub identity: Arc<NodeIdentity>,
    pub config: Arc<std::sync::RwLock<NexusConfig>>,
    pub config_path: PathBuf,
    pub nonce_cache: Arc<Mutex<NonceCache>>,
    pair_attempts: Arc<Mutex<HashMap<SocketAddr, (u32, Instant)>>>,
}

impl ControlPlaneContext {
    pub fn new(
        node_id: Uuid,
        role: NodeRole,
        supervisor: SupervisorManager,
        api_host: impl Into<String>,
        api_port: u16,
        binary_path: PathBuf,
        identity: Arc<NodeIdentity>,
        config: Arc<std::sync::RwLock<NexusConfig>>,
        config_path: PathBuf,
    ) -> Self {
        Self {
            node_id,
            role,
            capabilities: vec!["inference".to_string(), "catalog".to_string()],
            supervisor,
            api_host: api_host.into(),
            api_port,
            binary_path,
            discovery: None,
            rpc_ready: false,
            use_mmap: true,
            memory_budget_percent: 75,
            identity,
            config,
            config_path,
            nonce_cache: Arc::new(Mutex::new(NonceCache::default())),
            pair_attempts: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    pub fn with_discovery(mut self, discovery: Arc<DiscoveryService>) -> Self {
        self.discovery = Some(discovery);
        self
    }

    pub fn with_rpc_ready(mut self, rpc_ready: bool) -> Self {
        self.rpc_ready = rpc_ready;
        self
    }

    pub fn with_capabilities(mut self, capabilities: Vec<String>) -> Self {
        self.capabilities = capabilities;
        self
    }

    pub fn with_memory_policy(mut self, use_mmap: bool, memory_budget_percent: u8) -> Self {
        self.use_mmap = use_mmap;
        self.memory_budget_percent = memory_budget_percent;
        self
    }
}

/// Bind `addr` and serve control-plane routes until the task is aborted.
pub async fn serve(addr: SocketAddr, ctx: Arc<ControlPlaneContext>) -> std::io::Result<()> {
    let listener = TcpListener::bind(addr).await?;
    info!("Control-plane HTTP server listening on {}", addr);

    loop {
        let (stream, peer) = listener.accept().await?;
        let ctx = ctx.clone();
        tokio::spawn(async move {
            let io = TokioIo::new(stream);
            let service = service_fn(move |req| {
                let ctx = ctx.clone();
                async move { Ok::<_, Infallible>(route(req, ctx, peer).await) }
            });
            if let Err(err) = http1::Builder::new().serve_connection(io, service).await {
                warn!("Control-plane connection from {} error: {}", peer, err);
            }
        });
    }
}

/// Spawn the control-plane server on a background task. Returns the join handle.
pub fn spawn(
    addr: SocketAddr,
    ctx: Arc<ControlPlaneContext>,
) -> tokio::task::JoinHandle<std::io::Result<()>> {
    tokio::spawn(async move { serve(addr, ctx).await })
}

/// Bind an ephemeral port (useful for tests). Returns (bound address, join handle).
pub async fn spawn_ephemeral(
    ctx: Arc<ControlPlaneContext>,
) -> std::io::Result<(SocketAddr, tokio::task::JoinHandle<std::io::Result<()>>)> {
    let listener = TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], 0))).await?;
    let addr = listener.local_addr()?;
    let handle = tokio::spawn(async move {
        info!("Control-plane HTTP server listening on {}", addr);
        loop {
            let (stream, peer) = listener.accept().await?;
            let ctx = ctx.clone();
            tokio::spawn(async move {
                let io = TokioIo::new(stream);
                let service = service_fn(move |req| {
                    let ctx = ctx.clone();
                    async move { Ok::<_, Infallible>(route(req, ctx, peer).await) }
                });
                if let Err(err) = http1::Builder::new().serve_connection(io, service).await {
                    warn!("Control-plane connection from {} error: {}", peer, err);
                }
            });
        }
    });
    Ok((addr, handle))
}

async fn route(
    req: Request<Incoming>,
    ctx: Arc<ControlPlaneContext>,
    peer: SocketAddr,
) -> Response<Full<Bytes>> {
    let path = req.uri().path().to_string();
    let method = req.method().clone();

    match (method, path.as_str()) {
        (Method::GET, "/nexus/control/v1/models") | (Method::GET, "/cluster/models") => {
            let models_dir = ctx
                .config
                .read()
                .map(|c| PathBuf::from(&c.node.models_dir))
                .unwrap_or_else(|_| PathBuf::from("models"));
            let catalog = build_model_catalog(ctx.node_id, &models_dir);
            json_response(StatusCode::OK, &catalog)
        }
        (Method::POST, "/nexus/control/v1/state") => handle_state(req, ctx, peer).await,
        (Method::POST, "/nexus/control/v1/model/load") => handle_load(req, ctx, peer).await,
        (Method::POST, "/nexus/control/v1/model/unload") => handle_unload(req, ctx, peer).await,
        (Method::POST, "/nexus/control/v1/pair") => handle_pair(req, ctx, peer).await,
        _ => json_response(
            StatusCode::NOT_FOUND,
            &serde_json::json!({"error": "not found"}),
        ),
    }
}

async fn read_body(req: Request<Incoming>) -> Result<Vec<u8>, Response<Full<Bytes>>> {
    let limited = Limited::new(req.into_body(), MAX_CONTROL_RESPONSE_BYTES);
    let collected = match limited.collect().await {
        Ok(c) => c.to_bytes(),
        Err(_) => {
            return Err(json_response(
                StatusCode::PAYLOAD_TOO_LARGE,
                &serde_json::json!({"error": "request body too large"}),
            ));
        }
    };
    Ok(collected.to_vec())
}

fn json_response(status: StatusCode, value: &impl serde::Serialize) -> Response<Full<Bytes>> {
    let body = serde_json::to_vec(value).unwrap_or_else(|_| b"{}".to_vec());
    Response::builder()
        .status(status)
        .header(hyper::header::CONTENT_TYPE, "application/json")
        .body(Full::new(Bytes::from(body)))
        .unwrap_or_else(|_| Response::new(Full::new(Bytes::from_static(b"{}"))))
}

fn auth_error_response(err: AuthError) -> Response<Full<Bytes>> {
    let status = match err {
        AuthError::PairingRequired | AuthError::SignerNotAuthorized => StatusCode::FORBIDDEN,
        AuthError::StaleTimestamp | AuthError::ReplayNonce | AuthError::BadSignature => {
            StatusCode::UNAUTHORIZED
        }
        _ => StatusCode::BAD_REQUEST,
    };
    json_response(status, &serde_json::json!({"error": err.to_string()}))
}

async fn handle_state(
    req: Request<Incoming>,
    ctx: Arc<ControlPlaneContext>,
    peer: SocketAddr,
) -> Response<Full<Bytes>> {
    let headers = req.headers().clone();
    let body = match read_body(req).await {
        Ok(b) => b,
        Err(resp) => return resp,
    };
    let security = ctx.config.read().expect("config lock").network.security.clone();
    if let Err(err) = verify_control_request(
        &headers,
        "POST",
        "/nexus/control/v1/state",
        &body,
        &security,
        &ctx.nonce_cache,
        None,
        true,
        Some(peer.ip()),
    ) {
        return auth_error_response(err);
    }

    let _request: ControlPlaneRequest = match serde_json::from_slice(&body) {
        Ok(r) => r,
        Err(e) => {
            return json_response(
                StatusCode::BAD_REQUEST,
                &serde_json::json!({"error": format!("invalid JSON: {e}")}),
            );
        }
    };

    let profile = SystemProfile::probe();
    let allocatable = profile
        .max_allowed_memory_bytes_pct(ctx.memory_budget_percent)
        / (1024 * 1024);
    let state = build_control_plane_state(
        ctx.node_id,
        ctx.role,
        ctx.capabilities.clone(),
        &ctx.supervisor,
        allocatable,
        ctx.rpc_ready,
        Some(ctx.identity.public_key_hex()),
    )
    .await;
    json_response(StatusCode::OK, &state)
}

async fn handle_load(
    req: Request<Incoming>,
    ctx: Arc<ControlPlaneContext>,
    peer: SocketAddr,
) -> Response<Full<Bytes>> {
    let headers = req.headers().clone();
    let body = match read_body(req).await {
        Ok(b) => b,
        Err(resp) => return resp,
    };
    let security = ctx.config.read().expect("config lock").network.security.clone();
    let auth = match verify_control_request(
        &headers,
        "POST",
        "/nexus/control/v1/model/load",
        &body,
        &security,
        &ctx.nonce_cache,
        None,
        false,
        Some(peer.ip()),
    ) {
        Ok(a) => a,
        Err(err) => return auth_error_response(err),
    };
    if let Err(err) = authorize_privileged_signer(&security, auth.signer_id) {
        return auth_error_response(err);
    }

    let request: ModelLoadRequest = match serde_json::from_slice(&body) {
        Ok(r) => r,
        Err(e) => {
            return json_response(
                StatusCode::BAD_REQUEST,
                &serde_json::json!({"error": format!("invalid JSON: {e}")}),
            );
        }
    };

    let response = handle_load_model(
        &ctx.supervisor,
        &request,
        &ctx.api_host,
        ctx.api_port,
        &ctx.binary_path,
        ctx.use_mmap,
        ctx.memory_budget_percent,
    )
    .await;

    if response.success {
        if let Some(discovery) = &ctx.discovery {
            discovery.set_active_model(&response.active_model).await;
            discovery.set_status_flags(StatusFlags::READY).await;
        }
        info!(
            "Control-plane loaded model '{}' for requester {}",
            response.active_model, request.requester_id
        );
    } else {
        error!(
            "Control-plane model load failed: {:?}",
            response.error_message
        );
    }

    json_response(StatusCode::OK, &response)
}

async fn handle_unload(
    req: Request<Incoming>,
    ctx: Arc<ControlPlaneContext>,
    peer: SocketAddr,
) -> Response<Full<Bytes>> {
    let headers = req.headers().clone();
    let body = match read_body(req).await {
        Ok(b) => b,
        Err(resp) => return resp,
    };
    let security = ctx.config.read().expect("config lock").network.security.clone();
    let auth = match verify_control_request(
        &headers,
        "POST",
        "/nexus/control/v1/model/unload",
        &body,
        &security,
        &ctx.nonce_cache,
        None,
        false,
        Some(peer.ip()),
    ) {
        Ok(a) => a,
        Err(err) => return auth_error_response(err),
    };
    if let Err(err) = authorize_privileged_signer(&security, auth.signer_id) {
        return auth_error_response(err);
    }

    let request: ModelUnloadRequest = match serde_json::from_slice(&body) {
        Ok(r) => r,
        Err(e) => {
            return json_response(
                StatusCode::BAD_REQUEST,
                &serde_json::json!({"error": format!("invalid JSON: {e}")}),
            );
        }
    };

    let response = handle_unload_model(&ctx.supervisor, &request).await;
    if response.success {
        if let Some(discovery) = &ctx.discovery {
            discovery.set_active_model("").await;
            discovery.set_status_flags(StatusFlags(0)).await;
        }
        info!(
            "Control-plane unloaded model for requester {}",
            request.requester_id
        );
    }

    json_response(StatusCode::OK, &response)
}

fn allow_pair_attempt(ctx: &ControlPlaneContext, peer: SocketAddr) -> bool {
    let mut map = ctx.pair_attempts.lock().expect("pair attempts lock");
    let now = Instant::now();
    map.retain(|_, (_, ts)| now.duration_since(*ts) < Duration::from_secs(60));
    let entry = map.entry(peer).or_insert((0, now));
    if entry.0 >= 10 {
        return false;
    }
    entry.0 += 1;
    entry.1 = now;
    true
}

async fn handle_pair(
    req: Request<Incoming>,
    ctx: Arc<ControlPlaneContext>,
    peer: SocketAddr,
) -> Response<Full<Bytes>> {
    if !allow_pair_attempt(&ctx, peer) {
        return json_response(
            StatusCode::TOO_MANY_REQUESTS,
            &serde_json::json!({"error": "too many pairing attempts"}),
        );
    }

    let headers = req.headers().clone();
    let body = match read_body(req).await {
        Ok(b) => b,
        Err(resp) => return resp,
    };

    let pair_req: PairRequest = match serde_json::from_slice(&body) {
        Ok(r) => r,
        Err(e) => {
            return json_response(
                StatusCode::BAD_REQUEST,
                &serde_json::json!({"error": format!("invalid JSON: {e}")}),
            );
        }
    };

    let security = ctx.config.read().expect("config lock").network.security.clone();
    if let Err(err) = verify_control_request(
        &headers,
        "POST",
        "/nexus/control/v1/pair",
        &body,
        &security,
        &ctx.nonce_cache,
        Some(&pair_req.requester_public_key),
        false,
        Some(peer.ip()),
    ) {
        return auth_error_response(err);
    }

    if pair_req.protocol_version != CONTROL_PLANE_VERSION {
        return json_response(
            StatusCode::BAD_REQUEST,
            &serde_json::json!({"error": "protocol mismatch"}),
        );
    }

    let unix_now = crate::trust_auth::unix_timestamp_now() as u64;
    if !ctx.identity.verify_pairing_code_at(unix_now, &pair_req.pairing_code) {
        return json_response(
            StatusCode::FORBIDDEN,
            &serde_json::json!({"error": "invalid pairing code"}),
        );
    }

    {
        let mut config = ctx.config.write().expect("config lock");
        config
            .network
            .security
            .record_pair(pair_req.requester_id, pair_req.requester_public_key.clone());
        if let Err(e) = config.save_to_path(&ctx.config_path) {
            warn!("Failed to persist pairing to config: {}", e);
        }
    }

    let response = PairResponse {
        protocol_version: CONTROL_PLANE_VERSION,
        success: true,
        node_id: ctx.node_id,
        public_key: ctx.identity.public_key_hex(),
        message: "paired".to_string(),
    };
    json_response(StatusCode::OK, &response)
}
