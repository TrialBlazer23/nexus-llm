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
        expert_count: None,
        expert_used_count: None,
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
        bench: None,
        moe_stream_enabled: true,
        moe_cache_ceil_mb: 0,
        candidates: vec![PlacementCandidate {
            name: "big-desktop".into(),
            budget: NodeBudget::new(Uuid::new_v4(), "big-desktop", 28_000),
            backend: AccelerationBackend::X86Baseline,
            rpc_endpoint: Some("10.0.0.9:50052".into()),
            link: LinkQuality::unknown(),
            is_local: false,
            thermal_index: 10,
            moe_stream: false,
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

#[test]
fn ranks_local_moe_stream_for_oversize_moe() {
    use nexus::gguf::GgufTensorInfo;
    let expert = GgufTensorInfo {
        name: "blk.0.ffn_gate_exps.weight".into(),
        n_dims: 2,
        dims: [32, 64, 0, 0],
        ggml_type: 2,
        offset: 0,
        nbytes: 12_000_000_000, // 12 GB experts
        layer_index: Some(0),
    };
    let dense = GgufTensorInfo {
        name: "blk.0.attn_q.weight".into(),
        n_dims: 2,
        dims: [32, 64, 0, 0],
        ggml_type: 2,
        offset: 100,
        nbytes: 800_000_000, // 800 MB dense
        layer_index: Some(0),
    };
    let gguf = GgufMetadata {
        version: 3,
        tensor_count: 2,
        kv_count: 0,
        metadata: HashMap::new(),
        architecture: Some("qwen3moe".into()),
        model_name: Some("big-moe".into()),
        context_length: Some(4096),
        block_count: Some(1),
        head_count: Some(8),
        head_count_kv: Some(8),
        embedding_length: Some(512),
        expert_count: Some(128),
        expert_used_count: Some(8),
        file_size_bytes: 18_000_000_000,
        tensors: vec![expert, dense],
        quant_label: Some("Q4_0".into()),
    };
    let profile = SystemProfile {
        total_ram_mb: 12_000,
        available_ram_mb: 10_000,
        detected_backend: AccelerationBackend::ArmCpuDotProd,
        recommended_threads: 4,
    };
    let req = PlacementRequest {
        gguf: &gguf,
        policy: MemoryPolicy::from_safety(75, true, false, 2048),
        local_profile: profile,
        local_name: "phone".into(),
        local_gpu_layers: 0,
        enable_rpc: true,
        bench: None,
        moe_stream_enabled: true,
        moe_cache_ceil_mb: 3500,
        candidates: vec![PlacementCandidate {
            name: "worker".into(),
            budget: NodeBudget::new(Uuid::new_v4(), "worker", 1800),
            backend: AccelerationBackend::X86Baseline,
            rpc_endpoint: Some("10.0.0.2:50052".into()),
            link: LinkQuality::unknown(),
            is_local: false,
            thermal_index: 10,
            moe_stream: false,
        }],
    };
    let plans = rank_execution_plans(&req).expect("plans");
    assert!(
        plans
            .iter()
            .any(|p| matches!(p.target, PlanTarget::LocalMoeStream { .. })),
        "expected LocalMoeStream plan, got {:?}",
        plans.iter().map(|p| &p.target).collect::<Vec<_>>()
    );
    // MoE stream should outrank fragile distributed RPC for this case.
    let moe_idx = plans
        .iter()
        .position(|p| matches!(p.target, PlanTarget::LocalMoeStream { .. }))
        .unwrap();
    if let Some(dist_idx) = plans
        .iter()
        .position(|p| matches!(p.target, PlanTarget::Distributed { .. }))
    {
        assert!(moe_idx < dist_idx);
    }
    // Cold start uses the 2.2 tok/s heuristic when no MoE bench sample exists.
    assert!(
        (plans[moe_idx].predicted_tok_s - 2.2).abs() < 0.01,
        "expected cold-start 2.2, got {}",
        plans[moe_idx].predicted_tok_s
    );
}

#[test]
fn local_moe_stream_uses_measured_bench_tok_s() {
    use nexus::bench::{unix_now, BenchSample, BenchStore, BACKEND_MOE_STREAM};
    use nexus::gguf::GgufTensorInfo;

    let expert = GgufTensorInfo {
        name: "blk.0.ffn_gate_exps.weight".into(),
        n_dims: 2,
        dims: [32, 64, 0, 0],
        ggml_type: 2,
        offset: 0,
        nbytes: 12_000_000_000,
        layer_index: Some(0),
    };
    let dense = GgufTensorInfo {
        name: "blk.0.attn_q.weight".into(),
        n_dims: 2,
        dims: [32, 64, 0, 0],
        ggml_type: 2,
        offset: 100,
        nbytes: 800_000_000,
        layer_index: Some(0),
    };
    let gguf = GgufMetadata {
        version: 3,
        tensor_count: 2,
        kv_count: 0,
        metadata: HashMap::new(),
        architecture: Some("qwen3moe".into()),
        model_name: Some("big-moe".into()),
        context_length: Some(4096),
        block_count: Some(1),
        head_count: Some(8),
        head_count_kv: Some(8),
        embedding_length: Some(512),
        expert_count: Some(128),
        expert_used_count: Some(8),
        file_size_bytes: 18_000_000_000,
        tensors: vec![expert, dense],
        quant_label: Some("Q4_0".into()),
    };
    let mut store = BenchStore {
        version: 1,
        entries: vec![],
    };
    store.record_with_backend_key(
        "big-moe",
        "phone",
        BACKEND_MOE_STREAM,
        2048,
        BenchSample {
            gen_tok_s: 3.5,
            ttft_ms: None,
            prompt_tok_s: None,
            cache_hit_pct: Some(70.0),
            measured_at: unix_now(),
        },
    );
    let profile = SystemProfile {
        total_ram_mb: 12_000,
        available_ram_mb: 10_000,
        detected_backend: AccelerationBackend::ArmCpuDotProd,
        recommended_threads: 4,
    };
    let req = PlacementRequest {
        gguf: &gguf,
        policy: MemoryPolicy::from_safety(75, true, false, 2048),
        local_profile: profile,
        local_name: "phone".into(),
        local_gpu_layers: 0,
        enable_rpc: false,
        bench: Some(&store),
        moe_stream_enabled: true,
        moe_cache_ceil_mb: 3500,
        candidates: vec![],
    };
    let plans = rank_execution_plans(&req).expect("plans");
    let moe = plans
        .iter()
        .find(|p| matches!(p.target, PlanTarget::LocalMoeStream { .. }))
        .expect("LocalMoeStream plan");
    assert!(
        (moe.predicted_tok_s - 3.5).abs() < 0.01,
        "expected measured 3.5, got {}",
        moe.predicted_tok_s
    );
    assert_eq!(
        moe.context_size, 2048,
        "ExecutionPlan.context_size must match knob winner"
    );
}
