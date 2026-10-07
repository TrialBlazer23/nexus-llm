//! OpenAI-compatible mesh gateway (Phase 12 §5.1).
//!
//! Binds `network.gateway_port` (default 8090) and reverse-proxies
//! `/v1/chat/completions` to whichever mesh node currently holds the
//! requested model. Distinct from `api_port` (llama-server) and
//! `control_port` (signed control plane).

use crate::config::NexusConfig;
use crate::control_plane::build_model_catalog;
use crate::discovery::{DiscoveryService, PeerNode};
use crate::supervisor::SupervisorManager;
use crate::trust_auth::pairing_enforced;
use bytes::Bytes;
use futures_util::TryStreamExt;
use http_body_util::{BodyExt, Full, StreamBody};
use hyper::body::{Frame, Incoming};
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Method, Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use serde::Deserialize;
use serde_json::json;
use std::collections::BTreeSet;
use std::convert::Infallible;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use tracing::{info, warn};
use uuid::Uuid;

type RespBody = http_body_util::combinators::UnsyncBoxBody<Bytes, std::io::Error>;

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
    let local_model = local_active_model(ctx).await;
    if let Some(ref local) = local_model {
        if is_default_model(model) || model_matches(model, local) {
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
            candidates.retain(|p| model_matches(model, &p.active_model));
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
    match (method, path.as_str()) {
        (Method::GET, "/health") => json_response(StatusCode::OK, &json!({"status": "ok"})),
        (Method::GET, "/v1/models") => handle_models(ctx).await,
        (Method::POST, "/v1/chat/completions") => handle_chat_completions(req, ctx).await,
        _ => json_response(
            StatusCode::NOT_FOUND,
            &json!({"error": {"message": "not found", "type": "not_found"}}),
        ),
    }
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
    let mut ids: BTreeSet<String> = BTreeSet::new();

    if let Some(local) = local_active_model(&ctx).await {
        ids.insert(local);
    }

    let catalog = build_model_catalog(ctx.node_id, &ctx.models_dir);
    for entry in catalog.models {
        if !entry.filename.is_empty() {
            ids.insert(entry.filename.clone());
            if let Some(stem) = std::path::Path::new(&entry.filename)
                .file_stem()
                .and_then(|s| s.to_str())
            {
                ids.insert(stem.to_string());
            }
        }
        if !entry.digest.is_empty() {
            ids.insert(entry.digest);
        }
    }

    if let Some(discovery) = &ctx.discovery {
        for peer in discovery.get_active_peers().await {
            if !peer.active_model.is_empty() {
                ids.insert(peer.active_model);
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
