//! Hub command/event bus (Phase 8).
//!
//! Long-running work (model load/unload, peer refresh, target selection probe,
//! remote dispatch) runs off the TUI event loop. The loop only draws, mutates
//! state from [`HubEvent`], and enqueues [`HubCommand`].

use super::{HotSwapIntent, TargetExecutionNode, TargetSelectionState};
use crate::config::NexusConfig;
use crate::control_plane::ControlPlaneError;
use crate::control_plane::{
    blob_url, dispatch_load_model, dispatch_load_model_signed, fetch_models, request_blob_fetch,
    BlobFetchRequest, ModelLoadRequest, ModelLoadResponse, CONTROL_PLANE_VERSION,
};
use crate::discovery::{DiscoveryService, RpcSelectionPolicy, StatusFlags};
use crate::downloader::{DownloadAuth, ModelDownloader};
use crate::node_identity::NodeIdentity;
use crate::store::ModelIndex;
use crate::supervisor::{LlamaServerConfig, SupervisorManager, SupervisorState};
use crate::sysinfo::SystemProfile;
use crate::ui::models_view::ModelsView;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;
use tracing::{error, info};
use uuid::Uuid;

/// Commands issued by the event loop to background workers.
#[derive(Debug)]
pub enum HubCommand {
    /// Open target-selection modal (probe peers off the UI thread).
    OpenTargetSelection {
        model_path: PathBuf,
    },
    /// Load locally with optional GPU-layer override (0 = CPU safe mode).
    LoadModelLocal {
        path: PathBuf,
        gpu_layers: Option<u32>,
        context_size: usize,
        /// Precomputed llama-server extras (e.g. ranked distributed `--rpc` args).
        extra_args: Vec<String>,
    },
    /// Dispatch remote load via signed/unsigned control plane.
    LoadModelRemote {
        endpoint: String,
        api_endpoint: String,
        name: String,
        backend: String,
        model_name: String,
        context_size: usize,
        gpu_layers: u32,
    },
    Unload {
        active_model_name: String,
    },
    RefreshCluster,
    /// WAN download into models_dir.
    StartDownload {
        url: String,
    },
    /// LAN blob pull from a peer control endpoint.
    TransferModel {
        peer_endpoint: String,
        digest: String,
    },
    /// Ask a peer to pull our blob (push convenience).
    PushModel {
        peer_endpoint: String,
        digest: String,
        source_base_url: String,
    },
    /// Refresh mesh model catalogs from peers.
    RefreshModelCatalog,
}

/// Events applied on the UI thread.
#[derive(Debug)]
pub enum HubEvent {
    Status {
        message: String,
        color: ratatui::style::Color,
    },
    ModelLoadProgress {
        phase: String,
    },
    ModelLoaded {
        model_name: String,
        endpoint: String,
        backend_label: String,
        notice: String,
    },
    ModelFailed {
        message: String,
    },
    ModelUnloaded {
        unloaded_model: String,
    },
    UnloadNoop,
    TargetSelectionReady(TargetSelectionState),
    RemoteLoadSucceeded {
        model_name: String,
        name: String,
        backend: String,
        api_endpoint: String,
        notice: String,
    },
    RemoteLoadFailed {
        message: String,
    },
    ClusterRefreshed,
    /// Supervisor child exited unexpectedly.
    SupervisorCrashed {
        model: String,
        code: Option<i32>,
        stderr: String,
    },
    DownloadProgress {
        downloaded_bytes: u64,
        total_bytes: Option<u64>,
        percent: Option<f32>,
        speed_bytes_per_sec: f64,
        label: String,
    },
    DownloadFinished {
        message: String,
    },
    DownloadFailed {
        message: String,
    },
    ModelCatalogUpdated {
        remotes: Vec<(String, String, crate::control_plane::ModelCatalogResponse)>,
    },
}

#[derive(Clone)]
pub struct HubWorkerCtx {
    pub config: NexusConfig,
    pub discovery: Arc<DiscoveryService>,
    pub supervisor: SupervisorManager,
    pub identity: Arc<NodeIdentity>,
}

/// Spawn the background command worker. Returns a join handle.
pub fn spawn_hub_worker(
    mut cmd_rx: mpsc::Receiver<HubCommand>,
    evt_tx: mpsc::Sender<HubEvent>,
    ctx: HubWorkerCtx,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        while let Some(cmd) = cmd_rx.recv().await {
            match cmd {
                HubCommand::OpenTargetSelection { model_path } => {
                    let state = build_target_selection(&ctx, model_path).await;
                    let _ = evt_tx.send(HubEvent::TargetSelectionReady(state)).await;
                }
                HubCommand::LoadModelLocal {
                    path,
                    gpu_layers,
                    context_size,
                    extra_args,
                } => {
                    run_local_load(&ctx, &evt_tx, path, gpu_layers, context_size, extra_args).await;
                }
                HubCommand::LoadModelRemote {
                    endpoint,
                    api_endpoint,
                    name,
                    backend,
                    model_name,
                    context_size,
                    gpu_layers,
                } => {
                    run_remote_load(
                        &ctx,
                        &evt_tx,
                        &endpoint,
                        &api_endpoint,
                        &name,
                        &backend,
                        &model_name,
                        context_size,
                        gpu_layers,
                    )
                    .await;
                }
                HubCommand::Unload { active_model_name } => {
                    run_unload(&ctx, &evt_tx, active_model_name).await;
                }
                HubCommand::RefreshCluster => {
                    // ClusterView.refresh is applied on the UI side after this signal;
                    // worker only does the discovery wait cost by touching peers.
                    let _ = ctx.discovery.get_active_peers().await;
                    let _ = evt_tx.send(HubEvent::ClusterRefreshed).await;
                }
                HubCommand::StartDownload { url } => {
                    run_download(&ctx, &evt_tx, url, None).await;
                }
                HubCommand::TransferModel {
                    peer_endpoint,
                    digest,
                } => {
                    run_transfer(&ctx, &evt_tx, peer_endpoint, digest).await;
                }
                HubCommand::PushModel {
                    peer_endpoint,
                    digest,
                    source_base_url,
                } => {
                    run_push(&ctx, &evt_tx, peer_endpoint, digest, source_base_url).await;
                }
                HubCommand::RefreshModelCatalog => {
                    let ctx_clone = ctx.clone();
                    let evt_clone = evt_tx.clone();
                    tokio::spawn(async move {
                        run_refresh_catalog(&ctx_clone, &evt_clone).await;
                    });
                }
            }
        }
    })
}

async fn run_download(
    ctx: &HubWorkerCtx,
    evt_tx: &mpsc::Sender<HubEvent>,
    url: String,
    expected_sha: Option<String>,
) {
    let dest = ModelsView::download_dest_from_url(&ctx.config.node.models_dir, &url);
    let _ = evt_tx
        .send(HubEvent::DownloadProgress {
            downloaded_bytes: 0,
            total_bytes: None,
            percent: Some(0.0),
            speed_bytes_per_sec: 0.0,
            label: format!(
                "Downloading {}",
                dest.file_name().and_then(|s| s.to_str()).unwrap_or("model")
            ),
        })
        .await;

    let downloader = ModelDownloader::new();
    let evt = evt_tx.clone();
    let label = dest
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("model")
        .to_string();
    let result = downloader
        .download(&url, &dest, expected_sha.as_deref(), move |p| {
            let _ = evt.try_send(HubEvent::DownloadProgress {
                downloaded_bytes: p.downloaded_bytes,
                total_bytes: p.total_bytes,
                percent: p.percent,
                speed_bytes_per_sec: p.speed_bytes_per_sec,
                label: label.clone(),
            });
        })
        .await;

    match result {
        Ok(()) => {
            let _ = ModelIndex::reconcile_default(&ctx.config.node.models_dir);
            let _ = evt_tx
                .send(HubEvent::DownloadFinished {
                    message: format!("Downloaded {}", dest.display()),
                })
                .await;
        }
        Err(e) => {
            let _ = evt_tx
                .send(HubEvent::DownloadFailed {
                    message: format!("Download failed: {e}"),
                })
                .await;
        }
    }
}

async fn run_transfer(
    ctx: &HubWorkerCtx,
    evt_tx: &mpsc::Sender<HubEvent>,
    peer_endpoint: String,
    digest: String,
) {
    let digest = digest.trim().to_lowercase();
    let url = match blob_url(&peer_endpoint, &digest) {
        Ok(u) => u,
        Err(e) => {
            let _ = evt_tx
                .send(HubEvent::DownloadFailed {
                    message: format!("Bad peer endpoint: {e}"),
                })
                .await;
            return;
        }
    };
    let dest = ctx.config.node.models_dir.join(format!("{digest}.gguf"));

    let _ = evt_tx
        .send(HubEvent::DownloadProgress {
            downloaded_bytes: 0,
            total_bytes: None,
            percent: Some(0.0),
            speed_bytes_per_sec: 0.0,
            label: format!("Pulling {digest:.12}…"),
        })
        .await;

    let downloader = ModelDownloader::new();
    let auth = if ctx.config.network.security.pairing_enforced() {
        let signer_id = ctx.config.node_uuid().unwrap_or_else(|_| Uuid::nil());
        Some(DownloadAuth {
            identity: ctx.identity.clone(),
            signer_id,
        })
    } else {
        None
    };
    let evt = evt_tx.clone();
    let label = digest.clone();
    let result = downloader
        .download_authenticated(&url, &dest, Some(&digest), auth.as_ref(), move |p| {
            let _ = evt.try_send(HubEvent::DownloadProgress {
                downloaded_bytes: p.downloaded_bytes,
                total_bytes: p.total_bytes,
                percent: p.percent,
                speed_bytes_per_sec: p.speed_bytes_per_sec,
                label: format!("Pulling {label:.12}…"),
            });
        })
        .await;

    match result {
        Ok(()) => {
            let _ = ModelIndex::reconcile_default(&ctx.config.node.models_dir);
            let _ = evt_tx
                .send(HubEvent::DownloadFinished {
                    message: format!("Transferred {digest:.16}… verified"),
                })
                .await;
        }
        Err(e) => {
            let _ = evt_tx
                .send(HubEvent::DownloadFailed {
                    message: format!("Transfer failed: {e}"),
                })
                .await;
        }
    }
}

async fn run_push(
    ctx: &HubWorkerCtx,
    evt_tx: &mpsc::Sender<HubEvent>,
    peer_endpoint: String,
    digest: String,
    source_base_url: String,
) {
    let requester_id = ctx.config.node_uuid().unwrap_or_else(|_| Uuid::nil());
    let req = BlobFetchRequest {
        protocol_version: CONTROL_PLANE_VERSION,
        requester_id,
        digest: digest.clone(),
        source_base_url,
    };
    let client = reqwest::Client::new();
    let identity = if ctx.config.network.security.pairing_enforced() {
        Some(ctx.identity.as_ref())
    } else {
        None
    };
    match request_blob_fetch(&client, &peer_endpoint, &req, identity).await {
        Ok(resp) if resp.accepted => {
            let _ = evt_tx
                .send(HubEvent::Status {
                    message: format!("Push accepted by peer for {digest:.16}…"),
                    color: ratatui::style::Color::Green,
                })
                .await;
        }
        Ok(resp) => {
            let _ = evt_tx
                .send(HubEvent::Status {
                    message: format!("Push rejected: {}", resp.message),
                    color: ratatui::style::Color::Yellow,
                })
                .await;
        }
        Err(e) => {
            let _ = evt_tx
                .send(HubEvent::Status {
                    message: format!("Push failed: {e}"),
                    color: ratatui::style::Color::Red,
                })
                .await;
        }
    }
}

async fn run_refresh_catalog(ctx: &HubWorkerCtx, evt_tx: &mpsc::Sender<HubEvent>) {
    let peers = ctx.discovery.get_active_peers().await;
    if peers.is_empty() {
        let _ = evt_tx.send(HubEvent::ModelCatalogUpdated { remotes: Vec::new() }).await;
        return;
    }
    let client = reqwest::Client::builder()
        .timeout(Duration::from_millis(1500))
        .build()
        .unwrap_or_else(|_| reqwest::Client::new());

    let futures = peers.into_iter().map(|p| {
        let client = client.clone();
        async move {
            let endpoint = p.control_endpoint();
            match fetch_models(&client, &endpoint).await {
                Ok(catalog) => Some((p.label(), endpoint, catalog)),
                Err(e) => {
                    tracing::debug!("catalog fetch from {} failed: {}", endpoint, e);
                    None
                }
            }
        }
    });

    let results = futures_util::future::join_all(futures).await;
    let remotes: Vec<_> = results.into_iter().flatten().collect();
    let _ = evt_tx.send(HubEvent::ModelCatalogUpdated { remotes }).await;
}

pub(crate) async fn build_target_selection(
    ctx: &HubWorkerCtx,
    model_path: PathBuf,
) -> TargetSelectionState {
    use crate::cluster::{
        rank_execution_plans, LinkQuality, MemoryPolicy, NodeBudget, PlacementCandidate,
        PlacementRequest, PlanTarget,
    };
    use crate::gguf::GgufMetadata;

    let model_name = model_path
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("unknown")
        .to_string();

    let profile = SystemProfile::probe();
    let local_cap_mb = profile
        .max_allowed_memory_bytes_pct(ctx.config.hardware.safety.max_ram_usage_percent)
        / (1024 * 1024);
    let gpu_layers = if ctx.config.hardware.acceleration.prefer_gpu {
        ctx.config.hardware.acceleration.gpu_layers
    } else {
        99
    };
    let ctx_size = 4096usize; // refined by selected_context at confirm time

    let mut placement_candidates = Vec::new();
    let peers = ctx.discovery.get_active_peers().await;
    for p in &peers {
        if p.status.is_ready() || p.role.is_host() || p.is_rpc_ready() {
            let link = ctx
                .discovery
                .cached_link_quality(p.uuid)
                .await
                .unwrap_or_else(LinkQuality::unknown);
            // Use advertised free RAM (a 32 GB worker can exceed the 1800 MB default cap).
            let budget_mb = (p.free_ram_mb as u64).max(1);
            placement_candidates.push(PlacementCandidate {
                name: p.label(),
                budget: NodeBudget::new(p.uuid, p.label(), budget_mb),
                backend: p.backend,
                rpc_endpoint: if p.is_rpc_ready() {
                    Some(p.rpc_endpoint())
                } else {
                    None
                },
                link,
                is_local: false,
                thermal_index: p.thermal_index,
            });
        }
    }

    let mut candidates = Vec::new();
    if let Ok(gguf) = GgufMetadata::open(&model_path) {
        let policy = MemoryPolicy::from_safety(
            ctx.config.hardware.safety.max_ram_usage_percent,
            ctx.config.hardware.safety.mmap,
            ctx.config.hardware.safety.mlock,
            ctx_size,
        );
        let bench_store = crate::bench::BenchStore::load_default().ok();
        let req = PlacementRequest {
            gguf: &gguf,
            policy,
            local_profile: profile.clone(),
            local_name: "local".into(),
            local_gpu_layers: gpu_layers,
            candidates: placement_candidates,
            enable_rpc: ctx.config.cluster.enable_rpc && ctx.config.cluster.auto_offload,
            bench: bench_store.as_ref(),
        };
        if let Ok(plans) = rank_execution_plans(&req) {
            for plan in plans {
                let label = format!("~{:.1} tok/s", plan.predicted_tok_s);
                match plan.target {
                    PlanTarget::LocalGpu { ngl } => {
                        candidates.push(TargetExecutionNode::Local {
                            allocatable_mb: local_cap_mb,
                            backend: profile.detected_backend.to_string(),
                            gpu_layers: ngl,
                            predicted_label: label,
                        });
                    }
                    PlanTarget::LocalCpu => {
                        candidates.push(TargetExecutionNode::LocalCpu {
                            allocatable_mb: local_cap_mb,
                            backend: "CPU Fallback".into(),
                            threads: profile.recommended_threads,
                            predicted_label: label,
                        });
                    }
                    PlanTarget::Remote { peer_name } => {
                        if let Some(p) = peers.iter().find(|p| p.label() == peer_name) {
                            candidates.push(TargetExecutionNode::Remote {
                                uuid: p.uuid,
                                name: peer_name,
                                endpoint: p.control_endpoint(),
                                api_endpoint: p.api_endpoint(),
                                free_ram_mb: p.free_ram_mb,
                                backend: p.backend.to_string(),
                                predicted_label: label,
                            });
                        }
                    }
                    PlanTarget::Distributed { worker_names } => {
                        candidates.push(TargetExecutionNode::Distributed {
                            worker_names,
                            predicted_label: label,
                            rpc_endpoints: plan
                                .split
                                .as_ref()
                                .map(|s| s.remote_endpoints.clone())
                                .unwrap_or_default(),
                            extra_args: plan.llama_extra_args,
                            gpu_layers: plan.gpu_layers,
                        });
                    }
                }
            }
        }
    }

    // Fallback if ranking produced nothing (missing geometry / parse failure).
    if candidates.is_empty() {
        candidates.push(TargetExecutionNode::Local {
            allocatable_mb: local_cap_mb,
            backend: profile.detected_backend.to_string(),
            gpu_layers,
            predicted_label: "~?".into(),
        });
        candidates.push(TargetExecutionNode::LocalCpu {
            allocatable_mb: local_cap_mb,
            backend: "CPU Fallback".into(),
            threads: profile.recommended_threads,
            predicted_label: "~?".into(),
        });
        for p in &peers {
            if p.status.is_ready() || p.role.is_host() || p.is_rpc_ready() {
                candidates.push(TargetExecutionNode::Remote {
                    uuid: p.uuid,
                    name: p.label(),
                    endpoint: p.control_endpoint(),
                    api_endpoint: p.api_endpoint(),
                    free_ram_mb: p.free_ram_mb,
                    backend: p.backend.to_string(),
                    predicted_label: "~?".into(),
                });
            }
        }
    }

    TargetSelectionState {
        model_path,
        model_name,
        candidates,
        selected_idx: 0,
    }
}

async fn dispatch_remote(
    ctx: &HubWorkerCtx,
    endpoint: &str,
    req: ModelLoadRequest,
) -> Result<ModelLoadResponse, ControlPlaneError> {
    let client = reqwest::Client::new();
    if ctx.config.network.security.pairing_enforced() {
        dispatch_load_model_signed(&client, endpoint, &req, &ctx.identity).await
    } else {
        dispatch_load_model(&client, endpoint, &req).await
    }
}

// Continuous: remote-load progress needs distinct display fields; avoid drive-by struct.
#[allow(clippy::too_many_arguments)]
async fn run_remote_load(
    ctx: &HubWorkerCtx,
    evt_tx: &mpsc::Sender<HubEvent>,
    endpoint: &str,
    api_endpoint: &str,
    name: &str,
    backend: &str,
    model_name: &str,
    context_size: usize,
    gpu_layers: u32,
) {
    let _ = evt_tx
        .send(HubEvent::ModelLoadProgress {
            phase: format!("Dispatching remote load to {name}…"),
        })
        .await;

    let req = ModelLoadRequest {
        protocol_version: CONTROL_PLANE_VERSION,
        requester_id: ctx.config.node_uuid().unwrap_or_else(|_| Uuid::new_v4()),
        model_path: model_name.to_string(),
        context_size,
        gpu_layers,
        threads: ctx.config.hardware.acceleration.cpu_threads,
        rpc_workers: Vec::new(),
        tags: Vec::new(),
        target_port: None,
    };

    info!("Dispatching remote model load to {endpoint}: {req:?}");
    match dispatch_remote(ctx, endpoint, req).await {
        Ok(resp) if resp.success => {
            let target_api =
                if !resp.api_endpoint.is_empty() && !resp.api_endpoint.contains("0.0.0.0") {
                    resp.api_endpoint
                } else {
                    api_endpoint.to_string()
                };
            let _ = evt_tx
                .send(HubEvent::RemoteLoadSucceeded {
                    model_name: model_name.to_string(),
                    name: name.to_string(),
                    backend: backend.to_string(),
                    api_endpoint: target_api,
                    notice: format!(
                        "Connected to remote model '{model_name}' running on {name}. Ready for inference."
                    ),
                })
                .await;
        }
        Ok(resp) => {
            let err = resp
                .error_message
                .unwrap_or_else(|| "Unknown error".to_string());
            let _ = evt_tx
                .send(HubEvent::RemoteLoadFailed {
                    message: format!("Remote load failed on {name}: {err}"),
                })
                .await;
        }
        Err(e) => {
            let err_str = e.to_string();
            let hint = if err_str.contains("error sending request") {
                format!(
                    "Remote dispatch failed to {name}: no control-plane listener on {endpoint}. Is nexus/nexusd running on that device?"
                )
            } else {
                format!("Remote dispatch failed to {name}: {e}")
            };
            let _ = evt_tx
                .send(HubEvent::RemoteLoadFailed { message: hint })
                .await;
        }
    }
}

async fn run_unload(
    ctx: &HubWorkerCtx,
    evt_tx: &mpsc::Sender<HubEvent>,
    active_model_name: String,
) {
    let had_supervisor = ctx.supervisor.is_running().await;
    let had_named = active_model_name != "None (Idle)";
    if had_supervisor {
        info!("Unloading active model supervisor...");
        let _ = ctx.supervisor.stop().await;
    }
    if had_supervisor || had_named {
        ctx.discovery.set_active_model("").await;
        ctx.discovery.set_status_flags(StatusFlags(0)).await;
        let _ = evt_tx
            .send(HubEvent::ModelUnloaded {
                unloaded_model: active_model_name,
            })
            .await;
    } else {
        let _ = evt_tx.send(HubEvent::UnloadNoop).await;
    }
}

async fn run_local_load(
    ctx: &HubWorkerCtx,
    evt_tx: &mpsc::Sender<HubEvent>,
    model_path: PathBuf,
    custom_gpu_layers: Option<u32>,
    context_size: usize,
    precomputed_extra_args: Vec<String>,
) {
    let _ = evt_tx
        .send(HubEvent::ModelLoadProgress {
            phase: "Preparing local model load…".to_string(),
        })
        .await;

    if ctx.supervisor.is_running().await {
        let _ = evt_tx
            .send(HubEvent::ModelLoadProgress {
                phase: "Stopping previous supervisor…".to_string(),
            })
            .await;
        let _ = ctx.supervisor.stop().await;
    }

    let model_name = model_path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("unknown")
        .to_string();

    let profile = SystemProfile::probe();
    let threads = ctx.config.hardware.acceleration.cpu_threads;
    let gpu_layers = if let Some(layers) = custom_gpu_layers {
        layers
    } else if ctx.config.hardware.acceleration.prefer_gpu {
        ctx.config.hardware.acceleration.gpu_layers
    } else {
        0
    };

    let mut extra_args = precomputed_extra_args;

    if extra_args.is_empty() && ctx.config.cluster.enable_rpc {
        let host_cap_mb = profile
            .max_allowed_memory_bytes_pct(ctx.config.hardware.safety.max_ram_usage_percent)
            / (1024 * 1024);

        match crate::gguf::GgufMetadata::open(&model_path) {
            Ok(gguf) => {
                if !gguf.has_geometry() {
                    let _ = evt_tx
                        .send(HubEvent::ModelFailed {
                            message:
                                "GGUF geometry missing (block_count); refuse to plan layer split"
                                    .into(),
                        })
                        .await;
                    return;
                }
                let policy = crate::cluster::MemoryPolicy::from_safety(
                    ctx.config.hardware.safety.max_ram_usage_percent,
                    ctx.config.hardware.safety.mmap,
                    ctx.config.hardware.safety.mlock,
                    context_size,
                );
                let plan = crate::cluster::MemoryPlan::from_gguf(
                    &gguf,
                    &profile,
                    &policy,
                    ctx.config.cluster.auto_offload,
                );
                if matches!(
                    plan.verdict,
                    crate::cluster::Verdict::FitsWithOffload | crate::cluster::Verdict::Exceeds
                ) && ctx.config.cluster.auto_offload
                {
                    let _ = evt_tx
                        .send(HubEvent::ModelLoadProgress {
                            phase: "Selecting RPC offload candidate(s)…".to_string(),
                        })
                        .await;
                    let rpc_peers = ctx
                        .discovery
                        .select_rpc_candidates(RpcSelectionPolicy {
                            max_thermal_index: 75,
                            max_allocatable_mb: u64::MAX, // peer free_ram drives budget
                            require_pairing: ctx.config.network.security.require_pairing,
                            protocol_version: CONTROL_PLANE_VERSION,
                        })
                        .await;
                    let workers: Vec<(crate::cluster::NodeBudget, String)> = rpc_peers
                        .iter()
                        .map(|c| {
                            (
                                crate::cluster::NodeBudget::new(
                                    c.peer.uuid,
                                    c.peer.label(),
                                    c.allocatable_mb.max(1),
                                ),
                                c.peer.rpc_endpoint(),
                            )
                        })
                        .collect();
                    let kv = gguf.exact_kv_cache_bytes(context_size);
                    let compute =
                        crate::cluster::estimate_compute_buffer_mb(profile.detected_backend, 512);
                    match crate::cluster::plan_tensor_byte_split(
                        &gguf,
                        context_size,
                        kv,
                        host_cap_mb,
                        &workers,
                        compute,
                    ) {
                        Ok(split) => {
                            extra_args = split.build_llama_args();
                        }
                        Err(e) => {
                            if matches!(plan.verdict, crate::cluster::Verdict::Exceeds) {
                                let _ = evt_tx
                                    .send(HubEvent::ModelFailed {
                                        message: format!("Memory budget error: {e}"),
                                    })
                                    .await;
                                return;
                            }
                        }
                    }
                }
            }
            Err(e) => {
                let _ = evt_tx
                    .send(HubEvent::ModelFailed {
                        message: format!("Failed to parse GGUF for placement: {e}"),
                    })
                    .await;
                return;
            }
        }
    }

    let server_cfg = LlamaServerConfig {
        binary_path: PathBuf::from(&ctx.config.node.llama_server_binary),
        model_path: model_path.clone(),
        host: ctx.config.network.api_host.clone(),
        port: ctx.config.network.api_port,
        gpu_layers,
        threads,
        context_size,
        extra_args,
        use_mmap: ctx.config.hardware.safety.mmap,
        use_mlock: ctx.config.hardware.safety.mlock,
        cpu_threads_batch: ctx.config.hardware.acceleration.cpu_threads_batch,
        fallback_to_cpu: ctx.config.hardware.acceleration.fallback_to_cpu,
        cache_type_k: None,
        cache_type_v: None,
        memory_budget_percent: ctx.config.hardware.safety.max_ram_usage_percent,
        tags: Vec::new(),
    };

    let _ = evt_tx
        .send(HubEvent::ModelLoadProgress {
            phase: format!("Spawning llama-server (-ngl {gpu_layers}, ctx {context_size})…"),
        })
        .await;

    // Progress poller while spawn runs
    let evt_progress = evt_tx.clone();
    let supervisor = ctx.supervisor.clone();
    let progress_task = tokio::spawn(async move {
        loop {
            tokio::time::sleep(Duration::from_millis(250)).await;
            if let Some(state) = supervisor.state().await {
                let phase = match state {
                    SupervisorState::Starting => "Loading weights / waiting for /health…",
                    SupervisorState::Ready => "Supervisor ready",
                    SupervisorState::Failed => "Supervisor failed",
                    SupervisorState::Stopped => "Supervisor stopped",
                };
                if evt_progress
                    .send(HubEvent::ModelLoadProgress {
                        phase: phase.to_string(),
                    })
                    .await
                    .is_err()
                {
                    break;
                }
                if matches!(
                    state,
                    SupervisorState::Ready | SupervisorState::Failed | SupervisorState::Stopped
                ) {
                    break;
                }
            }
        }
    });

    info!("Spawning llama-server for model: {model_path:?}");
    let spawn_result = ctx.supervisor.spawn(server_cfg).await;
    progress_task.abort();

    match spawn_result {
        Ok(()) => {
            ctx.discovery.set_active_model(&model_name).await;
            ctx.discovery.set_status_flags(StatusFlags::READY).await;
            let endpoint = format!("http://127.0.0.1:{}", ctx.config.network.api_port);
            let backend_label = if gpu_layers > 0 {
                format!("Local GPU ({gpu_layers} layers)")
            } else {
                "Local CPU (DotProd / Multi-thread)".to_string()
            };
            let _ = evt_tx
                .send(HubEvent::ModelLoaded {
                    model_name: model_name.clone(),
                    endpoint,
                    backend_label,
                    notice: format!(
                        "Model '{model_name}' loaded successfully and ready for inference."
                    ),
                })
                .await;
        }
        Err(e) => {
            error!("Failed to launch model supervisor: {e}");
            let _ = evt_tx
                .send(HubEvent::ModelFailed {
                    message: format!("Launch failed: {e}"),
                })
                .await;
        }
    }
}

/// Decide whether to enqueue a load or stage a hot-swap intent.
pub async fn request_load_or_hot_swap(
    supervisor: &SupervisorManager,
    path: PathBuf,
    gpu_layers: Option<u32>,
    context_size: usize,
) -> Result<HubCommand, HotSwapIntent> {
    request_load_or_hot_swap_with_args(supervisor, path, gpu_layers, context_size, Vec::new()).await
}

pub async fn request_load_or_hot_swap_with_args(
    supervisor: &SupervisorManager,
    path: PathBuf,
    gpu_layers: Option<u32>,
    context_size: usize,
    extra_args: Vec<String>,
) -> Result<HubCommand, HotSwapIntent> {
    if supervisor.is_running().await {
        Err(HotSwapIntent {
            path,
            gpu_layers,
            context_size,
            extra_args,
        })
    } else {
        Ok(HubCommand::LoadModelLocal {
            path,
            gpu_layers,
            context_size,
            extra_args,
        })
    }
}

/// Helper used by tests — builds the CLI args GPU layer the supervisor would use.
pub fn effective_ngl(custom_gpu_layers: Option<u32>, prefer_gpu: bool, config_gpu: u32) -> u32 {
    if let Some(layers) = custom_gpu_layers {
        layers
    } else if prefer_gpu {
        config_gpu
    } else {
        0
    }
}
