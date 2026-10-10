//! Context and KV fit ladder over pantry metadata already in memory.
//!
//! The requested file is first. Same-family siblings follow, largest
//! `weights_bytes` first. This ladder does not invent an RPC split.

use crate::gguf::{GgufMetadata, GgufValue, KvCacheDtype};
use crate::sysinfo::SystemProfile;

use super::memory::{MemoryPlan, MemoryPolicy, Verdict};

/// Dense fit chosen by [`plan_fit_ladder`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LadderChoice {
    pub context_size: usize,
    pub kv_dtype: KvCacheDtype,
    pub file_index: usize,
    pub interactive: bool,
    pub predicted_tok_s: f32,
}

const QUANT_TOKENS: &[&[u8]] = &[
    b"F16", b"F32", b"Q2", b"Q3", b"Q4", b"Q5", b"Q6", b"Q8", b"IQ",
];

/// Strip one trailing `.gguf` and a final quant component, case-insensitive.
///
/// The component starts at `-`, `_`, or `.` (or at the start of the name) and
/// begins with `Q2`, `Q3`, `Q4`, `Q5`, `Q6`, `Q8`, `F16`, `F32`, or `IQ`.
/// Only ASCII alphanumerics and `_` may follow that token.
pub fn quant_stem(file_name: &str) -> String {
    let name = strip_one_gguf(file_name);
    match trailing_quant_cut(name) {
        Some(cut) => name[..cut].to_string(),
        None => name.to_string(),
    }
}

/// `requested`, then each of 4096, 2048, 1024, 512 that is strictly smaller.
pub fn context_steps(requested: usize) -> Vec<usize> {
    let mut steps = vec![requested];
    for step in [4096, 2048, 1024, 512] {
        if step < requested {
            steps.push(step);
        }
    }
    steps
}

/// First `Fits` or `FitsIfQuantizedKv` across context and KV, then siblings.
///
/// `files[0]` is the requested model. Later entries are other pantry metadata.
/// Passes `cluster_offload_available: false`. `interactive` is
/// `predicted_tok_s >= floor_tok_s`. `predict` runs only for the winning fit.
pub fn plan_fit_ladder(
    files: &[GgufMetadata],
    profile: &SystemProfile,
    policy: &MemoryPolicy,
    floor_tok_s: f32,
    predict: impl Fn(usize, KvCacheDtype, &GgufMetadata) -> f32,
) -> Option<LadderChoice> {
    if files.is_empty() {
        return None;
    }

    let mut order = Vec::with_capacity(files.len());
    order.push(0usize);
    let mut siblings: Vec<usize> = (1..files.len())
        .filter(|&index| is_sibling(&files[0], &files[index]))
        .collect();
    siblings.sort_by(|&left, &right| {
        files[right]
            .weights_bytes()
            .cmp(&files[left].weights_bytes())
            .then(left.cmp(&right))
    });
    order.extend(siblings);

    let budget_mb =
        profile.max_allowed_memory_bytes_pct(policy.max_ram_usage_percent) / (1024 * 1024);

    for index in order {
        let gguf = &files[index];
        for context_size in context_steps(policy.context_size) {
            for kv_dtype in [KvCacheDtype::F16, KvCacheDtype::Q8_0] {
                let trial = MemoryPolicy {
                    context_size,
                    kv_dtype,
                    ..policy.clone()
                };
                let plan = MemoryPlan::from_gguf_budget(
                    gguf,
                    budget_mb,
                    profile.detected_backend,
                    &trial,
                    false,
                );
                let chosen = match plan.verdict {
                    Verdict::Fits => kv_dtype,
                    // F16 plan that fits only after Q8 KV. Stop at this context.
                    Verdict::FitsIfQuantizedKv => KvCacheDtype::Q8_0,
                    _ => continue,
                };
                let predicted_tok_s = predict(context_size, chosen, gguf);
                return Some(LadderChoice {
                    context_size,
                    kv_dtype: chosen,
                    file_index: index,
                    interactive: predicted_tok_s >= floor_tok_s,
                    predicted_tok_s,
                });
            }
        }
    }
    None
}

fn strip_one_gguf(name: &str) -> &str {
    let bytes = name.as_bytes();
    if bytes.len() >= 5 && bytes[bytes.len() - 5..].eq_ignore_ascii_case(b".gguf") {
        &name[..bytes.len() - 5]
    } else {
        name
    }
}

/// Byte index where a trailing quant component begins, including its separator.
fn trailing_quant_cut(name: &str) -> Option<usize> {
    let bytes = name.as_bytes();
    let mut cut = None;
    for (index, _) in bytes.iter().enumerate() {
        let at_boundary = index == 0 || matches!(bytes[index - 1], b'-' | b'_' | b'.');
        if !at_boundary {
            continue;
        }
        for token in QUANT_TOKENS {
            let end = index + token.len();
            if end > bytes.len() || !bytes[index..end].eq_ignore_ascii_case(token) {
                continue;
            }
            let rest_is_quant_tail = bytes[end..]
                .iter()
                .all(|byte| byte.is_ascii_alphanumeric() || *byte == b'_');
            if rest_is_quant_tail {
                cut = Some(if index == 0 { 0 } else { index - 1 });
            }
        }
    }
    cut
}

/// Same architecture, and either both `general.name` values or both quant stems.
///
/// Headers do not store the path. When `model_name` is missing, stash the file
/// name in metadata under `filename` so [`quant_stem`] can see it.
fn is_sibling(requested: &GgufMetadata, candidate: &GgufMetadata) -> bool {
    if requested.architecture != candidate.architecture {
        return false;
    }
    match (&requested.model_name, &candidate.model_name) {
        (Some(left), Some(right)) => left == right,
        _ => {
            let left = quant_stem(&stem_source(requested));
            let right = quant_stem(&stem_source(candidate));
            // An empty stem is "no family name", not a shared name.
            !left.is_empty() && left == right
        }
    }
}

fn stem_source(meta: &GgufMetadata) -> String {
    const KEYS: &[&str] = &[
        "filename",
        "file_name",
        "general.file_name",
        "general.filename",
    ];
    for key in KEYS {
        if let Some(GgufValue::String(value)) = meta.metadata.get(*key) {
            if !value.is_empty() {
                return value.clone();
            }
        }
    }
    meta.model_name.clone().unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cluster::MemoryPolicy;
    use crate::gguf::{GgufMetadata, KvCacheDtype};
    use crate::sysinfo::{AccelerationBackend, SystemProfile};
    use std::collections::HashMap;

    fn geo(file_size_bytes: u64) -> GgufMetadata {
        GgufMetadata {
            version: 3,
            tensor_count: 0,
            kv_count: 0,
            metadata: HashMap::new(),
            architecture: Some("llama".into()),
            model_name: Some("coder".into()),
            context_length: Some(4096),
            block_count: Some(28),
            head_count: Some(12),
            head_count_kv: Some(12),
            embedding_length: Some(1536),
            expert_count: None,
            expert_used_count: None,
            file_size_bytes,
            tensors: vec![],
            quant_label: Some("Q4_K".into()),
        }
    }

    #[test]
    fn quant_stem_strips_quant_token() {
        assert_eq!(quant_stem("coder-7b-Q4_K_M.gguf"), "coder-7b");
        assert_eq!(quant_stem("coder-7b-Q8_0.gguf"), "coder-7b");
        assert_eq!(quant_stem("plain.gguf"), "plain");
    }

    #[test]
    fn context_steps_stay_at_or_below_request() {
        assert_eq!(context_steps(4096), vec![4096, 2048, 1024, 512]);
        assert_eq!(context_steps(2048), vec![2048, 1024, 512]);
        assert_eq!(context_steps(512), vec![512]);
    }

    #[test]
    fn ladder_drops_to_2048_q8_then_stops() {
        // 950 MiB weights. Exact KV at this geometry is ~672 MB at 4096 F16,
        // ~336 MB at 4096 Q8 and at 2048 F16, ~168 MB at 2048 Q8.
        // x86 compute buffer is 256 MB. Budget is 75% of 2000 MB = 1500 MB.
        // 4096 F16 = 1878, 4096 Q8 = 1542, 2048 F16 = 1542, 2048 Q8 = 1374.
        let files = [geo(950 * 1024 * 1024)];
        let profile = SystemProfile {
            total_ram_mb: 4_000,
            available_ram_mb: 2_000,
            detected_backend: AccelerationBackend::X86Baseline,
            recommended_threads: 2,
        };
        let policy = MemoryPolicy {
            max_ram_usage_percent: 75,
            mmap: true,
            mlock: false,
            kv_dtype: KvCacheDtype::F16,
            context_size: 4096,
            batch_size: 512,
        };
        let choice = plan_fit_ladder(&files, &profile, &policy, 3.0, |_ctx, _kv, _gguf| 4.0)
            .expect("2048 q8 fits");
        assert_eq!(choice.context_size, 2048);
        assert_eq!(choice.kv_dtype, KvCacheDtype::Q8_0);
        assert_eq!(choice.file_index, 0);
        assert!(choice.interactive);
    }

    #[test]
    fn slow_fit_is_batch_only() {
        let files = [geo(200 * 1024 * 1024)];
        let profile = SystemProfile {
            total_ram_mb: 8_000,
            available_ram_mb: 6_000,
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
        let choice = plan_fit_ladder(&files, &profile, &policy, 3.0, |_ctx, _kv, _gguf| 1.0)
            .expect("small file fits");
        assert!(!choice.interactive);
        assert_eq!(choice.context_size, 512);
    }

    fn tight_profile() -> SystemProfile {
        SystemProfile {
            total_ram_mb: 4_000,
            available_ram_mb: 2_000,
            detected_backend: AccelerationBackend::X86Baseline,
            recommended_threads: 2,
        }
    }

    fn tight_policy() -> MemoryPolicy {
        MemoryPolicy {
            max_ram_usage_percent: 75,
            mmap: true,
            mlock: false,
            kv_dtype: KvCacheDtype::F16,
            context_size: 4096,
            batch_size: 512,
        }
    }

    #[test]
    fn larger_sibling_wins_before_a_smaller_easier_fit() {
        // 1200 MiB fits only at 512 Q8 (room for KV is 44 MB; 512 Q8 is 42).
        // 800 MiB fits at 4096 Q8, but it is smaller and must not be tried first.
        let mut big = geo(1_200 * 1024 * 1024);
        big.model_name = Some("coder".into());
        let files = [geo(2_000 * 1024 * 1024), geo(800 * 1024 * 1024), big];
        let choice = plan_fit_ladder(&files, &tight_profile(), &tight_policy(), 3.0, |_, _, _| {
            4.0
        })
        .expect("largest sibling fits");
        assert_eq!(choice.file_index, 2);
        assert_eq!(choice.context_size, 512);
        assert_eq!(choice.kv_dtype, KvCacheDtype::Q8_0);
    }

    #[test]
    fn different_family_is_not_a_sibling() {
        // 1100 MiB of another architecture fits at 1024 Q8 and is larger than
        // the real sibling. It must be skipped. The 800 MiB sibling fits at 4096 Q8.
        let mut other_arch = geo(1_100 * 1024 * 1024);
        other_arch.architecture = Some("qwen".into());
        let mut other_name = geo(1_100 * 1024 * 1024);
        other_name.model_name = Some("other".into());
        let files = [
            geo(2_000 * 1024 * 1024),
            geo(800 * 1024 * 1024),
            other_arch,
            other_name,
        ];
        let choice = plan_fit_ladder(&files, &tight_profile(), &tight_policy(), 3.0, |_, _, _| {
            4.0
        })
        .expect("named sibling fits");
        assert_eq!(choice.file_index, 1);
        assert_eq!(choice.context_size, 4096);
        assert_eq!(choice.kv_dtype, KvCacheDtype::Q8_0);
    }

    #[test]
    fn unnamed_files_match_on_quant_stem() {
        let mut requested = geo(2_000 * 1024 * 1024);
        requested.model_name = None;
        requested.metadata.insert(
            "filename".into(),
            crate::gguf::GgufValue::String("coder-7b-Q8_0.gguf".into()),
        );
        let mut decoy = geo(1_100 * 1024 * 1024);
        decoy.model_name = None;
        decoy.metadata.insert(
            "filename".into(),
            crate::gguf::GgufValue::String("other-Q4_K_M.gguf".into()),
        );
        let mut sibling = geo(800 * 1024 * 1024);
        sibling.model_name = None;
        sibling.metadata.insert(
            "filename".into(),
            crate::gguf::GgufValue::String("coder-7b-Q4_K_M.gguf".into()),
        );
        let files = [requested, decoy, sibling];
        let choice = plan_fit_ladder(&files, &tight_profile(), &tight_policy(), 3.0, |_, _, _| {
            4.0
        })
        .expect("quant stem sibling fits");
        assert_eq!(choice.file_index, 2);
        assert_eq!(choice.context_size, 4096);
        assert_eq!(choice.kv_dtype, KvCacheDtype::Q8_0);
    }

    #[test]
    fn over_budget_file_returns_none() {
        let files = [geo(3_000 * 1024 * 1024)];
        assert!(
            plan_fit_ladder(&files, &tight_profile(), &tight_policy(), 3.0, |_, _, _| {
                9.0
            })
            .is_none()
        );
    }
}
