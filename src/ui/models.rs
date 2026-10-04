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
    pub exact_kv_mb: u64,
    pub lmk_compatible: bool,
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
                        let context_length = meta.context_length.unwrap_or(4096);
                        let exact_kv_bytes = meta.exact_kv_cache_bytes(context_length.min(4096));
                        let exact_kv_mb = exact_kv_bytes / (1024 * 1024);
                        let lmk_compatible = profile.can_safely_load_gguf(&meta, context_length.min(4096));

                        entries.push(ModelEntry {
                            path,
                            filename,
                            size_mb,
                            architecture: meta.architecture.unwrap_or_else(|| "unknown".to_string()),
                            context_length,
                            exact_kv_mb,
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
