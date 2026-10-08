use clap::Parser;
use nexus::config::NexusConfig;
use nexus::control_plane_server::{spawn as spawn_control_plane, ControlPlaneContext};
use nexus::discovery::{NodeRole, StatusFlags};
use nexus::gateway::{spawn as spawn_gateway, GatewayContext};
use nexus::registry_runtime::spawn_registry_runtime;
use nexus::supervisor::{LlamaServerConfig, SupervisorManager};
use nexus::sysinfo::{AccelerationBackend, SystemProfile};
use nexus::trust_auth::TrustBootstrap;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tracing::{error, info};

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

    /// Tracing filter level when RUST_LOG is unset
    #[arg(long, default_value = "info")]
    log_level: String,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = Args::parse();
    match nexus::logging::init_file_logging(&args.log_level) {
        Ok(path) => eprintln!("nexusd log file: {}", path.display()),
        Err(e) => eprintln!("Warning: file logging unavailable ({e})"),
    }

    info!("=== Nexus-LLM Headless Daemon (nexusd) ===");

    // 1. Load configuration
    let config = if let Some(config_path) = args.config {
        NexusConfig::load_from_path(config_path)?
    } else {
        NexusConfig::load()?
    };
    let trust = TrustBootstrap::load(config.clone())?;
    info!(
        "Node Role: {}, Models Dir: {:?}",
        config.node.role, config.node.models_dir
    );

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
    let discovery = std::sync::Arc::new(nexus::discovery::DiscoveryService::with_shared_config(
        trust.config.clone(),
        None,
    ));
    let _registry_runtime = spawn_registry_runtime(
        discovery.clone(),
        trust.identity.clone(),
        discovery.node_uuid(),
    );
    let listener_handle = discovery.clone().start_listener();
    let _mdns_handle = discovery.clone().start_mdns();
    info!(
        "Autonomous discovery daemon active (Node UUID: {})",
        discovery.node_uuid()
    );

    // 4. Shared supervisor + control-plane HTTP server (dedicated control_port)
    let supervisor = SupervisorManager::new();
    let control_ctx = Arc::new(
        ControlPlaneContext::new(
            discovery.node_uuid(),
            NodeRole::from_str_role(&config.node.role),
            supervisor.clone(),
            args.host.clone(),
            args.port,
            args.binary.clone(),
            trust.identity.clone(),
            trust.config.clone(),
            trust.config_path.clone(),
        )
        .with_discovery(discovery.clone())
        .with_capabilities(vec!["inference".to_string(), "daemon".to_string()])
        .with_memory_policy(
            config.hardware.safety.mmap,
            config.hardware.safety.max_ram_usage_percent,
        ),
    );
    let control_addr = SocketAddr::from(([0, 0, 0, 0], config.network.control_port));
    let control_handle = spawn_control_plane(control_addr, control_ctx);
    info!(
        "Control-plane HTTP server listening on port {}",
        config.network.control_port
    );

    let _gateway_handle = if config.network.gateway_enabled {
        let gateway_ctx = Arc::new(
            GatewayContext::new(
                supervisor.clone(),
                args.port,
                PathBuf::from(&config.node.models_dir),
                discovery.node_uuid(),
                trust.config.clone(),
            )
            .with_discovery(discovery.clone())
            .with_identity(trust.identity.clone()),
        );
        let gateway_addr = SocketAddr::from(([0, 0, 0, 0], config.network.gateway_port));
        let handle = spawn_gateway(gateway_addr, gateway_ctx);
        info!(
            "Mesh gateway listening on port {}",
            config.network.gateway_port
        );
        Some(handle)
    } else {
        None
    };

    // 5. Optional Model Launch
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
        let host_cap_mb = profile
            .max_allowed_memory_bytes_pct(config.hardware.safety.max_ram_usage_percent)
            / (1024 * 1024);

        let mut extra_args = Vec::new();

        if total_required_mb > host_cap_mb {
            info!(
                "Model memory requirement ({} MB) exceeds host standalone budget ({} MB). Evaluating cluster offload...",
                total_required_mb, host_cap_mb
            );

            let rpc_endpoint = if let Some(ep) = args.rpc {
                Some(ep)
            } else if !args.no_rpc_auto && config.cluster.enable_rpc && config.cluster.auto_offload
            {
                info!("Probing subnet for available RPC worker peer...");
                discovery.send_probe().await;
                tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                discovery
                    .select_rpc_candidate(nexus::discovery::RpcSelectionPolicy {
                        max_thermal_index: 75,
                        max_allocatable_mb: config.cluster.max_rpc_ram_mb,
                        require_pairing: config.network.security.require_pairing,
                        protocol_version: nexus::control_plane::CONTROL_PLANE_VERSION,
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

            let gguf = nexus::gguf::GgufMetadata::open(&model_path)
                .map_err(|e| format!("GGUF parse failed for placement: {e}"))?;
            if !gguf.has_geometry() {
                return Err(
                    "GGUF geometry missing (block_count); refuse to plan layer split".into(),
                );
            }

            let split = nexus::cluster::ClusterCoordinator::plan_from_gguf(
                &gguf,
                args.ctx,
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
            use_mmap: config.hardware.safety.mmap,
            use_mlock: false,
            cpu_threads_batch: 6,
            fallback_to_cpu: true,
            cache_type_k: None,
            cache_type_v: None,
            memory_budget_percent: config.hardware.safety.max_ram_usage_percent,
            tags: Vec::new(),
            slot_save_path: if config.inference.cache.prompt_cache_enabled {
                Some(PathBuf::from(&config.inference.cache.slot_save_path))
            } else {
                None
            },
        };

        info!(
            "Launching model {:?} with context size {}",
            model_path, args.ctx
        );
        match supervisor.spawn(server_cfg).await {
            Ok(()) => {
                let mut status = StatusFlags::READY;
                if profile.detected_backend == AccelerationBackend::Vulkan {
                    status.0 |= StatusFlags::VULKAN_ACTIVE.0;
                }
                discovery.set_status_flags(status).await;
                let broadcaster_handle = discovery.clone().start_broadcaster();
                info!("Listening for termination signal (Ctrl+C)...");
                let mut tick = tokio::time::interval(Duration::from_millis(500));
                loop {
                    tokio::select! {
                        res = tokio::signal::ctrl_c() => {
                            if let Err(e) = res {
                                error!("Error listening for Ctrl+C: {}", e);
                            } else {
                                info!("Termination signal received. Shutting down llama-server...");
                            }
                            break;
                        }
                        _ = tick.tick() => {
                            match supervisor.check_status().await {
                                Ok(Some((exit_status, _))) => {
                                    error!("llama-server process exited unexpectedly: {:?}", exit_status);
                                    break;
                                }
                                Ok(None) => {}
                                Err(e) => {
                                    error!("Failed to poll supervisor: {}", e);
                                    break;
                                }
                            }
                        }
                    }
                }
                discovery.set_status_flags(StatusFlags(0)).await;
                let _ = supervisor.stop().await;
                broadcaster_handle.abort();
                listener_handle.abort();
                control_handle.abort();
                info!("Shutdown complete.");
            }
            Err(e) => {
                listener_handle.abort();
                control_handle.abort();
                error!("Failed to launch supervisor: {}", e);
                std::process::exit(1);
            }
        }
    } else {
        let broadcaster_handle = discovery.clone().start_broadcaster();
        info!(
            "No model specified. Daemon idle with control-plane on port {}. Broadcasting beacon. Press Ctrl+C to exit.",
            config.network.control_port
        );
        tokio::signal::ctrl_c().await?;
        discovery.set_status_flags(StatusFlags(0)).await;
        broadcaster_handle.abort();
        listener_handle.abort();
        control_handle.abort();
        let _ = supervisor.stop().await;
        info!("Daemon stopped.");
    }

    Ok(())
}
