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
            let elems_per_token =
                2.0 * (block_count as f64) * (n_kv as f64) * (head_dim as f64);
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
        let (exact_kv_mb, lmk_compatible) = if m.block_count > 0 && m.head_count > 0 && m.embedding_length > 0 {
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
        });
    }
    entries.sort_by(|a, b| a.filename.cmp(&b.filename));
    entries
}
