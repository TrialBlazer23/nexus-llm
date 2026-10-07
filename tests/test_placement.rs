//! Phase 11 placement integration checks (no live llama-server required).

use nexus::cluster::{
    host_lmk_budget_mb, rank_execution_plans, LinkQuality, MemoryPlan, MemoryPolicy, NodeBudget,
    PlacementCandidate, PlacementRequest, PlanTarget, Verdict,
};
use nexus::config::NexusConfig;
use nexus::gguf::{GgufMetadata, KvCacheDtype};
use nexus::sysinfo::{AccelerationBackend, SystemProfile};
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
        file_size_bytes: 950_000_000,
        tensors: vec![],
        quant_label: Some("Q4_K".into()),
    }
}

#[test]
fn memory_plan_beats_heuristic_for_small_model() {
    let gguf = gguf_1p5b();
    let profile = SystemProfile {
        total_ram_mb: 4_000,
        available_ram_mb: 2_000,
        detected_backend: AccelerationBackend::X86Baseline,
        recommended_threads: 2,
    };
    assert!(!profile.can_safely_load(gguf.file_size_bytes, 4096));
    let policy = MemoryPolicy {
        max_ram_usage_percent: 75,
        mmap: true,
        mlock: false,
        kv_dtype: KvCacheDtype::F16,
        context_size: 4096,
        batch_size: 512,
    };
    let plan = MemoryPlan::from_gguf(&gguf, &profile, &policy, false);
    assert!(
        matches!(plan.verdict, Verdict::Fits | Verdict::FitsIfQuantizedKv),
        "got {:?}",
        plan.verdict
    );
    assert!(plan.kv_cache_mb < 800);
}

#[test]
fn host_lmk_percent_and_large_rpc_config() {
    assert_eq!(host_lmk_budget_mb(10_000, 90), 9_000);
    let mut cfg = NexusConfig::default();
    cfg.cluster.max_rpc_ram_mb = 32_768;
    assert!(cfg.validate().is_ok());
}

#[test]
fn ranked_plans_include_predicted_tok_s() {
    let gguf = gguf_1p5b();
    let profile = SystemProfile {
        total_ram_mb: 16_000,
        available_ram_mb: 12_000,
        detected_backend: AccelerationBackend::Vulkan,
        recommended_threads: 4,
    };
    let req = PlacementRequest {
        gguf: &gguf,
        policy: MemoryPolicy::from_safety(75, true, false, 2048),
        local_profile: profile,
        local_name: "local".into(),
        local_gpu_layers: 99,
        enable_rpc: true,
        candidates: vec![PlacementCandidate {
            name: "big-desktop".into(),
            budget: NodeBudget::new(Uuid::new_v4(), "big-desktop", 28_000),
            backend: AccelerationBackend::X86Baseline,
            rpc_endpoint: Some("10.0.0.9:50052".into()),
            link: LinkQuality::unknown(),
            is_local: false,
            thermal_index: 10,
        }],
    };
    let plans = rank_execution_plans(&req).expect("plans");
    assert!(!plans.is_empty());
    assert!(plans.iter().all(|p| p.predicted_tok_s > 0.0));
    assert!(plans.iter().any(|p| matches!(
        p.target,
        PlanTarget::LocalGpu { .. } | PlanTarget::LocalCpu | PlanTarget::Remote { .. }
    )));
}
