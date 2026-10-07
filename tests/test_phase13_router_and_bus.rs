//! Phase 13 integration tests: Intelligent Router and Model-to-Model Agent Bus.

use nexus::config::NexusConfig;
use nexus::control_plane::{
    dispatch_agent_message, dispatch_agent_message_signed, dispatch_pair, AgentTaskMessage,
    PairRequest, CONTROL_PLANE_VERSION,
};
use nexus::control_plane_server::{spawn_ephemeral, ControlPlaneContext};
use nexus::discovery::NodeRole;
use nexus::node_identity::NodeIdentity;
use nexus::router::{OrchestratorChoice, RouteDecision, RouteTarget, Router};
use nexus::supervisor::SupervisorManager;
use nexus::task::{TaskStatus, TaskStore};
use nexus::trust_auth::TrustBootstrap;
use std::path::PathBuf;
use std::sync::Arc;
use tempfile::TempDir;
use uuid::Uuid;

#[test]
fn router_tier1_explicit_tags() {
    let router = Router::new();
    let node1 = Uuid::new_v4();
    let node2 = Uuid::new_v4();
    let node3 = Uuid::new_v4();

    let routes = vec![
        RouteTarget::new(node1, "http://node1:8080", "qwen-coder", vec!["coder".into()]),
        RouteTarget::new(node2, "http://node2:8080", "llama-3.2-3b", vec!["general".into()]),
        RouteTarget::new(node3, "http://node3:8080", "llava-1.5", vec!["vision".into()]),
    ];

    // Explicit @coder tag
    let decision = router
        .route_deterministic("@coder write a binary search in Rust", &routes)
        .expect("should match @coder");

    match decision {
        RouteDecision::Direct {
            endpoint,
            model,
            matched_tag,
            clean_prompt,
            confidence,
        } => {
            assert_eq!(endpoint, "http://node1:8080");
            assert_eq!(model, "qwen-coder");
            assert_eq!(matched_tag, "coder");
            assert_eq!(clean_prompt, "write a binary search in Rust");
            assert_eq!(confidence, 100);
        }
        _ => panic!("Expected Direct decision"),
    }

    // Explicit @vision tag
    let decision_vis = router
        .route_deterministic("@vision describe this photo", &routes)
        .expect("should match @vision");
    assert_eq!(decision_vis.endpoint(), "http://node3:8080");
    assert_eq!(decision_vis.model(), "llava-1.5");

    // Non-existent explicit tag
    let decision_none = router.route_deterministic("@audio transcribe speech", &routes);
    assert!(decision_none.is_none());
}

#[test]
fn router_tier1_keyword_heuristics() {
    let router = Router::new();
    let routes = vec![
        RouteTarget::new(
            Uuid::new_v4(),
            "http://node1:8080",
            "qwen-coder",
            vec!["coder".into()],
        ),
        RouteTarget::new(
            Uuid::new_v4(),
            "http://node2:8080",
            "llama-3.2-3b",
            vec!["general".into()],
        ),
        RouteTarget::new(
            Uuid::new_v4(),
            "http://node3:8080",
            "llava-1.5",
            vec!["vision".into()],
        ),
    ];

    // Coder keyword: "fn "
    let dec = router
        .route_deterministic("Can you implement fn quicksort(arr: &mut [i32])?", &routes)
        .expect("should match coder keyword");
    assert_eq!(dec.endpoint(), "http://node1:8080");

    // Vision keyword: "screenshot"
    let dec_vis = router
        .route_deterministic("Please check this screenshot for errors", &routes)
        .expect("should match vision keyword");
    assert_eq!(dec_vis.endpoint(), "http://node3:8080");

    // General prompt: no keyword match -> returns None so Tier 2 can run
    let dec_general = router.route_deterministic("What is the capital of France?", &routes);
    assert!(dec_general.is_none());
}

#[test]
fn router_fallback_to_general() {
    let router = Router::new();
    let routes = vec![
        RouteTarget::new(
            Uuid::new_v4(),
            "http://node1:8080",
            "qwen-coder",
            vec!["coder".into()],
        ),
        RouteTarget::new(
            Uuid::new_v4(),
            "http://node2:8080",
            "llama-3.2-3b",
            vec!["general".into()],
        ),
    ];

    // Without an orchestrator client, unclassified prompt falls back to "general"
    let dec = tokio::runtime::Runtime::new().unwrap().block_on(async {
        router.route("Explain gravity in simple terms", &routes, None).await
    });

    match dec {
        RouteDecision::Fallback { endpoint, model, .. } => {
            assert_eq!(endpoint, "http://node2:8080");
            assert_eq!(model, "llama-3.2-3b");
        }
        other => panic!("Expected fallback decision, got: {:?}", other),
    }
}

#[test]
fn router_code_level_veto_on_hallucinated_route() {
    // Tests sanitization and schema parsing
    let raw_markdown = "```json\n{\"route\": \"coder\", \"reason\": \"prompt requests Rust code\", \"rewritten_prompt\": null}\n```";
    let sanitized = Router::sanitize_json_response(raw_markdown);
    let choice: Result<OrchestratorChoice, _> = serde_json::from_str(&sanitized);
    assert!(choice.is_ok());
    let parsed = choice.unwrap();
    assert_eq!(parsed.route, "coder");

    // If model hallucinates a non-existent route
    let hallucinated_json = "{\"route\": \"hallucinated_agent\", \"reason\": \"fake\"}";
    let parsed_bad: OrchestratorChoice = serde_json::from_str(hallucinated_json).unwrap();
    let available_routes = [
        RouteTarget::new(Uuid::new_v4(), "http://node1:8080", "coder", vec!["coder".into()]),
        RouteTarget::new(Uuid::new_v4(), "http://node2:8080", "general", vec!["general".into()]),
    ];

    // Code-level veto check
    let valid_target = available_routes
        .iter()
        .find(|r| r.matches_tag(&parsed_bad.route));
    assert!(valid_target.is_none(), "Hallucinated route must be vetoed by code whitelist");
}

#[test]
fn task_store_lifecycle_and_persistence() {
    let temp_dir = TempDir::new().unwrap();
    let store_path = temp_dir.path().join("tasks.json");

    let task_id = Uuid::new_v4();
    let from_node = Uuid::new_v4();

    {
        let store = TaskStore::load_or_create(&store_path).expect("create store");
        assert_eq!(store.list_tasks().len(), 0);

        let record = store
            .create_task(task_id, from_node, "coder", "solve two-sum in python")
            .expect("create task");
        assert_eq!(record.status, TaskStatus::Pending);

        assert!(store.update_status(task_id, TaskStatus::Running).unwrap());
        let current = store.get_task(task_id).unwrap();
        assert_eq!(current.status, TaskStatus::Running);

        assert!(store
            .complete_task(task_id, "def two_sum(nums, target): return []")
            .unwrap());
        let completed = store.get_task(task_id).unwrap();
        assert_eq!(completed.status, TaskStatus::Completed);
        assert_eq!(
            completed.output.as_deref(),
            Some("def two_sum(nums, target): return []")
        );
    }

    // Reload from disk to verify atomic durability
    {
        let reloaded = TaskStore::load_or_create(&store_path).expect("reload store");
        let tasks = reloaded.list_tasks();
        assert_eq!(tasks.len(), 1);
        let task = &tasks[0];
        assert_eq!(task.task_id, task_id);
        assert_eq!(task.status, TaskStatus::Completed);
        assert_eq!(task.to_route, "coder");
        assert_eq!(
            task.output.as_deref(),
            Some("def two_sum(nums, target): return []")
        );

        // Test failing a new task
        let task2_id = Uuid::new_v4();
        reloaded
            .create_task(task2_id, from_node, "vision", "segment image")
            .unwrap();
        assert!(reloaded.fail_task(task2_id, "out of memory").unwrap());

        let failed_task = reloaded.get_task(task2_id).unwrap();
        assert_eq!(failed_task.status, TaskStatus::Failed);
        assert_eq!(failed_task.error.as_deref(), Some("out of memory"));
        assert_eq!(reloaded.count_by_status(TaskStatus::Failed), 1);
        assert_eq!(reloaded.count_by_status(TaskStatus::Completed), 1);
    }
}

#[tokio::test]
async fn agent_bus_http_round_trip() {
    let supervisor = SupervisorManager::new();
    let temp_dir = TempDir::new().unwrap();
    let config_path = temp_dir.path().join("config.toml");
    NexusConfig::default().save_to_path(&config_path).unwrap();
    std::env::set_var("NEXUS_CONFIG", config_path.to_str().unwrap());
    let trust = TrustBootstrap::load(NexusConfig::default()).unwrap();

    let task_store_path = temp_dir.path().join("tasks.json");
    let task_store = Arc::new(TaskStore::load_or_create(&task_store_path).unwrap());

    let ctx = Arc::new(
        ControlPlaneContext::new(
            Uuid::new_v4(),
            NodeRole::HOST,
            supervisor,
            "127.0.0.1",
            18080,
            PathBuf::from("llama-server"),
            trust.identity,
            trust.config,
            trust.config_path,
        )
        .with_task_store(task_store.clone()),
    );

    let (addr, handle) = spawn_ephemeral(ctx).await.expect("bind ephemeral");
    let base = format!("http://{}", addr);
    let client = reqwest::Client::new();

    let task_id = Uuid::new_v4();
    let from_node = Uuid::new_v4();
    let msg = AgentTaskMessage {
        protocol_version: CONTROL_PLANE_VERSION,
        task_id,
        from_node,
        to_route: "coder".to_string(),
        prompt: "implement merge sort in rust".to_string(),
        reply_to: None,
    };

    let response = dispatch_agent_message(&client, &base, &msg)
        .await
        .expect("dispatch_agent_message");

    assert_eq!(response.task_id, task_id);
    assert_eq!(response.protocol_version, CONTROL_PLANE_VERSION);
    assert!(response.success);

    // Verify task is durably recorded in receiver's TaskStore
    let stored = task_store.get_task(task_id).expect("stored task");
    assert_eq!(stored.from_node, from_node);
    assert_eq!(stored.to_route, "coder");
    assert_eq!(stored.prompt, "implement merge sort in rust");

    handle.abort();
    std::env::remove_var("NEXUS_CONFIG");
}

#[tokio::test]
async fn agent_bus_signed_round_trip() {
    let dir = TempDir::new().unwrap();
    let server_config_path = dir.path().join("server_config.toml");
    let server_cfg = NexusConfig::default();
    server_cfg.save_to_path(&server_config_path).unwrap();
    std::env::set_var("NEXUS_CONFIG", server_config_path.to_str().unwrap());
    let server_trust = TrustBootstrap::load(NexusConfig::default()).unwrap();
    let server_id = Uuid::new_v4();

    let task_store_path = dir.path().join("server_tasks.json");
    let server_task_store = Arc::new(TaskStore::load_or_create(&task_store_path).unwrap());

    let ctx = Arc::new(
        ControlPlaneContext::new(
            server_id,
            NodeRole::HOST,
            SupervisorManager::new(),
            "127.0.0.1",
            18080,
            PathBuf::from("llama-server"),
            server_trust.identity.clone(),
            server_trust.config.clone(),
            server_trust.config_path.clone(),
        )
        .with_task_store(server_task_store.clone()),
    );

    let (addr, handle) = spawn_ephemeral(ctx).await.unwrap();
    let base = format!("http://{addr}");
    let client = reqwest::Client::new();

    // Client setup and pairing
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
    let pair_resp = dispatch_pair(&client, &base, &pair_req, &client_trust.identity)
        .await
        .expect("pair");
    assert!(pair_resp.success);

    // Send signed agent task message
    let task_id = Uuid::new_v4();
    let msg = AgentTaskMessage {
        protocol_version: CONTROL_PLANE_VERSION,
        task_id,
        from_node: requester_id,
        to_route: "general".to_string(),
        prompt: "analyze cluster status".to_string(),
        reply_to: None,
    };

    let response = dispatch_agent_message_signed(&client, &base, &msg, &client_trust.identity)
        .await
        .expect("signed agent message dispatched");

    assert_eq!(response.task_id, task_id);
    assert!(response.success);

    let recorded = server_task_store.get_task(task_id).unwrap();
    assert_eq!(recorded.from_node, requester_id);
    assert_eq!(recorded.to_route, "general");

    // Test untrusted signer is rejected
    let untrusted_identity = NodeIdentity::generate();
    let untrusted_task_id = Uuid::new_v4();
    let untrusted_msg = AgentTaskMessage {
        protocol_version: CONTROL_PLANE_VERSION,
        task_id: untrusted_task_id,
        from_node: Uuid::new_v4(),
        to_route: "general".to_string(),
        prompt: "unauthorized task".to_string(),
        reply_to: None,
    };

    let unauthorized_err = dispatch_agent_message_signed(
        &client,
        &base,
        &untrusted_msg,
        &untrusted_identity,
    )
    .await
    .unwrap_err();

    let err_msg = unauthorized_err.to_string();
    assert!(
        err_msg.contains("403") || err_msg.contains("SignerNotAuthorized"),
        "expected auth error, got: {err_msg}"
    );

    handle.abort();
    std::env::remove_var("NEXUS_CONFIG");
}
