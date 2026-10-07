//! Phase 15 integration tests: Background Learning, State Sync, and Agents View.

use nexus::client::ChatMessage;
use nexus::config::NexusConfig;
use nexus::control_plane::{
    dispatch_kb_manifest_signed, dispatch_kb_pull_signed, dispatch_kb_push_signed,
    KbManifestRequest, CONTROL_PLANE_VERSION,
};
use nexus::control_plane_server::{spawn_ephemeral, ControlPlaneContext};
use nexus::discovery::NodeRole;
use nexus::kb::embedder::{Embedder, FastPseudoEmbedder};
use nexus::kb::janitor::JanitorAgent;
use nexus::kb::retriever::KnowledgeRetriever;
use nexus::kb::store::KnowledgeStore;
use nexus::kb::sync::{
    apply_pull, apply_push, compute_diff, generate_manifest, KbSyncPullRequest, KbSyncPushRequest,
};
use nexus::kb::{EpisodicKind, EpisodicMemory, Persona};
use nexus::supervisor::SupervisorManager;
use nexus::task::{TaskRecord, TaskStatus, TaskStore};
use nexus::trust_auth::TrustBootstrap;
use nexus::ui::agents_view::AgentsView;
use nexus::ui::tunnel_view::TunnelView;
use ratatui::backend::TestBackend;
use ratatui::Terminal;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use tempfile::TempDir;
use uuid::Uuid;

#[test]
fn test_kb_manifest_generation_and_diff() {
    let temp_a = TempDir::new().unwrap();
    let temp_b = TempDir::new().unwrap();
    let store_a = KnowledgeStore::open(temp_a.path().join("knowledge_a.redb")).unwrap();
    let store_b = KnowledgeStore::open(temp_b.path().join("knowledge_b.redb")).unwrap();

    let node_a = Uuid::new_v4();
    let node_b = Uuid::new_v4();

    // 1. Store items on Node A
    let chunk1 = store_a
        .store_chunk(
            "doc1.md",
            Some("Doc 1"),
            "Nexus uses gossip sync for knowledge distribution.",
            HashMap::new(),
            None,
        )
        .unwrap();

    let chunk2 = store_a
        .store_chunk(
            "doc2.md",
            Some("Doc 2"),
            "Personas define persistent system instructions.",
            HashMap::new(),
            None,
        )
        .unwrap();

    let _persona_a = store_a
        .store_persona(
            "coder",
            "Code Specialist",
            "You write Rust.",
            vec!["rust".into()],
            None,
        )
        .unwrap();

    let mem_a = store_a
        .store_memory(
            Some("session-1"),
            EpisodicKind::Preference,
            "User Preference",
            "Prefers safe Rust code without unwrap.",
            None,
            vec!["preference".into()],
            None,
        )
        .unwrap();

    // Store a different chunk on Node B
    let _chunk_b = store_b
        .store_chunk(
            "b_only.md",
            Some("B doc"),
            "Unique document on node B.",
            HashMap::new(),
            None,
        )
        .unwrap();

    // 2. Generate manifests
    let manifest_a = generate_manifest(&store_a, node_a).unwrap();
    let manifest_b = generate_manifest(&store_b, node_b).unwrap();

    assert_eq!(manifest_a.chunks.len(), 2);
    assert_eq!(manifest_a.personas.len(), 1);
    assert_eq!(manifest_a.memories.len(), 1);

    assert_eq!(manifest_b.chunks.len(), 1);
    assert_eq!(manifest_b.personas.len(), 0);
    assert_eq!(manifest_b.memories.len(), 0);

    // 3. Compute diff: what Node B needs from Node A
    let diff_b_from_a = compute_diff(&manifest_b, &manifest_a);
    assert_eq!(diff_b_from_a.missing_chunk_ids.len(), 2);
    assert!(diff_b_from_a.missing_chunk_ids.contains(&chunk1.chunk_id));
    assert!(diff_b_from_a.missing_chunk_ids.contains(&chunk2.chunk_id));
    assert_eq!(diff_b_from_a.missing_or_stale_persona_ids, vec!["coder"]);
    assert_eq!(diff_b_from_a.missing_or_stale_memory_ids, vec![mem_a.id]);

    // 4. Node B pulls from Node A
    let pull_req = KbSyncPullRequest {
        protocol_version: CONTROL_PLANE_VERSION,
        requester_id: node_b,
        chunk_ids: diff_b_from_a.missing_chunk_ids.clone(),
        persona_ids: diff_b_from_a.missing_or_stale_persona_ids.clone(),
        memory_ids: diff_b_from_a.missing_or_stale_memory_ids.clone(),
    };
    let pull_resp = apply_pull(&store_a, &pull_req).unwrap();
    assert_eq!(pull_resp.chunks.len(), 2);
    assert_eq!(pull_resp.personas.len(), 1);
    assert_eq!(pull_resp.memories.len(), 1);

    // 5. Node B ingests items
    let push_req = KbSyncPushRequest {
        protocol_version: CONTROL_PLANE_VERSION,
        requester_id: node_b,
        chunks: pull_resp.chunks,
        personas: pull_resp.personas,
        memories: pull_resp.memories,
    };
    let push_resp = apply_push(&store_b, &push_req).unwrap();
    assert_eq!(push_resp.accepted_chunks, 2);
    assert_eq!(push_resp.accepted_personas, 1);
    assert_eq!(push_resp.accepted_memories, 1);

    // Verify Node B now has all 3 chunks
    let all_b_chunks = store_b.list_chunks().unwrap();
    assert_eq!(all_b_chunks.len(), 3);
    assert!(store_b.get_persona("coder").unwrap().is_some());
    assert!(store_b.get_memory(mem_a.id).unwrap().is_some());

    // 6. Push again -> Content-addressed chunks and identical items are idempotently skipped
    let duplicate_push = apply_push(&store_b, &push_req).unwrap();
    assert_eq!(duplicate_push.accepted_chunks, 0);
    assert_eq!(duplicate_push.accepted_personas, 0);
    assert_eq!(duplicate_push.accepted_memories, 0);
}

#[test]
fn test_kb_sync_conflict_resolution() {
    let temp_a = TempDir::new().unwrap();
    let temp_b = TempDir::new().unwrap();
    let store_a = KnowledgeStore::open(temp_a.path().join("res_a.redb")).unwrap();
    let store_b = KnowledgeStore::open(temp_b.path().join("res_b.redb")).unwrap();

    // Persona conflict resolution: v1 vs v2
    let p_v1 = store_a
        .store_persona(
            "assistant",
            "Assistant v1",
            "System prompt v1",
            vec![],
            None,
        )
        .unwrap();
    assert_eq!(p_v1.version, 1);

    // On store B, store twice so version is 2
    let _ = store_b
        .store_persona(
            "assistant",
            "Assistant v1",
            "System prompt v1",
            vec![],
            None,
        )
        .unwrap();
    let p_v2 = store_b
        .store_persona(
            "assistant",
            "Assistant v2",
            "System prompt v2",
            vec![],
            None,
        )
        .unwrap();
    assert_eq!(p_v2.version, 2);

    // Episodic memory conflict resolution: newer timestamp wins
    let shared_mem_id = Uuid::new_v4();
    let mem_older = EpisodicMemory {
        id: shared_mem_id,
        session_id: None,
        kind: EpisodicKind::Fact,
        title: "Cluster Fact".to_string(),
        summary: "Cluster has 2 nodes".to_string(),
        details: None,
        tags: vec![],
        embedding: None,
        timestamp: 1000,
    };
    store_a.upsert_raw_memory(&mem_older).unwrap();

    let mem_newer = EpisodicMemory {
        id: shared_mem_id,
        session_id: None,
        kind: EpisodicKind::Fact,
        title: "Cluster Fact".to_string(),
        summary: "Cluster has 5 nodes (updated)".to_string(),
        details: None,
        tags: vec![],
        embedding: None,
        timestamp: 2000,
    };
    store_b.upsert_raw_memory(&mem_newer).unwrap();

    // Push from B to A (newer v2 persona, newer timestamp memory)
    let push_b_to_a = KbSyncPushRequest {
        protocol_version: CONTROL_PLANE_VERSION,
        requester_id: Uuid::new_v4(),
        chunks: vec![],
        personas: vec![p_v2.clone()],
        memories: vec![mem_newer.clone()],
    };
    let res = apply_push(&store_a, &push_b_to_a).unwrap();
    assert_eq!(res.accepted_personas, 1);
    assert_eq!(res.accepted_memories, 1);

    // Verify store_a now holds v2 persona and updated memory
    let updated_p = store_a.get_persona("assistant").unwrap().unwrap();
    assert_eq!(updated_p.version, 2);
    assert_eq!(updated_p.name, "Assistant v2");

    let updated_m = store_a.get_memory(shared_mem_id).unwrap().unwrap();
    assert_eq!(updated_m.timestamp, 2000);
    assert_eq!(updated_m.summary, "Cluster has 5 nodes (updated)");

    // Push from A to B (older v1 persona, older timestamp memory) -> Rejected!
    let push_a_to_b = KbSyncPushRequest {
        protocol_version: CONTROL_PLANE_VERSION,
        requester_id: Uuid::new_v4(),
        chunks: vec![],
        personas: vec![p_v1],
        memories: vec![mem_older],
    };
    let rejected = apply_push(&store_b, &push_a_to_b).unwrap();
    assert_eq!(rejected.accepted_personas, 0);
    assert_eq!(rejected.accepted_memories, 0);

    // Store B still has v2 and timestamp 2000
    let b_p = store_b.get_persona("assistant").unwrap().unwrap();
    assert_eq!(b_p.version, 2);
    let b_m = store_b.get_memory(shared_mem_id).unwrap().unwrap();
    assert_eq!(b_m.timestamp, 2000);
}

#[tokio::test]
async fn test_control_plane_kb_sync_endpoints() {
    let supervisor = SupervisorManager::new();
    let node_id = Uuid::new_v4();
    let trust_dir = TempDir::new().unwrap();
    let config_path = trust_dir.path().join("config.toml");
    let config = NexusConfig::default();
    config.save_to_path(&config_path).unwrap();
    std::env::set_var("NEXUS_CONFIG", config_path.to_str().unwrap());

    let trust = TrustBootstrap::load(config).unwrap();
    let server_identity = trust.identity.clone();

    // Client identity and pairing
    let client_trust = TrustBootstrap::load(NexusConfig::default()).unwrap();
    let client_identity = client_trust.identity.clone();
    let client_id = client_trust.config.read().unwrap().node_uuid().unwrap();

    trust
        .config
        .write()
        .unwrap()
        .network
        .security
        .record_pair(client_id, client_identity.public_key_hex());

    let server_kb_path = trust_dir.path().join("server_knowledge.redb");
    let server_kb_store = Arc::new(KnowledgeStore::open(&server_kb_path).unwrap());

    let ctx = Arc::new(
        ControlPlaneContext::new(
            node_id,
            NodeRole::HOST,
            supervisor,
            "127.0.0.1",
            18080,
            PathBuf::from("llama-server"),
            server_identity,
            trust.config,
            trust.config_path,
        )
        .with_capabilities(vec!["inference".to_string(), "kb".to_string()])
        .with_kb_store(server_kb_store.clone()),
    );

    // Seed server's KB
    let chunk = server_kb_store
        .store_chunk("net.md", Some("Net"), "Network chunk", HashMap::new(), None)
        .unwrap();

    let mem = server_kb_store
        .store_memory(
            None,
            EpisodicKind::Fact,
            "Control Plane Fact",
            "Signed endpoints are active",
            None,
            vec!["network".into()],
            None,
        )
        .unwrap();

    let (addr, handle) = spawn_ephemeral(ctx.clone()).await.unwrap();
    let base_url = format!("http://{}", addr);
    let client = reqwest::Client::new();

    // 1. Fetch manifest
    let manifest_req = KbManifestRequest {
        protocol_version: CONTROL_PLANE_VERSION,
        requester_id: client_id,
    };
    let manifest_resp =
        dispatch_kb_manifest_signed(&client, &base_url, &manifest_req, &client_identity)
            .await
            .unwrap();

    assert_eq!(manifest_resp.manifest.chunks.len(), 1);
    assert_eq!(manifest_resp.manifest.chunks[0].chunk_id, chunk.chunk_id);
    assert_eq!(manifest_resp.manifest.memories.len(), 1);
    assert_eq!(manifest_resp.manifest.memories[0].id, mem.id);

    // 2. Pull chunk and memory
    let pull_req = KbSyncPullRequest {
        protocol_version: CONTROL_PLANE_VERSION,
        requester_id: client_id,
        chunk_ids: vec![chunk.chunk_id.clone()],
        persona_ids: vec![],
        memory_ids: vec![mem.id],
    };
    let pull_resp = dispatch_kb_pull_signed(&client, &base_url, &pull_req, &client_identity)
        .await
        .unwrap();
    assert_eq!(pull_resp.chunks.len(), 1);
    assert_eq!(pull_resp.chunks[0].content, "Network chunk");
    assert_eq!(pull_resp.memories.len(), 1);
    assert_eq!(pull_resp.memories[0].title, "Control Plane Fact");

    // 3. Push new persona to server
    let new_persona = Persona {
        id: "reviewer".to_string(),
        version: 1,
        name: "Code Reviewer".to_string(),
        system_prompt: "Thorough review prompt".to_string(),
        tags: vec!["review".into()],
        parameters: None,
        updated_at: 100,
    };
    let push_req = KbSyncPushRequest {
        protocol_version: CONTROL_PLANE_VERSION,
        requester_id: client_id,
        chunks: vec![],
        personas: vec![new_persona.clone()],
        memories: vec![],
    };
    let push_resp = dispatch_kb_push_signed(&client, &base_url, &push_req, &client_identity)
        .await
        .unwrap();
    assert!(push_resp.success);
    assert_eq!(push_resp.accepted_personas, 1);

    // Verify persona exists in server KB store
    let fetched = server_kb_store.get_persona("reviewer").unwrap().unwrap();
    assert_eq!(fetched.name, "Code Reviewer");

    handle.abort();
}

#[tokio::test]
async fn test_janitor_agent_distillation() {
    let temp_dir = TempDir::new().unwrap();
    let store = KnowledgeStore::open(temp_dir.path().join("janitor.redb")).unwrap();
    let embedder = Embedder::Pseudo(FastPseudoEmbedder::default());
    let janitor = JanitorAgent::new(store.clone(), embedder.clone());

    // 1. Distill dialogue turns
    let dialogue = vec![
        ChatMessage {
            role: "user".to_string(),
            content: "Hello! My name is Alice and I prefer Rust for systems programming.".to_string(),
        },
        ChatMessage {
            role: "assistant".to_string(),
            content: "Welcome Alice! Rust provides memory safety and zero-cost abstractions.".to_string(),
        },
        ChatMessage {
            role: "user".to_string(),
            content: "Good to know. Cluster endpoint hardware profile: node alpha has 16GB RAM and runs on port 8080.".to_string(),
        },
    ];

    let distilled = janitor
        .distill_dialogue(&dialogue, Some("session_alice"))
        .unwrap();
    assert!(!distilled.is_empty());

    // Verify preference extracted
    let pref_mem = distilled
        .iter()
        .find(|m| m.kind == EpisodicKind::Preference)
        .expect("preference memory");
    assert!(pref_mem.title.contains("Preference"));
    assert!(pref_mem.summary.to_lowercase().contains("i prefer"));
    assert!(pref_mem.embedding.is_some());
    assert_eq!(pref_mem.embedding.as_ref().unwrap().len(), 128);

    // Verify fact extracted
    let fact_mem = distilled
        .iter()
        .find(|m| m.kind == EpisodicKind::Fact)
        .expect("fact memory");
    assert!(fact_mem.title.contains("Fact"));
    assert!(fact_mem.embedding.is_some());

    // Verify summary created
    let summ_mem = distilled
        .iter()
        .find(|m| m.kind == EpisodicKind::Summary)
        .expect("summary memory");
    assert!(summ_mem.title.contains("Session Summary"));
    assert!(summ_mem.embedding.is_some());

    // 2. Distill completed agent bus task
    let task_id = Uuid::new_v4();
    let task = TaskRecord {
        task_id,
        from_node: Uuid::new_v4(),
        to_route: "coder".to_string(),
        prompt: "Refactor async network handler".to_string(),
        status: TaskStatus::Completed,
        output: Some(
            "Successfully refactored network handler using tokio select loop.".to_string(),
        ),
        error: None,
        created_at: 100,
        updated_at: 120,
    };

    let task_mem = janitor
        .distill_task(&task)
        .unwrap()
        .expect("task distilled");
    assert_eq!(task_mem.kind, EpisodicKind::Summary);
    assert!(task_mem.title.contains("Route coder"));
    assert!(task_mem.summary.contains("Prompt:"));
    assert!(task_mem.embedding.is_some());

    // 3. Search via KnowledgeRetriever to verify RAG can retrieve the distilled memories
    let retriever = KnowledgeRetriever::new(store, embedder);
    let search_results = retriever
        .retrieve_memories("prefer rust", 3, 0.0)
        .await
        .unwrap();

    assert!(!search_results.is_empty());
    assert!(search_results[0]
        .item
        .summary
        .to_lowercase()
        .contains("rust"));
}

#[test]
fn test_agents_view_and_tunnel_view_render() {
    let temp_dir = TempDir::new().unwrap();
    let task_store =
        Arc::new(TaskStore::load_or_create(temp_dir.path().join("tasks.json")).unwrap());
    let kb_store = Arc::new(KnowledgeStore::open(temp_dir.path().join("knowledge.redb")).unwrap());

    // Seed tasks
    let t1 = task_store
        .create_task(Uuid::new_v4(), Uuid::new_v4(), "coder", "Write unit test")
        .unwrap();
    task_store
        .update_status(t1.task_id, TaskStatus::Completed)
        .unwrap();
    task_store
        .complete_task(t1.task_id, "All tests passed")
        .unwrap();

    let _t2 = task_store
        .create_task(
            Uuid::new_v4(),
            Uuid::new_v4(),
            "general",
            "Explain gossip sync",
        )
        .unwrap();

    // Seed memories
    let _m1 = kb_store
        .store_memory(
            None,
            EpisodicKind::Preference,
            "Code Style",
            "Prefer standard Rust format",
            None,
            vec!["style".into()],
            None,
        )
        .unwrap();

    let mut agents_view = AgentsView::new(task_store, kb_store);
    assert_eq!(agents_view.tasks.len(), 2);
    assert_eq!(agents_view.memories.len(), 1);

    // Test cursor navigation
    agents_view.next_task();
    assert_eq!(agents_view.selected_task_idx, 1);
    agents_view.prev_task();
    assert_eq!(agents_view.selected_task_idx, 0);

    // Render AgentsView into TestBackend
    let backend = TestBackend::new(120, 40);
    let mut terminal = Terminal::new(backend).unwrap();

    terminal
        .draw(|f| {
            let area = f.area();
            agents_view.render(f, area);
        })
        .unwrap();

    // Render TunnelView into TestBackend
    let tunnel_view = TunnelView::new(8080, 50052);
    terminal
        .draw(|f| {
            let area = f.area();
            tunnel_view.render(f, area);
        })
        .unwrap();
}
