//! Content-addressed local model index (`~/.nexus/models.json`).
//!
//! Digests are lowercase hex SHA-256 of full file bytes. The index caches path,
//! size, mtime, and GGUF metadata so rescans avoid re-hashing unchanged files.

use crate::downloader::ModelDownloader;
use crate::gguf::GgufMetadata;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::SystemTime;
use thiserror::Error;
use tracing::{debug, warn};

const INDEX_FILE_NAME: &str = "models.json";
static INDEX_LOCK: Mutex<()> = Mutex::new(());

#[derive(Debug, Error)]
pub enum StoreError {
    #[error("I/O error in model store: {0}")]
    Io(#[from] std::io::Error),
    #[error("failed to serialize model index: {0}")]
    Serialize(#[from] serde_json::Error),
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ModelIndexEntry {
    pub digest: String,
    pub path: PathBuf,
    pub size_bytes: u64,
    /// Unix seconds since epoch (mtime).
    pub mtime: u64,
    pub filename: String,
    pub architecture: String,
    pub context_length: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub quantization: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_url: Option<String>,
    pub gguf_version: u32,
    pub block_count: usize,
    pub head_count: usize,
    pub embedding_length: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct ModelIndex {
    pub version: u32,
    pub models: Vec<ModelIndexEntry>,
}

impl ModelIndex {
    pub fn default_path() -> PathBuf {
        if let Ok(p) = std::env::var("NEXUS_MODELS_INDEX") {
            return PathBuf::from(p);
        }
        let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
        PathBuf::from(home).join(".nexus").join(INDEX_FILE_NAME)
    }

    pub fn load(path: &Path) -> Result<Self, StoreError> {
        if !path.exists() {
            return Ok(Self {
                version: 1,
                models: Vec::new(),
            });
        }
        let bytes = fs::read(path)?;
        let index: Self = serde_json::from_slice(&bytes)?;
        Ok(index)
    }

    pub fn save(&self, path: &Path) -> Result<(), StoreError> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let bytes = serde_json::to_vec_pretty(self)?;
        let tmp = path.with_extension("json.tmp");
        fs::write(&tmp, bytes)?;
        fs::rename(&tmp, path)?;
        Ok(())
    }

    pub fn find_by_digest(&self, digest: &str) -> Option<&ModelIndexEntry> {
        let needle = digest.trim().to_lowercase();
        self.models.iter().find(|m| m.digest == needle)
    }

    pub fn path_for_digest(&self, digest: &str) -> Option<&Path> {
        self.find_by_digest(digest).map(|e| e.path.as_path())
    }

    /// Reconcile on-disk `.gguf` files under `models_dir` with the index at `index_path`.
    ///
    /// Reuses cached digests when `(path, size_bytes, mtime)` match; otherwise hashes
    /// and reparses GGUF metadata. Drops entries whose files are gone **within
    /// `models_dir`**; entries for other directories are preserved.
    pub fn reconcile(models_dir: &Path, index_path: &Path) -> Result<Self, StoreError> {
        let _guard = INDEX_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let previous = Self::load(index_path)?;
        let models_dir = models_dir
            .canonicalize()
            .unwrap_or_else(|_| models_dir.to_path_buf());

        let mut preserved: Vec<ModelIndexEntry> = Vec::new();
        let mut by_path: HashMap<PathBuf, ModelIndexEntry> = HashMap::new();
        for e in previous.models {
            let parent_ok = e
                .path
                .parent()
                .and_then(|p| p.canonicalize().ok())
                .map(|p| p == models_dir)
                .unwrap_or(false);
            if parent_ok {
                by_path.insert(e.path.clone(), e);
            } else {
                preserved.push(e);
            }
        }

        let mut next = Self {
            version: 1,
            models: preserved,
        };

        if models_dir.is_dir() {
            for entry in fs::read_dir(&models_dir)?.flatten() {
                let path = entry.path();
                if !path.is_file() {
                    continue;
                }
                let is_gguf = path
                    .extension()
                    .and_then(|ext| ext.to_str())
                    .map(|ext| ext.eq_ignore_ascii_case("gguf"))
                    .unwrap_or(false);
                if !is_gguf {
                    continue;
                }

                let meta = match fs::metadata(&path) {
                    Ok(m) => m,
                    Err(e) => {
                        warn!("Skipping unreadable model {:?}: {}", path, e);
                        continue;
                    }
                };
                let size_bytes = meta.len();
                let mtime = meta
                    .modified()
                    .ok()
                    .and_then(|t| t.duration_since(SystemTime::UNIX_EPOCH).ok())
                    .map(|d| d.as_secs())
                    .unwrap_or(0);

                if let Some(cached) = by_path.remove(&path) {
                    if cached.size_bytes == size_bytes
                        && cached.mtime == mtime
                        && !cached.digest.is_empty()
                    {
                        next.models.push(cached);
                        continue;
                    }
                }

                match build_entry(&path, size_bytes, mtime) {
                    Ok(entry) => next.models.push(entry),
                    Err(e) => {
                        debug!("Skipping non-indexable {:?}: {}", path, e);
                    }
                }
            }
        }

        next.models
            .sort_by(|a, b| a.filename.to_lowercase().cmp(&b.filename.to_lowercase()));
        if let Err(e) = next.save(index_path) {
            warn!("Failed to persist model index {:?}: {}", index_path, e);
        }
        Ok(next)
    }

    /// Convenience: reconcile using `NEXUS_MODELS_INDEX` or `~/.nexus/models.json`.
    pub fn reconcile_default(models_dir: &Path) -> Result<Self, StoreError> {
        let path = std::env::var("NEXUS_MODELS_INDEX")
            .map(PathBuf::from)
            .unwrap_or_else(|_| Self::default_path());
        Self::reconcile(models_dir, &path)
    }
}

fn build_entry(path: &Path, size_bytes: u64, mtime: u64) -> Result<ModelIndexEntry, String> {
    let gguf = GgufMetadata::open(path).map_err(|e| e.to_string())?;
    let digest = ModelDownloader::calculate_sha256(path).map_err(|e| e.to_string())?;
    let filename = path
        .file_name()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| "unknown".to_string());
    Ok(ModelIndexEntry {
        digest,
        path: path.to_path_buf(),
        size_bytes,
        mtime,
        filename: filename.clone(),
        architecture: gguf
            .architecture
            .clone()
            .unwrap_or_else(|| "unknown".to_string()),
        context_length: gguf.context_length.unwrap_or(4096),
        model_name: gguf.model_name.clone(),
        quantization: guess_quantization(&filename),
        source_url: None,
        gguf_version: gguf.version,
        block_count: gguf.block_count.unwrap_or(0),
        head_count: gguf.head_count.unwrap_or(0),
        embedding_length: gguf.embedding_length.unwrap_or(0),
    })
}

fn guess_quantization(filename: &str) -> Option<String> {
    let upper = filename.to_uppercase();
    for tag in [
        "Q2_K", "Q3_K_S", "Q3_K_M", "Q3_K_L", "Q4_0", "Q4_1", "Q4_K_S", "Q4_K_M", "Q5_0", "Q5_1",
        "Q5_K_S", "Q5_K_M", "Q6_K", "Q8_0", "F16", "F32", "IQ2_XXS", "IQ3_XXS", "IQ4_XS",
    ] {
        if upper.contains(tag) {
            return Some(tag.to_string());
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn write_string(buf: &mut Vec<u8>, s: &str) {
        buf.extend_from_slice(&(s.len() as u64).to_le_bytes());
        buf.extend_from_slice(s.as_bytes());
    }

    fn write_kv_string(buf: &mut Vec<u8>, key: &str, value: &str) {
        write_string(buf, key);
        buf.extend_from_slice(&8u32.to_le_bytes()); // STRING
        write_string(buf, value);
    }

    fn build_gguf(arch: &str, name: &str) -> Vec<u8> {
        let mut buf = Vec::new();
        buf.extend_from_slice(&crate::gguf::GGUF_MAGIC.to_le_bytes());
        buf.extend_from_slice(&3u32.to_le_bytes());
        buf.extend_from_slice(&0u64.to_le_bytes()); // tensors
        buf.extend_from_slice(&3u64.to_le_bytes()); // kv
        write_kv_string(&mut buf, "general.architecture", arch);
        write_kv_string(&mut buf, "general.name", name);
        write_string(&mut buf, &format!("{arch}.context_length"));
        buf.extend_from_slice(&4u32.to_le_bytes());
        buf.extend_from_slice(&2048u32.to_le_bytes());
        buf
    }

    #[test]
    fn reconcile_caches_digest_on_mtime_hit() {
        let dir = TempDir::new().unwrap();
        let models = dir.path().join("models");
        fs::create_dir_all(&models).unwrap();
        let index_path = dir.path().join("models.json");
        let model_path = models.join("tiny.gguf");
        fs::write(&model_path, build_gguf("llama", "Tiny")).unwrap();

        let first = ModelIndex::reconcile(&models, &index_path).unwrap();
        assert_eq!(first.models.len(), 1);
        let digest = first.models[0].digest.clone();
        assert_eq!(digest.len(), 64);

        // Touch would change mtime; instead leave unchanged and reconcile again.
        let second = ModelIndex::reconcile(&models, &index_path).unwrap();
        assert_eq!(second.models[0].digest, digest);
    }

    #[test]
    fn reconcile_rehashes_on_size_change() {
        let dir = TempDir::new().unwrap();
        let models = dir.path().join("models");
        fs::create_dir_all(&models).unwrap();
        let index_path = dir.path().join("models.json");
        let model_path = models.join("tiny.gguf");
        fs::write(&model_path, build_gguf("llama", "Tiny")).unwrap();
        let first = ModelIndex::reconcile(&models, &index_path).unwrap();
        let digest1 = first.models[0].digest.clone();

        let mut bigger = build_gguf("llama", "Tiny");
        bigger.extend_from_slice(b"extra-bytes-for-size-change");
        fs::write(&model_path, bigger).unwrap();
        // Ensure mtime/size differ
        let second = ModelIndex::reconcile(&models, &index_path).unwrap();
        assert_ne!(second.models[0].digest, digest1);
        assert_eq!(second.models[0].digest.len(), 64);
    }

    #[test]
    fn reconcile_drops_missing_files() {
        let dir = TempDir::new().unwrap();
        let models = dir.path().join("models");
        fs::create_dir_all(&models).unwrap();
        let index_path = dir.path().join("models.json");
        let model_path = models.join("gone.gguf");
        fs::write(&model_path, build_gguf("llama", "Gone")).unwrap();
        let first = ModelIndex::reconcile(&models, &index_path).unwrap();
        assert_eq!(first.models.len(), 1);
        fs::remove_file(&model_path).unwrap();
        let second = ModelIndex::reconcile(&models, &index_path).unwrap();
        assert!(second.models.is_empty());
    }

    #[test]
    fn find_by_digest_is_case_insensitive() {
        let entry = ModelIndexEntry {
            digest: "abcd".repeat(16),
            path: PathBuf::from("/tmp/a.gguf"),
            size_bytes: 1,
            mtime: 0,
            filename: "a.gguf".into(),
            architecture: "llama".into(),
            context_length: 4096,
            model_name: None,
            quantization: None,
            source_url: None,
            gguf_version: 3,
            block_count: 0,
            head_count: 0,
            embedding_length: 0,
        };
        let index = ModelIndex {
            version: 1,
            models: vec![entry],
        };
        assert!(index.find_by_digest(&"ABCD".repeat(16)).is_some());
    }

    #[test]
    fn guess_quant_from_filename() {
        assert_eq!(
            guess_quantization("model-Q4_K_M.gguf").as_deref(),
            Some("Q4_K_M")
        );
    }

    #[test]
    fn save_round_trip() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("models.json");
        let index = ModelIndex {
            version: 1,
            models: vec![],
        };
        index.save(&path).unwrap();
        let loaded = ModelIndex::load(&path).unwrap();
        assert_eq!(loaded.version, 1);
    }
}
