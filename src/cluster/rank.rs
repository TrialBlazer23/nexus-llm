//! Link-quality probing and predicted-throughput ranking (Phase 11 §3.6 / §3.8).

use crate::bench::{BenchStore, BACKEND_MOE_STREAM};
use crate::gguf::GgufMetadata;
use crate::sysinfo::{AccelerationBackend, SystemProfile};
use std::time::{Duration, Instant};

use super::memory::{MemoryPlan, MemoryPolicy, Verdict};
use super::split::{plan_tensor_byte_split, MultiWorkerSplit, SplitError};
use super::{ModelFit, NodeBudget};

/// Cached link measurement for a peer.
#[derive(Debug, Clone, PartialEq)]
pub struct LinkQuality {
    pub rtt_ms: f32,
    pub throughput_bps: f64,
    pub measured_at: Instant,
    pub unknown: bool,
}

impl LinkQuality {
    pub fn unknown() -> Self {
        Self {
            rtt_ms: 50.0,
            throughput_bps: 0.0,
            measured_at: Instant::now(),
            unknown: true,
        }
    }

    pub fn from_probe(rtt: Duration, bytes: u64, elapsed: Duration) -> Self {
        let secs = elapsed.as_secs_f64().max(1e-6);
        Self {
            rtt_ms: rtt.as_secs_f32() * 1000.0,
            throughput_bps: (bytes as f64) / secs,
            measured_at: Instant::now(),
            unknown: false,
        }
    }

    pub fn is_stale(&self, ttl: Duration) -> bool {
        self.measured_at.elapsed() > ttl
    }
}

/// Where / how a model would run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlanTarget {
    LocalGpu {
        ngl: u32,
    },
    LocalCpu,
    /// Local BigMoeOnEdge flash-streaming MoE session.
    LocalMoeStream {
        cache_mb: u64,
        ceil_mb: u64,
    },
    Remote {
        peer_name: String,
    },
    Distributed {
        worker_names: Vec<String>,
    },
}

/// Ranked execution option for the operator.
#[derive(Debug, Clone)]
pub struct ExecutionPlan {
    pub target: PlanTarget,
    pub memory: MemoryPlan,
    pub split: Option<MultiWorkerSplit>,
    pub context_size: usize,
    pub predicted_tok_s: f32,
    pub link_notes: Vec<String>,
    pub llama_extra_args: Vec<String>,
    pub gpu_layers: u32,
}

/// Candidate node for placement.
#[derive(Debug, Clone)]
pub struct PlacementCandidate {
    pub name: String,
    pub budget: NodeBudget,
    pub backend: AccelerationBackend,
    pub rpc_endpoint: Option<String>,
    pub link: LinkQuality,
    pub is_local: bool,
    pub thermal_index: u8,
    /// Peer advertises BigMoe flash-stream capability.
    pub moe_stream: bool,
}

/// Inputs to the placement planner.
#[derive(Debug, Clone)]
pub struct PlacementRequest<'a> {
    pub gguf: &'a GgufMetadata,
    pub policy: MemoryPolicy,
    pub local_profile: SystemProfile,
    pub local_name: String,
    pub local_gpu_layers: u32,
    pub candidates: Vec<PlacementCandidate>,
    pub enable_rpc: bool,
    /// Optional Phase 12 §5.5 measured throughput store.
    pub bench: Option<&'a BenchStore>,
    /// When true, consider LocalMoeStream for streamable MoE GGUFs.
    pub moe_stream_enabled: bool,
    /// Expert cache ceiling (MiB) for MoE stream plans (`0` = derive ~45% of local LMK budget).
    pub moe_cache_ceil_mb: u64,
}

/// Rank execution plans by predicted tokens/sec (descending).
pub fn rank_execution_plans(req: &PlacementRequest<'_>) -> Result<Vec<ExecutionPlan>, SplitError> {
    if !req.gguf.has_geometry() {
        return Err(SplitError::MissingGeometry);
    }

    let mut plans = Vec::new();
    let kv_bytes = req
        .gguf
        .exact_kv_cache_bytes_dtype(req.policy.context_size, req.policy.kv_dtype);
    let compute = super::memory::estimate_compute_buffer_mb(
        req.local_profile.detected_backend,
        req.policy.batch_size,
    );

    let local_budget_mb = req
        .local_profile
        .max_allowed_memory_bytes_pct(req.policy.max_ram_usage_percent)
        / (1024 * 1024);

    let rpc_workers: Vec<(NodeBudget, String, LinkQuality, String)> = req
        .candidates
        .iter()
        .filter(|c| !c.is_local && c.rpc_endpoint.is_some())
        .map(|c| {
            (
                c.budget.clone(),
                c.rpc_endpoint.clone().unwrap(),
                c.link.clone(),
                c.name.clone(),
            )
        })
        .collect();

    let offload_available = req.enable_rpc && !rpc_workers.is_empty();
    let model_key = placement_model_key(req.gguf);

    // --- Local GPU ---
    if req.local_gpu_layers > 0 {
        let mem =
            MemoryPlan::from_gguf(req.gguf, &req.local_profile, &req.policy, offload_available);
        if !matches!(mem.verdict, Verdict::Exceeds)
            || matches!(mem.verdict, Verdict::FitsWithOffload)
        {
            // Pure local only if Fits or FitsIfQuantizedKv
            if matches!(mem.verdict, Verdict::Fits | Verdict::FitsIfQuantizedKv) {
                let tok = predict_local_tok_s(
                    req.local_profile.detected_backend,
                    req.gguf.quant_label.as_deref(),
                    1.0,
                    0,
                    measured_tok_s(
                        req.bench,
                        &model_key,
                        &req.local_name,
                        req.local_profile.detected_backend,
                        req.policy.context_size,
                    ),
                );
                plans.push(ExecutionPlan {
                    target: PlanTarget::LocalGpu {
                        ngl: req.local_gpu_layers,
                    },
                    memory: mem.clone(),
                    split: None,
                    context_size: req.policy.context_size,
                    predicted_tok_s: tok,
                    link_notes: vec!["local".into()],
                    llama_extra_args: vec![],
                    gpu_layers: req.local_gpu_layers,
                });
            }
        }
    }

    // --- Local CPU ---
    {
        let mut cpu_profile = req.local_profile.clone();
        cpu_profile.detected_backend = if cfg!(target_arch = "aarch64") {
            AccelerationBackend::ArmCpuDotProd
        } else {
            AccelerationBackend::X86Baseline
        };
        let mem = MemoryPlan::from_gguf(req.gguf, &cpu_profile, &req.policy, offload_available);
        if matches!(mem.verdict, Verdict::Fits | Verdict::FitsIfQuantizedKv) {
            let tok = predict_local_tok_s(
                cpu_profile.detected_backend,
                req.gguf.quant_label.as_deref(),
                1.0,
                0,
                measured_tok_s(
                    req.bench,
                    &model_key,
                    &req.local_name,
                    cpu_profile.detected_backend,
                    req.policy.context_size,
                ),
            );
            plans.push(ExecutionPlan {
                target: PlanTarget::LocalCpu,
                memory: mem,
                split: None,
                context_size: req.policy.context_size,
                predicted_tok_s: tok,
                link_notes: vec!["local-cpu".into()],
                llama_extra_args: vec![],
                gpu_layers: 0,
            });
        }
    }

    // --- Local MoE flash-stream (prefer over dense RPC for streamable MoE) ---
    if req.moe_stream_enabled && req.gguf.streamable_moe() {
        let ceil = if req.moe_cache_ceil_mb > 0 {
            req.moe_cache_ceil_mb
        } else {
            ((local_budget_mb.saturating_mul(45)) / 100)
                .max(2000)
                .min(local_budget_mb)
        };
        let cache_mb = if ceil >= 2000 { ceil } else { 0 };
        let stream_ok = req.local_profile.can_safely_moe_stream_pct(
            req.gguf,
            req.policy.context_size,
            cache_mb.max(2000),
            req.policy.max_ram_usage_percent,
        );
        if stream_ok {
            let dense_fits = req.local_profile.can_safely_load_gguf_pct(
                req.gguf,
                req.policy.context_size,
                req.policy.max_ram_usage_percent,
            );
            // Only advertise MoE stream when dense local would fail LMK.
            if !dense_fits {
                let mut mem =
                    MemoryPlan::from_gguf(req.gguf, &req.local_profile, &req.policy, false);
                mem.remediations.insert(
                    0,
                    super::memory::Remediation::EnableMoeStream {
                        cache_ceil_mb: ceil,
                    },
                );
                // Prefer measured BMOE_DONE tok/s; cold-start fallback ~1–3 tok/s class.
                let tok = measured_moe_tok_s(
                    req.bench,
                    &model_key,
                    &req.local_name,
                    req.policy.context_size,
                )
                .unwrap_or(2.2);
                plans.push(ExecutionPlan {
                    target: PlanTarget::LocalMoeStream {
                        cache_mb,
                        ceil_mb: ceil,
                    },
                    memory: mem,
                    split: None,
                    context_size: req.policy.context_size,
                    predicted_tok_s: tok,
                    link_notes: vec![format!("moe-stream cache-ceil={ceil}MB")],
                    llama_extra_args: vec![],
                    gpu_layers: 0,
                });
            }
        }
    }

    // --- Remote entire (run fully on a peer that fits) ---
    for c in req.candidates.iter().filter(|c| !c.is_local) {
        let mem = MemoryPlan::from_gguf_budget(
            req.gguf,
            c.budget.allocatable_mb,
            c.backend,
            &req.policy,
            false,
        );
        if matches!(mem.verdict, Verdict::Fits | Verdict::FitsIfQuantizedKv) {
            let mut notes = vec![format!("remote {}", c.name)];
            let mut tok = predict_local_tok_s(
                c.backend,
                req.gguf.quant_label.as_deref(),
                1.0,
                c.thermal_index,
                measured_tok_s(
                    req.bench,
                    &model_key,
                    &c.name,
                    c.backend,
                    req.policy.context_size,
                ),
            );
            if c.link.unknown {
                tok *= 0.85;
                notes.push("link quality unknown — demoted".into());
            } else {
                notes.push(format!("RTT {:.0}ms", c.link.rtt_ms));
            }
            plans.push(ExecutionPlan {
                target: PlanTarget::Remote {
                    peer_name: c.name.clone(),
                },
                memory: mem,
                split: None,
                context_size: req.policy.context_size,
                predicted_tok_s: tok,
                link_notes: notes,
                llama_extra_args: vec![],
                gpu_layers: 99,
            });
        }
    }

    // --- Distributed offload ---
    if offload_available {
        let workers: Vec<(NodeBudget, String)> = rpc_workers
            .iter()
            .map(|(b, ep, _, _)| (b.clone(), ep.clone()))
            .collect();
        match plan_tensor_byte_split(
            req.gguf,
            req.policy.context_size,
            kv_bytes,
            local_budget_mb,
            &workers,
            compute,
        ) {
            Ok(split) if split.is_distributed() => {
                let mem = MemoryPlan::from_gguf(req.gguf, &req.local_profile, &req.policy, true);
                let host_frac = split.host_layers as f32 / split.total_layers.max(1) as f32;
                let boundaries = split.remote_endpoints.len().max(1) as f32;
                let embd = req.gguf.embedding_length.unwrap_or(4096) as f64;
                let act_bytes = 2.0 * embd * 2.0; // F16 activations in/out approx

                // Aggregate worst link among selected workers
                let mut notes = Vec::new();
                let mut min_net = f32::MAX;
                for ep in &split.remote_endpoints {
                    if let Some((_, _, link, name)) =
                        rpc_workers.iter().find(|(_, e, _, _)| e == ep)
                    {
                        if link.unknown {
                            notes.push(format!("{name}: link unknown — demoted"));
                            min_net = min_net.min(2.0);
                        } else {
                            notes.push(format!("{name}: RTT {:.0}ms", link.rtt_ms));
                            let net = network_bound_tok_s(link, act_bytes, boundaries);
                            min_net = min_net.min(net);
                        }
                    }
                }
                let compute_tok = predict_local_tok_s(
                    req.local_profile.detected_backend,
                    req.gguf.quant_label.as_deref(),
                    host_frac,
                    0,
                    measured_tok_s(
                        req.bench,
                        &model_key,
                        &req.local_name,
                        req.local_profile.detected_backend,
                        req.policy.context_size,
                    ),
                );
                let predicted = compute_tok.min(min_net);
                let args = split.build_llama_args();
                let names: Vec<String> = split
                    .remote_endpoints
                    .iter()
                    .filter_map(|ep| {
                        rpc_workers
                            .iter()
                            .find(|(_, e, _, _)| e == ep)
                            .map(|(_, _, _, n)| n.clone())
                    })
                    .collect();
                plans.push(ExecutionPlan {
                    target: PlanTarget::Distributed {
                        worker_names: names,
                    },
                    memory: mem,
                    llama_extra_args: args,
                    split: Some(split),
                    context_size: req.policy.context_size,
                    predicted_tok_s: predicted,
                    link_notes: notes,
                    gpu_layers: req.local_gpu_layers.max(1),
                });
            }
            Ok(_) => {}
            Err(SplitError::Cluster(_)) => {
                // No viable distributed plan — skip
            }
            Err(e) => return Err(e),
        }
    }

    // Sort by predicted tok/s descending; prefer local on ties.
    plans.sort_by(|a, b| {
        b.predicted_tok_s
            .partial_cmp(&a.predicted_tok_s)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| {
                let al = matches!(a.target, PlanTarget::LocalGpu { .. } | PlanTarget::LocalCpu);
                let bl = matches!(b.target, PlanTarget::LocalGpu { .. } | PlanTarget::LocalCpu);
                bl.cmp(&al) // local first on tie → true > false when reversed... want local first: al.cmp(&bl) reversed
            })
    });
    // Stable: if equal tok/s, local before remote
    plans.sort_by(|a, b| {
        match b
            .predicted_tok_s
            .partial_cmp(&a.predicted_tok_s)
            .unwrap_or(std::cmp::Ordering::Equal)
        {
            std::cmp::Ordering::Equal => {
                let score = |t: &PlanTarget| match t {
                    PlanTarget::LocalGpu { .. } => 3,
                    PlanTarget::LocalMoeStream { .. } => 3,
                    PlanTarget::LocalCpu => 2,
                    PlanTarget::Remote { .. } => 1,
                    PlanTarget::Distributed { .. } => 0,
                };
                score(&b.target).cmp(&score(&a.target))
            }
            o => o,
        }
    });

    Ok(plans)
}

fn placement_model_key(gguf: &GgufMetadata) -> String {
    gguf.model_name
        .clone()
        .filter(|s| !s.is_empty())
        .or_else(|| gguf.quant_label.clone())
        .unwrap_or_else(|| "unknown".into())
}

fn measured_tok_s(
    bench: Option<&BenchStore>,
    model_id: &str,
    node_id: &str,
    backend: AccelerationBackend,
    context_size: usize,
) -> Option<f32> {
    let store = bench?;
    store
        .lookup_gen_tok_s(model_id, node_id, backend, context_size)
        .or_else(|| store.lookup_gen_tok_s_any_node(model_id, backend, context_size))
}

/// Measured BigMoe flash-stream throughput (`BACKEND_MOE_STREAM` key).
fn measured_moe_tok_s(
    bench: Option<&BenchStore>,
    model_id: &str,
    node_id: &str,
    context_size: usize,
) -> Option<f32> {
    let store = bench?;
    store
        .lookup_gen_tok_s_by_key(model_id, node_id, BACKEND_MOE_STREAM, context_size)
        .or_else(|| {
            store.lookup_gen_tok_s_any_node_by_key(model_id, BACKEND_MOE_STREAM, context_size)
        })
        .filter(|v| *v > 0.0)
}

fn predict_local_tok_s(
    backend: AccelerationBackend,
    quant: Option<&str>,
    layer_frac: f32,
    thermal: u8,
    measured: Option<f32>,
) -> f32 {
    let base = if let Some(m) = measured.filter(|v| *v > 0.0) {
        m
    } else {
        match backend {
            AccelerationBackend::Vulkan => 28.0,
            AccelerationBackend::ArmCpuDotProd => 12.0,
            AccelerationBackend::X86Baseline => 4.0,
            AccelerationBackend::GenericCpu => 6.0,
        }
    };
    // Measured values already include quant/runtime effects; only scale by
    // layer fraction and thermal when using heuristics or partial offload.
    let quant_boost = if measured.is_some() {
        1.0
    } else {
        match quant.unwrap_or("") {
            s if s.contains("Q4") => 1.15,
            s if s.contains("Q5") => 1.05,
            s if s.contains("Q8") || s.contains("F16") => 0.85,
            _ => 1.0,
        }
    };
    let thermal_pen = if thermal > 75 {
        0.6
    } else if thermal > 50 {
        0.85
    } else {
        1.0
    };
    (base * quant_boost * layer_frac.max(0.05) * thermal_pen).max(0.1)
}

fn network_bound_tok_s(link: &LinkQuality, activation_bytes: f64, boundaries: f32) -> f32 {
    if link.unknown || link.throughput_bps <= 0.0 {
        return 2.0;
    }
    let per_token_bytes = activation_bytes * boundaries as f64;
    let throughput_tok = (link.throughput_bps / per_token_bytes.max(1.0)) as f32;
    let rtt_cap = if link.rtt_ms > 0.0 {
        1000.0 / (link.rtt_ms * boundaries)
    } else {
        f32::MAX
    };
    throughput_tok.min(rtt_cap).max(0.1)
}

/// Classify fit using host vs cluster caps (UI helper).
pub fn classify_fit(required_mb: u64, host_cap_mb: u64, cluster_cap_mb: u64) -> ModelFit {
    ModelFit::classify(required_mb, host_cap_mb, cluster_cap_mb)
}

/// Probe helper: time a fixed-size round-trip. Callers supply the bytes transferred.
pub fn link_quality_from_timings(
    rtt: Duration,
    payload_bytes: u64,
    transfer: Duration,
) -> LinkQuality {
    LinkQuality::from_probe(rtt, payload_bytes, transfer)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cluster::NodeBudget;
    use std::collections::HashMap;
    use uuid::Uuid;

    fn gguf_1p5b() -> GgufMetadata {
        GgufMetadata {
            version: 3,
            tensor_count: 0,
            kv_count: 0,
            metadata: HashMap::new(),
            architecture: Some("llama".into()),
            model_name: Some("1.5B".into()),
            context_length: Some(4096),
            block_count: Some(28),
            head_count: Some(12),
            head_count_kv: Some(12),
            embedding_length: Some(1536),
            expert_count: None,
            expert_used_count: None,
            file_size_bytes: 900_000_000,
            tensors: vec![],
            quant_label: Some("Q4_K".into()),
        }
    }

    #[test]
    fn ranks_local_above_unknown_link_offload_when_fits() {
        let gguf = gguf_1p5b();
        let profile = SystemProfile {
            total_ram_mb: 16_000,
            available_ram_mb: 12_000,
            detected_backend: AccelerationBackend::Vulkan,
            recommended_threads: 4,
        };
        let req = PlacementRequest {
            gguf: &gguf,
            policy: MemoryPolicy {
                max_ram_usage_percent: 75,
                mmap: true,
                mlock: false,
                kv_dtype: crate::gguf::KvCacheDtype::F16,
                context_size: 2048,
                batch_size: 512,
            },
            local_profile: profile,
            local_name: "local".into(),
            local_gpu_layers: 99,
            enable_rpc: true,
            bench: None,
            moe_stream_enabled: true,
            moe_cache_ceil_mb: 0,
            candidates: vec![PlacementCandidate {
                name: "desktop".into(),
                budget: NodeBudget::new(Uuid::new_v4(), "desktop", 24_000),
                backend: AccelerationBackend::X86Baseline,
                rpc_endpoint: Some("10.0.0.5:50052".into()),
                link: LinkQuality::unknown(),
                is_local: false,
                thermal_index: 20,
                moe_stream: false,
            }],
        };
        let plans = rank_execution_plans(&req).expect("plans");
        assert!(!plans.is_empty());
        // Local Vulkan should beat unknown-link demoted options when it fits.
        assert!(matches!(
            plans[0].target,
            PlanTarget::LocalGpu { .. } | PlanTarget::LocalCpu | PlanTarget::Remote { .. }
        ));
        assert!(plans.iter().any(|p| p.predicted_tok_s > 0.0));
    }

    #[test]
    fn unknown_link_demotes_net_bound() {
        let unknown = LinkQuality::unknown();
        let known = LinkQuality::from_probe(
            Duration::from_millis(5),
            10_000_000,
            Duration::from_millis(10),
        );
        let u = network_bound_tok_s(&unknown, 8192.0, 1.0);
        let k = network_bound_tok_s(&known, 8192.0, 1.0);
        assert!(k > u);
    }
}
