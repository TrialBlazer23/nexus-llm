//! Load-time MoE stream budget.
//!
//! One plan picks a context and an expert cache. The cache grows until the
//! model's working set or the LMK room stops it. Bench hit-rate can raise that
//! target, or refuse to shrink a warm cache just to hold a larger context.
//! It never writes back to `config.toml` and never flips lossless to lossy.

use crate::bench::{BenchStore, MoeCacheHitStats};
use crate::config::{MoeCachePreference, MoeConfig};
use crate::gguf::GgufMetadata;
use crate::sysinfo::{AccelerationBackend, SystemProfile};

use super::{clamp_context_size, CONTEXT_STEP};

/// Extra ceiling supplied by a caller (UI selection or peer load request).
///
/// Operator `MoeConfig.cache_ceil_mb` is a separate cap. Neither value is a seed
/// that later code is allowed to shrink by a fixed percent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MoeCacheCap {
    /// No extra cap.
    None,
    /// Cache off.
    Off,
    /// Upper bound in MiB.
    Max(u64),
}

impl MoeCacheCap {
    /// Map a wire or UI `Option<u64>`.
    ///
    /// `None` is no cap. `Some(0)` is cache off. `Some(n)` is an upper bound.
    pub fn from_optional_mb(value: Option<u64>) -> Self {
        match value {
            None => Self::None,
            Some(0) => Self::Off,
            Some(n) => Self::Max(n),
        }
    }
}

/// Where `predicted_tok_s` came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MoeScoreSource {
    Measured,
    Prior,
}

/// Chosen MoE stream load. `moe` is a session copy; the caller's config is unchanged.
#[derive(Debug, Clone, PartialEq)]
pub struct MoeSpawnPlan {
    pub context_size: usize,
    /// `0` or `>= min_cache_mb`.
    pub cache_mb: u64,
    pub predicted_tok_s: f32,
    pub score_source: MoeScoreSource,
    pub moe: MoeConfig,
    pub notes: Vec<String>,
}

/// Predict MoE flash-stream tok/s from a measurement or from active params and cache coverage.
///
/// A positive measurement is returned unchanged, with no upper clamp. The cold-start
/// prior uses `MoeConfig` reference fields and rises with cache until the working set is covered.
pub fn predict_moe_stream_tok_s(
    gguf: &GgufMetadata,
    cache_mb: u64,
    backend: AccelerationBackend,
    measured: Option<f32>,
    moe: &MoeConfig,
) -> f32 {
    if let Some(m) = measured.filter(|v| *v > 0.0) {
        return m;
    }

    let active_frac = active_fraction(gguf, moe);
    let ref_frac =
        moe.reference_active_experts.max(1) as f32 / moe.reference_expert_count.max(1) as f32;
    let compute = moe.reference_tok_s() * (ref_frac / active_frac).sqrt();

    let working_set = prior_working_set_mb(gguf, moe).max(1) as f32;
    let hit = if cache_mb == 0 {
        0.0
    } else {
        ((cache_mb as f32) / working_set).min(1.0)
    };
    let io_pen = 0.55 + 0.45 * hit;
    let backend_nudge = match backend {
        AccelerationBackend::ArmCpuDotProd | AccelerationBackend::Vulkan => 1.0,
        AccelerationBackend::X86Baseline => 0.85,
        AccelerationBackend::GenericCpu => 0.9,
    };
    (compute * io_pen * backend_nudge).max(0.0)
}

/// Choose context and expert cache for one node. `None` when the GGUF cannot stream
/// or no context in the walk fits even with the cache off.
#[allow(clippy::too_many_arguments)]
pub fn plan_moe_spawn(
    gguf: &GgufMetadata,
    profile: &SystemProfile,
    moe: &MoeConfig,
    percent: u8,
    desired_ctx: usize,
    bench: Option<&BenchStore>,
    model_key: &str,
    node_id: &str,
    extra_cap: MoeCacheCap,
) -> Option<MoeSpawnPlan> {
    if !gguf.streamable_moe() {
        return None;
    }

    let contexts = context_candidates(desired_ctx, gguf.context_length);
    let desired = contexts.first().copied().unwrap_or(CONTEXT_STEP);
    let prior = prior_working_set_mb(gguf, moe);
    let room_desired = room_mb(profile, percent, gguf, desired, moe.cache_floor_mb);
    let hit = bench.and_then(|s| s.lookup_moe_cache_hit_exact(model_key, node_id, desired));
    let adjusted = adjust_useful(moe, prior, room_desired, hit);

    let (context_size, cache_mb) = pick_point(
        gguf,
        profile,
        moe,
        percent,
        extra_cap,
        &contexts,
        adjusted.useful,
        adjusted.warm_floor,
    )?;

    let mut session = moe.clone();
    let mut notes = adjusted.notes;
    apply_chronic_overlay(
        moe,
        &mut session,
        &mut notes,
        adjusted.hit,
        cache_mb,
        ceiling_mb(
            room_mb(profile, percent, gguf, context_size, moe.cache_floor_mb),
            moe,
            extra_cap,
        ),
    );

    let measured =
        bench.and_then(|s| s.lookup_moe_gen_tok_s(model_key, node_id, context_size, cache_mb));
    let score_source = if measured.is_some() {
        MoeScoreSource::Measured
    } else {
        MoeScoreSource::Prior
    };
    let predicted_tok_s =
        predict_moe_stream_tok_s(gguf, cache_mb, profile.detected_backend, measured, moe);
    notes.push(format!(
        "moe plan: ctx={context_size} cache={cache_mb} MiB ({})",
        match score_source {
            MoeScoreSource::Measured => "measured",
            MoeScoreSource::Prior => "prior",
        }
    ));

    Some(MoeSpawnPlan {
        context_size,
        cache_mb,
        predicted_tok_s,
        score_source,
        moe: session,
        notes,
    })
}

struct Adjustment {
    useful: u64,
    warm_floor: Option<u64>,
    hit: Option<MoeCacheHitStats>,
    notes: Vec<String>,
}

fn adjust_useful(
    moe: &MoeConfig,
    prior: u64,
    room_desired: u64,
    hit: Option<MoeCacheHitStats>,
) -> Adjustment {
    let mut useful = prior;
    let mut warm_floor = None;
    let mut notes = Vec::new();
    if moe.adapt {
        if let Some(stats) = hit {
            if stats.hit_pct >= f32::from(moe.warm_hit_pct) {
                if let Some(cache) = stats.cache_mb.filter(|c| *c > 0) {
                    useful = prior.min(cache);
                    warm_floor = Some(useful);
                    notes.push(format!(
                        "moe plan: warm hit {:.0}% at {cache} MiB — hold cache at {useful} MiB",
                        stats.hit_pct
                    ));
                }
            } else if stats.hit_pct < f32::from(moe.cold_hit_pct) {
                if let Some(cache) = stats.cache_mb {
                    let covers_prior = cache as f32 >= prior as f32 * 0.9;
                    if covers_prior && room_desired > useful {
                        useful = room_desired;
                        notes.push(format!(
                            "moe plan: cold hit {:.0}% at {cache} MiB — raise cache toward room {room_desired} MiB",
                            stats.hit_pct
                        ));
                    }
                }
            }
        }
    }
    Adjustment {
        useful,
        warm_floor,
        hit,
        notes,
    }
}

fn apply_chronic_overlay(
    moe: &MoeConfig,
    session: &mut MoeConfig,
    notes: &mut Vec<String>,
    hit: Option<MoeCacheHitStats>,
    cache_mb: u64,
    limit_mb: u64,
) {
    if !moe.adapt {
        return;
    }
    let Some(stats) = hit else {
        return;
    };
    let at_limit = cache_mb > 0 && limit_mb > 0 && cache_mb >= limit_mb;
    if stats.hit_pct >= f32::from(moe.chronic_hit_pct)
        || stats.samples_with_hit < usize::from(moe.chronic_min_samples)
        || !at_limit
    {
        return;
    }
    if moe.lossy_allowed() && moe.drop_cold_experts.is_none() {
        session.drop_cold_experts = Some(moe.chronic_drop_cold.clone());
        notes.push(format!(
            "moe plan: chronic hit {:.0}% ({} samples) — session drop_cold_experts={}",
            stats.hit_pct, stats.samples_with_hit, moe.chronic_drop_cold
        ));
    } else if !moe.lossy_allowed() {
        notes.push(format!(
            "moe plan: chronic hit {:.0}% but quality_mode=lossless — cache only",
            stats.hit_pct
        ));
    }
}

#[allow(clippy::too_many_arguments)]
fn pick_point(
    gguf: &GgufMetadata,
    profile: &SystemProfile,
    moe: &MoeConfig,
    percent: u8,
    extra_cap: MoeCacheCap,
    contexts: &[usize],
    useful: u64,
    warm_floor: Option<u64>,
) -> Option<(usize, u64)> {
    if moe.prefer == MoeCachePreference::Cache {
        for &ctx in contexts {
            let room = room_mb(profile, percent, gguf, ctx, moe.cache_floor_mb);
            let limit = ceiling_mb(room, moe, extra_cap);
            if forced_off(moe, extra_cap) {
                break;
            }
            if useful >= effective_min(moe)
                && limit >= useful
                && warm_floor.is_none_or(|floor| useful >= floor)
                && fits(gguf, profile, percent, ctx, useful)
            {
                return Some((ctx, useful));
            }
        }
    }

    for &ctx in contexts {
        let room = room_mb(profile, percent, gguf, ctx, moe.cache_floor_mb);
        let limit = ceiling_mb(room, moe, extra_cap);
        if forced_off(moe, extra_cap) {
            if fits(gguf, profile, percent, ctx, 0) {
                return Some((ctx, 0));
            }
            continue;
        }
        if let Some(floor) = warm_floor {
            if limit < floor {
                continue;
            }
        }
        let target = limit.min(useful);
        if legal_positive(target, moe) && fits(gguf, profile, percent, ctx, target) {
            return Some((ctx, target));
        }
        if warm_floor.is_none() {
            let min_cache = moe.min_cache_mb;
            if min_cache > 0 && limit >= min_cache && fits(gguf, profile, percent, ctx, min_cache) {
                return Some((ctx, min_cache));
            }
            if fits(gguf, profile, percent, ctx, 0) {
                return Some((ctx, 0));
            }
        }
    }
    None
}

fn prior_working_set_mb(gguf: &GgufMetadata, moe: &MoeConfig) -> u64 {
    let expert_bytes = gguf.expert_weight_bytes();
    let expert_mb = if expert_bytes > 0 {
        expert_bytes / (1024 * 1024)
    } else {
        gguf.file_size_bytes.saturating_mul(65) / 100 / (1024 * 1024)
    };
    let active = active_fraction(gguf, moe);
    let estimate =
        (expert_mb as f32 * active * moe.working_set_factor.max(1) as f32).round() as u64;
    estimate.max(effective_min(moe))
}

fn active_fraction(gguf: &GgufMetadata, moe: &MoeConfig) -> f32 {
    let used = gguf
        .expert_used_count
        .unwrap_or(moe.reference_active_experts.max(1) as usize) as f32;
    let total = gguf
        .expert_count
        .unwrap_or(moe.reference_expert_count.max(1) as usize)
        .max(1) as f32;
    (used / total).clamp(1.0 / 256.0, 1.0)
}

fn effective_min(moe: &MoeConfig) -> u64 {
    if moe.min_cache_mb == 0 {
        1
    } else {
        moe.min_cache_mb
    }
}

fn legal_positive(cache: u64, moe: &MoeConfig) -> bool {
    if cache == 0 {
        return false;
    }
    moe.min_cache_mb == 0 || cache >= moe.min_cache_mb
}

fn forced_off(moe: &MoeConfig, extra: MoeCacheCap) -> bool {
    matches!(extra, MoeCacheCap::Off) || operator_off(moe)
}

fn operator_off(moe: &MoeConfig) -> bool {
    moe.cache_ceil_mb > 0 && moe.min_cache_mb > 0 && moe.cache_ceil_mb < moe.min_cache_mb
}

fn ceiling_mb(room: u64, moe: &MoeConfig, extra: MoeCacheCap) -> u64 {
    if forced_off(moe, extra) {
        return 0;
    }
    let mut limit = room;
    if moe.cache_ceil_mb > 0 {
        limit = limit.min(moe.cache_ceil_mb);
    }
    if let MoeCacheCap::Max(n) = extra {
        limit = limit.min(n);
    }
    limit
}

fn room_mb(
    profile: &SystemProfile,
    percent: u8,
    gguf: &GgufMetadata,
    ctx: usize,
    floor_mb: u64,
) -> u64 {
    let lmk = profile.max_allowed_memory_bytes_pct(percent) / (1024 * 1024);
    let resident = gguf.moe_resident_weight_bytes() / (1024 * 1024);
    let kv = gguf.exact_kv_cache_bytes(ctx) / (1024 * 1024);
    lmk.saturating_sub(resident)
        .saturating_sub(kv)
        .saturating_sub(floor_mb)
}

fn fits(
    gguf: &GgufMetadata,
    profile: &SystemProfile,
    percent: u8,
    ctx: usize,
    cache_mb: u64,
) -> bool {
    gguf.moe_stream_footprint_bytes(ctx, cache_mb) <= profile.max_allowed_memory_bytes_pct(percent)
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bench::{unix_now, BenchSample, BenchStore, BACKEND_MOE_STREAM};
    use crate::config::MoeQualityMode;
    use crate::gguf::GgufTensorInfo;
    use std::collections::HashMap;

    fn streamable(expert_gb: u64, dense_mb: u64) -> GgufMetadata {
        let mut gguf = streamable_layers(expert_gb, dense_mb, 1, 512, 8);
        gguf.expert_count = Some(128);
        gguf.expert_used_count = Some(8);
        gguf
    }

    fn streamable_layers(
        expert_gb: u64,
        dense_mb: u64,
        layers: u64,
        embd: u64,
        kv_heads: u64,
    ) -> GgufMetadata {
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
            block_count: Some(layers as usize),
            head_count: Some(kv_heads as usize),
            head_count_kv: Some(kv_heads as usize),
            embedding_length: Some(embd as usize),
            expert_count: Some(128),
            expert_used_count: Some(8),
            file_size_bytes: expert.nbytes + dense.nbytes,
            tensors: vec![expert, dense],
            quant_label: Some("Q4_0".into()),
        }
    }

    fn ample() -> SystemProfile {
        SystemProfile {
            total_ram_mb: 16_000,
            available_ram_mb: 14_000,
            detected_backend: AccelerationBackend::ArmCpuDotProd,
            recommended_threads: 4,
        }
    }

    fn plan_default(gguf: &GgufMetadata, profile: &SystemProfile, ctx: usize) -> MoeSpawnPlan {
        plan_moe_spawn(
            gguf,
            profile,
            &MoeConfig::default(),
            75,
            ctx,
            None,
            "big-moe",
            "local",
            MoeCacheCap::None,
        )
        .expect("plan")
    }

    fn sample(tok_s: f32, hit: f32, cache: u64, ctx: usize) -> BenchStore {
        let mut store = BenchStore {
            version: 1,
            entries: vec![],
        };
        store.record_with_backend_key(
            "big-moe",
            "local",
            BACKEND_MOE_STREAM,
            ctx,
            BenchSample {
                gen_tok_s: tok_s,
                ttft_ms: None,
                prompt_tok_s: None,
                cache_hit_pct: Some(hit),
                cache_mb: Some(cache),
                measured_at: unix_now(),
            },
        );
        store
    }

    #[test]
    fn ample_room_uses_working_set_not_lmk_fraction() {
        let gguf = streamable(12, 800);
        let plan = plan_default(&gguf, &ample(), 4096);
        assert_eq!(plan.context_size, 4096);
        assert_eq!(plan.cache_mb, 3072);
        assert_eq!(plan.score_source, MoeScoreSource::Prior);
        let lmk = ample().max_allowed_memory_bytes_pct(75) / (1024 * 1024);
        assert!(plan.cache_mb < lmk / 2);
        let footprint = gguf.moe_stream_footprint_bytes(plan.context_size, plan.cache_mb);
        assert!(footprint <= ample().max_allowed_memory_bytes_pct(75));
    }

    #[test]
    fn operator_cap_below_working_set_wins() {
        let gguf = streamable(12, 800);
        let moe = MoeConfig {
            cache_ceil_mb: 2500,
            ..MoeConfig::default()
        };
        let plan = plan_moe_spawn(
            &gguf,
            &ample(),
            &moe,
            75,
            4096,
            None,
            "big-moe",
            "local",
            MoeCacheCap::None,
        )
        .expect("plan");
        assert_eq!(plan.cache_mb, 2500);
        assert_eq!(plan.context_size, 4096);
    }

    #[test]
    fn prefer_context_shrinks_cache_before_context() {
        let gguf = streamable_layers(18, 800, 64, 1024, 8);
        let profile = SystemProfile {
            total_ram_mb: 10_000,
            available_ram_mb: 8_000,
            detected_backend: AccelerationBackend::ArmCpuDotProd,
            recommended_threads: 4,
        };
        let moe = MoeConfig {
            cache_floor_mb: 200,
            ..MoeConfig::default()
        };
        let plan = plan_moe_spawn(
            &gguf,
            &profile,
            &moe,
            75,
            4096,
            None,
            "big-moe",
            "local",
            MoeCacheCap::None,
        )
        .expect("plan");
        assert_eq!(plan.context_size, 4096);
        assert!(plan.cache_mb < 4500, "cache {}", plan.cache_mb);
        assert!(plan.cache_mb >= moe.min_cache_mb);
        let footprint = gguf.moe_stream_footprint_bytes(plan.context_size, plan.cache_mb);
        assert!(footprint <= profile.max_allowed_memory_bytes_pct(75));
    }

    #[test]
    fn prefer_cache_steps_context_down_first() {
        let gguf = streamable_layers(18, 800, 64, 1024, 8);
        let profile = SystemProfile {
            total_ram_mb: 10_000,
            available_ram_mb: 8_000,
            detected_backend: AccelerationBackend::ArmCpuDotProd,
            recommended_threads: 4,
        };
        let moe = MoeConfig {
            cache_floor_mb: 200,
            prefer: MoeCachePreference::Cache,
            ..MoeConfig::default()
        };
        let plan = plan_moe_spawn(
            &gguf,
            &profile,
            &moe,
            75,
            4096,
            None,
            "big-moe",
            "local",
            MoeCacheCap::None,
        )
        .expect("plan");
        assert!(plan.context_size < 4096, "ctx {}", plan.context_size);
        assert_eq!(plan.cache_mb, 4608);
    }

    #[test]
    fn warm_sample_refuses_to_shrink_below_measured_cache() {
        let gguf = streamable_layers(18, 800, 64, 1024, 8);
        let profile = SystemProfile {
            total_ram_mb: 9_000,
            available_ram_mb: 7_000,
            detected_backend: AccelerationBackend::ArmCpuDotProd,
            recommended_threads: 4,
        };
        let moe = MoeConfig {
            cache_floor_mb: 200,
            ..MoeConfig::default()
        };
        let store = sample(2.0, 80.0, 3600, 4096);
        let plan = plan_moe_spawn(
            &gguf,
            &profile,
            &moe,
            75,
            4096,
            Some(&store),
            "big-moe",
            "local",
            MoeCacheCap::None,
        )
        .expect("plan");
        assert!(plan.context_size < 4096, "ctx {}", plan.context_size);
        assert_eq!(plan.cache_mb, 3600);
    }

    #[test]
    fn cold_sample_raises_cache_to_the_room() {
        let gguf = streamable(12, 800);
        let store = sample(1.0, 30.0, 3072, 4096);
        let plan = plan_moe_spawn(
            &gguf,
            &ample(),
            &MoeConfig::default(),
            75,
            4096,
            Some(&store),
            "big-moe",
            "local",
            MoeCacheCap::None,
        )
        .expect("plan");
        assert!(plan.cache_mb > 3072, "cache {}", plan.cache_mb);
        let room = room_mb(
            &ample(),
            75,
            &gguf,
            4096,
            MoeConfig::default().cache_floor_mb,
        );
        assert_eq!(plan.cache_mb, room);
    }

    #[test]
    fn chronic_lossy_overlays_drop_without_mutating_input() {
        let gguf = streamable(12, 800);
        let profile = SystemProfile {
            total_ram_mb: 6_000,
            available_ram_mb: 5_416,
            detected_backend: AccelerationBackend::ArmCpuDotProd,
            recommended_threads: 4,
        };
        let moe = MoeConfig {
            quality_mode: MoeQualityMode::Lossy,
            ..MoeConfig::default()
        };
        let mut store = sample(1.0, 20.0, 3072, 4096);
        for _ in 0..2 {
            store.record_with_backend_key(
                "big-moe",
                "local",
                BACKEND_MOE_STREAM,
                4096,
                BenchSample {
                    gen_tok_s: 1.0,
                    ttft_ms: None,
                    prompt_tok_s: None,
                    cache_hit_pct: Some(20.0),
                    cache_mb: Some(3072),
                    measured_at: unix_now(),
                },
            );
        }
        let plan = plan_moe_spawn(
            &gguf,
            &profile,
            &moe,
            100,
            4096,
            Some(&store),
            "big-moe",
            "local",
            MoeCacheCap::None,
        )
        .expect("plan");
        assert!(moe.drop_cold_experts.is_none());
        assert_eq!(
            plan.moe.drop_cold_experts.as_deref(),
            Some(moe.chronic_drop_cold.as_str())
        );
    }

    #[test]
    fn chronic_lossless_does_not_drop() {
        let gguf = streamable(12, 800);
        let profile = SystemProfile {
            total_ram_mb: 6_000,
            available_ram_mb: 5_416,
            detected_backend: AccelerationBackend::ArmCpuDotProd,
            recommended_threads: 4,
        };
        let mut store = sample(1.0, 20.0, 3072, 4096);
        for _ in 0..2 {
            store.record_with_backend_key(
                "big-moe",
                "local",
                BACKEND_MOE_STREAM,
                4096,
                BenchSample {
                    gen_tok_s: 1.0,
                    ttft_ms: None,
                    prompt_tok_s: None,
                    cache_hit_pct: Some(20.0),
                    cache_mb: Some(3072),
                    measured_at: unix_now(),
                },
            );
        }
        let plan = plan_moe_spawn(
            &gguf,
            &profile,
            &MoeConfig::default(),
            100,
            4096,
            Some(&store),
            "big-moe",
            "local",
            MoeCacheCap::None,
        )
        .expect("plan");
        assert!(plan.moe.drop_cold_experts.is_none());
        assert!(plan.notes.iter().any(|n| n.contains("lossless")));
    }

    #[test]
    fn operator_drop_is_left_alone() {
        let gguf = streamable(12, 800);
        let profile = SystemProfile {
            total_ram_mb: 6_000,
            available_ram_mb: 5_416,
            detected_backend: AccelerationBackend::ArmCpuDotProd,
            recommended_threads: 4,
        };
        let moe = MoeConfig {
            quality_mode: MoeQualityMode::Lossy,
            drop_cold_experts: Some("0.50".into()),
            ..MoeConfig::default()
        };
        let mut store = sample(1.0, 10.0, 3072, 4096);
        for _ in 0..2 {
            store.record_with_backend_key(
                "big-moe",
                "local",
                BACKEND_MOE_STREAM,
                4096,
                BenchSample {
                    gen_tok_s: 1.0,
                    ttft_ms: None,
                    prompt_tok_s: None,
                    cache_hit_pct: Some(10.0),
                    cache_mb: Some(3072),
                    measured_at: unix_now(),
                },
            );
        }
        let plan = plan_moe_spawn(
            &gguf,
            &profile,
            &moe,
            100,
            4096,
            Some(&store),
            "big-moe",
            "local",
            MoeCacheCap::None,
        )
        .expect("plan");
        assert_eq!(plan.moe.drop_cold_experts.as_deref(), Some("0.50"));
    }

    #[test]
    fn adapt_off_keeps_the_prior() {
        let gguf = streamable(12, 800);
        let moe = MoeConfig {
            adapt: false,
            ..MoeConfig::default()
        };
        let store = sample(1.0, 30.0, 3072, 4096);
        let plan = plan_moe_spawn(
            &gguf,
            &ample(),
            &moe,
            75,
            4096,
            Some(&store),
            "big-moe",
            "local",
            MoeCacheCap::None,
        )
        .expect("plan");
        assert_eq!(plan.cache_mb, 3072);
        assert!(plan.moe.drop_cold_experts.is_none());
    }

    #[test]
    fn measured_tok_s_is_the_plan_score() {
        let gguf = streamable(12, 800);
        let store = sample(9.5, 50.0, 3072, 4096);
        let plan = plan_moe_spawn(
            &gguf,
            &ample(),
            &MoeConfig::default(),
            75,
            4096,
            Some(&store),
            "big-moe",
            "local",
            MoeCacheCap::None,
        )
        .expect("plan");
        assert_eq!(plan.cache_mb, 3072);
        assert!((plan.predicted_tok_s - 9.5).abs() < 0.01);
        assert_eq!(plan.score_source, MoeScoreSource::Measured);
    }

    #[test]
    fn higher_topk_lowers_prior_and_measurement_is_not_clamped() {
        let mut a3b = streamable(12, 800);
        a3b.expert_used_count = Some(8);
        let mut wider = streamable(12, 800);
        wider.expert_used_count = Some(16);
        let moe = MoeConfig::default();
        let a3b_tok =
            predict_moe_stream_tok_s(&a3b, 3072, AccelerationBackend::ArmCpuDotProd, None, &moe);
        let wider_tok =
            predict_moe_stream_tok_s(&wider, 3072, AccelerationBackend::ArmCpuDotProd, None, &moe);
        assert!(wider_tok < a3b_tok, "wider {wider_tok} a3b {a3b_tok}");
        let measured =
            predict_moe_stream_tok_s(&a3b, 0, AccelerationBackend::X86Baseline, Some(20.0), &moe);
        assert!((measured - 20.0).abs() < 0.01);
    }

    #[test]
    fn off_cap_plans_cache_off_and_none_does_not() {
        let gguf = streamable(12, 800);
        let off = plan_moe_spawn(
            &gguf,
            &ample(),
            &MoeConfig::default(),
            75,
            4096,
            None,
            "big-moe",
            "local",
            MoeCacheCap::Off,
        )
        .expect("off");
        assert_eq!(off.cache_mb, 0);
        let open = plan_moe_spawn(
            &gguf,
            &ample(),
            &MoeConfig::default(),
            75,
            4096,
            None,
            "big-moe",
            "local",
            MoeCacheCap::None,
        )
        .expect("open");
        assert_eq!(open.cache_mb, 3072);
    }

    #[test]
    fn peer_max_is_a_ceiling_not_a_shrink_seed() {
        let gguf = streamable(12, 800);
        let plan = plan_moe_spawn(
            &gguf,
            &ample(),
            &MoeConfig::default(),
            75,
            4096,
            None,
            "big-moe",
            "local",
            MoeCacheCap::Max(2500),
        )
        .expect("plan");
        assert_eq!(plan.cache_mb, 2500);
    }

    #[test]
    fn non_streamable_model_has_no_plan() {
        let mut gguf = streamable(12, 800);
        gguf.architecture = Some("llama".into());
        gguf.expert_count = None;
        gguf.tensors.clear();
        assert!(plan_moe_spawn(
            &gguf,
            &ample(),
            &MoeConfig::default(),
            75,
            4096,
            None,
            "dense",
            "local",
            MoeCacheCap::None,
        )
        .is_none());
    }
}
