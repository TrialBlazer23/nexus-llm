//! Explicit memory budget planning (Phase 11 §3.1 / §3.2).

use crate::gguf::{GgufMetadata, KvCacheDtype};
use crate::sysinfo::{AccelerationBackend, SystemProfile};
use serde::{Deserialize, Serialize};

/// Outcome of comparing a model's resident footprint to a node budget.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Verdict {
    Fits,
    FitsWithOffload,
    FitsIfQuantizedKv,
    Exceeds,
}

/// Suggested remediation when a plan does not fit cleanly.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Remediation {
    ReduceCtx {
        to: usize,
    },
    QuantizeKv {
        dtype: String,
    },
    OffloadLayers {
        n: u32,
        peer_hint: String,
    },
    /// Prefer BigMoeOnEdge flash streaming with the given expert-cache ceiling (MiB).
    EnableMoeStream {
        cache_ceil_mb: u64,
    },
}

/// Operator / config policy that drives MemoryPlan arithmetic.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemoryPolicy {
    pub max_ram_usage_percent: u8,
    pub mmap: bool,
    pub mlock: bool,
    pub kv_dtype: KvCacheDtype,
    pub context_size: usize,
    pub batch_size: u32,
}

impl Default for MemoryPolicy {
    fn default() -> Self {
        Self {
            max_ram_usage_percent: 75,
            mmap: true,
            mlock: false,
            kv_dtype: KvCacheDtype::F16,
            context_size: 4096,
            batch_size: 512,
        }
    }
}

impl MemoryPolicy {
    pub fn from_safety(
        max_ram_usage_percent: u8,
        mmap: bool,
        mlock: bool,
        context_size: usize,
    ) -> Self {
        Self {
            max_ram_usage_percent: max_ram_usage_percent.clamp(1, 100),
            mmap,
            mlock,
            kv_dtype: KvCacheDtype::F16,
            context_size,
            batch_size: 512,
        }
    }
}

/// Explicit breakdown of model memory requirements vs a node budget.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemoryPlan {
    pub weights_mb: u64,
    pub weights_resident_mb: u64,
    pub kv_cache_mb: u64,
    pub compute_buffer_mb: u64,
    pub headroom_mb: i64,
    pub budget_mb: u64,
    pub verdict: Verdict,
    pub remediations: Vec<Remediation>,
    pub quant_label: Option<String>,
}

impl MemoryPlan {
    /// Anonymous / LMK-relevant footprint in MB.
    pub fn required_anonymous_mb(&self) -> u64 {
        self.weights_resident_mb
            .saturating_add(self.kv_cache_mb)
            .saturating_add(self.compute_buffer_mb)
    }

    /// Build a plan for a GGUF model against a probed profile and policy.
    pub fn from_gguf(
        gguf: &GgufMetadata,
        profile: &SystemProfile,
        policy: &MemoryPolicy,
        cluster_offload_available: bool,
    ) -> Self {
        let budget_mb =
            profile.max_allowed_memory_bytes_pct(policy.max_ram_usage_percent) / (1024 * 1024);
        Self::from_gguf_budget(
            gguf,
            budget_mb,
            profile.detected_backend,
            policy,
            cluster_offload_available,
        )
    }

    /// Build a plan against an explicit budget (MB).
    pub fn from_gguf_budget(
        gguf: &GgufMetadata,
        budget_mb: u64,
        backend: AccelerationBackend,
        policy: &MemoryPolicy,
        cluster_offload_available: bool,
    ) -> Self {
        let weights_bytes = gguf.weights_bytes();
        let weights_mb = weights_bytes / (1024 * 1024);
        let weights_resident_mb = if policy.mmap && !policy.mlock {
            0
        } else {
            weights_mb
        };

        let kv_bytes = if gguf.has_geometry() {
            gguf.exact_kv_cache_bytes_dtype(policy.context_size, policy.kv_dtype)
        } else {
            // Dims missing: last-resort heuristic (callers should refuse planning).
            crate::sysinfo::SystemProfile::estimate_kv_cache_bytes(policy.context_size)
        };
        let kv_cache_mb = kv_bytes / (1024 * 1024);
        let compute_buffer_mb = estimate_compute_buffer_mb(backend, policy.batch_size);

        // Dense fit counts the whole weight file. `weights_resident_mb` stays 0
        // under mmap without mlock: that field is anonymous RSS, not the verdict.
        let load_estimate_mb = weights_mb
            .saturating_add(kv_cache_mb)
            .saturating_add(compute_buffer_mb);

        let headroom_mb = budget_mb as i64 - load_estimate_mb as i64;

        let mut remediations = Vec::new();
        let verdict = if load_estimate_mb <= budget_mb {
            Verdict::Fits
        } else if policy.kv_dtype == KvCacheDtype::F16 {
            let q8_kv = gguf.exact_kv_cache_bytes_dtype(policy.context_size, KvCacheDtype::Q8_0)
                / (1024 * 1024);
            let q8_load = weights_mb
                .saturating_add(q8_kv)
                .saturating_add(compute_buffer_mb);
            if q8_load <= budget_mb {
                remediations.push(Remediation::QuantizeKv {
                    dtype: "q8_0".into(),
                });
                Verdict::FitsIfQuantizedKv
            } else if gguf.streamable_moe() {
                remediations.push(Remediation::EnableMoeStream { cache_ceil_mb: 0 });
                Verdict::Exceeds
            } else if cluster_offload_available {
                remediations.push(Remediation::OffloadLayers {
                    n: 0,
                    peer_hint: "rpc-worker".into(),
                });
                Verdict::FitsWithOffload
            } else {
                suggest_ctx_remediation(gguf, policy, budget_mb, &mut remediations);
                Verdict::Exceeds
            }
        } else if gguf.streamable_moe() {
            remediations.push(Remediation::EnableMoeStream { cache_ceil_mb: 0 });
            Verdict::Exceeds
        } else if cluster_offload_available {
            remediations.push(Remediation::OffloadLayers {
                n: 0,
                peer_hint: "rpc-worker".into(),
            });
            Verdict::FitsWithOffload
        } else {
            suggest_ctx_remediation(gguf, policy, budget_mb, &mut remediations);
            Verdict::Exceeds
        };

        // Half-context remediation when over budget.
        if matches!(verdict, Verdict::Exceeds | Verdict::FitsWithOffload)
            && policy.context_size > 512
        {
            let half = (policy.context_size / 2).max(512);
            if !remediations
                .iter()
                .any(|r| matches!(r, Remediation::ReduceCtx { .. }))
            {
                remediations.push(Remediation::ReduceCtx { to: half });
            }
        }

        MemoryPlan {
            weights_mb,
            weights_resident_mb,
            kv_cache_mb,
            compute_buffer_mb,
            headroom_mb,
            budget_mb,
            verdict,
            remediations,
            quant_label: gguf.quant_label.clone(),
        }
    }

    pub fn list_badge(&self) -> &'static str {
        match self.verdict {
            Verdict::Fits => "[OK]",
            Verdict::FitsWithOffload | Verdict::FitsIfQuantizedKv => "[RPC]",
            Verdict::Exceeds => {
                if self
                    .remediations
                    .iter()
                    .any(|r| matches!(r, Remediation::EnableMoeStream { .. }))
                {
                    "[MOE]"
                } else {
                    "[OOM]"
                }
            }
        }
    }
}

fn suggest_ctx_remediation(
    gguf: &GgufMetadata,
    policy: &MemoryPolicy,
    budget_mb: u64,
    remediations: &mut Vec<Remediation>,
) {
    let mut ctx = policy.context_size;
    while ctx > 512 {
        ctx /= 2;
        let kv = gguf.exact_kv_cache_bytes_dtype(ctx, policy.kv_dtype) / (1024 * 1024);
        let weights_mb = gguf.weights_bytes() / (1024 * 1024);
        let compute = 256u64;
        if weights_mb.saturating_add(kv).saturating_add(compute) <= budget_mb {
            remediations.push(Remediation::ReduceCtx { to: ctx });
            return;
        }
    }
}

/// Conservative compute-buffer defaults (MB). Calibrate on target hardware later.
pub fn estimate_compute_buffer_mb(backend: AccelerationBackend, batch_size: u32) -> u64 {
    let base: u64 = match backend {
        AccelerationBackend::Vulkan => 512,
        AccelerationBackend::ArmCpuDotProd => 384,
        AccelerationBackend::X86Baseline => 256,
        AccelerationBackend::GenericCpu => 320,
    };
    let batch_factor = (batch_size as u64 / 512).max(1);
    base.saturating_mul(batch_factor).min(2048u64)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn tiny_1p5b() -> GgufMetadata {
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
            file_size_bytes: 1_000_000_000, // ~953 MB Q4 weights
            tensors: vec![],
            quant_label: Some("Q4_K".into()),
        }
    }

    #[test]
    fn dense_mmap_counts_full_weights() {
        let mut heavy = tiny_1p5b();
        heavy.file_size_bytes = 4_500 * 1024 * 1024;
        let mut light = tiny_1p5b();
        light.file_size_bytes = 900 * 1024 * 1024;
        let cpu = SystemProfile {
            total_ram_mb: 6_000,
            available_ram_mb: 4_096,
            detected_backend: AccelerationBackend::X86Baseline,
            recommended_threads: 2,
        };
        let policy = MemoryPolicy {
            max_ram_usage_percent: 75,
            mmap: true,
            mlock: false,
            kv_dtype: KvCacheDtype::F16,
            context_size: 512,
            batch_size: 512,
        };
        let rejected = MemoryPlan::from_gguf(&heavy, &cpu, &policy, false);
        assert_eq!(
            rejected.verdict,
            Verdict::Exceeds,
            "headroom {}",
            rejected.headroom_mb
        );
        let accepted = MemoryPlan::from_gguf(&light, &cpu, &policy, false);
        assert!(
            matches!(accepted.verdict, Verdict::Fits | Verdict::FitsIfQuantizedKv),
            "{:?}",
            accepted.verdict
        );
        let phone = SystemProfile {
            total_ram_mb: 16_000,
            available_ram_mb: 12_000,
            detected_backend: AccelerationBackend::Vulkan,
            recommended_threads: 4,
        };
        let on_phone = MemoryPlan::from_gguf(&heavy, &phone, &policy, false);
        assert!(
            matches!(on_phone.verdict, Verdict::Fits | Verdict::FitsIfQuantizedKv),
            "{:?}",
            on_phone.verdict
        );
    }

    #[test]
    fn honors_ram_percent() {
        let gguf = tiny_1p5b();
        let profile = SystemProfile {
            total_ram_mb: 16_000,
            available_ram_mb: 10_000,
            detected_backend: AccelerationBackend::X86Baseline,
            recommended_threads: 2,
        };
        let mut policy = MemoryPolicy {
            context_size: 4096,
            mmap: true,
            max_ram_usage_percent: 50,
            ..Default::default()
        };
        let p50 = MemoryPlan::from_gguf(&gguf, &profile, &policy, false);
        policy.max_ram_usage_percent = 90;
        let p90 = MemoryPlan::from_gguf(&gguf, &profile, &policy, false);
        assert!(p90.budget_mb > p50.budget_mb);
    }
}
