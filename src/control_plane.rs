use crate::discovery::{NodeRole, ServiceEndpoint};
use crate::node_identity::NodeIdentity;
use crate::trust_auth::{apply_auth_headers, AuthError};
use reqwest::Url;
use serde::{Deserialize, Serialize};
use std::time::Duration;
use thiserror::Error;
use tracing::info;
use uuid::Uuid;

pub const CONTROL_PLANE_VERSION: u16 = 1;
pub const MAX_CONTROL_RESPONSE_BYTES: usize = 1024 * 1024;

/// Information about a model loaded in a supervisor slot on a node.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct LoadedModelInfo {
    pub model: String,
    pub endpoint: String,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default)]
    pub ctx_remaining: usize,
    #[serde(default)]
    pub memory_mb: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ControlPlaneState {
    pub node_id: Uuid,
    pub protocol_version: u16,
    pub role: NodeRole,
    pub capabilities: Vec<String>,
    pub ready: bool,
    pub inferring: bool,
    pub rpc_ready: bool,
    pub allocatable_memory_mb: u64,
    pub active_model: Option<String>,
    #[serde(default)]
    pub loaded_models: Vec<LoadedModelInfo>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signing_public_key: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ControlPlaneRequest {
    pub protocol_version: u16,
    pub requester_id: Uuid,
}

#[derive(Debug, Error)]
pub enum ControlPlaneError {
    #[error("control-plane request failed: {0}")]
    Request(#[from] reqwest::Error),
    #[error("control-plane response exceeded {limit} bytes")]
    ResponseTooLarge { limit: usize },
    #[error("invalid control-plane response: {0}")]
    InvalidResponse(String),
    #[error("control-plane protocol mismatch: expected {expected}, got {actual}")]
    ProtocolMismatch { expected: u16, actual: u16 },
    #[error("control-plane identity mismatch: expected {expected}, got {actual}")]
    IdentityMismatch { expected: Uuid, actual: Uuid },
    #[error("control-plane auth failed: {0}")]
    Auth(#[from] AuthError),
    #[error("control-plane HTTP {status}: {body}")]
    HttpStatus { status: u16, body: String },
}

pub fn validate_state(
    state: &ControlPlaneState,
    expected_node_id: Uuid,
    expected_protocol: u16,
    max_allocatable_memory_mb: u64,
) -> Result<(), ControlPlaneError> {
    if state.node_id != expected_node_id {
        return Err(ControlPlaneError::IdentityMismatch {
            expected: expected_node_id,
            actual: state.node_id,
        });
    }
    if state.protocol_version != expected_protocol {
        return Err(ControlPlaneError::ProtocolMismatch {
            expected: expected_protocol,
            actual: state.protocol_version,
        });
    }
    if state.allocatable_memory_mb > max_allocatable_memory_mb {
        return Err(ControlPlaneError::InvalidResponse(format!(
            "allocatable memory {} MB exceeds policy cap {} MB",
            state.allocatable_memory_mb, max_allocatable_memory_mb
        )));
    }
    if state.rpc_ready && !state.ready {
        return Err(ControlPlaneError::InvalidResponse(
            "rpc_ready requires ready".to_string(),
        ));
    }
    if state.capabilities.len() > 64
        || state
            .capabilities
            .iter()
            .any(|capability| capability.len() > 128)
    {
        return Err(ControlPlaneError::InvalidResponse(
            "capability metadata exceeds bounds".to_string(),
        ));
    }
    Ok(())
}

pub fn endpoint_from_state(
    state: &ControlPlaneState,
    endpoint: &ServiceEndpoint,
) -> ServiceEndpoint {
    ServiceEndpoint {
        node_id: state.node_id,
        cluster_id: endpoint.cluster_id,
        protocol_version: state.protocol_version,
        role: state.role,
        capabilities: state.capabilities.clone(),
        addresses: endpoint.addresses.clone(),
        api_port: endpoint.api_port,
        rpc_port: if state.rpc_ready {
            endpoint.rpc_port
        } else {
            0
        },
        control_port: endpoint.control_port,
        display_name: endpoint.display_name.clone(),
    }
}

/// Build a control-plane state snapshot from live supervisor + discovery signals.
pub async fn build_control_plane_state(
    node_id: Uuid,
    role: NodeRole,
    capabilities: Vec<String>,
    manager: &crate::supervisor::SupervisorManager,
    allocatable_memory_mb: u64,
    rpc_ready: bool,
    signing_public_key: Option<String>,
) -> ControlPlaneState {
    build_control_plane_state_with_host(
        node_id,
        role,
        capabilities,
        manager,
        allocatable_memory_mb,
        rpc_ready,
        signing_public_key,
        "127.0.0.1",
    )
    .await
}

/// Build a control-plane state snapshot with explicit api_host for loaded model endpoints.
#[allow(clippy::too_many_arguments)]
pub async fn build_control_plane_state_with_host(
    node_id: Uuid,
    role: NodeRole,
    capabilities: Vec<String>,
    manager: &crate::supervisor::SupervisorManager,
    allocatable_memory_mb: u64,
    rpc_ready: bool,
    signing_public_key: Option<String>,
    api_host: &str,
) -> ControlPlaneState {
    let healthy = manager.is_healthy().await;
    let active_model = manager.active_model().await;
    let slots = manager.slots_info().await;
    let loaded_models = slots
        .into_iter()
        .map(|slot| LoadedModelInfo {
            model: slot.model_name,
            endpoint: format!("http://{}:{}", api_host, slot.port),
            tags: slot.tags,
            ctx_remaining: slot.context_size,
            memory_mb: slot.memory_bytes / (1024 * 1024),
        })
        .collect();

    ControlPlaneState {
        node_id,
        protocol_version: CONTROL_PLANE_VERSION,
        role,
        capabilities,
        ready: true,
        inferring: healthy,
        rpc_ready,
        allocatable_memory_mb,
        active_model,
        loaded_models,
        signing_public_key,
    }
}

async fn signed_post_bytes(
    client: &reqwest::Client,
    base_url: &str,
    path: &str,
    body: &[u8],
    identity: Option<&NodeIdentity>,
    signer_id: Uuid,
) -> Result<reqwest::Response, ControlPlaneError> {
    let mut url = Url::parse(base_url)
        .map_err(|error| ControlPlaneError::InvalidResponse(error.to_string()))?;
    url.set_path(path);
    let mut req = client.post(url).body(body.to_vec());
    if let Some(id) = identity {
        req = apply_auth_headers(req, id, signer_id, "POST", path, body);
    }
    let response = req.send().await?;
    if !response.status().is_success() {
        let status = response.status().as_u16();
        let body = response.text().await.unwrap_or_default();
        return Err(ControlPlaneError::HttpStatus { status, body });
    }
    Ok(response)
}

pub async fn fetch_state(
    client: &reqwest::Client,
    base_url: &str,
    request: &ControlPlaneRequest,
) -> Result<ControlPlaneState, ControlPlaneError> {
    let mut url = Url::parse(base_url)
        .map_err(|error| ControlPlaneError::InvalidResponse(error.to_string()))?;
    url.set_path("/nexus/control/v1/state");
    let response = client
        .post(url)
        .timeout(Duration::from_secs(2))
        .json(request)
        .send()
        .await?
        .error_for_status()?;
    if response
        .content_length()
        .is_some_and(|length| length > MAX_CONTROL_RESPONSE_BYTES as u64)
    {
        return Err(ControlPlaneError::ResponseTooLarge {
            limit: MAX_CONTROL_RESPONSE_BYTES,
        });
    }
    let bytes = response.bytes().await?;
    if bytes.len() > MAX_CONTROL_RESPONSE_BYTES {
        return Err(ControlPlaneError::ResponseTooLarge {
            limit: MAX_CONTROL_RESPONSE_BYTES,
        });
    }
    serde_json::from_slice(&bytes)
        .map_err(|error| ControlPlaneError::InvalidResponse(error.to_string()))
}

pub async fn fetch_state_signed(
    client: &reqwest::Client,
    base_url: &str,
    request: &ControlPlaneRequest,
    identity: &NodeIdentity,
    signer_id: Uuid,
) -> Result<ControlPlaneState, ControlPlaneError> {
    let body = serde_json::to_vec(request)
        .map_err(|e| ControlPlaneError::InvalidResponse(e.to_string()))?;
    let response = signed_post_bytes(
        client,
        base_url,
        "/nexus/control/v1/state",
        &body,
        Some(identity),
        signer_id,
    )
    .await?;
    let bytes = response.bytes().await?;
    if bytes.len() > MAX_CONTROL_RESPONSE_BYTES {
        return Err(ControlPlaneError::ResponseTooLarge {
            limit: MAX_CONTROL_RESPONSE_BYTES,
        });
    }
    serde_json::from_slice(&bytes)
        .map_err(|error| ControlPlaneError::InvalidResponse(error.to_string()))
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PairRequest {
    pub protocol_version: u16,
    pub requester_id: Uuid,
    pub requester_public_key: String,
    pub pairing_code: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PairResponse {
    pub protocol_version: u16,
    pub success: bool,
    pub node_id: Uuid,
    pub public_key: String,
    pub message: String,
}

pub async fn dispatch_pair(
    client: &reqwest::Client,
    base_url: &str,
    request: &PairRequest,
    identity: &NodeIdentity,
) -> Result<PairResponse, ControlPlaneError> {
    let body = serde_json::to_vec(request)
        .map_err(|e| ControlPlaneError::InvalidResponse(e.to_string()))?;
    let response = signed_post_bytes(
        client,
        base_url,
        "/nexus/control/v1/pair",
        &body,
        Some(identity),
        request.requester_id,
    )
    .await?;
    let bytes = response.bytes().await?;
    serde_json::from_slice(&bytes)
        .map_err(|error| ControlPlaneError::InvalidResponse(error.to_string()))
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ModelCatalogEntry {
    pub filename: String,
    pub size_mb: u64,
    pub architecture: String,
    pub context_length: usize,
    /// Lowercase hex SHA-256 of the model bytes (empty if unknown).
    #[serde(default)]
    pub digest: String,
    #[serde(default)]
    pub size_bytes: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ModelCatalogResponse {
    pub protocol_version: u16,
    pub node_id: Uuid,
    pub models: Vec<ModelCatalogEntry>,
}

/// Build a catalog response from the content-addressed model index.
pub fn build_model_catalog(node_id: Uuid, models_dir: &std::path::Path) -> ModelCatalogResponse {
    let index = crate::store::ModelIndex::reconcile_default(models_dir).unwrap_or_default();
    let models = index
        .models
        .into_iter()
        .map(|m| ModelCatalogEntry {
            filename: m.filename,
            size_mb: m.size_bytes / (1024 * 1024),
            architecture: m.architecture,
            context_length: m.context_length,
            digest: m.digest,
            size_bytes: m.size_bytes,
        })
        .collect();
    ModelCatalogResponse {
        protocol_version: CONTROL_PLANE_VERSION,
        node_id,
        models,
    }
}

pub async fn fetch_models(
    client: &reqwest::Client,
    base_url: &str,
) -> Result<ModelCatalogResponse, ControlPlaneError> {
    let mut url = Url::parse(base_url)
        .map_err(|error| ControlPlaneError::InvalidResponse(error.to_string()))?;
    url.set_path("/nexus/control/v1/models");
    let response = client
        .get(url)
        .timeout(Duration::from_secs(3))
        .send()
        .await?
        .error_for_status()?;
    let bytes = response.bytes().await?;
    if bytes.len() > MAX_CONTROL_RESPONSE_BYTES {
        return Err(ControlPlaneError::ResponseTooLarge {
            limit: MAX_CONTROL_RESPONSE_BYTES,
        });
    }
    serde_json::from_slice(&bytes)
        .map_err(|error| ControlPlaneError::InvalidResponse(error.to_string()))
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct BlobFetchRequest {
    pub protocol_version: u16,
    pub requester_id: Uuid,
    pub digest: String,
    pub source_base_url: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct BlobFetchResponse {
    pub protocol_version: u16,
    pub accepted: bool,
    pub message: String,
}

/// Ask a peer to pull `digest` from `source_base_url` (push convenience).
pub async fn request_blob_fetch(
    client: &reqwest::Client,
    base_url: &str,
    request: &BlobFetchRequest,
    identity: Option<&NodeIdentity>,
) -> Result<BlobFetchResponse, ControlPlaneError> {
    let body = serde_json::to_vec(request)
        .map_err(|e| ControlPlaneError::InvalidResponse(e.to_string()))?;
    let response = signed_post_bytes(
        client,
        base_url,
        "/nexus/control/v1/blob/fetch",
        &body,
        identity,
        request.requester_id,
    )
    .await?;
    let bytes = response.bytes().await?;
    if bytes.len() > MAX_CONTROL_RESPONSE_BYTES {
        return Err(ControlPlaneError::ResponseTooLarge {
            limit: MAX_CONTROL_RESPONSE_BYTES,
        });
    }
    serde_json::from_slice(&bytes)
        .map_err(|error| ControlPlaneError::InvalidResponse(error.to_string()))
}

/// Build the blob URL for a digest on a control-plane base URL.
pub fn blob_url(base_url: &str, digest: &str) -> Result<String, ControlPlaneError> {
    let mut url = Url::parse(base_url)
        .map_err(|error| ControlPlaneError::InvalidResponse(error.to_string()))?;
    let digest = digest.trim().to_lowercase();
    url.set_path(&format!("/nexus/control/v1/blob/{digest}"));
    Ok(url.to_string())
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ModelLoadRequest {
    pub protocol_version: u16,
    pub requester_id: Uuid,
    pub model_path: String,
    pub context_size: usize,
    pub gpu_layers: u32,
    pub threads: usize,
    pub rpc_workers: Vec<String>,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default)]
    pub target_port: Option<u16>,
    /// Force inference backend: `"auto"` (default), `"llama"`, or `"bmoe"`.
    #[serde(default = "default_backend_auto")]
    pub backend: String,
    /// Client-planned MoE expert cache ceiling (MiB); peer re-plans under stream LMK.
    #[serde(default)]
    pub moe_cache_ceil_mb: Option<u64>,
}

fn default_backend_auto() -> String {
    "auto".to_string()
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ModelLoadResponse {
    pub protocol_version: u16,
    pub success: bool,
    pub active_model: String,
    pub api_endpoint: String,
    pub error_message: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ModelUnloadRequest {
    pub protocol_version: u16,
    pub requester_id: Uuid,
    pub model_path: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ModelUnloadResponse {
    pub protocol_version: u16,
    pub success: bool,
    pub message: String,
}

/// Request envelope for model-to-model task delegation over the agent bus.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AgentTaskMessage {
    pub protocol_version: u16,
    pub task_id: Uuid,
    pub from_node: Uuid,
    pub to_route: String,
    pub prompt: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reply_to: Option<String>,
}

/// Response returned by the agent bus for a delegated task.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AgentTaskResponse {
    pub protocol_version: u16,
    pub task_id: Uuid,
    pub success: bool,
    pub status: crate::task::TaskStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// Request to store a document chunk into a node's Knowledge Base.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct KbStoreRequest {
    pub protocol_version: u16,
    pub requester_id: Uuid,
    pub document_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    pub content: String,
    #[serde(default)]
    pub metadata: std::collections::HashMap<String, String>,
}

/// Response returned after storing a chunk into the Knowledge Base.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct KbStoreResponse {
    pub protocol_version: u16,
    pub success: bool,
    pub chunk_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error_message: Option<String>,
}

/// Request to query relevant knowledge chunks from a node's Knowledge Base.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct KbQueryRequest {
    pub protocol_version: u16,
    pub requester_id: Uuid,
    pub query: String,
    #[serde(default = "default_query_limit")]
    pub limit: usize,
}

fn default_query_limit() -> usize {
    5
}

/// Individual chunk result returned from a Knowledge Base query.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct KbQueryResultItem {
    pub chunk_id: String,
    pub document_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    pub content: String,
    pub score: f32,
}

/// Response envelope containing Knowledge Base query search results.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct KbQueryResponse {
    pub protocol_version: u16,
    pub success: bool,
    pub results: Vec<KbQueryResultItem>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error_message: Option<String>,
}

/// Request to retrieve a node's Knowledge Base sync manifest.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct KbManifestRequest {
    pub protocol_version: u16,
    pub requester_id: Uuid,
}

/// Response containing a node's Knowledge Base sync manifest.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct KbManifestResponse {
    pub protocol_version: u16,
    pub manifest: crate::kb::sync::KbSyncManifest,
}

pub async fn dispatch_load_model(
    client: &reqwest::Client,
    base_url: &str,
    request: &ModelLoadRequest,
) -> Result<ModelLoadResponse, ControlPlaneError> {
    let mut url = Url::parse(base_url)
        .map_err(|error| ControlPlaneError::InvalidResponse(error.to_string()))?;
    url.set_path("/nexus/control/v1/model/load");
    let response = client
        .post(url)
        .timeout(Duration::from_secs(10))
        .json(request)
        .send()
        .await?
        .error_for_status()?;
    let bytes = response.bytes().await?;
    if bytes.len() > MAX_CONTROL_RESPONSE_BYTES {
        return Err(ControlPlaneError::ResponseTooLarge {
            limit: MAX_CONTROL_RESPONSE_BYTES,
        });
    }
    serde_json::from_slice(&bytes)
        .map_err(|error| ControlPlaneError::InvalidResponse(error.to_string()))
}

pub async fn dispatch_load_model_signed(
    client: &reqwest::Client,
    base_url: &str,
    request: &ModelLoadRequest,
    identity: &NodeIdentity,
) -> Result<ModelLoadResponse, ControlPlaneError> {
    let body = serde_json::to_vec(request)
        .map_err(|e| ControlPlaneError::InvalidResponse(e.to_string()))?;
    let response = signed_post_bytes(
        client,
        base_url,
        "/nexus/control/v1/model/load",
        &body,
        Some(identity),
        request.requester_id,
    )
    .await?;
    let bytes = response.bytes().await?;
    if bytes.len() > MAX_CONTROL_RESPONSE_BYTES {
        return Err(ControlPlaneError::ResponseTooLarge {
            limit: MAX_CONTROL_RESPONSE_BYTES,
        });
    }
    serde_json::from_slice(&bytes)
        .map_err(|error| ControlPlaneError::InvalidResponse(error.to_string()))
}

pub async fn dispatch_unload_model(
    client: &reqwest::Client,
    base_url: &str,
    request: &ModelUnloadRequest,
) -> Result<ModelUnloadResponse, ControlPlaneError> {
    let mut url = Url::parse(base_url)
        .map_err(|error| ControlPlaneError::InvalidResponse(error.to_string()))?;
    url.set_path("/nexus/control/v1/model/unload");
    let response = client
        .post(url)
        .timeout(Duration::from_secs(5))
        .json(request)
        .send()
        .await?
        .error_for_status()?;
    let bytes = response.bytes().await?;
    if bytes.len() > MAX_CONTROL_RESPONSE_BYTES {
        return Err(ControlPlaneError::ResponseTooLarge {
            limit: MAX_CONTROL_RESPONSE_BYTES,
        });
    }
    serde_json::from_slice(&bytes)
        .map_err(|error| ControlPlaneError::InvalidResponse(error.to_string()))
}

pub async fn dispatch_unload_model_signed(
    client: &reqwest::Client,
    base_url: &str,
    request: &ModelUnloadRequest,
    identity: &NodeIdentity,
) -> Result<ModelUnloadResponse, ControlPlaneError> {
    let body = serde_json::to_vec(request)
        .map_err(|e| ControlPlaneError::InvalidResponse(e.to_string()))?;
    let response = signed_post_bytes(
        client,
        base_url,
        "/nexus/control/v1/model/unload",
        &body,
        Some(identity),
        request.requester_id,
    )
    .await?;
    let bytes = response.bytes().await?;
    if bytes.len() > MAX_CONTROL_RESPONSE_BYTES {
        return Err(ControlPlaneError::ResponseTooLarge {
            limit: MAX_CONTROL_RESPONSE_BYTES,
        });
    }
    serde_json::from_slice(&bytes)
        .map_err(|error| ControlPlaneError::InvalidResponse(error.to_string()))
}

pub async fn dispatch_agent_message(
    client: &reqwest::Client,
    base_url: &str,
    msg: &AgentTaskMessage,
) -> Result<AgentTaskResponse, ControlPlaneError> {
    let mut url = Url::parse(base_url)
        .map_err(|error| ControlPlaneError::InvalidResponse(error.to_string()))?;
    url.set_path("/nexus/control/v1/agent/message");
    let response = client
        .post(url)
        .timeout(Duration::from_secs(60))
        .json(msg)
        .send()
        .await?
        .error_for_status()?;
    let bytes = response.bytes().await?;
    if bytes.len() > MAX_CONTROL_RESPONSE_BYTES {
        return Err(ControlPlaneError::ResponseTooLarge {
            limit: MAX_CONTROL_RESPONSE_BYTES,
        });
    }
    serde_json::from_slice(&bytes)
        .map_err(|error| ControlPlaneError::InvalidResponse(error.to_string()))
}

pub async fn dispatch_agent_message_signed(
    client: &reqwest::Client,
    base_url: &str,
    msg: &AgentTaskMessage,
    identity: &NodeIdentity,
) -> Result<AgentTaskResponse, ControlPlaneError> {
    let body =
        serde_json::to_vec(msg).map_err(|e| ControlPlaneError::InvalidResponse(e.to_string()))?;
    let response = signed_post_bytes(
        client,
        base_url,
        "/nexus/control/v1/agent/message",
        &body,
        Some(identity),
        msg.from_node,
    )
    .await?;
    let bytes = response.bytes().await?;
    if bytes.len() > MAX_CONTROL_RESPONSE_BYTES {
        return Err(ControlPlaneError::ResponseTooLarge {
            limit: MAX_CONTROL_RESPONSE_BYTES,
        });
    }
    serde_json::from_slice(&bytes)
        .map_err(|error| ControlPlaneError::InvalidResponse(error.to_string()))
}

pub async fn dispatch_kb_store(
    client: &reqwest::Client,
    base_url: &str,
    request: &KbStoreRequest,
) -> Result<KbStoreResponse, ControlPlaneError> {
    let mut url = Url::parse(base_url)
        .map_err(|error| ControlPlaneError::InvalidResponse(error.to_string()))?;
    url.set_path("/nexus/control/v1/kb/store");
    let response = client
        .post(url)
        .timeout(Duration::from_secs(10))
        .json(request)
        .send()
        .await?
        .error_for_status()?;
    let bytes = response.bytes().await?;
    if bytes.len() > MAX_CONTROL_RESPONSE_BYTES {
        return Err(ControlPlaneError::ResponseTooLarge {
            limit: MAX_CONTROL_RESPONSE_BYTES,
        });
    }
    serde_json::from_slice(&bytes)
        .map_err(|error| ControlPlaneError::InvalidResponse(error.to_string()))
}

pub async fn dispatch_kb_store_signed(
    client: &reqwest::Client,
    base_url: &str,
    request: &KbStoreRequest,
    identity: &NodeIdentity,
) -> Result<KbStoreResponse, ControlPlaneError> {
    let body = serde_json::to_vec(request)
        .map_err(|e| ControlPlaneError::InvalidResponse(e.to_string()))?;
    let response = signed_post_bytes(
        client,
        base_url,
        "/nexus/control/v1/kb/store",
        &body,
        Some(identity),
        request.requester_id,
    )
    .await?;
    let bytes = response.bytes().await?;
    if bytes.len() > MAX_CONTROL_RESPONSE_BYTES {
        return Err(ControlPlaneError::ResponseTooLarge {
            limit: MAX_CONTROL_RESPONSE_BYTES,
        });
    }
    serde_json::from_slice(&bytes)
        .map_err(|error| ControlPlaneError::InvalidResponse(error.to_string()))
}

pub async fn dispatch_kb_query(
    client: &reqwest::Client,
    base_url: &str,
    request: &KbQueryRequest,
) -> Result<KbQueryResponse, ControlPlaneError> {
    let mut url = Url::parse(base_url)
        .map_err(|error| ControlPlaneError::InvalidResponse(error.to_string()))?;
    url.set_path("/nexus/control/v1/kb/query");
    let response = client
        .post(url)
        .timeout(Duration::from_secs(10))
        .json(request)
        .send()
        .await?
        .error_for_status()?;
    let bytes = response.bytes().await?;
    if bytes.len() > MAX_CONTROL_RESPONSE_BYTES {
        return Err(ControlPlaneError::ResponseTooLarge {
            limit: MAX_CONTROL_RESPONSE_BYTES,
        });
    }
    serde_json::from_slice(&bytes)
        .map_err(|error| ControlPlaneError::InvalidResponse(error.to_string()))
}

pub async fn dispatch_kb_query_signed(
    client: &reqwest::Client,
    base_url: &str,
    request: &KbQueryRequest,
    identity: &NodeIdentity,
) -> Result<KbQueryResponse, ControlPlaneError> {
    let body = serde_json::to_vec(request)
        .map_err(|e| ControlPlaneError::InvalidResponse(e.to_string()))?;
    let response = signed_post_bytes(
        client,
        base_url,
        "/nexus/control/v1/kb/query",
        &body,
        Some(identity),
        request.requester_id,
    )
    .await?;
    let bytes = response.bytes().await?;
    if bytes.len() > MAX_CONTROL_RESPONSE_BYTES {
        return Err(ControlPlaneError::ResponseTooLarge {
            limit: MAX_CONTROL_RESPONSE_BYTES,
        });
    }
    serde_json::from_slice(&bytes)
        .map_err(|error| ControlPlaneError::InvalidResponse(error.to_string()))
}

pub async fn dispatch_kb_manifest(
    client: &reqwest::Client,
    base_url: &str,
    request: &KbManifestRequest,
) -> Result<KbManifestResponse, ControlPlaneError> {
    let mut url = Url::parse(base_url)
        .map_err(|error| ControlPlaneError::InvalidResponse(error.to_string()))?;
    url.set_path("/nexus/control/v1/kb/sync/manifest");
    let response = client
        .post(url)
        .timeout(Duration::from_secs(10))
        .json(request)
        .send()
        .await?
        .error_for_status()?;
    let bytes = response.bytes().await?;
    if bytes.len() > MAX_CONTROL_RESPONSE_BYTES {
        return Err(ControlPlaneError::ResponseTooLarge {
            limit: MAX_CONTROL_RESPONSE_BYTES,
        });
    }
    serde_json::from_slice(&bytes)
        .map_err(|error| ControlPlaneError::InvalidResponse(error.to_string()))
}

pub async fn dispatch_kb_manifest_signed(
    client: &reqwest::Client,
    base_url: &str,
    request: &KbManifestRequest,
    identity: &NodeIdentity,
) -> Result<KbManifestResponse, ControlPlaneError> {
    let body = serde_json::to_vec(request)
        .map_err(|e| ControlPlaneError::InvalidResponse(e.to_string()))?;
    let response = signed_post_bytes(
        client,
        base_url,
        "/nexus/control/v1/kb/sync/manifest",
        &body,
        Some(identity),
        request.requester_id,
    )
    .await?;
    let bytes = response.bytes().await?;
    if bytes.len() > MAX_CONTROL_RESPONSE_BYTES {
        return Err(ControlPlaneError::ResponseTooLarge {
            limit: MAX_CONTROL_RESPONSE_BYTES,
        });
    }
    serde_json::from_slice(&bytes)
        .map_err(|error| ControlPlaneError::InvalidResponse(error.to_string()))
}

pub async fn dispatch_kb_pull(
    client: &reqwest::Client,
    base_url: &str,
    request: &crate::kb::sync::KbSyncPullRequest,
) -> Result<crate::kb::sync::KbSyncPullResponse, ControlPlaneError> {
    let mut url = Url::parse(base_url)
        .map_err(|error| ControlPlaneError::InvalidResponse(error.to_string()))?;
    url.set_path("/nexus/control/v1/kb/sync/pull");
    let response = client
        .post(url)
        .timeout(Duration::from_secs(15))
        .json(request)
        .send()
        .await?
        .error_for_status()?;
    let bytes = response.bytes().await?;
    if bytes.len() > MAX_CONTROL_RESPONSE_BYTES {
        return Err(ControlPlaneError::ResponseTooLarge {
            limit: MAX_CONTROL_RESPONSE_BYTES,
        });
    }
    serde_json::from_slice(&bytes)
        .map_err(|error| ControlPlaneError::InvalidResponse(error.to_string()))
}

pub async fn dispatch_kb_pull_signed(
    client: &reqwest::Client,
    base_url: &str,
    request: &crate::kb::sync::KbSyncPullRequest,
    identity: &NodeIdentity,
) -> Result<crate::kb::sync::KbSyncPullResponse, ControlPlaneError> {
    let body = serde_json::to_vec(request)
        .map_err(|e| ControlPlaneError::InvalidResponse(e.to_string()))?;
    let response = signed_post_bytes(
        client,
        base_url,
        "/nexus/control/v1/kb/sync/pull",
        &body,
        Some(identity),
        request.requester_id,
    )
    .await?;
    let bytes = response.bytes().await?;
    if bytes.len() > MAX_CONTROL_RESPONSE_BYTES {
        return Err(ControlPlaneError::ResponseTooLarge {
            limit: MAX_CONTROL_RESPONSE_BYTES,
        });
    }
    serde_json::from_slice(&bytes)
        .map_err(|error| ControlPlaneError::InvalidResponse(error.to_string()))
}

pub async fn dispatch_kb_push(
    client: &reqwest::Client,
    base_url: &str,
    request: &crate::kb::sync::KbSyncPushRequest,
) -> Result<crate::kb::sync::KbSyncPushResponse, ControlPlaneError> {
    let mut url = Url::parse(base_url)
        .map_err(|error| ControlPlaneError::InvalidResponse(error.to_string()))?;
    url.set_path("/nexus/control/v1/kb/sync/push");
    let response = client
        .post(url)
        .timeout(Duration::from_secs(15))
        .json(request)
        .send()
        .await?
        .error_for_status()?;
    let bytes = response.bytes().await?;
    if bytes.len() > MAX_CONTROL_RESPONSE_BYTES {
        return Err(ControlPlaneError::ResponseTooLarge {
            limit: MAX_CONTROL_RESPONSE_BYTES,
        });
    }
    serde_json::from_slice(&bytes)
        .map_err(|error| ControlPlaneError::InvalidResponse(error.to_string()))
}

pub async fn dispatch_kb_push_signed(
    client: &reqwest::Client,
    base_url: &str,
    request: &crate::kb::sync::KbSyncPushRequest,
    identity: &NodeIdentity,
) -> Result<crate::kb::sync::KbSyncPushResponse, ControlPlaneError> {
    let body = serde_json::to_vec(request)
        .map_err(|e| ControlPlaneError::InvalidResponse(e.to_string()))?;
    let response = signed_post_bytes(
        client,
        base_url,
        "/nexus/control/v1/kb/sync/push",
        &body,
        Some(identity),
        request.requester_id,
    )
    .await?;
    let bytes = response.bytes().await?;
    if bytes.len() > MAX_CONTROL_RESPONSE_BYTES {
        return Err(ControlPlaneError::ResponseTooLarge {
            limit: MAX_CONTROL_RESPONSE_BYTES,
        });
    }
    serde_json::from_slice(&bytes)
        .map_err(|error| ControlPlaneError::InvalidResponse(error.to_string()))
}

pub async fn handle_load_model(
    manager: &crate::supervisor::SupervisorManager,
    request: &ModelLoadRequest,
    api_host: &str,
    api_port: u16,
    binary_path: &std::path::Path,
    use_mmap: bool,
    memory_budget_percent: u8,
) -> ModelLoadResponse {
    handle_load_model_with_moe(
        manager,
        request,
        api_host,
        api_port,
        binary_path,
        use_mmap,
        memory_budget_percent,
        &crate::config::MoeConfig::default(),
    )
    .await
}

/// Load a model, selecting llama-server or bmoe-cli based on GGUF + MoE policy.
#[allow(clippy::too_many_arguments)]
pub async fn handle_load_model_with_moe(
    manager: &crate::supervisor::SupervisorManager,
    request: &ModelLoadRequest,
    api_host: &str,
    api_port: u16,
    binary_path: &std::path::Path,
    use_mmap: bool,
    memory_budget_percent: u8,
    moe: &crate::config::MoeConfig,
) -> ModelLoadResponse {
    if request.protocol_version != CONTROL_PLANE_VERSION {
        return ModelLoadResponse {
            protocol_version: CONTROL_PLANE_VERSION,
            success: false,
            active_model: String::new(),
            api_endpoint: String::new(),
            error_message: Some(format!(
                "protocol mismatch: expected {}, got {}",
                CONTROL_PLANE_VERSION, request.protocol_version
            )),
        };
    }

    let raw_path = std::path::PathBuf::from(&request.model_path);
    let model_path = if raw_path.exists() {
        raw_path
    } else {
        let filename = raw_path.file_name().unwrap_or(raw_path.as_os_str());
        let default_models_dir = std::env::var("HOME")
            .map(|h| std::path::PathBuf::from(h).join("nexus-models"))
            .unwrap_or_else(|_| std::path::PathBuf::from("models"));
        let candidate = default_models_dir.join(filename);
        if candidate.exists() {
            candidate
        } else {
            raw_path
        }
    };
    let model_name = model_path
        .file_name()
        .map(|f| f.to_string_lossy().to_string())
        .unwrap_or_else(|| request.model_path.clone());

    let port = request.target_port.unwrap_or(api_port);
    let profile = crate::sysinfo::SystemProfile::probe();
    let gguf = crate::gguf::GgufMetadata::open(&model_path).ok();
    let backend_pref = request.backend.trim().to_ascii_lowercase();
    let force_bmoe = backend_pref == "bmoe";
    let force_llama = backend_pref == "llama" || backend_pref == "llama-server";
    // Never inject RPC workers into a bmoe stream session.
    let want_bmoe = if force_llama {
        false
    } else if force_bmoe {
        true
    } else if let Some(ref meta) = gguf {
        request.rpc_workers.is_empty()
            && crate::bmoe_client::should_use_bmoe(
                meta,
                &profile,
                moe,
                request.context_size,
                memory_budget_percent,
            )
    } else {
        false
    };

    if want_bmoe {
        if !request.rpc_workers.is_empty() {
            return ModelLoadResponse {
                protocol_version: CONTROL_PLANE_VERSION,
                success: false,
                active_model: String::new(),
                api_endpoint: String::new(),
                error_message: Some(
                    "MoE flash-stream cannot be combined with RPC layer offload".into(),
                ),
            };
        }
        let meta = match gguf.as_ref() {
            Some(m) if m.streamable_moe() => m,
            _ => {
                return ModelLoadResponse {
                    protocol_version: CONTROL_PLANE_VERSION,
                    success: false,
                    active_model: String::new(),
                    api_endpoint: String::new(),
                    error_message: Some("bmoe load requires a streamable MoE GGUF".into()),
                };
            }
        };
        let model_key = crate::cluster::moe_model_key(meta);
        let bench = crate::bench::BenchStore::load_default().ok();
        let extra_cap = crate::cluster::MoeCacheCap::from_optional_mb(request.moe_cache_ceil_mb);
        let knobs = match crate::cluster::plan_moe_spawn(
            meta,
            &profile,
            moe,
            memory_budget_percent,
            request.context_size,
            bench.as_ref(),
            &model_key,
            "local",
            extra_cap,
        ) {
            Some(k) => k,
            None => {
                return ModelLoadResponse {
                    protocol_version: CONTROL_PLANE_VERSION,
                    success: false,
                    active_model: String::new(),
                    api_endpoint: String::new(),
                    error_message: Some(
                        "MoE stream LMK: no feasible (context, cache) plan on this node".into(),
                    ),
                };
            }
        };
        for note in &knobs.notes {
            info!("{note}");
        }
        let binary = std::path::PathBuf::from(&knobs.moe.bmoe_binary);
        let bmoe_cfg = crate::bmoe_client::BmoeSessionConfig::from_profile_with_ceil(
            binary,
            model_path.clone(),
            api_host,
            port,
            knobs.context_size,
            request.threads,
            knobs.moe,
            &profile,
            memory_budget_percent,
            request.tags.clone(),
            Some(knobs.cache_mb),
        );
        return match manager.spawn_bmoe(bmoe_cfg).await {
            Ok(slot_port) => ModelLoadResponse {
                protocol_version: CONTROL_PLANE_VERSION,
                success: true,
                active_model: model_name,
                api_endpoint: format!("http://{}:{}", api_host, slot_port),
                error_message: None,
            },
            Err(e) => ModelLoadResponse {
                protocol_version: CONTROL_PLANE_VERSION,
                success: false,
                active_model: String::new(),
                api_endpoint: String::new(),
                error_message: Some(e.to_string()),
            },
        };
    }

    let mut extra_args = Vec::new();
    for worker in &request.rpc_workers {
        extra_args.push("--rpc".to_string());
        extra_args.push(worker.clone());
        extra_args.push("--split-mode".to_string());
        extra_args.push("layer".to_string());
    }

    let config = crate::supervisor::LlamaServerConfig {
        binary_path: binary_path.to_path_buf(),
        model_path,
        host: api_host.to_string(),
        port,
        gpu_layers: request.gpu_layers,
        threads: request.threads,
        context_size: request.context_size,
        extra_args,
        use_mmap,
        use_mlock: false,
        cpu_threads_batch: request.threads,
        fallback_to_cpu: true,
        cache_type_k: None,
        cache_type_v: None,
        memory_budget_percent,
        tags: request.tags.clone(),
        slot_save_path: std::env::var("NEXUS_SLOT_CACHE_DIR")
            .ok()
            .map(std::path::PathBuf::from),
    };

    match manager.spawn_slot(config).await {
        Ok(slot_port) => ModelLoadResponse {
            protocol_version: CONTROL_PLANE_VERSION,
            success: true,
            active_model: model_name,
            api_endpoint: format!("http://{}:{}", api_host, slot_port),
            error_message: None,
        },
        Err(e) => ModelLoadResponse {
            protocol_version: CONTROL_PLANE_VERSION,
            success: false,
            active_model: String::new(),
            api_endpoint: String::new(),
            error_message: Some(e.to_string()),
        },
    }
}

pub async fn handle_unload_model(
    manager: &crate::supervisor::SupervisorManager,
    request: &ModelUnloadRequest,
) -> ModelUnloadResponse {
    let result = if let Some(target) = &request.model_path {
        manager.stop_model(target).await
    } else {
        manager.stop().await.map(|_| true)
    };
    match result {
        Ok(_) => ModelUnloadResponse {
            protocol_version: CONTROL_PLANE_VERSION,
            success: true,
            message: "Model unloaded successfully".to_string(),
        },
        Err(e) => ModelUnloadResponse {
            protocol_version: CONTROL_PLANE_VERSION,
            success: false,
            message: format!("Failed to unload model: {}", e),
        },
    }
}

pub async fn handle_agent_message(
    task_store: &crate::task::TaskStore,
    manager: &crate::supervisor::SupervisorManager,
    request: &AgentTaskMessage,
    api_host: &str,
) -> AgentTaskResponse {
    if request.protocol_version != CONTROL_PLANE_VERSION {
        return AgentTaskResponse {
            protocol_version: CONTROL_PLANE_VERSION,
            task_id: request.task_id,
            success: false,
            status: crate::task::TaskStatus::Failed,
            output: None,
            error: Some(format!(
                "protocol mismatch: expected {}, got {}",
                CONTROL_PLANE_VERSION, request.protocol_version
            )),
        };
    }

    // Record incoming task in durable TaskStore
    let _ = task_store.create_task(
        request.task_id,
        request.from_node,
        &request.to_route,
        &request.prompt,
    );
    let _ = task_store.update_status(request.task_id, crate::task::TaskStatus::Running);

    // Look for an active slot matching route tag or model name
    let slots = manager.slots_info().await;
    let target_slot = slots
        .iter()
        .find(|s| {
            s.tags
                .iter()
                .any(|t| t.eq_ignore_ascii_case(&request.to_route))
                || s.model_name
                    .to_lowercase()
                    .contains(&request.to_route.to_lowercase())
        })
        .or_else(|| slots.first());

    let response = if let Some(slot) = target_slot {
        let endpoint = format!("http://{}:{}/v1/chat/completions", api_host, slot.port);
        let client = reqwest::Client::new();
        let payload = serde_json::json!({
            "model": slot.model_name,
            "messages": [
                {"role": "user", "content": &request.prompt}
            ],
            "stream": false
        });

        match client
            .post(&endpoint)
            .timeout(Duration::from_secs(30))
            .json(&payload)
            .send()
            .await
        {
            Ok(resp) if resp.status().is_success() => {
                match resp.json::<serde_json::Value>().await {
                    Ok(val) => {
                        let text = val
                            .get("choices")
                            .and_then(|c| c.as_array())
                            .and_then(|arr| arr.first())
                            .and_then(|c| c.get("message"))
                            .and_then(|m| m.get("content"))
                            .and_then(|txt| txt.as_str())
                            .unwrap_or("")
                            .to_string();

                        let _ = task_store.complete_task(request.task_id, &text);
                        AgentTaskResponse {
                            protocol_version: CONTROL_PLANE_VERSION,
                            task_id: request.task_id,
                            success: true,
                            status: crate::task::TaskStatus::Completed,
                            output: Some(text),
                            error: None,
                        }
                    }
                    Err(e) => {
                        let err_msg = format!("failed to parse model completion: {}", e);
                        let _ = task_store.fail_task(request.task_id, &err_msg);
                        AgentTaskResponse {
                            protocol_version: CONTROL_PLANE_VERSION,
                            task_id: request.task_id,
                            success: false,
                            status: crate::task::TaskStatus::Failed,
                            output: None,
                            error: Some(err_msg),
                        }
                    }
                }
            }
            Ok(resp) => {
                let err_msg = format!("model endpoint returned HTTP status {}", resp.status());
                let _ = task_store.fail_task(request.task_id, &err_msg);
                AgentTaskResponse {
                    protocol_version: CONTROL_PLANE_VERSION,
                    task_id: request.task_id,
                    success: false,
                    status: crate::task::TaskStatus::Failed,
                    output: None,
                    error: Some(err_msg),
                }
            }
            Err(e) => {
                let err_msg = format!("failed to reach model slot on port {}: {}", slot.port, e);
                let _ = task_store.fail_task(request.task_id, &err_msg);
                AgentTaskResponse {
                    protocol_version: CONTROL_PLANE_VERSION,
                    task_id: request.task_id,
                    success: false,
                    status: crate::task::TaskStatus::Failed,
                    output: None,
                    error: Some(err_msg),
                }
            }
        }
    } else {
        // No active slot found; record as accepted / completed with route message
        let msg = format!(
            "Task accepted by node but no active model slot currently matches route '{}'",
            request.to_route
        );
        let _ = task_store.update_status(request.task_id, crate::task::TaskStatus::Pending);
        AgentTaskResponse {
            protocol_version: CONTROL_PLANE_VERSION,
            task_id: request.task_id,
            success: true,
            status: crate::task::TaskStatus::Pending,
            output: Some(msg),
            error: None,
        }
    };

    // If reply_to callback URL was specified, dispatch the response asynchronously
    if let Some(reply_url) = &request.reply_to {
        let client = reqwest::Client::new();
        let _ = client
            .post(reply_url)
            .timeout(Duration::from_secs(5))
            .json(&response)
            .send()
            .await;
    }

    response
}

pub async fn handle_kb_store(
    store: &crate::kb::KnowledgeStore,
    request: &KbStoreRequest,
) -> KbStoreResponse {
    if request.protocol_version != CONTROL_PLANE_VERSION {
        return KbStoreResponse {
            protocol_version: CONTROL_PLANE_VERSION,
            success: false,
            chunk_id: String::new(),
            error_message: Some(format!(
                "protocol mismatch: expected {}, got {}",
                CONTROL_PLANE_VERSION, request.protocol_version
            )),
        };
    }

    match store.store_chunk(
        &request.document_id,
        request.title.as_deref(),
        &request.content,
        request.metadata.clone(),
        None,
    ) {
        Ok(chunk) => KbStoreResponse {
            protocol_version: CONTROL_PLANE_VERSION,
            success: true,
            chunk_id: chunk.chunk_id,
            error_message: None,
        },
        Err(e) => KbStoreResponse {
            protocol_version: CONTROL_PLANE_VERSION,
            success: false,
            chunk_id: String::new(),
            error_message: Some(e.to_string()),
        },
    }
}

pub async fn handle_kb_query(
    store: &crate::kb::KnowledgeStore,
    request: &KbQueryRequest,
) -> KbQueryResponse {
    if request.protocol_version != CONTROL_PLANE_VERSION {
        return KbQueryResponse {
            protocol_version: CONTROL_PLANE_VERSION,
            success: false,
            results: Vec::new(),
            error_message: Some(format!(
                "protocol mismatch: expected {}, got {}",
                CONTROL_PLANE_VERSION, request.protocol_version
            )),
        };
    }

    let chunks = match store.list_chunks() {
        Ok(c) => c,
        Err(e) => {
            return KbQueryResponse {
                protocol_version: CONTROL_PLANE_VERSION,
                success: false,
                results: Vec::new(),
                error_message: Some(e.to_string()),
            };
        }
    };

    let ranked = crate::kb::vector::rank_chunks(None, &request.query, &chunks, request.limit, 0.0);
    let results = ranked
        .into_iter()
        .map(|r| KbQueryResultItem {
            chunk_id: r.item.chunk_id,
            document_id: r.item.document_id,
            title: r.item.title,
            content: r.item.content,
            score: r.score,
        })
        .collect();

    KbQueryResponse {
        protocol_version: CONTROL_PLANE_VERSION,
        success: true,
        results,
        error_message: None,
    }
}

pub async fn handle_kb_manifest(
    store: &crate::kb::KnowledgeStore,
    node_id: Uuid,
    request: &KbManifestRequest,
) -> KbManifestResponse {
    if request.protocol_version != CONTROL_PLANE_VERSION {
        return KbManifestResponse {
            protocol_version: CONTROL_PLANE_VERSION,
            manifest: crate::kb::sync::KbSyncManifest {
                protocol_version: CONTROL_PLANE_VERSION,
                node_id,
                chunks: Vec::new(),
                personas: Vec::new(),
                memories: Vec::new(),
            },
        };
    }

    let manifest = crate::kb::sync::generate_manifest(store, node_id).unwrap_or_else(|_| {
        crate::kb::sync::KbSyncManifest {
            protocol_version: CONTROL_PLANE_VERSION,
            node_id,
            chunks: Vec::new(),
            personas: Vec::new(),
            memories: Vec::new(),
        }
    });

    KbManifestResponse {
        protocol_version: CONTROL_PLANE_VERSION,
        manifest,
    }
}

pub async fn handle_kb_pull(
    store: &crate::kb::KnowledgeStore,
    request: &crate::kb::sync::KbSyncPullRequest,
) -> crate::kb::sync::KbSyncPullResponse {
    if request.protocol_version != CONTROL_PLANE_VERSION {
        return crate::kb::sync::KbSyncPullResponse {
            protocol_version: CONTROL_PLANE_VERSION,
            chunks: Vec::new(),
            personas: Vec::new(),
            memories: Vec::new(),
        };
    }

    crate::kb::sync::apply_pull(store, request).unwrap_or_else(|_| {
        crate::kb::sync::KbSyncPullResponse {
            protocol_version: CONTROL_PLANE_VERSION,
            chunks: Vec::new(),
            personas: Vec::new(),
            memories: Vec::new(),
        }
    })
}

pub async fn handle_kb_push(
    store: &crate::kb::KnowledgeStore,
    request: &crate::kb::sync::KbSyncPushRequest,
) -> crate::kb::sync::KbSyncPushResponse {
    if request.protocol_version != CONTROL_PLANE_VERSION {
        return crate::kb::sync::KbSyncPushResponse {
            protocol_version: CONTROL_PLANE_VERSION,
            success: false,
            accepted_chunks: 0,
            accepted_personas: 0,
            accepted_memories: 0,
            error_message: Some(format!(
                "protocol mismatch: expected {}, got {}",
                CONTROL_PLANE_VERSION, request.protocol_version
            )),
        };
    }

    crate::kb::sync::apply_push(store, request).unwrap_or_else(|e| {
        crate::kb::sync::KbSyncPushResponse {
            protocol_version: CONTROL_PLANE_VERSION,
            success: false,
            accepted_chunks: 0,
            accepted_personas: 0,
            accepted_memories: 0,
            error_message: Some(e.to_string()),
        }
    })
}
