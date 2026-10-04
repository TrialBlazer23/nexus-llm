use clap::Parser;
use nexus::config::NexusConfig;
use nexus::supervisor::{LlamaServerConfig, ProcessSupervisor};
use nexus::sysinfo::SystemProfile;
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
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let subscriber = FmtSubscriber::builder()
        .with_max_level(Level::INFO)
        .finish();
    tracing::subscriber::set_global_default(subscriber)
        .expect("Failed to set tracing subscriber");

    let args = Args::parse();

    info!("=== Nexus-LLM Headless Daemon (nexusd) ===");

    // 1. Load configuration
    let config = if let Some(config_path) = args.config {
        NexusConfig::load_from_path(config_path)?
    } else {
        NexusConfig::load()?
    };
    info!("Node Role: {}, Models Dir: {:?}", config.node.role, config.node.models_dir);

    // 2. System Introspection & Android LMK Profiling
    let profile = SystemProfile::probe();
    info!("Hardware Introspection:");
    info!("  Total RAM: {} MB", profile.total_ram_mb);
    info!("  Available RAM: {} MB", profile.available_ram_mb);
    info!("  Memory Safety Cap (75%): {} MB", profile.max_allowed_memory_bytes() / (1024 * 1024));
    info!("  Detected Backend: {}", profile.detected_backend);
    info!("  Recommended CPU Threads: {}", profile.recommended_threads);

    // 3. Start Autonomous UDP Discovery Service
    let discovery = std::sync::Arc::new(nexus::discovery::DiscoveryService::new(config.clone(), None));
    let _broadcaster_handle = discovery.clone().start_broadcaster();
    let _listener_handle = discovery.clone().start_listener();
    info!("Autonomous discovery daemon active (Node UUID: {})", discovery.node_uuid());

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

        let server_cfg = LlamaServerConfig {
            binary_path: args.binary,
            model_path: model_path.clone(),
            host: args.host,
            port: args.port,
            gpu_layers,
            threads,
            context_size: args.ctx,
        };

        info!("Launching model {:?} with context size {}", model_path, args.ctx);
        match ProcessSupervisor::spawn_with_fallback(server_cfg).await {
            Ok(mut supervisor) => {
                info!("Server supervisor active on backend: {}", supervisor.active_backend());
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
                supervisor.stop().await?;
                info!("Shutdown complete.");
            }
            Err(e) => {
                error!("Failed to launch supervisor: {}", e);
                std::process::exit(1);
            }
        }
    } else {
        info!("No model specified. Daemon idle. Broadcasting beacon. Press Ctrl+C to exit.");
        tokio::signal::ctrl_c().await?;
        info!("Daemon stopped.");
    }

    Ok(())
}
