use crate::gguf::GgufMetadata;
use crate::store::ModelIndex;
use crate::sysinfo::SystemProfile;
use std::path::{Path, PathBuf};

/// Local GGUF catalog row with metadata cached at scan time (no reopen in render).
#[derive(Debug, Clone)]
pub struct ModelEntry {
    pub path: PathBuf,
    pub filename: String,
    pub size_mb: u64,
    pub architecture: String,
    pub context_length: usize,
    pub exact_kv_mb: u64,
    pub lmk_compatible: bool,
    /// Cached GGUF header fields — filled once in [`scan_models_dir`].
    pub gguf_version: u32,
    pub block_count: usize,
    pub head_count: usize,
    pub embedding_length: usize,
    /// Lowercase hex SHA-256 of file bytes (from content-addressed store).
    pub digest: String,
    /// Shard count in this logical model (1 for non-sharded).
    pub shard_count: usize,
    /// Total expected shards if known from shard naming pattern (e.g. 5 for -00001-of-00005).
    pub total_shards: Option<usize>,
}

/// Scan a directory for GGUF model files via the content-addressed index.
pub fn scan_models_dir<P: AsRef<Path>>(dir: P) -> Vec<ModelEntry> {
    let dir = dir.as_ref();
    let index = ModelIndex::reconcile_default(dir).unwrap_or_else(|_| ModelIndex {
        version: 1,
        models: Vec::new(),
    });
    entries_from_index(index, Some(dir))
}

/// Scan using an explicit index path (tests / alternate roots).
pub fn scan_models_dir_with_index(dir: &Path, index_path: &Path) -> Vec<ModelEntry> {
    let index = ModelIndex::reconcile(dir, index_path).unwrap_or_else(|_| ModelIndex {
        version: 1,
        models: Vec::new(),
    });
    entries_from_index(index, Some(dir))
}

fn compute_kv_cache_bytes(
    block_count: usize,
    head_count: usize,
    head_count_kv: usize,
    embedding_length: usize,
    context_size: usize,
) -> u64 {
    if block_count > 0 && head_count > 0 && embedding_length > 0 {
        if let Some(head_dim) = embedding_length.checked_div(head_count) {
            let n_kv = if head_count_kv > 0 {
                head_count_kv
            } else {
                head_count
            };
            let elems_per_token = 2.0 * (block_count as f64) * (n_kv as f64) * (head_dim as f64);
            // 2.0 bytes per element for F16 dtype
            let bytes = elems_per_token * 2.0 * (context_size as f64);
            return bytes.round() as u64;
        }
    }
    (context_size as u64).saturating_mul(200 * 1024)
}

fn entries_from_index(index: ModelIndex, only_under: Option<&Path>) -> Vec<ModelEntry> {
    let profile = SystemProfile::probe();
    let only_canon = only_under.and_then(|p| p.canonicalize().ok());
    let mut entries = Vec::new();
    for m in index.models {
        if let Some(ref root) = only_canon {
            let parent = m.path.parent().and_then(|p| p.canonicalize().ok());
            if parent.as_ref() != Some(root) {
                continue;
            }
        }
        let ctx = m.context_length.min(4096);
        let (exact_kv_mb, lmk_compatible) =
            if m.block_count > 0 && m.head_count > 0 && m.embedding_length > 0 {
                let kv_bytes = compute_kv_cache_bytes(
                    m.block_count,
                    m.head_count,
                    m.head_count_kv,
                    m.embedding_length,
                    ctx,
                );
                let kv_mb = kv_bytes / (1024 * 1024);
                let total_required = m.size_bytes.saturating_add(kv_bytes);
                let compatible = total_required <= profile.max_allowed_memory_bytes();
                (kv_mb, compatible)
            } else {
                // Fallback for models without cached geometry in the index
                match GgufMetadata::open(&m.path) {
                    Ok(meta) => {
                        let exact_kv_mb = meta.exact_kv_cache_bytes(ctx) / (1024 * 1024);
                        let lmk_compatible = profile.can_safely_load_gguf(&meta, ctx);
                        (exact_kv_mb, lmk_compatible)
                    }
                    Err(_) => (0, false),
                }
            };
        entries.push(ModelEntry {
            path: m.path,
            filename: m.filename,
            size_mb: m.size_bytes / (1024 * 1024),
            architecture: m.architecture,
            context_length: m.context_length,
            exact_kv_mb,
            lmk_compatible,
            gguf_version: m.gguf_version,
            block_count: m.block_count,
            head_count: m.head_count,
            embedding_length: m.embedding_length,
            digest: m.digest,
            shard_count: 1,
            total_shards: None,
        });
    }
    aggregate_model_entries(entries)
}

/// Parse shard information from a GGUF filename.
/// Returns `(prefix, shard_index, total_shards)`.
/// E.g. `"qwen2.5-coder-32b-00002-of-00005.gguf"` -> `Some(("qwen2.5-coder-32b", 2, 5))`.
pub fn parse_shard_info(filename: &str) -> Option<(String, usize, usize)> {
    let lower = filename.to_ascii_lowercase();
    if !lower.ends_with(".gguf") {
        return None;
    }
    let without_ext = &filename[..filename.len() - 5];
    if let Some(of_idx) = without_ext.rfind("-of-") {
        let after_of = &without_ext[of_idx + 4..];
        if let Ok(total) = after_of.parse::<usize>() {
            let before_of = &without_ext[..of_idx];
            if let Some(dash_idx) = before_of.rfind('-') {
                let shard_num_str = &before_of[dash_idx + 1..];
                if let Ok(shard_idx) = shard_num_str.parse::<usize>() {
                    if total > 0 && shard_idx > 0 && shard_idx <= total {
                        let prefix = before_of[..dash_idx].to_string();
                        return Some((prefix, shard_idx, total));
                    }
                }
            }
        }
    }
    None
}

/// Parse the prefix from a sharded GGUF filename (e.g. `qwen-00001-of-00003.gguf` -> `Some("qwen")`).
pub fn parse_shard_prefix(filename: &str) -> Option<String> {
    parse_shard_info(filename).map(|(prefix, _, _)| prefix)
}

/// Detect whether a model file is part of a multi-shard split (e.g. `foo-00001-of-00003.gguf`).
/// Returns all sibling shard paths sorted, or `vec![path.to_path_buf()]` if not sharded.
pub fn detect_model_shards(path: &Path) -> Vec<PathBuf> {
    let filename = match path.file_name().and_then(|s| s.to_str()) {
        Some(f) => f,
        None => return vec![path.to_path_buf()],
    };

    if let Some(prefix) = parse_shard_prefix(filename) {
        if let Some(parent) = path.parent() {
            if let Ok(entries) = std::fs::read_dir(parent) {
                let needle = format!("{}-", prefix);
                let mut shards: Vec<PathBuf> = entries
                    .flatten()
                    .filter_map(|e| {
                        let name = e.file_name().to_string_lossy().to_string();
                        if name.starts_with(&needle)
                            && name.to_ascii_lowercase().ends_with(".gguf")
                            && parse_shard_info(&name)
                                .map(|(p, _, _)| p == prefix)
                                .unwrap_or(false)
                        {
                            Some(e.path())
                        } else {
                            None
                        }
                    })
                    .collect();
                shards.sort();
                if !shards.is_empty() {
                    return shards;
                }
            }
        }
    }

    vec![path.to_path_buf()]
}

/// Group sibling model shards into single logical catalog rows.
pub fn aggregate_model_entries(entries: Vec<ModelEntry>) -> Vec<ModelEntry> {
    let profile = SystemProfile::probe();
    let mut sharded_groups: std::collections::HashMap<(PathBuf, String), Vec<ModelEntry>> =
        std::collections::HashMap::new();
    let mut non_sharded = Vec::new();

    for entry in entries {
        if let Some((prefix, _shard_idx, _total)) = parse_shard_info(&entry.filename) {
            let parent = entry
                .path
                .parent()
                .map(|p| p.to_path_buf())
                .unwrap_or_default();
            sharded_groups
                .entry((parent, prefix))
                .or_default()
                .push(entry);
        } else {
            non_sharded.push(entry);
        }
    }

    let mut result = non_sharded;

    for ((_parent, prefix), mut shards) in sharded_groups {
        // Sort shards so primary shard (00001-of-*) comes first
        shards.sort_by(|a, b| {
            let idx_a = parse_shard_info(&a.filename)
                .map(|(_, s, _)| s)
                .unwrap_or(usize::MAX);
            let idx_b = parse_shard_info(&b.filename)
                .map(|(_, s, _)| s)
                .unwrap_or(usize::MAX);
            idx_a.cmp(&idx_b).then_with(|| a.filename.cmp(&b.filename))
        });

        let total_expected = parse_shard_info(&shards[0].filename).map(|(_, _, t)| t);
        let count = shards.len();
        let total_size_mb: u64 = shards.iter().map(|s| s.size_mb).sum();

        // Primary representative entry is the earliest available shard
        let primary = &shards[0];
        let display_name = match total_expected {
            Some(tot) if tot == count => format!("{prefix} [{count} shards]"),
            Some(tot) => format!("{prefix} [{count}/{tot} shards]"),
            None => format!("{prefix} [{count} shards]"),
        };

        let kv_mb = primary.exact_kv_mb;
        let total_req_bytes = total_size_mb
            .saturating_add(kv_mb)
            .saturating_mul(1024 * 1024);
        let lmk_compatible = total_req_bytes <= profile.max_allowed_memory_bytes();

        result.push(ModelEntry {
            path: primary.path.clone(),
            filename: display_name,
            size_mb: total_size_mb,
            architecture: primary.architecture.clone(),
            context_length: primary.context_length,
            exact_kv_mb: kv_mb,
            lmk_compatible,
            gguf_version: primary.gguf_version,
            block_count: primary.block_count,
            head_count: primary.head_count,
            embedding_length: primary.embedding_length,
            digest: primary.digest.clone(),
            shard_count: count,
            total_shards: total_expected,
        });
    }

    result.sort_by(|a, b| a.filename.cmp(&b.filename));
    result
}

/// Sum the total bytes of all shard files on disk.
pub fn calculate_shards_total_bytes(paths: &[PathBuf]) -> u64 {
    paths
        .iter()
        .filter_map(|p| std::fs::metadata(p).ok().map(|m| m.len()))
        .sum()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn test_parse_shard_prefix() {
        assert_eq!(
            parse_shard_prefix("Llama-3-70B-00001-of-00004.gguf"),
            Some("Llama-3-70B".to_string())
        );
        assert_eq!(
            parse_shard_prefix("qwen2.5-coder-32b-00002-of-00005.gguf"),
            Some("qwen2.5-coder-32b".to_string())
        );
        assert_eq!(parse_shard_prefix("regular-model.gguf"), None);
        assert_eq!(parse_shard_prefix("not-a-gguf-00001-of-00002.bin"), None);
    }

    #[test]
    fn test_detect_model_shards_groups_siblings() {
        let dir = tempdir().unwrap();
        let s1 = dir.path().join("model-00001-of-00003.gguf");
        let s2 = dir.path().join("model-00002-of-00003.gguf");
        let s3 = dir.path().join("model-00003-of-00003.gguf");
        let unrelated = dir.path().join("other-model.gguf");

        std::fs::write(&s1, b"shard1").unwrap();
        std::fs::write(&s2, b"shard22").unwrap();
        std::fs::write(&s3, b"shard333").unwrap();
        std::fs::write(&unrelated, b"other").unwrap();

        let detected = detect_model_shards(&s2);
        assert_eq!(detected.len(), 3);
        assert_eq!(detected[0], s1);
        assert_eq!(detected[1], s2);
        assert_eq!(detected[2], s3);

        let total_bytes = calculate_shards_total_bytes(&detected);
        assert_eq!(total_bytes, 6 + 7 + 8);

        // Non-sharded model returns itself only
        let detected_unrelated = detect_model_shards(&unrelated);
        assert_eq!(detected_unrelated, vec![unrelated.clone()]);
    }

    #[test]
    fn test_parse_shard_info() {
        assert_eq!(
            parse_shard_info("DeepSeek-R1-Q8_0-00001-of-00003.gguf"),
            Some(("DeepSeek-R1-Q8_0".to_string(), 1, 3))
        );
        assert_eq!(
            parse_shard_info("DeepSeek-R1-Q8_0-00002-of-00003.gguf"),
            Some(("DeepSeek-R1-Q8_0".to_string(), 2, 3))
        );
        assert_eq!(parse_shard_info("single-file.gguf"), None);
    }

    #[test]
    fn test_aggregate_model_entries() {
        let e1 = ModelEntry {
            path: PathBuf::from("/models/qwen-00001-of-00002.gguf"),
            filename: "qwen-00001-of-00002.gguf".to_string(),
            size_mb: 2048,
            architecture: "qwen2".to_string(),
            context_length: 4096,
            exact_kv_mb: 256,
            lmk_compatible: true,
            gguf_version: 3,
            block_count: 28,
            head_count: 16,
            embedding_length: 2048,
            digest: "digest1".to_string(),
            shard_count: 1,
            total_shards: None,
        };
        let e2 = ModelEntry {
            path: PathBuf::from("/models/qwen-00002-of-00002.gguf"),
            filename: "qwen-00002-of-00002.gguf".to_string(),
            size_mb: 2048,
            architecture: "qwen2".to_string(),
            context_length: 4096,
            exact_kv_mb: 256,
            lmk_compatible: true,
            gguf_version: 3,
            block_count: 28,
            head_count: 16,
            embedding_length: 2048,
            digest: "digest2".to_string(),
            shard_count: 1,
            total_shards: None,
        };
        let single = ModelEntry {
            path: PathBuf::from("/models/tiny.gguf"),
            filename: "tiny.gguf".to_string(),
            size_mb: 500,
            architecture: "llama".to_string(),
            context_length: 2048,
            exact_kv_mb: 100,
            lmk_compatible: true,
            gguf_version: 3,
            block_count: 12,
            head_count: 8,
            embedding_length: 1024,
            digest: "digest_tiny".to_string(),
            shard_count: 1,
            total_shards: None,
        };

        let aggregated = aggregate_model_entries(vec![e1, e2, single]);
        assert_eq!(aggregated.len(), 2);

        let qwen = aggregated
            .iter()
            .find(|m| m.filename.contains("qwen"))
            .unwrap();
        assert_eq!(qwen.filename, "qwen [2 shards]");
        assert_eq!(qwen.size_mb, 4096);
        assert_eq!(qwen.shard_count, 2);
        assert_eq!(qwen.total_shards, Some(2));
        assert_eq!(qwen.path, PathBuf::from("/models/qwen-00001-of-00002.gguf"));

        let tiny = aggregated
            .iter()
            .find(|m| m.filename.contains("tiny"))
            .unwrap();
        assert_eq!(tiny.filename, "tiny.gguf");
        assert_eq!(tiny.size_mb, 500);
        assert_eq!(tiny.shard_count, 1);
        assert_eq!(tiny.total_shards, None);
    }
}
