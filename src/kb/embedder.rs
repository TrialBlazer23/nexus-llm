//! Distributed embeddings client and zero-inference pseudo-embedder.

use crate::kb::vector::normalize_vector;
use crate::kb::KbError;
use serde::{Deserialize, Serialize};
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::time::Duration;

pub const DEFAULT_PSEUDO_DIMENSION: usize = 128;

/// Request envelope for OpenAI-compatible /v1/embeddings endpoint.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EmbeddingApiRequest {
    pub model: String,
    pub input: String,
}

/// Response envelope from /v1/embeddings endpoint.
#[derive(Debug, Clone, Deserialize)]
pub struct EmbeddingApiResponse {
    pub data: Vec<EmbeddingApiData>,
    #[serde(default)]
    pub model: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct EmbeddingApiData {
    pub embedding: Vec<f32>,
    #[serde(default)]
    pub index: usize,
}

/// Deterministic, zero-inference pseudo-embedder using token hashing.
///
/// Ensures RAG vector indexing and similarity search are fully operational
/// even on low-RAM nodes without an active embedding GGUF model loaded.
#[derive(Debug, Clone)]
pub struct FastPseudoEmbedder {
    dimension: usize,
}

impl Default for FastPseudoEmbedder {
    fn default() -> Self {
        Self::new(DEFAULT_PSEUDO_DIMENSION)
    }
}

impl FastPseudoEmbedder {
    pub fn new(dimension: usize) -> Self {
        Self {
            dimension: dimension.max(16),
        }
    }

    /// Generate a normalized deterministic embedding vector for `text`.
    pub fn embed(&self, text: &str) -> Vec<f32> {
        let mut vector = vec![0.0f32; self.dimension];
        let lower = text.to_lowercase();
        let tokens: Vec<&str> = lower
            .split(|c: char| !c.is_alphanumeric())
            .filter(|s| !s.is_empty())
            .collect();

        if tokens.is_empty() {
            return vector;
        }

        // 1. Unigram word hashing
        for token in &tokens {
            let mut hasher = DefaultHasher::new();
            token.hash(&mut hasher);
            let idx = (hasher.finish() as usize) % self.dimension;
            vector[idx] += 1.0;
        }

        // 2. Character 3-gram hashing for subword captures
        let chars: Vec<char> = lower.chars().collect();
        if chars.len() >= 3 {
            for window in chars.windows(3) {
                let mut hasher = DefaultHasher::new();
                for c in window {
                    c.hash(&mut hasher);
                }
                let idx = (hasher.finish() as usize) % self.dimension;
                vector[idx] += 0.5;
            }
        }

        normalize_vector(&mut vector);
        vector
    }
}

/// Remote HTTP embedding client querying an OpenAI-compatible `/v1/embeddings` endpoint.
#[derive(Debug, Clone)]
pub struct RemoteEmbedder {
    endpoint: String,
    model: String,
    client: reqwest::Client,
}

impl RemoteEmbedder {
    pub fn new(endpoint: impl Into<String>, model: impl Into<String>) -> Self {
        Self {
            endpoint: endpoint.into().trim_end_matches('/').to_string(),
            model: model.into(),
            client: reqwest::Client::builder()
                .timeout(Duration::from_secs(30))
                .build()
                .unwrap_or_default(),
        }
    }

    pub async fn embed(&self, text: &str) -> Result<Vec<f32>, KbError> {
        let url = format!("{}/v1/embeddings", self.endpoint);
        let req = EmbeddingApiRequest {
            model: self.model.clone(),
            input: text.to_string(),
        };

        let resp = self
            .client
            .post(&url)
            .json(&req)
            .send()
            .await
            .map_err(|e| KbError::Embedding(format!("HTTP request failed: {e}")))?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(KbError::Embedding(format!(
                "API returned error status {status}: {body}"
            )));
        }

        let parsed: EmbeddingApiResponse = resp
            .json()
            .await
            .map_err(|e| KbError::Embedding(format!("Failed to parse response: {e}")))?;

        let mut vector = parsed
            .data
            .into_iter()
            .next()
            .map(|d| d.embedding)
            .ok_or_else(|| KbError::Embedding("Empty embedding data in response".to_string()))?;

        normalize_vector(&mut vector);
        Ok(vector)
    }
}

/// Unified embedder enum supporting both local pseudo-embedding and remote model endpoints.
#[derive(Debug, Clone)]
pub enum Embedder {
    Pseudo(FastPseudoEmbedder),
    Remote(RemoteEmbedder),
}

impl Default for Embedder {
    fn default() -> Self {
        Self::Pseudo(FastPseudoEmbedder::default())
    }
}

impl Embedder {
    pub async fn embed(&self, text: &str) -> Result<Vec<f32>, KbError> {
        match self {
            Self::Pseudo(p) => Ok(p.embed(text)),
            Self::Remote(r) => r.embed(text).await,
        }
    }

    pub fn embed_sync(&self, text: &str) -> Vec<f32> {
        match self {
            Self::Pseudo(p) => p.embed(text),
            Self::Remote(_) => FastPseudoEmbedder::default().embed(text),
        }
    }
}
