//! Joint (context_size, cache_ceil) search under MoE stream LMK.

use crate::bench::{BenchStore, BACKEND_MOE_STREAM};
use crate::gguf::GgufMetadata;
use crate::sysinfo::{AccelerationBackend, SystemProfile};

use super::{clamp_context_size, CONTEXT_STEP};

/// Reference MoE flash-stream throughput for A3B-class models (8/128 experts)
/// with a warm expert cache on ArmDotProd (Phase 16 soak).
pub const MOE_COLD_START_TOK_S: f32 = 2.2;

/// Reference active-expert fraction for [`MOE_COLD_START_TOK_S`] (Qwen*30B-A3B).
const REF_ACTIVE_FRAC: f32 = 8.0 / 128.0;

/// One feasible (ctx, cache) point under stream LMK.
#[derive(Debug, Clone, PartialEq)]
pub struct MoeStreamKnobPlan {
    pub context_size: usize,
    /// Expert cache ceiling (MiB): `0` or `>= 2000`.
    pub cache_ceil_mb: u64,
    /// Effective cache budget passed to LMK / `--cache-mb` (ceil when ≥2000, else 0).
    pub cache_mb: u64,
    pub footprint_mb: u64,
    pub predicted_tok_s: f32,
}

/// Predict MoE flash-stream tok/s from measured bench or active-params + flash I/O.
///
/// Measured samples are returned unchanged. Cold-start uses `expert_used_count /
/// expert_count`, a cache-vs-working-set I/O penalty, and a small backend nudge.
pub fn predict_moe_stream_tok_s(
    gguf: &GgufMetadata,
    cache_mb: u64,
    backend: AccelerationBackend,
    measured: Option<f32>,
) -> f32 {
    if let Some(m) = measured.filter(|v| *v > 0.0) {
        return m;
    }

    let used = gguf.expert_used_count.unwrap_or(8) as f32;
    let total = gguf.expert_count.unwrap_or(128).max(1) as f32;
    let active_frac = (used / total).clamp(1.0 / 256.0, 1.0);

    let compute = MOE_COLD_START_TOK_S * (REF_ACTIVE_FRAC / active_frac).sqrt();

    let expert_mb = {
        let bytes = gguf.expert_weight_bytes();
        if bytes > 0 {
            bytes / (1024 * 1024)
        } else {
            gguf.file_size_bytes.saturating_mul(65) / 100 / (1024 * 1024)
        }
    } as f32;
    let working_set_mb = (expert_mb * active_frac * 4.0).max(500.0);
    let hit = if cache_mb == 0 {
        0.0
    } else {
        ((cache_mb as f32) / working_set_mb).min(1.0)
    };
    let io_pen = 0.55 + 0.45 * hit;

    let backend_nudge = match backend {
        AccelerationBackend::ArmCpuDotProd | AccelerationBackend::Vulkan => 1.0,
        AccelerationBackend::X86Baseline => 0.85,
        AccelerationBackend::GenericCpu => 0.9,
    };

    (compute * io_pen * backend_nudge).clamp(0.3, 8.0)
}

/// Search a small grid of `(context_size, cache_ceil)` pairs and pick the best
/// feasible plan under stream LMK.
///
/// Scoring: higher measured/heuristic tok/s, then higher context, then higher ceil.
#[allow(clippy::too_many_arguments)]
pub fn plan_moe_stream_knobs(
    gguf: &GgufMetadata,
    profile: &SystemProfile,
    percent: u8,
    desired_ctx: usize,
    default_ceil_mb: u64,
    bench: Option<&BenchStore>,
    model_key: &str,
    node_id: &str,
) -> Option<MoeStreamKnobPlan> {
    if !gguf.streamable_moe() {
        return None;
    }

    let budget_mb = profile.max_allowed_memory_bytes_pct(percent) / (1024 * 1024);
    let resident_mb = gguf.moe_resident_weight_bytes() / (1024 * 1024);
    let hit = bench.and_then(|s| s.lookup_moe_cache_hit(model_key, node_id, desired_ctx));
    // Ceil-only governor: bias the grid seed from rolling hit% (lossy applied at spawn).
    let default_ceil_mb = super::govern_moe_stream(
        &crate::config::MoeConfig::default(),
        default_ceil_mb,
        budget_mb,
        hit,
    )
    .default_ceil_mb;
    let contexts = context_candidates(desired_ctx, gguf.context_length);

    let mut best: Option<MoeStreamKnobPlan> = None;
    for ctx in contexts {
        let kv_mb = gguf.exact_kv_cache_bytes(ctx) / (1024 * 1024);
        let max_ceil = budget_mb.saturating_sub(resident_mb).saturating_sub(kv_mb);
        for ceil in ceil_candidates(default_ceil_mb, max_ceil) {
            let cache_mb = if ceil >= 2000 { ceil } else { 0 };
            if !profile.can_safely_moe_stream_pct(gguf, ctx, cache_mb, percent) {
                continue;
            }
            let footprint = gguf.moe_stream_footprint_bytes(ctx, cache_mb);
            let footprint_mb = footprint / (1024 * 1024);
            let measured = measured_moe_tok_s(bench, model_key, node_id, ctx);
            let tok = predict_moe_stream_tok_s(gguf, cache_mb, profile.detected_backend, measured);
            let candidate = MoeStreamKnobPlan {
                context_size: ctx,
                cache_ceil_mb: ceil,
                cache_mb,
                footprint_mb,
                predicted_tok_s: tok,
            };
            if is_better(&candidate, best.as_ref()) {
                best = Some(candidate);
            }
        }
    }
    best
}

fn context_candidates(desired_ctx: usize, model_limit: Option<usize>) -> Vec<usize> {
    let start = clamp_context_size(desired_ctx, model_limit);
    let mut out = Vec::new();
    let mut ctx = start;
    while out.len() < 8 && ctx >= CONTEXT_STEP {
        out.push(ctx);
        if ctx == CONTEXT_STEP {
            break;
        }
        ctx = ctx.saturating_sub(CONTEXT_STEP).max(CONTEXT_STEP);
    }
    out
}

fn ceil_candidates(default_ceil_mb: u64, max_ceil: u64) -> Vec<u64> {
    let mut raw = vec![
        default_ceil_mb,
        default_ceil_mb.saturating_mul(75) / 100,
        default_ceil_mb.saturating_mul(50) / 100,
        2000,
        0,
    ];
    raw.sort_unstable_by(|a, b| b.cmp(a));
    raw.dedup();

    let mut out = Vec::new();
    for c in raw {
        let normalized = normalize_ceil(c, max_ceil);
        if !out.contains(&normalized) {
            out.push(normalized);
        }
    }
    // Always consider cache-off when max_ceil is tight.
    if !out.contains(&0) {
        out.push(0);
    }
    out
}

/// Clamp into BigMoe-valid band: `0` or `>= 2000`, and never above `max_ceil`.
fn normalize_ceil(ceil: u64, max_ceil: u64) -> u64 {
    if max_ceil < 2000 {
        return 0;
    }
    if ceil == 0 {
        return 0;
    }
    let capped = ceil.min(max_ceil);
    if capped < 2000 {
        0
    } else {
        capped
    }
}

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

fn is_better(candidate: &MoeStreamKnobPlan, current: Option<&MoeStreamKnobPlan>) -> bool {
    let Some(cur) = current else {
        return true;
    };
    match candidate
        .predicted_tok_s
        .partial_cmp(&cur.predicted_tok_s)
        .unwrap_or(std::cmp::Ordering::Equal)
    {
        std::cmp::Ordering::Greater => true,
        std::cmp::Ordering::Less => false,
        std::cmp::Ordering::Equal => {
            if candidate.context_size != cur.context_size {
                return candidate.context_size > cur.context_size;
            }
            candidate.cache_ceil_mb > cur.cache_ceil_mb
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bench::{unix_now, BenchSample, BenchStore};
    use crate::gguf::GgufTensorInfo;
    use crate::sysinfo::AccelerationBackend;
    use std::collections::HashMap;

    fn streamable_moe_gguf(expert_gb: u64, dense_mb: u64) -> GgufMetadata {
        let expert = GgufTensorInfo {
            name: "blk.0.ffn_gate_exps.weight".into(),
            n_dims: 2,
            dims: [32, 64, 0, 0],
            ggml_type: 2,
            offset: 0,
            nbytes: expert_gb * 1024 * 1024 * 1024,
            layer_index: Some(0),
        };
        let dense = GgufTensorInfo {
            name: "blk.0.attn_q.weight".into(),
            n_dims: 2,
            dims: [32, 64, 0, 0],
            ggml_type: 2,
            offset: 100,
            nbytes: dense_mb * 1024 * 1024,
            layer_index: Some(0),
        };
        GgufMetadata {
            version: 3,
            tensor_count: 2,
            kv_count: 0,
            metadata: HashMap::new(),
            architecture: Some("qwen3moe".into()),
            model_name: Some("big-moe".into()),
            context_length: Some(8192),
            block_count: Some(1),
            head_count: Some(8),
            head_count_kv: Some(8),
            embedding_length: Some(512),
            expert_count: Some(128),
            expert_used_count: Some(8),
            file_size_bytes: expert.nbytes + dense.nbytes,
            tensors: vec![expert, dense],
            quant_label: Some("Q4_0".into()),
        }
    }

    #[test]
    fn normalize_ceil_never_emits_invalid_band() {
        assert_eq!(normalize_ceil(1500, 4000), 0);
        assert_eq!(normalize_ceil(2500, 4000), 2500);
        assert_eq!(normalize_ceil(5000, 3000), 3000);
        assert_eq!(normalize_ceil(2500, 1500), 0);
        assert_eq!(normalize_ceil(0, 4000), 0);
    }

    #[test]
    fn ample_ram_keeps_desired_ctx_and_default_ceil() {
        let gguf = streamable_moe_gguf(12, 800);
        let profile = SystemProfile {
            total_ram_mb: 16_000,
            available_ram_mb: 14_000,
            detected_backend: AccelerationBackend::ArmCpuDotProd,
            recommended_threads: 4,
        };
        let plan = plan_moe_stream_knobs(&gguf, &profile, 75, 4096, 3500, None, "big-moe", "local")
            .expect("plan");
        assert_eq!(plan.context_size, 4096);
        assert_eq!(plan.cache_ceil_mb, 3500);
        assert_eq!(plan.cache_mb, 3500);
        let expected =
            predict_moe_stream_tok_s(&gguf, 3500, AccelerationBackend::ArmCpuDotProd, None);
        assert!(
            (plan.predicted_tok_s - expected).abs() < 0.01,
            "got {} expected {}",
            plan.predicted_tok_s,
            expected
        );
        assert!(!matches!(plan.cache_ceil_mb, 1..=1999));
    }

    #[test]
    fn tight_ram_reduces_ctx_or_ceil() {
        let gguf = streamable_moe_gguf(12, 800);
        // ~2.5 GB LMK budget: resident ~800MB leaves little room for large ctx+cache.
        let profile = SystemProfile {
            total_ram_mb: 4_000,
            available_ram_mb: 3_400,
            detected_backend: AccelerationBackend::ArmCpuDotProd,
            recommended_threads: 4,
        };
        let plan = plan_moe_stream_knobs(&gguf, &profile, 75, 4096, 3500, None, "big-moe", "local")
            .expect("plan");
        assert!(
            plan.context_size < 4096 || plan.cache_ceil_mb < 3500,
            "expected reduced knobs, got ctx={} ceil={}",
            plan.context_size,
            plan.cache_ceil_mb
        );
        assert!(matches!(plan.cache_ceil_mb, 0 | 2000..));
        let footprint = gguf.moe_stream_footprint_bytes(plan.context_size, plan.cache_mb);
        let max_allowed = profile.max_allowed_memory_bytes_pct(75);
        assert!(footprint <= max_allowed);
    }

    #[test]
    fn measured_bench_at_matching_ctx() {
        let gguf = streamable_moe_gguf(12, 800);
        let profile = SystemProfile {
            total_ram_mb: 16_000,
            available_ram_mb: 14_000,
            detected_backend: AccelerationBackend::ArmCpuDotProd,
            recommended_threads: 4,
        };
        let mut store = BenchStore {
            version: 1,
            entries: vec![],
        };
        store.record_with_backend_key(
            "big-moe",
            "local",
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
        let at_2048 = plan_moe_stream_knobs(
            &gguf,
            &profile,
            75,
            2048,
            3500,
            Some(&store),
            "big-moe",
            "local",
        )
        .expect("plan");
        assert_eq!(at_2048.context_size, 2048);
        assert!((at_2048.predicted_tok_s - 3.5).abs() < 0.01);

        // Measured 3.5 @ 2048 beats heuristic @ 4096 under tok/s-first scoring.
        let prefers_measured = plan_moe_stream_knobs(
            &gguf,
            &profile,
            75,
            4096,
            3500,
            Some(&store),
            "big-moe",
            "local",
        )
        .expect("plan");
        assert_eq!(prefers_measured.context_size, 2048);
        assert!((prefers_measured.predicted_tok_s - 3.5).abs() < 0.01);

        // Without a bench hit for this model, active-params heuristic applies.
        let mut store_4096_only = BenchStore {
            version: 1,
            entries: vec![],
        };
        store_4096_only.record_with_backend_key(
            "other-model",
            "local",
            BACKEND_MOE_STREAM,
            4096,
            BenchSample {
                gen_tok_s: 4.0,
                ttft_ms: None,
                prompt_tok_s: None,
                cache_hit_pct: None,
                measured_at: unix_now(),
            },
        );
        let cold = plan_moe_stream_knobs(
            &gguf,
            &profile,
            75,
            4096,
            3500,
            Some(&store_4096_only),
            "big-moe",
            "local",
        )
        .expect("plan");
        assert_eq!(cold.context_size, 4096);
        let expected = predict_moe_stream_tok_s(
            &gguf,
            cold.cache_mb,
            AccelerationBackend::ArmCpuDotProd,
            None,
        );
        assert!((cold.predicted_tok_s - expected).abs() < 0.01);
    }

    #[test]
    fn measured_overrides_active_params_heuristic() {
        let gguf = streamable_moe_gguf(12, 800);
        let predicted =
            predict_moe_stream_tok_s(&gguf, 0, AccelerationBackend::X86Baseline, Some(3.5));
        assert!((predicted - 3.5).abs() < 0.01);
    }

    #[test]
    fn a3b_warm_cache_near_reference_tok_s() {
        let gguf = streamable_moe_gguf(12, 800);
        // working_set ≈ max(500, 12288 * 0.0625 * 4) = max(500, 3072) = 3072
        let predicted =
            predict_moe_stream_tok_s(&gguf, 3500, AccelerationBackend::ArmCpuDotProd, None);
        assert!(
            (predicted - MOE_COLD_START_TOK_S).abs() < 0.2,
            "A3B warm cache should be near {MOE_COLD_START_TOK_S}, got {predicted}"
        );
    }

    #[test]
    fn higher_topk_demotes_tok_s() {
        let mut a3b = streamable_moe_gguf(12, 800);
        a3b.expert_used_count = Some(8);
        let mut a8b = streamable_moe_gguf(12, 800);
        a8b.expert_used_count = Some(16);
        let a3b_tok =
            predict_moe_stream_tok_s(&a3b, 3500, AccelerationBackend::ArmCpuDotProd, None);
        let a8b_tok =
            predict_moe_stream_tok_s(&a8b, 3500, AccelerationBackend::ArmCpuDotProd, None);
        assert!(
            a8b_tok < a3b_tok,
            "higher top-k should demote: a8b={a8b_tok} a3b={a3b_tok}"
        );
    }

    #[test]
    fn cold_cache_slower_than_warm() {
        let gguf = streamable_moe_gguf(12, 800);
        let cold = predict_moe_stream_tok_s(&gguf, 0, AccelerationBackend::ArmCpuDotProd, None);
        let warm = predict_moe_stream_tok_s(&gguf, 3500, AccelerationBackend::ArmCpuDotProd, None);
        assert!(cold < warm, "cold={cold} should be < warm={warm}");
    }

    #[test]
    fn knob_planner_prefers_warmer_ceil_on_tok_s() {
        let gguf = streamable_moe_gguf(12, 800);
        let profile = SystemProfile {
            total_ram_mb: 16_000,
            available_ram_mb: 14_000,
            detected_backend: AccelerationBackend::ArmCpuDotProd,
            recommended_threads: 4,
        };
        let plan = plan_moe_stream_knobs(&gguf, &profile, 75, 4096, 3500, None, "big-moe", "local")
            .expect("plan");
        assert!(
            plan.cache_ceil_mb >= 2000,
            "expected warm ceil winner, got {}",
            plan.cache_ceil_mb
        );
        let cold_tok = predict_moe_stream_tok_s(&gguf, 0, AccelerationBackend::ArmCpuDotProd, None);
        assert!(
            plan.predicted_tok_s > cold_tok,
            "winner tok/s {} should beat cold {}",
            plan.predicted_tok_s,
            cold_tok
        );
    }

    #[test]
    fn warm_hit_governor_shrinks_default_ceil_seed() {
        let gguf = streamable_moe_gguf(12, 800);
        let profile = SystemProfile {
            total_ram_mb: 16_000,
            available_ram_mb: 14_000,
            detected_backend: AccelerationBackend::ArmCpuDotProd,
            recommended_threads: 4,
        };
        let ungoverned =
            plan_moe_stream_knobs(&gguf, &profile, 75, 4096, 3500, None, "big-moe", "local")
                .expect("plan");
        let mut store = BenchStore {
            version: 1,
            entries: vec![],
        };
        // Hit-only samples with tok/s below heuristic so ceil seed (not measured tok/s)
        // drives the comparison: warm governor shrinks 3500→2625.
        store.record_with_backend_key(
            "big-moe",
            "local",
            BACKEND_MOE_STREAM,
            4096,
            BenchSample {
                gen_tok_s: 0.5,
                ttft_ms: None,
                prompt_tok_s: None,
                cache_hit_pct: Some(85.0),
                measured_at: unix_now(),
            },
        );
        let governed = plan_moe_stream_knobs(
            &gguf,
            &profile,
            75,
            4096,
            3500,
            Some(&store),
            "big-moe",
            "local",
        )
        .expect("plan");
        assert!(
            governed.cache_ceil_mb <= ungoverned.cache_ceil_mb,
            "warm governor ceil {} should be ≤ ungoverned {}",
            governed.cache_ceil_mb,
            ungoverned.cache_ceil_mb
        );
    }
}
