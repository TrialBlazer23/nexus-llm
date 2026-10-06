use clap::Parser;
use nexus::config::NexusConfig;
use nexus::control_plane_server::{spawn_control_plane, ControlPlaneServerState};
use nexus::discovery::NodeRole;
use nexus::supervisor::{LlamaServerConfig, ProcessSupervisor, SupervisorManager};
use nexus::sysinfo::{AccelerationBackend, SystemProfile};
use std::path::PathBuf;
use tracing::{error, info, Level};
use tracing_subscriber::FmtSubscriber;

#[derive(Parser, Debug)]
#[command(name = "nexusd")]
#[command(about = "Nexus-LLM Headless Supervisor Daemon for Node A / Compute Host")]
struct Args {
    /// Path to custom config file
    #[arg(long)]
    config: Option<PathBuf>,

    /// Path to GGUF model file to load
    #[arg(short, long)]
    model: Option<PathBuf>,

    /// API host address to bind
    #[arg(long, default_value = "0.0.0.0")]
    host: String,

    /// API port to listen on
    #[arg(short, long, default_value_t = 8080)]
    port: u16,

    /// Context size (in tokens)
    #[arg(short = 'c', long, default_value_t = 4096)]
    ctx: usize,

    /// Number of CPU threads to utilize
    #[arg(short = 't', long)]
    threads: Option<usize>,

    /// Path to llama-server binary
    #[arg(long, default_value = "llama-server")]
    binary: PathBuf,

    /// Explicit RPC worker endpoint for layer offloading (e.g. 192.168.1.100:50052 or 127.0.0.1:50052)
    #[arg(long)]
    rpc: Option<String>,

    /// Disable automatic RPC discovery when model exceeds Node A's memory budget
    #[arg(long)]
    no_rpc_auto: bool,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let subscriber = FmtSubscriber::builder()
        .with_max_level(Level::INFO)
        .finish();
    tracing::subscriber::set_global_default(subscriber).expect("Failed to set tracing subscriber");

    let args = Args::parse();

    info!("=== Nexus-LLM Headless Daemon (nexusd) ===");

    // 1. Load configuration
    let config = if let Some(config_path) = args.config {
        NexusConfig::load_from_path(config_path)?
    } else {
        NexusConfig::load()?
    };
    info!(
        "Node Role: {}, Models Dir: {:?}",
        config.node.role, config.node.models_dir
    );

    if let Err(e) = std::fs::create_dir_all(&config.node.models_dir) {
        tracing::warn!("Failed to create models_dir {:?}: {}", config.node.models_dir, e);
    }

    // 2. System Introspection & Android LMK Profiling
    let profile = SystemProfile::probe();
    info!("Hardware Introspection:");
    info!("  Total RAM: {} MB", profile.total_ram_mb);
    info!("  Available RAM: {} MB", profile.available_ram_mb);
    info!(
        "  Memory Safety Cap (75%): {} MB",
        profile.max_allowed_memory_bytes() / (1024 * 1024)
    );
    info!("  Detected Backend: {}", profile.detected_backend);
    info!("  Recommended CPU Threads: {}", profile.recommended_threads);

    // 3. Start Autonomous UDP Discovery Service
    let discovery = std::sync::Arc::new(nexus::discovery::DiscoveryService::new(
        config.clone(),
        None,
    ));
    let listener_handle = discovery.clone().start_listener();
    let _mdns_handle = discovery.clone().start_mdns();
    info!(
        "Autonomous discovery daemon active (Node UUID: {})",
        discovery.node_uuid()
    );

    // 3b. Control-plane HTTP (catalog / load / unload) — always on, even with no model.
    let cp_manager = SupervisorManager::new();
    let bind_host = if config.network.api_host == "0.0.0.0" {
        "0.0.0.0"
    } else {
        config.network.api_host.as_str()
    };
    let cp_bind = format!("{}:{}", bind_host, config.network.control_port)
        .parse()
        .unwrap_or_else(|_| ([0, 0, 0, 0], config.network.control_port).into());
    let cp_handle = spawn_control_plane(
        cp_bind,
        ControlPlaneServerState {
            node_id: discovery.node_uuid(),
            role: NodeRole::from_str_role(&config.node.role),
            models_dir: config.node.models_dir.clone(),
            api_host: config.network.api_host.clone(),
            api_port: config.network.api_port,
            manager: cp_manager,
            allocatable_memory_mb: profile.max_allowed_memory_bytes() / (1024 * 1024),
        },
    );
    info!(
        "Control-plane listening on {}:{}",
        bind_host, config.network.control_port
    );

    // 4. Optional Model Launch
    if let Some(model_path) = args.model {
        let model_name = model_path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("unknown")
            .to_string();
        discovery.set_active_model(&model_name).await;

        let threads = args.threads.unwrap_or(profile.recommended_threads);
        let gpu_layers = if config.hardware.acceleration.prefer_gpu {
            config.hardware.acceleration.gpu_layers
        } else {
            0
        };

        // Determine if model exceeds standalone memory budget and requires cluster layer offloading
        let model_metadata = tokio::fs::metadata(&model_path).await?;
        let model_size_bytes = model_metadata.len();
        let kv_bytes = SystemProfile::estimate_kv_cache_bytes(args.ctx);
        let total_required_mb = (model_size_bytes + kv_bytes) / (1024 * 1024);
        let host_cap_mb = profile.max_allowed_memory_bytes() / (1024 * 1024);

        let mut extra_args = Vec::new();

        if total_required_mb > host_cap_mb {
            info!(
                "Model memory requirement ({} MB) exceeds host standalone budget ({} MB). Evaluating cluster offload...",
                total_required_mb, host_cap_mb
            );

            let rpc_endpoint = if let Some(ep) = args.rpc {
                Some(ep)
            } else if !args.no_rpc_auto && config.cluster.auto_offload {
                info!("Probing subnet for available RPC worker peer...");
                discovery.send_probe().await;
                tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                discovery
                    .select_rpc_candidate(nexus::discovery::RpcSelectionPolicy {
                        max_thermal_index: 75,
                        max_allocatable_mb: config.cluster.max_rpc_ram_mb,
                        require_pairing: config.network.security.require_pairing,
                    })
                    .await
                    .map(|candidate| candidate.peer.rpc_endpoint())
            } else {
                None
            };

            let remote_ram = if rpc_endpoint.is_some() {
                Some(config.cluster.max_rpc_ram_mb)
            } else {
                None
            };

            let budget = nexus::cluster::ClusterCoordinator::calculate_budget(
                profile.available_ram_mb,
                remote_ram,
            );

            let total_layers = if let Ok(gguf) = nexus::gguf::GgufMetadata::open(&model_path) {
                gguf.block_count.unwrap_or(32) as u32
            } else {
                32
            };

            let split = nexus::cluster::ClusterCoordinator::plan_layer_split(
                model_size_bytes,
                kv_bytes,
                total_layers,
                &budget,
                rpc_endpoint.as_deref(),
            )?;

            info!(
                "Cluster Layer Pipelining: {} total layers -> {} Host layers (Vulkan), {} Remote layers (RPC endpoint: {:?}, split: {:?})",
                split.total_layers,
                split.host_layers,
                split.remote_layers,
                split.remote_endpoint,
                split.tensor_split
            );

            extra_args = split.build_llama_args();
        }

        let server_cfg = LlamaServerConfig {
            binary_path: args.binary,
            model_path: model_path.clone(),
            host: args.host,
            port: args.port,
            gpu_layers,
            threads,
            context_size: args.ctx,
            extra_args,
        };

        info!(
            "Launching model {:?} with context size {}",
            model_path, args.ctx
        );
        match ProcessSupervisor::spawn_with_fallback(server_cfg).await {
            Ok(mut supervisor) => {
                info!(
                    "Server supervisor active on backend: {}",
                    supervisor.active_backend()
                );
                let mut status = nexus::discovery::StatusFlags::READY;
                if supervisor.active_backend() == AccelerationBackend::Vulkan {
                    status.0 |= nexus::discovery::StatusFlags::VULKAN_ACTIVE.0;
                }
                discovery.set_status_flags(status).await;
                let broadcaster_handle = discovery.clone().start_broadcaster();
                info!("Listening for termination signal (Ctrl+C)...");
                tokio::select! {
                    res = tokio::signal::ctrl_c() => {
                        if let Err(e) = res {
                            error!("Error listening for Ctrl+C: {}", e);
                        } else {
                            info!("Termination signal received. Shutting down llama-server...");
                        }
                    }
                    exit_res = supervisor.wait() => {
                        error!("llama-server process exited unexpectedly: {:?}", exit_res);
                    }
                }
                discovery
                    .set_status_flags(nexus::discovery::StatusFlags(0))
                    .await;
                supervisor.stop().await?;
                broadcaster_handle.abort();
                listener_handle.abort();
                cp_handle.abort();
                info!("Shutdown complete.");
            }
            Err(e) => {
                listener_handle.abort();
                cp_handle.abort();
                error!("Failed to launch supervisor: {}", e);
                std::process::exit(1);
            }
        }
    } else {
        let broadcaster_handle = discovery.clone().start_broadcaster();
        info!("No model specified. Daemon idle. Broadcasting beacon. Press Ctrl+C to exit.");
        tokio::signal::ctrl_c().await?;
        discovery
            .set_status_flags(nexus::discovery::StatusFlags(0))
            .await;
        broadcaster_handle.abort();
        listener_handle.abort();
        cp_handle.abort();
        info!("Daemon stopped.");
    }

    Ok(())
}
