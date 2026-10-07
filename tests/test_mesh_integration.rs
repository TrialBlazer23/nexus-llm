//! Two-node in-process mesh: UDP discovery + control-plane HTTP (Continuous §6).

use nexus::config::NexusConfig;
use nexus::control_plane::{fetch_state, ControlPlaneRequest, CONTROL_PLANE_VERSION};
use nexus::control_plane_server::{spawn_ephemeral, ControlPlaneContext};
use nexus::discovery::{DiscoveryService, NodeRole, StatusFlags};
use nexus::supervisor::SupervisorManager;
use nexus::trust_auth::TrustBootstrap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tempfile::TempDir;
use uuid::Uuid;

fn free_udp_port() -> u16 {
    let sock = std::net::UdpSocket::bind("127.0.0.1:0").expect("bind ephemeral udp");
    sock.local_addr().expect("local_addr").port()
}

fn mesh_node_config(discovery_port: u16, api_port: u16, control_port: u16) -> NexusConfig {
    let mut cfg = NexusConfig::default();
    cfg.network.discovery_port = discovery_port;
    cfg.network.api_port = api_port;
    cfg.network.control_port = control_port;
    // Gateway not under test here; disable to avoid ephemeral port collisions.
    cfg.network.gateway_enabled = false;
    cfg.network.broadcast_interval_ms = 50;
    cfg.network.peer_timeout_ms = 5_000;
    cfg.network.discovery.enabled = true;
    cfg.network.discovery.mdns.enabled = false;
    cfg.node.role = "host".into();
    cfg.validate().expect("mesh node config valid");
    cfg
}

fn trust_context(
    node_id: Uuid,
    supervisor: SupervisorManager,
    api_port: u16,
) -> (Arc<ControlPlaneContext>, TempDir) {
    let dir = TempDir::new().expect("tempdir");
    let path = dir.path().join("config.toml");
    NexusConfig::default().save_to_path(&path).expect("save");
    std::env::set_var("NEXUS_CONFIG", path.to_str().unwrap());
    let trust = TrustBootstrap::load(NexusConfig::default()).expect("trust");
    let ctx = Arc::new(ControlPlaneContext::new(
        node_id,
        NodeRole::HOST,
        supervisor,
        "127.0.0.1",
        api_port,
        PathBuf::from("llama-server"),
        trust.identity,
        trust.config,
        trust.config_path,
    ));
    (ctx, dir)
}

#[tokio::test]
async fn two_node_discovery_and_control_plane_state() {
    let port_a = free_udp_port();
    let port_b = free_udp_port();
    // Keep api/control distinct from discovery and each other (config validate).
    let api_a = free_udp_port().saturating_add(1).max(10_000);
    let api_b = free_udp_port().saturating_add(2).max(10_100);
    let ctrl_a = free_udp_port().saturating_add(3).max(10_200);
    let ctrl_b = free_udp_port().saturating_add(4).max(10_300);

    let id_a = Uuid::new_v4();
    let id_b = Uuid::new_v4();

    let svc_a = Arc::new(DiscoveryService::new(
        mesh_node_config(port_a, api_a, ctrl_a),
        Some(id_a),
    ));
    let svc_b = Arc::new(DiscoveryService::new(
        mesh_node_config(port_b, api_b, ctrl_b),
        Some(id_b),
    ));

    svc_a
        .set_status_flags(StatusFlags(
            StatusFlags::READY.0 | StatusFlags::VULKAN_ACTIVE.0,
        ))
        .await;
    svc_b.set_status_flags(StatusFlags::READY).await;

    let listen_a = svc_a.clone().start_listener();
    let listen_b = svc_b.clone().start_listener();
    // Give listeners a moment to bind before probes.
    tokio::time::sleep(Duration::from_millis(80)).await;

    svc_a.add_static_peer(&format!("127.0.0.1:{port_b}")).await;
    svc_b.add_static_peer(&format!("127.0.0.1:{port_a}")).await;

    let broadcaster_a = svc_a.clone().start_broadcaster();
    let broadcaster_b = svc_b.clone().start_broadcaster();

    // Wait until each peer cache sees the other UUID.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    let mut saw_b_on_a = false;
    let mut saw_a_on_b = false;
    while tokio::time::Instant::now() < deadline {
        {
            let peers_lock = svc_a.peers();
            let peers = peers_lock.read().await;
            saw_b_on_a = peers.contains_key(&id_b);
        }
        {
            let peers_lock = svc_b.peers();
            let peers = peers_lock.read().await;
            saw_a_on_b = peers.contains_key(&id_a);
        }
        if saw_b_on_a && saw_a_on_b {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(
        saw_b_on_a,
        "node A must observe node B via UDP beacon (port {port_b})"
    );
    assert!(
        saw_a_on_b,
        "node B must observe node A via UDP beacon (port {port_a})"
    );

    // Control-plane HTTP round-trip across both ephemeral servers.
    let (ctx_a, _dir_a) = trust_context(id_a, SupervisorManager::new(), api_a);
    let (ctx_b, _dir_b) = trust_context(id_b, SupervisorManager::new(), api_b);
    let (addr_a, handle_a) = spawn_ephemeral(ctx_a).await.expect("cp A");
    let (addr_b, handle_b) = spawn_ephemeral(ctx_b).await.expect("cp B");

    let http = reqwest::Client::new();
    let state_b = fetch_state(
        &http,
        &format!("http://{addr_b}"),
        &ControlPlaneRequest {
            protocol_version: CONTROL_PLANE_VERSION,
            requester_id: id_a,
        },
    )
    .await
    .expect("A→B fetch_state");
    assert_eq!(state_b.node_id, id_b);
    assert!(state_b.ready);

    let state_a = fetch_state(
        &http,
        &format!("http://{addr_a}"),
        &ControlPlaneRequest {
            protocol_version: CONTROL_PLANE_VERSION,
            requester_id: id_b,
        },
    )
    .await
    .expect("B→A fetch_state");
    assert_eq!(state_a.node_id, id_a);

    handle_a.abort();
    handle_b.abort();
    listen_a.abort();
    listen_b.abort();
    broadcaster_a.abort();
    broadcaster_b.abort();
}
