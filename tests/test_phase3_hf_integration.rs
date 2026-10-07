//! Phase 3 integration tests — Hugging Face REST API client, repo parsing, and gated auth recovery.

use nexus::hf::{FitStatus, HfClient, HfError};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

async fn run_mock_hf_server() -> (String, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    let handle = tokio::spawn(async move {
        while let Ok((mut stream, _)) = listener.accept().await {
            tokio::spawn(async move {
                let mut buf = vec![0u8; 4096];
                let n = stream.read(&mut buf).await.unwrap_or(0);
                let req = String::from_utf8_lossy(&buf[..n]);

                if req.contains("GET /api/models/test-org/test-model") {
                    let body = r#"{
                        "_id": "mock_id",
                        "id": "test-org/test-model",
                        "author": "test-org",
                        "downloads": 5432,
                        "likes": 128,
                        "private": false,
                        "gated": false,
                        "siblings": [
                            {
                                "rfilename": "test-model-Q4_K_M.gguf",
                                "size": 2100000000,
                                "lfs": { "size": 2100000000, "sha256": "sha_q4_test" }
                            },
                            {
                                "rfilename": "test-model-Q8_0-00001-of-00002.gguf",
                                "size": 1800000000,
                                "lfs": { "size": 1800000000, "sha256": "sha_q8_part1" }
                            },
                            {
                                "rfilename": "test-model-Q8_0-00002-of-00002.gguf",
                                "size": 1700000000,
                                "lfs": { "size": 1700000000, "sha256": "sha_q8_part2" }
                            },
                            {
                                "rfilename": "README.md",
                                "size": 1024
                            }
                        ]
                    }"#;
                    let resp = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                        body.len(),
                        body
                    );
                    let _ = stream.write_all(resp.as_bytes()).await;
                } else if req.contains("GET /api/models/gated/model") {
                    let body = r#"{"error":"Gated model. You must be authenticated to access this model."}"#;
                    let resp = format!(
                        "HTTP/1.1 401 Unauthorized\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                        body.len(),
                        body
                    );
                    let _ = stream.write_all(resp.as_bytes()).await;
                } else if req.contains("GET /api/models/authenticated/model") {
                    if req
                        .to_ascii_lowercase()
                        .contains("authorization: bearer hf_secret_token_123")
                    {
                        let body = r#"{"id":"authenticated/model","siblings":[]}"#;
                        let resp = format!(
                            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                            body.len(),
                            body
                        );
                        let _ = stream.write_all(resp.as_bytes()).await;
                    } else {
                        let body = r#"{"error":"Missing or invalid token"}"#;
                        let resp = format!(
                            "HTTP/1.1 403 Forbidden\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                            body.len(),
                            body
                        );
                        let _ = stream.write_all(resp.as_bytes()).await;
                    }
                } else {
                    let body = r#"{"error":"Not Found"}"#;
                    let resp = format!(
                        "HTTP/1.1 404 Not Found\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                        body.len(),
                        body
                    );
                    let _ = stream.write_all(resp.as_bytes()).await;
                }
            });
        }
    });

    (format!("http://{}", addr), handle)
}

#[tokio::test]
async fn test_hf_client_model_details_and_gguf_groups() {
    let (base_url, server_handle) = run_mock_hf_server().await;
    let client = HfClient::new(None).with_base_url(&base_url);

    let detail = client
        .model_details("test-org/test-model")
        .await
        .expect("model_details should succeed");

    assert_eq!(detail.id, "test-org/test-model");
    assert_eq!(detail.siblings.len(), 4);

    // Available RAM = 4000 MB, Cluster Free RAM = 8000 MB
    let groups = HfClient::parse_gguf_groups(&detail, 4000, 8000);
    assert_eq!(groups.len(), 2, "Only GGUF files should be grouped");

    // Check Q4_K_M group
    let q4 = groups.iter().find(|g| g.quant_label == "Q4_K_M").unwrap();
    assert_eq!(q4.files.len(), 1);
    assert!(!q4.is_sharded);
    assert_eq!(q4.total_size_bytes, 2_100_000_000);
    assert_eq!(q4.fit_status, FitStatus::Fits);
    assert_eq!(q4.fit_status.badge_text(), "[OK] Fits");

    // Check Q8_0 sharded group
    let q8 = groups.iter().find(|g| g.quant_label == "Q8_0").unwrap();
    assert_eq!(q8.files.len(), 2);
    assert!(q8.is_sharded);
    assert_eq!(q8.total_size_bytes, 3_500_000_000);
    assert_eq!(q8.fit_status, FitStatus::OffloadRequired);
    assert_eq!(q8.fit_status.badge_text(), "[RPC] Offload");

    server_handle.abort();
}

#[tokio::test]
async fn test_hf_client_gated_auth_error_401() {
    let (base_url, server_handle) = run_mock_hf_server().await;
    let client = HfClient::new(None).with_base_url(&base_url);

    let res = client.model_details("gated/model").await;
    match res {
        Err(HfError::GatedOrUnauthorized { repo_id, status }) => {
            assert_eq!(repo_id, "gated/model");
            assert_eq!(status, 401);
        }
        other => panic!("Expected GatedOrUnauthorized error, got: {:?}", other),
    }

    server_handle.abort();
}

#[tokio::test]
async fn test_hf_client_not_found_404() {
    let (base_url, server_handle) = run_mock_hf_server().await;
    let client = HfClient::new(None).with_base_url(&base_url);

    let res = client.model_details("nonexistent/model").await;
    match res {
        Err(HfError::NotFound(repo_id)) => {
            assert_eq!(repo_id, "nonexistent/model");
        }
        other => panic!("Expected NotFound error, got: {:?}", other),
    }

    server_handle.abort();
}

#[tokio::test]
async fn test_hf_client_bearer_token_sent() {
    let (base_url, server_handle) = run_mock_hf_server().await;

    // 1. Without token -> 403 Forbidden
    let client_no_token = HfClient::new(None).with_base_url(&base_url);
    let res = client_no_token.model_details("authenticated/model").await;
    assert!(matches!(
        res,
        Err(HfError::GatedOrUnauthorized { status: 403, .. })
    ));

    // 2. With valid token -> 200 OK
    let client_with_token =
        HfClient::new(Some("hf_secret_token_123".to_string())).with_base_url(&base_url);
    let res_auth = client_with_token.model_details("authenticated/model").await;
    assert!(
        res_auth.is_ok(),
        "Authenticated request must succeed: {:?}",
        res_auth
    );

    server_handle.abort();
}
