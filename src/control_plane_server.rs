//! Minimal HTTP control-plane listener for remote model orchestration.
//!
//! Binds a dedicated `network.control_port` (default 9998) so it never collides
//! with llama-server on `api_port` (8080). Routes match the existing reqwest
//! clients in `control_plane.rs`.

use crate::config::NexusConfig;
use crate::control_plane::{
    blob_url, build_control_plane_state_with_host, build_model_catalog, handle_unload_model,
    BlobFetchRequest, BlobFetchResponse, ControlPlaneRequest, ModelLoadRequest, ModelUnloadRequest,
    PairRequest, PairResponse, CONTROL_PLANE_VERSION, MAX_CONTROL_RESPONSE_BYTES,
};
use crate::discovery::{DiscoveryService, NodeRole, StatusFlags};
use crate::downloader::{DownloadAuth, ModelDownloader};
use crate::node_identity::NodeIdentity;
use crate::store::ModelIndex;
use crate::supervisor::SupervisorManager;
use crate::sysinfo::SystemProfile;
use crate::trust_auth::{
    authorize_privileged_signer, verify_control_request, AuthError, NonceCache,
};
use bytes::Bytes;
use futures_util::stream::unfold;
use http_body_util::{BodyExt, Full, Limited, StreamBody};
use hyper::body::{Frame, Incoming};
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Method, Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use std::collections::HashMap;
use std::convert::Infallible;
use std::io::SeekFrom;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncSeekExt};
use tracing::{error, info, warn};
use uuid::Uuid;

type RespBody = http_body_util::combinators::UnsyncBoxBody<Bytes, std::io::Error>;

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
    pub task_store: Arc<crate::task::TaskStore>,
    pub kb_store: Arc<crate::kb::KnowledgeStore>,
}

impl ControlPlaneContext {
    // Continuous: keep flat ctor; reshaping into a builder is out of scope for hygiene.
    #[allow(clippy::too_many_arguments)]
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
        let task_store = Arc::new(
            crate::task::TaskStore::load_or_create(crate::task::TaskStore::default_path())
                .unwrap_or_else(|_| {
                    crate::task::TaskStore::load_or_create(
                        std::env::temp_dir().join("nexus_tasks.json"),
                    )
                    .expect("fallback task store")
                }),
        );
        let kb_store = Arc::new(
            crate::kb::KnowledgeStore::open(crate::kb::KnowledgeStore::default_path())
                .or_else(|_| {
                    let unique_name = format!("nexus_kb_{}.redb", Uuid::new_v4());
                    crate::kb::KnowledgeStore::open(std::env::temp_dir().join(unique_name))
                })
                .expect("fallback kb store"),
        );
        Self {
            node_id,
            role,
            capabilities: {
                let mut caps = vec![
                    "inference".to_string(),
                    "catalog".to_string(),
                    "blob".to_string(),
                    "embeddings".to_string(),
                    "kb".to_string(),
                ];
                if config
                    .read()
                    .map(|c| c.inference.moe.enabled)
                    .unwrap_or(true)
                {
                    caps.push("moe_stream".to_string());
                }
                caps
            },
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
            task_store,
            kb_store,
        }
    }

    pub fn with_task_store(mut self, task_store: Arc<crate::task::TaskStore>) -> Self {
        self.task_store = task_store;
        self
    }

    pub fn with_kb_store(mut self, kb_store: Arc<crate::kb::KnowledgeStore>) -> Self {
        self.kb_store = kb_store;
        self
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
    let listener = tokio::net::TcpListener::bind(addr).await?;
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
    let listener = tokio::net::TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], 0))).await?;
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
) -> Response<RespBody> {
    let path = req.uri().path().to_string();
    let method = req.method().clone();

    match (method.clone(), path.as_str()) {
        (Method::GET, "/nexus/control/v1/models") | (Method::GET, "/cluster/models") => {
            let models_dir = ctx
                .config
                .read()
                .map(|c| PathBuf::from(&c.node.models_dir))
                .unwrap_or_else(|_| PathBuf::from("models"));
            let catalog = build_model_catalog(ctx.node_id, &models_dir);
            json_response(StatusCode::OK, &catalog)
        }
        (Method::GET, p) if p.starts_with("/nexus/control/v1/blob/") => {
            let digest = p.trim_start_matches("/nexus/control/v1/blob/");
            handle_blob_get(req, ctx, peer, digest).await
        }
        (Method::POST, "/nexus/control/v1/blob/fetch") => handle_blob_fetch(req, ctx, peer).await,
        (Method::POST, "/nexus/control/v1/state") => handle_state(req, ctx, peer).await,
        (Method::POST, "/nexus/control/v1/model/load") => handle_load(req, ctx, peer).await,
        (Method::POST, "/nexus/control/v1/model/unload") => handle_unload(req, ctx, peer).await,
        (Method::POST, "/nexus/control/v1/agent/message") => {
            handle_agent_message_route(req, ctx, peer).await
        }
        (Method::POST, "/nexus/control/v1/kb/store") => handle_kb_store_route(req, ctx, peer).await,
        (Method::POST, "/nexus/control/v1/kb/query") => handle_kb_query_route(req, ctx, peer).await,
        (Method::POST, "/nexus/control/v1/kb/sync/manifest") => {
            handle_kb_manifest_route(req, ctx, peer).await
        }
        (Method::POST, "/nexus/control/v1/kb/sync/pull") => {
            handle_kb_pull_route(req, ctx, peer).await
        }
        (Method::POST, "/nexus/control/v1/kb/sync/push") => {
            handle_kb_push_route(req, ctx, peer).await
        }
        (Method::POST, "/nexus/control/v1/pair") => handle_pair(req, ctx, peer).await,
        _ => json_response(
            StatusCode::NOT_FOUND,
            &serde_json::json!({"error": "not found"}),
        ),
    }
}

// Err carries a ready HTTP response; boxing would add noise without shrinking the hot path.
#[allow(clippy::result_large_err)]
async fn read_body(req: Request<Incoming>) -> Result<Vec<u8>, Response<RespBody>> {
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

fn full_body(bytes: Bytes) -> RespBody {
    Full::new(bytes)
        .map_err(|never| match never {})
        .boxed_unsync()
}

fn json_response(status: StatusCode, value: &impl serde::Serialize) -> Response<RespBody> {
    let body = serde_json::to_vec(value).unwrap_or_else(|_| b"{}".to_vec());
    Response::builder()
        .status(status)
        .header(hyper::header::CONTENT_TYPE, "application/json")
        .body(full_body(Bytes::from(body)))
        .unwrap_or_else(|_| Response::new(full_body(Bytes::from_static(b"{}"))))
}

fn auth_error_response(err: AuthError) -> Response<RespBody> {
    let status = match err {
        AuthError::PairingRequired | AuthError::SignerNotAuthorized => StatusCode::FORBIDDEN,
        AuthError::StaleTimestamp | AuthError::ReplayNonce | AuthError::BadSignature => {
            StatusCode::UNAUTHORIZED
        }
        _ => StatusCode::BAD_REQUEST,
    };
    json_response(status, &serde_json::json!({"error": err.to_string()}))
}

fn parse_byte_range(header: Option<&str>, file_len: u64) -> Option<(u64, u64)> {
    let header = header?;
    let header = header.strip_prefix("bytes=")?;
    let (start_s, end_s) = header.split_once('-')?;
    let start: u64 = if start_s.is_empty() {
        return None;
    } else {
        start_s.parse().ok()?
    };
    let end: u64 = if end_s.is_empty() {
        file_len.saturating_sub(1)
    } else {
        end_s.parse().ok()?
    };
    if start > end || start >= file_len {
        return None;
    }
    Some((start, end.min(file_len.saturating_sub(1))))
}

async fn handle_blob_get(
    req: Request<Incoming>,
    ctx: Arc<ControlPlaneContext>,
    peer: SocketAddr,
    digest: &str,
) -> Response<RespBody> {
    let path = format!("/nexus/control/v1/blob/{}", digest.trim().to_lowercase());
    let headers = req.headers().clone();
    let range_header = headers
        .get(hyper::header::RANGE)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string());

    let security = ctx
        .config
        .read()
        .expect("config lock")
        .network
        .security
        .clone();
    let auth = match verify_control_request(
        &headers,
        "GET",
        &path,
        &[],
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

    let models_dir = ctx
        .config
        .read()
        .map(|c| PathBuf::from(&c.node.models_dir))
        .unwrap_or_else(|_| PathBuf::from("models"));
    let index = match ModelIndex::reconcile_default(&models_dir) {
        Ok(i) => i,
        Err(e) => {
            return json_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                &serde_json::json!({"error": e.to_string()}),
            );
        }
    };
    let Some(entry) = index.find_by_digest(digest) else {
        return json_response(
            StatusCode::NOT_FOUND,
            &serde_json::json!({"error": "blob not found"}),
        );
    };
    let file_path = entry.path.clone();
    let file_len = entry.size_bytes;

    let (start, end, status) = match parse_byte_range(range_header.as_deref(), file_len) {
        Some((s, e)) => (s, e, StatusCode::PARTIAL_CONTENT),
        None if range_header.is_some() => {
            return Response::builder()
                .status(StatusCode::RANGE_NOT_SATISFIABLE)
                .header(hyper::header::CONTENT_RANGE, format!("bytes */{file_len}"))
                .body(full_body(Bytes::new()))
                .unwrap_or_else(|_| Response::new(full_body(Bytes::new())));
        }
        None => (0, file_len.saturating_sub(1), StatusCode::OK),
    };
    let content_len = end.saturating_sub(start).saturating_add(1);

    let mut file = match tokio::fs::File::open(&file_path).await {
        Ok(f) => f,
        Err(e) => {
            return json_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                &serde_json::json!({"error": e.to_string()}),
            );
        }
    };
    if let Err(e) = file.seek(SeekFrom::Start(start)).await {
        return json_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            &serde_json::json!({"error": e.to_string()}),
        );
    }

    let stream = unfold((file, content_len), |(mut file, remaining)| async move {
        if remaining == 0 {
            return None;
        }
        let to_read = remaining.min(64 * 1024) as usize;
        let mut buf = vec![0u8; to_read];
        match file.read(&mut buf).await {
            Ok(0) => None,
            Ok(n) => {
                buf.truncate(n);
                Some((
                    Ok::<_, std::io::Error>(Frame::data(Bytes::from(buf))),
                    (file, remaining - n as u64),
                ))
            }
            Err(e) => Some((Err(e), (file, 0))),
        }
    });
    let body = StreamBody::new(stream).boxed_unsync();

    let mut builder = Response::builder()
        .status(status)
        .header(hyper::header::ACCEPT_RANGES, "bytes")
        .header(hyper::header::CONTENT_LENGTH, content_len.to_string())
        .header(hyper::header::CONTENT_TYPE, "application/octet-stream");
    if status == StatusCode::PARTIAL_CONTENT {
        builder = builder.header(
            hyper::header::CONTENT_RANGE,
            format!("bytes {start}-{end}/{file_len}"),
        );
    }
    builder
        .body(body)
        .unwrap_or_else(|_| Response::new(full_body(Bytes::new())))
}

async fn handle_blob_fetch(
    req: Request<Incoming>,
    ctx: Arc<ControlPlaneContext>,
    peer: SocketAddr,
) -> Response<RespBody> {
    let headers = req.headers().clone();
    let body = match read_body(req).await {
        Ok(b) => b,
        Err(resp) => return resp,
    };
    let security = ctx
        .config
        .read()
        .expect("config lock")
        .network
        .security
        .clone();
    let auth = match verify_control_request(
        &headers,
        "POST",
        "/nexus/control/v1/blob/fetch",
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

    let request: BlobFetchRequest = match serde_json::from_slice(&body) {
        Ok(r) => r,
        Err(e) => {
            return json_response(
                StatusCode::BAD_REQUEST,
                &serde_json::json!({"error": format!("invalid JSON: {e}")}),
            );
        }
    };

    let digest = request.digest.trim().to_lowercase();
    let digest_for_msg = digest.clone();
    let source = request.source_base_url.clone();
    let models_dir = ctx
        .config
        .read()
        .map(|c| PathBuf::from(&c.node.models_dir))
        .unwrap_or_else(|_| PathBuf::from("models"));
    let dest_name = format!("{digest}.gguf");
    let dest = models_dir.join(&dest_name);
    let identity = ctx.identity.clone();
    let signer_id = ctx
        .config
        .read()
        .ok()
        .and_then(|c| c.node_uuid().ok())
        .unwrap_or(ctx.node_id);
    let use_auth = ctx
        .config
        .read()
        .map(|c| c.network.security.pairing_enforced())
        .unwrap_or(false);
    let url = match blob_url(&source, &digest) {
        Ok(u) => u,
        Err(e) => {
            return json_response(
                StatusCode::BAD_REQUEST,
                &serde_json::json!({"error": e.to_string()}),
            );
        }
    };

    tokio::spawn(async move {
        let downloader = ModelDownloader::new();
        let auth = if use_auth {
            Some(DownloadAuth {
                identity,
                signer_id,
            })
        } else {
            None
        };
        match downloader
            .download_authenticated(&url, &dest, Some(&digest), auth.as_ref(), |_| {})
            .await
        {
            Ok(()) => {
                let _ = ModelIndex::reconcile_default(&models_dir);
                info!("Background blob fetch completed for {}", digest);
            }
            Err(e) => error!("Background blob fetch failed for {}: {}", digest, e),
        }
    });

    json_response(
        StatusCode::ACCEPTED,
        &BlobFetchResponse {
            protocol_version: CONTROL_PLANE_VERSION,
            accepted: true,
            message: format!("fetch of {digest_for_msg} accepted"),
        },
    )
}

async fn handle_state(
    req: Request<Incoming>,
    ctx: Arc<ControlPlaneContext>,
    peer: SocketAddr,
) -> Response<RespBody> {
    let headers = req.headers().clone();
    let body = match read_body(req).await {
        Ok(b) => b,
        Err(resp) => return resp,
    };
    let security = ctx
        .config
        .read()
        .expect("config lock")
        .network
        .security
        .clone();
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
    let allocatable =
        profile.max_allowed_memory_bytes_pct(ctx.memory_budget_percent) / (1024 * 1024);
    let state = build_control_plane_state_with_host(
        ctx.node_id,
        ctx.role,
        ctx.capabilities.clone(),
        &ctx.supervisor,
        allocatable,
        ctx.rpc_ready,
        Some(ctx.identity.public_key_hex()),
        &ctx.api_host,
    )
    .await;
    json_response(StatusCode::OK, &state)
}

async fn handle_load(
    req: Request<Incoming>,
    ctx: Arc<ControlPlaneContext>,
    peer: SocketAddr,
) -> Response<RespBody> {
    let headers = req.headers().clone();
    let body = match read_body(req).await {
        Ok(b) => b,
        Err(resp) => return resp,
    };
    let security = ctx
        .config
        .read()
        .expect("config lock")
        .network
        .security
        .clone();
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

    let moe = ctx
        .config
        .read()
        .map(|c| c.inference.moe.clone())
        .unwrap_or_default();
    let response = crate::control_plane::handle_load_model_with_moe(
        &ctx.supervisor,
        &request,
        &ctx.api_host,
        ctx.api_port,
        &ctx.binary_path,
        ctx.use_mmap,
        ctx.memory_budget_percent,
        &moe,
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
) -> Response<RespBody> {
    let headers = req.headers().clone();
    let body = match read_body(req).await {
        Ok(b) => b,
        Err(resp) => return resp,
    };
    let security = ctx
        .config
        .read()
        .expect("config lock")
        .network
        .security
        .clone();
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

async fn handle_agent_message_route(
    req: Request<Incoming>,
    ctx: Arc<ControlPlaneContext>,
    peer: SocketAddr,
) -> Response<RespBody> {
    let headers = req.headers().clone();
    let body = match read_body(req).await {
        Ok(b) => b,
        Err(resp) => return resp,
    };
    let security = ctx
        .config
        .read()
        .expect("config lock")
        .network
        .security
        .clone();
    let auth = match verify_control_request(
        &headers,
        "POST",
        "/nexus/control/v1/agent/message",
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

    let request: crate::control_plane::AgentTaskMessage = match serde_json::from_slice(&body) {
        Ok(r) => r,
        Err(e) => {
            return json_response(
                StatusCode::BAD_REQUEST,
                &serde_json::json!({"error": format!("invalid JSON: {e}")}),
            );
        }
    };

    let response = crate::control_plane::handle_agent_message(
        &ctx.task_store,
        &ctx.supervisor,
        &request,
        &ctx.api_host,
    )
    .await;

    json_response(StatusCode::OK, &response)
}

async fn handle_kb_store_route(
    req: Request<Incoming>,
    ctx: Arc<ControlPlaneContext>,
    peer: SocketAddr,
) -> Response<RespBody> {
    let headers = req.headers().clone();
    let body = match read_body(req).await {
        Ok(b) => b,
        Err(resp) => return resp,
    };
    let security = ctx
        .config
        .read()
        .expect("config lock")
        .network
        .security
        .clone();
    let auth = match verify_control_request(
        &headers,
        "POST",
        "/nexus/control/v1/kb/store",
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

    let request: crate::control_plane::KbStoreRequest = match serde_json::from_slice(&body) {
        Ok(r) => r,
        Err(e) => {
            return json_response(
                StatusCode::BAD_REQUEST,
                &serde_json::json!({"error": format!("invalid JSON: {e}")}),
            );
        }
    };

    let response = crate::control_plane::handle_kb_store(&ctx.kb_store, &request).await;
    json_response(StatusCode::OK, &response)
}

async fn handle_kb_query_route(
    req: Request<Incoming>,
    ctx: Arc<ControlPlaneContext>,
    peer: SocketAddr,
) -> Response<RespBody> {
    let headers = req.headers().clone();
    let body = match read_body(req).await {
        Ok(b) => b,
        Err(resp) => return resp,
    };
    let security = ctx
        .config
        .read()
        .expect("config lock")
        .network
        .security
        .clone();
    let auth = match verify_control_request(
        &headers,
        "POST",
        "/nexus/control/v1/kb/query",
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

    let request: crate::control_plane::KbQueryRequest = match serde_json::from_slice(&body) {
        Ok(r) => r,
        Err(e) => {
            return json_response(
                StatusCode::BAD_REQUEST,
                &serde_json::json!({"error": format!("invalid JSON: {e}")}),
            );
        }
    };

    let response = crate::control_plane::handle_kb_query(&ctx.kb_store, &request).await;
    json_response(StatusCode::OK, &response)
}

async fn handle_kb_manifest_route(
    req: Request<Incoming>,
    ctx: Arc<ControlPlaneContext>,
    peer: SocketAddr,
) -> Response<RespBody> {
    let headers = req.headers().clone();
    let body = match read_body(req).await {
        Ok(b) => b,
        Err(resp) => return resp,
    };
    let security = ctx
        .config
        .read()
        .expect("config lock")
        .network
        .security
        .clone();
    let auth = match verify_control_request(
        &headers,
        "POST",
        "/nexus/control/v1/kb/sync/manifest",
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

    let request: crate::control_plane::KbManifestRequest = match serde_json::from_slice(&body) {
        Ok(r) => r,
        Err(e) => {
            return json_response(
                StatusCode::BAD_REQUEST,
                &serde_json::json!({"error": format!("invalid JSON: {e}")}),
            );
        }
    };

    let response =
        crate::control_plane::handle_kb_manifest(&ctx.kb_store, ctx.node_id, &request).await;
    json_response(StatusCode::OK, &response)
}

async fn handle_kb_pull_route(
    req: Request<Incoming>,
    ctx: Arc<ControlPlaneContext>,
    peer: SocketAddr,
) -> Response<RespBody> {
    let headers = req.headers().clone();
    let body = match read_body(req).await {
        Ok(b) => b,
        Err(resp) => return resp,
    };
    let security = ctx
        .config
        .read()
        .expect("config lock")
        .network
        .security
        .clone();
    let auth = match verify_control_request(
        &headers,
        "POST",
        "/nexus/control/v1/kb/sync/pull",
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

    let request: crate::kb::sync::KbSyncPullRequest = match serde_json::from_slice(&body) {
        Ok(r) => r,
        Err(e) => {
            return json_response(
                StatusCode::BAD_REQUEST,
                &serde_json::json!({"error": format!("invalid JSON: {e}")}),
            );
        }
    };

    let response = crate::control_plane::handle_kb_pull(&ctx.kb_store, &request).await;
    json_response(StatusCode::OK, &response)
}

async fn handle_kb_push_route(
    req: Request<Incoming>,
    ctx: Arc<ControlPlaneContext>,
    peer: SocketAddr,
) -> Response<RespBody> {
    let headers = req.headers().clone();
    let body = match read_body(req).await {
        Ok(b) => b,
        Err(resp) => return resp,
    };
    let security = ctx
        .config
        .read()
        .expect("config lock")
        .network
        .security
        .clone();
    let auth = match verify_control_request(
        &headers,
        "POST",
        "/nexus/control/v1/kb/sync/push",
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

    let request: crate::kb::sync::KbSyncPushRequest = match serde_json::from_slice(&body) {
        Ok(r) => r,
        Err(e) => {
            return json_response(
                StatusCode::BAD_REQUEST,
                &serde_json::json!({"error": format!("invalid JSON: {e}")}),
            );
        }
    };

    let response = crate::control_plane::handle_kb_push(&ctx.kb_store, &request).await;
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
) -> Response<RespBody> {
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

    let security = ctx
        .config
        .read()
        .expect("config lock")
        .network
        .security
        .clone();
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
    if !ctx
        .identity
        .verify_pairing_code_at(unix_now, &pair_req.pairing_code)
    {
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
