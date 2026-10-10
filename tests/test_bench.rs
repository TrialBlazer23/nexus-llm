//! Phase 12 §5.5 bench store + ranker integration.

mod support;

use nexus::bench::{
    backend_key, measure_once, run_benchmark, unix_now, BenchRunConfig, BenchSample, BenchStore,
};
use nexus::client::NexusClient;
use nexus::cluster::{
    rank_execution_plans, LinkQuality, MemoryPolicy, NodeBudget, PlacementCandidate,
    PlacementRequest, PlanTarget,
};
use nexus::config::MoeConfig;
use nexus::gguf::{GgufMetadata, KvCacheDtype};
use nexus::sysinfo::{AccelerationBackend, SystemProfile};
use std::collections::HashMap;
use std::time::Duration;
use support::fake_llama::{spawn_fake_llama, FakeLlamaConfig};
use tempfile::TempDir;
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
fn bench_store_round_trip_and_lookup() {
    let dir = TempDir::new().expect("tmpdir");
    let path = dir.path().join("bench.json");
    let mut store = BenchStore {
        version: 1,
        entries: vec![],
    };
    store.record(
        "phi.gguf",
        "local",
        AccelerationBackend::Vulkan,
        2048,
        BenchSample {
            gen_tok_s: 42.0,
            ttft_ms: Some(120),
            prompt_tok_s: Some(80.0),
            cache_hit_pct: None,
            cache_mb: None,
            measured_at: unix_now(),
        },
    );
    store.save(&path).expect("save");
    let loaded = BenchStore::load(&path).expect("load");
    assert_eq!(
        loaded.lookup_gen_tok_s("phi", "local", AccelerationBackend::Vulkan, 2048),
        Some(42.0)
    );
    assert_eq!(
        loaded.lookup_gen_tok_s_any_node("phi.gguf", AccelerationBackend::Vulkan, 2048),
        Some(42.0)
    );
    assert!(loaded
        .lookup_gen_tok_s("phi", "local", AccelerationBackend::X86Baseline, 2048)
        .is_none());
}

#[tokio::test]
async fn bench_measure_against_fake_llama() {
    let (base, handle) = spawn_fake_llama(FakeLlamaConfig {
        model_id: "bench-model".into(),
        sse_tokens: vec!["a".into(), "b".into(), "c".into(), "d".into()],
    })
    .await
    .expect("fake");

    let client = NexusClient::new(&base);
    let sample = measure_once(&client, "bench-model", "ping", 16)
        .await
        .expect("measure");
    assert!(sample.gen_tok_s > 0.0);
    assert!(sample.ttft_ms.is_some());

    let dir = TempDir::new().expect("tmpdir");
    let path = dir.path().join("bench.json");
    let mut store = BenchStore {
        version: 1,
        entries: vec![],
    };
    let cfg = BenchRunConfig {
        endpoint: &base,
        model: "bench-model",
        node_id: "ci-node",
        backend: AccelerationBackend::GenericCpu,
        context_size: 2048,
        runs: 2,
        prompt: "ping",
        max_tokens: 16,
    };
    let entry = run_benchmark(&mut store, &cfg)
        .await
        .expect("run_benchmark");
    store.save(&path).expect("save");
    assert_eq!(entry.samples.len(), 2);
    assert_eq!(backend_key(AccelerationBackend::GenericCpu), "cpu");

    handle.abort();
}

#[test]
fn ranker_prefers_measured_tok_s_over_heuristic() {
    let gguf = gguf_1p5b();
    let profile = SystemProfile {
        total_ram_mb: 16_000,
        available_ram_mb: 12_000,
        detected_backend: AccelerationBackend::Vulkan,
        recommended_threads: 4,
    };

    let mut store = BenchStore {
        version: 1,
        entries: vec![],
    };
    // Absurdly high measured throughput so ranking must pick it up.
    store.record(
        "1.5B",
        "local",
        AccelerationBackend::Vulkan,
        2048,
        BenchSample {
            gen_tok_s: 500.0,
            ttft_ms: Some(10),
            prompt_tok_s: Some(200.0),
            cache_hit_pct: None,
            cache_mb: None,
            measured_at: unix_now(),
        },
    );

    let moe = MoeConfig::default();
    let req = PlacementRequest {
        gguf: &gguf,
        policy: MemoryPolicy {
            max_ram_usage_percent: 75,
            mmap: true,
            mlock: false,
            kv_dtype: KvCacheDtype::F16,
            context_size: 2048,
            batch_size: 512,
        },
        local_profile: profile,
        local_name: "local".into(),
        local_gpu_layers: 99,
        enable_rpc: false,
        candidates: vec![],
        bench: Some(&store),
        moe: &moe,
    };
    let plans = rank_execution_plans(&req).expect("plans");
    let local = plans
        .iter()
        .find(|p| matches!(p.target, PlanTarget::LocalGpu { .. }))
        .expect("local gpu plan");
    // Heuristic Vulkan base is ~28; measured 500 must dominate.
    assert!(
        local.predicted_tok_s > 100.0,
        "expected measured tok/s, got {}",
        local.predicted_tok_s
    );
}

#[test]
fn ranker_without_bench_stays_heuristic() {
    let gguf = gguf_1p5b();
    let profile = SystemProfile {
        total_ram_mb: 16_000,
        available_ram_mb: 12_000,
        detected_backend: AccelerationBackend::Vulkan,
        recommended_threads: 4,
    };
    let moe = MoeConfig::default();
    let req = PlacementRequest {
        gguf: &gguf,
        policy: MemoryPolicy {
            max_ram_usage_percent: 75,
            mmap: true,
            mlock: false,
            kv_dtype: KvCacheDtype::F16,
            context_size: 2048,
            batch_size: 512,
        },
        local_profile: profile,
        local_name: "local".into(),
        local_gpu_layers: 99,
        enable_rpc: true,
        bench: None,
        moe: &moe,
        candidates: vec![PlacementCandidate {
            name: "desktop".into(),
            budget: NodeBudget::new(Uuid::new_v4(), "desktop", 24_000),
            backend: AccelerationBackend::X86Baseline,
            rpc_endpoint: Some("10.0.0.5:50052".into()),
            link: LinkQuality::unknown(),
            is_local: false,
            thermal_index: 20,
            moe_stream: false,
            moe_cache_ceil_mb: 0,
            total_ram_mb: 0,
        }],
    };
    let plans = rank_execution_plans(&req).expect("plans");
    assert!(!plans.is_empty());
    let _ = Duration::from_millis(1);
}
