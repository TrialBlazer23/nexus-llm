//! OpenAI-compatible mesh gateway (Phase 12 §5.1).
//!
//! Binds `network.gateway_port` (default 8090) and reverse-proxies
//! `/v1/chat/completions` to whichever mesh node currently holds the
//! requested model. Distinct from `api_port` (llama-server) and
//! `control_port` (signed control plane).

use crate::config::NexusConfig;
use crate::control_plane::build_model_catalog;
use crate::discovery::{DiscoveryService, PeerNode};
use crate::node_identity::NodeIdentity;
use crate::supervisor::SupervisorManager;
use crate::sysinfo::SystemProfile;
use crate::trust_auth::pairing_enforced;
use bytes::Bytes;
use futures_util::stream::unfold;
use futures_util::{StreamExt, TryStreamExt};
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
use std::path::PathBuf;
use std::sync::Arc;
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
    if let Some(enabled) = payload.prompt_cache_enabled {
        cfg.inference.cache.prompt_cache_enabled = enabled;
    }
    if let Some(path) = payload.slot_save_path {
        cfg.inference.cache.slot_save_path = path;
    }
    if let Some(max_mb) = payload.max_cache_mb {
        cfg.inference.cache.max_cache_mb = max_mb;
    }
    if let Some(floor) = payload.battery_floor_percent {
        cfg.hardware.safety.battery_floor_percent = floor;
    }
    if let Some(action) = payload.battery_action {
        cfg.hardware.safety.battery_action = action;
    }
    if let Some(ram) = payload.max_ram_usage_percent {
        cfg.hardware.safety.max_ram_usage_percent = ram;
    }
    if let Some(enabled) = payload.moe_enabled {
        cfg.inference.moe.enabled = enabled;
    }
    if let Some(cache_mb) = payload.moe_cache_mb {
        cfg.inference.moe.cache_mb = cache_mb;
    }
    if let Some(ceil) = payload.moe_cache_ceil_mb {
        cfg.inference.moe.cache_ceil_mb = ceil;
    }
    if let Some(mode) = payload.moe_quality_mode {
        match mode.to_ascii_lowercase().as_str() {
            "lossy" => cfg.inference.moe.quality_mode = crate::config::MoeQualityMode::Lossy,
            "lossless" => cfg.inference.moe.quality_mode = crate::config::MoeQualityMode::Lossless,
            _ => {}
        }
    }
    if let Some(overlap) = payload.moe_overlap {
        cfg.inference.moe.overlap = overlap;
    }
    if let Some(drop) = payload.moe_drop_cold_experts {
        cfg.inference.moe.drop_cold_experts = if drop.is_empty() { None } else { Some(drop) };
    }
    if let Some(sub) = payload.moe_expert_substitute {
        cfg.inference.moe.expert_substitute = if sub.is_empty() { None } else { Some(sub) };
    }
    if let Some(n) = payload.moe_route_ahead {
        cfg.inference.moe.route_ahead = if n == 0 { None } else { Some(n) };
    }
    if let Err(e) = cfg.inference.moe.validate() {
        return json_response(StatusCode::BAD_REQUEST, &json!({"error": e.to_string()}));
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

    let model = match serde_json::from_slice::<ChatProxyRequest>(&body_bytes) {
        Ok(parsed) => parsed.model,
        Err(e) => {
            return json_response(
                StatusCode::BAD_REQUEST,
                &json!({"error": {"message": format!("invalid JSON: {e}"), "type": "invalid_request"}}),
            );
        }
    };

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

    proxy_upstream(&resolved.base_url, "/v1/chat/completions", body_bytes).await
}

async fn proxy_upstream(base_url: &str, path: &str, body: Bytes) -> Response<RespBody> {
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

    let byte_stream = upstream
        .bytes_stream()
        .map_err(|e| std::io::Error::other(format!("upstream stream error: {e}")));
    let frame_stream = byte_stream.map_ok(Frame::data);
    let body = StreamBody::new(frame_stream).boxed_unsync();

    Response::builder()
        .status(status)
        .header(hyper::header::CONTENT_TYPE, content_type)
        .header(hyper::header::CACHE_CONTROL, "no-cache")
        .body(body)
        .unwrap_or_else(|_| {
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
}
