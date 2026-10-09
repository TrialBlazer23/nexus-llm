//! Hugging Face Hub REST API client and GGUF quantization resolver (Phase 3).

use ratatui::style::Color;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::time::Duration;
use thiserror::Error;

#[derive(Error, Debug)]
pub enum HfError {
    #[error("Network error: {0}")]
    Reqwest(#[from] reqwest::Error),

    #[error("JSON deserialization error: {0}")]
    Json(#[from] serde_json::Error),

    #[error("Gated or unauthorized repo '{repo_id}' (HTTP {status})")]
    GatedOrUnauthorized { repo_id: String, status: u16 },

    #[error("Model repo '{0}' not found on Hugging Face (HTTP 404)")]
    NotFound(String),

    #[error("Hugging Face API error (HTTP {status}): {message}")]
    Api { status: u16, message: String },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum FitStatus {
    /// Fits safely within local node: Size + KV Cache <= 0.75 * MemAvailable
    Fits,
    /// Exceeds local safe limit, but fits within cluster aggregate free RAM
    OffloadRequired,
    /// Exceeds total available cluster memory
    Exceeds,
}

impl FitStatus {
    pub fn badge_text(&self) -> &'static str {
        match self {
            FitStatus::Fits => "[OK] Fits",
            FitStatus::OffloadRequired => "[RPC] Offload",
            FitStatus::Exceeds => "[WARN] Exceeds",
        }
    }

    pub fn color(&self) -> Color {
        match self {
            FitStatus::Fits => Color::Green,
            FitStatus::OffloadRequired => Color::Yellow,
            FitStatus::Exceeds => Color::Red,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HfModelSummary {
    pub id: String,
    #[serde(default)]
    pub author: Option<String>,
    #[serde(default)]
    pub downloads: u64,
    #[serde(default)]
    pub likes: u64,
    #[serde(default)]
    pub private: bool,
    #[serde(default)]
    pub gated: Option<serde_json::Value>,
    #[serde(default)]
    pub pipeline_tag: Option<String>,
    #[serde(default)]
    pub tags: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HfModelDetail {
    pub id: String,
    #[serde(default)]
    pub author: Option<String>,
    #[serde(default)]
    pub downloads: u64,
    #[serde(default)]
    pub likes: u64,
    #[serde(default)]
    pub private: bool,
    #[serde(default)]
    pub gated: Option<serde_json::Value>,
    #[serde(default)]
    pub siblings: Vec<HfSibling>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HfSibling {
    pub rfilename: String,
    #[serde(default)]
    pub size: Option<u64>,
    #[serde(default)]
    pub lfs: Option<HfLfs>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HfLfs {
    pub size: Option<u64>,
    pub sha256: Option<String>,
    pub oid: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HfGgufGroup {
    pub base_name: String,
    pub quant_label: String,
    pub total_size_bytes: u64,
    pub files: Vec<HfGgufFile>,
    pub is_sharded: bool,
    pub fit_status: FitStatus,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HfGgufFile {
    pub filename: String,
    pub size_bytes: u64,
    pub sha256: Option<String>,
    pub download_url: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CuratedModel {
    pub name: &'static str,
    pub description: &'static str,
    pub repo_id: &'static str,
    pub filename: &'static str,
    pub quant_label: &'static str,
    pub approx_size_mb: u64,
    pub min_ram_mb: u64,
    pub download_url: &'static str,
}

pub fn curated_starter_models() -> Vec<CuratedModel> {
    vec![
        CuratedModel {
            name: "Qwen 2.5 Coder 1.5B Instruct",
            description: "Fast code & general assistant. Great for low-memory devices.",
            repo_id: "Qwen/Qwen2.5-Coder-1.5B-Instruct-GGUF",
            filename: "qwen2.5-coder-1.5b-instruct-q4_k_m.gguf",
            quant_label: "Q4_K_M",
            approx_size_mb: 986,
            min_ram_mb: 1800,
            download_url: "https://huggingface.co/Qwen/Qwen2.5-Coder-1.5B-Instruct-GGUF/resolve/main/qwen2.5-coder-1.5b-instruct-q4_k_m.gguf",
        },
        CuratedModel {
            name: "Llama 3.2 1B Instruct",
            description: "Ultra-compact Meta conversational model for edge devices.",
            repo_id: "bartowski/Llama-3.2-1B-Instruct-GGUF",
            filename: "Llama-3.2-1B-Instruct-Q4_K_M.gguf",
            quant_label: "Q4_K_M",
            approx_size_mb: 808,
            min_ram_mb: 1500,
            download_url: "https://huggingface.co/bartowski/Llama-3.2-1B-Instruct-GGUF/resolve/main/Llama-3.2-1B-Instruct-Q4_K_M.gguf",
        },
        CuratedModel {
            name: "Llama 3.2 3B Instruct",
            description: "High quality lightweight assistant. Ideal balance for phones & laptops.",
            repo_id: "bartowski/Llama-3.2-3B-Instruct-GGUF",
            filename: "Llama-3.2-3B-Instruct-Q4_K_M.gguf",
            quant_label: "Q4_K_M",
            approx_size_mb: 2018,
            min_ram_mb: 3200,
            download_url: "https://huggingface.co/bartowski/Llama-3.2-3B-Instruct-GGUF/resolve/main/Llama-3.2-3B-Instruct-Q4_K_M.gguf",
        },
        CuratedModel {
            name: "Gemma 2 2B Instruct",
            description: "Google edge model with strong reasoning for its size.",
            repo_id: "bartowski/gemma-2-2b-it-GGUF",
            filename: "gemma-2-2b-it-Q4_K_M.gguf",
            quant_label: "Q4_K_M",
            approx_size_mb: 1630,
            min_ram_mb: 2600,
            download_url: "https://huggingface.co/bartowski/gemma-2-2b-it-GGUF/resolve/main/gemma-2-2b-it-Q4_K_M.gguf",
        },
        CuratedModel {
            name: "SmolLM2 1.7B Instruct",
            description: "Hugging Face ultra-efficient small LM for on-device chat.",
            repo_id: "HuggingFaceTB/SmolLM2-1.7B-Instruct-GGUF",
            filename: "smollm2-1.7b-instruct-q4_k_m.gguf",
            quant_label: "Q4_K_M",
            approx_size_mb: 1060,
            min_ram_mb: 2000,
            download_url: "https://huggingface.co/HuggingFaceTB/SmolLM2-1.7B-Instruct-GGUF/resolve/main/smollm2-1.7b-instruct-q4_k_m.gguf",
        },
    ]
}

pub struct HfClient {
    client: reqwest::Client,
    token: Option<String>,
    base_url: Option<String>,
}

impl HfClient {
    pub fn new(token: Option<String>) -> Self {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(15))
            .build()
            .unwrap_or_else(|_| reqwest::Client::new());
        Self {
            client,
            token,
            base_url: None,
        }
    }

    /// Override the base URL for offline mock testing.
    pub fn with_base_url(mut self, base_url: impl Into<String>) -> Self {
        self.base_url = Some(base_url.into());
        self
    }

    /// Helper to determine if an input string is a Hugging Face repo ID or URL.
    ///
    /// Accepts:
    /// - `"bartowski/Llama-3.2-3B-Instruct-GGUF"` -> `Some("bartowski/Llama-3.2-3B-Instruct-GGUF")`
    /// - `"https://huggingface.co/bartowski/Llama-3.2-3B-Instruct-GGUF"` -> `Some("bartowski/Llama-3.2-3B-Instruct-GGUF")`
    /// - Rejects direct file URLs containing `/resolve/`, `/blob/`, or ending in `.gguf`.
    pub fn parse_repo_id(input: &str) -> Option<String> {
        let trimmed = input.trim();
        if trimmed.is_empty() {
            return None;
        }

        // Handle full URLs: https://huggingface.co/{owner}/{repo} or https://hf.co/{owner}/{repo}
        if let Some(stripped) = trimmed
            .strip_prefix("https://huggingface.co/")
            .or_else(|| trimmed.strip_prefix("http://huggingface.co/"))
            .or_else(|| trimmed.strip_prefix("https://hf.co/"))
            .or_else(|| trimmed.strip_prefix("http://hf.co/"))
            .or_else(|| trimmed.strip_prefix("huggingface.co/"))
            .or_else(|| trimmed.strip_prefix("hf.co/"))
        {
            let stripped = stripped.trim_end_matches('/');
            let segments: Vec<&str> = stripped.split('/').filter(|s| !s.is_empty()).collect();
            if segments.len() >= 2 {
                if segments.contains(&"resolve")
                    || segments.contains(&"blob")
                    || stripped.to_ascii_lowercase().ends_with(".gguf")
                {
                    return None;
                }
                return Some(format!("{}/{}", segments[0], segments[1]));
            }
            return None;
        }

        // Handle bare repo identifiers: owner/model
        if !trimmed.contains("://") && !trimmed.contains(' ') {
            let segments: Vec<&str> = trimmed.split('/').collect();
            if segments.len() == 2
                && !segments[0].is_empty()
                && !segments[1].is_empty()
                && !trimmed.to_ascii_lowercase().ends_with(".gguf")
            {
                return Some(trimmed.to_string());
            }
        }

        None
    }

    /// Fetch repository details, metadata, and file siblings.
    pub async fn model_details(&self, repo_id: &str) -> Result<HfModelDetail, HfError> {
        let base = self.base_url.as_deref().unwrap_or("https://huggingface.co");
        let url = format!("{base}/api/models/{repo_id}");
        let mut req = self.client.get(&url);
        if let Some(tok) = &self.token {
            let t = tok.trim();
            if !t.is_empty() {
                req = req.header("Authorization", format!("Bearer {t}"));
            }
        }
        let resp = req.send().await?;
        let status = resp.status();
        if status == reqwest::StatusCode::UNAUTHORIZED || status == reqwest::StatusCode::FORBIDDEN {
            return Err(HfError::GatedOrUnauthorized {
                repo_id: repo_id.to_string(),
                status: status.as_u16(),
            });
        }
        if status == reqwest::StatusCode::NOT_FOUND {
            return Err(HfError::NotFound(repo_id.to_string()));
        }
        if !status.is_success() {
            let text = resp.text().await.unwrap_or_default();
            return Err(HfError::Api {
                status: status.as_u16(),
                message: text,
            });
        }
        let detail = resp.json::<HfModelDetail>().await?;
        Ok(detail)
    }

    /// Search for GGUF models on Hugging Face.
    pub async fn search_models(
        &self,
        query: &str,
        limit: usize,
    ) -> Result<Vec<HfModelSummary>, HfError> {
        let base = self.base_url.as_deref().unwrap_or("https://huggingface.co");
        let encoded = urlencoding_simple(query);
        let url = format!(
            "{base}/api/models?search={}&filter=gguf&sort=downloads&direction=-1&limit={}",
            encoded, limit
        );
        let mut req = self.client.get(&url);
        if let Some(tok) = &self.token {
            let t = tok.trim();
            if !t.is_empty() {
                req = req.header("Authorization", format!("Bearer {t}"));
            }
        }
        let resp = req.send().await?;
        let status = resp.status();
        if !status.is_success() {
            let text = resp.text().await.unwrap_or_default();
            return Err(HfError::Api {
                status: status.as_u16(),
                message: text,
            });
        }
        let list = resp.json::<Vec<HfModelSummary>>().await?;
        Ok(list)
    }

    /// Fetch trending GGUF models on Hugging Face.
    pub async fn trending_models(&self, limit: usize) -> Result<Vec<HfModelSummary>, HfError> {
        let base = self.base_url.as_deref().unwrap_or("https://huggingface.co");
        let url = format!(
            "{base}/api/models?filter=gguf&sort=trendingScore&direction=-1&limit={}",
            limit
        );
        let mut req = self.client.get(&url);
        if let Some(tok) = &self.token {
            let t = tok.trim();
            if !t.is_empty() {
                req = req.header("Authorization", format!("Bearer {t}"));
            }
        }
        let resp = req.send().await?;
        let status = resp.status();
        if !status.is_success() {
            let text = resp.text().await.unwrap_or_default();
            return Err(HfError::Api {
                status: status.as_u16(),
                message: text,
            });
        }
        let list = resp.json::<Vec<HfModelSummary>>().await?;
        Ok(list)
    }

    /// Parse siblings into grouped GGUF files with shard aggregation and memory fit badges.
    pub fn parse_gguf_groups(
        detail: &HfModelDetail,
        available_ram_mb: u64,
        cluster_free_mb: u64,
    ) -> Vec<HfGgufGroup> {
        let mut grouped: BTreeMap<String, Vec<HfGgufFile>> = BTreeMap::new();

        for sibling in &detail.siblings {
            if !sibling.rfilename.to_ascii_lowercase().ends_with(".gguf") {
                continue;
            }
            let size = sibling
                .size
                .or_else(|| sibling.lfs.as_ref().and_then(|l| l.size))
                .unwrap_or(0);
            let sha256 = sibling
                .lfs
                .as_ref()
                .and_then(|l| l.sha256.clone().or_else(|| l.oid.clone()));
            let download_url = format!(
                "https://huggingface.co/{}/resolve/main/{}",
                detail.id, sibling.rfilename
            );

            let base_name = extract_base_group_name(&sibling.rfilename);
            let file = HfGgufFile {
                filename: sibling.rfilename.clone(),
                size_bytes: size,
                sha256,
                download_url,
            };

            grouped.entry(base_name).or_default().push(file);
        }

        let mut result = Vec::new();

        for (base_name, mut files) in grouped {
            files.sort_by(|a, b| a.filename.cmp(&b.filename));
            let total_size_bytes: u64 = files.iter().map(|f| f.size_bytes).sum();
            let is_sharded = files.len() > 1;
            let quant_label = extract_quant_label(&base_name);
            let fit_status =
                calculate_fit_status(total_size_bytes, available_ram_mb, cluster_free_mb);

            result.push(HfGgufGroup {
                base_name,
                quant_label,
                total_size_bytes,
                files,
                is_sharded,
                fit_status,
            });
        }

        // Sort groups: fits first, then by size ascending
        result.sort_by(|a, b| {
            let fit_rank = |f: FitStatus| match f {
                FitStatus::Fits => 0,
                FitStatus::OffloadRequired => 1,
                FitStatus::Exceeds => 2,
            };
            fit_rank(a.fit_status)
                .cmp(&fit_rank(b.fit_status))
                .then_with(|| a.total_size_bytes.cmp(&b.total_size_bytes))
        });

        result
    }
}

/// Calculate memory fit status based on model size and node/cluster memory limits.
pub fn calculate_fit_status(
    size_bytes: u64,
    available_ram_mb: u64,
    cluster_free_mb: u64,
) -> FitStatus {
    let size_mb = size_bytes / (1024 * 1024);
    let estimated_kv_mb = 512;
    let required_mb = size_mb + estimated_kv_mb;
    let safe_local_cap = (available_ram_mb as f64 * 0.75) as u64;

    if required_mb <= safe_local_cap {
        FitStatus::Fits
    } else if required_mb <= cluster_free_mb {
        FitStatus::OffloadRequired
    } else {
        FitStatus::Exceeds
    }
}

/// Extract base name stripping shard indicators like `-00001-of-00003.gguf`.
fn extract_base_group_name(filename: &str) -> String {
    let lower = filename.to_ascii_lowercase();
    if let Some(pos) = lower.find("-0000") {
        let rest = &lower[pos..];
        if rest.contains("-of-000") {
            return filename[..pos].to_string();
        }
    }
    // Strip trailing .gguf
    if let Some(stripped) = filename
        .strip_suffix(".gguf")
        .or_else(|| filename.strip_suffix(".GGUF"))
    {
        stripped.to_string()
    } else {
        filename.to_string()
    }
}

/// Extract known quantization tag (e.g. Q4_K_M, Q8_0, IQ3_M, FP16) from a model name.
pub fn extract_quant_label(name: &str) -> String {
    let candidates = [
        "IQ1_S", "IQ1_M", "IQ2_XXS", "IQ2_XS", "IQ2_S", "IQ2_M", "IQ3_XXS", "IQ3_XS", "IQ3_S",
        "IQ3_M", "IQ4_XS", "IQ4_NL", "Q2_K_S", "Q2_K", "Q3_K_S", "Q3_K_M", "Q3_K_L", "Q4_0",
        "Q4_1", "Q4_K_S", "Q4_K_M", "Q4_K", "Q5_0", "Q5_1", "Q5_K_S", "Q5_K_M", "Q5_K", "Q6_K",
        "Q8_0", "Q8_1", "Q8_K", "BF16", "FP16", "F16", "FP32", "F32",
    ];

    let upper = name.to_ascii_uppercase();
    for cand in candidates {
        if upper.contains(cand) {
            return cand.to_string();
        }
    }

    // Fallback: extract last segment delimited by hyphen, underscore, or period
    name.rsplit(['-', '_', '.'])
        .next()
        .unwrap_or("GGUF")
        .to_string()
}

fn urlencoding_simple(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char);
            }
            b' ' => out.push('+'),
            _ => {
                use std::fmt::Write;
                let _ = write!(out, "%{:02X}", b);
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_repo_id() {
        assert_eq!(
            HfClient::parse_repo_id("bartowski/Llama-3.2-3B-Instruct-GGUF"),
            Some("bartowski/Llama-3.2-3B-Instruct-GGUF".to_string())
        );
        assert_eq!(
            HfClient::parse_repo_id("https://huggingface.co/bartowski/Llama-3.2-3B-Instruct-GGUF"),
            Some("bartowski/Llama-3.2-3B-Instruct-GGUF".to_string())
        );
        assert_eq!(
            HfClient::parse_repo_id("https://huggingface.co/bartowski/Llama-3.2-3B-Instruct-GGUF/"),
            Some("bartowski/Llama-3.2-3B-Instruct-GGUF".to_string())
        );
        assert_eq!(
            HfClient::parse_repo_id(
                "https://huggingface.co/bartowski/Llama-3.2-3B-Instruct-GGUF/tree/main"
            ),
            Some("bartowski/Llama-3.2-3B-Instruct-GGUF".to_string())
        );
        // Direct file links should be rejected so they download directly
        assert_eq!(
            HfClient::parse_repo_id(
                "https://huggingface.co/bartowski/Llama-3.2-3B-Instruct-GGUF/resolve/main/model.gguf"
            ),
            None
        );
        assert_eq!(
            HfClient::parse_repo_id("https://hf.co/bartowski/Llama-3.2-3B-Instruct-GGUF"),
            Some("bartowski/Llama-3.2-3B-Instruct-GGUF".to_string())
        );
        assert_eq!(
            HfClient::parse_repo_id("hf.co/bartowski/Llama-3.2-3B-Instruct-GGUF"),
            Some("bartowski/Llama-3.2-3B-Instruct-GGUF".to_string())
        );
        assert_eq!(
            HfClient::parse_repo_id(
                "https://hf.co/bartowski/Llama-3.2-3B-Instruct-GGUF/blob/main/model.gguf"
            ),
            None
        );
        assert_eq!(
            HfClient::parse_repo_id("https://example.com/other/model.gguf"),
            None
        );
        assert_eq!(HfClient::parse_repo_id("local-file.gguf"), None);
        assert!(!curated_starter_models().is_empty());
    }

    #[test]
    fn test_extract_quant_label() {
        assert_eq!(
            extract_quant_label("Llama-3.2-3B-Instruct-Q4_K_M.gguf"),
            "Q4_K_M"
        );
        assert_eq!(
            extract_quant_label("DeepSeek-R1-Q8_0-00001-of-00003"),
            "Q8_0"
        );
        assert_eq!(extract_quant_label("Qwen2.5-Coder-7B-BF16"), "BF16");
    }

    #[test]
    fn test_parse_gguf_groups_and_shards() {
        let detail = HfModelDetail {
            id: "bartowski/test-model-GGUF".to_string(),
            author: Some("bartowski".to_string()),
            downloads: 1000,
            likes: 50,
            private: false,
            gated: None,
            siblings: vec![
                HfSibling {
                    rfilename: "test-model-Q4_K_M.gguf".to_string(),
                    size: Some(2_000_000_000),
                    lfs: Some(HfLfs {
                        size: Some(2_000_000_000),
                        sha256: Some("sha_q4".to_string()),
                        oid: None,
                    }),
                },
                HfSibling {
                    rfilename: "test-model-Q8_0-00001-of-00002.gguf".to_string(),
                    size: Some(1_800_000_000),
                    lfs: Some(HfLfs {
                        size: Some(1_800_000_000),
                        sha256: Some("sha_q8_1".to_string()),
                        oid: None,
                    }),
                },
                HfSibling {
                    rfilename: "test-model-Q8_0-00002-of-00002.gguf".to_string(),
                    size: Some(1_700_000_000),
                    lfs: Some(HfLfs {
                        size: Some(1_700_000_000),
                        sha256: Some("sha_q8_2".to_string()),
                        oid: None,
                    }),
                },
                HfSibling {
                    rfilename: "README.md".to_string(),
                    size: Some(5000),
                    lfs: None,
                },
            ],
        };

        // Available RAM 4000 MB (safe cap 3000 MB)
        let groups = HfClient::parse_gguf_groups(&detail, 4000, 8000);
        assert_eq!(groups.len(), 2, "Should only group .gguf files");

        let q4 = groups.iter().find(|g| g.quant_label == "Q4_K_M").unwrap();
        assert_eq!(q4.files.len(), 1);
        assert!(!q4.is_sharded);
        assert_eq!(q4.total_size_bytes, 2_000_000_000);
        assert_eq!(q4.fit_status, FitStatus::Fits);

        let q8 = groups.iter().find(|g| g.quant_label == "Q8_0").unwrap();
        assert_eq!(q8.files.len(), 2);
        assert!(q8.is_sharded);
        assert_eq!(q8.total_size_bytes, 3_500_000_000);
        // 3.5GB + 512MB = ~3850MB > 3000MB cap, but <= 8000MB cluster
        assert_eq!(q8.fit_status, FitStatus::OffloadRequired);
    }
}
