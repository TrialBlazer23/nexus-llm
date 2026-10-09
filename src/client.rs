use crate::discovery::{DiscoveryError, DiscoveryService};
use eventsource_stream::Eventsource;
use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use std::pin::Pin;
use std::time::Duration;
use thiserror::Error;
use tokio_stream::Stream;
use tracing::{debug, info};

pub type TokenStream = Pin<Box<dyn Stream<Item = Result<String, ClientError>> + Send>>;

#[derive(Error, Debug)]
pub enum ClientError {
    #[error("HTTP client error: {0}")]
    Reqwest(#[from] reqwest::Error),

    #[error("Discovery error: {0}")]
    Discovery(#[from] DiscoveryError),

    #[error("JSON serialization error: {0}")]
    Json(#[from] serde_json::Error),

    #[error("API request failed with status {status}: {message}")]
    ApiError {
        status: reqwest::StatusCode,
        message: String,
    },

    #[error("EventSource stream error: {0}")]
    EventSource(String),

    #[error("Discovery resolution timed out after {0:?}")]
    DiscoveryTimeout(Duration),
}

/// Chat message with standard role definitions.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ChatMessage {
    pub role: String,
    pub content: String,
}

impl ChatMessage {
    pub fn system(content: impl Into<String>) -> Self {
        Self {
            role: "system".to_string(),
            content: content.into(),
        }
    }

    pub fn user(content: impl Into<String>) -> Self {
        Self {
            role: "user".to_string(),
            content: content.into(),
        }
    }

    pub fn assistant(content: impl Into<String>) -> Self {
        Self {
            role: "assistant".to_string(),
            content: content.into(),
        }
    }

    /// UI-only chrome (load/connect banners). Never sent to the inference API.
    pub fn status(content: impl Into<String>) -> Self {
        Self {
            role: "status".to_string(),
            content: content.into(),
        }
    }

    pub fn is_status(&self) -> bool {
        self.role == "status"
    }
}

/// Request payload for OpenAI-compatible /v1/chat/completions endpoint.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatCompletionRequest {
    pub model: String,
    pub messages: Vec<ChatMessage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub top_p: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<usize>,
    #[serde(default)]
    pub stream: bool,
}

/// Non-streaming response payload.
#[derive(Debug, Clone, Deserialize)]
pub struct ChatCompletionResponse {
    pub id: Option<String>,
    pub choices: Vec<ChatCompletionChoice>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ChatCompletionChoice {
    pub index: usize,
    pub message: ChatMessage,
    pub finish_reason: Option<String>,
}

/// Server-Sent Events (SSE) streaming chunk payload.
#[derive(Debug, Clone, Deserialize)]
pub struct ChatCompletionChunk {
    pub id: Option<String>,
    pub choices: Vec<ChatCompletionChunkChoice>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ChatCompletionChunkChoice {
    #[serde(default)]
    pub index: Option<usize>,
    #[serde(default)]
    pub delta: ChatCompletionChunkDelta,
    pub finish_reason: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct ChatCompletionChunkDelta {
    pub role: Option<String>,
    pub content: Option<String>,
    pub reasoning_content: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SseErrorChunk {
    pub error: SseErrorDetail,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SseErrorDetail {
    pub message: String,
    #[serde(default)]
    pub code: Option<serde_json::Value>,
    #[serde(rename = "type", default)]
    pub error_type: Option<String>,
}

/// OpenAI-compatible HTTP REST and SSE streaming client for Nexus-LLM.
#[derive(Debug, Clone)]
pub struct NexusClient {
    endpoint: String,
    client: reqwest::Client,
}

impl NexusClient {
    /// Create client targeting a known base URL (e.g., "http://192.168.1.50:8080").
    pub fn new(endpoint: impl Into<String>) -> Self {
        let endpoint = endpoint.into().trim_end_matches('/').to_string();
        Self {
            endpoint,
            client: reqwest::Client::builder()
                .connect_timeout(Duration::from_secs(10))
                .timeout(Duration::from_secs(300))
                .build()
                .expect("Valid reqwest client"),
        }
    }

    /// Automatically discover compute host on the local subnet and construct a client.
    ///
    /// Resolution order: pinned ready primary-compute anchor → `find_best_host` →
    /// static peer `/health` probes. Times out with `DiscoveryTimeout` if none succeed.
    pub async fn resolve_from_discovery(
        discovery: &DiscoveryService,
        timeout: Duration,
    ) -> Result<Self, ClientError> {
        info!(
            "Resolving compute host via autonomous discovery (timeout: {:?})...",
            timeout
        );
        let start = tokio::time::Instant::now();

        while start.elapsed() < timeout {
            if let Some(host) = discovery.resolve_primary_compute_anchor().await {
                info!(
                    "Discovered pinned anchor host: {} ({}) at {} (Model: {:?}, Vulkan: {})",
                    host.label(),
                    host.uuid,
                    host.api_endpoint(),
                    host.active_model,
                    host.status.is_vulkan_active()
                );
                return Ok(Self::new(host.api_endpoint()));
            }

            if let Some(host) = discovery.find_best_trusted_host().await {
                info!(
                    "Discovered trusted host: {} ({}) at {} (Model: {:?}, Vulkan: {})",
                    host.label(),
                    host.uuid,
                    host.api_endpoint(),
                    host.active_model,
                    host.status.is_vulkan_active()
                );
                return Ok(Self::new(host.api_endpoint()));
            }

            // Probe any configured static peers via HTTP /health if UDP broadcast was blocked
            for peer in &discovery.config().network.static_peers {
                let endpoint = if peer.starts_with("http://") || peer.starts_with("https://") {
                    peer.clone()
                } else if peer.contains(':') {
                    format!("http://{}", peer)
                } else {
                    format!("http://{}:{}", peer, discovery.config().network.api_port)
                };
                let test_client = Self::new(&endpoint);
                if let Ok(true) = test_client.health().await {
                    info!(
                        "Discovered active host via static peer health check: {}",
                        endpoint
                    );
                    return Ok(test_client);
                }
            }

            tokio::time::sleep(Duration::from_millis(250)).await;
        }

        Err(ClientError::DiscoveryTimeout(timeout))
    }

    pub fn endpoint(&self) -> &str {
        &self.endpoint
    }

    /// Health check /health.
    pub async fn health(&self) -> Result<bool, ClientError> {
        let url = format!("{}/health", self.endpoint);
        let resp = self.client.get(&url).send().await?;
        Ok(resp.status().is_success())
    }

    /// Fetch loaded models list from /v1/models.
    pub async fn models(&self) -> Result<Vec<String>, ClientError> {
        let url = format!("{}/v1/models", self.endpoint);
        let resp = self.client.get(&url).send().await?;
        if !resp.status().is_success() {
            return Err(ClientError::ApiError {
                status: resp.status(),
                message: resp.text().await.unwrap_or_default(),
            });
        }

        #[derive(Deserialize)]
        struct ModelItem {
            id: String,
        }
        #[derive(Deserialize)]
        struct ModelsResponse {
            data: Vec<ModelItem>,
        }

        let body: ModelsResponse = resp.json().await?;
        Ok(body.data.into_iter().map(|m| m.id).collect())
    }

    /// Execute a non-streaming chat completion request.
    pub async fn complete_chat(
        &self,
        mut req: ChatCompletionRequest,
    ) -> Result<String, ClientError> {
        req.stream = false;
        let url = format!("{}/v1/chat/completions", self.endpoint);

        let resp = self.client.post(&url).json(&req).send().await?;
        if !resp.status().is_success() {
            return Err(ClientError::ApiError {
                status: resp.status(),
                message: resp.text().await.unwrap_or_default(),
            });
        }

        let body: ChatCompletionResponse = resp.json().await?;
        let content = body
            .choices
            .first()
            .map(|c| c.message.content.clone())
            .unwrap_or_default();

        Ok(content)
    }

    /// Execute a streaming chat completion request yielding incremental text tokens over SSE.
    pub async fn stream_chat(
        &self,
        mut req: ChatCompletionRequest,
    ) -> Result<TokenStream, ClientError> {
        req.stream = true;
        let url = format!("{}/v1/chat/completions", self.endpoint);

        let resp = self.client.post(&url).json(&req).send().await?;
        if !resp.status().is_success() {
            return Err(ClientError::ApiError {
                status: resp.status(),
                message: resp.text().await.unwrap_or_default(),
            });
        }

        let stream = resp
            .bytes_stream()
            .eventsource()
            .filter_map(|event_result| async move {
                match event_result {
                    Ok(event) => {
                        let data = event.data.trim();
                        if data == "[DONE]" || data.is_empty() {
                            None
                        } else {
                            match serde_json::from_str::<ChatCompletionChunk>(data) {
                                Ok(chunk) => {
                                    if let Some(first_choice) = chunk.choices.first() {
                                        if let Some(content) = &first_choice.delta.content {
                                            if !content.is_empty() {
                                                return Some(Ok(content.clone()));
                                            }
                                        }
                                        if let Some(reasoning) =
                                            &first_choice.delta.reasoning_content
                                        {
                                            if !reasoning.is_empty() {
                                                return Some(Ok(reasoning.clone()));
                                            }
                                        }
                                    }
                                    None
                                }
                                Err(e) => {
                                    if let Ok(err_chunk) =
                                        serde_json::from_str::<SseErrorChunk>(data)
                                    {
                                        return Some(Err(ClientError::ApiError {
                                            status: reqwest::StatusCode::INTERNAL_SERVER_ERROR,
                                            message: err_chunk.error.message,
                                        }));
                                    }
                                    debug!("Failed to parse SSE chunk: {} (data: {})", e, data);
                                    None
                                }
                            }
                        }
                    }
                    Err(e) => Some(Err(ClientError::EventSource(e.to_string()))),
                }
            });

        Ok(Box::pin(stream))
    }

    /// Request float embeddings for `input` using `model`.
    pub async fn create_embedding(
        &self,
        model: &str,
        input: &str,
    ) -> Result<Vec<f32>, ClientError> {
        let url = format!("{}/v1/embeddings", self.endpoint);
        let req = EmbeddingRequest {
            model: model.to_string(),
            input: input.to_string(),
        };

        let resp = self.client.post(&url).json(&req).send().await?;
        if !resp.status().is_success() {
            return Err(ClientError::ApiError {
                status: resp.status(),
                message: resp.text().await.unwrap_or_default(),
            });
        }

        let body: EmbeddingResponse = resp.json().await?;
        let embedding = body
            .data
            .into_iter()
            .next()
            .map(|d| d.embedding)
            .ok_or_else(|| ClientError::ApiError {
                status: reqwest::StatusCode::OK,
                message: "Empty embedding data array returned by API".to_string(),
            })?;

        Ok(embedding)
    }
}

/// Request payload for OpenAI-compatible /v1/embeddings endpoint.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EmbeddingRequest {
    pub model: String,
    pub input: String,
}

/// Response payload from /v1/embeddings endpoint.
#[derive(Debug, Clone, Deserialize)]
pub struct EmbeddingResponse {
    pub data: Vec<EmbeddingData>,
    #[serde(default)]
    pub model: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct EmbeddingData {
    pub embedding: Vec<f32>,
    #[serde(default)]
    pub index: usize,
}
