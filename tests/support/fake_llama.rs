//! Minimal OpenAI-compatible `llama-server` stub for CI.
//!
//! Serves `/health`, `/v1/models`, and canned SSE `/v1/chat/completions`.
//! No real GGUF or llama.cpp binary required.

use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::body::Incoming;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Method, Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::net::TcpListener;
use tokio::task::JoinHandle;

/// Configuration for the fake llama-server.
#[derive(Clone)]
pub struct FakeLlamaConfig {
    pub model_id: String,
    pub sse_tokens: Vec<String>,
}

impl Default for FakeLlamaConfig {
    fn default() -> Self {
        Self {
            model_id: "fake-model".to_string(),
            sse_tokens: vec!["Hello".into(), " from".into(), " nexus".into()],
        }
    }
}

/// Spawn an ephemeral fake llama-server. Returns (base URL, join handle).
pub async fn spawn_fake_llama(cfg: FakeLlamaConfig) -> std::io::Result<(String, JoinHandle<()>)> {
    let listener = TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], 0))).await?;
    let addr = listener.local_addr()?;
    let cfg = Arc::new(cfg);
    let handle = tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                break;
            };
            let cfg = cfg.clone();
            tokio::spawn(async move {
                let io = TokioIo::new(stream);
                let service = service_fn(move |req| {
                    let cfg = cfg.clone();
                    async move { Ok::<_, Infallible>(route(req, &cfg).await) }
                });
                let _ = http1::Builder::new().serve_connection(io, service).await;
            });
        }
    });
    Ok((format!("http://{}", addr), handle))
}

async fn route(req: Request<Incoming>, cfg: &FakeLlamaConfig) -> Response<Full<Bytes>> {
    let path = req.uri().path().to_string();
    let method = req.method().clone();

    // Drain body so keep-alive clients stay happy.
    let _ = req.collect().await;

    match (method, path.as_str()) {
        (Method::GET, "/health") => json_response(StatusCode::OK, r#"{"status":"ok"}"#),
        (Method::GET, "/v1/models") => {
            let body = format!(
                r#"{{"object":"list","data":[{{"id":"{}","object":"model","owned_by":"nexus-fake"}}]}}"#,
                cfg.model_id
            );
            json_response(StatusCode::OK, &body)
        }
        (Method::POST, "/v1/chat/completions") => {
            // Always stream canned SSE (client sets stream=true for stream_chat).
            let mut sse = String::new();
            for token in &cfg.sse_tokens {
                let chunk = format!(
                    r#"{{"id":"chatcmpl-fake","choices":[{{"index":0,"delta":{{"content":{}}},"finish_reason":null}}]}}"#,
                    serde_json::to_string(token).unwrap_or_else(|_| "\"\"".into())
                );
                sse.push_str("data: ");
                sse.push_str(&chunk);
                sse.push_str("\n\n");
            }
            sse.push_str("data: [DONE]\n\n");
            Response::builder()
                .status(StatusCode::OK)
                .header("content-type", "text/event-stream")
                .header("cache-control", "no-cache")
                .body(Full::new(Bytes::from(sse)))
                .unwrap_or_else(|_| empty(StatusCode::INTERNAL_SERVER_ERROR))
        }
        _ => empty(StatusCode::NOT_FOUND),
    }
}

fn json_response(status: StatusCode, body: &str) -> Response<Full<Bytes>> {
    Response::builder()
        .status(status)
        .header("content-type", "application/json")
        .body(Full::new(Bytes::from(body.to_string())))
        .unwrap_or_else(|_| empty(StatusCode::INTERNAL_SERVER_ERROR))
}

fn empty(status: StatusCode) -> Response<Full<Bytes>> {
    Response::builder()
        .status(status)
        .body(Full::new(Bytes::new()))
        .unwrap()
}
