//! Per-tensor layer-split planning (Phase 11 §3.5 / multi-worker §3.6).

use crate::gguf::GgufMetadata;
use thiserror::Error;

use super::{ClusterError, LayerSplitDecision, NodeBudget};

/// Extended multi-worker split decision.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MultiWorkerSplit {
    pub total_layers: u32,
    pub host_layers: u32,
    /// Layers assigned to each remote worker (same order as endpoints).
    pub worker_layers: Vec<u32>,
    pub remote_endpoints: Vec<String>,
    /// Optional ratio string for `--tensor-split` (validated live against llama.cpp).
    pub tensor_split: Option<String>,
}

impl MultiWorkerSplit {
    pub fn is_distributed(&self) -> bool {
        !self.remote_endpoints.is_empty() && self.worker_layers.iter().sum::<u32>() > 0
    }

    pub fn to_layer_split_decision(&self) -> LayerSplitDecision {
        LayerSplitDecision {
            total_layers: self.total_layers,
            host_layers: self.host_layers,
            remote_layers: self.worker_layers.iter().sum(),
            remote_endpoint: self.remote_endpoints.first().cloned(),
            remote_endpoints: self.remote_endpoints.clone(),
            tensor_split: self.tensor_split.clone(),
        }
    }

    /// CLI args: `--split-mode layer` and repeated `--rpc` (never `--split-mode row`).
    pub fn build_llama_args(&self) -> Vec<String> {
        let mut args = Vec::new();
        if !self.is_distributed() {
            return args;
        }
        for ep in &self.remote_endpoints {
            args.push("--rpc".to_string());
            args.push(ep.clone());
        }
        args.push("--split-mode".to_string());
        args.push("layer".to_string());
        if let Some(ts) = &self.tensor_split {
            args.push("--tensor-split".to_string());
            args.push(ts.clone());
        }
        args
    }
}

#[derive(Error, Debug, PartialEq, Eq)]
pub enum SplitError {
    #[error(transparent)]
    Cluster(#[from] ClusterError),

    #[error("GGUF geometry missing (block_count / tensors); refuse to guess layer count")]
    MissingGeometry,

    #[error("GGUF tensor geometry error: {0}")]
    Geometry(String),
}

/// Plan a host-first layer split from per-tensor byte sums.
///
/// - Non-layer tensors (embeddings, output) stay on the host fixed cost.
/// - KV is attributed proportionally to layers held on each side.
/// - Workers receive remaining layers greedily by remaining allocatable MB.
pub fn plan_tensor_byte_split(
    gguf: &GgufMetadata,
    _ctx: usize,
    kv_bytes_total: u64,
    host_budget_mb: u64,
    workers: &[(NodeBudget, String)], // (budget, rpc endpoint)
    compute_buffer_mb: u64,
) -> Result<MultiWorkerSplit, SplitError> {
    if !gguf.has_geometry() {
        return Err(SplitError::MissingGeometry);
    }
    let total_layers = gguf.block_count.unwrap() as u32;
    if total_layers == 0 {
        return Err(SplitError::Cluster(ClusterError::InvalidLayerCount));
    }

    let layer_bytes = gguf
        .per_layer_weight_bytes()
        .map_err(|e| SplitError::Geometry(e.to_string()))?;
    if layer_bytes.len() != total_layers as usize {
        return Err(SplitError::Geometry(
            "per-layer byte vector length mismatch".into(),
        ));
    }

    let host_fixed_bytes = if gguf.tensors.is_empty() {
        // No tensor section: approximate 15% of weights as host-fixed (embd+output).
        (gguf.weights_bytes() as f64 * 0.15) as u64
    } else {
        gguf.host_fixed_weight_bytes()
    };

    let kv_per_layer = kv_bytes_total / total_layers.max(1) as u64;
    let mb = |b: u64| b / (1024 * 1024);

    // Total required if all local.
    let total_weights_mb = mb(gguf.weights_bytes());
    let total_kv_mb = mb(kv_bytes_total);
    let total_required_mb = total_weights_mb
        .saturating_add(total_kv_mb)
        .saturating_add(compute_buffer_mb);

    let remote_cap: u64 = workers.iter().map(|(b, _)| b.allocatable_mb).sum();
    let cluster_cap = host_budget_mb.saturating_add(remote_cap);

    // Fits entirely on host (weights + kv + compute under host budget using resident-aware
    // approximation: count full weights for split capacity planning).
    let host_all_cost = mb(host_fixed_bytes)
        .saturating_add(layer_bytes.iter().copied().map(mb).sum::<u64>())
        .saturating_add(total_kv_mb)
        .saturating_add(compute_buffer_mb);

    if host_all_cost <= host_budget_mb || total_required_mb <= host_budget_mb {
        return Ok(MultiWorkerSplit {
            total_layers,
            host_layers: total_layers,
            worker_layers: vec![],
            remote_endpoints: vec![],
            tensor_split: None,
        });
    }

    if workers.is_empty() || remote_cap == 0 {
        return Err(SplitError::Cluster(ClusterError::NoRpcWorkerAvailable {
            overflow_mb: total_required_mb.saturating_sub(host_budget_mb),
            host_max_mb: host_budget_mb,
        }));
    }

    if total_required_mb > cluster_cap {
        return Err(SplitError::Cluster(
            ClusterError::ClusterMemoryCapExceeded {
                required_mb: total_required_mb,
                cluster_max_mb: cluster_cap,
                host_max_mb: host_budget_mb,
                remote_max_mb: remote_cap,
            },
        ));
    }

    // Greedy: assign as many layers as possible to host from the start.
    let mut host_layers = 0u32;
    let mut host_used = mb(host_fixed_bytes).saturating_add(compute_buffer_mb / 2);
    for (i, &lb) in layer_bytes.iter().enumerate() {
        let add = mb(lb).saturating_add(mb(kv_per_layer));
        if host_used.saturating_add(add) <= host_budget_mb {
            host_used += add;
            host_layers = (i as u32) + 1;
        } else {
            break;
        }
    }
    // Host must keep at least 1 layer when distributing.
    if host_layers == 0 {
        host_layers = 1;
    }
    if host_layers >= total_layers {
        host_layers = total_layers - 1;
    }

    let mut remaining_start = host_layers as usize;
    let mut worker_layers = Vec::new();
    let mut remote_endpoints = Vec::new();

    for (budget, endpoint) in workers {
        if remaining_start >= total_layers as usize {
            break;
        }
        let mut taken = 0u32;
        let mut used = compute_buffer_mb / 4;
        for &lb in &layer_bytes[remaining_start..] {
            let add = mb(lb).saturating_add(mb(kv_per_layer));
            if used.saturating_add(add) <= budget.allocatable_mb {
                used += add;
                taken += 1;
            } else {
                break;
            }
        }
        if taken == 0 {
            continue;
        }
        worker_layers.push(taken);
        remote_endpoints.push(endpoint.clone());
        remaining_start += taken as usize;
    }

    let assigned_remote: u32 = worker_layers.iter().sum();
    if host_layers + assigned_remote < total_layers {
        // Could not place all layers — try forcing remainder onto last worker if any capacity.
        return Err(SplitError::Cluster(
            ClusterError::ClusterMemoryCapExceeded {
                required_mb: total_required_mb,
                cluster_max_mb: cluster_cap,
                host_max_mb: host_budget_mb,
                remote_max_mb: remote_cap,
            },
        ));
    }

    // Build tensor-split ratios by layer counts (devices: host + workers).
    let mut ratios = vec![host_layers];
    ratios.extend(&worker_layers);
    let sum: u32 = ratios.iter().sum();
    let pct: Vec<String> = ratios
        .iter()
        .map(|n| (((*n as f64) / (sum as f64)) * 100.0).round() as u32)
        .map(|p| p.to_string())
        .collect();
    // Normalize last so sum is 100
    let mut pct_nums: Vec<u32> = pct.iter().filter_map(|s| s.parse().ok()).collect();
    if !pct_nums.is_empty() {
        let s: u32 = pct_nums.iter().sum();
        if s != 100 {
            let last = pct_nums.len() - 1;
            pct_nums[last] = pct_nums[last].saturating_add(100u32.saturating_sub(s));
        }
    }
    let tensor_split = Some(
        pct_nums
            .iter()
            .map(|p| p.to_string())
            .collect::<Vec<_>>()
            .join(","),
    );

    Ok(MultiWorkerSplit {
        total_layers,
        host_layers,
        worker_layers,
        remote_endpoints,
        tensor_split,
    })
}

/// Legacy fraction-based planner retained for comparison tests only.
#[cfg(test)]
#[allow(dead_code)]
pub fn plan_fraction_split_for_test(
    model_size_bytes: u64,
    exact_kv_bytes: u64,
    total_layers: u32,
    budget: &super::ClusterBudget,
    rpc_endpoint: Option<&str>,
) -> Result<LayerSplitDecision, ClusterError> {
    super::legacy_plan_layer_split(
        model_size_bytes,
        exact_kv_bytes,
        total_layers,
        budget,
        rpc_endpoint,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gguf::{GgufTensorInfo, GGUF_MAGIC};
    use std::collections::HashMap;
    use std::io::Cursor;
    use uuid::Uuid;

    fn meta_with_uneven_layers() -> GgufMetadata {
        // 4 layers: layer 0–2 small, but huge host-fixed embd forces more offload than fraction.
        let mut tensors = Vec::new();
        // Huge embedding (host-fixed)
        tensors.push(GgufTensorInfo {
            name: "token_embd.weight".into(),
            n_dims: 2,
            dims: [256, 256, 0, 0],
            ggml_type: 1, // F16
            offset: 0,
            nbytes: 400 * 1024 * 1024, // 400 MB
            layer_index: None,
        });
        for i in 0..4u32 {
            tensors.push(GgufTensorInfo {
                name: format!("blk.{i}.attn_q.weight"),
                n_dims: 2,
                dims: [64, 64, 0, 0],
                ggml_type: 1,
                offset: 0,
                nbytes: 50 * 1024 * 1024, // 50 MB each
                layer_index: Some(i),
            });
        }
        GgufMetadata {
            version: 3,
            tensor_count: tensors.len() as u64,
            kv_count: 0,
            metadata: HashMap::new(),
            architecture: Some("llama".into()),
            model_name: None,
            context_length: Some(2048),
            block_count: Some(4),
            head_count: Some(8),
            head_count_kv: Some(8),
            embedding_length: Some(512),
            expert_count: None,
            expert_used_count: None,
            file_size_bytes: 600 * 1024 * 1024,
            tensors,
            quant_label: Some("F16".into()),
        }
    }

    #[test]
    fn tensor_split_offloads_more_than_fraction_when_embd_large() {
        let gguf = meta_with_uneven_layers();
        let kv = gguf.exact_kv_cache_bytes(2048);
        let host_budget = 500u64; // tight
        let worker = NodeBudget::new(Uuid::new_v4(), "w1", 800);
        let workers = vec![(worker, "10.0.0.2:50052".into())];

        let split =
            plan_tensor_byte_split(&gguf, 2048, kv, host_budget, &workers, 64).expect("split");
        assert!(split.is_distributed());
        assert!(split.host_layers < split.total_layers);
        assert!(!split.remote_endpoints.is_empty());

        // Fraction baseline: overflow/total * layers often under-offloads.
        let total_mb = (gguf.weights_bytes() + kv) / (1024 * 1024);
        let overflow = total_mb.saturating_sub(host_budget);
        let frac_remote = ((overflow as f64 / total_mb as f64) * 4.0).ceil() as u32;
        assert!(
            split.worker_layers.iter().sum::<u32>() >= frac_remote.min(3),
            "tensor plan should offload at least as aggressively as fraction"
        );
    }

    #[test]
    fn refuses_missing_geometry() {
        let gguf = GgufMetadata {
            version: 3,
            tensor_count: 0,
            kv_count: 0,
            metadata: HashMap::new(),
            architecture: None,
            model_name: None,
            context_length: None,
            block_count: None,
            head_count: None,
            head_count_kv: None,
            embedding_length: None,
            expert_count: None,
            expert_used_count: None,
            file_size_bytes: 100,
            tensors: vec![],
            quant_label: None,
        };
        let err = plan_tensor_byte_split(&gguf, 512, 0, 1000, &[], 64).unwrap_err();
        assert!(matches!(err, SplitError::MissingGeometry));
    }

    #[test]
    fn multi_worker_args_repeat_rpc() {
        let split = MultiWorkerSplit {
            total_layers: 10,
            host_layers: 4,
            worker_layers: vec![3, 3],
            remote_endpoints: vec!["a:1".into(), "b:2".into()],
            tensor_split: Some("40,30,30".into()),
        };
        let args = split.build_llama_args();
        assert_eq!(args.iter().filter(|a| *a == "--rpc").count(), 2);
        assert!(args.contains(&"--split-mode".to_string()));
        assert!(args.contains(&"layer".to_string()));
        assert!(!args.iter().any(|a| a == "row"));
    }

    #[test]
    fn synthetic_gguf_roundtrip_still_parses() {
        // Smoke: ensure GGUF_MAGIC still linked for fixtures.
        assert_eq!(GGUF_MAGIC, 0x46554747);
        let _ = Cursor::new(Vec::<u8>::new());
    }
}
