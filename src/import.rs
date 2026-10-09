//! Model import pipeline for local and mobile storage scanning, validation, and linking.

use crate::gguf::GgufMetadata;
use crate::hf::{calculate_fit_status, FitStatus};
use crate::store::ModelIndex;
use crate::sysinfo::SystemProfile;
use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use thiserror::Error;
use tracing::{info, warn};

#[derive(Error, Debug)]
pub enum ImportError {
    #[error("I/O error during model import: {0}")]
    Io(#[from] std::io::Error),

    #[error("Source file does not exist: {0:?}")]
    SourceNotFound(PathBuf),

    #[error("Target file already exists in models directory: {0:?}")]
    TargetAlreadyExists(PathBuf),

    #[error("Invalid GGUF model: {0}")]
    InvalidGguf(String),

    #[error("Failed to reconcile models index: {0}")]
    ReconcileFailed(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImportMode {
    Symlink,
    Copy,
    Move,
}

impl std::fmt::Display for ImportMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ImportMode::Symlink => write!(f, "symlink"),
            ImportMode::Copy => write!(f, "copy"),
            ImportMode::Move => write!(f, "move"),
        }
    }
}

#[derive(Debug, Clone)]
pub struct ImportCandidate {
    pub source_path: PathBuf,
    pub filename: String,
    pub size_bytes: u64,
    pub size_mb: u64,
    pub is_valid_gguf: bool,
    pub validation_error: Option<String>,
    pub architecture: Option<String>,
    pub context_length: Option<usize>,
    pub fit_status: FitStatus,
    pub already_in_models_dir: bool,
}

#[derive(Debug, Clone)]
pub struct ImportOutcome {
    pub filename: String,
    pub source: PathBuf,
    pub target: PathBuf,
    pub mode_used: ImportMode,
    pub size_bytes: u64,
}

/// Detect standard download and storage locations on the current platform.
pub fn candidate_scan_locations() -> Vec<PathBuf> {
    let mut dirs = Vec::new();

    // Android Termux standard storage locations
    dirs.push(PathBuf::from("/sdcard/Download"));
    dirs.push(PathBuf::from("/storage/emulated/0/Download"));

    if let Ok(home) = std::env::var("HOME") {
        let home_path = PathBuf::from(home);
        dirs.push(home_path.join("storage/downloads"));
        dirs.push(home_path.join("downloads"));
        dirs.push(home_path.join("Downloads"));
        dirs.push(home_path.join("Downloads/models"));
    }

    // Current working directory
    if let Ok(cwd) = std::env::current_dir() {
        dirs.push(cwd);
    }

    // Filter only existing directories and deduplicate canonical paths
    let mut seen = HashSet::new();
    let mut valid_dirs = Vec::new();

    for d in dirs {
        if d.is_dir() {
            let canon = d.canonicalize().unwrap_or_else(|_| d.clone());
            if seen.insert(canon) {
                valid_dirs.push(d);
            }
        }
    }

    valid_dirs
}

/// Scan a specific directory for `.gguf` model candidates.
pub fn scan_directory(dir: &Path, models_dir: &Path) -> Vec<ImportCandidate> {
    let mut candidates = Vec::new();
    let entries = match fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return candidates,
    };

    let sysinfo = SystemProfile::probe();
    let available_ram_mb = sysinfo.available_ram_mb;
    let cluster_free_mb = available_ram_mb;

    let canon_models_dir = models_dir
        .canonicalize()
        .unwrap_or_else(|_| models_dir.to_path_buf());

    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }

        let is_gguf = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.eq_ignore_ascii_case("gguf"))
            .unwrap_or(false);

        if !is_gguf {
            continue;
        }

        let meta = match fs::metadata(&path) {
            Ok(m) => m,
            Err(_) => continue,
        };

        let size_bytes = meta.len();
        let size_mb = size_bytes / (1024 * 1024);
        let filename = path
            .file_name()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_default();

        let already_in_models_dir = if let Ok(canon_path) = path.canonicalize() {
            canon_path
                .parent()
                .map(|p| p == canon_models_dir)
                .unwrap_or(false)
                || models_dir.join(&filename).exists()
        } else {
            models_dir.join(&filename).exists()
        };

        if size_bytes == 0 {
            candidates.push(ImportCandidate {
                source_path: path,
                filename,
                size_bytes: 0,
                size_mb: 0,
                is_valid_gguf: false,
                validation_error: Some("File is empty (0 bytes)".into()),
                architecture: None,
                context_length: None,
                fit_status: FitStatus::Exceeds,
                already_in_models_dir,
            });
            continue;
        }

        match GgufMetadata::open(&path) {
            Ok(header) => {
                let fit_status =
                    calculate_fit_status(size_bytes, available_ram_mb, cluster_free_mb);
                candidates.push(ImportCandidate {
                    source_path: path,
                    filename,
                    size_bytes,
                    size_mb,
                    is_valid_gguf: true,
                    validation_error: None,
                    architecture: header.architecture,
                    context_length: header.context_length,
                    fit_status,
                    already_in_models_dir,
                });
            }
            Err(e) => {
                candidates.push(ImportCandidate {
                    source_path: path,
                    filename,
                    size_bytes,
                    size_mb,
                    is_valid_gguf: false,
                    validation_error: Some(format!("Invalid GGUF header: {e}")),
                    architecture: None,
                    context_length: None,
                    fit_status: FitStatus::Exceeds,
                    already_in_models_dir,
                });
            }
        }
    }

    candidates.sort_by_key(|a| a.filename.to_lowercase());
    candidates
}

/// Scan all standard candidate download directories.
pub fn scan_all_candidate_locations(models_dir: &Path) -> Vec<ImportCandidate> {
    let mut all = Vec::new();
    let mut seen_paths = HashSet::new();

    for dir in candidate_scan_locations() {
        // Skip scanning the models directory itself
        if let (Ok(c1), Ok(c2)) = (dir.canonicalize(), models_dir.canonicalize()) {
            if c1 == c2 {
                continue;
            }
        }
        for candidate in scan_directory(&dir, models_dir) {
            let canon = candidate
                .source_path
                .canonicalize()
                .unwrap_or_else(|_| candidate.source_path.clone());
            if seen_paths.insert(canon) {
                all.push(candidate);
            }
        }
    }

    all.sort_by_key(|a| a.filename.to_lowercase());
    all
}

/// Import a candidate file into `models_dir` using symlink with copy fallback.
pub fn import_file(
    source_path: &Path,
    models_dir: &Path,
    preferred_mode: ImportMode,
) -> Result<ImportOutcome, ImportError> {
    if !source_path.is_file() {
        return Err(ImportError::SourceNotFound(source_path.to_path_buf()));
    }

    fs::create_dir_all(models_dir)?;

    let filename = source_path
        .file_name()
        .ok_or_else(|| ImportError::InvalidGguf("Invalid source file name".into()))?
        .to_string_lossy()
        .to_string();

    let target = models_dir.join(&filename);
    let meta = fs::metadata(source_path)?;
    let size_bytes = meta.len();

    // Check if source and target are already the same file
    if target.exists() {
        if let (Ok(c_src), Ok(c_tgt)) = (source_path.canonicalize(), target.canonicalize()) {
            if c_src == c_tgt {
                info!(
                    "File {:?} is already imported in models directory",
                    filename
                );
                return Ok(ImportOutcome {
                    filename,
                    source: source_path.to_path_buf(),
                    target,
                    mode_used: ImportMode::Symlink,
                    size_bytes,
                });
            }
        }
        return Err(ImportError::TargetAlreadyExists(target));
    }

    let mode_used = match preferred_mode {
        ImportMode::Symlink => {
            #[cfg(unix)]
            let symlink_res = std::os::unix::fs::symlink(source_path, &target);
            #[cfg(windows)]
            let symlink_res = std::os::windows::fs::symlink_file(source_path, &target);

            match symlink_res {
                Ok(()) => {
                    info!("Successfully symlinked {:?} -> {:?}", source_path, target);
                    ImportMode::Symlink
                }
                Err(err) => {
                    warn!(
                        "Symlink failed ({}); falling back to copying {:?} -> {:?}",
                        err, source_path, target
                    );
                    fs::copy(source_path, &target)?;
                    ImportMode::Copy
                }
            }
        }
        ImportMode::Copy => {
            fs::copy(source_path, &target)?;
            info!("Successfully copied {:?} -> {:?}", source_path, target);
            ImportMode::Copy
        }
        ImportMode::Move => {
            if fs::rename(source_path, &target).is_err() {
                // Cross-device move fallback
                fs::copy(source_path, &target)?;
                let _ = fs::remove_file(source_path);
            }
            info!("Successfully moved {:?} -> {:?}", source_path, target);
            ImportMode::Move
        }
    };

    // Reconcile model index immediately
    ModelIndex::reconcile_default(models_dir)
        .map_err(|e| ImportError::ReconcileFailed(e.to_string()))?;

    Ok(ImportOutcome {
        filename,
        source: source_path.to_path_buf(),
        target,
        mode_used,
        size_bytes,
    })
}

/// Find corrupted or 0-byte `.gguf` files within `models_dir`.
pub fn find_corrupted_models(models_dir: &Path) -> Vec<PathBuf> {
    let mut corrupted = Vec::new();
    let entries = match fs::read_dir(models_dir) {
        Ok(e) => e,
        Err(_) => return corrupted,
    };

    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }

        let is_gguf = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.eq_ignore_ascii_case("gguf"))
            .unwrap_or(false);

        if !is_gguf {
            continue;
        }

        let meta = match fs::metadata(&path) {
            Ok(m) => m,
            Err(_) => {
                corrupted.push(path);
                continue;
            }
        };

        if meta.len() == 0 {
            warn!("Model file {:?} is 0 bytes (empty)", path);
            corrupted.push(path);
        } else {
            match GgufMetadata::open(&path) {
                Ok(_) => {}
                Err(e) => {
                    warn!(
                        "Model file {:?} failed GGUF metadata verification: {}",
                        path, e
                    );
                    corrupted.push(path);
                }
            }
        }
    }

    corrupted.sort();
    corrupted
}

/// Safely delete a list of corrupted model files from disk and update the index.
pub fn delete_corrupted_models(
    files: &[PathBuf],
    models_dir: &Path,
) -> Result<usize, std::io::Error> {
    let mut deleted = 0;
    for file in files {
        if file.exists() && fs::remove_file(file).is_ok() {
            info!("Removed corrupted model file: {:?}", file);
            deleted += 1;
        }
    }
    let _ = ModelIndex::reconcile_default(models_dir);
    Ok(deleted)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn test_scan_and_import_symlink_with_fallback() {
        let temp_src = TempDir::new().unwrap();
        let temp_models = TempDir::new().unwrap();

        // Create a fake GGUF file
        let src_file = temp_src.path().join("test-model.gguf");
        // Write GGUF magic b"GGUF" followed by version 3 and 0 tensors/kv
        let mut data = Vec::new();
        data.extend_from_slice(b"GGUF");
        data.extend_from_slice(&3u32.to_le_bytes()); // version 3
        data.extend_from_slice(&0u64.to_le_bytes()); // tensor count
        data.extend_from_slice(&0u64.to_le_bytes()); // kv count
        fs::write(&src_file, &data).unwrap();

        let candidates = scan_directory(temp_src.path(), temp_models.path());
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].filename, "test-model.gguf");
        assert!(candidates[0].is_valid_gguf);
        assert_eq!(candidates[0].size_bytes, data.len() as u64);

        let outcome = import_file(&src_file, temp_models.path(), ImportMode::Symlink).unwrap();
        assert_eq!(outcome.filename, "test-model.gguf");
        assert!(outcome.target.exists());

        // Target should be in find_corrupted_models? No, it has valid magic
        let corrupted = find_corrupted_models(temp_models.path());
        assert!(corrupted.is_empty());
    }

    #[test]
    fn test_corrupted_model_detection_and_cleanup() {
        let temp_models = TempDir::new().unwrap();

        // Create a 0-byte file
        let zero_byte = temp_models.path().join("empty.gguf");
        fs::write(&zero_byte, b"").unwrap();

        // Create an HTML error file
        let html_file = temp_models.path().join("webpage.gguf");
        fs::write(
            &html_file,
            b"<!DOCTYPE html><html><body>Error</body></html>",
        )
        .unwrap();

        let corrupted = find_corrupted_models(temp_models.path());
        assert_eq!(corrupted.len(), 2);

        let deleted = delete_corrupted_models(&corrupted, temp_models.path()).unwrap();
        assert_eq!(deleted, 2);

        assert!(!zero_byte.exists());
        assert!(!html_file.exists());
    }
}
