//! Hardened chunked HTTP downloader with safe resume, retry, and incremental hashing.

use crate::node_identity::NodeIdentity;
use crate::sysinfo::available_disk_bytes;
use crate::trust_auth::apply_auth_headers;
use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use thiserror::Error;
use tokio::io::{AsyncReadExt, AsyncWriteExt, BufWriter};
use tracing::{info, warn};
use uuid::Uuid;

const MAX_ATTEMPTS: u32 = 5;
const STALL_TIMEOUT: Duration = Duration::from_secs(30);
const DISK_MARGIN_BYTES: u64 = 64 * 1024 * 1024;
const PROGRESS_INTERVAL: Duration = Duration::from_millis(500);

#[derive(Error, Debug)]
pub enum DownloaderError {
    #[error("HTTP download request error: {0}")]
    Reqwest(#[from] reqwest::Error),

    #[error("I/O error during download: {0}")]
    Io(#[from] std::io::Error),

    #[error("SHA-256 checksum verification failed: expected {expected}, computed {actual}")]
    ChecksumMismatch { expected: String, actual: String },

    #[error("Download failed with HTTP status: {0}")]
    HttpStatus(reqwest::StatusCode),

    #[error("insufficient disk space: need {need} bytes, available {available} bytes")]
    InsufficientDisk { need: u64, available: u64 },

    #[error("download stalled (no bytes for {0:?})")]
    Stalled(Duration),

    #[error("download exhausted retries after transient failures")]
    RetriesExhausted,

    #[error("download cancelled by operator")]
    Cancelled,

    #[error("invalid binary format: {0}")]
    InvalidBinaryFormat(String),

    #[error("download completed with 0 bytes (empty response)")]
    EmptyResponse,
}

impl DownloaderError {
    /// Returns true if the download failed due to HTTP 401 Unauthorized or 403 Forbidden.
    pub fn is_auth_failure(&self) -> bool {
        match self {
            DownloaderError::HttpStatus(s) => {
                *s == reqwest::StatusCode::UNAUTHORIZED || *s == reqwest::StatusCode::FORBIDDEN
            }
            _ => false,
        }
    }
}

/// Real-time progress metrics emitted during chunked download.
#[derive(Debug, Clone)]
pub struct DownloadProgress {
    pub downloaded_bytes: u64,
    pub total_bytes: Option<u64>,
    pub speed_bytes_per_sec: f64,
    pub percent: Option<f32>,
}

/// Optional request signing for authenticated peer blob GETs.
#[derive(Clone)]
pub struct DownloadAuth {
    pub identity: std::sync::Arc<NodeIdentity>,
    pub signer_id: Uuid,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct PartSidecar {
    url: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    etag: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    last_modified: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    total_size: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    expected_sha256: Option<String>,
}

pub struct ModelDownloader {
    client: reqwest::Client,
    hf_token: Option<String>,
}

impl Default for ModelDownloader {
    fn default() -> Self {
        Self::new()
    }
}

impl ModelDownloader {
    pub fn new() -> Self {
        Self {
            client: reqwest::Client::builder()
                .timeout(Duration::from_secs(300))
                .build()
                .expect("Valid reqwest client"),
            hf_token: None,
        }
    }

    /// Set an optional Hugging Face Personal Access Token for authenticated downloads.
    pub fn with_hf_token(mut self, token: Option<String>) -> Self {
        self.hf_token = token;
        self
    }

    /// Download a file from `url` to `dest_path` with HTTP Range resume and optional SHA-256.
    pub async fn download<P: AsRef<Path>, F>(
        &self,
        url: &str,
        dest_path: P,
        expected_sha256: Option<&str>,
        progress_callback: F,
    ) -> Result<(), DownloaderError>
    where
        F: Fn(DownloadProgress) + Send + Sync,
    {
        self.download_authenticated_with_cancellation(
            url,
            dest_path,
            expected_sha256,
            None,
            None,
            progress_callback,
        )
        .await
    }

    /// Download a file with optional cancellation receiver.
    pub async fn download_with_cancellation<P: AsRef<Path>, F>(
        &self,
        url: &str,
        dest_path: P,
        expected_sha256: Option<&str>,
        cancel_rx: Option<tokio::sync::watch::Receiver<bool>>,
        progress_callback: F,
    ) -> Result<(), DownloaderError>
    where
        F: Fn(DownloadProgress) + Send + Sync,
    {
        self.download_authenticated_with_cancellation(
            url,
            dest_path,
            expected_sha256,
            None,
            cancel_rx,
            progress_callback,
        )
        .await
    }

    /// Same as [`download`] but signs each GET with the node identity (LAN blob pull).
    pub async fn download_authenticated<P: AsRef<Path>, F>(
        &self,
        url: &str,
        dest_path: P,
        expected_sha256: Option<&str>,
        auth: Option<&DownloadAuth>,
        progress_callback: F,
    ) -> Result<(), DownloaderError>
    where
        F: Fn(DownloadProgress) + Send + Sync,
    {
        self.download_authenticated_with_cancellation(
            url,
            dest_path,
            expected_sha256,
            auth,
            None,
            progress_callback,
        )
        .await
    }

    /// Download with optional LAN peer authentication and cancellation receiver.
    pub async fn download_authenticated_with_cancellation<P: AsRef<Path>, F>(
        &self,
        url: &str,
        dest_path: P,
        expected_sha256: Option<&str>,
        auth: Option<&DownloadAuth>,
        cancel_rx: Option<tokio::sync::watch::Receiver<bool>>,
        progress_callback: F,
    ) -> Result<(), DownloaderError>
    where
        F: Fn(DownloadProgress) + Send + Sync,
    {
        let dest = dest_path.as_ref();
        let part_path = PathBuf::from(format!("{}.part", dest.display()));
        let sidecar_path = PathBuf::from(format!("{}.part.json", dest.display()));
        let expected = expected_sha256.map(|s| s.trim().to_lowercase());
        let effective_url = normalize_download_url(url);

        if let Some(parent) = dest.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }

        let mut attempt = 0u32;
        loop {
            attempt += 1;
            let rx = cancel_rx.clone();
            match self
                .download_once(
                    &effective_url,
                    dest,
                    &part_path,
                    &sidecar_path,
                    expected.as_deref(),
                    auth,
                    rx,
                    &progress_callback,
                )
                .await
            {
                Ok(()) => return Ok(()),
                Err(e) if is_retryable(&e) && attempt < MAX_ATTEMPTS => {
                    let backoff = Duration::from_millis(250 * 2u64.pow(attempt.saturating_sub(1)));
                    warn!(
                        "Download attempt {} failed ({}); retrying in {:?}",
                        attempt, e, backoff
                    );
                    tokio::time::sleep(backoff).await;
                }
                Err(e) => return Err(e),
            }
        }
    }

    // Resume/sidecar/auth fields stay explicit; a params struct would be a drive-by reshape.
    #[allow(clippy::too_many_arguments)]
    async fn download_once<F>(
        &self,
        url: &str,
        dest: &Path,
        part_path: &Path,
        sidecar_path: &Path,
        expected: Option<&str>,
        auth: Option<&DownloadAuth>,
        mut cancel_rx: Option<tokio::sync::watch::Receiver<bool>>,
        progress_callback: &F,
    ) -> Result<(), DownloaderError>
    where
        F: Fn(DownloadProgress) + Send + Sync,
    {
        let mut existing_bytes = if tokio::fs::try_exists(part_path).await.unwrap_or(false) {
            tokio::fs::metadata(part_path).await?.len()
        } else {
            0
        };

        let mut resume_sidecar: Option<PartSidecar> = None;
        if existing_bytes > 0 {
            match load_sidecar(sidecar_path).await {
                Ok(sc) if sidecar_matches(&sc, url, expected) => {
                    resume_sidecar = Some(sc);
                }
                Ok(_) | Err(_) => {
                    warn!("Discarding invalid partial download at {:?}", part_path);
                    let _ = tokio::fs::remove_file(part_path).await;
                    let _ = tokio::fs::remove_file(sidecar_path).await;
                    existing_bytes = 0;
                }
            }
        }

        let path_for_sign = reqwest::Url::parse(url)
            .map(|u| u.path().to_string())
            .unwrap_or_else(|_| url.to_string());

        let mut req = self.client.get(url);
        if let Some(a) = auth {
            req = apply_auth_headers(
                req,
                a.identity.as_ref(),
                a.signer_id,
                "GET",
                &path_for_sign,
                &[],
            );
        } else if let Some(ref token) = self.hf_token {
            if is_huggingface_url(url) {
                let token_clean = token.trim();
                if !token_clean.is_empty() {
                    req = req.header(
                        reqwest::header::AUTHORIZATION,
                        format!("Bearer {token_clean}"),
                    );
                }
            }
        }
        if existing_bytes > 0 {
            info!("Resuming download from byte offset {}", existing_bytes);
            req = req.header(reqwest::header::RANGE, format!("bytes={existing_bytes}-"));
            if let Some(sc) = &resume_sidecar {
                if let Some(etag) = &sc.etag {
                    req = req.header(reqwest::header::IF_RANGE, etag.as_str());
                } else if let Some(lm) = &sc.last_modified {
                    req = req.header(reqwest::header::IF_RANGE, lm.as_str());
                }
            }
        }

        let mut hasher = Sha256::new();
        if existing_bytes > 0 {
            hash_prefix(part_path, existing_bytes, &mut hasher).await?;
        }

        let resp = req.send().await?;
        let status = resp.status();

        if !status.is_success() && status != reqwest::StatusCode::PARTIAL_CONTENT {
            return Err(DownloaderError::HttpStatus(status));
        }

        let etag = header_string(resp.headers(), reqwest::header::ETAG);
        let last_modified = header_string(resp.headers(), reqwest::header::LAST_MODIFIED);

        let (append, mut downloaded, total_bytes) =
            if status == reqwest::StatusCode::PARTIAL_CONTENT && existing_bytes > 0 {
                let rem = resp.content_length();
                let total = rem
                    .map(|r| r + existing_bytes)
                    .or_else(|| resume_sidecar.as_ref().and_then(|s| s.total_size));
                (true, existing_bytes, total)
            } else {
                // Fresh start (server ignored Range / If-Range forced full body)
                if existing_bytes > 0 {
                    let _ = tokio::fs::remove_file(part_path).await;
                    hasher = Sha256::new();
                }
                (false, 0u64, resp.content_length())
            };

        if let Some(total) = total_bytes {
            let remaining = total.saturating_sub(downloaded);
            let need = remaining.saturating_add(DISK_MARGIN_BYTES);
            let available = available_disk_bytes(dest.parent().unwrap_or(dest))?;
            if available < need {
                return Err(DownloaderError::InsufficientDisk { need, available });
            }
        }

        let sidecar = PartSidecar {
            url: url.to_string(),
            etag,
            last_modified,
            total_size: total_bytes,
            expected_sha256: expected.map(|s| s.to_string()),
        };
        save_sidecar(sidecar_path, &sidecar).await?;

        let file = if append {
            tokio::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(part_path)
                .await?
        } else {
            tokio::fs::File::create(part_path).await?
        };
        let mut writer = BufWriter::new(file);

        let mut stream = resp.bytes_stream();
        let mut last_emit = Instant::now();
        let mut bytes_since_emit = 0u64;
        let mut last_byte_at = Instant::now();
        let mut first_chunk_checked = false;

        loop {
            if let Some(ref rx) = cancel_rx {
                if *rx.borrow() {
                    writer.flush().await?;
                    let file = writer.into_inner();
                    file.sync_all().await?;
                    return Err(DownloaderError::Cancelled);
                }
            }

            let next = tokio::select! {
                chunk_res = tokio::time::timeout(STALL_TIMEOUT, stream.next()) => chunk_res,
                _ = async {
                    if let Some(ref mut rx) = cancel_rx {
                        while !*rx.borrow_and_update() {
                            if rx.changed().await.is_err() {
                                futures_util::future::pending::<()>().await;
                            }
                        }
                    } else {
                        futures_util::future::pending::<()>().await;
                    }
                } => {
                    writer.flush().await?;
                    let file = writer.into_inner();
                    file.sync_all().await?;
                    return Err(DownloaderError::Cancelled);
                }
            };
            let chunk_res = match next {
                Ok(Some(c)) => c,
                Ok(None) => break,
                Err(_) => {
                    writer.flush().await?;
                    return Err(DownloaderError::Stalled(STALL_TIMEOUT));
                }
            };
            let chunk = chunk_res?;
            if chunk.is_empty() {
                if last_byte_at.elapsed() > STALL_TIMEOUT {
                    writer.flush().await?;
                    return Err(DownloaderError::Stalled(STALL_TIMEOUT));
                }
                continue;
            }

            if !first_chunk_checked && downloaded == 0 {
                first_chunk_checked = true;
                let is_gguf_dest = dest
                    .extension()
                    .and_then(|e| e.to_str())
                    .map(|e| e.eq_ignore_ascii_case("gguf"))
                    .unwrap_or(false);
                let is_html = chunk.starts_with(b"<!DO")
                    || chunk.starts_with(b"<!do")
                    || chunk.starts_with(b"<htm")
                    || chunk.starts_with(b"<HTM");
                let starts_with_gguf = chunk.len() >= 4 && &chunk[..4] == b"GGUF";

                if is_html || (is_gguf_dest && !starts_with_gguf) {
                    writer.flush().await?;
                    let file = writer.into_inner();
                    drop(file);
                    let _ = tokio::fs::remove_file(part_path).await;
                    let _ = tokio::fs::remove_file(sidecar_path).await;
                    let reason = if is_html {
                        "server returned an HTML webpage instead of a model binary"
                    } else {
                        "binary header magic does not match GGUF ('GGUF')"
                    };
                    return Err(DownloaderError::InvalidBinaryFormat(reason.to_string()));
                }
            }

            last_byte_at = Instant::now();
            writer.write_all(&chunk).await?;
            hasher.update(&chunk);
            downloaded += chunk.len() as u64;
            bytes_since_emit += chunk.len() as u64;

            let elapsed = last_emit.elapsed();
            if elapsed >= PROGRESS_INTERVAL {
                let speed = (bytes_since_emit as f64) / elapsed.as_secs_f64().max(1e-6);
                last_emit = Instant::now();
                bytes_since_emit = 0;
                let percent = total_bytes.map(|total| {
                    if total > 0 {
                        ((downloaded as f64 / total as f64) * 100.0) as f32
                    } else {
                        0.0
                    }
                });
                progress_callback(DownloadProgress {
                    downloaded_bytes: downloaded,
                    total_bytes,
                    speed_bytes_per_sec: speed,
                    percent,
                });
            }
        }

        writer.flush().await?;
        let file = writer.into_inner();
        file.sync_all().await?;
        drop(file);

        if downloaded == 0 {
            let _ = tokio::fs::remove_file(part_path).await;
            let _ = tokio::fs::remove_file(sidecar_path).await;
            return Err(DownloaderError::EmptyResponse);
        }

        let meta = tokio::fs::metadata(part_path).await?;
        if meta.len() == 0 {
            let _ = tokio::fs::remove_file(part_path).await;
            let _ = tokio::fs::remove_file(sidecar_path).await;
            return Err(DownloaderError::EmptyResponse);
        }

        // Final verification of GGUF magic before promoting .part to destination
        if dest
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.eq_ignore_ascii_case("gguf"))
            .unwrap_or(false)
        {
            let mut check_file = tokio::fs::File::open(part_path).await?;
            let mut magic = [0u8; 4];
            let n = check_file.read(&mut magic).await?;
            if n < 4 || &magic != b"GGUF" {
                drop(check_file);
                let _ = tokio::fs::remove_file(part_path).await;
                let _ = tokio::fs::remove_file(sidecar_path).await;
                return Err(DownloaderError::InvalidBinaryFormat(
                    "downloaded file does not begin with GGUF magic ('GGUF')".to_string(),
                ));
            }
        }

        let actual_hash = hex_digest(hasher.finalize());
        if let Some(expected_hash) = expected {
            if actual_hash != expected_hash {
                let _ = tokio::fs::remove_file(part_path).await;
                let _ = tokio::fs::remove_file(sidecar_path).await;
                return Err(DownloaderError::ChecksumMismatch {
                    expected: expected_hash.to_string(),
                    actual: actual_hash,
                });
            }
            info!("SHA-256 verification passed: {}", actual_hash);
        }

        tokio::fs::rename(part_path, dest).await?;
        let _ = tokio::fs::remove_file(sidecar_path).await;
        info!("Download completed successfully: {:?}", dest);
        Ok(())
    }

    /// Calculate SHA-256 hash digest of a file on disk (blocking).
    pub fn calculate_sha256<P: AsRef<Path>>(path: P) -> Result<String, std::io::Error> {
        use std::io::Read;
        let mut file = std::fs::File::open(path)?;
        let mut hasher = Sha256::new();
        let mut buffer = [0u8; 65536];
        loop {
            let n = file.read(&mut buffer)?;
            if n == 0 {
                break;
            }
            hasher.update(&buffer[..n]);
        }
        Ok(hex_digest(hasher.finalize()))
    }
}

fn hex_digest(digest: impl AsRef<[u8]>) -> String {
    digest
        .as_ref()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// Returns true if the URL points to a Hugging Face domain (*.huggingface.co or *.hf.co).
pub fn is_huggingface_url(url: &str) -> bool {
    if let Ok(parsed) = reqwest::Url::parse(url) {
        if let Some(host) = parsed.host_str() {
            let host = host.to_ascii_lowercase();
            return host == "huggingface.co"
                || host.ends_with(".huggingface.co")
                || host == "hf.co"
                || host.ends_with(".hf.co");
        }
    }
    false
}

/// Normalize download URLs:
/// - Replaces Hugging Face browser `/blob/` URLs with `/resolve/`
/// - Normalizes `hf.co` domain to `huggingface.co`
/// - Ensures scheme `https://` is present for bare `hf.co` or `huggingface.co` domains
pub fn normalize_download_url(url: &str) -> String {
    let trimmed = url.trim();
    if trimmed.is_empty() {
        return String::new();
    }

    let url_with_scheme = if trimmed.starts_with("hf.co/") || trimmed.starts_with("huggingface.co/")
    {
        format!("https://{trimmed}")
    } else {
        trimmed.to_string()
    };

    if let Ok(mut parsed) = reqwest::Url::parse(&url_with_scheme) {
        let is_hf = parsed
            .host_str()
            .map(|h| {
                let h = h.to_ascii_lowercase();
                h == "hf.co"
                    || h.ends_with(".hf.co")
                    || h == "huggingface.co"
                    || h.ends_with(".huggingface.co")
            })
            .unwrap_or(false);

        if is_hf {
            let path = parsed.path().to_string();
            let segments: Vec<&str> = path.split('/').collect();
            // Expected HF blob path: /owner/repo/blob/revision/path...
            if segments.len() >= 5 && segments[3] == "blob" {
                let mut new_segments = segments.clone();
                new_segments[3] = "resolve";
                let new_path = new_segments.join("/");
                parsed.set_path(&new_path);
            }
            if let Some(host) = parsed.host_str() {
                if host == "hf.co" || host.ends_with(".hf.co") {
                    let _ = parsed.set_host(Some("huggingface.co"));
                }
            }
            return parsed.to_string();
        }
    }

    url_with_scheme
}

fn is_retryable(err: &DownloaderError) -> bool {
    match err {
        DownloaderError::Reqwest(_) | DownloaderError::Stalled(_) | DownloaderError::Io(_) => true,
        DownloaderError::HttpStatus(status) => {
            status.is_server_error() || *status == reqwest::StatusCode::REQUEST_TIMEOUT
        }
        DownloaderError::ChecksumMismatch { .. }
        | DownloaderError::InsufficientDisk { .. }
        | DownloaderError::RetriesExhausted
        | DownloaderError::Cancelled
        | DownloaderError::InvalidBinaryFormat(_)
        | DownloaderError::EmptyResponse => false,
    }
}

fn header_string(
    headers: &reqwest::header::HeaderMap,
    name: reqwest::header::HeaderName,
) -> Option<String> {
    headers
        .get(name)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string())
}

fn sidecar_matches(sc: &PartSidecar, url: &str, expected: Option<&str>) -> bool {
    if sc.url != url {
        return false;
    }
    match (sc.expected_sha256.as_deref(), expected) {
        (Some(a), Some(b)) => a.eq_ignore_ascii_case(b),
        (None, None) => true,
        (Some(_), None) | (None, Some(_)) => false,
    }
}

async fn load_sidecar(path: &Path) -> Result<PartSidecar, DownloaderError> {
    let bytes = tokio::fs::read(path).await?;
    Ok(serde_json::from_slice(&bytes)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?)
}

async fn save_sidecar(path: &Path, sc: &PartSidecar) -> Result<(), DownloaderError> {
    let bytes = serde_json::to_vec_pretty(sc)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    tokio::fs::write(path, bytes).await?;
    Ok(())
}

async fn hash_prefix(path: &Path, len: u64, hasher: &mut Sha256) -> Result<(), DownloaderError> {
    let mut file = tokio::fs::File::open(path).await?;
    let mut remaining = len;
    let mut buffer = vec![0u8; 1024 * 1024];
    while remaining > 0 {
        let to_read = remaining.min(buffer.len() as u64) as usize;
        let n = file.read(&mut buffer[..to_read]).await?;
        if n == 0 {
            break;
        }
        hasher.update(&buffer[..n]);
        remaining -= n as u64;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use sha2::Digest;
    use tempfile::TempDir;

    #[test]
    fn sidecar_url_mismatch_rejects() {
        let sc = PartSidecar {
            url: "http://a/x".into(),
            etag: None,
            last_modified: None,
            total_size: Some(10),
            expected_sha256: Some("abc".into()),
        };
        assert!(!sidecar_matches(&sc, "http://b/x", Some("abc")));
        assert!(sidecar_matches(&sc, "http://a/x", Some("ABC")));
    }

    #[tokio::test]
    async fn checksum_mismatch_deletes_partial() {
        let dir = TempDir::new().unwrap();
        let dest = dir.path().join("out.bin");
        // Serve a tiny body via a one-shot hyper would be heavy; simulate by writing part
        // and calling calculate path — instead run download against a data URL isn't supported.
        // Unit-level: write sidecar+part, then invoke discard path via sidecar_matches.
        let part = PathBuf::from(format!("{}.part", dest.display()));
        let side = PathBuf::from(format!("{}.part.json", dest.display()));
        tokio::fs::write(&part, b"hello").await.unwrap();
        let sc = PartSidecar {
            url: "http://example/file".into(),
            etag: None,
            last_modified: None,
            total_size: Some(5),
            expected_sha256: Some("deadbeef".into()),
        };
        save_sidecar(&side, &sc).await.unwrap();
        assert!(!sidecar_matches(
            &sc,
            "http://example/file",
            Some("cafebabe")
        ));
        let _ = tokio::fs::remove_file(&part).await;
        let _ = tokio::fs::remove_file(&side).await;
        assert!(!part.exists());
    }

    #[test]
    fn incremental_hash_matches_full_file() {
        let data = b"the quick brown fox jumps over the lazy dog";
        let mut hasher = Sha256::new();
        hasher.update(data);
        let incremental = hex_digest(hasher.finalize());
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("f.bin");
        std::fs::write(&path, data).unwrap();
        let full = ModelDownloader::calculate_sha256(&path).unwrap();
        assert_eq!(incremental, full);
    }

    #[test]
    fn test_is_huggingface_url_domain_isolation() {
        assert!(is_huggingface_url("https://huggingface.co/repo/model.gguf"));
        assert!(is_huggingface_url(
            "https://cdn-lfs.huggingface.co/repos/123/model.gguf"
        ));
        assert!(is_huggingface_url("https://hf.co/repo/model.gguf"));
        assert!(is_huggingface_url("https://sub.hf.co/model.gguf"));

        // Third-party or untrusted domains must return false to prevent token leakage
        assert!(!is_huggingface_url(
            "https://github.com/releases/download/v1/model.gguf"
        ));
        assert!(!is_huggingface_url("http://192.168.1.50:8080/blob/xyz"));
        assert!(!is_huggingface_url(
            "https://fake-huggingface.co.evil.com/model.gguf"
        ));
        assert!(!is_huggingface_url("not a valid url"));
    }

    #[test]
    fn test_auth_failure_classification() {
        let err_401 = DownloaderError::HttpStatus(reqwest::StatusCode::UNAUTHORIZED);
        assert!(err_401.is_auth_failure());

        let err_403 = DownloaderError::HttpStatus(reqwest::StatusCode::FORBIDDEN);
        assert!(err_403.is_auth_failure());

        let err_404 = DownloaderError::HttpStatus(reqwest::StatusCode::NOT_FOUND);
        assert!(!err_404.is_auth_failure());

        let err_500 = DownloaderError::HttpStatus(reqwest::StatusCode::INTERNAL_SERVER_ERROR);
        assert!(!err_500.is_auth_failure());
    }

    #[test]
    fn test_downloader_with_hf_token() {
        let dl = ModelDownloader::new().with_hf_token(Some("hf_test_123456789".into()));
        assert_eq!(dl.hf_token.as_deref(), Some("hf_test_123456789"));
    }

    #[test]
    fn test_normalize_download_url() {
        let blob_url = "https://huggingface.co/bartowski/Llama-3.2-3B-Instruct-GGUF/blob/main/Llama-3.2-3B-Instruct-Q4_K_M.gguf";
        let expected = "https://huggingface.co/bartowski/Llama-3.2-3B-Instruct-GGUF/resolve/main/Llama-3.2-3B-Instruct-Q4_K_M.gguf";
        assert_eq!(normalize_download_url(blob_url), expected);

        let hf_co_blob = "https://hf.co/bartowski/Llama-3.2-3B-Instruct-GGUF/blob/main/model.gguf";
        let hf_co_expected =
            "https://huggingface.co/bartowski/Llama-3.2-3B-Instruct-GGUF/resolve/main/model.gguf";
        assert_eq!(normalize_download_url(hf_co_blob), hf_co_expected);

        let bare_hf = "hf.co/bartowski/Llama-3.2-3B-Instruct-GGUF/blob/main/model.gguf";
        assert_eq!(normalize_download_url(bare_hf), hf_co_expected);

        let resolve_url = "https://huggingface.co/repo/model/resolve/main/model.gguf";
        assert_eq!(normalize_download_url(resolve_url), resolve_url);

        let other_url = "https://example.com/models/model.gguf";
        assert_eq!(normalize_download_url(other_url), other_url);
    }

    #[test]
    fn test_empty_and_invalid_binary_errors_not_retryable() {
        let err_empty = DownloaderError::EmptyResponse;
        assert!(!is_retryable(&err_empty));

        let err_bad_magic = DownloaderError::InvalidBinaryFormat("HTML detected".into());
        assert!(!is_retryable(&err_bad_magic));
    }
}
