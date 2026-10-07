//! Phase 14 integration tests: Embedded Knowledge Base, Vector Search, and RAG.

use nexus::config::NexusConfig;
use nexus::control_plane::{
    dispatch_kb_query, dispatch_kb_query_signed, dispatch_kb_store, dispatch_kb_store_signed,
    dispatch_pair, KbQueryRequest, KbStoreRequest, PairRequest, CONTROL_PLANE_VERSION,
};
use nexus::control_plane_server::{spawn_ephemeral, ControlPlaneContext};
use nexus::discovery::NodeRole;
use nexus::kb::embedder::{Embedder, FastPseudoEmbedder};
use nexus::kb::retriever::KnowledgeRetriever;
use nexus::kb::store::KnowledgeStore;
use nexus::kb::vector::cosine_similarity;
use nexus::kb::EpisodicKind;
use nexus::node_identity::NodeIdentity;
use nexus::router::{RouteDecision, RouteTarget, Router};
use nexus::supervisor::SupervisorManager;
use nexus::trust_auth::TrustBootstrap;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use tempfile::TempDir;
use uuid::Uuid;

#[test]
fn test_redb_chunk_storage_and_content_addressing() {
    let temp_dir = TempDir::new().unwrap();
    let db_path = temp_dir.path().join("knowledge.redb");
    let store = KnowledgeStore::open(&db_path).expect("open store");

    let content = "Nexus uses Ed25519 signatures and TOFU pairing.";
    let mut meta = HashMap::new();
    meta.insert("source".to_string(), "security.md".to_string());

    let chunk = store
        .store_chunk("security.md", Some("Security Overview"), content, meta, None)
        .expect("store chunk");

    // Content-addressing verification
    let expected_id = KnowledgeStore::hash_content(content);
    assert_eq!(chunk.chunk_id, expected_id);
    assert_eq!(chunk.chunk_id.len(), 64);

    // Retrieval
    let retrieved = store
        .get_chunk(&chunk.chunk_id)
        .expect("get chunk")
        .expect("chunk exists");
    assert_eq!(retrieved.content, content);
    assert_eq!(retrieved.title.as_deref(), Some("Security Overview"));
    assert_eq!(retrieved.metadata.get("source").map(String::as_str), Some("security.md"));

    // Listing
    let all = store.list_chunks().expect("list chunks");
    assert_eq!(all.len(), 1);

    // Deletion
    let deleted = store.delete_chunk(&chunk.chunk_id).expect("delete chunk");
    assert!(deleted);
    let after_delete = store.get_chunk(&chunk.chunk_id).expect("get after delete");
    assert!(after_delete.is_none());
}

#[test]
fn test_persona_versioning() {
    let temp_dir = TempDir::new().unwrap();
    let db_path = temp_dir.path().join("knowledge.redb");
    let store = KnowledgeStore::open(&db_path).expect("open store");

    let p1 = store
        .store_persona("coder", "Code Specialist", "You are an expert Rust programmer.", vec!["coder".into()], None)
        .expect("store persona v1");
    assert_eq!(p1.id, "coder");
    assert_eq!(p1.version, 1);

    // Update persona increments version
    let p2 = store
        .store_persona("coder", "Senior Code Specialist", "You write idiomatic, safe Rust.", vec!["coder".into()], None)
        .expect("store persona v2");
    assert_eq!(p2.version, 2);
    assert_eq!(p2.system_prompt, "You write idiomatic, safe Rust.");

    let fetched = store.get_persona("coder").expect("get persona").unwrap();
    assert_eq!(fetched.version, 2);
    assert_eq!(fetched.name, "Senior Code Specialist");

    let all = store.list_personas().expect("list personas");
    assert_eq!(all.len(), 1);
}

#[test]
fn test_episodic_memory_kinds() {
    let temp_dir = TempDir::new().unwrap();
    let db_path = temp_dir.path().join("knowledge.redb");
    let store = KnowledgeStore::open(&db_path).expect("open store");

    let mem1 = store
        .store_memory(
            Some("session-1"),
            EpisodicKind::Preference,
            "Response Tone",
            "User prefers concise responses without conversational filler.",
            None,
            vec!["ui".into()],
            None,
        )
        .expect("store preference");

    let _mem2 = store
        .store_memory(
            Some("session-1"),
            EpisodicKind::Fact,
            "Cluster Topo",
            "Node penryn-mac has 3.6 GB usable RAM.",
            None,
            vec!["cluster".into()],
            None,
        )
        .expect("store fact");

    // Filter by kind
    let prefs = store.list_memories(Some(EpisodicKind::Preference)).expect("list prefs");
    assert_eq!(prefs.len(), 1);
    assert_eq!(prefs[0].id, mem1.id);
    assert_eq!(prefs[0].title, "Response Tone");

    let facts = store.list_memories(Some(EpisodicKind::Fact)).expect("list facts");
    assert_eq!(facts.len(), 1);
    assert_eq!(facts[0].title, "Cluster Topo");

    let all = store.list_memories(None).expect("list all");
    assert_eq!(all.len(), 2);

    let stats = store.stats().expect("stats");
    assert_eq!(stats.total_memories, 2);
    assert_eq!(stats.total_personas, 0);
    assert_eq!(stats.total_chunks, 0);
}

#[test]
fn test_cosine_similarity_and_pseudo_embedder() {
    // Basic vector math
    let v1 = vec![1.0, 0.0, 0.0];
    let v2 = vec![1.0, 0.0, 0.0];
    let v3 = vec![0.0, 1.0, 0.0];
    let v4 = vec![-1.0, 0.0, 0.0];

    assert!((cosine_similarity(&v1, &v2) - 1.0).abs() < 1e-5);
    assert!((cosine_similarity(&v1, &v3) - 0.0).abs() < 1e-5);
    assert!((cosine_similarity(&v1, &v4) - (-1.0)).abs() < 1e-5);

    // Fast pseudo-embedder semantic proximity
    let embedder = FastPseudoEmbedder::default();
    let emb_rust1 = embedder.embed("Rust systems programming with ownership and borrowing");
    let emb_rust2 = embedder.embed("Rust language ownership and borrow checker safety");
    let emb_cooking = embedder.embed("Baking chocolate chip cookies with organic butter");

    let sim_related = cosine_similarity(&emb_rust1, &emb_rust2);
    let sim_unrelated = cosine_similarity(&emb_rust1, &emb_cooking);

    assert!(
        sim_related > 0.4,
        "Related texts must have positive similarity, got: {sim_related}"
    );
    assert!(
        sim_related > sim_unrelated,
        "Related texts ({sim_related}) must score higher than unrelated ({sim_unrelated})"
    );
}

#[tokio::test]
async fn test_vector_retrieval_and_hybrid_search() {
    let temp_dir = TempDir::new().unwrap();
    let db_path = temp_dir.path().join("knowledge.redb");
    let store = KnowledgeStore::open(&db_path).expect("open store");
    let pseudo = FastPseudoEmbedder::default();

    let doc_rust = "Rust ownership and memory safety guarantee zero data races.";
    let doc_bread = "Sourdough bread fermentation requires flour, water, and active yeast.";
    let doc_net = "Distributed consensus protocols like Raft ensure fault tolerance.";

    let emb_rust = pseudo.embed(doc_rust);
    let emb_bread = pseudo.embed(doc_bread);
    let emb_net = pseudo.embed(doc_net);

    store
        .store_chunk("rust.md", Some("Rust Safety"), doc_rust, HashMap::new(), Some(emb_rust))
        .unwrap();
    store
        .store_chunk("bread.md", Some("Bread Guide"), doc_bread, HashMap::new(), Some(emb_bread))
        .unwrap();
    store
        .store_chunk("net.md", Some("Raft Consensus"), doc_net, HashMap::new(), Some(emb_net))
        .unwrap();

    let retriever = KnowledgeRetriever::new(store, Embedder::Pseudo(pseudo));

    // Query for Rust memory safety
    let results = retriever
        .retrieve_chunks("How does Rust guarantee memory safety without garbage collection?", 2, 0.2)
        .await
        .expect("retrieve chunks");

    assert!(!results.is_empty());
    assert_eq!(results[0].item.document_id, "rust.md");
    assert!(results[0].score > 0.3);
}

#[tokio::test]
async fn test_rag_prompt_augmentation_and_router() {
    let temp_dir = TempDir::new().unwrap();
    let db_path = temp_dir.path().join("knowledge.redb");
    let store = KnowledgeStore::open(&db_path).expect("open store");
    let pseudo = FastPseudoEmbedder::default();

    let content = "The Nexus orchestrator model runs as a constrained classifier on port 8080.";
    let emb = pseudo.embed(content);
    store
        .store_chunk("arch.md", Some("Nexus Architecture"), content, HashMap::new(), Some(emb))
        .unwrap();

    let retriever = KnowledgeRetriever::new(store, Embedder::Pseudo(pseudo));
    let router = Router::new();

    let routes = vec![
        RouteTarget::new(Uuid::new_v4(), "http://node1:8080", "coder", vec!["coder".into()]),
        RouteTarget::new(Uuid::new_v4(), "http://node2:8080", "general", vec!["general".into()]),
    ];

    // Explicit @rag directive
    let prompt = "@rag how does the Nexus orchestrator model classify requests?";
    let (decision, augmented) = router
        .route_with_rag(prompt, &routes, None, Some(&retriever), 3)
        .await;

    // Augmented prompt should contain the knowledge block
    assert!(augmented.contains("Knowledge Base Context"));
    assert!(augmented.contains("Nexus Architecture"));
    assert!(augmented.contains("Nexus orchestrator model runs as a constrained classifier"));

    // Route decision targets general or coder
    match decision {
        RouteDecision::Direct { matched_tag, .. } => {
            // If keywords match
            assert!(!matched_tag.is_empty());
        }
        RouteDecision::Fallback { endpoint, .. } => {
            assert!(endpoint.contains("8080"));
        }
        _ => {}
    }
}

#[tokio::test]
async fn test_kb_control_plane_http_signed() {
    let dir = TempDir::new().unwrap();
    let config_path = dir.path().join("server_config.toml");
    let server_cfg = NexusConfig::default();
    server_cfg.save_to_path(&config_path).unwrap();
    std::env::set_var("NEXUS_CONFIG", config_path.to_str().unwrap());
    let server_trust = TrustBootstrap::load(NexusConfig::default()).unwrap();
    let server_id = Uuid::new_v4();

    let kb_path = dir.path().join("server_knowledge.redb");
    let server_kb_store = Arc::new(KnowledgeStore::open(&kb_path).unwrap());

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
        .with_kb_store(server_kb_store.clone()),
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

    // 1. Signed KB Store request
    let store_req = KbStoreRequest {
        protocol_version: CONTROL_PLANE_VERSION,
        requester_id,
        document_id: "cluster_setup.md".to_string(),
        title: Some("Cluster Setup".to_string()),
        content: "Nexus nodes discover each other using UDP multicast on 239.255.0.1".to_string(),
        metadata: HashMap::new(),
    };

    let store_resp = dispatch_kb_store_signed(&client, &base, &store_req, &client_trust.identity)
        .await
        .expect("signed kb store");
    assert!(store_resp.success);
    assert_eq!(store_resp.chunk_id.len(), 64);

    // Verify stored directly in server's redb
    let in_store = server_kb_store.get_chunk(&store_resp.chunk_id).unwrap();
    assert!(in_store.is_some());

    // 2. Signed KB Query request
    let query_req = KbQueryRequest {
        protocol_version: CONTROL_PLANE_VERSION,
        requester_id,
        query: "UDP multicast cluster discovery".to_string(),
        limit: 5,
    };

    let query_resp = dispatch_kb_query_signed(&client, &base, &query_req, &client_trust.identity)
        .await
        .expect("signed kb query");
    assert!(query_resp.success);
    assert!(!query_resp.results.is_empty());
    assert_eq!(query_resp.results[0].chunk_id, store_resp.chunk_id);
    assert_eq!(query_resp.results[0].document_id, "cluster_setup.md");

    // 3. Untrusted identity query is rejected
    let untrusted_identity = NodeIdentity::generate();
    let untrusted_query_req = KbQueryRequest {
        protocol_version: CONTROL_PLANE_VERSION,
        requester_id: Uuid::new_v4(),
        query: "multicast".to_string(),
        limit: 5,
    };

    let unauthorized_err = dispatch_kb_query_signed(
        &client,
        &base,
        &untrusted_query_req,
        &untrusted_identity,
    )
    .await
    .unwrap_err();

    let err_str = unauthorized_err.to_string();
    assert!(
        err_str.contains("403") || err_str.contains("SignerNotAuthorized"),
        "expected auth error, got: {err_str}"
    );

    // Also verify unauthenticated dispatch_kb_store works in open loopback mode
    let unauth_store_req = KbStoreRequest {
        protocol_version: CONTROL_PLANE_VERSION,
        requester_id,
        document_id: "local_notes.md".to_string(),
        title: None,
        content: "A quick local note".to_string(),
        metadata: HashMap::new(),
    };
    let unauth_store_resp = dispatch_kb_store(&client, &base, &unauth_store_req).await;
    // When pairing is enforced, unauthenticated requests without headers are rejected
    assert!(unauth_store_resp.is_err());

    let unauth_query_req = KbQueryRequest {
        protocol_version: CONTROL_PLANE_VERSION,
        requester_id,
        query: "multicast".to_string(),
        limit: 5,
    };
    let unauth_query_resp = dispatch_kb_query(&client, &base, &unauth_query_req).await;
    assert!(unauth_query_resp.is_err());

    handle.abort();
    std::env::remove_var("NEXUS_CONFIG");
}
