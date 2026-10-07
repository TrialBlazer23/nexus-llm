use nexus::client::{ChatCompletionChunk, ChatCompletionRequest, ChatMessage};
use nexus::config::NexusConfig;
use nexus::discovery::{
    BackendHealth, BeaconPacket, DiscoveryError, DiscoveryService, NodeRole, PeerNode,
    RpcSelectionPolicy, ServiceEndpoint, StatusFlags, BEACON_MAGIC, BEACON_PACKET_SIZE,
    BEACON_VERSION,
};
use nexus::peer_registry::ObservationSource;
use nexus::sysinfo::AccelerationBackend;
use std::net::SocketAddr;
use std::time::{Duration, Instant};
use uuid::Uuid;

fn hex_bytes(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

#[test]
fn test_beacon_packet_encoding_and_crc() {
    let uuid = Uuid::parse_str("a1a2a3a4-b1b2-c1c2-d1d2-d3d4d5d6d7d8").unwrap();
    let packet = BeaconPacket {
        magic: BEACON_MAGIC,
        version: BEACON_VERSION,
        role: NodeRole::HOST,
        status: StatusFlags(StatusFlags::READY.0 | StatusFlags::VULKAN_ACTIVE.0),
        uuid,
        api_port: 8080,
        rpc_port: 0,
        total_ram_mb: 12000,
        free_ram_mb: 5000,
        backend: AccelerationBackend::Vulkan,
        thermal_index: 35,
        active_model: "llama-3-8b".to_string(),
    };

    let encoded = packet.encode();
    assert_eq!(
        encoded.len(),
        BEACON_PACKET_SIZE,
        "Beacon must be exactly 64 bytes"
    );

    // Verify magic signature bytes ("NXUS" = 0x4E, 0x58, 0x55, 0x53)
    assert_eq!(&encoded[0..4], &[0x4E, 0x58, 0x55, 0x53]);
    assert_eq!(encoded[4], 1); // Version 1
    assert_eq!(encoded[5], 1); // Role Host

    // Verify successful decode
    let decoded = BeaconPacket::decode(&encoded).expect("Beacon decoding failed");
    assert_eq!(decoded.magic, BEACON_MAGIC);
    assert_eq!(decoded.version, BEACON_VERSION);
    assert_eq!(decoded.role, NodeRole::HOST);
    assert!(decoded.status.is_ready());
    assert!(decoded.status.is_vulkan_active());
    assert_eq!(decoded.uuid, uuid);
    assert_eq!(decoded.api_port, 8080);
    assert_eq!(decoded.rpc_port, 0);
    assert_eq!(decoded.total_ram_mb, 12000);
    assert_eq!(decoded.free_ram_mb, 5000);
    assert_eq!(decoded.backend, AccelerationBackend::Vulkan);
    assert_eq!(decoded.thermal_index, 35);
    assert_eq!(decoded.active_model, "llama-3-8b");
}

#[test]
fn test_beacon_rpc_flags_and_port() {
    let uuid = Uuid::new_v4();
    let packet = BeaconPacket {
        magic: BEACON_MAGIC,
        version: BEACON_VERSION,
        role: NodeRole::CLIENT,
        status: StatusFlags(StatusFlags::READY.0 | StatusFlags::RPC_READY.0),
        uuid,
        api_port: 8080,
        rpc_port: 50052,
        total_ram_mb: 3600,
        free_ram_mb: 1800,
        backend: AccelerationBackend::X86Baseline,
        thermal_index: 20,
        active_model: "".to_string(),
    };

    let encoded = packet.encode();
    assert_eq!(encoded.len(), BEACON_PACKET_SIZE);

    let decoded = BeaconPacket::decode(&encoded).expect("Failed to decode RPC beacon");
    assert!(decoded.status.is_rpc_ready());
    assert_eq!(decoded.rpc_port, 50052);
    assert_eq!(decoded.backend, AccelerationBackend::X86Baseline);
    assert_eq!(decoded.free_ram_mb, 1800);
}

#[test]
fn test_beacon_corruption_rejection() {
    let packet = BeaconPacket {
        magic: BEACON_MAGIC,
        version: BEACON_VERSION,
        role: NodeRole::HOST,
        status: StatusFlags::READY,
        uuid: Uuid::new_v4(),
        api_port: 8080,
        rpc_port: 0,
        total_ram_mb: 8000,
        free_ram_mb: 3000,
        backend: AccelerationBackend::ArmCpuDotProd,
        thermal_index: 20,
        active_model: "qwen-2.5-7b".to_string(),
    };

    let mut encoded = packet.encode();

    // 1. Corrupted checksum rejection
    encoded[62] ^= 0xFF;
    match BeaconPacket::decode(&encoded) {
        Err(DiscoveryError::ChecksumMismatch { .. }) => (),
        other => panic!("Expected ChecksumMismatch, got {:?}", other),
    }
    encoded[62] ^= 0xFF; // Restore checksum

    // 2. Corrupted magic signature rejection
    encoded[0] = 0x00;
    // Fix checksum for modified magic to ensure magic check specifically fails
    let new_crc = nexus::discovery::compute_crc16(&encoded[0..62]);
    encoded[62..64].copy_from_slice(&new_crc.to_be_bytes());
    match BeaconPacket::decode(&encoded) {
        Err(DiscoveryError::InvalidMagic(m)) => assert_ne!(m, BEACON_MAGIC),
        other => panic!("Expected InvalidMagic, got {:?}", other),
    }

    // 3. Packet size rejection
    let short_buf = [0u8; 32];
    match BeaconPacket::decode(&short_buf) {
        Err(DiscoveryError::PacketSizeMismatch {
            expected: 64,
            actual: 32,
        }) => (),
        other => panic!("Expected PacketSizeMismatch, got {:?}", other),
    }
}

#[test]
fn test_udp_v1_fixture_is_stable() {
    let uuid = Uuid::parse_str("00000000-0000-0000-0000-000000000001").unwrap();
    let packet = BeaconPacket {
        magic: BEACON_MAGIC,
        version: BEACON_VERSION,
        role: NodeRole::HOST,
        status: StatusFlags::READY,
        uuid,
        api_port: 8080,
        rpc_port: 0,
        total_ram_mb: 8192,
        free_ram_mb: 4096,
        backend: AccelerationBackend::ArmCpuDotProd,
        thermal_index: 12,
        active_model: "tiny".to_string(),
    };

    let encoded = packet.encode();
    assert_eq!(
        hex_bytes(&encoded),
        "4e58555301010001000000000000000000000000000000011f9000000000200000001000020c74696e790000000000000000000000000000000000000000c5ba"
    );
    assert_eq!(BeaconPacket::decode(&encoded).unwrap(), packet);
}

#[tokio::test]
async fn test_peer_cache_expiry_and_pruning() {
    let mut config = NexusConfig::default();
    config.network.peer_timeout_ms = 50; // 50ms timeout for fast unit testing

    let discovery = DiscoveryService::new(config, None);
    let peer_uuid = Uuid::new_v4();

    // Insert peer into cache
    {
        let peers_lock = discovery.peers();
        let mut peers = peers_lock.write().await;
        peers.insert(
            peer_uuid,
            PeerNode {
                uuid: peer_uuid,
                addr: SocketAddr::from(([192, 168, 1, 100], 8080)),
                role: NodeRole::HOST,
                status: StatusFlags::READY,
                api_port: 8080,
                control_port: 9998,
                rpc_port: 0,
                total_ram_mb: 12000,
                free_ram_mb: 6000,
                backend: AccelerationBackend::Vulkan,
                thermal_index: 30,
                active_model: "llama-3".to_string(),
                display_name: String::new(),
                last_seen: Instant::now(),
            },
        );
    }

    // Should be present immediately
    let active_before = discovery.get_active_peers().await;
    assert_eq!(active_before.len(), 1);
    assert_eq!(active_before[0].uuid, peer_uuid);

    // Sleep past the 50ms peer timeout
    tokio::time::sleep(Duration::from_millis(80)).await;

    // Must be pruned after timeout
    let active_after = discovery.get_active_peers().await;
    assert_eq!(
        active_after.len(),
        0,
        "Stale peer must be pruned from active peers"
    );
}

#[tokio::test]
async fn test_find_best_host_scoring() {
    let config = NexusConfig::default();
    let discovery = DiscoveryService::new(config, None);

    let host1_uuid = Uuid::new_v4();
    let host2_uuid = Uuid::new_v4();
    let rpc_worker_uuid = Uuid::new_v4();

    {
        let peers_lock = discovery.peers();
        let mut peers = peers_lock.write().await;

        // Host 1: CPU, 2000 MB free, thermal 50
        peers.insert(
            host1_uuid,
            PeerNode {
                uuid: host1_uuid,
                addr: SocketAddr::from(([192, 168, 1, 101], 8080)),
                role: NodeRole::HOST,
                status: StatusFlags::READY,
                api_port: 8080,
                control_port: 9998,
                rpc_port: 0,
                total_ram_mb: 4000,
                free_ram_mb: 2000,
                backend: AccelerationBackend::ArmCpuDotProd,
                thermal_index: 50,
                active_model: "qwen".to_string(),
                display_name: String::new(),
                last_seen: Instant::now(),
            },
        );

        // Host 2: Vulkan, 6000 MB free, thermal 25
        peers.insert(
            host2_uuid,
            PeerNode {
                uuid: host2_uuid,
                addr: SocketAddr::from(([192, 168, 1, 102], 8080)),
                role: NodeRole::HOST,
                status: StatusFlags(StatusFlags::READY.0 | StatusFlags::VULKAN_ACTIVE.0),
                api_port: 8080,
                control_port: 9998,
                rpc_port: 0,
                total_ram_mb: 12000,
                free_ram_mb: 6000,
                backend: AccelerationBackend::Vulkan,
                thermal_index: 25,
                active_model: "llama".to_string(),
                display_name: String::new(),
                last_seen: Instant::now(),
            },
        );

        // RPC Worker node: Client role, RPC_READY, 1800 MB free
        peers.insert(
            rpc_worker_uuid,
            PeerNode {
                uuid: rpc_worker_uuid,
                addr: SocketAddr::from(([192, 168, 1, 103], 8080)),
                role: NodeRole::CLIENT,
                status: StatusFlags(StatusFlags::READY.0 | StatusFlags::RPC_READY.0),
                api_port: 8080,
                control_port: 9998,
                rpc_port: 50052,
                total_ram_mb: 3600,
                free_ram_mb: 1800,
                backend: AccelerationBackend::X86Baseline,
                thermal_index: 10,
                active_model: "".to_string(),
                display_name: String::new(),
                last_seen: Instant::now(),
            },
        );
    }

    let best = discovery.find_best_host().await.expect("Must find a host");
    assert_eq!(
        best.uuid, host2_uuid,
        "Host 2 (Vulkan, more RAM, cooler) must score higher"
    );
    assert_eq!(best.api_endpoint(), "http://192.168.1.102:8080");

    let rpc = discovery
        .select_rpc_candidate(RpcSelectionPolicy {
            max_allocatable_mb: u64::MAX,
            ..RpcSelectionPolicy::default()
        })
        .await
        .expect("Must find RPC peer");
    assert_eq!(rpc.peer.uuid, rpc_worker_uuid);
    assert_eq!(rpc.peer.rpc_endpoint(), "192.168.1.103:50052");
    assert!(rpc.peer.is_rpc_ready());
}

#[test]
fn test_sse_chunk_deserialization() {
    let sample_chunk = r#"{
        "id": "chatcmpl-123",
        "choices": [
            {
                "index": 0,
                "delta": {
                    "role": "assistant",
                    "content": "Hello world!"
                },
                "finish_reason": null
            }
        ]
    }"#;

    let chunk: ChatCompletionChunk =
        serde_json::from_str(sample_chunk).expect("Failed to deserialize chunk");
    assert_eq!(chunk.choices.len(), 1);
    assert_eq!(
        chunk.choices[0].delta.content.as_deref(),
        Some("Hello world!")
    );
}

#[test]
fn test_chat_request_serialization() {
    let req = ChatCompletionRequest {
        model: "llama-3".to_string(),
        messages: vec![
            ChatMessage::system("You are Nexus."),
            ChatMessage::user("Hello!"),
        ],
        temperature: Some(0.8),
        top_p: Some(0.95),
        max_tokens: Some(256),
        stream: true,
    };

    let json = serde_json::to_string(&req).expect("Failed to serialize request");
    assert!(json.contains("\"stream\":true"));
    assert!(json.contains("\"model\":\"llama-3\""));
    assert!(json.contains("\"role\":\"system\""));
}

#[test]
fn test_get_broadcast_addresses_and_targets() {
    let bcast_addrs = nexus::discovery::get_broadcast_addresses();
    println!("Detected interface broadcast addresses: {:?}", bcast_addrs);

    let mut config = NexusConfig::default();
    config.network.static_peers = vec!["10.0.0.99".to_string()];
    let targets = DiscoveryService::get_broadcast_targets(&config);
    println!("Broadcast targets: {:?}", targets);

    // Must include 255.255.255.255
    let limited_bcast: SocketAddr = format!("255.255.255.255:{}", config.network.discovery_port)
        .parse()
        .unwrap();
    assert!(targets.contains(&limited_bcast));

    // Must include 127.0.0.1
    let loopback: SocketAddr = format!("127.0.0.1:{}", config.network.discovery_port)
        .parse()
        .unwrap();
    assert!(targets.contains(&loopback));

    // Must include static peer
    let static_peer: SocketAddr = format!("10.0.0.99:{}", config.network.discovery_port)
        .parse()
        .unwrap();
    assert!(targets.contains(&static_peer));

    // Must include detected interface broadcast addresses
    for bcast in bcast_addrs {
        let expected = SocketAddr::new(std::net::IpAddr::V4(bcast), config.network.discovery_port);
        assert!(targets.contains(&expected));
    }
}

#[tokio::test]
async fn test_dynamic_static_peer_addition() {
    let config = NexusConfig::default();
    let discovery = DiscoveryService::new(config, None);

    let initial_targets = discovery.broadcast_targets().await;
    assert!(!initial_targets.contains(&"192.168.1.150:9999".parse().unwrap()));

    discovery.add_static_peer("192.168.1.150:9999").await;
    let targets = discovery.broadcast_targets().await;
    assert!(targets.contains(&"192.168.1.150:9999".parse().unwrap()));

    // IP-only string should use default discovery port
    discovery.add_static_peer("10.10.10.10").await;
    let targets2 = discovery.broadcast_targets().await;
    assert!(targets2.contains(&"10.10.10.10:9999".parse().unwrap()));
}

#[tokio::test]
async fn test_record_service_endpoint_merges_mdns() {
    let config = NexusConfig::default();
    let discovery = DiscoveryService::new(config, None);

    let node_id = Uuid::new_v4();
    let endpoint = ServiceEndpoint {
        node_id,
        cluster_id: None,
        protocol_version: 1,
        role: NodeRole::HOST,
        capabilities: vec!["inference".to_string()],
        addresses: vec!["192.168.1.200:8080".parse().unwrap()],
        api_port: 8080,
        control_port: 9998,
        display_name: "galaxy-s23".to_string(),
        rpc_port: 50052,
    };

    discovery
        .record_service_endpoint(endpoint, ObservationSource::Mdns)
        .await;

    let peers = discovery.get_active_peers().await;
    assert_eq!(peers.len(), 1);
    assert_eq!(peers[0].uuid, node_id);
    assert!(peers[0].role.is_host());
    assert!(peers[0].is_rpc_ready());
    assert_eq!(peers[0].api_endpoint(), "http://192.168.1.200:8080");
    assert_eq!(peers[0].rpc_endpoint(), "192.168.1.200:50052");
    assert_eq!(peers[0].display_name, "galaxy-s23");
    assert_eq!(peers[0].label(), "galaxy-s23");

    // Also check registry
    let registry = discovery.peer_registry();
    let reg_read = registry.read().await;
    assert!(reg_read.get(node_id).is_some());
}

#[test]
fn test_peer_label_prefers_display_name() {
    let uuid = Uuid::parse_str("aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee").unwrap();
    let mut peer = PeerNode {
        uuid,
        addr: SocketAddr::from(([127, 0, 0, 1], 8080)),
        role: NodeRole::HOST,
        status: StatusFlags::READY,
        api_port: 8080,
        control_port: 9998,
        rpc_port: 0,
        total_ram_mb: 1000,
        free_ram_mb: 500,
        backend: AccelerationBackend::GenericCpu,
        thermal_index: 0,
        active_model: String::new(),
        display_name: String::new(),
        last_seen: Instant::now(),
    };
    assert_eq!(peer.label(), "Node-aaaaaaaa");
    peer.display_name = "macrowave".to_string();
    assert_eq!(peer.label(), "macrowave");
}

#[test]
fn test_resolved_display_name_uses_configured_name() {
    let mut config = NexusConfig::default();
    config.node.name = "studio-box".to_string();
    assert_eq!(config.resolved_display_name(), "studio-box");
    config.node.name = "auto".to_string();
    let resolved = config.resolved_display_name();
    assert!(!resolved.is_empty());
    assert_ne!(resolved, "auto");
}

#[tokio::test]
async fn test_resolve_from_discovery_falls_back_to_best_host() {
    use nexus::client::NexusClient;
    let config = NexusConfig::default();
    assert!(config.network.anchors.primary_compute_id.is_none());
    let discovery = DiscoveryService::new(config, None);
    let host_id = Uuid::new_v4();
    discovery.peers().write().await.insert(
        host_id,
        PeerNode {
            uuid: host_id,
            addr: SocketAddr::from(([10, 0, 0, 42], 8080)),
            role: NodeRole::HOST,
            status: StatusFlags::READY,
            api_port: 8080,
            control_port: 9998,
            rpc_port: 0,
            total_ram_mb: 8000,
            free_ram_mb: 4000,
            backend: AccelerationBackend::Vulkan,
            thermal_index: 10,
            active_model: "demo".to_string(),
            display_name: "lan-host".to_string(),
            last_seen: Instant::now(),
        },
    );

    let client = NexusClient::resolve_from_discovery(&discovery, Duration::from_secs(1))
        .await
        .expect("default config must resolve a ready beacon host");
    assert_eq!(client.endpoint(), "http://10.0.0.42:8080");
}

#[tokio::test]
async fn test_backend_health_tracking() {
    let config = NexusConfig::default();
    let discovery = DiscoveryService::new(config, None);

    assert_eq!(*discovery.udp_health().read().await, BackendHealth::Started);
    assert_eq!(
        *discovery.mdns_health().read().await,
        BackendHealth::Started
    );

    discovery.set_udp_health(BackendHealth::Healthy).await;
    assert_eq!(*discovery.udp_health().read().await, BackendHealth::Healthy);

    discovery.set_mdns_health(BackendHealth::Healthy).await;
    assert_eq!(
        *discovery.mdns_health().read().await,
        BackendHealth::Healthy
    );
}

/// Adversarial / truncated / random 64-byte buffers must never panic.
#[test]
fn test_beacon_decode_never_panics_on_random_bytes() {
    use proptest::prelude::*;

    proptest!(|(bytes in prop::collection::vec(any::<u8>(), 0..128))| {
        let _ = BeaconPacket::decode(&bytes);
    });
}

#[test]
fn test_beacon_decode_fixed_size_random_never_panics() {
    use proptest::prelude::*;

    proptest!(|(bytes in prop::array::uniform32(any::<u8>()), extra in prop::array::uniform32(any::<u8>()))| {
        let mut buf = [0u8; 64];
        buf[..32].copy_from_slice(&bytes);
        buf[32..].copy_from_slice(&extra);
        let _ = BeaconPacket::decode(&buf);
    });
}

#[test]
fn test_beacon_roundtrip_property() {
    use proptest::prelude::*;

    proptest!(|(
        role in 0u8..=7u8,
        status in any::<u16>(),
        api_port in any::<u16>(),
        rpc_port in any::<u16>(),
        total_ram_mb in any::<u32>(),
        free_ram_mb in any::<u32>(),
        thermal_index in any::<u8>(),
        model in "[a-zA-Z0-9._-]{0,40}",
    )| {
        let packet = BeaconPacket {
            magic: BEACON_MAGIC,
            version: BEACON_VERSION,
            role: NodeRole(role),
            status: StatusFlags(status),
            uuid: Uuid::nil(),
            api_port,
            rpc_port,
            total_ram_mb,
            free_ram_mb,
            backend: AccelerationBackend::GenericCpu,
            thermal_index,
            active_model: model,
        };
        let encoded = packet.encode();
        let decoded = BeaconPacket::decode(&encoded).expect("valid encode must decode");
        assert_eq!(decoded.magic, packet.magic);
        assert_eq!(decoded.version, packet.version);
        assert_eq!(decoded.role, packet.role);
        assert_eq!(decoded.status, packet.status);
        assert_eq!(decoded.api_port, packet.api_port);
        assert_eq!(decoded.rpc_port, packet.rpc_port);
        assert_eq!(decoded.total_ram_mb, packet.total_ram_mb);
        assert_eq!(decoded.free_ram_mb, packet.free_ram_mb);
        assert_eq!(decoded.thermal_index, packet.thermal_index);
        // Model field is 24-byte null-padded ASCII on the wire.
        let expected_model = {
            let bytes = packet.active_model.as_bytes();
            let copy_len = bytes.len().min(24);
            String::from_utf8_lossy(&bytes[..copy_len])
                .trim_end_matches('\0')
                .to_string()
        };
        assert_eq!(decoded.active_model, expected_model);
    });
}
