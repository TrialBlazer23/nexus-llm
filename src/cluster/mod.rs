//! Cluster budgets, layer-split planning, and placement ranking (Phase 11).

mod memory;
mod moe_knobs;
mod rank;
mod split;

pub use memory::{estimate_compute_buffer_mb, MemoryPlan, MemoryPolicy, Remediation, Verdict};
pub use moe_knobs::{
    plan_moe_stream_knobs, predict_moe_stream_tok_s, MoeStreamKnobPlan, MOE_COLD_START_TOK_S,
};
pub use rank::{
    classify_fit, link_quality_from_timings, rank_execution_plans, ExecutionPlan, LinkQuality,
    PlacementCandidate, PlacementRequest, PlanTarget,
};
pub use split::{plan_tensor_byte_split, MultiWorkerSplit, SplitError};

use crate::gguf::GgufMetadata;
use thiserror::Error;
use uuid::Uuid;

/// Default LMK safety percent when config is unavailable.
pub const LMK_SAFETY_PERCENT: f64 = 0.75;

/// Default context window used when GGUF metadata has no `context_length`.
pub const DEFAULT_CONTEXT_SIZE: usize = 4096;

/// Step size for Hub `[+]`/`[-]` context adjustments.
pub const CONTEXT_STEP: usize = 512;

/// Whether a model+KV footprint fits on a host, needs RPC offload, or exceeds the cluster.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModelFit {
    Fits,
    NeedsRpc,
    WontFit,
}

impl ModelFit {
    pub fn classify(required_mb: u64, host_cap_mb: u64, cluster_cap_mb: u64) -> Self {
        if required_mb <= host_cap_mb {
            Self::Fits
        } else if required_mb <= cluster_cap_mb {
            Self::NeedsRpc
        } else {
            Self::WontFit
        }
    }

    pub fn list_badge(self) -> &'static str {
        match self {
            Self::Fits => "[OK]",
            Self::NeedsRpc => "[RPC]",
            Self::WontFit => "[OOM]",
        }
    }

    pub fn modal_badge(self) -> &'static str {
        match self {
            Self::Fits => "✅ fits",
            Self::NeedsRpc => "⚠️ needs RPC",
            Self::WontFit => "❌ won't fit",
        }
    }
}

/// Allocatable host budget under an explicit safety percent (1–100).
pub fn host_lmk_budget_mb(available_ram_mb: u64, percent: u8) -> u64 {
    let pct = u64::from(percent.clamp(1, 100));
    (available_ram_mb.saturating_mul(pct)) / 100
}

/// Clamp a context size to a model-safe range (min 512, max model/default ceiling).
pub fn clamp_context_size(desired: usize, model_context_limit: Option<usize>) -> usize {
    let upper = model_context_limit
        .unwrap_or(DEFAULT_CONTEXT_SIZE)
        .max(CONTEXT_STEP);
    desired.clamp(CONTEXT_STEP, upper)
}

#[derive(Error, Debug, PartialEq, Eq)]
pub enum ClusterError {
    #[error("Cluster memory ceiling exceeded: Model and KV cache require {required_mb} MB, but maximum cluster capacity is {cluster_max_mb} MB (Host: {host_max_mb} MB, RPC Worker: {remote_max_mb} MB)")]
    ClusterMemoryCapExceeded {
        required_mb: u64,
        cluster_max_mb: u64,
        host_max_mb: u64,
        remote_max_mb: u64,
    },

    #[error(
        "No RPC worker available to offload {overflow_mb} MB beyond host's {host_max_mb} MB budget"
    )]
    NoRpcWorkerAvailable { overflow_mb: u64, host_max_mb: u64 },

    #[error("Model has invalid block/layer count")]
    InvalidLayerCount,

    #[error("GGUF geometry missing; refuse to plan without block_count")]
    MissingGeometry,
}

/// Dynamic budget profile for an individual node in the cluster.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeBudget {
    pub node_id: Uuid,
    pub name: String,
    pub allocatable_mb: u64,
}

impl NodeBudget {
    pub fn new(node_id: Uuid, name: impl Into<String>, allocatable_mb: u64) -> Self {
        Self {
            node_id,
            name: name.into(),
            allocatable_mb,
        }
    }
}

/// Represents the memory budget available across the cluster.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClusterBudget {
    pub host_max_mb: u64,
    pub remote_max_mb: u64,
    pub total_cluster_mb: u64,
}

/// Resulting layer allocation decision between host and remote worker(s).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LayerSplitDecision {
    pub total_layers: u32,
    pub host_layers: u32,
    pub remote_layers: u32,
    pub remote_endpoint: Option<String>,
    /// All RPC worker endpoints (Phase 11 multi-worker).
    pub remote_endpoints: Vec<String>,
    pub tensor_split: Option<String>,
}

impl LayerSplitDecision {
    pub fn is_distributed(&self) -> bool {
        self.remote_layers > 0
            && (!self.remote_endpoints.is_empty() || self.remote_endpoint.is_some())
    }

    /// Construct CLI arguments for llama-server (`--split-mode layer` only).
    pub fn build_llama_args(&self) -> Vec<String> {
        let mut args = Vec::new();
        if !self.is_distributed() {
            return args;
        }
        let endpoints = if !self.remote_endpoints.is_empty() {
            self.remote_endpoints.clone()
        } else if let Some(ep) = &self.remote_endpoint {
            vec![ep.clone()]
        } else {
            vec![]
        };
        for endpoint in endpoints {
            args.push("--rpc".to_string());
            args.push(endpoint);
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

pub struct ClusterCoordinator;

impl ClusterCoordinator {
    /// Calculate the available memory budget dynamically for a host and optional RPC worker(s).
    pub fn calculate_dynamic_budget(
        host_avail_mb: u64,
        host_cap_mb: Option<u64>,
        remote_peer_avail_mb: Option<u64>,
        safety_percent: u8,
    ) -> ClusterBudget {
        let host_safety_budget = host_lmk_budget_mb(host_avail_mb, safety_percent);
        let host_max_mb = match host_cap_mb {
            Some(cap) => host_safety_budget.min(cap),
            None => host_safety_budget,
        };
        let remote_max_mb = remote_peer_avail_mb.unwrap_or(0);
        let total_cluster_mb = host_max_mb + remote_max_mb;

        ClusterBudget {
            host_max_mb,
            remote_max_mb,
            total_cluster_mb,
        }
    }

    pub fn calculate_from_nodes(host: &NodeBudget, worker: Option<&NodeBudget>) -> ClusterBudget {
        let host_max_mb = host.allocatable_mb;
        let remote_max_mb = worker.map_or(0, |w| w.allocatable_mb);
        let total_cluster_mb = host_max_mb + remote_max_mb;

        ClusterBudget {
            host_max_mb,
            remote_max_mb,
            total_cluster_mb,
        }
    }

    /// Calculate budget from host available RAM using default 75% safety.
    pub fn calculate_budget(
        host_avail_mb: u64,
        remote_peer_avail_mb: Option<u64>,
    ) -> ClusterBudget {
        Self::calculate_dynamic_budget(host_avail_mb, None, remote_peer_avail_mb, 75)
    }

    /// Plan layer splitting using per-tensor bytes when GGUF geometry is available.
    ///
    /// Falls back to legacy fraction math only when `tensors` is empty but `block_count` is known
    /// (still refuses when `block_count` is missing — no more `unwrap_or(32)`).
    pub fn plan_layer_split(
        model_size_bytes: u64,
        exact_kv_bytes: u64,
        total_layers: u32,
        budget: &ClusterBudget,
        rpc_endpoint: Option<&str>,
    ) -> Result<LayerSplitDecision, ClusterError> {
        legacy_plan_layer_split(
            model_size_bytes,
            exact_kv_bytes,
            total_layers,
            budget,
            rpc_endpoint,
        )
    }

    /// Tensor-aware multi-worker plan from GGUF metadata.
    pub fn plan_from_gguf(
        gguf: &GgufMetadata,
        ctx: usize,
        budget: &ClusterBudget,
        rpc_endpoint: Option<&str>,
    ) -> Result<LayerSplitDecision, ClusterError> {
        let total_layers = gguf.block_count.ok_or(ClusterError::MissingGeometry)? as u32;
        if total_layers == 0 {
            return Err(ClusterError::InvalidLayerCount);
        }
        let exact_kv_bytes = gguf.exact_kv_cache_bytes(ctx);

        if gguf.has_geometry() {
            let workers: Vec<(NodeBudget, String)> = match (rpc_endpoint, budget.remote_max_mb > 0)
            {
                (Some(ep), true) if !ep.is_empty() => {
                    vec![(
                        NodeBudget::new(Uuid::nil(), "worker", budget.remote_max_mb),
                        ep.to_string(),
                    )]
                }
                _ => vec![],
            };
            match plan_tensor_byte_split(
                gguf,
                ctx,
                exact_kv_bytes,
                budget.host_max_mb,
                &workers,
                256,
            ) {
                Ok(split) => return Ok(split.to_layer_split_decision()),
                Err(SplitError::Cluster(e)) => return Err(e),
                Err(SplitError::MissingGeometry) => return Err(ClusterError::MissingGeometry),
                Err(SplitError::Geometry(_)) => {
                    // Fall through to legacy with known layer count
                }
            }
        }

        Self::plan_layer_split(
            gguf.file_size_bytes,
            exact_kv_bytes,
            total_layers,
            budget,
            rpc_endpoint,
        )
    }

    /// Ranked placement plans (Phase 11 entry point).
    pub fn rank_execution_plans(
        req: &PlacementRequest<'_>,
    ) -> Result<Vec<ExecutionPlan>, SplitError> {
        rank::rank_execution_plans(req)
    }
}

/// Legacy fraction × layers planner (used when tensor section unavailable).
pub(crate) fn legacy_plan_layer_split(
    model_size_bytes: u64,
    exact_kv_bytes: u64,
    total_layers: u32,
    budget: &ClusterBudget,
    rpc_endpoint: Option<&str>,
) -> Result<LayerSplitDecision, ClusterError> {
    if total_layers == 0 {
        return Err(ClusterError::InvalidLayerCount);
    }

    let total_required_mb = (model_size_bytes + exact_kv_bytes) / (1024 * 1024);

    if total_required_mb <= budget.host_max_mb {
        return Ok(LayerSplitDecision {
            total_layers,
            host_layers: total_layers,
            remote_layers: 0,
            remote_endpoint: None,
            remote_endpoints: vec![],
            tensor_split: None,
        });
    }

    let endpoint = match rpc_endpoint {
        Some(ep) if !ep.is_empty() && budget.remote_max_mb > 0 => ep.to_string(),
        _ => {
            let overflow_mb = total_required_mb.saturating_sub(budget.host_max_mb);
            return Err(ClusterError::NoRpcWorkerAvailable {
                overflow_mb,
                host_max_mb: budget.host_max_mb,
            });
        }
    };

    if total_required_mb > budget.total_cluster_mb {
        return Err(ClusterError::ClusterMemoryCapExceeded {
            required_mb: total_required_mb,
            cluster_max_mb: budget.total_cluster_mb,
            host_max_mb: budget.host_max_mb,
            remote_max_mb: budget.remote_max_mb,
        });
    }

    let overflow_mb = total_required_mb - budget.host_max_mb;
    let remote_fraction = (overflow_mb as f64) / (total_required_mb as f64);
    let mut remote_layers = (total_layers as f64 * remote_fraction).ceil() as u32;
    if remote_layers == 0 {
        remote_layers = 1;
    }
    if remote_layers >= total_layers {
        remote_layers = total_layers - 1;
    }
    let host_layers = total_layers - remote_layers;

    let host_ratio = ((host_layers as f64 / total_layers as f64) * 100.0).round() as u32;
    let remote_ratio = 100 - host_ratio;
    let tensor_split = format!("{host_ratio},{remote_ratio}");

    Ok(LayerSplitDecision {
        total_layers,
        host_layers,
        remote_layers,
        remote_endpoint: Some(endpoint.clone()),
        remote_endpoints: vec![endpoint],
        tensor_split: Some(tensor_split),
    })
}

#[cfg(test)]
mod fit_tests {
    use super::*;

    #[test]
    fn classify_model_fit_thresholds() {
        assert_eq!(ModelFit::classify(1000, 2000, 4000), ModelFit::Fits);
        assert_eq!(ModelFit::classify(3000, 2000, 4000), ModelFit::NeedsRpc);
        assert_eq!(ModelFit::classify(5000, 2000, 4000), ModelFit::WontFit);
        assert_eq!(ModelFit::Fits.list_badge(), "[OK]");
        assert_eq!(ModelFit::NeedsRpc.modal_badge(), "⚠️ needs RPC");
    }

    #[test]
    fn clamp_context_respects_model_limit() {
        assert_eq!(clamp_context_size(100, None), CONTEXT_STEP);
        assert_eq!(clamp_context_size(8192, Some(4096)), 4096);
        assert_eq!(clamp_context_size(2048, Some(8192)), 2048);
    }

    #[test]
    fn host_lmk_honors_percent() {
        assert_eq!(host_lmk_budget_mb(10_000, 75), 7_500);
        assert_eq!(host_lmk_budget_mb(10_000, 50), 5_000);
    }

    #[test]
    fn multi_rpc_args() {
        let d = LayerSplitDecision {
            total_layers: 10,
            host_layers: 5,
            remote_layers: 5,
            remote_endpoint: Some("a:1".into()),
            remote_endpoints: vec!["a:1".into(), "b:2".into()],
            tensor_split: Some("50,25,25".into()),
        };
        let args = d.build_llama_args();
        assert_eq!(args.iter().filter(|a| *a == "--rpc").count(), 2);
    }
}
