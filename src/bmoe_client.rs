//! BigMoeOnEdge `bmoe-cli --session` client and OpenAI/SSE adapter (Phase 16).
//!
//! Nexus shells out to the external `bmoe-cli` binary (no C++ bindings). The
//! session protocol uses JSON requests on stdin and `BMOE_*` lines on stdout.

use crate::client::{ChatCompletionRequest, ChatMessage};
use crate::config::MoeConfig;
use crate::gguf::GgufMetadata;
use crate::sysinfo::SystemProfile;
use bytes::Bytes;
use futures_util::stream::unfold;
use http_body_util::{BodyExt, Full, StreamBody};
use hyper::body::{Frame, Incoming};
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Method, Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use serde::Deserialize;
use serde_json::json;
use std::convert::Infallible;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;
use thiserror::Error;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::sync::{mpsc, Mutex};
use tracing::{debug, info, warn};

type RespBody = http_body_util::combinators::UnsyncBoxBody<Bytes, std::io::Error>;

#[derive(Error, Debug)]
pub enum BmoeError {
    #[error("bmoe-cli binary not found at: {0}")]
    BinaryNotFound(PathBuf),
    #[error("model file not found at: {0}")]
    ModelNotFound(PathBuf),
    #[error("Android LMK Guard (MoE stream): required {required_mb} MB exceeds ceiling {max_allowed_mb} MB of {available_mb} MB available")]
    MemoryCapExceeded {
        required_mb: u64,
        available_mb: u64,
        max_allowed_mb: u64,
    },
    #[error("failed to spawn bmoe-cli: {0}")]
    SpawnFailed(String),
    #[error("bmoe-cli did not become ready: {0}")]
    NotReady(String),
    #[error("session protocol error: {0}")]
    Protocol(String),
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),
}

/// CLI configuration for spawning `bmoe-cli --session --moe-stream …`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BmoeSessionConfig {
    pub binary_path: PathBuf,
    pub model_path: PathBuf,
    pub context_size: usize,
    pub threads: usize,
    pub host: String,
    pub port: u16,
    pub moe: MoeConfig,
    pub memory_budget_percent: u8,
    /// Effective expert cache ceiling used for LMK + `--cache-ceil-mb`.
    pub cache_ceil_mb: u64,
    pub tags: Vec<String>,
}

impl BmoeSessionConfig {
    pub fn build_args(&self) -> Vec<String> {
        let mut args = vec![
            "-m".into(),
            self.model_path.to_string_lossy().to_string(),
            "-c".into(),
            self.context_size.to_string(),
            "-t".into(),
            self.threads.to_string(),
            "--moe-stream".into(),
            "--session".into(),
            "--progress".into(),
            "--cache-mb".into(),
            self.moe.resolved_cache_mb_arg(),
            "--dense-weights".into(),
            self.moe.dense_weights.clone(),
            "--io-threads".into(),
            self.moe.io_threads.to_string(),
        ];
        if self.cache_ceil_mb > 0 {
            args.push("--cache-ceil-mb".into());
            args.push(self.cache_ceil_mb.to_string());
        }
        if self.moe.cache_floor_mb > 0 {
            args.push("--cache-floor-mb".into());
            args.push(self.moe.cache_floor_mb.to_string());
        }
        if self.moe.overlap {
            args.push("--overlap".into());
        }
        if self.moe.lossy_allowed() {
            if let Some(ref f) = self.moe.drop_cold_experts {
                args.push("--drop-cold-experts".into());
                args.push(f.clone());
            }
            if let Some(ref l) = self.moe.expert_substitute {
                args.push("--expert-substitute".into());
                args.push(l.clone());
            }
            if let Some(n) = self.moe.route_ahead {
                if n > 0 {
                    args.push("--route-ahead".into());
                    args.push(n.to_string());
                }
            }
        }
        args
    }

    /// Build config from Nexus MoE settings + probed profile.
    #[allow(clippy::too_many_arguments)]
    pub fn from_profile(
        binary: impl Into<PathBuf>,
        model: impl Into<PathBuf>,
        host: impl Into<String>,
        port: u16,
        context_size: usize,
        threads: usize,
        moe: MoeConfig,
        profile: &SystemProfile,
        memory_budget_percent: u8,
        tags: Vec<String>,
    ) -> Self {
        let cache_ceil_mb =
            moe.derive_cache_ceil_mb(profile.available_ram_mb, memory_budget_percent);
        Self {
            binary_path: binary.into(),
            model_path: model.into(),
            context_size,
            threads,
            host: host.into(),
            port,
            moe,
            memory_budget_percent,
            cache_ceil_mb,
            tags,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct BmoeReady {
    pub load_s: Option<f64>,
    pub arch: Option<String>,
    pub n_ctx: Option<u32>,
    pub n_expert_used: Option<u32>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct BmoeProgress {
    pub delta_text: Option<String>,
    pub delta_reasoning: Option<String>,
    pub reset: Option<u8>,
    pub cache_hit_pct: Option<f64>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct BmoeDone {
    pub id: Option<i64>,
    pub cancelled: Option<bool>,
    pub tokens: Option<u32>,
    pub tok_s: Option<f64>,
    pub text: Option<String>,
    pub reasoning: Option<String>,
    pub cache_hit_pct: Option<f64>,
}

#[derive(Debug, Clone)]
pub enum BmoeEvent {
    Begin { id: i64 },
    Progress(BmoeProgress),
    Done(BmoeDone),
    Error { id: i64, fatal: bool, msg: String },
}

/// Live handle to a warm `bmoe-cli --session` process plus its OpenAI HTTP shim.
pub struct BmoeRuntime {
    pub config: BmoeSessionConfig,
    child: Child,
    stdin: Arc<Mutex<ChildStdin>>,
    event_rx: Arc<Mutex<mpsc::UnboundedReceiver<BmoeEvent>>>,
    next_id: AtomicU64,
    ready: BmoeReady,
    adapter_shutdown: Option<tokio::sync::oneshot::Sender<()>>,
    pub state: Arc<std::sync::atomic::AtomicU8>,
}

pub const BMOE_STATE_STARTING: u8 = 0;
pub const BMOE_STATE_READY: u8 = 1;
pub const BMOE_STATE_STOPPED: u8 = 2;
pub const BMOE_STATE_FAILED: u8 = 3;

impl BmoeRuntime {
    pub fn model_name(&self) -> String {
        self.config
            .model_path
            .file_name()
            .map(|f| f.to_string_lossy().to_string())
            .unwrap_or_else(|| "moe-model".into())
    }

    pub fn is_ready(&self) -> bool {
        self.state.load(Ordering::SeqCst) == BMOE_STATE_READY
    }

    pub fn ready_info(&self) -> &BmoeReady {
        &self.ready
    }

    pub fn api_endpoint(&self) -> String {
        format!("http://{}:{}", self.config.host, self.config.port)
    }

    /// Spawn bmoe-cli, wait for BMOE_READY, start OpenAI adapter on config.port.
    pub async fn spawn(config: BmoeSessionConfig) -> Result<Self, BmoeError> {
        if !config.binary_path.exists() && !is_in_path(&config.binary_path) {
            return Err(BmoeError::BinaryNotFound(config.binary_path.clone()));
        }
        if !config.model_path.exists() {
            return Err(BmoeError::ModelNotFound(config.model_path.clone()));
        }

        // Stream LMK: resident + cache + KV (not full GGUF size).
        let gguf = GgufMetadata::open(&config.model_path).ok();
        let profile = SystemProfile::probe();
        let cache_for_guard = if config.cache_ceil_mb > 0 {
            config.cache_ceil_mb
        } else if config.moe.resolved_cache_mb_arg() == "0" {
            0
        } else {
            2000 // conservative lower bound when auto
        };
        if let Some(ref meta) = gguf {
            let footprint = meta.moe_stream_footprint_bytes(config.context_size, cache_for_guard);
            let max_allowed = profile.max_allowed_memory_bytes_pct(config.memory_budget_percent);
            if footprint > max_allowed {
                return Err(BmoeError::MemoryCapExceeded {
                    required_mb: footprint / (1024 * 1024),
                    available_mb: profile.available_ram_mb,
                    max_allowed_mb: max_allowed / (1024 * 1024),
                });
            }
        }

        let args = config.build_args();
        info!(
            "Spawning bmoe-cli --session for {:?} with args {:?}",
            config.model_path, args
        );

        let mut child = Command::new(&config.binary_path)
            .args(&args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .map_err(|e| BmoeError::SpawnFailed(e.to_string()))?;

        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| BmoeError::SpawnFailed("missing stdin".into()))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| BmoeError::SpawnFailed("missing stdout".into()))?;
        let stderr = child.stderr.take();

        if let Some(stderr) = stderr {
            tokio::spawn(async move {
                let mut lines = BufReader::new(stderr).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    debug!("bmoe-cli stderr: {line}");
                }
            });
        }

        let (event_tx, event_rx) = mpsc::unbounded_channel();
        let ready = wait_for_ready(stdout, event_tx.clone()).await?;

        // Continue reading stdout in background after READY.
        // wait_for_ready returns after spawning the reader — see helper.

        let state = Arc::new(std::sync::atomic::AtomicU8::new(BMOE_STATE_READY));
        let stdin = Arc::new(Mutex::new(stdin));
        let event_rx = Arc::new(Mutex::new(event_rx));

        let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
        let adapter_cfg = AdapterState {
            stdin: stdin.clone(),
            event_rx: event_rx.clone(),
            next_id: Arc::new(AtomicU64::new(1)),
            model_name: config
                .model_path
                .file_name()
                .map(|f| f.to_string_lossy().to_string())
                .unwrap_or_else(|| "moe-model".into()),
            context_size: ready
                .n_ctx
                .map(|c| c as usize)
                .unwrap_or(config.context_size),
        };
        let listen_addr = SocketAddr::from(([127, 0, 0, 1], config.port));
        // Prefer binding the configured host when it is loopback/unspecified.
        let bind_addr: SocketAddr = format!("{}:{}", config.host, config.port)
            .parse()
            .unwrap_or(listen_addr);

        tokio::spawn(run_openai_adapter(bind_addr, adapter_cfg, shutdown_rx));

        Ok(Self {
            config,
            child,
            stdin,
            event_rx,
            next_id: AtomicU64::new(1),
            ready,
            adapter_shutdown: Some(shutdown_tx),
            state,
        })
    }

    /// Send a generate request and collect streamed text deltas.
    pub async fn generate(
        &self,
        prompt: &str,
        n_predict: usize,
        clear_kv: bool,
    ) -> Result<String, BmoeError> {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst) as i64;
        let req = json!({
            "cmd": "generate",
            "id": id,
            "prompt": prompt,
            "n_predict": n_predict,
            "clear_kv": clear_kv,
            "think": false,
        });
        {
            let mut stdin = self.stdin.lock().await;
            stdin
                .write_all(format!("{req}\n").as_bytes())
                .await
                .map_err(|e| BmoeError::Protocol(e.to_string()))?;
            stdin
                .flush()
                .await
                .map_err(|e| BmoeError::Protocol(e.to_string()))?;
        }

        let mut text = String::new();
        let mut rx = self.event_rx.lock().await;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(600);
        loop {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            let ev = tokio::time::timeout(remaining, rx.recv())
                .await
                .map_err(|_| BmoeError::Protocol("generate timed out".into()))?
                .ok_or_else(|| BmoeError::Protocol("event channel closed".into()))?;
            match ev {
                BmoeEvent::Begin { id: eid } if eid != id => continue,
                BmoeEvent::Begin { .. } => {}
                BmoeEvent::Progress(p) => {
                    if p.reset == Some(1) {
                        text.clear();
                    }
                    if let Some(d) = p.delta_text {
                        text.push_str(&d);
                    }
                }
                BmoeEvent::Done(d) => {
                    if d.id.is_some_and(|i| i != id) {
                        continue;
                    }
                    if let Some(t) = d.text {
                        if !t.is_empty() {
                            text = t;
                        }
                    }
                    return Ok(text);
                }
                BmoeEvent::Error {
                    id: eid,
                    fatal,
                    msg,
                } => {
                    if eid != id && eid != 0 {
                        continue;
                    }
                    let err = BmoeError::Protocol(msg);
                    if fatal {
                        self.state.store(BMOE_STATE_FAILED, Ordering::SeqCst);
                    }
                    return Err(err);
                }
            }
        }
    }

    pub async fn stop(&mut self) -> Result<(), BmoeError> {
        if let Some(tx) = self.adapter_shutdown.take() {
            let _ = tx.send(());
        }
        {
            let mut stdin = self.stdin.lock().await;
            let _ = stdin.write_all(b"{\"cmd\":\"close\"}\n").await;
            let _ = stdin.flush().await;
        }
        let _ = self.child.kill().await;
        self.state.store(BMOE_STATE_STOPPED, Ordering::SeqCst);
        Ok(())
    }
}

impl Drop for BmoeRuntime {
    fn drop(&mut self) {
        if let Some(tx) = self.adapter_shutdown.take() {
            let _ = tx.send(());
        }
        let _ = self.child.start_kill();
    }
}

/// Read stdout until BMOE_READY, then spawn a background event pump.
async fn wait_for_ready(
    stdout: ChildStdout,
    event_tx: mpsc::UnboundedSender<BmoeEvent>,
) -> Result<BmoeReady, BmoeError> {
    let mut lines = BufReader::new(stdout).lines();
    let deadline = Duration::from_secs(600);
    let start = tokio::time::Instant::now();

    loop {
        let remaining = deadline.saturating_sub(start.elapsed());
        if remaining.is_zero() {
            return Err(BmoeError::NotReady("timeout waiting for BMOE_READY".into()));
        }
        let line = tokio::time::timeout(remaining, lines.next_line())
            .await
            .map_err(|_| BmoeError::NotReady("timeout reading bmoe stdout".into()))?
            .map_err(|e| BmoeError::NotReady(e.to_string()))?
            .ok_or_else(|| BmoeError::NotReady("bmoe-cli exited before READY".into()))?;

        if let Some(rest) = line.strip_prefix("BMOE_READY ") {
            let ready: BmoeReady = serde_json::from_str(rest).unwrap_or(BmoeReady {
                load_s: None,
                arch: None,
                n_ctx: None,
                n_expert_used: None,
            });
            // Pump remaining lines in background.
            tokio::spawn(async move {
                while let Ok(Some(line)) = lines.next_line().await {
                    if let Some(ev) = parse_bmoe_line(&line) {
                        if event_tx.send(ev).is_err() {
                            break;
                        }
                    }
                }
            });
            return Ok(ready);
        }
        if let Some(rest) = line.strip_prefix("BMOE_ERROR ") {
            return Err(BmoeError::NotReady(rest.to_string()));
        }
        debug!("bmoe stdout (pre-ready): {line}");
    }
}

pub fn parse_bmoe_line(line: &str) -> Option<BmoeEvent> {
    if let Some(rest) = line.strip_prefix("BMOE_BEGIN ") {
        let v: serde_json::Value = serde_json::from_str(rest).ok()?;
        let id = v.get("id")?.as_i64()?;
        return Some(BmoeEvent::Begin { id });
    }
    if let Some(rest) = line.strip_prefix("BMOE_PROGRESS ") {
        let p: BmoeProgress = serde_json::from_str(rest).ok()?;
        return Some(BmoeEvent::Progress(p));
    }
    if let Some(rest) = line.strip_prefix("BMOE_DONE ") {
        let d: BmoeDone = serde_json::from_str(rest).ok()?;
        return Some(BmoeEvent::Done(d));
    }
    if let Some(rest) = line.strip_prefix("BMOE_ERROR ") {
        let v: serde_json::Value = serde_json::from_str(rest).ok()?;
        return Some(BmoeEvent::Error {
            id: v.get("id").and_then(|x| x.as_i64()).unwrap_or(0),
            fatal: v.get("fatal").and_then(|x| x.as_bool()).unwrap_or(true),
            msg: v
                .get("msg")
                .and_then(|x| x.as_str())
                .unwrap_or("unknown")
                .to_string(),
        });
    }
    None
}

/// Render OpenAI chat messages into a single prompt string for bmoe generate.
pub fn messages_to_prompt(messages: &[ChatMessage]) -> String {
    let mut out = String::new();
    for m in messages {
        if m.is_status() {
            continue;
        }
        match m.role.as_str() {
            "system" => {
                out.push_str("System: ");
                out.push_str(&m.content);
                out.push_str("\n\n");
            }
            "user" => {
                out.push_str("User: ");
                out.push_str(&m.content);
                out.push_str("\n\n");
            }
            "assistant" => {
                out.push_str("Assistant: ");
                out.push_str(&m.content);
                out.push_str("\n\n");
            }
            _ => {
                out.push_str(&m.content);
                out.push('\n');
            }
        }
    }
    out.push_str("Assistant:");
    out
}

struct AdapterState {
    stdin: Arc<Mutex<ChildStdin>>,
    event_rx: Arc<Mutex<mpsc::UnboundedReceiver<BmoeEvent>>>,
    next_id: Arc<AtomicU64>,
    model_name: String,
    context_size: usize,
}

async fn run_openai_adapter(
    addr: SocketAddr,
    state: AdapterState,
    mut shutdown: tokio::sync::oneshot::Receiver<()>,
) {
    let listener = match tokio::net::TcpListener::bind(addr).await {
        Ok(l) => l,
        Err(e) => {
            warn!("bmoe OpenAI adapter failed to bind {addr}: {e}");
            return;
        }
    };
    info!("bmoe OpenAI adapter listening on http://{addr}");
    let state = Arc::new(state);

    loop {
        tokio::select! {
            _ = &mut shutdown => break,
            accepted = listener.accept() => {
                let Ok((stream, _)) = accepted else { continue };
                let io = TokioIo::new(stream);
                let st = state.clone();
                tokio::spawn(async move {
                    let svc = service_fn(move |req| {
                        let st = st.clone();
                        async move { adapter_handle(req, st).await }
                    });
                    if let Err(e) = http1::Builder::new().serve_connection(io, svc).await {
                        debug!("bmoe adapter conn error: {e}");
                    }
                });
            }
        }
    }
}

async fn adapter_handle(
    req: Request<Incoming>,
    state: Arc<AdapterState>,
) -> Result<Response<RespBody>, Infallible> {
    let path = req.uri().path().to_string();
    match (req.method(), path.as_str()) {
        (&Method::GET, "/health") => Ok(json_response(StatusCode::OK, &json!({"status":"ok"}))),
        (&Method::GET, "/v1/models") => Ok(json_response(
            StatusCode::OK,
            &json!({
                "object": "list",
                "data": [{
                    "id": state.model_name,
                    "object": "model",
                    "owned_by": "nexus-bmoe"
                }]
            }),
        )),
        (&Method::POST, "/v1/chat/completions") => Ok(handle_chat(req, state).await),
        _ => Ok(json_response(
            StatusCode::NOT_FOUND,
            &json!({"error": {"message": "not found", "type": "not_found"}}),
        )),
    }
}

async fn handle_chat(req: Request<Incoming>, state: Arc<AdapterState>) -> Response<RespBody> {
    let body = match req.collect().await {
        Ok(c) => c.to_bytes(),
        Err(e) => {
            return json_response(
                StatusCode::BAD_REQUEST,
                &json!({"error": {"message": e.to_string(), "type": "invalid_request"}}),
            );
        }
    };
    let parsed: ChatCompletionRequest = match serde_json::from_slice(&body) {
        Ok(p) => p,
        Err(e) => {
            return json_response(
                StatusCode::BAD_REQUEST,
                &json!({"error": {"message": e.to_string(), "type": "invalid_request"}}),
            );
        }
    };
    let prompt = messages_to_prompt(&parsed.messages);
    let prompt_est = (prompt.len() / 3).max(1);
    let max_avail = state.context_size.saturating_sub(prompt_est).max(1);
    let n_predict = parsed.max_tokens.unwrap_or(512).min(max_avail);
    let stream = parsed.stream;
    let id = state.next_id.fetch_add(1, Ordering::SeqCst) as i64;
    let req_line = json!({
        "cmd": "generate",
        "id": id,
        "prompt": prompt,
        "n_predict": n_predict,
        "clear_kv": false,
        "think": false,
    });
    {
        let mut stdin = state.stdin.lock().await;
        if let Err(e) = stdin.write_all(format!("{req_line}\n").as_bytes()).await {
            return json_response(
                StatusCode::BAD_GATEWAY,
                &json!({"error": {"message": e.to_string(), "type": "bmoe_error"}}),
            );
        }
        let _ = stdin.flush().await;
    }

    if !stream {
        match collect_generation(&state, id).await {
            Ok(text) => json_response(
                StatusCode::OK,
                &json!({
                    "id": format!("chatcmpl-bmoe-{id}"),
                    "object": "chat.completion",
                    "choices": [{
                        "index": 0,
                        "message": {"role": "assistant", "content": text},
                        "finish_reason": "stop"
                    }]
                }),
            ),
            Err(e) => json_response(
                StatusCode::BAD_GATEWAY,
                &json!({"error": {"message": e.to_string(), "type": "bmoe_error"}}),
            ),
        }
    } else {
        sse_stream_generation(state, id)
    }
}

async fn collect_generation(state: &AdapterState, id: i64) -> Result<String, BmoeError> {
    let mut text = String::new();
    let mut rx = state.event_rx.lock().await;
    loop {
        let ev = rx
            .recv()
            .await
            .ok_or_else(|| BmoeError::Protocol("channel closed".into()))?;
        match ev {
            BmoeEvent::Progress(p) => {
                if p.reset == Some(1) {
                    text.clear();
                }
                if let Some(d) = p.delta_text {
                    text.push_str(&d);
                }
            }
            BmoeEvent::Done(d) => {
                if d.id.is_some_and(|i| i != id) {
                    continue;
                }
                if let Some(t) = d.text {
                    if !t.is_empty() {
                        text = t;
                    }
                }
                return Ok(text);
            }
            BmoeEvent::Error { id: eid, msg, .. } if eid == id || eid == 0 => {
                return Err(BmoeError::Protocol(msg));
            }
            BmoeEvent::Begin { .. } | BmoeEvent::Error { .. } => {}
        }
    }
}

fn sse_stream_generation(state: Arc<AdapterState>, id: i64) -> Response<RespBody> {
    let completion_id = format!("chatcmpl-bmoe-{id}");
    let model = state.model_name.clone();
    let stream = unfold(
        (state, id, completion_id, model, false),
        |(state, id, completion_id, model, done)| async move {
            if done {
                return None;
            }
            let ev = {
                let mut rx = state.event_rx.lock().await;
                rx.recv().await
            };
            match ev {
                Some(BmoeEvent::Progress(p)) => {
                    let delta = p.delta_text.unwrap_or_default();
                    if delta.is_empty() && p.reset != Some(1) {
                        return Some((
                            Ok::<_, std::io::Error>(Frame::data(Bytes::new())),
                            (state, id, completion_id, model, false),
                        ));
                    }
                    let chunk = json!({
                        "id": completion_id,
                        "object": "chat.completion.chunk",
                        "choices": [{
                            "index": 0,
                            "delta": {"content": delta},
                            "finish_reason": null
                        }]
                    });
                    let line = format!("data: {chunk}\n\n");
                    Some((
                        Ok(Frame::data(Bytes::from(line))),
                        (state, id, completion_id, model, false),
                    ))
                }
                Some(BmoeEvent::Done(_)) => {
                    let chunk = json!({
                        "id": completion_id,
                        "object": "chat.completion.chunk",
                        "choices": [{
                            "index": 0,
                            "delta": {},
                            "finish_reason": "stop"
                        }]
                    });
                    let line = format!("data: {chunk}\n\ndata: [DONE]\n\n");
                    Some((
                        Ok(Frame::data(Bytes::from(line))),
                        (state, id, completion_id, model, true),
                    ))
                }
                Some(BmoeEvent::Error { msg, .. }) => {
                    let line = format!(
                        "data: {}\n\n",
                        json!({"error": {"message": msg, "type": "bmoe_error"}})
                    );
                    Some((
                        Ok(Frame::data(Bytes::from(line))),
                        (state, id, completion_id, model, true),
                    ))
                }
                Some(BmoeEvent::Begin { .. }) => Some((
                    Ok(Frame::data(Bytes::new())),
                    (state, id, completion_id, model, false),
                )),
                None => None,
            }
        },
    );

    let body = BodyExt::boxed_unsync(StreamBody::new(stream));
    Response::builder()
        .status(StatusCode::OK)
        .header("content-type", "text/event-stream")
        .header("cache-control", "no-cache")
        .body(body)
        .unwrap_or_else(|_| json_response(StatusCode::INTERNAL_SERVER_ERROR, &json!({})))
}

fn json_response(status: StatusCode, value: &serde_json::Value) -> Response<RespBody> {
    let body = Full::new(Bytes::from(value.to_string()))
        .map_err(|e| std::io::Error::other(e.to_string()))
        .boxed_unsync();
    Response::builder()
        .status(status)
        .header("content-type", "application/json")
        .body(body)
        .unwrap_or_else(|_| {
            Response::new(
                Full::new(Bytes::from_static(b"{}"))
                    .map_err(|e| std::io::Error::other(e.to_string()))
                    .boxed_unsync(),
            )
        })
}

fn is_in_path(binary: &Path) -> bool {
    if binary.is_absolute() {
        return binary.exists();
    }
    if let Ok(path_var) = std::env::var("PATH") {
        for dir in std::env::split_paths(&path_var) {
            if dir.join(binary).is_file() {
                return true;
            }
        }
    }
    false
}

/// Decide whether a model load should use bmoe stream vs llama-server.
pub fn should_use_bmoe(
    gguf: &GgufMetadata,
    profile: &SystemProfile,
    moe: &MoeConfig,
    context_size: usize,
    memory_budget_percent: u8,
) -> bool {
    if !moe.enabled {
        return false;
    }
    let ceil = moe.derive_cache_ceil_mb(profile.available_ram_mb, memory_budget_percent);
    let cache_mb = if ceil > 0 {
        ceil
    } else if moe.resolved_cache_mb_arg() == "0" {
        0
    } else {
        2000
    };
    profile.should_prefer_moe_stream(gguf, context_size, cache_mb, memory_budget_percent, true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::MoeQualityMode;

    #[test]
    fn build_args_lossless_defaults() {
        let cfg = BmoeSessionConfig {
            binary_path: PathBuf::from("bmoe-cli"),
            model_path: PathBuf::from("/models/qwen.gguf"),
            context_size: 4096,
            threads: 4,
            host: "127.0.0.1".into(),
            port: 8080,
            moe: MoeConfig::default(),
            memory_budget_percent: 75,
            cache_ceil_mb: 3500,
            tags: vec![],
        };
        let args = cfg.build_args();
        assert!(args.contains(&"--moe-stream".into()));
        assert!(args.contains(&"--session".into()));
        assert!(args.contains(&"--cache-ceil-mb".into()));
        assert!(args.contains(&"3500".into()));
        assert!(!args.iter().any(|a| a == "--drop-cold-experts"));
        assert!(!args.iter().any(|a| a == "--route-ahead"));
    }

    #[test]
    fn build_args_lossy_when_enabled() {
        let moe = MoeConfig {
            quality_mode: MoeQualityMode::Lossy,
            drop_cold_experts: Some("0.75".into()),
            route_ahead: Some(2),
            overlap: true,
            ..MoeConfig::default()
        };
        let cfg = BmoeSessionConfig {
            binary_path: PathBuf::from("bmoe-cli"),
            model_path: PathBuf::from("m.gguf"),
            context_size: 2048,
            threads: 4,
            host: "127.0.0.1".into(),
            port: 8080,
            moe,
            memory_budget_percent: 75,
            cache_ceil_mb: 0,
            tags: vec![],
        };
        let args = cfg.build_args();
        assert!(args.contains(&"--overlap".into()));
        assert!(args.contains(&"--drop-cold-experts".into()));
        assert!(args.contains(&"0.75".into()));
        assert!(args.contains(&"--route-ahead".into()));
        assert!(args.contains(&"2".into()));
    }

    #[test]
    fn parse_progress_and_done() {
        let ev = parse_bmoe_line(
            r#"BMOE_PROGRESS {"step":1,"steps":10,"delta_text":"Hi","cache_hit_pct":12.5}"#,
        );
        match ev {
            Some(BmoeEvent::Progress(p)) => assert_eq!(p.delta_text.as_deref(), Some("Hi")),
            other => panic!("unexpected {other:?}"),
        }
        let done = parse_bmoe_line(r#"BMOE_DONE {"id":1,"tokens":3,"text":"Hi there"}"#);
        match done {
            Some(BmoeEvent::Done(d)) => assert_eq!(d.text.as_deref(), Some("Hi there")),
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn messages_to_prompt_includes_roles() {
        let msgs = vec![ChatMessage::system("Be brief."), ChatMessage::user("Hello")];
        let p = messages_to_prompt(&msgs);
        assert!(p.contains("System: Be brief."));
        assert!(p.contains("User: Hello"));
        assert!(p.ends_with("Assistant:"));
    }
}
