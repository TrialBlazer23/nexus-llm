use nexus::config::NexusConfig;
use nexus::control_plane::{validate_state, ControlPlaneState, CONTROL_PLANE_VERSION};
use nexus::discovery::{
    DiscoveryEvent, DiscoveryService, NodeRole, RpcSelectionPolicy, ServiceEndpoint, StatusFlags,
};
use nexus::peer_registry::{ObservationSource, PeerLifecycle, PeerRegistry, RegistryError};
use std::net::SocketAddr;
use std::time::{Duration, Instant};
use uuid::Uuid;

fn endpoint(node_id: Uuid, address: [u8; 4]) -> ServiceEndpoint {
    ServiceEndpoint {
        node_id,
        cluster_id: None,
        protocol_version: CONTROL_PLANE_VERSION,
        role: NodeRole::HOST,
        capabilities: vec!["inference".to_string()],
        addresses: vec![SocketAddr::from((address, 8080))],
        api_port: 8080,
        rpc_port: 50052,
    }
}

#[test]
fn registry_merges_observations_by_stable_id() {
    let id = Uuid::new_v4();
    let now = Instant::now();
    let mut registry = PeerRegistry::new(4, Duration::from_secs(6)).unwrap();
    registry
        .apply_event(
            DiscoveryEvent::ServiceFound(endpoint(id, [192, 168, 1, 20])),
            ObservationSource::Udp,
            now,
        )
        .unwrap();
    registry
        .apply_event(
            DiscoveryEvent::ServiceUpdated(endpoint(id, [192, 168, 1, 20])),
            ObservationSource::Mdns,
            now + Duration::from_secs(1),
        )
        .unwrap();

    let record = registry.get(id).unwrap();
    assert_eq!(record.sources.len(), 2);
    assert_eq!(record.lifecycle, PeerLifecycle::Verifying);
}

#[test]
fn verified_peer_rejects_conflicting_unverified_endpoint() {
    let id = Uuid::new_v4();
    let now = Instant::now();
    let mut registry = PeerRegistry::new(4, Duration::from_secs(6)).unwrap();
    registry
        .observe(endpoint(id, [192, 168, 1, 20]), ObservationSource::Udp, now)
        .unwrap();
    registry
        .mark_verified(id, endpoint(id, [192, 168, 1, 20]), now)
        .unwrap();

    let result = registry.observe(
        endpoint(id, [192, 168, 1, 21]),
        ObservationSource::Udp,
        now + Duration::from_secs(1),
    );
    assert_eq!(result, Err(RegistryError::EndpointConflict(id)));
}

#[test]
fn registry_expires_and_removes_terminal_peers() {
    let id = Uuid::new_v4();
    let now = Instant::now();
    let mut registry = PeerRegistry::new(4, Duration::from_secs(1)).unwrap();
    registry
        .observe(
            endpoint(id, [192, 168, 1, 20]),
            ObservationSource::Static,
            now,
        )
        .unwrap();
    assert_eq!(registry.expire(now + Duration::from_secs(2)), vec![id]);
    registry
        .apply_event(
            DiscoveryEvent::ServiceRemoved { node_id: id },
            ObservationSource::Udp,
            now + Duration::from_secs(3),
        )
        .unwrap();
    assert_eq!(registry.remove_terminal(), vec![id]);
}

#[test]
fn control_plane_validates_identity_protocol_and_policy() {
    let id = Uuid::new_v4();
    let state = ControlPlaneState {
        node_id: id,
        protocol_version: CONTROL_PLANE_VERSION,
        role: NodeRole::HOST,
        capabilities: vec!["inference".to_string()],
        ready: true,
        inferring: false,
        rpc_ready: true,
        allocatable_memory_mb: 1800,
        active_model: None,
    };
    validate_state(&state, id, CONTROL_PLANE_VERSION, 1800).unwrap();

    let mut invalid = state.clone();
    invalid.rpc_ready = false;
    invalid.ready = false;
    invalid.allocatable_memory_mb = 1801;
    assert!(validate_state(&invalid, id, CONTROL_PLANE_VERSION, 1800).is_err());
}

#[tokio::test]
async fn rpc_selection_requires_policy_and_caps_allocatable_memory() {
    let mut config = NexusConfig::default();
    config.network.security.require_pairing = true;
    let worker_id = Uuid::new_v4();
    config.network.security.allowed_peer_ids = vec![worker_id];
    let discovery = DiscoveryService::new(config, Some(Uuid::new_v4()));
    let peer = endpoint(worker_id, [192, 168, 1, 30]);
    let now = Instant::now();
    let mut observed = peer.clone();
    observed.addresses = vec![SocketAddr::from(([192, 168, 1, 30], 8080))];
    discovery
        .peer_registry()
        .write()
        .await
        .observe(observed, ObservationSource::Udp, now)
        .unwrap();

    let mut beacon_peer = nexus::discovery::PeerNode {
        uuid: worker_id,
        addr: SocketAddr::from(([192, 168, 1, 30], 8080)),
        role: NodeRole::HOST,
        status: StatusFlags(StatusFlags::READY.0 | StatusFlags::RPC_READY.0),
        api_port: 8080,
        rpc_port: 50052,
        total_ram_mb: 4096,
        free_ram_mb: 3000,
        backend: nexus::sysinfo::AccelerationBackend::X86Baseline,
        thermal_index: 20,
        active_model: String::new(),
        last_seen: now,
    };
    discovery
        .peers()
        .write()
        .await
        .insert(worker_id, beacon_peer.clone());

    let candidate = discovery
        .select_rpc_candidate(RpcSelectionPolicy {
            max_thermal_index: 75,
            max_allocatable_mb: 2200,
            require_pairing: true,
        })
        .await
        .unwrap();
    assert_eq!(candidate.allocatable_mb, 1800);
    assert_eq!(candidate.peer.uuid, worker_id);

    beacon_peer.thermal_index = 90;
    discovery
        .peers()
        .write()
        .await
        .insert(worker_id, beacon_peer);
    assert!(discovery
        .select_rpc_candidate(RpcSelectionPolicy {
            max_thermal_index: 75,
            max_allocatable_mb: 1800,
            require_pairing: true,
        })
        .await
        .is_none());
}

#[tokio::test]
async fn primary_compute_resolution_does_not_promote_unpinned_host() {
    let mut config = NexusConfig::default();
    let pinned_id = Uuid::new_v4();
    config.network.anchors.primary_compute_id = Some(pinned_id);
    let discovery = DiscoveryService::new(config, Some(Uuid::new_v4()));
    let peer_id = Uuid::new_v4();
    discovery.peers().write().await.insert(
        peer_id,
        nexus::discovery::PeerNode {
            uuid: peer_id,
            addr: SocketAddr::from(([192, 168, 1, 31], 8080)),
            role: NodeRole::HOST,
            status: StatusFlags::READY,
            api_port: 8080,
            rpc_port: 0,
            total_ram_mb: 16000,
            free_ram_mb: 12000,
            backend: nexus::sysinfo::AccelerationBackend::Vulkan,
            thermal_index: 0,
            active_model: String::new(),
            last_seen: Instant::now(),
        },
    );

    assert!(discovery.resolve_primary_compute_anchor().await.is_none());
}
