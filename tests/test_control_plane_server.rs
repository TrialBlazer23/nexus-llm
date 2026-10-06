//! Loopback HTTP integration tests for the Phase 7 control-plane server.

use nexus::control_plane::{
    dispatch_load_model, dispatch_unload_model, fetch_state, ControlPlaneRequest,
    ModelLoadRequest, ModelUnloadRequest, CONTROL_PLANE_VERSION,
};
use nexus::control_plane_server::{spawn_ephemeral, ControlPlaneContext};
use nexus::discovery::NodeRole;
use nexus::supervisor::SupervisorManager;
use std::path::PathBuf;
use std::sync::Arc;
use uuid::Uuid;

fn test_context(supervisor: SupervisorManager) -> Arc<ControlPlaneContext> {
    Arc::new(ControlPlaneContext::new(
        Uuid::new_v4(),
        NodeRole::HOST,
        supervisor,
        "127.0.0.1",
        18080,
        PathBuf::from("llama-server"),
    ))
}

#[tokio::test]
async fn control_plane_http_state_round_trip() {
    let supervisor = SupervisorManager::new();
    let node_id = Uuid::new_v4();
    let ctx = Arc::new(
        ControlPlaneContext::new(
            node_id,
            NodeRole::HOST,
            supervisor.clone(),
            "127.0.0.1",
            18080,
            PathBuf::from("llama-server"),
        )
        .with_capabilities(vec!["inference".to_string()]),
    );
    let (addr, handle) = spawn_ephemeral(ctx).await.expect("bind ephemeral");
    let base = format!("http://{}", addr);
    let client = reqwest::Client::new();

    let state = fetch_state(
        &client,
        &base,
        &ControlPlaneRequest {
            protocol_version: CONTROL_PLANE_VERSION,
            requester_id: Uuid::new_v4(),
        },
    )
    .await
    .expect("fetch_state over HTTP");

    assert_eq!(state.node_id, node_id);
    assert_eq!(state.protocol_version, CONTROL_PLANE_VERSION);
    assert!(state.ready);
    assert!(!state.inferring);
    assert!(state.active_model.is_none());

    handle.abort();
}

#[tokio::test]
async fn control_plane_http_load_fails_without_binary_and_unload_succeeds() {
    let supervisor = SupervisorManager::new();
    let ctx = test_context(supervisor.clone());
    let (addr, handle) = spawn_ephemeral(ctx).await.expect("bind ephemeral");
    let base = format!("http://{}", addr);
    let client = reqwest::Client::new();
    let requester_id = Uuid::new_v4();

    let load_resp = dispatch_load_model(
        &client,
        &base,
        &ModelLoadRequest {
            protocol_version: CONTROL_PLANE_VERSION,
            requester_id,
            model_path: "missing-model.gguf".to_string(),
            context_size: 2048,
            gpu_layers: 0,
            threads: 2,
            rpc_workers: Vec::new(),
        },
    )
    .await
    .expect("HTTP load round-trip");

    assert!(!load_resp.success);
    assert!(load_resp.error_message.is_some());
    assert!(!supervisor.is_running().await);

    let unload_resp = dispatch_unload_model(
        &client,
        &base,
        &ModelUnloadRequest {
            protocol_version: CONTROL_PLANE_VERSION,
            requester_id,
            model_path: None,
        },
    )
    .await
    .expect("HTTP unload round-trip");
    assert!(unload_resp.success);

    handle.abort();
}

#[tokio::test]
async fn control_plane_http_rejects_unknown_route() {
    let supervisor = SupervisorManager::new();
    let ctx = test_context(supervisor);
    let (addr, handle) = spawn_ephemeral(ctx).await.expect("bind ephemeral");
    let client = reqwest::Client::new();
    let resp = client
        .get(format!("http://{}/nexus/control/v1/nope", addr))
        .send()
        .await
        .expect("GET unknown");
    assert_eq!(resp.status(), reqwest::StatusCode::NOT_FOUND);
    handle.abort();
}

#[test]
fn default_network_control_port_is_distinct() {
    let config = nexus::config::NexusConfig::default();
    assert_eq!(config.network.control_port, 9998);
    assert_ne!(config.network.control_port, config.network.api_port);
    assert_ne!(config.network.control_port, config.network.discovery_port);
}
