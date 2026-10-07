//! Integration tests for Phase 4: Models Tab Explorer & Sharded GGUF Support.

use nexus::config::NexusConfig;
use nexus::discovery::DiscoveryService;
use nexus::hf::{HfGgufFile, HfModelSummary};
use nexus::node_identity::NodeIdentity;
use nexus::supervisor::SupervisorManager;
use nexus::ui::hub::commands::{spawn_hub_worker, HubCommand, HubEvent, HubWorkerCtx};
use nexus::ui::models::{
    aggregate_model_entries, calculate_shards_total_bytes, detect_model_shards, parse_shard_info,
    parse_shard_prefix, ModelEntry,
};
use nexus::ui::models_view::{ModelsTabMode, ModelsView};
use std::io::Write;
use std::net::TcpListener;
use std::sync::Arc;
use tempfile::tempdir;
use tokio::sync::mpsc;

#[test]
fn test_parse_shard_info_and_prefix() {
    assert_eq!(
        parse_shard_info("DeepSeek-R1-Distill-Qwen-32B-Q4_K_M-00001-of-00003.gguf"),
        Some(("DeepSeek-R1-Distill-Qwen-32B-Q4_K_M".to_string(), 1, 3))
    );
    assert_eq!(
        parse_shard_prefix("DeepSeek-R1-Distill-Qwen-32B-Q4_K_M-00001-of-00003.gguf"),
        Some("DeepSeek-R1-Distill-Qwen-32B-Q4_K_M".to_string())
    );
    assert_eq!(
        parse_shard_info("qwen-00003-of-00005.gguf"),
        Some(("qwen".to_string(), 3, 5))
    );
    assert_eq!(parse_shard_info("regular-model.gguf"), None);
    assert_eq!(parse_shard_info("invalid-00005-of-00002.gguf"), None);
}

#[test]
fn test_sharded_model_aggregation_complete_and_incomplete() {
    let dir = tempdir().unwrap();

    // 3 complete shards
    let s1 = dir.path().join("llama-70b-00001-of-00003.gguf");
    let s2 = dir.path().join("llama-70b-00002-of-00003.gguf");
    let s3 = dir.path().join("llama-70b-00003-of-00003.gguf");
    std::fs::write(&s1, vec![0u8; 1000]).unwrap();
    std::fs::write(&s2, vec![0u8; 2000]).unwrap();
    std::fs::write(&s3, vec![0u8; 3000]).unwrap();

    // 1 incomplete shard (2 of 4)
    let inc1 = dir.path().join("mixtral-8x7b-00001-of-00004.gguf");
    let inc2 = dir.path().join("mixtral-8x7b-00002-of-00004.gguf");
    std::fs::write(&inc1, vec![0u8; 1500]).unwrap();
    std::fs::write(&inc2, vec![0u8; 1500]).unwrap();

    // 1 un-sharded model
    let single = dir.path().join("phi-3.gguf");
    std::fs::write(&single, vec![0u8; 500]).unwrap();

    let entries = vec![
        ModelEntry {
            path: s2.clone(),
            filename: "llama-70b-00002-of-00003.gguf".to_string(),
            size_mb: 200,
            architecture: "llama".to_string(),
            context_length: 4096,
            exact_kv_mb: 50,
            lmk_compatible: true,
            gguf_version: 3,
            block_count: 32,
            head_count: 32,
            embedding_length: 4096,
            digest: "d2".to_string(),
            shard_count: 1,
            total_shards: None,
        },
        ModelEntry {
            path: s1.clone(),
            filename: "llama-70b-00001-of-00003.gguf".to_string(),
            size_mb: 100,
            architecture: "llama".to_string(),
            context_length: 4096,
            exact_kv_mb: 50,
            lmk_compatible: true,
            gguf_version: 3,
            block_count: 32,
            head_count: 32,
            embedding_length: 4096,
            digest: "d1".to_string(),
            shard_count: 1,
            total_shards: None,
        },
        ModelEntry {
            path: s3.clone(),
            filename: "llama-70b-00003-of-00003.gguf".to_string(),
            size_mb: 300,
            architecture: "llama".to_string(),
            context_length: 4096,
            exact_kv_mb: 50,
            lmk_compatible: true,
            gguf_version: 3,
            block_count: 32,
            head_count: 32,
            embedding_length: 4096,
            digest: "d3".to_string(),
            shard_count: 1,
            total_shards: None,
        },
        ModelEntry {
            path: inc1.clone(),
            filename: "mixtral-8x7b-00001-of-00004.gguf".to_string(),
            size_mb: 150,
            architecture: "mixtral".to_string(),
            context_length: 4096,
            exact_kv_mb: 40,
            lmk_compatible: true,
            gguf_version: 3,
            block_count: 32,
            head_count: 32,
            embedding_length: 4096,
            digest: "inc1".to_string(),
            shard_count: 1,
            total_shards: None,
        },
        ModelEntry {
            path: inc2.clone(),
            filename: "mixtral-8x7b-00002-of-00004.gguf".to_string(),
            size_mb: 150,
            architecture: "mixtral".to_string(),
            context_length: 4096,
            exact_kv_mb: 40,
            lmk_compatible: true,
            gguf_version: 3,
            block_count: 32,
            head_count: 32,
            embedding_length: 4096,
            digest: "inc2".to_string(),
            shard_count: 1,
            total_shards: None,
        },
        ModelEntry {
            path: single.clone(),
            filename: "phi-3.gguf".to_string(),
            size_mb: 50,
            architecture: "phi3".to_string(),
            context_length: 2048,
            exact_kv_mb: 20,
            lmk_compatible: true,
            gguf_version: 3,
            block_count: 24,
            head_count: 16,
            embedding_length: 2048,
            digest: "phi".to_string(),
            shard_count: 1,
            total_shards: None,
        },
    ];

    let aggregated = aggregate_model_entries(entries);
    assert_eq!(aggregated.len(), 3);

    // 1. llama-70b: all 3 shards present
    let llama = aggregated
        .iter()
        .find(|m| m.filename.contains("llama-70b"))
        .unwrap();
    assert_eq!(llama.filename, "llama-70b [3 shards]");
    assert_eq!(llama.size_mb, 600);
    assert_eq!(llama.shard_count, 3);
    assert_eq!(llama.total_shards, Some(3));
    assert_eq!(llama.path, s1);

    // detect_model_shards on llama primary path
    let shards = detect_model_shards(&llama.path);
    assert_eq!(shards.len(), 3);
    assert_eq!(calculate_shards_total_bytes(&shards), 6000);

    // 2. mixtral-8x7b: partial 2 of 4 shards
    let mixtral = aggregated
        .iter()
        .find(|m| m.filename.contains("mixtral-8x7b"))
        .unwrap();
    assert_eq!(mixtral.filename, "mixtral-8x7b [2/4 shards]");
    assert_eq!(mixtral.size_mb, 300);
    assert_eq!(mixtral.shard_count, 2);
    assert_eq!(mixtral.total_shards, Some(4));
    assert_eq!(mixtral.path, inc1);

    // 3. phi-3: un-sharded
    let phi = aggregated
        .iter()
        .find(|m| m.filename.contains("phi-3"))
        .unwrap();
    assert_eq!(phi.filename, "phi-3.gguf");
    assert_eq!(phi.size_mb, 50);
    assert_eq!(phi.shard_count, 1);
    assert_eq!(phi.total_shards, None);
    assert_eq!(phi.path, single);
}

#[tokio::test]
async fn test_multi_shard_sequential_download_queue() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();

    let shard1_bytes = b"GGUF_TEST_SHARD_1".to_vec();
    let shard2_bytes = b"GGUF_TEST_SHARD_2".to_vec();

    let s1 = shard1_bytes.clone();
    let s2 = shard2_bytes.clone();

    std::thread::spawn(move || {
        for _ in 0..2 {
            if let Ok((mut stream, _)) = listener.accept() {
                let mut buf = [0u8; 1024];
                let _ = std::io::Read::read(&mut stream, &mut buf);
                let req = String::from_utf8_lossy(&buf);

                let body = if req.contains("00001") { &s1 } else { &s2 };

                let resp = format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nAccept-Ranges: bytes\r\n\r\n",
                    body.len()
                );
                let _ = stream.write_all(resp.as_bytes());
                let _ = stream.write_all(body);
                let _ = stream.flush();
            }
        }
    });

    let dir = tempdir().unwrap();
    let mut config = NexusConfig::default();
    config.node.models_dir = dir.path().to_path_buf();

    let discovery = Arc::new(DiscoveryService::new(config.clone(), None));
    let identity = Arc::new(NodeIdentity::generate());
    let supervisor = SupervisorManager::new();

    let ctx = HubWorkerCtx {
        config: config.clone(),
        discovery,
        supervisor,
        identity,
    };

    let (cmd_tx, cmd_rx) = mpsc::channel(16);
    let (evt_tx, mut evt_rx) = mpsc::channel(64);

    let _worker_handle = spawn_hub_worker(cmd_rx, evt_tx, ctx);

    let files = vec![
        HfGgufFile {
            filename: "model-00001-of-00002.gguf".to_string(),
            size_bytes: shard1_bytes.len() as u64,
            sha256: None,
            download_url: format!("http://127.0.0.1:{port}/model-00001-of-00002.gguf"),
        },
        HfGgufFile {
            filename: "model-00002-of-00002.gguf".to_string(),
            size_bytes: shard2_bytes.len() as u64,
            sha256: None,
            download_url: format!("http://127.0.0.1:{port}/model-00002-of-00002.gguf"),
        },
    ];

    cmd_tx
        .send(HubCommand::StartDownloadGroup { files })
        .await
        .unwrap();

    let mut finished = false;
    while let Some(evt) = evt_rx.recv().await {
        if let HubEvent::DownloadFinished { message } = evt {
            assert!(message.contains("Downloaded all 2 shards"));
            finished = true;
            break;
        }
    }
    assert!(finished);

    let p1 = dir.path().join("model-00001-of-00002.gguf");
    let p2 = dir.path().join("model-00002-of-00002.gguf");
    assert!(p1.exists());
    assert!(p2.exists());
    assert_eq!(std::fs::read(&p1).unwrap(), shard1_bytes);
    assert_eq!(std::fs::read(&p2).unwrap(), shard2_bytes);
}

#[test]
fn test_models_view_dual_mode_navigation_and_search() {
    let dir = tempdir().unwrap();
    let mut view = ModelsView::new(dir.path().to_path_buf());

    // Initially in Local mode
    assert_eq!(view.mode, ModelsTabMode::Local);

    // Toggle to HF Explorer
    let mode = view.toggle_mode();
    assert_eq!(mode, ModelsTabMode::HfExplorer);
    assert_eq!(view.mode, ModelsTabMode::HfExplorer);

    // Populate mock HF models
    let m1 = HfModelSummary {
        id: "Qwen/Qwen2.5-Coder-7B-GGUF".to_string(),
        author: Some("Qwen".to_string()),
        downloads: 50_000,
        likes: 1_200,
        private: false,
        gated: None,
        pipeline_tag: Some("text-generation".to_string()),
        tags: vec!["code".to_string(), "gguf".to_string()],
    };
    let m2 = HfModelSummary {
        id: "meta-llama/Llama-3.2-3B-Instruct-GGUF".to_string(),
        author: Some("meta-llama".to_string()),
        downloads: 120_000,
        likes: 3_500,
        private: false,
        gated: None,
        pipeline_tag: Some("text-generation".to_string()),
        tags: vec!["llama".to_string()],
    };

    view.set_hf_models(vec![m1.clone(), m2.clone()]);
    assert_eq!(view.hf_models.len(), 2);
    assert_eq!(view.hf_selected_idx, 0);
    assert_eq!(view.selected_hf_model().unwrap().id, m1.id);

    // Navigation
    view.hf_next();
    assert_eq!(view.hf_selected_idx, 1);
    assert_eq!(view.selected_hf_model().unwrap().id, m2.id);

    view.hf_next(); // wrap around
    assert_eq!(view.hf_selected_idx, 0);

    view.hf_previous(); // wrap backwards
    assert_eq!(view.hf_selected_idx, 1);

    // Search query interaction
    view.hf_is_searching = true;
    view.hf_search_query.push_str("deepseek");
    assert_eq!(view.hf_search_query, "deepseek");
    assert!(view.hf_is_searching);

    // Toggle back to Local mode resets search state
    let mode = view.toggle_mode();
    assert_eq!(mode, ModelsTabMode::Local);
    assert_eq!(view.mode, ModelsTabMode::Local);
    assert!(!view.hf_is_searching);
}
