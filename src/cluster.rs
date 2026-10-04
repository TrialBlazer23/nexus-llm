use crate::gguf::GgufMetadata;
use thiserror::Error;

/// Maximum standalone RAM budget for Node A (Galaxy S23 Ultra) in Megabytes.
pub const NODE_A_MAX_STANDALONE_MB: u64 = 8500;

/// Strict maximum RAM allocation for rpc-server on Node B (Mac Core 2 Duo) in Megabytes.
pub const NODE_B_MAX_RPC_RAM_MB: u64 = 1800;

/// Safety multiplier to guard against Android Low Memory Killer (LMK).
pub const LMK_SAFETY_PERCENT: f64 = 0.75;

#[derive(Error, Debug, PartialEq, Eq)]
pub enum ClusterError {
    #[error("Cluster memory ceiling exceeded: Model and KV cache require {required_mb} MB, but maximum cluster capacity is {cluster_max_mb} MB (Host: {host_max_mb} MB, RPC Worker: {remote_max_mb} MB)")]
    ClusterMemoryCapExceeded {
        required_mb: u64,
        cluster_max_mb: u64,
        host_max_mb: u64,
        remote_max_mb: u64,
    },

    #[error("No RPC worker available to offload {overflow_mb} MB beyond Node A's {host_max_mb} MB budget")]
    NoRpcWorkerAvailable {
        overflow_mb: u64,
        host_max_mb: u64,
    },

    #[error("Model has invalid block/layer count")]
    InvalidLayerCount,
}

/// Represents the memory budget available across the heterogeneous cluster.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClusterBudget {
    pub host_max_mb: u64,
    pub remote_max_mb: u64,
    pub total_cluster_mb: u64,
}

/// Resulting layer allocation decision between Node A and Node B.
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
    /// Calculate the available memory budget for the cluster.
    /// Node A is capped at min(0.75 * host_available_mb, 8500 MB).
    /// Node B is capped at min(remote_peer_available_mb, 1800 MB).
    pub fn calculate_budget(host_avail_mb: u64, remote_peer_avail_mb: Option<u64>) -> ClusterBudget {
        let host_lmk_ceiling = (host_avail_mb as f64 * LMK_SAFETY_PERCENT) as u64;
        let host_max_mb = host_lmk_ceiling.min(NODE_A_MAX_STANDALONE_MB);
        let remote_max_mb = remote_peer_avail_mb.map_or(0, |avail| avail.min(NODE_B_MAX_RPC_RAM_MB));
        let total_cluster_mb = host_max_mb + remote_max_mb;

        ClusterBudget {
            host_max_mb,
            remote_max_mb,
            total_cluster_mb,
        }
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

        // Rule 1: Fits entirely within Node A's memory budget -> 100% on Node A
        if total_required_mb <= budget.host_max_mb {
            return Ok(LayerSplitDecision {
                total_layers,
                host_layers: total_layers,
                remote_layers: 0,
                remote_endpoint: None,
                tensor_split: None,
            });
        }

        // Rule 2: Exceeds Node A -> Must have an RPC worker available
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

        // Rule 3: Exceeds total cluster capacity (Node A + Node B max) -> Reject early
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
            remote_layers = total_layers - 1; // Node A must run at least 1 layer (greedy host)
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
