//! Benchmark store and measurement helpers (Phase 12 §5.5).
//!
//! Persists measured throughput to `~/.nexus/bench.json` (override with
//! `NEXUS_BENCH_PATH`) so the placement ranker can prefer real numbers over
//! heuristic `predict_local_tok_s` bases.

use crate::client::{ChatCompletionRequest, ChatMessage, ClientError, NexusClient};
use crate::sysinfo::AccelerationBackend;
use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Instant, SystemTime, UNIX_EPOCH};
use thiserror::Error;
use tracing::{debug, warn};

const STORE_FILE_NAME: &str = "bench.json";
const STORE_VERSION: u32 = 1;
const MAX_SAMPLES: usize = 8;
static STORE_LOCK: Mutex<()> = Mutex::new(());

/// BenchStore backend key for BigMoeOnEdge flash-stream sessions (not an
/// `AccelerationBackend` — that enum is reserved for hardware probe).
pub const BACKEND_MOE_STREAM: &str = "moe-stream";

#[derive(Debug, Error)]
pub enum BenchError {
    #[error("I/O error in bench store: {0}")]
    Io(#[from] std::io::Error),
    #[error("failed to serialize bench store: {0}")]
    Serialize(#[from] serde_json::Error),
    #[error("client error during bench: {0}")]
    Client(#[from] ClientError),
    #[error("endpoint unhealthy: {0}")]
    Unhealthy(String),
    #[error("bench produced no tokens")]
    NoTokens,
}

/// Stable short label for backend keys in the store.
pub fn backend_key(backend: AccelerationBackend) -> &'static str {
    match backend {
        AccelerationBackend::Vulkan => "vulkan",
        AccelerationBackend::ArmCpuDotProd => "arm-dotprod",
        AccelerationBackend::X86Baseline => "x86-sse41",
        AccelerationBackend::GenericCpu => "cpu",
    }
}

pub fn parse_backend_key(s: &str) -> Option<AccelerationBackend> {
    match s.trim().to_lowercase().as_str() {
        "vulkan" => Some(AccelerationBackend::Vulkan),
        "arm-dotprod" | "arm" => Some(AccelerationBackend::ArmCpuDotProd),
        "x86-sse41" | "x86" | "x86-baseline" => Some(AccelerationBackend::X86Baseline),
        "cpu" | "generic" | "generic-cpu" => Some(AccelerationBackend::GenericCpu),
        _ => None,
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct BenchSample {
    pub gen_tok_s: f32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ttft_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt_tok_s: Option<f32>,
    /// MoE expert-cache hit rate from `BMOE_DONE` (percent), when available.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_hit_pct: Option<f32>,
    /// Unix seconds since epoch.
    pub measured_at: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct BenchEntry {
    pub model_id: String,
    pub node_id: String,
    pub backend: String,
    pub context_size: usize,
    /// Rolling average of recent `gen_tok_s` samples.
    pub gen_tok_s: f32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ttft_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt_tok_s: Option<f32>,
    /// Rolling average of MoE expert-cache hit percent samples.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_hit_pct: Option<f32>,
    pub measured_at: u64,
    #[serde(default)]
    pub samples: Vec<BenchSample>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct BenchStore {
    pub version: u32,
    pub entries: Vec<BenchEntry>,
}

impl BenchStore {
    pub fn default_path() -> PathBuf {
        if let Ok(p) = std::env::var("NEXUS_BENCH_PATH") {
            return PathBuf::from(p);
        }
        let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
        PathBuf::from(home).join(".nexus").join(STORE_FILE_NAME)
    }

    pub fn load(path: &Path) -> Result<Self, BenchError> {
        if !path.exists() {
            return Ok(Self {
                version: STORE_VERSION,
                entries: Vec::new(),
            });
        }
        let bytes = fs::read(path)?;
        let mut store: Self = serde_json::from_slice(&bytes)?;
        if store.version == 0 {
            store.version = STORE_VERSION;
        }
        Ok(store)
    }

    pub fn save(&self, path: &Path) -> Result<(), BenchError> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let bytes = serde_json::to_vec_pretty(self)?;
        let tmp = path.with_extension("json.tmp");
        fs::write(&tmp, bytes)?;
        fs::rename(&tmp, path)?;
        Ok(())
    }

    pub fn load_default() -> Result<Self, BenchError> {
        let _guard = STORE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        Self::load(&Self::default_path())
    }

    pub fn save_default(&self) -> Result<(), BenchError> {
        let _guard = STORE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        self.save(&Self::default_path())
    }

    /// Lookup rolling-average gen tok/s for a key. Model match is case-insensitive
    /// and accepts stem vs `.gguf` filename.
    pub fn lookup_gen_tok_s(
        &self,
        model_id: &str,
        node_id: &str,
        backend: AccelerationBackend,
        context_size: usize,
    ) -> Option<f32> {
        self.lookup_gen_tok_s_by_key(model_id, node_id, backend_key(backend), context_size)
    }

    /// Lookup by free-form backend key (e.g. [`BACKEND_MOE_STREAM`]).
    pub fn lookup_gen_tok_s_by_key(
        &self,
        model_id: &str,
        node_id: &str,
        backend: &str,
        context_size: usize,
    ) -> Option<f32> {
        self.find_entry_by_key(model_id, node_id, backend, context_size)
            .map(|e| e.gen_tok_s)
    }

    /// Prefer exact node match; fall back to any node for same model/backend/ctx.
    pub fn lookup_gen_tok_s_any_node(
        &self,
        model_id: &str,
        backend: AccelerationBackend,
        context_size: usize,
    ) -> Option<f32> {
        self.lookup_gen_tok_s_any_node_by_key(model_id, backend_key(backend), context_size)
    }

    /// Any-node lookup for a free-form backend key.
    pub fn lookup_gen_tok_s_any_node_by_key(
        &self,
        model_id: &str,
        backend: &str,
        context_size: usize,
    ) -> Option<f32> {
        let mut best: Option<&BenchEntry> = None;
        for e in &self.entries {
            if !model_ids_match(&e.model_id, model_id) {
                continue;
            }
            if e.backend != backend || e.context_size != context_size {
                continue;
            }
            best = Some(match best {
                Some(cur) if cur.measured_at >= e.measured_at => cur,
                _ => e,
            });
        }
        best.map(|e| e.gen_tok_s)
    }

    pub fn find_entry(
        &self,
        model_id: &str,
        node_id: &str,
        backend: AccelerationBackend,
        context_size: usize,
    ) -> Option<&BenchEntry> {
        self.find_entry_by_key(model_id, node_id, backend_key(backend), context_size)
    }

    pub fn find_entry_by_key(
        &self,
        model_id: &str,
        node_id: &str,
        backend: &str,
        context_size: usize,
    ) -> Option<&BenchEntry> {
        self.entries.iter().find(|e| {
            model_ids_match(&e.model_id, model_id)
                && e.node_id == node_id
                && e.backend == backend
                && e.context_size == context_size
        })
    }

    /// Append a sample and refresh rolling averages. Returns the updated entry.
    pub fn record(
        &mut self,
        model_id: impl Into<String>,
        node_id: impl Into<String>,
        backend: AccelerationBackend,
        context_size: usize,
        sample: BenchSample,
    ) -> &BenchEntry {
        self.record_with_backend_key(
            model_id,
            node_id,
            backend_key(backend),
            context_size,
            sample,
        )
    }

    /// Append a sample under a free-form backend key (e.g. [`BACKEND_MOE_STREAM`]).
    pub fn record_with_backend_key(
        &mut self,
        model_id: impl Into<String>,
        node_id: impl Into<String>,
        backend: &str,
        context_size: usize,
        sample: BenchSample,
    ) -> &BenchEntry {
        let model_id = model_id.into();
        let node_id = node_id.into();
        let backend_s = backend.to_string();

        if let Some(idx) = self.entries.iter().position(|e| {
            model_ids_match(&e.model_id, &model_id)
                && e.node_id == node_id
                && e.backend == backend_s
                && e.context_size == context_size
        }) {
            let entry = &mut self.entries[idx];
            entry.samples.push(sample.clone());
            if entry.samples.len() > MAX_SAMPLES {
                let drain = entry.samples.len() - MAX_SAMPLES;
                entry.samples.drain(0..drain);
            }
            recompute_entry(entry);
            return &self.entries[idx];
        }

        let mut entry = BenchEntry {
            model_id,
            node_id,
            backend: backend_s,
            context_size,
            gen_tok_s: sample.gen_tok_s,
            ttft_ms: sample.ttft_ms,
            prompt_tok_s: sample.prompt_tok_s,
            cache_hit_pct: sample.cache_hit_pct,
            measured_at: sample.measured_at,
            samples: vec![sample],
        };
        recompute_entry(&mut entry);
        self.entries.push(entry);
        self.entries.last().expect("just pushed")
    }

    /// Best-effort record + save to the default path. Logs and ignores I/O errors.
    pub fn record_default_best_effort(
        model_id: &str,
        node_id: &str,
        backend: AccelerationBackend,
        context_size: usize,
        sample: BenchSample,
    ) {
        Self::record_default_best_effort_key(
            model_id,
            node_id,
            backend_key(backend),
            context_size,
            sample,
        );
    }

    /// Best-effort record under a free-form backend key.
    pub fn record_default_best_effort_key(
        model_id: &str,
        node_id: &str,
        backend: &str,
        context_size: usize,
        sample: BenchSample,
    ) {
        let path = Self::default_path();
        let _guard = STORE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        match Self::load(&path) {
            Ok(mut store) => {
                store.record_with_backend_key(model_id, node_id, backend, context_size, sample);
                if let Err(e) = store.save(&path) {
                    warn!("bench store save failed: {e}");
                }
            }
            Err(e) => warn!("bench store load failed: {e}"),
        }
    }
}

fn recompute_entry(entry: &mut BenchEntry) {
    if entry.samples.is_empty() {
        return;
    }
    let n = entry.samples.len() as f32;
    entry.gen_tok_s = entry.samples.iter().map(|s| s.gen_tok_s).sum::<f32>() / n;
    entry.ttft_ms = entry.samples.iter().rev().find_map(|s| s.ttft_ms);
    let prompt_vals: Vec<f32> = entry
        .samples
        .iter()
        .filter_map(|s| s.prompt_tok_s)
        .collect();
    entry.prompt_tok_s = if prompt_vals.is_empty() {
        None
    } else {
        Some(prompt_vals.iter().sum::<f32>() / prompt_vals.len() as f32)
    };
    let hit_vals: Vec<f32> = entry
        .samples
        .iter()
        .filter_map(|s| s.cache_hit_pct)
        .collect();
    entry.cache_hit_pct = if hit_vals.is_empty() {
        None
    } else {
        Some(hit_vals.iter().sum::<f32>() / hit_vals.len() as f32)
    };
    entry.measured_at = entry
        .samples
        .iter()
        .map(|s| s.measured_at)
        .max()
        .unwrap_or(entry.measured_at);
}

pub fn model_ids_match(a: &str, b: &str) -> bool {
    let a = a.trim().to_lowercase();
    let b = b.trim().to_lowercase();
    if a.is_empty() || b.is_empty() {
        return false;
    }
    if a == b {
        return true;
    }
    let stem = |s: &str| {
        Path::new(s)
            .file_stem()
            .and_then(|x| x.to_str())
            .unwrap_or(s)
            .trim_end_matches(".gguf")
            .to_lowercase()
    };
    stem(&a) == stem(&b)
}

pub fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Estimate prompt tokens from character length (rough 4 chars/token).
pub fn estimate_prompt_tokens(text: &str) -> f32 {
    (text.chars().count() as f32 / 4.0).max(1.0)
}

/// Run one timed streaming completion and return a sample.
pub async fn measure_once(
    client: &NexusClient,
    model: &str,
    prompt: &str,
    max_tokens: usize,
) -> Result<BenchSample, BenchError> {
    let start = Instant::now();
    let mut first_token_at: Option<Instant> = None;
    let mut tokens = 0usize;

    let mut stream = client
        .stream_chat(ChatCompletionRequest {
            model: model.to_string(),
            messages: vec![ChatMessage::user(prompt)],
            temperature: Some(0.0),
            top_p: None,
            max_tokens: Some(max_tokens),
            stream: true,
        })
        .await?;

    while let Some(item) = stream.next().await {
        let _token = item?;
        if first_token_at.is_none() {
            first_token_at = Some(Instant::now());
        }
        tokens += 1;
    }

    if tokens == 0 {
        return Err(BenchError::NoTokens);
    }

    let elapsed = start.elapsed().as_secs_f64().max(1e-6);
    let gen_tok_s = tokens as f32 / elapsed as f32;
    let ttft_ms = first_token_at.map(|t| t.duration_since(start).as_millis() as u64);
    let prompt_tok_s = ttft_ms.map(|ms| {
        let secs = (ms as f32 / 1000.0).max(1e-3);
        estimate_prompt_tokens(prompt) / secs
    });

    Ok(BenchSample {
        gen_tok_s,
        ttft_ms,
        prompt_tok_s,
        cache_hit_pct: None,
        measured_at: unix_now(),
    })
}

/// Parameters for an explicit `nexus bench` run.
#[derive(Debug, Clone)]
pub struct BenchRunConfig<'a> {
    pub endpoint: &'a str,
    pub model: &'a str,
    pub node_id: &'a str,
    pub backend: AccelerationBackend,
    pub context_size: usize,
    pub runs: u32,
    pub prompt: &'a str,
    pub max_tokens: usize,
}

/// Run N timed completions, record into `store`, return the updated entry clone.
pub async fn run_benchmark(
    store: &mut BenchStore,
    cfg: &BenchRunConfig<'_>,
) -> Result<BenchEntry, BenchError> {
    let client = NexusClient::new(cfg.endpoint);
    if !client.health().await.unwrap_or(false) {
        return Err(BenchError::Unhealthy(cfg.endpoint.to_string()));
    }

    let runs = cfg.runs.max(1);
    for i in 0..runs {
        debug!("bench run {}/{} against {}", i + 1, runs, cfg.endpoint);
        let sample = measure_once(&client, cfg.model, cfg.prompt, cfg.max_tokens).await?;
        store.record(
            cfg.model,
            cfg.node_id,
            cfg.backend,
            cfg.context_size,
            sample,
        );
    }

    store
        .find_entry(cfg.model, cfg.node_id, cfg.backend, cfg.context_size)
        .cloned()
        .ok_or(BenchError::NoTokens)
}

#[cfg(test)]
mod unit_tests {
    use super::*;

    #[test]
    fn model_ids_match_stem() {
        assert!(model_ids_match("phi.gguf", "phi"));
        assert!(model_ids_match("PHI", "phi.gguf"));
        assert!(!model_ids_match("phi", "llama"));
    }

    #[test]
    fn record_rolls_average() {
        let mut store = BenchStore {
            version: 1,
            entries: vec![],
        };
        store.record(
            "m",
            "local",
            AccelerationBackend::Vulkan,
            2048,
            BenchSample {
                gen_tok_s: 10.0,
                ttft_ms: Some(100),
                prompt_tok_s: Some(50.0),
                cache_hit_pct: None,
                measured_at: 1,
            },
        );
        store.record(
            "m",
            "local",
            AccelerationBackend::Vulkan,
            2048,
            BenchSample {
                gen_tok_s: 20.0,
                ttft_ms: Some(80),
                prompt_tok_s: Some(60.0),
                cache_hit_pct: None,
                measured_at: 2,
            },
        );
        let e = store
            .find_entry("m", "local", AccelerationBackend::Vulkan, 2048)
            .expect("entry");
        assert!((e.gen_tok_s - 15.0).abs() < 0.01);
        assert_eq!(e.samples.len(), 2);
        assert_eq!(
            store.lookup_gen_tok_s("m.gguf", "local", AccelerationBackend::Vulkan, 2048),
            Some(e.gen_tok_s)
        );
    }

    #[test]
    fn moe_stream_key_records_cache_hit_average() {
        let mut store = BenchStore {
            version: 1,
            entries: vec![],
        };
        store.record_with_backend_key(
            "qwen-moe.gguf",
            "local",
            BACKEND_MOE_STREAM,
            4096,
            BenchSample {
                gen_tok_s: 2.0,
                ttft_ms: None,
                prompt_tok_s: None,
                cache_hit_pct: Some(40.0),
                measured_at: 1,
            },
        );
        store.record_with_backend_key(
            "qwen-moe.gguf",
            "local",
            BACKEND_MOE_STREAM,
            4096,
            BenchSample {
                gen_tok_s: 3.0,
                ttft_ms: None,
                prompt_tok_s: None,
                cache_hit_pct: Some(60.0),
                measured_at: 2,
            },
        );
        let e = store
            .find_entry_by_key("qwen-moe", "local", BACKEND_MOE_STREAM, 4096)
            .expect("moe entry");
        assert!((e.gen_tok_s - 2.5).abs() < 0.01);
        assert_eq!(e.cache_hit_pct, Some(50.0));
        assert_eq!(
            store.lookup_gen_tok_s_by_key("qwen-moe.gguf", "local", BACKEND_MOE_STREAM, 4096),
            Some(2.5)
        );
        assert_eq!(
            store.lookup_gen_tok_s_any_node_by_key("qwen-moe", BACKEND_MOE_STREAM, 4096),
            Some(2.5)
        );
    }
}
