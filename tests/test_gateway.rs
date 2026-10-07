//! Phase 12 §5.1 mesh gateway: model→holder resolution + OpenAI proxy.

mod support;

use futures_util::StreamExt;
use nexus::client::{ChatCompletionRequest, ChatMessage, NexusClient};
use nexus::config::NexusConfig;
use nexus::discovery::{DiscoveryService, NodeRole, PeerNode, StatusFlags};
use nexus::gateway::{
    model_matches, resolve_model_upstream, spawn_ephemeral, GatewayContext, UpstreamSource,
};
use nexus::supervisor::SupervisorManager;
use nexus::sysinfo::AccelerationBackend;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Instant;
use support::fake_llama::{spawn_fake_llama, FakeLlamaConfig};
use tempfile::TempDir;
use uuid::Uuid;

fn gateway_test_config() -> NexusConfig {
    let mut cfg = NexusConfig::default();
    cfg.network.gateway_enabled = true;
    cfg.network.gateway_port = 18081;
    cfg.network.api_port = 18080;
    cfg.network.control_port = 19998;
    cfg.network.discovery_port = 19999;
    cfg.network.security.require_pairing = false;
    cfg.network.discovery.mdns.enabled = false;
    cfg.validate().expect("gateway test config");
    cfg
}

#[tokio::test]
async fn gateway_health_and_models_list() {
    let (fake_base, fake_handle) = spawn_fake_llama(FakeLlamaConfig {
        model_id: "local-phi".into(),
        ..Default::default()
    })
    .await
    .expect("fake llama");

    let cfg = gateway_test_config();
    let models_dir = TempDir::new().expect("tmpdir");
    let discovery = Arc::new(DiscoveryService::new(cfg.clone(), None));
    discovery.set_active_model("local-phi").await;
    discovery.set_status_flags(StatusFlags::READY).await;

    let ctx = Arc::new(
        GatewayContext::new(
            SupervisorManager::new(),
            cfg.network.api_port,
            models_dir.path().to_path_buf(),
            discovery.node_uuid(),
            Arc::new(std::sync::RwLock::new(cfg)),
        )
        .with_discovery(discovery)
        .with_local_api_base(&fake_base),
    );

    let (addr, gw_handle) = spawn_ephemeral(ctx).await.expect("gateway bind");
    let client = NexusClient::new(format!("http://{}", addr));

    assert!(client.health().await.expect("health"));
    let models = client.models().await.expect("models");
    assert!(
        models.iter().any(|m| m == "local-phi"),
        "expected local-phi in {models:?}"
    );

    gw_handle.abort();
    fake_handle.abort();
}

#[tokio::test]
async fn gateway_routes_stream_to_matching_peer() {
    let (base_a, handle_a) = spawn_fake_llama(FakeLlamaConfig {
        model_id: "model-a".into(),
        sse_tokens: vec!["AAA".into()],
    })
    .await
    .expect("fake a");
    let (base_b, handle_b) = spawn_fake_llama(FakeLlamaConfig {
        model_id: "model-b".into(),
        sse_tokens: vec!["BBB".into()],
    })
    .await
    .expect("fake b");

    let port_a: u16 = base_a
        .trim_start_matches("http://127.0.0.1:")
        .parse()
        .expect("port a");
    let port_b: u16 = base_b
        .trim_start_matches("http://127.0.0.1:")
        .parse()
        .expect("port b");

    let cfg = gateway_test_config();
    let models_dir = TempDir::new().expect("tmpdir");
    let discovery = Arc::new(DiscoveryService::new(cfg.clone(), None));

    let id_a = Uuid::new_v4();
    let id_b = Uuid::new_v4();
    let peer_a = PeerNode {
        uuid: id_a,
        addr: SocketAddr::from(([127, 0, 0, 1], port_a)),
        role: NodeRole::HOST,
        status: StatusFlags::READY,
        api_port: port_a,
        rpc_port: 0,
        control_port: 9998,
        total_ram_mb: 8192,
        free_ram_mb: 4096,
        backend: AccelerationBackend::GenericCpu,
        thermal_index: 20,
        active_model: "model-a".into(),
        display_name: "peer-a".into(),
        last_seen: Instant::now(),
    };
    let peer_b = PeerNode {
        uuid: id_b,
        addr: SocketAddr::from(([127, 0, 0, 1], port_b)),
        role: NodeRole::HOST,
        status: StatusFlags::READY,
        api_port: port_b,
        rpc_port: 0,
        control_port: 9998,
        total_ram_mb: 8192,
        free_ram_mb: 2048,
        backend: AccelerationBackend::GenericCpu,
        thermal_index: 30,
        active_model: "model-b".into(),
        display_name: "peer-b".into(),
        last_seen: Instant::now(),
    };

    {
        let peers = discovery.peers();
        let mut map = peers.write().await;
        map.insert(id_a, peer_a);
        map.insert(id_b, peer_b);
    }

    let ctx = Arc::new(
        GatewayContext::new(
            SupervisorManager::new(),
            cfg.network.api_port,
            models_dir.path().to_path_buf(),
            discovery.node_uuid(),
            Arc::new(std::sync::RwLock::new(cfg)),
        )
        .with_discovery(discovery),
    );

    let (addr, gw_handle) = spawn_ephemeral(ctx).await.expect("gateway");
    let client = NexusClient::new(format!("http://{}", addr));

    let mut stream_a = client
        .stream_chat(ChatCompletionRequest {
            model: "model-a".into(),
            messages: vec![ChatMessage::user("ping")],
            temperature: None,
            top_p: None,
            max_tokens: Some(8),
            stream: true,
        })
        .await
        .expect("stream a");
    let mut out_a = String::new();
    while let Some(tok) = stream_a.next().await {
        out_a.push_str(&tok.expect("token"));
    }
    assert_eq!(out_a, "AAA");

    let mut stream_b = client
        .stream_chat(ChatCompletionRequest {
            model: "model-b".into(),
            messages: vec![ChatMessage::user("ping")],
            temperature: None,
            top_p: None,
            max_tokens: Some(8),
            stream: true,
        })
        .await
        .expect("stream b");
    let mut out_b = String::new();
    while let Some(tok) = stream_b.next().await {
        out_b.push_str(&tok.expect("token"));
    }
    assert_eq!(out_b, "BBB");

    gw_handle.abort();
    handle_a.abort();
    handle_b.abort();
}

#[tokio::test]
async fn gateway_prefers_local_over_peer() {
    let (local_base, local_handle) = spawn_fake_llama(FakeLlamaConfig {
        model_id: "shared-model".into(),
        sse_tokens: vec!["LOCAL".into()],
    })
    .await
    .expect("local fake");
    let (peer_base, peer_handle) = spawn_fake_llama(FakeLlamaConfig {
        model_id: "shared-model".into(),
        sse_tokens: vec!["PEER".into()],
    })
    .await
    .expect("peer fake");

    let peer_port: u16 = peer_base
        .trim_start_matches("http://127.0.0.1:")
        .parse()
        .expect("peer port");

    let cfg = gateway_test_config();
    let models_dir = TempDir::new().expect("tmpdir");
    let discovery = Arc::new(DiscoveryService::new(cfg.clone(), None));
    discovery.set_active_model("shared-model").await;

    let peer_id = Uuid::new_v4();
    {
        let peers = discovery.peers();
        let mut map = peers.write().await;
        map.insert(
            peer_id,
            PeerNode {
                uuid: peer_id,
                addr: SocketAddr::from(([127, 0, 0, 1], peer_port)),
                role: NodeRole::HOST,
                status: StatusFlags::READY,
                api_port: peer_port,
                rpc_port: 0,
                control_port: 9998,
                total_ram_mb: 8192,
                free_ram_mb: 8000,
                backend: AccelerationBackend::Vulkan,
                thermal_index: 10,
                active_model: "shared-model".into(),
                display_name: "remote".into(),
                last_seen: Instant::now(),
            },
        );
    }

    let ctx = Arc::new(
        GatewayContext::new(
            SupervisorManager::new(),
            cfg.network.api_port,
            models_dir.path().to_path_buf(),
            discovery.node_uuid(),
            Arc::new(std::sync::RwLock::new(cfg)),
        )
        .with_discovery(discovery)
        .with_local_api_base(&local_base),
    );

    let resolved = resolve_model_upstream(&ctx, "shared-model")
        .await
        .expect("resolve");
    assert_eq!(resolved.source, UpstreamSource::Local);
    assert_eq!(
        resolved.base_url,
        local_base.trim_end_matches('/').to_string()
    );

    let (addr, gw_handle) = spawn_ephemeral(ctx).await.expect("gateway");
    let client = NexusClient::new(format!("http://{}", addr));
    let mut stream = client
        .stream_chat(ChatCompletionRequest {
            model: "shared-model".into(),
            messages: vec![ChatMessage::user("x")],
            temperature: None,
            top_p: None,
            max_tokens: Some(8),
            stream: true,
        })
        .await
        .expect("stream");
    let mut out = String::new();
    while let Some(tok) = stream.next().await {
        out.push_str(&tok.expect("token"));
    }
    assert_eq!(out, "LOCAL");

    gw_handle.abort();
    local_handle.abort();
    peer_handle.abort();
}

#[tokio::test]
async fn gateway_404_when_no_holder() {
    let cfg = gateway_test_config();
    let models_dir = TempDir::new().expect("tmpdir");
    let discovery = Arc::new(DiscoveryService::new(cfg.clone(), None));
    let ctx = Arc::new(
        GatewayContext::new(
            SupervisorManager::new(),
            cfg.network.api_port,
            models_dir.path().to_path_buf(),
            discovery.node_uuid(),
            Arc::new(std::sync::RwLock::new(cfg)),
        )
        .with_discovery(discovery),
    );

    assert!(resolve_model_upstream(&ctx, "missing-model")
        .await
        .is_none());

    let (addr, gw_handle) = spawn_ephemeral(ctx).await.expect("gateway");
    let client = reqwest::Client::new();
    let resp = client
        .post(format!("http://{}/v1/chat/completions", addr))
        .json(&serde_json::json!({
            "model": "missing-model",
            "messages": [{"role": "user", "content": "hi"}],
            "stream": false
        }))
        .send()
        .await
        .expect("post");
    assert_eq!(resp.status(), reqwest::StatusCode::NOT_FOUND);

    gw_handle.abort();
}

#[tokio::test]
async fn gateway_skips_untrusted_peer_when_pairing_enforced() {
    let (peer_base, peer_handle) = spawn_fake_llama(FakeLlamaConfig {
        model_id: "secret-model".into(),
        sse_tokens: vec!["SECRET".into()],
    })
    .await
    .expect("peer");

    let peer_port: u16 = peer_base
        .trim_start_matches("http://127.0.0.1:")
        .parse()
        .expect("port");

    let mut cfg = gateway_test_config();
    cfg.network.security.require_pairing = true;

    let models_dir = TempDir::new().expect("tmpdir");
    let discovery = Arc::new(DiscoveryService::new(cfg.clone(), None));
    let peer_id = Uuid::new_v4();
    {
        let peers = discovery.peers();
        let mut map = peers.write().await;
        map.insert(
            peer_id,
            PeerNode {
                uuid: peer_id,
                addr: SocketAddr::from(([127, 0, 0, 1], peer_port)),
                role: NodeRole::HOST,
                status: StatusFlags::READY,
                api_port: peer_port,
                rpc_port: 0,
                control_port: 9998,
                total_ram_mb: 8192,
                free_ram_mb: 4000,
                backend: AccelerationBackend::GenericCpu,
                thermal_index: 10,
                active_model: "secret-model".into(),
                display_name: "untrusted".into(),
                last_seen: Instant::now(),
            },
        );
    }

    let ctx = GatewayContext::new(
        SupervisorManager::new(),
        cfg.network.api_port,
        models_dir.path().to_path_buf(),
        discovery.node_uuid(),
        Arc::new(std::sync::RwLock::new(cfg)),
    )
    .with_discovery(discovery);

    assert!(
        resolve_model_upstream(&ctx, "secret-model").await.is_none(),
        "untrusted peer must not be selected when pairing is enforced"
    );

    peer_handle.abort();
}

#[test]
fn model_matches_unit() {
    assert!(model_matches("foo", "foo.gguf"));
    assert!(model_matches("Foo.GGUF", "foo.gguf"));
    assert!(!model_matches("bar", "foo.gguf"));
}

#[test]
fn gateway_port_defaults_and_validation() {
    let cfg = NexusConfig::default();
    assert_eq!(cfg.network.gateway_port, 8090);
    assert!(cfg.network.gateway_enabled);
    assert_ne!(cfg.network.gateway_port, cfg.network.api_port);
    assert_ne!(cfg.network.gateway_port, cfg.network.control_port);
    cfg.validate().expect("defaults valid");

    let mut bad = NexusConfig::default();
    bad.network.gateway_port = bad.network.api_port;
    assert!(bad.validate().is_err());

    let mut disabled = NexusConfig::default();
    disabled.network.gateway_enabled = false;
    disabled.network.gateway_port = disabled.network.api_port;
    disabled.validate().expect("disabled gateway may collide");
}
