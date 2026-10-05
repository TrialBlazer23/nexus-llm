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
