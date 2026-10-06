use crate::gguf::GgufMetadata;
use thiserror::Error;
use uuid::Uuid;

/// Safety multiplier to guard against Out-Of-Memory (OOM) and Android Low Memory Killer (LMK).
pub const LMK_SAFETY_PERCENT: f64 = 0.75;

/// Default context window used when GGUF metadata has no `context_length`.
pub const DEFAULT_CONTEXT_SIZE: usize = 4096;

/// Step size for Hub `[+]`/`[-]` context adjustments.
pub const CONTEXT_STEP: usize = 512;

/// Default standalone RAM budget guideline in Megabytes.
/// Deprecated: Memory budgets are now determined dynamically via SystemProfile.
#[deprecated(note = "use dynamic host budget resolution")]
pub const NODE_A_MAX_STANDALONE_MB: u64 = 8500;

/// Default RAM allocation cap for constrained RPC workers in Megabytes.
/// Deprecated: Worker caps are now configured via config.cluster.max_rpc_ram_mb or dynamic policy.
#[deprecated(note = "use dynamic worker allocatable budget")]
pub const NODE_B_MAX_RPC_RAM_MB: u64 = 1800;

/// Whether a model+KV footprint fits on a host, needs RPC offload, or exceeds the cluster.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModelFit {
    Fits,
    NeedsRpc,
    WontFit,
}

impl ModelFit {
    /// Classify fit given required MB, host-alone capacity, and host+RPC cluster capacity.
    pub fn classify(required_mb: u64, host_cap_mb: u64, cluster_cap_mb: u64) -> Self {
        if required_mb <= host_cap_mb {
            Self::Fits
        } else if required_mb <= cluster_cap_mb {
            Self::NeedsRpc
        } else {
            Self::WontFit
        }
    }

    /// Models-list badge text (ASCII for narrow columns).
    pub fn list_badge(self) -> &'static str {
        match self {
            Self::Fits => "[OK]",
            Self::NeedsRpc => "[RPC]",
            Self::WontFit => "[OOM]",
        }
    }

    /// Target-selection modal badge.
    pub fn modal_badge(self) -> &'static str {
        match self {
            Self::Fits => "✅ fits",
            Self::NeedsRpc => "⚠️ needs RPC",
            Self::WontFit => "❌ won't fit",
        }
    }
}

/// Allocatable host budget under the LMK safety factor.
pub fn host_lmk_budget_mb(available_ram_mb: u64) -> u64 {
    (available_ram_mb as f64 * LMK_SAFETY_PERCENT) as u64
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

    #[error("No RPC worker available to offload {overflow_mb} MB beyond host's {host_max_mb} MB budget")]
    NoRpcWorkerAvailable {
        overflow_mb: u64,
        host_max_mb: u64,
    },

    #[error("Model has invalid block/layer count")]
    InvalidLayerCount,
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

/// Resulting layer allocation decision between host and remote worker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LayerSplitDecision {
    pub total_layers: u32,
    pub host_layers: u32,
    pub remote_layers: u32,
    pub remote_endpoint: Option<String>,
    pub tensor_split: Option<String>,
}

impl LayerSplitDecision {
    /// True if layers are offloaded to a remote RPC worker.
    pub fn is_distributed(&self) -> bool {
        self.remote_layers > 0 && self.remote_endpoint.is_some()
    }

    /// Construct CLI arguments for llama-server.
    pub fn build_llama_args(&self) -> Vec<String> {
        let mut args = Vec::new();
        if self.is_distributed() {
            if let Some(endpoint) = &self.remote_endpoint {
                args.push("--rpc".to_string());
                args.push(endpoint.clone());
                args.push("--split-mode".to_string());
                args.push("layer".to_string());
                if let Some(ts) = &self.tensor_split {
                    args.push("--tensor-split".to_string());
                    args.push(ts.clone());
                }
            }
        }
        args
    }
}

pub struct ClusterCoordinator;

impl ClusterCoordinator {
    /// Calculate the available memory budget dynamically for a host and optional RPC worker(s).
    ///
    /// - `host_avail_mb`: Available physical RAM on the host node.
    /// - `host_cap_mb`: Optional explicit allocation cap on the host. If None, 75% safety limit is used.
    /// - `remote_peer_avail_mb`: Optional allocatable RAM on the chosen remote RPC worker.
    pub fn calculate_dynamic_budget(
        host_avail_mb: u64,
        host_cap_mb: Option<u64>,
        remote_peer_avail_mb: Option<u64>,
    ) -> ClusterBudget {
        let host_safety_budget = (host_avail_mb as f64 * LMK_SAFETY_PERCENT) as u64;
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

    /// Calculate memory budget for a target host and candidate RPC workers using NodeBudget profiles.
    pub fn calculate_from_nodes(
        host: &NodeBudget,
        worker: Option<&NodeBudget>,
    ) -> ClusterBudget {
        let host_max_mb = host.allocatable_mb;
        let remote_max_mb = worker.map_or(0, |w| w.allocatable_mb);
        let total_cluster_mb = host_max_mb + remote_max_mb;

        ClusterBudget {
            host_max_mb,
            remote_max_mb,
            total_cluster_mb,
        }
    }

    /// Calculate budget dynamically from host available RAM (scaled to 75% safety) and remote worker RAM.
    pub fn calculate_budget(host_avail_mb: u64, remote_peer_avail_mb: Option<u64>) -> ClusterBudget {
        Self::calculate_dynamic_budget(host_avail_mb, None, remote_peer_avail_mb)
    }

    /// Plan layer splitting using greedy host-first watermarking.
    pub fn plan_layer_split(
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

        // Rule 1: Fits entirely within host's memory budget -> 100% local on host
        if total_required_mb <= budget.host_max_mb {
            return Ok(LayerSplitDecision {
                total_layers,
                host_layers: total_layers,
                remote_layers: 0,
                remote_endpoint: None,
                tensor_split: None,
            });
        }

        // Rule 2: Exceeds host -> Must have an RPC worker available
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

        // Rule 3: Exceeds total cluster capacity -> Reject early
        if total_required_mb > budget.total_cluster_mb {
            return Err(ClusterError::ClusterMemoryCapExceeded {
                required_mb: total_required_mb,
                cluster_max_mb: budget.total_cluster_mb,
                host_max_mb: budget.host_max_mb,
                remote_max_mb: budget.remote_max_mb,
            });
        }

        // Calculate layer offload count:
        // Remote fraction of weights based on overflow beyond host budget
        let overflow_mb = total_required_mb - budget.host_max_mb;
        let remote_fraction = (overflow_mb as f64) / (total_required_mb as f64);
        let mut remote_layers = (total_layers as f64 * remote_fraction).ceil() as u32;
        if remote_layers == 0 {
            remote_layers = 1;
        }
        if remote_layers >= total_layers {
            remote_layers = total_layers - 1; // Host must run at least 1 layer (greedy host)
        }
        let host_layers = total_layers - remote_layers;

        // Tensor split proportion e.g. "82,18"
        let host_ratio = ((host_layers as f64 / total_layers as f64) * 100.0).round() as u32;
        let remote_ratio = 100 - host_ratio;
        let tensor_split = format!("{},{}", host_ratio, remote_ratio);

        Ok(LayerSplitDecision {
            total_layers,
            host_layers,
            remote_layers,
            remote_endpoint: Some(endpoint),
            tensor_split: Some(tensor_split),
        })
    }

    /// Convenience helper using GgufMetadata directly.
    pub fn plan_from_gguf(
        gguf: &GgufMetadata,
        ctx: usize,
        budget: &ClusterBudget,
        rpc_endpoint: Option<&str>,
    ) -> Result<LayerSplitDecision, ClusterError> {
        let total_layers = gguf.block_count.unwrap_or(32) as u32;
        let exact_kv_bytes = gguf.exact_kv_cache_bytes(ctx);
        Self::plan_layer_split(
            gguf.file_size_bytes,
            exact_kv_bytes,
            total_layers,
            budget,
            rpc_endpoint,
        )
    }
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
}
