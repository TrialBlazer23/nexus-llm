//! Phase 9 trust: signatures, pairing codes, control-plane auth, routing.

use nexus::config::{NexusConfig, SecurityConfig};
use nexus::control_plane::{
    build_control_plane_state, dispatch_load_model, dispatch_load_model_signed, ModelLoadRequest,
    PairRequest, CONTROL_PLANE_VERSION,
};
use nexus::control_plane_server::{spawn_ephemeral, ControlPlaneContext};
use nexus::discovery::{DiscoveryService, NodeRole, PeerNode, StatusFlags};
use nexus::node_identity::{NodeIdentity, PAIRING_CODE_WINDOW_SECS};
use nexus::supervisor::SupervisorManager;
use nexus::sysinfo::AccelerationBackend;
use nexus::trust_auth::{
    authorize_privileged_signer, canonical_signing_message, pairing_enforced,
    verify_control_request, AuthError, NonceCache, TrustBootstrap, HDR_NONCE, HDR_SIGNATURE,
    HDR_SIGNER, HDR_TIMESTAMP,
};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Instant;
use tempfile::TempDir;
use uuid::Uuid;

fn test_ctx(trust: &TrustBootstrap, node_id: Uuid) -> Arc<ControlPlaneContext> {
    Arc::new(
        ControlPlaneContext::new(
            node_id,
            NodeRole::HOST,
            SupervisorManager::new(),
            "127.0.0.1",
            18080,
            PathBuf::from("llama-server"),
            trust.identity.clone(),
            trust.config.clone(),
            trust.config_path.clone(),
        )
        .with_capabilities(vec!["inference".to_string()]),
    )
}

#[test]
fn canonical_signature_round_trip() {
    let id = NodeIdentity::generate();
    let signer = Uuid::new_v4();
    let body = br#"{"hello":"world"}"#;
    let msg = canonical_signing_message(
        "POST",
        "/nexus/control/v1/state",
        body,
        1_700_000_000,
        "abcd1234abcd1234",
        signer,
    );
    let sig = id.sign(msg.as_bytes());
    assert!(nexus::node_identity::verify_signature(
        &id.public_key_bytes(),
        msg.as_bytes(),
        &sig
    ));
}

#[test]
fn stale_timestamp_rejected() {
    let mut headers = http::HeaderMap::new();
    headers.insert(HDR_TIMESTAMP, "1".parse().unwrap());
    headers.insert(HDR_NONCE, "abcd1234abcd1234".parse().unwrap());
    headers.insert(HDR_SIGNER, Uuid::new_v4().to_string().parse().unwrap());
    headers.insert(HDR_SIGNATURE, "00".repeat(64).parse().unwrap());
    let security = SecurityConfig::default();
    let cache = Mutex::new(NonceCache::default());
    let err = verify_control_request(
        &headers,
        "POST",
        "/nexus/control/v1/state",
        b"{}",
        &security,
        &cache,
        None,
        false,
        Some("127.0.0.1".parse().unwrap()),
    )
    .unwrap_err();
    assert_eq!(err, AuthError::StaleTimestamp);
}

#[test]
fn replay_nonce_rejected() {
    let identity = NodeIdentity::generate();
    let signer = Uuid::new_v4();
    let body = b"{}";
    let timestamp = nexus::trust_auth::unix_timestamp_now();
    let nonce = "cafebabecafebabe";
    let sig = nexus::node_identity::hex_encode(
        &identity.sign(
            canonical_signing_message(
                "POST",
                "/nexus/control/v1/state",
                body,
                timestamp,
                nonce,
                signer,
            )
            .as_bytes(),
        ),
    );
    let mut headers = http::HeaderMap::new();
    headers.insert(HDR_TIMESTAMP, timestamp.to_string().parse().unwrap());
    headers.insert(HDR_NONCE, nonce.parse().unwrap());
    headers.insert(HDR_SIGNER, signer.to_string().parse().unwrap());
    headers.insert(HDR_SIGNATURE, sig.parse().unwrap());
    let mut security = SecurityConfig::default();
    security.paired_peers.push(nexus::config::PairedPeer {
        node_id: signer,
        public_key_hex: identity.public_key_hex(),
    });
    let cache = Mutex::new(NonceCache::default());
    verify_control_request(
        &headers,
        "POST",
        "/nexus/control/v1/state",
        body,
        &security,
        &cache,
        None,
        false,
        Some("127.0.0.1".parse().unwrap()),
    )
    .unwrap();
    let err = verify_control_request(
        &headers,
        "POST",
        "/nexus/control/v1/state",
        body,
        &security,
        &cache,
        None,
        false,
        Some("127.0.0.1".parse().unwrap()),
    )
    .unwrap_err();
    assert_eq!(err, AuthError::ReplayNonce);
}

#[test]
fn pairing_code_window_verification() {
    let id = NodeIdentity::generate();
    let now = 1_700_000_000u64;
    let code = id.current_pairing_code(now);
    assert!(id.verify_pairing_code_at(now, &code));
    assert!(id.verify_pairing_code_at(now + PAIRING_CODE_WINDOW_SECS, &code));
}

#[tokio::test]
async fn control_plane_rejects_unsigned_load_when_pairing_enforced() {
    let dir = TempDir::new().unwrap();
    let config_path = dir.path().join("config.toml");
    let mut config = NexusConfig::default();
    config.network.security.require_pairing = true;
    config.network.security.allowed_peer_ids = vec![Uuid::new_v4()];
    config.save_to_path(&config_path).unwrap();
    std::env::set_var("NEXUS_CONFIG", config_path.to_str().unwrap());
    let trust = TrustBootstrap::load(config).unwrap();
    let node_id = Uuid::new_v4();
    let ctx = test_ctx(&trust, node_id);
    let (addr, handle) = spawn_ephemeral(ctx).await.unwrap();
    let base = format!("http://{addr}");
    let client = reqwest::Client::new();
    let err = dispatch_load_model(
        &client,
        &base,
        &ModelLoadRequest {
            protocol_version: CONTROL_PLANE_VERSION,
            requester_id: Uuid::new_v4(),
            model_path: "x.gguf".into(),
            context_size: 512,
            gpu_layers: 0,
            threads: 2,
            rpc_workers: vec![],
            tags: vec![],
            target_port: None,
            backend: "auto".to_string(),
        },
    )
    .await
    .unwrap_err();
    let msg = err.to_string();
    assert!(
        msg.contains("403") || msg.contains("401") || msg.contains("PairingRequired"),
        "expected auth failure, got: {msg}"
    );
    handle.abort();
    std::env::remove_var("NEXUS_CONFIG");
}

#[tokio::test]
async fn pairing_grants_remote_load() {
    let dir = TempDir::new().unwrap();
    let config_path = dir.path().join("config.toml");
    let server_config = NexusConfig::default();
    server_config.save_to_path(&config_path).unwrap();
    std::env::set_var("NEXUS_CONFIG", config_path.to_str().unwrap());
    let server_trust = TrustBootstrap::load(NexusConfig::default()).unwrap();
    let server_id = Uuid::new_v4();
    let ctx = test_ctx(&server_trust, server_id);
    let (addr, handle) = spawn_ephemeral(ctx).await.unwrap();
    let base = format!("http://{addr}");

    let client_trust = TrustBootstrap::load(NexusConfig::default()).unwrap();
    let requester_id = client_trust.config.read().unwrap().node_uuid().unwrap();
    let code = server_trust.identity.current_pairing_code(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs(),
    );
    let pair_req = PairRequest {
        protocol_version: CONTROL_PLANE_VERSION,
        requester_id,
        requester_public_key: client_trust.identity.public_key_hex(),
        pairing_code: code,
    };
    let client = reqwest::Client::new();
    let pair_resp =
        nexus::control_plane::dispatch_pair(&client, &base, &pair_req, &client_trust.identity)
            .await
            .expect("pair");
    assert!(pair_resp.success);

    let load_req = ModelLoadRequest {
        protocol_version: CONTROL_PLANE_VERSION,
        requester_id,
        model_path: "missing.gguf".into(),
        context_size: 512,
        gpu_layers: 0,
        threads: 2,
        rpc_workers: vec![],
        tags: vec![],
        target_port: None,
        backend: "auto".to_string(),
    };
    let load = dispatch_load_model_signed(&client, &base, &load_req, &client_trust.identity)
        .await
        .expect("signed load allowed after pair");
    assert!(!load.success);
    handle.abort();
    std::env::remove_var("NEXUS_CONFIG");
}

#[tokio::test]
async fn forged_beacon_does_not_enable_trusted_connect() {
    let mut config = NexusConfig::default();
    config.network.security.require_pairing = true;
    let legit_id = Uuid::new_v4();
    config.network.security.allowed_peer_ids = vec![legit_id];
    config
        .network
        .security
        .paired_peers
        .push(nexus::config::PairedPeer {
            node_id: legit_id,
            public_key_hex: NodeIdentity::generate().public_key_hex(),
        });
    let discovery = DiscoveryService::new(config, Some(Uuid::new_v4()));
    let attacker_ip = "192.168.1.66".parse().unwrap();
    discovery.peers().write().await.insert(
        legit_id,
        PeerNode {
            uuid: legit_id,
            addr: SocketAddr::new(attacker_ip, 8080),
            role: NodeRole::HOST,
            status: StatusFlags::READY,
            api_port: 8080,
            rpc_port: 0,
            control_port: 9998,
            total_ram_mb: 8192,
            free_ram_mb: 6000,
            backend: AccelerationBackend::GenericCpu,
            thermal_index: 0,
            active_model: String::new(),
            display_name: "forged".into(),
            moe_stream: false,
            moe_cache_ceil_mb: 0,
            last_seen: Instant::now(),
        },
    );
    assert!(
        !discovery.is_peer_trusted_for_routing(legit_id).await,
        "beacon-only peer must not be trusted without registry verification"
    );
}

#[test]
fn authorize_privileged_requires_allowlist_when_enforced() {
    let security = SecurityConfig {
        require_pairing: true,
        allowed_peer_ids: vec![Uuid::new_v4()],
        ..Default::default()
    };
    assert!(pairing_enforced(&security));
    assert!(authorize_privileged_signer(&security, Uuid::new_v4()).is_err());
}

#[tokio::test]
async fn build_state_includes_signing_public_key() {
    let state = build_control_plane_state(
        Uuid::new_v4(),
        NodeRole::HOST,
        vec!["inference".into()],
        &SupervisorManager::new(),
        1024,
        false,
        Some("abcd".into()),
    )
    .await;
    assert_eq!(state.signing_public_key.as_deref(), Some("abcd"));
}
