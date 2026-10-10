//! Hit%-driven MoE expert-cache governor (Phase 16.5 #5).
//!
//! Adapts the default cache ceiling from rolling `cache_hit_pct` and, when
//! chronically cold under an already-Lossy quality mode, overlays a mild
//! session-only `drop_cold_experts`. Never flips Lossless → Lossy and never
//! persists advice into config.toml.

use crate::bench::{BenchStore, MoeCacheHitStats};
use crate::config::MoeConfig;

/// Warm caches can shrink the default ceil (RAM savings).
pub const HIT_WARM_PCT: f32 = 70.0;
/// Below this, bump the default ceil toward LMK headroom.
pub const HIT_COLD_PCT: f32 = 40.0;
/// Chronically cold: eligible for session lossy overlay when quality is Lossy.
pub const HIT_CHRONIC_PCT: f32 = 25.0;
/// Minimum hit samples before chronic lossy overlay applies.
pub const CHRONIC_MIN_SAMPLES: usize = 3;
/// Mild BigMoe `--drop-cold-experts` factor used for chronic cold sessions.
pub const CHRONIC_DROP_COLD: &str = "0.85";

/// Load/plan advice from the hit-rate governor.
#[derive(Debug, Clone, PartialEq)]
pub struct MoeGovernorAdvice {
    pub default_ceil_mb: u64,
    /// Session overlay only; `None` means leave `MoeConfig.drop_cold_experts` unchanged.
    pub drop_cold_experts: Option<String>,
    pub notes: Vec<String>,
}

/// Adjust default ceil (and optional lossy overlay) from hit-rate stats.
pub fn govern_moe_stream(
    moe: &MoeConfig,
    default_ceil_mb: u64,
    lmk_budget_mb: u64,
    hit: Option<MoeCacheHitStats>,
) -> MoeGovernorAdvice {
    let headroom = ((lmk_budget_mb.saturating_mul(45)) / 100).max(default_ceil_mb);
    let mut notes = Vec::new();
    let mut ceil = default_ceil_mb;
    let mut drop_cold: Option<String> = None;

    let Some(stats) = hit else {
        return MoeGovernorAdvice {
            default_ceil_mb: normalize_moe_ceil(ceil, headroom),
            drop_cold_experts: None,
            notes,
        };
    };

    if stats.samples_with_hit >= 1 {
        if stats.hit_pct >= HIT_WARM_PCT {
            let shrunk = default_ceil_mb.saturating_mul(75) / 100;
            ceil = normalize_moe_ceil(shrunk, headroom);
            notes.push(format!(
                "moe governor: warm hit {:.0}% — shrink default ceil {}→{}",
                stats.hit_pct, default_ceil_mb, ceil
            ));
        } else if stats.hit_pct < HIT_COLD_PCT {
            let bumped = (default_ceil_mb.saturating_mul(125) / 100).max(default_ceil_mb);
            let target = bumped.min(headroom);
            ceil = normalize_moe_ceil(target, headroom);
            notes.push(format!(
                "moe governor: cold hit {:.0}% — bump default ceil {}→{}",
                stats.hit_pct, default_ceil_mb, ceil
            ));
        }
    }

    if stats.hit_pct < HIT_CHRONIC_PCT && stats.samples_with_hit >= CHRONIC_MIN_SAMPLES {
        if moe.lossy_allowed() && moe.drop_cold_experts.is_none() {
            drop_cold = Some(CHRONIC_DROP_COLD.to_string());
            notes.push(format!(
                "moe governor: chronic hit {:.0}% ({} samples) — session drop_cold_experts={}",
                stats.hit_pct, stats.samples_with_hit, CHRONIC_DROP_COLD
            ));
        } else if !moe.lossy_allowed() {
            notes.push(format!(
                "moe governor: chronic hit {:.0}% but quality_mode=lossless — ceil only",
                stats.hit_pct
            ));
        }
    }

    MoeGovernorAdvice {
        default_ceil_mb: normalize_moe_ceil(ceil, headroom),
        drop_cold_experts: drop_cold,
        notes,
    }
}

/// Apply session lossy overlay (and log-friendly notes) using bench hit stats.
///
/// Ceil bias is applied inside [`super::plan_moe_stream_knobs`]; this mutates
/// `MoeConfig` for spawn only.
pub fn apply_moe_governor_to_config(
    moe: &mut MoeConfig,
    bench: Option<&BenchStore>,
    model_key: &str,
    node_id: &str,
    context_size: usize,
    lmk_budget_mb: u64,
) -> Vec<String> {
    let hit = bench.and_then(|s| s.lookup_moe_cache_hit(model_key, node_id, context_size));
    let seed = if moe.cache_ceil_mb > 0 {
        moe.cache_ceil_mb
    } else {
        // Fallback seed when no explicit ceil; match derive ~45% of budget.
        ((lmk_budget_mb.saturating_mul(45)) / 100)
            .max(2000)
            .min(lmk_budget_mb)
    };
    let advice = govern_moe_stream(moe, seed, lmk_budget_mb, hit);
    if let Some(drop) = advice.drop_cold_experts {
        moe.drop_cold_experts = Some(drop);
    }
    advice.notes
}

/// BigMoe-valid band: `0` or `>= 2000`, never above `max_ceil`.
fn normalize_moe_ceil(ceil: u64, max_ceil: u64) -> u64 {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::MoeQualityMode;

    fn moe_lossless() -> MoeConfig {
        MoeConfig::default()
    }

    fn moe_lossy() -> MoeConfig {
        MoeConfig {
            quality_mode: MoeQualityMode::Lossy,
            ..MoeConfig::default()
        }
    }

    #[test]
    fn no_hit_stats_leaves_ceil_and_drop() {
        let moe = moe_lossless();
        let advice = govern_moe_stream(&moe, 3500, 10_000, None);
        assert_eq!(advice.default_ceil_mb, 3500);
        assert!(advice.drop_cold_experts.is_none());
    }

    #[test]
    fn warm_hit_shrinks_ceil() {
        let moe = moe_lossless();
        let advice = govern_moe_stream(
            &moe,
            3500,
            10_000,
            Some(MoeCacheHitStats {
                hit_pct: 80.0,
                samples_with_hit: 2,
            }),
        );
        assert_eq!(advice.default_ceil_mb, 2625);
        assert!(advice.drop_cold_experts.is_none());
    }

    #[test]
    fn cold_hit_bumps_ceil() {
        let moe = moe_lossless();
        let advice = govern_moe_stream(
            &moe,
            2000,
            10_000,
            Some(MoeCacheHitStats {
                hit_pct: 30.0,
                samples_with_hit: 1,
            }),
        );
        assert!(
            advice.default_ceil_mb > 2000,
            "expected bump, got {}",
            advice.default_ceil_mb
        );
        assert!(advice.drop_cold_experts.is_none());
    }

    #[test]
    fn chronic_lossless_bumps_ceil_no_drop() {
        let moe = moe_lossless();
        let advice = govern_moe_stream(
            &moe,
            2000,
            10_000,
            Some(MoeCacheHitStats {
                hit_pct: 20.0,
                samples_with_hit: 3,
            }),
        );
        assert!(advice.default_ceil_mb > 2000);
        assert!(advice.drop_cold_experts.is_none());
        assert!(advice.notes.iter().any(|n| n.contains("lossless")));
    }

    #[test]
    fn chronic_lossy_injects_drop_cold() {
        let moe = moe_lossy();
        let advice = govern_moe_stream(
            &moe,
            2000,
            10_000,
            Some(MoeCacheHitStats {
                hit_pct: 20.0,
                samples_with_hit: 3,
            }),
        );
        assert_eq!(advice.drop_cold_experts.as_deref(), Some(CHRONIC_DROP_COLD));
    }

    #[test]
    fn chronic_lossy_preserves_operator_drop() {
        let mut moe = moe_lossy();
        moe.drop_cold_experts = Some("0.75".into());
        let advice = govern_moe_stream(
            &moe,
            2000,
            10_000,
            Some(MoeCacheHitStats {
                hit_pct: 20.0,
                samples_with_hit: 3,
            }),
        );
        assert!(advice.drop_cold_experts.is_none());
    }

    #[test]
    fn apply_sets_drop_on_config() {
        let mut moe = moe_lossy();
        let mut store = BenchStore {
            version: 1,
            entries: vec![],
        };
        for _ in 0..3 {
            store.record_with_backend_key(
                "big-moe",
                "local",
                crate::bench::BACKEND_MOE_STREAM,
                2048,
                crate::bench::BenchSample {
                    gen_tok_s: 2.0,
                    ttft_ms: None,
                    prompt_tok_s: None,
                    cache_hit_pct: Some(15.0),
                    measured_at: crate::bench::unix_now(),
                },
            );
        }
        let notes =
            apply_moe_governor_to_config(&mut moe, Some(&store), "big-moe", "local", 2048, 10_000);
        assert_eq!(moe.drop_cold_experts.as_deref(), Some(CHRONIC_DROP_COLD));
        assert!(!notes.is_empty());
    }
}
