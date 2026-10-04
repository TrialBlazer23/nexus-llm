use futures_util::StreamExt;
use sha2::{Digest, Sha256};
use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use thiserror::Error;
use tracing::info;

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
}

/// Real-time progress metrics emitted during chunked download.
#[derive(Debug, Clone)]
pub struct DownloadProgress {
    pub downloaded_bytes: u64,
    pub total_bytes: Option<u64>,
    pub speed_bytes_per_sec: f64,
    pub percent: Option<f32>,
}

pub struct ModelDownloader {
    client: reqwest::Client,
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
        }
    }

    /// Download a file from `url` to `dest_path` with HTTP Range chunked resume and optional SHA-256 validation.
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
        let dest = dest_path.as_ref();
        let part_path = PathBuf::from(format!("{}.part", dest.display()));

        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent)?;
        }

        // Check if a partial file already exists to resume from
        let existing_bytes = if part_path.exists() {
            std::fs::metadata(&part_path)?.len()
        } else {
            0
        };

        let mut req = self.client.get(url);
        if existing_bytes > 0 {
            info!("Resuming download from byte offset {}", existing_bytes);
            req = req.header("Range", format!("bytes={}-", existing_bytes));
        }

        let resp = req.send().await?;
        let status = resp.status();

        if !status.is_success() && status != reqwest::StatusCode::PARTIAL_CONTENT {
            return Err(DownloaderError::HttpStatus(status));
        }

        let (mut file, mut downloaded, total_bytes) = if status == reqwest::StatusCode::PARTIAL_CONTENT {
            let total = resp
                .content_length()
                .map(|rem| rem + existing_bytes);

            let file = OpenOptions::new()
                .create(true)
                .append(true)
                .open(&part_path)?;

            (file, existing_bytes, total)
        } else {
            // Server did not honor Range or download starting fresh
            let total = resp.content_length();
            let file = File::create(&part_path)?;
            (file, 0u64, total)
        };

        let mut stream = resp.bytes_stream();
        let mut last_emit = Instant::now();
        let mut bytes_since_emit = 0u64;

        while let Some(chunk_res) = stream.next().await {
            let chunk = chunk_res?;
            file.write_all(&chunk)?;

            downloaded += chunk.len() as u64;
            bytes_since_emit += chunk.len() as u64;

            let elapsed = last_emit.elapsed();
            if elapsed >= Duration::from_millis(500) {
                let speed = (bytes_since_emit as f64) / elapsed.as_secs_f64();
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

        file.flush()?;
        drop(file);

        // Verify SHA-256 if expected hash is provided
        if let Some(expected_hash) = expected_sha256 {
            info!("Verifying SHA-256 checksum on downloaded file...");
            let actual_hash = Self::calculate_sha256(&part_path)?;
            let expected_clean = expected_hash.trim().to_lowercase();

            if actual_hash != expected_clean {
                return Err(DownloaderError::ChecksumMismatch {
                    expected: expected_clean,
                    actual: actual_hash,
                });
            }
            info!("SHA-256 verification passed: {}", actual_hash);
        }

        // Atomic move from .part to final destination
        std::fs::rename(&part_path, dest)?;
        info!("Download completed successfully: {:?}", dest);

        Ok(())
    }

    /// Calculate SHA-256 hash digest of a file on disk.
    pub fn calculate_sha256<P: AsRef<Path>>(path: P) -> Result<String, std::io::Error> {
        let mut file = File::open(path)?;
        let mut hasher = Sha256::new();
        let mut buffer = [0u8; 65536];

        loop {
            let n = file.read(&mut buffer)?;
            if n == 0 {
                break;
            }
            hasher.update(&buffer[..n]);
        }

        let result = hasher.finalize();
        Ok(format!("{:x}", result))
    }
}
