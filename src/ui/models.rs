use crate::cluster::{MemoryPlan, MemoryPolicy, Verdict};
use crate::gguf::GgufMetadata;
use crate::sysinfo::SystemProfile;
use std::fs;
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
    pub quant_label: String,
    pub fit_badge: String,
    pub weights_mb: u64,
    pub compute_buffer_mb: u64,
    pub cluster_fit: bool,
}

/// Scan a directory for GGUF model files and inspect their metadata.
pub fn scan_models_dir<P: AsRef<Path>>(dir: P) -> Vec<ModelEntry> {
    scan_models_dir_with_policy(dir, None)
}

/// Scan with an explicit max_ram percent and optional cluster remote capacity (MB).
pub fn scan_models_dir_with_policy<P: AsRef<Path>>(
    dir: P,
    max_ram_percent: Option<u8>,
) -> Vec<ModelEntry> {
    let mut entries = Vec::new();
    let profile = SystemProfile::probe();
    let percent = max_ram_percent.unwrap_or(75);

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
                        let ctx = context_length.min(4096);
                        let policy = MemoryPolicy::from_safety(percent, true, false, ctx);
                        let plan = MemoryPlan::from_gguf(&meta, &profile, &policy, true);
                        let exact_kv_mb = plan.kv_cache_mb;
                        let lmk_compatible = matches!(plan.verdict, Verdict::Fits);
                        let cluster_fit = !matches!(plan.verdict, Verdict::Exceeds);

                        entries.push(ModelEntry {
                            path,
                            filename,
                            size_mb,
                            architecture: meta
                                .architecture
                                .clone()
                                .unwrap_or_else(|| "unknown".to_string()),
                            context_length,
                            exact_kv_mb,
                            lmk_compatible,
                            gguf_version: meta.version,
                            block_count: meta.block_count.unwrap_or(0),
                            head_count: meta.head_count.unwrap_or(0),
                            embedding_length: meta.embedding_length.unwrap_or(0),
                            quant_label: meta
                                .quant_label
                                .clone()
                                .unwrap_or_else(|| "?".to_string()),
                            fit_badge: plan.list_badge().to_string(),
                            weights_mb: plan.weights_mb,
                            compute_buffer_mb: plan.compute_buffer_mb,
                            cluster_fit,
                        });
                    }
                }
            }
        }
    }

    entries.sort_by(|a, b| a.filename.cmp(&b.filename));
    entries
}
