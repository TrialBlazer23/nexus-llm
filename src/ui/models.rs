use crate::cluster::DEFAULT_CONTEXT_SIZE;
use crate::gguf::GgufMetadata;
use crate::sysinfo::SystemProfile;
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone)]
pub struct ModelEntry {
    pub path: PathBuf,
    pub filename: String,
    pub size_mb: u64,
    pub architecture: String,
    pub context_length: usize,
    /// Exact KV MB computed at `kv_context` tokens.
    pub exact_kv_mb: u64,
    /// Context size used when computing `exact_kv_mb`.
    pub kv_context: usize,
    pub lmk_compatible: bool,
}

impl ModelEntry {
    /// Required RAM (weights + KV) at the given context size.
    pub fn required_mb_at(&self, context_size: usize) -> u64 {
        let base_ctx = self.kv_context.max(1);
        let kv_mb = ((self.exact_kv_mb as f64) * (context_size as f64) / (base_ctx as f64)).ceil() as u64;
        self.size_mb.saturating_add(kv_mb)
    }
}

/// Scan a directory for GGUF model files and inspect their metadata.
pub fn scan_models_dir<P: AsRef<Path>>(dir: P) -> Vec<ModelEntry> {
    let mut entries = Vec::new();
    let profile = SystemProfile::probe();

    if let Ok(read_dir) = fs::read_dir(dir) {
        for entry in read_dir.flatten() {
            let path = entry.path();
            if path.is_file() {
                let is_gguf = path
                    .extension()
                    .and_then(|ext| ext.to_str())
                    .map(|ext| ext.eq_ignore_ascii_case("gguf"))
                    .unwrap_or(false);

                if is_gguf {
                    let filename = path
                        .file_name()
                        .map(|s| s.to_string_lossy().to_string())
                        .unwrap_or_else(|| "unknown".to_string());

                    if let Ok(meta) = GgufMetadata::open(&path) {
                        let size_mb = meta.file_size_bytes / (1024 * 1024);
                        let context_length = meta.context_length.unwrap_or(DEFAULT_CONTEXT_SIZE);
                        let kv_context = context_length.min(DEFAULT_CONTEXT_SIZE);
                        let exact_kv_bytes = meta.exact_kv_cache_bytes(kv_context);
                        let exact_kv_mb = exact_kv_bytes / (1024 * 1024);
                        let lmk_compatible = profile.can_safely_load_gguf(&meta, kv_context);

                        entries.push(ModelEntry {
                            path,
                            filename,
                            size_mb,
                            architecture: meta.architecture.unwrap_or_else(|| "unknown".to_string()),
                            context_length,
                            exact_kv_mb,
                            kv_context,
                            lmk_compatible,
                        });
                    }
                }
            }
        }
    }

    entries.sort_by(|a, b| a.filename.cmp(&b.filename));
    entries
}
