//! OpenAI-compatible mesh gateway (Phase 12 §5.1).
//!
//! Binds `network.gateway_port` (default 8090) and reverse-proxies
//! `/v1/chat/completions` to whichever mesh node currently holds the
//! requested model. Distinct from `api_port` (llama-server) and
//! `control_port` (signed control plane).

use crate::appliance::{
    apply_front_door_rung, burst_candidate_skip_lines, catalog_filename_for_digest,
    cold_predict_tok_s, decide_appliance, format_route_header, front_door_tags, link_header_token,
    load_presets, observe_stream_done, pantry_reverse_device_port, pantry_source_base,
    peer_lost_sse, plan_front_door, preset_tags_for_file, score_burst_candidate, tier2_model_id,
    warn_front_door_once, ApplianceDecision, BurstActivity, RouteStay, PANTRY_CATALOG_INTERVAL,
    PANTRY_CATALOG_WAIT,
};
use crate::bench::BenchStore;
use crate::client::NexusClient;
use crate::cluster::{
    moe_model_key, plan_fit_ladder, plan_moe_spawn, select_burst_winner, BurstScore, LinkClass,
    MemoryPolicy, MoeCacheCap,
};
use crate::config::NexusConfig;
use crate::control_plane::{
    blob_url, build_model_catalog, dispatch_load_model, dispatch_load_model_signed,
    dispatch_unload_model, dispatch_unload_model_signed, fetch_models, request_blob_fetch,
    BlobFetchRequest, ModelLoadRequest, ModelUnloadRequest, CONTROL_PLANE_VERSION,
};
use crate::discovery::{get_local_ip, DiscoveryService, PeerNode, StatusFlags};
use crate::gguf::{GgufMetadata, GgufValue, KvCacheDtype};
use crate::node_identity::NodeIdentity;
use crate::router::{RouteDecision, RouteTarget, Router};
use crate::store::ModelIndex;
use crate::supervisor::{LlamaServerConfig, SupervisorManager};
use crate::sysinfo::{AccelerationBackend, SystemProfile};
use crate::trust_auth::pairing_enforced;
use bytes::Bytes;
use futures_util::stream::{unfold, BoxStream};
use futures_util::{Stream, StreamExt, TryStreamExt};
use http_body_util::{BodyExt, Full, StreamBody};
use hyper::body::{Frame, Incoming};
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Method, Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::collections::HashMap;
use std::convert::Infallible;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;
use tracing::{info, warn};
use uuid::Uuid;

static EMBEDDED_INDEX_HTML: &str = include_str!("../web/dist/index.html");
static EMBEDDED_MANIFEST_JSON: &str = include_str!("../web/dist/manifest.json");

type RespBody = http_body_util::combinators::UnsyncBoxBody<Bytes, std::io::Error>;

/// Status report for an active or completed background model download.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DownloadProgressStatus {
    pub filename: String,
    pub downloaded_bytes: u64,
    pub total_bytes: Option<u64>,
    pub speed_bytes_per_sec: f64,
    pub percent: Option<f32>,
    pub done: bool,
    pub error: Option<String>,
}

/// Shared state for the mesh OpenAI gateway.
#[derive(Clone)]
pub struct GatewayContext {
    pub supervisor: SupervisorManager,
    pub discovery: Option<Arc<DiscoveryService>>,
    pub api_port: u16,
    pub models_dir: PathBuf,
    pub node_id: Uuid,
    pub config: Arc<std::sync::RwLock<NexusConfig>>,
    /// Override local llama OpenAI base URL (tests / tunnels).
    pub local_api_base: Option<String>,
    pub identity: Option<Arc<NodeIdentity>>,
    pub session_tokens: Arc<tokio::sync::RwLock<HashMap<String, std::time::Instant>>>,
    pub active_pin: Option<String>,
    pub download_status: Arc<tokio::sync::RwLock<Option<DownloadProgressStatus>>>,
    /// Last escalated request per burst peer. The local front door is not recorded.
    pub burst_activity: BurstActivity,
}

impl GatewayContext {
    pub fn new(
        supervisor: SupervisorManager,
        api_port: u16,
        models_dir: PathBuf,
        node_id: Uuid,
        config: Arc<std::sync::RwLock<NexusConfig>>,
    ) -> Self {
        Self {
            supervisor,
            discovery: None,
            api_port,
            models_dir,
            node_id,
            config,
            local_api_base: None,
            identity: None,
            session_tokens: Arc::new(tokio::sync::RwLock::new(HashMap::new())),
            active_pin: None,
            download_status: Arc::new(tokio::sync::RwLock::new(None)),
            burst_activity: BurstActivity::default(),
        }
    }

    pub fn with_discovery(mut self, discovery: Arc<DiscoveryService>) -> Self {
        self.discovery = Some(discovery);
        self
    }

    pub fn with_local_api_base(mut self, base: impl Into<String>) -> Self {
        self.local_api_base = Some(base.into());
        self
    }

    pub fn with_identity(mut self, identity: Arc<NodeIdentity>) -> Self {
        self.identity = Some(identity);
        self
    }

    pub fn with_pin(mut self, pin: impl Into<String>) -> Self {
        self.active_pin = Some(pin.into());
        self
    }

    fn local_upstream(&self) -> String {
        self.local_api_base
            .clone()
            .unwrap_or_else(|| format!("http://127.0.0.1:{}", self.api_port))
    }
}

/// Resolved upstream OpenAI base URL for a chat request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedUpstream {
    pub base_url: String,
    pub model_id: String,
    pub source: UpstreamSource,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UpstreamSource {
    Local,
    Peer,
}

/// Case-insensitive model id match (filename, stem, or path suffix).
pub fn model_matches(requested: &str, holder: &str) -> bool {
    let req = requested.trim();
    let hold = holder.trim();
    if req.is_empty() || hold.is_empty() {
        return false;
    }
    let req_l = req.to_lowercase();
    let hold_l = hold.to_lowercase();
    if req_l == hold_l {
        return true;
    }
    let req_stem = strip_gguf(&req_l);
    let hold_stem = strip_gguf(&hold_l);
    if req_stem == hold_stem {
        return true;
    }
    if hold_l.ends_with(&req_l) || hold_stem.ends_with(req_stem) {
        return true;
    }
    if let Some(file_name) = std::path::Path::new(&hold_l).file_name() {
        let name = file_name.to_string_lossy();
        if name == req_l || strip_gguf(&name) == req_stem {
            return true;
        }
    }
    false
}

fn strip_gguf(s: &str) -> &str {
    s.strip_suffix(".gguf").unwrap_or(s)
}

fn is_default_model(model: &str) -> bool {
    let m = model.trim();
    m.is_empty() || m.eq_ignore_ascii_case("default") || m.eq_ignore_ascii_case("local")
}

/// Resolve `model` → OpenAI base URL using local supervisor / discovery peers.
pub async fn resolve_model_upstream(ctx: &GatewayContext, model: &str) -> Option<ResolvedUpstream> {
    let appliance_enabled = ctx
        .config
        .read()
        .map(|cfg| cfg.inference.appliance.enabled)
        .unwrap_or(false);
    // Ordinary (default) chat belongs to the appliance router. Do not hand it
    // to the largest ready peer. A loaded local model still resolves here.
    if appliance_enabled && is_default_model(model) {
        return local_active_model(ctx).await.map(|local| ResolvedUpstream {
            base_url: ctx.local_upstream(),
            model_id: local,
            source: UpstreamSource::Local,
        });
    }

    let catalog = build_model_catalog(ctx.node_id, &ctx.models_dir);
    let resolved_names: Vec<String> = catalog
        .models
        .iter()
        .filter(|m| m.digest.eq_ignore_ascii_case(model) || model_matches(model, &m.filename))
        .flat_map(|m| {
            let mut names = vec![m.filename.clone()];
            if let Some(stem) = std::path::Path::new(&m.filename)
                .file_stem()
                .and_then(|s| s.to_str())
            {
                names.push(stem.to_string());
            }
            names
        })
        .collect();

    let matches_candidate = |candidate: &str| -> bool {
        if is_default_model(model) || model_matches(model, candidate) {
            return true;
        }
        for name in &resolved_names {
            if model_matches(name, candidate) {
                return true;
            }
        }
        false
    };

    let local_model = local_active_model(ctx).await;
    if let Some(ref local) = local_model {
        if matches_candidate(local) {
            return Some(ResolvedUpstream {
                base_url: ctx.local_upstream(),
                model_id: local.clone(),
                source: UpstreamSource::Local,
            });
        }
    }

    if let Some(discovery) = &ctx.discovery {
        let require_trust = {
            let cfg = ctx.config.read().ok()?;
            pairing_enforced(&cfg.network.security)
        };
        let peers = discovery.get_active_peers().await;
        let mut candidates: Vec<PeerNode> = peers
            .into_iter()
            .filter(|p| !p.active_model.is_empty())
            .filter(|p| p.status.is_ready() || !p.active_model.is_empty())
            .collect();

        if !is_default_model(model) {
            candidates.retain(|p| matches_candidate(&p.active_model));
        }

        candidates.sort_by_key(|p| {
            std::cmp::Reverse({
                let mut score = p.free_ram_mb as i64;
                if p.status.is_ready() {
                    score += 1000;
                }
                score -= p.thermal_index as i64;
                score
            })
        });

        for peer in candidates {
            if require_trust && !discovery.is_peer_trusted_for_routing(peer.uuid).await {
                continue;
            }
            return Some(ResolvedUpstream {
                base_url: peer.api_endpoint(),
                model_id: peer.active_model.clone(),
                source: UpstreamSource::Peer,
            });
        }
    }

    None
}

async fn local_active_model(ctx: &GatewayContext) -> Option<String> {
    if let Some(name) = ctx.supervisor.active_model().await {
        if !name.is_empty() {
            return Some(name);
        }
    }
    if let Some(discovery) = &ctx.discovery {
        let name = discovery.get_active_model().await;
        if !name.is_empty() {
            return Some(name);
        }
    }
    None
}

/// Bind `addr` and serve gateway routes until the task is aborted.
pub async fn serve(addr: SocketAddr, ctx: Arc<GatewayContext>) -> std::io::Result<()> {
    let listener = tokio::net::TcpListener::bind(addr).await?;
    info!("Mesh gateway listening on {}", addr);
    accept_loop(listener, ctx).await
}

/// Spawn the gateway on a background task.
pub fn spawn(
    addr: SocketAddr,
    ctx: Arc<GatewayContext>,
) -> tokio::task::JoinHandle<std::io::Result<()>> {
    tokio::spawn(async move { serve(addr, ctx).await })
}

/// Bind an ephemeral port (tests). Returns (bound address, join handle).
pub async fn spawn_ephemeral(
    ctx: Arc<GatewayContext>,
) -> std::io::Result<(SocketAddr, tokio::task::JoinHandle<std::io::Result<()>>)> {
    let listener = tokio::net::TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], 0))).await?;
    let addr = listener.local_addr()?;
    let handle = tokio::spawn(async move {
        info!("Mesh gateway listening on {}", addr);
        accept_loop(listener, ctx).await
    });
    Ok((addr, handle))
}

async fn accept_loop(
    listener: tokio::net::TcpListener,
    ctx: Arc<GatewayContext>,
) -> std::io::Result<()> {
    loop {
        let (stream, peer) = listener.accept().await?;
        let ctx = ctx.clone();
        tokio::spawn(async move {
            let io = TokioIo::new(stream);
            let service = service_fn(move |req| {
                let ctx = ctx.clone();
                async move { Ok::<_, Infallible>(route(req, ctx).await) }
            });
            if let Err(err) = http1::Builder::new().serve_connection(io, service).await {
                warn!("Gateway connection from {} error: {}", peer, err);
            }
        });
    }
}

async fn route(req: Request<Incoming>, ctx: Arc<GatewayContext>) -> Response<RespBody> {
    let path = req.uri().path().to_string();
    let method = req.method().clone();
    match (method.clone(), path.as_str()) {
        (Method::GET, "/") | (Method::GET, "/index.html") => handle_web_index().await,
        (Method::GET, "/manifest.json") => handle_web_manifest(),
        (Method::GET, "/health") => json_response(StatusCode::OK, &json!({"status": "ok"})),
        (Method::GET, "/v1/models") => handle_models(ctx).await,
        (Method::POST, "/v1/chat/completions") => handle_chat_completions(req, ctx).await,
        (Method::GET, "/api/system/profile") => handle_system_profile(ctx).await,
        (Method::GET, "/api/cluster/nodes") => handle_cluster_nodes(ctx).await,
        (Method::GET, "/api/events") => handle_events_stream(ctx).await,
        (Method::GET, "/api/logs/stream") => handle_logs_stream(ctx).await,
        (Method::POST, "/api/auth/verify") => handle_auth_verify(req, ctx).await,
        (Method::POST, "/api/model/load") => handle_api_model_load(req, ctx).await,
        (Method::POST, "/api/model/unload") => handle_api_model_unload(req, ctx).await,
        (Method::GET, "/api/hf/search") => handle_hf_search(req, ctx).await,
        (Method::GET, "/api/hf/repo") => handle_hf_repo(req, ctx).await,
        (Method::POST, "/api/models/download") => handle_model_download(req, ctx).await,
        (Method::GET, "/api/models/download/status") => handle_model_download_status(ctx).await,
        (Method::GET, "/api/config") => handle_config_get(ctx).await,
        (Method::POST, "/api/config") => handle_config_update(req, ctx).await,
        _ => {
            if method == Method::GET
                && path.starts_with("/api/models/")
                && path.ends_with("/inspect")
            {
                let stripped = &path["/api/models/".len()..path.len() - "/inspect".len()];
                handle_model_inspect(stripped, ctx).await
            } else if path.starts_with("/assets/") {
                handle_web_asset(&path).await
            } else {
                json_response(
                    StatusCode::NOT_FOUND,
                    &json!({"error": {"message": "not found", "type": "not_found"}}),
                )
            }
        }
    }
}

async fn handle_web_index() -> Response<RespBody> {
    if let Ok(dir) = std::env::var("NEXUS_WEB_DIR") {
        let path = PathBuf::from(dir).join("index.html");
        if let Ok(content) = tokio::fs::read(&path).await {
            return html_response(StatusCode::OK, content);
        }
    }
    html_response(StatusCode::OK, EMBEDDED_INDEX_HTML.as_bytes().to_vec())
}

fn handle_web_manifest() -> Response<RespBody> {
    Response::builder()
        .status(StatusCode::OK)
        .header(hyper::header::CONTENT_TYPE, "application/manifest+json")
        .body(full_body(Bytes::from_static(
            EMBEDDED_MANIFEST_JSON.as_bytes(),
        )))
        .unwrap_or_else(|_| Response::new(full_body(Bytes::from_static(b"{}"))))
}

async fn handle_web_asset(path: &str) -> Response<RespBody> {
    if let Ok(dir) = std::env::var("NEXUS_WEB_DIR") {
        let clean = path.trim_start_matches('/');
        let file_path = PathBuf::from(dir).join(clean);
        if let Ok(bytes) = tokio::fs::read(&file_path).await {
            let mime = if path.ends_with(".js") {
                "application/javascript"
            } else if path.ends_with(".css") {
                "text/css"
            } else if path.ends_with(".svg") {
                "image/svg+xml"
            } else {
                "application/octet-stream"
            };
            return Response::builder()
                .status(StatusCode::OK)
                .header(hyper::header::CONTENT_TYPE, mime)
                .body(full_body(Bytes::from(bytes)))
                .unwrap_or_else(|_| Response::new(full_body(Bytes::from_static(b""))));
        }
    }
    if path.ends_with("icon.svg") {
        let svg = r##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 100 100"><circle cx="50" cy="50" r="48" fill="#0b0f19" stroke="#06b6d4" stroke-width="4"/><text x="50" y="62" font-size="40" font-family="sans-serif" font-weight="bold" text-anchor="middle" fill="#06b6d4">N</text></svg>"##;
        return Response::builder()
            .status(StatusCode::OK)
            .header(hyper::header::CONTENT_TYPE, "image/svg+xml")
            .body(full_body(Bytes::from_static(svg.as_bytes())))
            .unwrap_or_else(|_| Response::new(full_body(Bytes::from_static(b""))));
    }
    json_response(StatusCode::NOT_FOUND, &json!({"error": "asset not found"}))
}

fn html_response(status: StatusCode, body: Vec<u8>) -> Response<RespBody> {
    Response::builder()
        .status(status)
        .header(hyper::header::CONTENT_TYPE, "text/html; charset=utf-8")
        .header(hyper::header::CACHE_CONTROL, "no-cache")
        .body(full_body(Bytes::from(body)))
        .unwrap_or_else(|_| Response::new(full_body(Bytes::from_static(b""))))
}

async fn handle_system_profile(ctx: Arc<GatewayContext>) -> Response<RespBody> {
    let profile = SystemProfile::probe();
    let node_name = {
        let cfg = ctx.config.read().unwrap();
        cfg.node.name.clone()
    };
    let local_model = local_active_model(&ctx).await;
    json_response(
        StatusCode::OK,
        &json!({
            "node_id": ctx.node_id.to_string(),
            "node_name": node_name,
            "total_ram_mb": profile.total_ram_mb,
            "available_ram_mb": profile.available_ram_mb,
            "backend": format!("{:?}", profile.detected_backend),
            "cpu_threads": profile.recommended_threads,
            "active_model": local_model,
        }),
    )
}

async fn handle_cluster_nodes(ctx: Arc<GatewayContext>) -> Response<RespBody> {
    let mut nodes = Vec::new();
    let profile = SystemProfile::probe();
    let node_name = {
        let cfg = ctx.config.read().unwrap();
        if cfg.node.name == "auto" {
            "Local Host".to_string()
        } else {
            cfg.node.name.clone()
        }
    };
    let active_model = local_active_model(&ctx).await.unwrap_or_default();
    nodes.push(json!({
        "uuid": ctx.node_id.to_string(),
        "display_name": node_name,
        "is_local": true,
        "total_ram_mb": profile.total_ram_mb,
        "free_ram_mb": profile.available_ram_mb,
        "backend": format!("{:?}", profile.detected_backend),
        "active_model": active_model,
        "role": "Host",
    }));

    if let Some(ref disc) = ctx.discovery {
        for peer in disc.get_active_peers().await {
            let display = if peer.display_name.is_empty() {
                format!("Node-{}", &peer.uuid.to_string()[..8])
            } else {
                peer.display_name.clone()
            };
            nodes.push(json!({
                "uuid": peer.uuid.to_string(),
                "display_name": display,
                "is_local": false,
                "total_ram_mb": peer.total_ram_mb,
                "free_ram_mb": peer.free_ram_mb,
                "backend": format!("{:?}", peer.backend),
                "active_model": peer.active_model,
                "role": format!("{:?}", peer.role),
            }));
        }
    }

    json_response(StatusCode::OK, &nodes)
}

async fn handle_events_stream(ctx: Arc<GatewayContext>) -> Response<RespBody> {
    let stream = unfold(ctx, |ctx| async move {
        tokio::time::sleep(Duration::from_secs(2)).await;
        let local_model = local_active_model(&ctx).await.unwrap_or_default();
        let peers_count = match &ctx.discovery {
            Some(d) => d.get_active_peers().await.len(),
            None => 0,
        };
        let payload = json!({
            "type": "heartbeat",
            "active_model": local_model,
            "peers_count": peers_count,
        });
        let sse = format!(
            "data: {}\n\n",
            serde_json::to_string(&payload).unwrap_or_default()
        );
        Some((sse, ctx))
    });

    let frame_stream =
        stream.map(|data| Ok::<Frame<Bytes>, std::io::Error>(Frame::data(Bytes::from(data))));
    let body = StreamBody::new(frame_stream).boxed_unsync();
    Response::builder()
        .status(StatusCode::OK)
        .header(hyper::header::CONTENT_TYPE, "text/event-stream")
        .header(hyper::header::CACHE_CONTROL, "no-cache")
        .body(body)
        .unwrap_or_else(|_| {
            json_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                &json!({"error": "stream failed"}),
            )
        })
}

async fn handle_logs_stream(_ctx: Arc<GatewayContext>) -> Response<RespBody> {
    let initial = "data: [INFO] Connected to Nexus-LLM live telemetry stream\n\n".to_string();
    let stream = unfold(Some(initial), |mut state| async move {
        if let Some(msg) = state.take() {
            Some((msg, None))
        } else {
            tokio::time::sleep(Duration::from_secs(5)).await;
            let ping = "data: [HEARTBEAT] Telemetry pulse alive\n\n".to_string();
            Some((ping, None))
        }
    });

    let frame_stream =
        stream.map(|data| Ok::<Frame<Bytes>, std::io::Error>(Frame::data(Bytes::from(data))));
    let body = StreamBody::new(frame_stream).boxed_unsync();
    Response::builder()
        .status(StatusCode::OK)
        .header(hyper::header::CONTENT_TYPE, "text/event-stream")
        .header(hyper::header::CACHE_CONTROL, "no-cache")
        .body(body)
        .unwrap_or_else(|_| {
            json_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                &json!({"error": "stream failed"}),
            )
        })
}

#[derive(Debug, Deserialize)]
struct PinVerifyRequest {
    pin: String,
}

async fn handle_auth_verify(
    req: Request<Incoming>,
    ctx: Arc<GatewayContext>,
) -> Response<RespBody> {
    let body_bytes = match req.collect().await {
        Ok(c) => c.to_bytes(),
        Err(e) => return json_response(StatusCode::BAD_REQUEST, &json!({"error": e.to_string()})),
    };
    let payload = match serde_json::from_slice::<PinVerifyRequest>(&body_bytes) {
        Ok(p) => p,
        Err(e) => return json_response(StatusCode::BAD_REQUEST, &json!({"error": e.to_string()})),
    };

    let pin = payload.pin.trim();
    let unix_now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();

    let mut valid = false;
    if let Some(ref active) = ctx.active_pin {
        if active == pin {
            valid = true;
        }
    }
    if !valid {
        if let Some(ref id) = ctx.identity {
            if id.verify_pairing_code_at(unix_now, pin) {
                valid = true;
            }
        }
    }
    if !valid && ctx.identity.is_none() && ctx.active_pin.is_none() {
        valid = true;
    }

    if valid {
        let token = Uuid::new_v4().to_string();
        let expires = std::time::Instant::now() + std::time::Duration::from_secs(86400);
        ctx.session_tokens
            .write()
            .await
            .insert(token.clone(), expires);
        json_response(
            StatusCode::OK,
            &json!({
                "success": true,
                "token": token,
                "expires_in": 86400
            }),
        )
    } else {
        json_response(
            StatusCode::UNAUTHORIZED,
            &json!({
                "success": false,
                "error": "Invalid 6-digit PIN"
            }),
        )
    }
}

async fn is_authorized(req: &Request<Incoming>, ctx: &GatewayContext) -> bool {
    let auth_header = req
        .headers()
        .get(hyper::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok());

    if let Some(auth) = auth_header {
        if let Some(token) = auth.strip_prefix("Bearer ") {
            let tokens = ctx.session_tokens.read().await;
            if let Some(&expire) = tokens.get(token.trim()) {
                if std::time::Instant::now() < expire {
                    return true;
                }
            }
        }
    }
    ctx.active_pin.is_none() && ctx.identity.is_none()
}

#[derive(Debug, Deserialize)]
struct ApiModelLoadReq {
    model_path: String,
    #[serde(default = "default_context_size")]
    context_size: usize,
    #[serde(default = "default_gpu_layers")]
    gpu_layers: u32,
}
fn default_context_size() -> usize {
    4096
}
fn default_gpu_layers() -> u32 {
    99
}

async fn handle_api_model_load(
    req: Request<Incoming>,
    ctx: Arc<GatewayContext>,
) -> Response<RespBody> {
    if !is_authorized(&req, &ctx).await {
        return json_response(
            StatusCode::UNAUTHORIZED,
            &json!({"error": "Admin authorization required (verify 6-digit PIN first)"}),
        );
    }

    let body_bytes = match req.collect().await {
        Ok(c) => c.to_bytes(),
        Err(e) => return json_response(StatusCode::BAD_REQUEST, &json!({"error": e.to_string()})),
    };
    let payload: ApiModelLoadReq = match serde_json::from_slice(&body_bytes) {
        Ok(p) => p,
        Err(e) => return json_response(StatusCode::BAD_REQUEST, &json!({"error": e.to_string()})),
    };

    let filename = std::path::Path::new(&payload.model_path)
        .file_name()
        .map(|f| f.to_string_lossy().to_string())
        .unwrap_or_else(|| payload.model_path.clone());
    let candidate = ctx.models_dir.join(&filename);
    let resolved_path = if candidate.exists() {
        candidate
    } else {
        std::path::PathBuf::from(&payload.model_path)
    };

    let (threads, binary, use_mmap, budget_percent, slot_save_path, moe) = {
        let cfg_lock = ctx.config.read().unwrap();
        let slot_path = if cfg_lock.inference.cache.prompt_cache_enabled {
            Some(PathBuf::from(&cfg_lock.inference.cache.slot_save_path))
        } else {
            None
        };
        (
            cfg_lock.hardware.acceleration.cpu_threads,
            cfg_lock.node.llama_server_binary.clone(),
            cfg_lock.hardware.safety.mmap,
            cfg_lock.hardware.safety.max_ram_usage_percent,
            slot_path,
            cfg_lock.inference.moe.clone(),
        )
    };

    let profile = SystemProfile::probe();
    if let Ok(gguf) = crate::gguf::GgufMetadata::open(&resolved_path) {
        if crate::bmoe_client::should_use_bmoe(
            &gguf,
            &profile,
            &moe,
            payload.context_size,
            budget_percent,
        ) {
            let model_key = crate::cluster::moe_model_key(&gguf);
            let bench = crate::bench::BenchStore::load_default().ok();
            let Some(plan) = crate::cluster::plan_moe_spawn(
                &gguf,
                &profile,
                &moe,
                budget_percent,
                payload.context_size,
                bench.as_ref(),
                &model_key,
                "local",
                crate::cluster::MoeCacheCap::None,
            ) else {
                return json_response(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    &json!({
                        "success": false,
                        "error": "MoE stream LMK: no feasible (context, cache) plan on this node"
                    }),
                );
            };
            for note in &plan.notes {
                info!("{note}");
            }
            let binary = PathBuf::from(&plan.moe.bmoe_binary);
            let bmoe_cfg = crate::bmoe_client::BmoeSessionConfig::from_profile_with_ceil(
                binary,
                resolved_path.clone(),
                "127.0.0.1",
                ctx.api_port,
                plan.context_size,
                threads,
                plan.moe,
                &profile,
                budget_percent,
                Vec::new(),
                Some(plan.cache_mb),
            );
            return match ctx.supervisor.spawn_bmoe(bmoe_cfg).await {
                Ok(_) => {
                    if let Some(ref d) = ctx.discovery {
                        d.set_active_model(&filename).await;
                    }
                    json_response(
                        StatusCode::OK,
                        &json!({
                            "success": true,
                            "message": format!("Model '{filename}' loaded via bmoe-cli"),
                            "active_model": filename,
                            "backend": "bmoe"
                        }),
                    )
                }
                Err(e) => json_response(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    &json!({
                        "success": false,
                        "error": e.to_string()
                    }),
                ),
            };
        }
    }

    let server_config = crate::supervisor::LlamaServerConfig {
        binary_path: PathBuf::from(binary),
        model_path: resolved_path,
        host: "127.0.0.1".to_string(),
        port: ctx.api_port,
        gpu_layers: payload.gpu_layers,
        threads,
        context_size: payload.context_size,
        extra_args: Vec::new(),
        use_mmap,
        use_mlock: false,
        cpu_threads_batch: threads,
        fallback_to_cpu: true,
        cache_type_k: None,
        cache_type_v: None,
        memory_budget_percent: budget_percent,
        tags: Vec::new(),
        slot_save_path,
    };

    match ctx.supervisor.spawn_slot(server_config).await {
        Ok(_) => {
            if let Some(ref d) = ctx.discovery {
                d.set_active_model(&filename).await;
            }
            json_response(
                StatusCode::OK,
                &json!({
                    "success": true,
                    "message": format!("Model '{filename}' loaded successfully"),
                    "active_model": filename,
                    "backend": "llama-server"
                }),
            )
        }
        Err(e) => json_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            &json!({
                "success": false,
                "error": e.to_string()
            }),
        ),
    }
}

async fn handle_api_model_unload(
    req: Request<Incoming>,
    ctx: Arc<GatewayContext>,
) -> Response<RespBody> {
    if !is_authorized(&req, &ctx).await {
        return json_response(
            StatusCode::UNAUTHORIZED,
            &json!({"error": "Admin authorization required"}),
        );
    }

    let _ = ctx.supervisor.stop().await;
    if let Some(ref d) = ctx.discovery {
        d.set_active_model("").await;
    }
    json_response(
        StatusCode::OK,
        &json!({
            "success": true,
            "message": "Model unloaded"
        }),
    )
}

fn url_decode(s: &str) -> String {
    let mut result = Vec::new();
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Ok(b) =
                u8::from_str_radix(std::str::from_utf8(&bytes[i + 1..i + 3]).unwrap_or(""), 16)
            {
                result.push(b);
                i += 3;
                continue;
            }
        } else if bytes[i] == b'+' {
            result.push(b' ');
            i += 1;
            continue;
        }
        result.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&result).to_string()
}

fn parse_query_param(query: Option<&str>, key: &str) -> Option<String> {
    let q = query?;
    for pair in q.split('&') {
        let mut parts = pair.splitn(2, '=');
        if let (Some(k), Some(v)) = (parts.next(), parts.next()) {
            if k == key {
                return Some(url_decode(v));
            }
        }
    }
    None
}

async fn handle_hf_search(req: Request<Incoming>, ctx: Arc<GatewayContext>) -> Response<RespBody> {
    let query_str = req.uri().query();
    let q = parse_query_param(query_str, "q").unwrap_or_default();
    let limit = parse_query_param(query_str, "limit")
        .and_then(|l| l.parse::<usize>().ok())
        .unwrap_or(10);

    let token = {
        let cfg = ctx.config.read().unwrap();
        cfg.resolved_hf_token()
    };
    let client = crate::hf::HfClient::new(token);
    match client.search_models(&q, limit).await {
        Ok(results) => json_response(StatusCode::OK, &results),
        Err(e) => json_response(
            StatusCode::BAD_GATEWAY,
            &json!({"error": format!("Hugging Face search failed: {e}")}),
        ),
    }
}

async fn handle_hf_repo(req: Request<Incoming>, ctx: Arc<GatewayContext>) -> Response<RespBody> {
    let query_str = req.uri().query();
    let repo_id = match parse_query_param(query_str, "id") {
        Some(id) if !id.trim().is_empty() => id.trim().to_string(),
        _ => {
            return json_response(
                StatusCode::BAD_REQUEST,
                &json!({"error": "Missing 'id' query parameter"}),
            )
        }
    };

    let token = {
        let cfg = ctx.config.read().unwrap();
        cfg.resolved_hf_token()
    };
    let client = crate::hf::HfClient::new(token);
    match client.model_details(&repo_id).await {
        Ok(detail) => {
            let profile = SystemProfile::probe();
            let mut cluster_free_mb = profile.available_ram_mb;
            if let Some(ref d) = ctx.discovery {
                for peer in d.get_active_peers().await {
                    cluster_free_mb = cluster_free_mb.saturating_add(peer.free_ram_mb as u64);
                }
            }
            let groups = crate::hf::HfClient::parse_gguf_groups(
                &detail,
                profile.available_ram_mb,
                cluster_free_mb,
            );
            json_response(
                StatusCode::OK,
                &json!({
                    "repo_id": repo_id,
                    "detail": detail,
                    "quant_groups": groups,
                }),
            )
        }
        Err(e) => json_response(
            StatusCode::BAD_GATEWAY,
            &json!({"error": format!("Hugging Face fetch failed: {e}")}),
        ),
    }
}

#[derive(Debug, Deserialize)]
struct ModelDownloadRequest {
    url: String,
    filename: String,
    #[serde(default)]
    expected_sha256: Option<String>,
}

async fn handle_model_download(
    req: Request<Incoming>,
    ctx: Arc<GatewayContext>,
) -> Response<RespBody> {
    if !is_authorized(&req, &ctx).await {
        return json_response(
            StatusCode::UNAUTHORIZED,
            &json!({"error": "Admin authorization required (verify 6-digit PIN first)"}),
        );
    }
    let body_bytes = match req.collect().await {
        Ok(c) => c.to_bytes(),
        Err(e) => return json_response(StatusCode::BAD_REQUEST, &json!({"error": e.to_string()})),
    };
    let payload = match serde_json::from_slice::<ModelDownloadRequest>(&body_bytes) {
        Ok(p) => p,
        Err(e) => return json_response(StatusCode::BAD_REQUEST, &json!({"error": e.to_string()})),
    };

    let filename = payload.filename.trim().to_string();
    if filename.is_empty()
        || filename.contains('/')
        || filename.contains('\\')
        || filename.contains("..")
    {
        return json_response(
            StatusCode::BAD_REQUEST,
            &json!({"error": "Invalid filename; must be a flat filename"}),
        );
    }

    {
        let lock = ctx.download_status.read().await;
        if let Some(ref st) = *lock {
            if !st.done && st.error.is_none() {
                return json_response(
                    StatusCode::CONFLICT,
                    &json!({
                        "error": format!("Download already in progress for '{}'", st.filename),
                        "active": st
                    }),
                );
            }
        }
    }

    let dest_path = ctx.models_dir.join(&filename);
    let _ = tokio::fs::create_dir_all(&ctx.models_dir).await;

    let status_lock = ctx.download_status.clone();
    {
        let mut status = status_lock.write().await;
        *status = Some(DownloadProgressStatus {
            filename: filename.clone(),
            downloaded_bytes: 0,
            total_bytes: None,
            speed_bytes_per_sec: 0.0,
            percent: Some(0.0),
            done: false,
            error: None,
        });
    }

    let token = {
        let cfg = ctx.config.read().unwrap();
        cfg.resolved_hf_token()
    };
    let url = payload.url.clone();
    let expected_sha = payload.expected_sha256.clone();
    let spawn_filename = filename.clone();

    tokio::spawn(async move {
        let downloader = crate::downloader::ModelDownloader::new().with_hf_token(token);
        let status_cb = status_lock.clone();
        let fname = spawn_filename.clone();
        let res = downloader
            .download(&url, &dest_path, expected_sha.as_deref(), move |progress| {
                if let Ok(mut lock) = status_cb.try_write() {
                    *lock = Some(DownloadProgressStatus {
                        filename: fname.clone(),
                        downloaded_bytes: progress.downloaded_bytes,
                        total_bytes: progress.total_bytes,
                        speed_bytes_per_sec: progress.speed_bytes_per_sec,
                        percent: progress.percent,
                        done: false,
                        error: None,
                    });
                }
            })
            .await;

        let mut lock = status_lock.write().await;
        match res {
            Ok(_) => {
                if let Some(ref mut st) = *lock {
                    st.done = true;
                    st.percent = Some(100.0);
                }
            }
            Err(e) => {
                if let Some(ref mut st) = *lock {
                    st.done = true;
                    st.error = Some(e.to_string());
                }
            }
        }
    });

    json_response(
        StatusCode::OK,
        &json!({
            "status": "started",
            "filename": filename
        }),
    )
}

async fn handle_model_download_status(ctx: Arc<GatewayContext>) -> Response<RespBody> {
    let lock = ctx.download_status.read().await;
    match &*lock {
        Some(st) => json_response(StatusCode::OK, st),
        None => json_response(StatusCode::OK, &json!({"status": "idle"})),
    }
}

async fn handle_model_inspect(model_id: &str, ctx: Arc<GatewayContext>) -> Response<RespBody> {
    let decoded = url_decode(model_id);
    let target_path = ctx.models_dir.join(&decoded);
    let resolved = if target_path.exists() {
        Some(target_path)
    } else {
        let direct = PathBuf::from(&decoded);
        if direct.exists() {
            Some(direct)
        } else if let Ok(entries) = std::fs::read_dir(&ctx.models_dir) {
            entries.flatten().map(|e| e.path()).find(|p| {
                p.file_name()
                    .map(|f| f.to_string_lossy().to_string())
                    .map(|name| model_matches(&decoded, &name))
                    .unwrap_or(false)
            })
        } else {
            None
        }
    };

    let path = match resolved {
        Some(p) => p,
        None => {
            return json_response(
                StatusCode::NOT_FOUND,
                &json!({"error": format!("Model file not found: {}", decoded)}),
            );
        }
    };

    match crate::gguf::GgufMetadata::open(&path) {
        Ok(meta) => json_response(
            StatusCode::OK,
            &json!({
                "filename": path.file_name().map(|f| f.to_string_lossy()).unwrap_or_default(),
                "architecture": meta.architecture,
                "model_name": meta.model_name,
                "context_length": meta.context_length,
                "block_count": meta.block_count,
                "head_count": meta.head_count,
                "head_count_kv": meta.head_count_kv,
                "embedding_length": meta.embedding_length,
                "expert_count": meta.expert_count,
                "expert_used_count": meta.expert_used_count,
                "is_moe": meta.is_moe(),
                "streamable_moe": meta.streamable_moe(),
                "tensor_count": meta.tensor_count,
                "kv_count": meta.kv_count,
                "file_size_bytes": meta.file_size_bytes,
                "quant_label": meta.quant_label,
            }),
        ),
        Err(e) => json_response(
            StatusCode::BAD_REQUEST,
            &json!({"error": format!("Failed to parse GGUF metadata: {e}")}),
        ),
    }
}

#[derive(Debug, Deserialize)]
struct ConfigUpdateRequest {
    pub prompt_cache_enabled: Option<bool>,
    pub slot_save_path: Option<String>,
    pub max_cache_mb: Option<u64>,
    pub battery_floor_percent: Option<u8>,
    pub battery_action: Option<String>,
    pub max_ram_usage_percent: Option<u8>,
    pub moe_enabled: Option<bool>,
    pub moe_cache_mb: Option<String>,
    pub moe_cache_ceil_mb: Option<u64>,
    pub moe_quality_mode: Option<String>,
    pub moe_overlap: Option<bool>,
    pub moe_drop_cold_experts: Option<String>,
    pub moe_expert_substitute: Option<String>,
    pub moe_route_ahead: Option<u32>,
}

/// Apply a settings patch onto a copy. The live config changes only after `NexusConfig::validate`.
fn apply_settings_update(
    cfg: &mut NexusConfig,
    payload: &ConfigUpdateRequest,
) -> Result<(), String> {
    let mut next = cfg.clone();
    if let Some(enabled) = payload.prompt_cache_enabled {
        next.inference.cache.prompt_cache_enabled = enabled;
    }
    if let Some(path) = payload.slot_save_path.clone() {
        next.inference.cache.slot_save_path = path;
    }
    if let Some(max_mb) = payload.max_cache_mb {
        next.inference.cache.max_cache_mb = max_mb;
    }
    if let Some(floor) = payload.battery_floor_percent {
        next.hardware.safety.battery_floor_percent = floor;
    }
    if let Some(action) = payload.battery_action.clone() {
        next.hardware.safety.battery_action = action;
    }
    if let Some(ram) = payload.max_ram_usage_percent {
        next.hardware.safety.max_ram_usage_percent = ram;
    }
    if let Some(enabled) = payload.moe_enabled {
        next.inference.moe.enabled = enabled;
    }
    if let Some(cache_mb) = payload.moe_cache_mb.clone() {
        next.inference.moe.cache_mb = cache_mb;
    }
    if let Some(ceil) = payload.moe_cache_ceil_mb {
        next.inference.moe.cache_ceil_mb = ceil;
    }
    if let Some(mode) = payload.moe_quality_mode.as_deref() {
        match mode.to_ascii_lowercase().as_str() {
            "lossy" => next.inference.moe.quality_mode = crate::config::MoeQualityMode::Lossy,
            "lossless" => next.inference.moe.quality_mode = crate::config::MoeQualityMode::Lossless,
            _ => {}
        }
    }
    if let Some(overlap) = payload.moe_overlap {
        next.inference.moe.overlap = overlap;
    }
    if let Some(drop) = payload.moe_drop_cold_experts.clone() {
        next.inference.moe.drop_cold_experts = if drop.is_empty() { None } else { Some(drop) };
    }
    if let Some(sub) = payload.moe_expert_substitute.clone() {
        next.inference.moe.expert_substitute = if sub.is_empty() { None } else { Some(sub) };
    }
    if let Some(n) = payload.moe_route_ahead {
        next.inference.moe.route_ahead = if n == 0 { None } else { Some(n) };
    }
    next.validate().map_err(|err| err.to_string())?;
    *cfg = next;
    Ok(())
}

async fn handle_config_get(ctx: Arc<GatewayContext>) -> Response<RespBody> {
    let cfg = ctx.config.read().unwrap();
    json_response(
        StatusCode::OK,
        &json!({
            "inference": {
                "prompt_cache_enabled": cfg.inference.cache.prompt_cache_enabled,
                "slot_save_path": cfg.inference.cache.slot_save_path,
                "max_cache_mb": cfg.inference.cache.max_cache_mb,
                "moe": {
                    "enabled": cfg.inference.moe.enabled,
                    "bmoe_binary": cfg.inference.moe.bmoe_binary,
                    "cache_mb": cfg.inference.moe.cache_mb,
                    "cache_floor_mb": cfg.inference.moe.cache_floor_mb,
                    "cache_ceil_mb": cfg.inference.moe.cache_ceil_mb,
                    "io_threads": cfg.inference.moe.io_threads,
                    "dense_weights": cfg.inference.moe.dense_weights,
                    "overlap": cfg.inference.moe.overlap,
                    "quality_mode": cfg.inference.moe.quality_mode,
                    "drop_cold_experts": cfg.inference.moe.drop_cold_experts,
                    "expert_substitute": cfg.inference.moe.expert_substitute,
                    "route_ahead": cfg.inference.moe.route_ahead,
                    "min_cache_mb": cfg.inference.moe.min_cache_mb,
                    "prefer": cfg.inference.moe.prefer,
                    "adapt": cfg.inference.moe.adapt,
                    "warm_hit_pct": cfg.inference.moe.warm_hit_pct,
                    "cold_hit_pct": cfg.inference.moe.cold_hit_pct,
                    "chronic_hit_pct": cfg.inference.moe.chronic_hit_pct,
                    "working_set_factor": cfg.inference.moe.working_set_factor,
                    "reference_tok_millis": cfg.inference.moe.reference_tok_millis,
                }
            },
            "safety": {
                "battery_floor_percent": cfg.hardware.safety.battery_floor_percent,
                "battery_action": cfg.hardware.safety.battery_action,
                "max_ram_usage_percent": cfg.hardware.safety.max_ram_usage_percent,
            }
        }),
    )
}

async fn handle_config_update(
    req: Request<Incoming>,
    ctx: Arc<GatewayContext>,
) -> Response<RespBody> {
    if !is_authorized(&req, &ctx).await {
        return json_response(
            StatusCode::UNAUTHORIZED,
            &json!({"error": "Admin authorization required (verify 6-digit PIN first)"}),
        );
    }
    let body_bytes = match req.collect().await {
        Ok(c) => c.to_bytes(),
        Err(e) => return json_response(StatusCode::BAD_REQUEST, &json!({"error": e.to_string()})),
    };
    let payload = match serde_json::from_slice::<ConfigUpdateRequest>(&body_bytes) {
        Ok(p) => p,
        Err(e) => return json_response(StatusCode::BAD_REQUEST, &json!({"error": e.to_string()})),
    };

    let mut cfg = ctx.config.write().unwrap();
    if let Err(err) = apply_settings_update(&mut cfg, &payload) {
        return json_response(StatusCode::BAD_REQUEST, &json!({"error": err}));
    }

    if let Err(e) = cfg.save() {
        warn!("Failed to persist config to disk: {}", e);
    }

    json_response(
        StatusCode::OK,
        &json!({
            "success": true,
            "inference": {
                "prompt_cache_enabled": cfg.inference.cache.prompt_cache_enabled,
                "slot_save_path": cfg.inference.cache.slot_save_path,
                "max_cache_mb": cfg.inference.cache.max_cache_mb,
                "moe": {
                    "enabled": cfg.inference.moe.enabled,
                    "cache_mb": cfg.inference.moe.cache_mb,
                    "cache_ceil_mb": cfg.inference.moe.cache_ceil_mb,
                    "quality_mode": cfg.inference.moe.quality_mode,
                    "overlap": cfg.inference.moe.overlap,
                }
            },
            "safety": {
                "battery_floor_percent": cfg.hardware.safety.battery_floor_percent,
                "battery_action": cfg.hardware.safety.battery_action,
                "max_ram_usage_percent": cfg.hardware.safety.max_ram_usage_percent,
            }
        }),
    )
}

#[derive(Debug, Deserialize)]
struct ChatProxyRequest {
    #[serde(default)]
    model: String,
}

async fn handle_chat_completions(
    req: Request<Incoming>,
    ctx: Arc<GatewayContext>,
) -> Response<RespBody> {
    let body_bytes = match req.collect().await {
        Ok(collected) => collected.to_bytes(),
        Err(e) => {
            return json_response(
                StatusCode::BAD_REQUEST,
                &json!({"error": {"message": format!("failed to read body: {e}"), "type": "invalid_request"}}),
            );
        }
    };
    let body_bytes = strip_status_messages(&body_bytes);

    let model = match serde_json::from_slice::<ChatProxyRequest>(&body_bytes) {
        Ok(parsed) => parsed.model,
        Err(e) => {
            return json_response(
                StatusCode::BAD_REQUEST,
                &json!({"error": {"message": format!("invalid JSON: {e}"), "type": "invalid_request"}}),
            );
        }
    };

    let appliance_on = ctx
        .config
        .read()
        .map(|cfg| cfg.inference.appliance.enabled)
        .unwrap_or(false);
    if appliance_on && is_default_model(&model) {
        return handle_appliance_chat(ctx, body_bytes).await;
    }

    let resolved = match resolve_model_upstream(&ctx, &model).await {
        Some(r) => r,
        None => {
            return json_response(
                StatusCode::NOT_FOUND,
                &json!({
                    "error": {
                        "message": format!(
                            "no active holder for model '{model}' (load a model on a mesh node first)"
                        ),
                        "type": "model_not_found"
                    }
                }),
            );
        }
    };

    proxy_upstream(
        &resolved.base_url,
        "/v1/chat/completions",
        body_bytes,
        None,
        false,
        None,
    )
    .await
}

async fn proxy_upstream(
    base_url: &str,
    path: &str,
    body: Bytes,
    route_header: Option<String>,
    note_peer_lost: bool,
    burst: Option<(BurstActivity, Uuid)>,
) -> Response<RespBody> {
    let url = format!(
        "{}{}",
        base_url.trim_end_matches('/'),
        if path.starts_with('/') {
            path.to_string()
        } else {
            format!("/{path}")
        }
    );

    let client = match reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(600))
        .build()
    {
        Ok(c) => c,
        Err(e) => {
            return json_response(
                StatusCode::BAD_GATEWAY,
                &json!({"error": {"message": e.to_string(), "type": "proxy_error"}}),
            );
        }
    };

    let upstream = match client
        .post(&url)
        .header(reqwest::header::CONTENT_TYPE, "application/json")
        .header(
            reqwest::header::ACCEPT,
            "text/event-stream, application/json",
        )
        .body(body)
        .send()
        .await
    {
        Ok(resp) => resp,
        Err(e) => {
            return json_response(
                StatusCode::BAD_GATEWAY,
                &json!({"error": {"message": format!("upstream unreachable: {e}"), "type": "proxy_error"}}),
            );
        }
    };

    let status =
        StatusCode::from_u16(upstream.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
    let content_type = upstream
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("application/json")
        .to_string();

    let mapped = upstream
        .bytes_stream()
        .map_err(|e| std::io::Error::other(format!("upstream stream error: {e}")));
    let is_sse = content_type
        .to_ascii_lowercase()
        .contains("text/event-stream");
    let annotate = note_peer_lost && is_sse && status.is_success();
    let byte_stream: BoxStream<'static, Result<Bytes, std::io::Error>> =
        if annotate || burst.is_some() {
            Box::pin(PeerLostStream::new(mapped, annotate, burst))
        } else {
            Box::pin(mapped)
        };
    let frame_stream = byte_stream.map_ok(Frame::data);
    let body = StreamBody::new(frame_stream).boxed_unsync();

    let mut builder = Response::builder()
        .status(status)
        .header(hyper::header::CONTENT_TYPE, content_type)
        .header(hyper::header::CACHE_CONTROL, "no-cache");
    if let Some(route) = route_header {
        builder = builder.header("X-Nexus-Route", route);
    }
    builder.body(body).unwrap_or_else(|_| {
        json_response(
            StatusCode::BAD_GATEWAY,
            &json!({"error": {"message": "failed to build proxy response", "type": "proxy_error"}}),
        )
    })
}

async fn handle_models(ctx: Arc<GatewayContext>) -> Response<RespBody> {
    let mut ids: Vec<String> = Vec::new();
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();

    let mut add_id = |id: String| {
        let trimmed = id.trim();
        if !trimmed.is_empty() && seen.insert(trimmed.to_string()) {
            ids.push(trimmed.to_string());
        }
    };

    if let Some(local) = local_active_model(&ctx).await {
        add_id(local);
    }

    if let Some(discovery) = &ctx.discovery {
        for peer in discovery.get_active_peers().await {
            if !peer.active_model.is_empty() {
                add_id(peer.active_model);
            }
        }
    }

    let catalog = build_model_catalog(ctx.node_id, &ctx.models_dir);
    for entry in catalog.models {
        if !entry.filename.is_empty() {
            add_id(entry.filename.clone());
            if let Some(stem) = std::path::Path::new(&entry.filename)
                .file_stem()
                .and_then(|s| s.to_str())
            {
                add_id(stem.to_string());
            }
        }
    }

    let data: Vec<serde_json::Value> = ids
        .into_iter()
        .map(|id| {
            json!({
                "id": id,
                "object": "model",
                "owned_by": "nexus-mesh",
            })
        })
        .collect();

    json_response(
        StatusCode::OK,
        &json!({
            "object": "list",
            "data": data,
        }),
    )
}

/// Escalate with no interactive winner stays local when a model is loaded, else HTTP 503.
fn decision_after_rank(
    decision: ApplianceDecision,
    winner: Option<usize>,
    local_loaded: bool,
) -> Result<ApplianceDecision, StatusCode> {
    match decision {
        ApplianceDecision::Escalate { .. } if winner.is_none() => {
            if local_loaded {
                Ok(ApplianceDecision::Local {
                    reason: RouteStay::NoInteractivePeer,
                })
            } else {
                Err(StatusCode::SERVICE_UNAVAILABLE)
            }
        }
        other => Ok(other),
    }
}

fn no_interactive_peer_body(tag: &str) -> serde_json::Value {
    json!({
        "error": {
            "message": format!("no interactive peer for tag {tag}"),
            "type": "no_interactive_peer"
        }
    })
}

struct ChosenLoad {
    filename: String,
    digest: String,
    context_size: usize,
    predicted_tok_s: f32,
    interactive: bool,
    backend: String,
    moe_cache_ceil_mb: Option<u64>,
    tags: Vec<String>,
    /// Dense ladder dtype. MoE plans leave this empty; llama-server then keeps its default.
    kv_dtype: Option<KvCacheDtype>,
}

/// Full Vulkan offload. A CPU front door must not copy `gpu_layers = 0` onto a Vulkan phone.
fn peer_offload_layers(backend: AccelerationBackend) -> u32 {
    match backend {
        AccelerationBackend::Vulkan => 99,
        AccelerationBackend::ArmCpuDotProd
        | AccelerationBackend::X86Baseline
        | AccelerationBackend::GenericCpu => 0,
    }
}

fn cache_type_pair(dtype: Option<KvCacheDtype>) -> (Option<String>, Option<String>) {
    match dtype {
        Some(dtype) => {
            let arg = dtype.as_llama_arg().to_string();
            (Some(arg.clone()), Some(arg))
        }
        None => (None, None),
    }
}

struct PantryRow {
    filename: String,
    digest: String,
    tags: Vec<String>,
    meta: GgufMetadata,
}

struct ApplianceSnap {
    floor: f32,
    battery_floor: u8,
    battery_action: String,
    max_ram: u8,
    mmap: bool,
    mlock: bool,
    presets_dir: PathBuf,
    models_dir: PathBuf,
    control_port: u16,
    label: String,
    pairing: bool,
    threads: usize,
    moe: crate::config::MoeConfig,
}

fn read_appliance_snap(ctx: &GatewayContext) -> Option<ApplianceSnap> {
    let cfg = ctx.config.read().ok()?;
    Some(ApplianceSnap {
        floor: cfg.inference.appliance.floor_tok_s(),
        battery_floor: cfg.hardware.safety.battery_floor_percent,
        battery_action: cfg.hardware.safety.battery_action.clone(),
        max_ram: cfg.hardware.safety.max_ram_usage_percent,
        mmap: cfg.hardware.safety.mmap,
        mlock: cfg.hardware.safety.mlock,
        presets_dir: cfg.node.presets_dir.clone(),
        models_dir: PathBuf::from(&cfg.node.models_dir),
        control_port: cfg.network.control_port,
        label: cfg.resolved_display_name(),
        pairing: cfg.network.security.pairing_enforced(),
        threads: cfg.hardware.acceleration.cpu_threads,
        moe: cfg.inference.moe.clone(),
    })
}

/// Drop `role: status` turns so a route notice is not appended to the model prompt.
fn strip_status_messages(body: &Bytes) -> Bytes {
    let Ok(mut value) = serde_json::from_slice::<serde_json::Value>(body) else {
        return body.clone();
    };
    let Some(messages) = value
        .get_mut("messages")
        .and_then(|item| item.as_array_mut())
    else {
        return body.clone();
    };
    let before = messages.len();
    messages.retain(|message| {
        !message
            .get("role")
            .and_then(|role| role.as_str())
            .is_some_and(|role| role.eq_ignore_ascii_case("status"))
    });
    if messages.len() == before {
        return body.clone();
    }
    serde_json::to_vec(&value)
        .map(Bytes::from)
        .unwrap_or_else(|_| body.clone())
}

fn user_prompt(body: &serde_json::Value) -> String {
    let Some(messages) = body.get("messages").and_then(|value| value.as_array()) else {
        return String::new();
    };
    for message in messages.iter().rev() {
        let role = message
            .get("role")
            .and_then(|value| value.as_str())
            .unwrap_or("");
        if !role.eq_ignore_ascii_case("user") {
            continue;
        }
        if let Some(text) = message.get("content").and_then(|value| value.as_str()) {
            return text.to_string();
        }
        if let Some(parts) = message.get("content").and_then(|value| value.as_array()) {
            let mut text = String::new();
            for part in parts {
                if let Some(piece) = part.as_str() {
                    text.push_str(piece);
                } else if let Some(piece) = part.get("text").and_then(|value| value.as_str()) {
                    text.push_str(piece);
                }
            }
            return text;
        }
    }
    String::new()
}

fn route_targets(tags: &[String]) -> Vec<RouteTarget> {
    tags.iter()
        .map(|tag| RouteTarget::new(Uuid::nil(), "http://127.0.0.1", "local", vec![tag.clone()]))
        .collect()
}

fn known_route_tags(presets_dir: &Path, extra: &[String]) -> Vec<String> {
    let mut tags = Vec::new();
    let mut push = |tag: &str| {
        if !tag.is_empty()
            && !tags
                .iter()
                .any(|have: &String| have.eq_ignore_ascii_case(tag))
        {
            tags.push(tag.to_string());
        }
    };
    for preset in load_presets(presets_dir) {
        for tag in &preset.tags {
            push(tag);
        }
    }
    for tag in extra {
        push(tag);
    }
    for tag in ["general", "chat", "coder"] {
        push(tag);
    }
    tags
}

fn tier1_tag(prompt: &str, presets_dir: &Path, local_tags: &[String]) -> Option<String> {
    let mut router = Router::new();
    router.load_presets_dir(presets_dir);
    let mut tags = known_route_tags(presets_dir, local_tags);
    if let Some((explicit, _)) = Router::extract_explicit_tag(prompt) {
        if !tags.iter().any(|tag| tag.eq_ignore_ascii_case(explicit)) {
            tags.push(explicit.to_string());
        }
    }
    let routes = route_targets(&tags);
    match router.route_deterministic(prompt, &routes) {
        Some(RouteDecision::Direct { matched_tag, .. }) => Some(matched_tag),
        _ => None,
    }
}

async fn tier2_tag(
    prompt: &str,
    presets_dir: &Path,
    local_base: &str,
    loaded_model: &str,
) -> Result<String, ()> {
    let mut router = Router::new();
    router.load_presets_dir(presets_dir);
    let tags = known_route_tags(presets_dir, &[]);
    let routes = route_targets(&tags);
    let client = NexusClient::new(local_base);
    let model = tier2_model_id(loaded_model);
    match router
        .route_orchestrated(prompt, &routes, &client, &model)
        .await
    {
        Ok(RouteDecision::Orchestrated { route, .. }) => Ok(route),
        Ok(_) => Err(()),
        Err(_) => Err(()),
    }
}

async fn local_front_door_tags(ctx: &GatewayContext) -> (bool, Vec<String>, String) {
    let slots = ctx.supervisor.slots_info().await;
    if let Some(slot) = slots.into_iter().find(|slot| !slot.model_name.is_empty()) {
        let presets = ctx
            .config
            .read()
            .ok()
            .map(|cfg| load_presets(&cfg.node.presets_dir))
            .unwrap_or_default();
        let tags = if slot.tags.is_empty() {
            preset_tags_for_file(&slot.model_name, None, &presets)
        } else {
            slot.tags
        };
        return (true, front_door_tags(&tags), slot.model_name);
    }
    if let Some(name) = local_active_model(ctx).await {
        let presets = ctx
            .config
            .read()
            .ok()
            .map(|cfg| load_presets(&cfg.node.presets_dir))
            .unwrap_or_default();
        let tags = preset_tags_for_file(&name, None, &presets);
        return (true, front_door_tags(&tags), name);
    }
    (false, Vec::new(), "local".to_string())
}

fn tag_carried(tags: &[String], tag: &str) -> bool {
    tags.iter().any(|have| have.eq_ignore_ascii_case(tag))
}

fn load_tagged_pantry(models_dir: &Path, presets_dir: &Path, tag: &str) -> Vec<PantryRow> {
    let index = ModelIndex::reconcile_default(models_dir).unwrap_or_default();
    let presets = load_presets(presets_dir);
    let mut rows = Vec::new();
    for entry in index.models {
        let Ok(mut meta) = GgufMetadata::open(&entry.path) else {
            continue;
        };
        if !meta.metadata.contains_key("filename") {
            meta.metadata.insert(
                "filename".to_string(),
                GgufValue::String(entry.filename.clone()),
            );
        }
        let tags = preset_tags_for_file(&entry.filename, entry.model_name.as_deref(), &presets);
        if !tag_carried(&tags, tag) {
            continue;
        }
        rows.push(PantryRow {
            filename: entry.filename,
            digest: entry.digest,
            tags,
            meta,
        });
    }
    rows
}

fn predict_file(
    meta: &GgufMetadata,
    peer: &PeerNode,
    context_size: usize,
    bench: Option<&BenchStore>,
) -> f32 {
    let model_id = meta
        .model_name
        .clone()
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| "model".to_string());
    let measured = bench.and_then(|store| {
        store
            .lookup_gen_tok_s(
                &model_id,
                &peer.uuid.to_string(),
                peer.backend,
                context_size,
            )
            .or_else(|| store.lookup_gen_tok_s_any_node(&model_id, peer.backend, context_size))
    });
    cold_predict_tok_s(
        peer.backend,
        meta.weights_bytes(),
        peer.thermal_index,
        meta.quant_label.as_deref(),
        measured,
    )
}

fn plan_tagged_file(
    row: &PantryRow,
    peer: &PeerNode,
    snap: &ApplianceSnap,
    bench: Option<&BenchStore>,
) -> Option<ChosenLoad> {
    let profile = SystemProfile::from_advertised(
        u64::from(peer.free_ram_mb),
        u64::from(peer.total_ram_mb),
        peer.backend,
    );
    let policy = MemoryPolicy {
        max_ram_usage_percent: snap.max_ram,
        mmap: snap.mmap,
        mlock: snap.mlock,
        kv_dtype: KvCacheDtype::F16,
        context_size: 4096,
        batch_size: 512,
    };
    if row.meta.streamable_moe() {
        let key = moe_model_key(&row.meta);
        let plan = plan_moe_spawn(
            &row.meta,
            &profile,
            &snap.moe,
            snap.max_ram,
            policy.context_size,
            bench,
            &key,
            &peer.uuid.to_string(),
            MoeCacheCap::None,
        )?;
        return Some(ChosenLoad {
            filename: row.filename.clone(),
            digest: row.digest.clone(),
            context_size: plan.context_size,
            predicted_tok_s: plan.predicted_tok_s,
            interactive: plan.predicted_tok_s >= snap.floor,
            backend: "bmoe".to_string(),
            moe_cache_ceil_mb: Some(plan.cache_mb),
            tags: row.tags.clone(),
            kv_dtype: None,
        });
    }
    let choice = plan_fit_ladder(
        std::slice::from_ref(&row.meta),
        &profile,
        &policy,
        snap.floor,
        |context_size, _, meta| predict_file(meta, peer, context_size, bench),
    )?;
    Some(ChosenLoad {
        filename: row.filename.clone(),
        digest: row.digest.clone(),
        context_size: choice.context_size,
        predicted_tok_s: choice.predicted_tok_s,
        interactive: choice.interactive,
        backend: "auto".to_string(),
        moe_cache_ceil_mb: None,
        tags: row.tags.clone(),
        kv_dtype: Some(choice.kv_dtype),
    })
}

fn prefer_plan(candidate: ChosenLoad, best: &mut Option<ChosenLoad>) {
    let replace = match best {
        None => true,
        Some(current) => match (candidate.interactive, current.interactive) {
            (true, false) => true,
            (false, true) => false,
            _ => candidate.predicted_tok_s > current.predicted_tok_s,
        },
    };
    if replace {
        *best = Some(candidate);
    }
}

fn route_text(reason: RouteStay, tag: &str) -> String {
    match reason {
        RouteStay::Fallback => "fallback".to_string(),
        RouteStay::NoInteractivePeer => format!("{tag}; reason=no interactive peer"),
        _ => {
            if tag.is_empty() {
                "general".to_string()
            } else {
                tag.to_string()
            }
        }
    }
}

fn model_stem(filename: &str) -> String {
    Path::new(filename)
        .file_stem()
        .and_then(|stem| stem.to_str())
        .filter(|stem| !stem.is_empty())
        .unwrap_or(filename)
        .to_string()
}

fn body_with_model(body: &Bytes, model: &str) -> Bytes {
    let Ok(mut value) = serde_json::from_slice::<serde_json::Value>(body) else {
        return body.clone();
    };
    if let Some(object) = value.as_object_mut() {
        object.insert(
            "model".to_string(),
            serde_json::Value::String(model.to_string()),
        );
    }
    serde_json::to_vec(&value)
        .map(Bytes::from)
        .unwrap_or_else(|_| body.clone())
}

async fn handle_appliance_chat(ctx: Arc<GatewayContext>, body_bytes: Bytes) -> Response<RespBody> {
    let value: serde_json::Value = match serde_json::from_slice(&body_bytes) {
        Ok(value) => value,
        Err(err) => {
            return json_response(
                StatusCode::BAD_REQUEST,
                &json!({"error": {"message": format!("invalid JSON: {err}"), "type": "invalid_request"}}),
            );
        }
    };
    let Some(snap) = read_appliance_snap(&ctx) else {
        return json_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            &json!({"error": {"message": "config lock poisoned", "type": "internal"}}),
        );
    };
    if !ctx.supervisor.is_running().await {
        warn_front_door_once_from(&ctx);
    }
    let prompt = user_prompt(&value);
    let (local_loaded, local_tags, local_model) = local_front_door_tags(&ctx).await;
    let tier1 = tier1_tag(&prompt, &snap.presets_dir, &local_tags);
    let tier2 = if tier1.is_none() && local_loaded {
        Some(
            tier2_tag(
                &prompt,
                &snap.presets_dir,
                &ctx.local_upstream(),
                &local_model,
            )
            .await,
        )
    } else {
        None
    };
    let decision = match &tier2 {
        None => decide_appliance(&local_tags, local_loaded, tier1.as_deref(), None),
        Some(Ok(tag)) => decide_appliance(&local_tags, local_loaded, None, Some(Ok(tag.as_str()))),
        Some(Err(())) => decide_appliance(&local_tags, local_loaded, None, Some(Err(()))),
    };
    let asked = tier1
        .clone()
        .or_else(|| match &tier2 {
            Some(Ok(tag)) => Some(tag.clone()),
            Some(Err(())) => Some("fallback".to_string()),
            None => None,
        })
        .unwrap_or_else(|| "general".to_string());

    match decision {
        ApplianceDecision::Unavailable { tag } => json_response(
            StatusCode::SERVICE_UNAVAILABLE,
            &no_interactive_peer_body(&tag),
        ),
        ApplianceDecision::Local { reason } => {
            let header = format_route_header(
                &snap.label,
                "local",
                &local_model,
                &route_text(reason, &asked),
            );
            proxy_upstream(
                &ctx.local_upstream(),
                "/v1/chat/completions",
                body_bytes,
                Some(header),
                true,
                None,
            )
            .await
        }
        ApplianceDecision::Escalate { tag } => {
            escalate_chat(ctx, body_bytes, &snap, &tag, local_loaded, &local_model).await
        }
    }
}

fn warn_front_door_once_from(ctx: &GatewayContext) {
    if let Ok(cfg) = ctx.config.read() {
        warn_front_door_once(&cfg);
    }
}

async fn escalate_chat(
    ctx: Arc<GatewayContext>,
    body_bytes: Bytes,
    snap: &ApplianceSnap,
    tag: &str,
    local_loaded: bool,
    local_model: &str,
) -> Response<RespBody> {
    let require_trust = ctx
        .config
        .read()
        .map(|cfg| pairing_enforced(&cfg.network.security))
        .unwrap_or(false);
    let peers = match &ctx.discovery {
        Some(discovery) => discovery.get_active_peers().await,
        None => Vec::new(),
    };
    let pantry = load_tagged_pantry(&snap.models_dir, &snap.presets_dir, tag);
    let bench = BenchStore::load_default().ok();
    let mut rows: Vec<(PeerNode, BurstScore, Option<ChosenLoad>)> = Vec::new();
    for peer in peers {
        if peer.uuid == ctx.node_id || !peer.status.is_ready() {
            continue;
        }
        if require_trust {
            if let Some(discovery) = &ctx.discovery {
                if !discovery.is_peer_trusted_for_routing(peer.uuid).await {
                    continue;
                }
            }
        }
        let mut best: Option<ChosenLoad> = None;
        for row in &pantry {
            if let Some(plan) = plan_tagged_file(row, &peer, snap, bench.as_ref()) {
                prefer_plan(plan, &mut best);
            }
        }
        let predicted = best.as_ref().map(|plan| plan.predicted_tok_s);
        let interactive = best.as_ref().is_some_and(|plan| plan.interactive);
        let score = score_burst_candidate(
            peer.label(),
            peer.link_class,
            predicted,
            interactive,
            peer.thermal_index,
            peer.battery_percent,
            snap.battery_floor,
            &snap.battery_action,
        );
        // No plan or a batch-only plan is a different skip. Do not call it battery or thermal.
        if predicted.is_some() && interactive {
            for line in burst_candidate_skip_lines(
                peer.thermal_index,
                peer.battery_percent,
                snap.battery_floor,
                &snap.battery_action,
            ) {
                warn!("appliance escalation skip {}: {line}", peer.label());
            }
        }
        rows.push((peer, score, best));
    }

    let mut scores: Vec<BurstScore> = rows.iter().map(|(_, score, _)| score.clone()).collect();
    let mut winner = select_burst_winner(&scores);
    let mut attempts = 0;
    let mut prepared: Option<(usize, String, String)> = None;
    while let Some(index) = winner {
        if attempts >= 2 {
            break;
        }
        attempts += 1;
        let loaded = if let Some(chosen) = rows[index].2.as_ref() {
            ensure_peer_model(&ctx, &rows[index].0, chosen, snap).await
        } else {
            Err(())
        };
        match loaded {
            Ok((api, loaded_name)) => {
                prepared = Some((index, api, loaded_name));
                break;
            }
            Err(()) => {
                warn!(
                    "appliance escalation to {} failed for tag {tag}",
                    rows[index].0.label()
                );
                scores[index].eligible = false;
                if attempts >= 2 {
                    break;
                }
                winner = select_burst_winner(&scores);
            }
        }
    }

    if let Some((index, api, model)) = prepared {
        ctx.burst_activity.touch(rows[index].0.uuid);
        let peer = &rows[index].0;
        let header = format_route_header(
            &peer.label(),
            link_header_token(peer.link_class),
            &model,
            tag,
        );
        let body = body_with_model(&body_bytes, &model_stem(&model));
        let burst = Some((ctx.burst_activity.clone(), rows[index].0.uuid));
        return proxy_upstream(
            &api,
            "/v1/chat/completions",
            body,
            Some(header),
            true,
            burst,
        )
        .await;
    }

    match decision_after_rank(
        ApplianceDecision::Escalate {
            tag: tag.to_string(),
        },
        None,
        local_loaded,
    ) {
        Err(status) => json_response(status, &no_interactive_peer_body(tag)),
        Ok(ApplianceDecision::Local { reason }) => {
            let header =
                format_route_header(&snap.label, "local", local_model, &route_text(reason, tag));
            proxy_upstream(
                &ctx.local_upstream(),
                "/v1/chat/completions",
                body_bytes,
                Some(header),
                true,
                None,
            )
            .await
        }
        Ok(_) => json_response(
            StatusCode::SERVICE_UNAVAILABLE,
            &no_interactive_peer_body(tag),
        ),
    }
}

async fn ensure_peer_model(
    ctx: &GatewayContext,
    peer: &PeerNode,
    chosen: &ChosenLoad,
    snap: &ApplianceSnap,
) -> Result<(String, String), ()> {
    let written = format!("{}.gguf", chosen.digest.trim().to_lowercase());
    if model_matches(&chosen.filename, &peer.active_model)
        || model_matches(&written, &peer.active_model)
        || model_matches(&model_stem(&chosen.filename), &peer.active_model)
    {
        let name = if peer.active_model.trim().is_empty() {
            written
        } else {
            peer.active_model.clone()
        };
        return Ok((peer.api_endpoint(), name));
    }
    let client = reqwest::Client::new();
    let control = peer.control_endpoint();
    let catalog = fetch_models(&client, &control).await.map_err(|_| ())?;
    let rows: Vec<(&str, &str)> = catalog
        .models
        .iter()
        .map(|entry| (entry.filename.as_str(), entry.digest.as_str()))
        .collect();
    let load_name = if let Some(name) = catalog_filename_for_digest(rows, &chosen.digest) {
        name
    } else {
        if chosen.digest.is_empty() {
            return Err(());
        }
        let device_port =
            pantry_reverse_device_port(peer.api_port, peer.control_port, peer.rpc_port);
        let source_base = pantry_source_base(
            peer.link_class,
            &get_local_ip().to_string(),
            snap.control_port,
            device_port,
        )?;
        if peer.link_class == LinkClass::Usb {
            let serial = peer.usb_serial.as_deref().ok_or(())?;
            crate::tunnel::install_control_reverse(serial, device_port, snap.control_port)
                .map_err(|_| ())?;
        }
        let _source = blob_url(&source_base, &chosen.digest).map_err(|_| ())?;
        let request = BlobFetchRequest {
            protocol_version: CONTROL_PLANE_VERSION,
            requester_id: ctx.node_id,
            digest: chosen.digest.clone(),
            source_base_url: source_base,
        };
        let identity = if snap.pairing {
            ctx.identity.as_deref()
        } else {
            None
        };
        let response = request_blob_fetch(&client, &control, &request, identity)
            .await
            .map_err(|_| ())?;
        if !response.accepted {
            return Err(());
        }
        // `accepted` only queues the write. Load the filename the catalog shows after it lands.
        poll_catalog_for_digest(&client, &control, &chosen.digest).await?
    };
    let (cache_type_k, cache_type_v) = cache_type_pair(chosen.kv_dtype);
    let request = ModelLoadRequest {
        protocol_version: CONTROL_PLANE_VERSION,
        requester_id: ctx.node_id,
        model_path: load_name.clone(),
        context_size: chosen.context_size,
        gpu_layers: peer_offload_layers(peer.backend),
        threads: snap.threads.max(1),
        rpc_workers: Vec::new(),
        tags: chosen.tags.clone(),
        target_port: None,
        backend: chosen.backend.clone(),
        moe_cache_ceil_mb: chosen.moe_cache_ceil_mb,
        cache_type_k,
        cache_type_v,
    };
    let loaded = if snap.pairing {
        let identity = ctx.identity.as_ref().ok_or(())?;
        dispatch_load_model_signed(&client, &control, &request, identity).await
    } else {
        dispatch_load_model(&client, &control, &request).await
    }
    .map_err(|_| ())?;
    if !loaded.success {
        return Err(());
    }
    let api = if !loaded.api_endpoint.is_empty() && !loaded.api_endpoint.contains("0.0.0.0") {
        loaded.api_endpoint
    } else {
        peer.api_endpoint()
    };
    Ok((api, load_name))
}

/// Poll the peer catalog until `digest` has a filename, or the wait elapses.
async fn poll_catalog_for_digest(
    client: &reqwest::Client,
    control: &str,
    digest: &str,
) -> Result<String, ()> {
    let deadline = tokio::time::Instant::now() + PANTRY_CATALOG_WAIT;
    loop {
        let catalog = fetch_models(client, control).await.map_err(|_| ())?;
        let rows: Vec<(&str, &str)> = catalog
            .models
            .iter()
            .map(|entry| (entry.filename.as_str(), entry.digest.as_str()))
            .collect();
        if let Some(name) = catalog_filename_for_digest(rows, digest) {
            return Ok(name);
        }
        if tokio::time::Instant::now() >= deadline {
            return Err(());
        }
        tokio::time::sleep(PANTRY_CATALOG_INTERVAL).await;
    }
}

/// Load the section 10.2 file into `supervisor`. Escalation does not stop it.
pub async fn load_appliance_front_door(
    supervisor: &SupervisorManager,
    discovery: &DiscoveryService,
    cfg: &NexusConfig,
    binary: &Path,
    host: &str,
    port: u16,
) -> Option<String> {
    if !cfg.inference.appliance.enabled {
        return None;
    }
    let plan = plan_front_door(cfg).or_else(|| {
        warn_front_door_once(cfg);
        None
    })?;
    let profile = SystemProfile::probe();
    let gpu_layers = if cfg.hardware.acceleration.prefer_gpu {
        cfg.hardware.acceleration.gpu_layers
    } else {
        0
    };
    let threads = if cfg.hardware.acceleration.cpu_threads > 0 {
        cfg.hardware.acceleration.cpu_threads
    } else {
        profile.recommended_threads.max(1)
    };
    let mut server = LlamaServerConfig::new(binary, &plan.model_path, host, port);
    server.gpu_layers = gpu_layers;
    server.threads = threads;
    server.use_mmap = cfg.hardware.safety.mmap;
    server.use_mlock = cfg.hardware.safety.mlock;
    server.cpu_threads_batch = cfg.hardware.acceleration.cpu_threads_batch.max(1);
    server.fallback_to_cpu = cfg.hardware.acceleration.fallback_to_cpu;
    server.memory_budget_percent = cfg.hardware.safety.max_ram_usage_percent;
    server.tags = plan.tags.clone();
    apply_front_door_rung(&mut server, &plan);
    if cfg.inference.cache.prompt_cache_enabled {
        server.slot_save_path = Some(PathBuf::from(&cfg.inference.cache.slot_save_path));
    }
    match supervisor.spawn(server).await {
        Ok(()) => {
            discovery.set_active_model(&plan.filename).await;
            let mut status = StatusFlags::READY;
            if profile.detected_backend == AccelerationBackend::Vulkan {
                status.0 |= StatusFlags::VULKAN_ACTIVE.0;
            }
            discovery.set_status_flags(status).await;
            tracing::info!("appliance front door loaded: {}", plan.filename);
            Some(plan.filename)
        }
        Err(err) => {
            tracing::warn!("appliance front door did not start: {err}");
            None
        }
    }
}

/// Schedule idle burst unloads off the UI task. The local front door is never unloaded.
pub fn unload_idle_burst_peers(ctx: &Arc<GatewayContext>) {
    let idle_secs = {
        let Ok(snap) = ctx.config.read() else {
            return;
        };
        if !snap.inference.appliance.enabled {
            return;
        }
        snap.inference.appliance.burst_idle_secs
    };
    let idle = ctx.burst_activity.idle_candidates(idle_secs, ctx.node_id);
    if idle.is_empty() {
        return;
    }
    let ctx = Arc::clone(ctx);
    tokio::spawn(async move {
        unload_idle_ids(ctx, idle, idle_secs).await;
    });
}

async fn unload_idle_ids(
    ctx: Arc<GatewayContext>,
    idle: Vec<(uuid::Uuid, std::time::Instant)>,
    idle_secs: u64,
) {
    let pairing = ctx
        .config
        .read()
        .map(|snap| snap.network.security.pairing_enforced())
        .unwrap_or(false);
    let Some(discovery) = ctx.discovery.clone() else {
        for (id, started) in idle {
            ctx.burst_activity
                .finish_idle_unload(id, started, false, idle_secs, ctx.node_id);
        }
        return;
    };
    let peers = discovery.get_active_peers().await;
    let client = reqwest::Client::new();
    for (id, started) in idle {
        if id == ctx.node_id {
            ctx.burst_activity
                .finish_idle_unload(id, started, false, idle_secs, ctx.node_id);
            continue;
        }
        if !ctx.burst_activity.claim_unload(id) {
            continue;
        }
        if !ctx
            .burst_activity
            .still_the_same_idle(id, started, idle_secs, ctx.node_id)
        {
            ctx.burst_activity
                .finish_idle_unload(id, started, false, idle_secs, ctx.node_id);
            continue;
        }
        let Some(peer) = peers.iter().find(|peer| peer.uuid == id) else {
            warn!("burst idle unload skipped; peer {id} is not in this tick");
            ctx.burst_activity
                .finish_idle_unload(id, started, false, idle_secs, ctx.node_id);
            continue;
        };
        let request = ModelUnloadRequest {
            protocol_version: CONTROL_PLANE_VERSION,
            requester_id: ctx.node_id,
            model_path: None,
        };
        let result = if pairing {
            match ctx.identity.as_ref() {
                Some(identity) => {
                    dispatch_unload_model_signed(
                        &client,
                        &peer.control_endpoint(),
                        &request,
                        identity,
                    )
                    .await
                }
                None => dispatch_unload_model(&client, &peer.control_endpoint(), &request).await,
            }
        } else {
            dispatch_unload_model(&client, &peer.control_endpoint(), &request).await
        };
        let ok = match &result {
            Ok(response) => response.success,
            Err(err) => {
                warn!("burst idle unload for {} failed: {err}", peer.label());
                false
            }
        };
        if !ok {
            if let Ok(response) = &result {
                warn!(
                    "burst idle unload for {} was refused: {}",
                    peer.label(),
                    response.message
                );
            }
        }
        ctx.burst_activity
            .finish_idle_unload(id, started, ok, idle_secs, ctx.node_id);
    }
}

/// Forwards upstream bytes. A successful SSE body that ends without `[DONE]`
/// gains one status-role `peer lost` event. The prompt is not sent again.
/// Burst activity is refreshed on each chunk and when the stream is dropped
/// so a long reply is not treated as idle.
struct PeerLostStream<S> {
    inner: S,
    carry: String,
    saw_done: bool,
    exhausted: bool,
    annotate: bool,
    burst: Option<(BurstActivity, Uuid)>,
}

impl<S> PeerLostStream<S> {
    fn new(inner: S, annotate: bool, burst: Option<(BurstActivity, Uuid)>) -> Self {
        Self {
            inner,
            carry: String::new(),
            saw_done: false,
            exhausted: false,
            annotate,
            burst,
        }
    }

    fn touch(&self) {
        if let Some((activity, id)) = &self.burst {
            activity.touch(*id);
        }
    }
}

impl<S> Drop for PeerLostStream<S> {
    fn drop(&mut self) {
        self.touch();
    }
}

impl<S> Stream for PeerLostStream<S>
where
    S: Stream<Item = Result<Bytes, std::io::Error>> + Unpin,
{
    type Item = Result<Bytes, std::io::Error>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        if this.exhausted {
            return Poll::Ready(None);
        }
        match Pin::new(&mut this.inner).poll_next(cx) {
            Poll::Ready(Some(Ok(bytes))) => {
                this.touch();
                if this.annotate && !this.saw_done && observe_stream_done(&mut this.carry, &bytes) {
                    this.saw_done = true;
                }
                Poll::Ready(Some(Ok(bytes)))
            }
            Poll::Ready(Some(Err(_))) | Poll::Ready(None) => {
                this.exhausted = true;
                this.touch();
                if !this.annotate || this.saw_done {
                    Poll::Ready(None)
                } else {
                    this.saw_done = true;
                    Poll::Ready(Some(Ok(Bytes::from(peer_lost_sse()))))
                }
            }
            Poll::Pending => Poll::Pending,
        }
    }
}

fn full_body(bytes: Bytes) -> RespBody {
    Full::new(bytes)
        .map_err(|never| match never {})
        .boxed_unsync()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::discovery::{NodeRole, StatusFlags};
    use crate::sysinfo::PowerSource;
    use std::collections::HashMap;
    use std::time::Instant;
    use uuid::Uuid;

    fn dense_950() -> GgufMetadata {
        GgufMetadata {
            version: 3,
            tensor_count: 0,
            kv_count: 0,
            metadata: HashMap::new(),
            architecture: Some("llama".into()),
            model_name: Some("coder".into()),
            context_length: Some(4096),
            block_count: Some(28),
            head_count: Some(12),
            head_count_kv: Some(12),
            embedding_length: Some(1536),
            expert_count: None,
            expert_used_count: None,
            file_size_bytes: 950 * 1024 * 1024,
            tensors: vec![],
            quant_label: Some("Q4_K".into()),
        }
    }

    fn snap() -> ApplianceSnap {
        ApplianceSnap {
            floor: 3.0,
            battery_floor: 20,
            battery_action: "decline".into(),
            max_ram: 75,
            mmap: true,
            mlock: false,
            presets_dir: PathBuf::from("presets"),
            models_dir: PathBuf::from("models"),
            control_port: 9998,
            label: "hub".into(),
            pairing: false,
            threads: 2,
            moe: NexusConfig::default().inference.moe,
        }
    }

    fn peer(backend: AccelerationBackend) -> PeerNode {
        PeerNode {
            uuid: Uuid::from_u128(9),
            addr: "10.0.0.8:8080".parse().unwrap(),
            role: NodeRole::HOST,
            status: StatusFlags::READY,
            api_port: 8080,
            rpc_port: 50052,
            control_port: 9998,
            total_ram_mb: 4_000,
            free_ram_mb: 2_000,
            backend,
            thermal_index: 0,
            active_model: String::new(),
            display_name: "phone".into(),
            moe_stream: false,
            moe_cache_ceil_mb: 0,
            link_class: LinkClass::Usb,
            usb_serial: Some("serial-a".into()),
            usb_api_endpoint: Some("http://127.0.0.1:18080".into()),
            usb_control_endpoint: Some("http://127.0.0.1:18081".into()),
            battery_percent: Some(80),
            power_source: PowerSource::Ac,
            last_seen: Instant::now(),
        }
    }

    fn empty_update(battery_action: Option<String>) -> ConfigUpdateRequest {
        ConfigUpdateRequest {
            prompt_cache_enabled: None,
            slot_save_path: None,
            max_cache_mb: None,
            battery_floor_percent: None,
            battery_action,
            max_ram_usage_percent: None,
            moe_enabled: None,
            moe_cache_mb: None,
            moe_cache_ceil_mb: None,
            moe_quality_mode: None,
            moe_overlap: None,
            moe_drop_cold_experts: None,
            moe_expert_substitute: None,
            moe_route_ahead: None,
        }
    }

    #[test]
    fn remote_load_request_carries_ladder_kv_dtype() {
        let row = PantryRow {
            filename: "coder.gguf".into(),
            digest: "abc".into(),
            tags: vec!["coder".into()],
            meta: dense_950(),
        };
        let cpu = peer(AccelerationBackend::X86Baseline);
        let chosen = plan_tagged_file(&row, &cpu, &snap(), None).expect("2048 q8 fits");
        assert_eq!(chosen.context_size, 2048);
        assert_eq!(chosen.kv_dtype, Some(KvCacheDtype::Q8_0));
        let (cache_type_k, cache_type_v) = cache_type_pair(chosen.kv_dtype);
        let phone = peer(AccelerationBackend::Vulkan);
        let request = ModelLoadRequest {
            protocol_version: CONTROL_PLANE_VERSION,
            requester_id: Uuid::from_u128(1),
            model_path: chosen.filename.clone(),
            context_size: chosen.context_size,
            gpu_layers: peer_offload_layers(phone.backend),
            threads: 2,
            rpc_workers: Vec::new(),
            tags: chosen.tags.clone(),
            target_port: None,
            backend: chosen.backend.clone(),
            moe_cache_ceil_mb: chosen.moe_cache_ceil_mb,
            cache_type_k,
            cache_type_v,
        };
        assert_eq!(request.context_size, 2048);
        assert_eq!(request.cache_type_k.as_deref(), Some("q8_0"));
        assert_eq!(request.cache_type_v.as_deref(), Some("q8_0"));
        assert_eq!(request.gpu_layers, 99);
        assert_eq!(peer_offload_layers(AccelerationBackend::X86Baseline), 0);
        assert_eq!(peer_offload_layers(cpu.backend), 0);
    }

    #[test]
    fn handle_config_update_does_not_persist_sleep_battery_action() {
        let path = std::env::temp_dir().join(format!(
            "nexus-config-sleep-{}-{}.toml",
            std::process::id(),
            Uuid::new_v4()
        ));
        let mut cfg = NexusConfig::default();
        cfg.save_to_path(&path).expect("seed config");
        let before = std::fs::read(&path).expect("read seed");
        let err = apply_settings_update(&mut cfg, &empty_update(Some("sleep".into())))
            .expect_err("sleep is not a battery action");
        assert!(err.contains("sleep"), "{err}");
        assert_eq!(cfg.hardware.safety.battery_action, "decline");
        assert_eq!(std::fs::read(&path).expect("reread"), before);
        let text = String::from_utf8_lossy(&before);
        assert!(!text.contains("sleep"));
        let _ = std::fs::remove_file(&path);
    }
}

fn json_response(status: StatusCode, value: &impl serde::Serialize) -> Response<RespBody> {
    let body = serde_json::to_vec(value).unwrap_or_else(|_| b"{}".to_vec());
    Response::builder()
        .status(status)
        .header(hyper::header::CONTENT_TYPE, "application/json")
        .body(full_body(Bytes::from(body)))
        .unwrap_or_else(|_| Response::new(full_body(Bytes::from_static(b"{}"))))
}

#[cfg(test)]
mod unit_tests {
    use super::*;

    #[test]
    fn model_matches_filename_and_stem() {
        assert!(model_matches("phi.gguf", "phi.gguf"));
        assert!(model_matches("phi", "phi.gguf"));
        assert!(model_matches("PHI.GGUF", "phi.gguf"));
        assert!(model_matches("phi.gguf", "/models/phi.gguf"));
        assert!(!model_matches("phi", "llama.gguf"));
        assert!(!model_matches("", "phi.gguf"));
    }

    #[test]
    fn no_interactive_peer_stays_or_503() {
        let tags = crate::appliance::front_door_tags(&[]);
        let go = crate::appliance::decide_appliance(&tags, true, Some("coder"), None);
        let stayed = decision_after_rank(go.clone(), None, true).expect("loaded model stays");
        match stayed {
            ApplianceDecision::Local { reason } => {
                assert_eq!(reason.as_reason(), "no interactive peer");
            }
            other => panic!("expected local stay, got {other:?}"),
        }
        let status = decision_after_rank(go, None, false).expect_err("missing model is 503");
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        let body = no_interactive_peer_body("coder");
        assert_eq!(body["error"]["type"], "no_interactive_peer");
        assert!(body["error"]["message"]
            .as_str()
            .unwrap_or("")
            .contains("coder"));
    }

    #[test]
    fn status_role_is_removed_before_the_model_prompt() {
        let raw = br#"{"model":"default","messages":[{"role":"user","content":"hi"},{"role":"status","content":"phone-a coder-7b USB 18 tok/s"},{"role":"STATUS","content":"peer lost"}]}"#;
        let stripped = strip_status_messages(&Bytes::from_static(raw));
        let value: serde_json::Value = serde_json::from_slice(&stripped).unwrap();
        let roles: Vec<&str> = value["messages"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|message| message["role"].as_str())
            .collect();
        assert_eq!(roles, vec!["user"]);
        let text = String::from_utf8_lossy(&stripped);
        assert!(!text.contains("peer lost"));
        assert!(!text.contains("USB"));
    }
}
