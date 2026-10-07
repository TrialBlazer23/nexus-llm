//! Phase 2 integration tests — model deletion, shard cleanup, and download cancellation.

use nexus::config::NexusConfig;
use nexus::discovery::DiscoveryService;
use nexus::downloader::{DownloaderError, ModelDownloader};
use nexus::node_identity::NodeIdentity;
use nexus::supervisor::SupervisorManager;
use nexus::ui::hub::commands::{spawn_hub_worker, HubCommand, HubEvent, HubWorkerCtx};
use nexus::ui::models::detect_model_shards;
use std::path::PathBuf;
use std::sync::Arc;
use tempfile::TempDir;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::{mpsc, watch};

async fn run_mock_http_server(data: Vec<u8>) -> (String, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let data_len = data.len();

    let handle = tokio::spawn(async move {
        while let Ok((mut stream, _)) = listener.accept().await {
            let data = data.clone();
            tokio::spawn(async move {
                let mut buf = vec![0u8; 4096];
                let n = stream.read(&mut buf).await.unwrap_or(0);
                let req = String::from_utf8_lossy(&buf[..n]);

                let range_start = if let Some(pos) = req.find("Range: bytes=") {
                    let rest = &req[pos + 13..];
                    let end_pos = rest.find('-').unwrap_or(0);
                    rest[..end_pos].parse::<usize>().unwrap_or(0)
                } else {
                    0
                };

                if range_start > 0 {
                    let slice = &data[range_start..];
                    let header = format!(
                        "HTTP/1.1 206 Partial Content\r\nContent-Length: {}\r\nContent-Range: bytes {}-{}/{}\r\nAccept-Ranges: bytes\r\nConnection: close\r\n\r\n",
                        slice.len(),
                        range_start,
                        data_len - 1,
                        data_len
                    );
                    let _ = stream.write_all(header.as_bytes()).await;
                    let _ = stream.write_all(slice).await;
                    let _ = stream.flush().await;
                } else {
                    let header = format!(
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nAccept-Ranges: bytes\r\nConnection: close\r\n\r\n",
                        data_len
                    );
                    let _ = stream.write_all(header.as_bytes()).await;
                    let half = data.len() / 2;
                    let _ = stream.write_all(&data[..half]).await;
                    let _ = stream.flush().await;
                    // Keep stream open briefly so client can process the chunk and cancel
                    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                    let _ = stream.write_all(&data[half..]).await;
                    let _ = stream.flush().await;
                }
            });
        }
    });

    (format!("http://{}", addr), handle)
}

#[tokio::test]
async fn test_download_cancellation_preserves_partial_state_and_resumes() {
    let temp_dir = TempDir::new().unwrap();
    let dest_file = temp_dir.path().join("model.bin");
    let part_file = PathBuf::from(format!("{}.part", dest_file.display()));
    let sidecar_file = PathBuf::from(format!("{}.part.json", dest_file.display()));

    let dummy_data: Vec<u8> = (0..20_000).map(|i| (i % 251) as u8).collect();
    let (server_url, server_handle) = run_mock_http_server(dummy_data.clone()).await;

    let (cancel_tx, cancel_rx) = watch::channel(false);
    let downloader = ModelDownloader::new();

    let dest_clone = dest_file.clone();
    let server_url_clone = server_url.clone();
    let cancel_tx_clone = cancel_tx.clone();

    // Trigger cancellation after receiving initial bytes
    let download_handle = tokio::spawn(async move {
        downloader
            .download_with_cancellation(
                &server_url_clone,
                &dest_clone,
                None,
                Some(cancel_rx),
                move |prog| {
                    if prog.downloaded_bytes > 0 {
                        let _ = cancel_tx_clone.send(true);
                    }
                },
            )
            .await
    });

    let res = download_handle.await.unwrap();
    match res {
        Err(DownloaderError::Cancelled) => {}
        other => panic!("Expected DownloaderError::Cancelled, got: {:?}", other),
    }

    // Verify .part and .part.json exist and are intact
    assert!(
        part_file.exists(),
        ".part file must exist after cancellation"
    );
    assert!(
        sidecar_file.exists(),
        ".part.json sidecar must exist after cancellation"
    );
    let partial_len = std::fs::metadata(&part_file).unwrap().len();
    assert!(partial_len > 0, "Partial file must have downloaded bytes");
    assert!(
        !dest_file.exists(),
        "Target file must not exist until completion"
    );

    // Resume download with new cancel receiver (not cancelled)
    let (_new_cancel_tx, new_cancel_rx) = watch::channel(false);
    let resume_downloader = ModelDownloader::new();
    let resume_res = resume_downloader
        .download_with_cancellation(&server_url, &dest_file, None, Some(new_cancel_rx), |_| {})
        .await;

    assert!(
        resume_res.is_ok(),
        "Resumed download should succeed: {:?}",
        resume_res
    );
    assert!(
        dest_file.exists(),
        "Target file should exist after resumed completion"
    );
    assert!(!part_file.exists(), ".part file should be renamed/removed");
    assert!(
        !sidecar_file.exists(),
        ".part.json sidecar should be removed"
    );

    let downloaded_data = std::fs::read(&dest_file).unwrap();
    assert_eq!(downloaded_data.len(), dummy_data.len());
    assert_eq!(downloaded_data, dummy_data);

    server_handle.abort();
}

#[tokio::test]
async fn test_delete_model_multi_shard_and_sidecars() {
    let temp_dir = TempDir::new().unwrap();
    let dir = temp_dir.path();

    let shard1 = dir.join("deepseek-v3-00001-of-00003.gguf");
    let shard2 = dir.join("deepseek-v3-00002-of-00003.gguf");
    let shard3 = dir.join("deepseek-v3-00003-of-00003.gguf");
    let shard1_part = dir.join("deepseek-v3-00001-of-00003.gguf.part");
    let shard1_sidecar = dir.join("deepseek-v3-00001-of-00003.gguf.part.json");
    let shard3_part = dir.join("deepseek-v3-00003-of-00003.gguf.part");

    std::fs::write(&shard1, b"shard 1 content").unwrap();
    std::fs::write(&shard2, b"shard 2 content").unwrap();
    std::fs::write(&shard3, b"shard 3 content").unwrap();
    std::fs::write(&shard1_part, b"part 1 content").unwrap();
    std::fs::write(&shard1_sidecar, b"{}").unwrap();
    std::fs::write(&shard3_part, b"part 3 content").unwrap();

    let detected = detect_model_shards(&shard1);
    assert_eq!(detected.len(), 3);
    assert!(detected.contains(&shard1));
    assert!(detected.contains(&shard2));
    assert!(detected.contains(&shard3));

    let mut config = NexusConfig::default();
    config.node.models_dir = dir.to_path_buf();
    let discovery = Arc::new(DiscoveryService::new(config.clone(), None));
    let identity = Arc::new(NodeIdentity::generate());
    let ctx = HubWorkerCtx {
        config,
        discovery,
        supervisor: SupervisorManager::new(),
        identity,
    };

    let (evt_tx, mut evt_rx) = mpsc::channel(10);
    nexus::ui::hub::commands::run_delete_model(
        &ctx,
        &evt_tx,
        shard1.clone(),
        detected,
        "deepseek-v3-00001-of-00003.gguf".to_string(),
    )
    .await;

    let evt = evt_rx.recv().await.expect("Expected ModelDeleted event");
    match evt {
        HubEvent::ModelDeleted { filename } => {
            assert_eq!(filename, "deepseek-v3-00001-of-00003.gguf");
        }
        other => panic!("Expected ModelDeleted, got {:?}", other),
    }

    assert!(!shard1.exists(), "Shard 1 must be deleted");
    assert!(!shard2.exists(), "Shard 2 must be deleted");
    assert!(!shard3.exists(), "Shard 3 must be deleted");
    assert!(!shard1_part.exists(), "Shard 1 .part must be deleted");
    assert!(
        !shard1_sidecar.exists(),
        "Shard 1 .part.json must be deleted"
    );
    assert!(!shard3_part.exists(), "Shard 3 .part must be deleted");
}

#[tokio::test]
async fn test_delete_model_via_command_worker() {
    let temp_dir = TempDir::new().unwrap();
    let dir = temp_dir.path();

    let model_file = dir.join("single-model.gguf");
    let model_part = dir.join("single-model.gguf.part");
    let model_sidecar = dir.join("single-model.gguf.part.json");

    std::fs::write(&model_file, b"content").unwrap();
    std::fs::write(&model_part, b"part").unwrap();
    std::fs::write(&model_sidecar, b"{}").unwrap();

    let mut config = NexusConfig::default();
    config.node.models_dir = dir.to_path_buf();
    let discovery = Arc::new(DiscoveryService::new(config.clone(), None));
    let identity = Arc::new(NodeIdentity::generate());
    let ctx = HubWorkerCtx {
        config,
        discovery,
        supervisor: SupervisorManager::new(),
        identity,
    };

    let (cmd_tx, cmd_rx) = mpsc::channel(10);
    let (evt_tx, mut evt_rx) = mpsc::channel(10);

    let _worker_handle = spawn_hub_worker(cmd_rx, evt_tx, ctx);

    cmd_tx
        .send(HubCommand::DeleteModel {
            path: model_file.clone(),
            shards: vec![model_file.clone()],
            filename: "single-model.gguf".to_string(),
        })
        .await
        .unwrap();

    let evt = evt_rx.recv().await.expect("Expected ModelDeleted event");
    match evt {
        HubEvent::ModelDeleted { filename } => {
            assert_eq!(filename, "single-model.gguf");
        }
        other => panic!("Expected ModelDeleted, got {:?}", other),
    }

    assert!(!model_file.exists());
    assert!(!model_part.exists());
    assert!(!model_sidecar.exists());
}
