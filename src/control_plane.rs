use crate::discovery::{NodeRole, ServiceEndpoint};
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
    }
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

pub async fn handle_load_model(
    manager: &crate::supervisor::SupervisorManager,
    request: &ModelLoadRequest,
    api_host: &str,
    api_port: u16,
    binary_path: &std::path::Path,
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
