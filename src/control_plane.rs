use crate::discovery::{NodeRole, ServiceEndpoint};
use crate::node_identity::NodeIdentity;
use crate::trust_auth::{apply_auth_headers, AuthError};
use reqwest::Url;
use serde::{Deserialize, Serialize};
use std::time::Duration;
use thiserror::Error;
use uuid::Uuid;

pub const CONTROL_PLANE_VERSION: u16 = 1;
pub const MAX_CONTROL_RESPONSE_BYTES: usize = 16 * 1024;

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
    let healthy = manager.is_healthy().await;
    let active_model = manager.active_model().await;
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

pub async fn handle_load_model(
    manager: &crate::supervisor::SupervisorManager,
    request: &ModelLoadRequest,
    api_host: &str,
    api_port: u16,
    binary_path: &std::path::Path,
    use_mmap: bool,
    memory_budget_percent: u8,
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
        port: api_port,
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
    };

    match manager.spawn(config).await {
        Ok(()) => ModelLoadResponse {
            protocol_version: CONTROL_PLANE_VERSION,
            success: true,
            active_model: model_name,
            api_endpoint: format!("http://{}:{}", api_host, api_port),
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
    _request: &ModelUnloadRequest,
) -> ModelUnloadResponse {
    match manager.stop().await {
        Ok(()) => ModelUnloadResponse {
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
