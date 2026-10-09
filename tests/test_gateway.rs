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
        moe_stream: false,
        moe_cache_ceil_mb: 0,
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
        moe_stream: false,
        moe_cache_ceil_mb: 0,
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
                moe_stream: false,
                moe_cache_ceil_mb: 0,
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
                moe_stream: false,
                moe_cache_ceil_mb: 0,
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

#[tokio::test]
async fn gateway_serves_web_ui_and_api() {
    let cfg = gateway_test_config();
    let models_dir = TempDir::new().expect("tmpdir");
    let ctx = Arc::new(
        GatewayContext::new(
            SupervisorManager::new(),
            cfg.network.api_port,
            models_dir.path().to_path_buf(),
            Uuid::new_v4(),
            Arc::new(std::sync::RwLock::new(cfg)),
        )
        .with_pin("123456"),
    );

    let (addr, gw_handle) = spawn_ephemeral(ctx).await.expect("gateway bind");
    let client = reqwest::Client::new();

    // 1. GET / serves embedded index.html
    let resp = client
        .get(format!("http://{}/", addr))
        .send()
        .await
        .expect("get /");
    assert_eq!(resp.status(), reqwest::StatusCode::OK);
    let content_type = resp
        .headers()
        .get("content-type")
        .unwrap()
        .to_str()
        .unwrap();
    assert!(content_type.contains("text/html"));
    let body = resp.text().await.expect("html text");
    assert!(body.contains("Nexus-LLM"));
    assert!(body.contains("Mesh Hub"));

    // 2. GET /manifest.json serves PWA manifest
    let resp = client
        .get(format!("http://{}/manifest.json", addr))
        .send()
        .await
        .expect("get manifest");
    assert_eq!(resp.status(), reqwest::StatusCode::OK);
    let body = resp.text().await.expect("manifest text");
    assert!(body.contains("Nexus-LLM Hub"));

    // 3. GET /api/system/profile
    let resp = client
        .get(format!("http://{}/api/system/profile", addr))
        .send()
        .await
        .expect("profile");
    assert_eq!(resp.status(), reqwest::StatusCode::OK);
    let json: serde_json::Value = resp.json().await.expect("json");
    assert!(json.get("total_ram_mb").is_some());
    assert!(json.get("available_ram_mb").is_some());

    // 4. GET /api/cluster/nodes
    let resp = client
        .get(format!("http://{}/api/cluster/nodes", addr))
        .send()
        .await
        .expect("nodes");
    assert_eq!(resp.status(), reqwest::StatusCode::OK);
    let nodes: Vec<serde_json::Value> = resp.json().await.expect("nodes json");
    assert!(!nodes.is_empty());
    assert_eq!(nodes[0]["is_local"], true);

    // 5. POST /api/auth/verify with wrong pin
    let resp = client
        .post(format!("http://{}/api/auth/verify", addr))
        .json(&serde_json::json!({"pin": "999999"}))
        .send()
        .await
        .expect("auth wrong");
    assert_eq!(resp.status(), reqwest::StatusCode::UNAUTHORIZED);

    // 6. POST /api/auth/verify with correct pin
    let resp = client
        .post(format!("http://{}/api/auth/verify", addr))
        .json(&serde_json::json!({"pin": "123456"}))
        .send()
        .await
        .expect("auth correct");
    assert_eq!(resp.status(), reqwest::StatusCode::OK);
    let auth_data: serde_json::Value = resp.json().await.expect("auth json");
    assert!(auth_data["token"].is_string());

    // 7. GET /api/config
    let resp = client
        .get(format!("http://{}/api/config", addr))
        .send()
        .await
        .expect("get config");
    assert_eq!(resp.status(), reqwest::StatusCode::OK);
    let cfg_json: serde_json::Value = resp.json().await.expect("config json");
    assert_eq!(cfg_json["inference"]["prompt_cache_enabled"], true);
    assert_eq!(cfg_json["safety"]["battery_floor_percent"], 20);

    // 8. POST /api/config without auth (rejected with 401)
    let resp = client
        .post(format!("http://{}/api/config", addr))
        .json(&serde_json::json!({
            "prompt_cache_enabled": false,
            "battery_floor_percent": 30
        }))
        .send()
        .await
        .expect("post config no auth");
    assert_eq!(resp.status(), reqwest::StatusCode::UNAUTHORIZED);

    // 9. POST /api/config with auth token
    let token = auth_data["token"].as_str().unwrap();
    let resp = client
        .post(format!("http://{}/api/config", addr))
        .header("Authorization", format!("Bearer {}", token))
        .json(&serde_json::json!({
            "prompt_cache_enabled": false,
            "battery_floor_percent": 30,
            "max_cache_mb": 4096
        }))
        .send()
        .await
        .expect("post config with auth");
    assert_eq!(resp.status(), reqwest::StatusCode::OK);
    let updated_cfg: serde_json::Value = resp.json().await.expect("updated json");
    assert_eq!(updated_cfg["inference"]["prompt_cache_enabled"], false);
    assert_eq!(updated_cfg["inference"]["max_cache_mb"], 4096);
    assert_eq!(updated_cfg["safety"]["battery_floor_percent"], 30);

    // 10. GET /api/models/download/status
    let resp = client
        .get(format!("http://{}/api/models/download/status", addr))
        .send()
        .await
        .expect("download status");
    assert_eq!(resp.status(), reqwest::StatusCode::OK);
    let dl_status: serde_json::Value = resp.json().await.expect("dl status json");
    assert_eq!(dl_status["status"], "idle");

    // 11. GET /api/models/{id}/inspect for non-existent model (404)
    let resp = client
        .get(format!(
            "http://{}/api/models/nonexistent.gguf/inspect",
            addr
        ))
        .send()
        .await
        .expect("inspect 404");
    assert_eq!(resp.status(), reqwest::StatusCode::NOT_FOUND);

    // 12. Create a minimal dummy GGUF and test GET /api/models/{id}/inspect (200)
    let dummy_path = models_dir.path().join("test-model.gguf");
    let mut dummy_buf = Vec::new();
    dummy_buf.extend_from_slice(&nexus::gguf::GGUF_MAGIC.to_le_bytes()); // Magic
    dummy_buf.extend_from_slice(&3u32.to_le_bytes()); // Version 3
    dummy_buf.extend_from_slice(&0u64.to_le_bytes()); // 0 tensors
    dummy_buf.extend_from_slice(&1u64.to_le_bytes()); // 1 KV
    let key = "general.architecture";
    dummy_buf.extend_from_slice(&(key.len() as u64).to_le_bytes());
    dummy_buf.extend_from_slice(key.as_bytes());
    dummy_buf.extend_from_slice(&8u32.to_le_bytes()); // String type
    let val = "llama";
    dummy_buf.extend_from_slice(&(val.len() as u64).to_le_bytes());
    dummy_buf.extend_from_slice(val.as_bytes());
    std::fs::write(&dummy_path, &dummy_buf).expect("write dummy gguf");

    let resp = client
        .get(format!(
            "http://{}/api/models/test-model.gguf/inspect",
            addr
        ))
        .send()
        .await
        .expect("inspect dummy");
    assert_eq!(resp.status(), reqwest::StatusCode::OK);
    let inspect_data: serde_json::Value = resp.json().await.expect("inspect json");
    assert_eq!(inspect_data["architecture"], "llama");
    assert_eq!(inspect_data["filename"], "test-model.gguf");

    gw_handle.abort();
}

#[tokio::test]
async fn gateway_resolves_model_by_sha256_digest() {
    let cfg = gateway_test_config();
    let models_dir = TempDir::new().expect("tmpdir");
    let dummy_path = models_dir.path().join("dummy-llm.gguf");

    let mut dummy_buf = Vec::new();
    dummy_buf.extend_from_slice(&nexus::gguf::GGUF_MAGIC.to_le_bytes());
    dummy_buf.extend_from_slice(&3u32.to_le_bytes());
    dummy_buf.extend_from_slice(&0u64.to_le_bytes());
    dummy_buf.extend_from_slice(&1u64.to_le_bytes());
    let key = "general.architecture";
    dummy_buf.extend_from_slice(&(key.len() as u64).to_le_bytes());
    dummy_buf.extend_from_slice(key.as_bytes());
    dummy_buf.extend_from_slice(&8u32.to_le_bytes());
    let val = "llama";
    dummy_buf.extend_from_slice(&(val.len() as u64).to_le_bytes());
    dummy_buf.extend_from_slice(val.as_bytes());
    std::fs::write(&dummy_path, &dummy_buf).expect("write dummy gguf");

    let index = nexus::store::ModelIndex::reconcile_default(models_dir.path()).expect("index");
    assert!(!index.models.is_empty(), "expected indexed model");
    let digest = index.models[0].digest.clone();
    assert!(!digest.is_empty(), "expected non-empty digest");

    let discovery = Arc::new(DiscoveryService::new(cfg.clone(), None));
    discovery.set_active_model("dummy-llm.gguf").await;
    discovery.set_status_flags(StatusFlags::READY).await;

    let ctx = GatewayContext::new(
        SupervisorManager::new(),
        cfg.network.api_port,
        models_dir.path().to_path_buf(),
        discovery.node_uuid(),
        Arc::new(std::sync::RwLock::new(cfg)),
    )
    .with_discovery(discovery);

    let resolved = resolve_model_upstream(&ctx, &digest).await;
    assert!(resolved.is_some(), "expected digest {digest} to resolve");
    assert_eq!(resolved.unwrap().model_id, "dummy-llm.gguf");
}
