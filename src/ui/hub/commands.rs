//! Hub command/event bus (Phase 8).
//!
//! Long-running work (model load/unload, peer refresh, target selection probe,
//! remote dispatch) runs off the TUI event loop. The loop only draws, mutates
//! state from [`HubEvent`], and enqueues [`HubCommand`].

use super::{HotSwapIntent, TargetExecutionNode, TargetSelectionState};
use crate::config::NexusConfig;
use crate::control_plane::ControlPlaneError;
use crate::control_plane::{
    dispatch_load_model, dispatch_load_model_signed, ModelLoadRequest, ModelLoadResponse,
    CONTROL_PLANE_VERSION,
};
use crate::discovery::{DiscoveryService, RpcSelectionPolicy, StatusFlags};
use crate::node_identity::NodeIdentity;
use crate::supervisor::{LlamaServerConfig, SupervisorManager, SupervisorState};
use crate::sysinfo::SystemProfile;
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
    /// Reserved for Phase 10 — WAN download.
    StartDownload {
        url: String,
    },
    /// Reserved for Phase 10 — LAN blob transfer.
    TransferModel {
        peer_endpoint: String,
        digest: String,
    },
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
                } => {
                    run_local_load(&ctx, &evt_tx, path, gpu_layers, context_size).await;
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
                HubCommand::StartDownload { .. } | HubCommand::TransferModel { .. } => {
                    let _ = evt_tx
                        .send(HubEvent::Status {
                            message: "Download/transfer reserved for Phase 10".to_string(),
                            color: ratatui::style::Color::Yellow,
                        })
                        .await;
                }
            }
        }
    })
}

pub(crate) async fn build_target_selection(
    ctx: &HubWorkerCtx,
    model_path: PathBuf,
) -> TargetSelectionState {
    let model_name = model_path
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("unknown")
        .to_string();

    let profile = SystemProfile::probe();
    let local_cap_mb = profile.max_allowed_memory_bytes() / (1024 * 1024);
    let gpu_layers = if ctx.config.hardware.acceleration.prefer_gpu {
        ctx.config.hardware.acceleration.gpu_layers
    } else {
        99
    };

    let mut candidates = vec![
        TargetExecutionNode::Local {
            allocatable_mb: local_cap_mb,
            backend: profile.detected_backend.to_string(),
            gpu_layers,
        },
        TargetExecutionNode::LocalCpu {
            allocatable_mb: local_cap_mb,
            backend: "CPU Fallback (DotProd / Multi-thread)".to_string(),
            threads: profile.recommended_threads,
        },
    ];

    let peers = ctx.discovery.get_active_peers().await;
    for p in peers {
        if p.status.is_ready() || p.role.is_host() || p.is_rpc_ready() {
            candidates.push(TargetExecutionNode::Remote {
                uuid: p.uuid,
                name: p.label(),
                endpoint: p.control_endpoint(),
                api_endpoint: p.api_endpoint(),
                free_ram_mb: p.free_ram_mb,
                backend: p.backend.to_string(),
            });
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

    let model_size_bytes = std::fs::metadata(&model_path).map(|m| m.len()).unwrap_or(0);
    let kv_bytes = SystemProfile::estimate_kv_cache_bytes(context_size);
    let total_required_mb = (model_size_bytes + kv_bytes) / (1024 * 1024);
    let host_cap_mb = profile
        .max_allowed_memory_bytes_pct(ctx.config.hardware.safety.max_ram_usage_percent)
        / (1024 * 1024);

    let mut extra_args = Vec::new();

    if total_required_mb > host_cap_mb && ctx.config.cluster.enable_rpc {
        let _ = evt_tx
            .send(HubEvent::ModelLoadProgress {
                phase: "Selecting RPC offload candidate…".to_string(),
            })
            .await;
        let rpc_peer = if ctx.config.cluster.auto_offload {
            ctx.discovery
                .select_rpc_candidate(RpcSelectionPolicy {
                    max_thermal_index: 75,
                    max_allocatable_mb: ctx.config.cluster.max_rpc_ram_mb,
                    require_pairing: ctx.config.network.security.require_pairing,
                    protocol_version: CONTROL_PLANE_VERSION,
                })
                .await
        } else {
            None
        };
        let rpc_endpoint = rpc_peer.map(|candidate| candidate.peer.rpc_endpoint());
        let remote_ram = if rpc_endpoint.is_some() {
            Some(ctx.config.cluster.max_rpc_ram_mb)
        } else {
            None
        };
        let budget = crate::cluster::ClusterCoordinator::calculate_budget(
            profile.available_ram_mb,
            remote_ram,
        );

        let total_layers = if let Ok(gguf) = crate::gguf::GgufMetadata::open(&model_path) {
            gguf.block_count.unwrap_or(32) as u32
        } else {
            32
        };

        match crate::cluster::ClusterCoordinator::plan_layer_split(
            model_size_bytes,
            kv_bytes,
            total_layers,
            &budget,
            rpc_endpoint.as_deref(),
        ) {
            Ok(split) => {
                extra_args = split.build_llama_args();
            }
            Err(e) => {
                let _ = evt_tx
                    .send(HubEvent::ModelFailed {
                        message: format!("Memory budget error: {e}"),
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
        memory_budget_percent: ctx.config.hardware.safety.max_ram_usage_percent,
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
    if supervisor.is_running().await {
        Err(HotSwapIntent {
            path,
            gpu_layers,
            context_size,
        })
    } else {
        Ok(HubCommand::LoadModelLocal {
            path,
            gpu_layers,
            context_size,
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
