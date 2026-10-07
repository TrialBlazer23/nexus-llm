//! Phase 10 — model store, blob Range transfer, catalog digests.
//!
//! Env locks intentionally span awaits so concurrent tests cannot stomp
//! `NEXUS_MODELS_INDEX` / `NEXUS_CONFIG` mid-request.
#![allow(clippy::await_holding_lock)]

use nexus::config::NexusConfig;
use nexus::control_plane::{
    build_model_catalog, fetch_models, request_blob_fetch, BlobFetchRequest, CONTROL_PLANE_VERSION,
};
use nexus::control_plane_server::{spawn_ephemeral, ControlPlaneContext};
use nexus::discovery::NodeRole;
use nexus::downloader::ModelDownloader;
use nexus::store::ModelIndex;
use nexus::supervisor::SupervisorManager;
use nexus::trust_auth::TrustBootstrap;
use std::io::Write;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock};
use tempfile::TempDir;
use uuid::Uuid;

/// Serialize tests that mutate process-wide `NEXUS_MODELS_INDEX`.
fn index_env_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

fn build_synthetic_gguf(arch: &str, name: &str) -> Vec<u8> {
    let mut buf = Vec::new();
    buf.extend_from_slice(&0x46554747u32.to_le_bytes());
    buf.extend_from_slice(&3u32.to_le_bytes());
    buf.extend_from_slice(&0u64.to_le_bytes());
    buf.extend_from_slice(&4u64.to_le_bytes());
    write_str(&mut buf, "general.architecture");
    buf.extend_from_slice(&8u32.to_le_bytes());
    write_str(&mut buf, arch);
    write_str(&mut buf, "general.name");
    buf.extend_from_slice(&8u32.to_le_bytes());
    write_str(&mut buf, name);
    write_str(&mut buf, "llama.context_length");
    buf.extend_from_slice(&4u32.to_le_bytes());
    buf.extend_from_slice(&4096u32.to_le_bytes());
    write_str(&mut buf, "llama.block_count");
    buf.extend_from_slice(&4u32.to_le_bytes());
    buf.extend_from_slice(&16u32.to_le_bytes());
    buf
}

fn write_str(buf: &mut Vec<u8>, s: &str) {
    buf.extend_from_slice(&(s.len() as u64).to_le_bytes());
    buf.extend_from_slice(s.as_bytes());
}

fn write_model(dir: &std::path::Path, name: &str) -> (PathBuf, String) {
    let path = dir.join(name);
    let bytes = build_synthetic_gguf("llama", "Tiny");
    std::fs::write(&path, &bytes).unwrap();
    // pad so Range tests have room
    let mut f = std::fs::OpenOptions::new()
        .append(true)
        .open(&path)
        .unwrap();
    f.write_all(&[0u8; 256]).unwrap();
    drop(f);
    let digest = ModelDownloader::calculate_sha256(&path).unwrap();
    (path, digest)
}

fn test_ctx(models_dir: PathBuf) -> (Arc<ControlPlaneContext>, TempDir) {
    let dir = TempDir::new().unwrap();
    let config_path = dir.path().join("config.toml");
    let mut config = NexusConfig::default();
    config.node.models_dir = models_dir;
    config.save_to_path(&config_path).unwrap();
    std::env::set_var("NEXUS_CONFIG", config_path.to_str().unwrap());
    let trust = TrustBootstrap::load(config).unwrap();
    let ctx = Arc::new(ControlPlaneContext::new(
        Uuid::new_v4(),
        NodeRole::HOST,
        SupervisorManager::new(),
        "127.0.0.1",
        18080,
        PathBuf::from("llama-server"),
        trust.identity,
        trust.config,
        trust.config_path,
    ));
    (ctx, dir)
}

#[test]
fn catalog_includes_digest() {
    let _guard = index_env_lock().lock().unwrap();
    let models = TempDir::new().unwrap();
    let index = TempDir::new().unwrap();
    let index_path = index.path().join("models.json");
    std::env::set_var("NEXUS_MODELS_INDEX", index_path.to_str().unwrap());
    let (_path, digest) = write_model(models.path(), "a.gguf");
    let catalog = build_model_catalog(Uuid::new_v4(), models.path());
    assert_eq!(catalog.models.len(), 1);
    assert_eq!(catalog.models[0].digest, digest);
    assert!(catalog.models[0].size_bytes > 0);
}

#[tokio::test]
async fn blob_range_returns_partial_content() {
    let _guard = index_env_lock().lock().unwrap();
    let models = TempDir::new().unwrap();
    let index = TempDir::new().unwrap();
    let index_path = index.path().join("models.json");
    std::env::set_var("NEXUS_MODELS_INDEX", index_path.to_str().unwrap());
    let (_path, digest) = write_model(models.path(), "blob.gguf");
    let _ = ModelIndex::reconcile(models.path(), &index_path).unwrap();

    let (ctx, _cfg_dir) = test_ctx(models.path().to_path_buf());
    // Ensure config models_dir matches
    {
        let mut cfg = ctx.config.write().unwrap();
        cfg.node.models_dir = models.path().to_path_buf();
    }
    let (addr, handle) = spawn_ephemeral(ctx).await.unwrap();
    let client = reqwest::Client::new();
    let url = format!("http://{}/nexus/control/v1/blob/{}", addr, digest);
    let resp = client
        .get(&url)
        .header("Range", "bytes=0-15")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::PARTIAL_CONTENT);
    let bytes = resp.bytes().await.unwrap();
    assert_eq!(bytes.len(), 16);
    handle.abort();
}

#[tokio::test]
async fn blob_unknown_digest_is_404() {
    let _guard = index_env_lock().lock().unwrap();
    let models = TempDir::new().unwrap();
    let index = TempDir::new().unwrap();
    std::env::set_var(
        "NEXUS_MODELS_INDEX",
        index.path().join("models.json").to_str().unwrap(),
    );
    let (ctx, _) = test_ctx(models.path().to_path_buf());
    let (addr, handle) = spawn_ephemeral(ctx).await.unwrap();
    let client = reqwest::Client::new();
    let resp = client
        .get(format!(
            "http://{}/nexus/control/v1/blob/{}",
            addr,
            "00".repeat(32)
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::NOT_FOUND);
    handle.abort();
}

#[tokio::test]
async fn catalog_http_exposes_digest() {
    let _guard = index_env_lock().lock().unwrap();
    let models = TempDir::new().unwrap();
    let index = TempDir::new().unwrap();
    let index_path = index.path().join("models.json");
    std::env::set_var("NEXUS_MODELS_INDEX", index_path.to_str().unwrap());
    let (_path, digest) = write_model(models.path(), "cat.gguf");
    let (ctx, _) = test_ctx(models.path().to_path_buf());
    {
        let mut cfg = ctx.config.write().unwrap();
        cfg.node.models_dir = models.path().to_path_buf();
    }
    let (addr, handle) = spawn_ephemeral(ctx).await.unwrap();
    let client = reqwest::Client::new();
    let catalog = fetch_models(&client, &format!("http://{}", addr))
        .await
        .unwrap();
    assert_eq!(catalog.models.len(), 1);
    assert_eq!(catalog.models[0].digest, digest);
    handle.abort();
}

#[tokio::test]
async fn blob_fetch_accepted() {
    let _guard = index_env_lock().lock().unwrap();
    let models = TempDir::new().unwrap();
    let index = TempDir::new().unwrap();
    std::env::set_var(
        "NEXUS_MODELS_INDEX",
        index.path().join("models.json").to_str().unwrap(),
    );
    let (ctx, _) = test_ctx(models.path().to_path_buf());
    let (addr, handle) = spawn_ephemeral(ctx).await.unwrap();
    let client = reqwest::Client::new();
    let resp = request_blob_fetch(
        &client,
        &format!("http://{}", addr),
        &BlobFetchRequest {
            protocol_version: CONTROL_PLANE_VERSION,
            requester_id: Uuid::new_v4(),
            digest: "ab".repeat(32),
            source_base_url: format!("http://{}", addr),
        },
        None,
    )
    .await
    .unwrap();
    assert!(resp.accepted);
    handle.abort();
}

#[tokio::test]
async fn downloader_rejects_bad_sidecar_and_resumes() {
    let _guard = index_env_lock().lock().unwrap();
    let dir = TempDir::new().unwrap();
    let dest = dir.path().join("out.bin");
    let part = PathBuf::from(format!("{}.part", dest.display()));
    let side = PathBuf::from(format!("{}.part.json", dest.display()));
    std::fs::write(&part, b"stale").unwrap();
    std::fs::write(
        &side,
        br#"{"url":"http://other/file","expected_sha256":"dead"}"#,
    )
    .unwrap();

    // Spin a tiny hyper static file server via control plane blob is heavy; use
    // reqwest against a data-less path — instead verify discard by re-running
    // download against a local file server from spawn_ephemeral.
    let models = TempDir::new().unwrap();
    let index = TempDir::new().unwrap();
    let index_path = index.path().join("models.json");
    std::env::set_var("NEXUS_MODELS_INDEX", index_path.to_str().unwrap());
    let (model_path, digest) = write_model(models.path(), "dl.gguf");
    let _ = ModelIndex::reconcile(models.path(), &index_path).unwrap();
    let (ctx, _) = test_ctx(models.path().to_path_buf());
    {
        let mut cfg = ctx.config.write().unwrap();
        cfg.node.models_dir = models.path().to_path_buf();
    }
    let (addr, handle) = spawn_ephemeral(ctx).await.unwrap();
    let url = format!("http://{}/nexus/control/v1/blob/{}", addr, digest);

    // Poisoned partial for this URL with wrong hash → must restart
    let dest2 = dir.path().join("pulled.gguf");
    let part2 = PathBuf::from(format!("{}.part", dest2.display()));
    let side2 = PathBuf::from(format!("{}.part.json", dest2.display()));
    std::fs::write(&part2, b"xxx").unwrap();
    std::fs::write(
        &side2,
        format!(
            r#"{{"url":"{}","expected_sha256":"00{}"}}"#,
            url,
            "11".repeat(31)
        )
        .as_bytes(),
    )
    .unwrap();

    let dl = ModelDownloader::new();
    dl.download(&url, &dest2, Some(&digest), |_| {})
        .await
        .expect("download should succeed after discarding bad partial");
    assert!(dest2.exists());
    assert_eq!(ModelDownloader::calculate_sha256(&dest2).unwrap(), digest);
    assert_eq!(
        std::fs::metadata(&dest2).unwrap().len(),
        std::fs::metadata(&model_path).unwrap().len()
    );
    handle.abort();
}
