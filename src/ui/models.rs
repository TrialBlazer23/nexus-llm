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

fn entries_from_index(index: ModelIndex, only_under: Option<&Path>) -> Vec<ModelEntry> {
    let profile = SystemProfile::probe();
    let only_canon = only_under.and_then(|p| p.canonicalize().ok());
    let mut entries = Vec::new();
    for m in index.models {
        if let Some(ref root) = only_canon {
            let parent = m
                .path
                .parent()
                .and_then(|p| p.canonicalize().ok());
            if parent.as_ref() != Some(root) {
                continue;
            }
        }
        let (exact_kv_mb, lmk_compatible) = match GgufMetadata::open(&m.path) {
            Ok(meta) => {
                let ctx = m.context_length.min(4096);
                let exact_kv_mb = meta.exact_kv_cache_bytes(ctx) / (1024 * 1024);
                let lmk_compatible = profile.can_safely_load_gguf(&meta, ctx);
                (exact_kv_mb, lmk_compatible)
            }
            Err(_) => (0, false),
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
