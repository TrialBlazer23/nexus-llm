//! Phase 12 integration tests: multi-instance supervisor and state advertising.

use nexus::config::NexusConfig;
use nexus::control_plane::{
    build_control_plane_state_with_host, dispatch_unload_model, fetch_state, ControlPlaneRequest,
    ModelUnloadRequest, CONTROL_PLANE_VERSION,
};
use nexus::control_plane_server::{spawn_ephemeral, ControlPlaneContext};
use nexus::discovery::NodeRole;
use nexus::supervisor::{LlamaServerConfig, SupervisorManager};
use nexus::trust_auth::TrustBootstrap;
use std::path::PathBuf;
use std::sync::Arc;
use tempfile::TempDir;
use uuid::Uuid;

#[tokio::test]
async fn supervisor_port_allocation_and_slot_tracking() {
    let supervisor = SupervisorManager::with_base_port(8080);
    assert!(!supervisor.is_running().await);
    assert_eq!(supervisor.active_models().await.len(), 0);
    assert_eq!(supervisor.slots_info().await.len(), 0);

    // Initial allocated port should be base_port
    let port1 = supervisor.allocate_port(None).await;
    assert_eq!(port1, 8080);

    // Preferred port if not occupied should be respected
    let port2 = supervisor.allocate_port(Some(8088)).await;
    assert_eq!(port2, 8088);
}

#[tokio::test]
async fn control_plane_state_advertises_loaded_models() {
    let supervisor = SupervisorManager::new();
    let node_id = Uuid::new_v4();

    let state = build_control_plane_state_with_host(
        node_id,
        NodeRole::HOST,
        vec!["inference".into(), "orchestrator".into()],
        &supervisor,
        2048,
        true,
        Some("test_pubkey".into()),
        "192.168.1.100",
    )
    .await;

    assert_eq!(state.node_id, node_id);
    assert_eq!(state.protocol_version, CONTROL_PLANE_VERSION);
    assert!(state.ready);
    assert!(state.rpc_ready);
    assert_eq!(state.loaded_models.len(), 0);
}

#[tokio::test]
async fn control_plane_http_state_round_trip_with_loaded_models() {
    let supervisor = SupervisorManager::new();
    let node_id = Uuid::new_v4();
    let dir = TempDir::new().unwrap();
    let config_path = dir.path().join("config.toml");
    NexusConfig::default().save_to_path(&config_path).unwrap();
    std::env::set_var("NEXUS_CONFIG", config_path.to_str().unwrap());
    let trust = TrustBootstrap::load(NexusConfig::default()).unwrap();

    let ctx = Arc::new(
        ControlPlaneContext::new(
            node_id,
            NodeRole::HOST,
            supervisor.clone(),
            "127.0.0.1",
            18080,
            PathBuf::from("llama-server"),
            trust.identity,
            trust.config,
            trust.config_path,
        )
        .with_capabilities(vec!["inference".to_string(), "router".to_string()]),
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
    assert_eq!(state.loaded_models.len(), 0);

    handle.abort();
}

#[tokio::test]
async fn control_plane_http_targeted_unload() {
    let supervisor = SupervisorManager::new();
    let dir = TempDir::new().unwrap();
    let config_path = dir.path().join("config.toml");
    NexusConfig::default().save_to_path(&config_path).unwrap();
    std::env::set_var("NEXUS_CONFIG", config_path.to_str().unwrap());
    let trust = TrustBootstrap::load(NexusConfig::default()).unwrap();

    let ctx = Arc::new(ControlPlaneContext::new(
        Uuid::new_v4(),
        NodeRole::HOST,
        supervisor.clone(),
        "127.0.0.1",
        18080,
        PathBuf::from("llama-server"),
        trust.identity,
        trust.config,
        trust.config_path,
    ));

    let (addr, handle) = spawn_ephemeral(ctx).await.expect("bind ephemeral");
    let base = format!("http://{}", addr);
    let client = reqwest::Client::new();

    // Unload a specific nonexistent model name gracefully succeeds
    let unload_resp = dispatch_unload_model(
        &client,
        &base,
        &ModelUnloadRequest {
            protocol_version: CONTROL_PLANE_VERSION,
            requester_id: Uuid::new_v4(),
            model_path: Some("qwen-1.7b.gguf".to_string()),
        },
    )
    .await
    .expect("unload request");

    assert!(unload_resp.success);
    handle.abort();
}

#[tokio::test]
async fn supervisor_cumulative_memory_guard_trips() {
    let supervisor = SupervisorManager::new();
    let temp_dir = TempDir::new().unwrap();
    let fake_model = temp_dir.path().join("fake_model.gguf");
    tokio::fs::write(&fake_model, vec![0u8; 10 * 1024 * 1024])
        .await
        .unwrap();

    let current_binary = std::env::current_exe().unwrap();
    let mut cfg = LlamaServerConfig::new(
        current_binary,
        &fake_model,
        "127.0.0.1",
        8080,
    );
    // Setting budget percent to 0 forces memory cap error
    cfg.memory_budget_percent = 0;

    let res = supervisor.spawn_slot(cfg).await;
    assert!(res.is_err());
    match res.unwrap_err() {
        nexus::supervisor::SupervisorError::MemoryCapExceeded { .. } => {}
        other => panic!("Expected MemoryCapExceeded error, got: {:?}", other),
    }
}
