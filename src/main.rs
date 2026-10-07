use clap::{Parser, Subcommand};
use futures_util::StreamExt;
use nexus::client::{ChatCompletionRequest, ChatMessage, NexusClient};
use nexus::config::NexusConfig;
use nexus::control_plane::{dispatch_pair, PairRequest, CONTROL_PLANE_VERSION};
use nexus::control_plane_server::{spawn as spawn_control_plane, ControlPlaneContext};
use nexus::discovery::{DiscoveryService, NodeRole};
use nexus::downloader::ModelDownloader;
use nexus::gateway::{spawn as spawn_gateway, GatewayContext};
use nexus::gguf::GgufMetadata;
use nexus::preset::Preset;
use nexus::registry_runtime::spawn_registry_runtime;
use nexus::supervisor::{LlamaServerConfig, SupervisorManager};
use nexus::sysinfo::SystemProfile;
use nexus::trust_auth::TrustBootstrap;
use nexus::tunnel::{AdbTunnelSupervisor, TransportMode};
use nexus::ui::chat::{run_chat_tui, ChatApp};
use nexus::ui::dashboard::run_dashboard_tui;
use nexus::ui::hub::{run_hub_tui, HubApp};
use nexus::ui::models::scan_models_dir;
use std::io::Write;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

#[derive(Parser, Debug)]
#[command(name = "nexus")]
#[command(about = "Nexus-LLM CLI Orchestrator & Client")]
struct Cli {
    /// Tracing filter level when RUST_LOG is unset (file logs under ~/.nexus/logs/)
    #[arg(long, global = true, default_value = "info")]
    log_level: String,

    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Subcommand, Debug)]
enum Commands {
    /// Run the local inference host and advertise it on the network
    Host {
        /// Path to GGUF model file to load
        #[arg(short, long)]
        model: PathBuf,

        /// API host address to bind
        #[arg(long)]
        host: Option<String>,

        /// API port to listen on
        #[arg(short, long)]
        port: Option<u16>,

        /// Context size in tokens
        #[arg(short = 'c', long, default_value_t = 4096)]
        ctx: usize,

        /// Path to llama-server binary
        #[arg(long, default_value = "llama-server")]
        binary: PathBuf,
    },

    /// Probe hardware and display memory and acceleration profile
    Info,

    /// Check if a model file and context size can safely load within Android LMK limits
    Check {
        /// Path to the GGUF model file
        #[arg(short, long)]
        model: PathBuf,

        /// Context size in tokens (default: 4096)
        #[arg(short = 'c', long, default_value_t = 4096)]
        ctx: usize,
    },

    /// Inspect a GGUF model file header and metadata without loading weights
    Inspect {
        /// Path to the GGUF model file
        #[arg(short, long)]
        model: PathBuf,

        /// Optional context tokens for exact KV cache computation (default: 4096)
        #[arg(short = 'c', long, default_value_t = 4096)]
        ctx: usize,
    },

    /// Download a remote model file with HTTP Range resume and SHA-256 verification
    Download {
        /// Download URL
        url: String,

        /// Destination file path
        #[arg(short, long)]
        output: PathBuf,

        /// Optional expected SHA-256 hash
        #[arg(short, long)]
        sha256: Option<String>,
    },

    /// Pair with a remote node using its 6-digit control-plane pairing code
    Pair {
        /// Control-plane base URL (e.g. http://192.168.1.50:9998)
        #[arg(long)]
        host: String,
        /// Six-digit pairing code shown on the target device
        #[arg(long)]
        code: String,
    },

    /// Display current or generated configuration
    Config {
        /// Path to custom config file
        #[arg(short, long)]
        file: Option<PathBuf>,
    },

    /// Listen for discovery beacons and display active cluster nodes
    Discover {
        /// Seconds to listen for beacons
        #[arg(short, long, default_value_t = 3)]
        timeout: u64,
    },

    /// Open full-screen cluster performance monitor dashboard
    Dashboard,

    /// Browse local models directory and inspect GGUF architecture parameters
    Models {
        /// Optional path to custom models directory
        #[arg(short, long)]
        dir: Option<PathBuf>,
    },

    /// Run as an RPC compute worker to receive offloaded model layers
    #[command(name = "worker", alias = "rpc")]
    Worker {
        /// TCP port to bind rpc-server (default: 50052)
        #[arg(short, long, default_value_t = 50052)]
        port: u16,

        /// Maximum RAM allocation in Megabytes (default 1800; raise freely on large hosts)
        #[arg(short, long, default_value_t = 1800)]
        mem: u64,

        /// Path to llama.cpp rpc-server binary (defaults to config `node.rpc_server_binary`)
        #[arg(long)]
        binary: Option<PathBuf>,
    },

    /// Inspect or manage ADB USB port forwarding and reverse tunnels to phone
    Tunnel {
        /// Action to perform: status, setup, teardown
        #[arg(default_value = "status")]
        action: String,

        /// API port (default: 8080)
        #[arg(long, default_value_t = 8080)]
        api_port: u16,

        /// RPC port (default: 50052)
        #[arg(long, default_value_t = 50052)]
        rpc_port: u16,
    },

    /// Connect to compute host and execute chat generation (interactive TUI or CLI stream)
    Client {
        /// Optional host endpoint (e.g. http://192.168.1.100:8080). If omitted, auto-discovers host.
        #[arg(long)]
        host: Option<String>,

        /// Discovered node UUID or configured node name
        #[arg(long)]
        node: Option<String>,

        /// Preferred transport mode: auto (USB priority with Wi-Fi fallback), usb (force ADB), wifi (subnet only)
        #[arg(short = 't', long, default_value = "auto")]
        transport: String,

        /// User prompt to send (if omitted, starts interactive split-screen TUI)
        #[arg(short, long)]
        prompt: Option<String>,

        /// Model name to specify in request
        #[arg(short, long, default_value = "default")]
        model: String,

        /// Optional persona preset name (e.g., "coder", "general")
        #[arg(long)]
        preset: Option<String>,
    },

    /// Diagnose mesh / inference preconditions (binaries, ports, config, profile)
    Doctor,

    /// Measure prompt/gen throughput and persist to ~/.nexus/bench.json (Phase 12 §5.5)
    Bench {
        /// OpenAI-compatible base URL (gateway, api_port, or fake llama)
        #[arg(long)]
        endpoint: String,

        /// Model id as advertised by `/v1/models`
        #[arg(short, long)]
        model: String,

        /// Context-size key recorded with the sample (does not change server -c)
        #[arg(short = 'c', long, default_value_t = 2048)]
        ctx: usize,

        /// Number of timed streaming runs
        #[arg(short = 'n', long, default_value_t = 3)]
        runs: u32,

        /// Prompt text for the benchmark completion
        #[arg(long, default_value = "Write a short paragraph about mesh networking.")]
        prompt: String,

        /// Max tokens to generate per run
        #[arg(long, default_value_t = 64)]
        max_tokens: usize,

        /// Node id key in the store (default: local)
        #[arg(long, default_value = "local")]
        node: String,

        /// Backend key: vulkan | arm-dotprod | x86-sse41 | cpu (default: probed)
        #[arg(long)]
        backend: Option<String>,
    },
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let cli = Cli::parse();
    let log_path = match nexus::logging::init_file_logging(&cli.log_level) {
        Ok(path) => {
            eprintln!("Nexus log file: {}", path.display());
            Some(path)
        }
        Err(e) => {
            eprintln!("Warning: file logging unavailable ({e}); continuing without subscriber");
            None
        }
    };

    let command = match cli.command {
        Some(cmd) => cmd,
        None => {
            let config = NexusConfig::load()?;
            let trust = TrustBootstrap::load(config.clone())?;
            let discovery = Arc::new(DiscoveryService::with_shared_config(
                trust.config.clone(),
                None,
            ));
            let _broadcaster = discovery.clone().start_broadcaster();
            let _listener = discovery.clone().start_listener();
            let _mdns = discovery.clone().start_mdns();
            discovery.send_probe().await;

            // Order: default_host → PreferAdbTunnel USB → local mesh gateway
            // (gateway resolves model→holder) → discovery → localhost api_port.
            // Clone host-resolution inputs under the lock, then await without holding it.
            let (default_host, prefer_adb, api_port, rpc_port, gateway_enabled, gateway_port) = {
                let cfg = trust.config.read().unwrap();
                (
                    cfg.network.default_host.clone(),
                    cfg.cluster.prefer_adb_tunnel,
                    cfg.network.api_port,
                    cfg.cluster.rpc_port,
                    cfg.network.gateway_enabled,
                    cfg.network.gateway_port,
                )
            };
            let host = if let Some(dh) = default_host {
                dh
            } else {
                let usb_ep = if prefer_adb {
                    AdbTunnelSupervisor::resolve_transport_endpoint(
                        TransportMode::Auto,
                        api_port,
                        rpc_port,
                    )
                    .0
                } else {
                    None
                };
                if let Some(usb) = usb_ep {
                    usb
                } else if gateway_enabled {
                    format!("http://127.0.0.1:{gateway_port}")
                } else {
                    match NexusClient::resolve_from_discovery(&discovery, Duration::from_secs(3))
                        .await
                    {
                        Ok(client) => client.endpoint().to_string(),
                        Err(_) => format!("http://127.0.0.1:{api_port}"),
                    }
                }
            };

            let client = NexusClient::new(host);
            let hub = HubApp::new(
                config,
                client,
                discovery,
                trust.identity,
                trust.config,
                trust.config_path,
            );
            let result = run_hub_tui(hub).await;
            if let Some(path) = &log_path {
                eprintln!("Nexus log file: {}", path.display());
            }
            return result;
        }
    };

    match command {
        Commands::Pair { host, code } => {
            let mut config = NexusConfig::load()?;
            let trust = TrustBootstrap::load(config.clone())?;
            let requester_id = config.node_uuid()?;
            let pair_req = PairRequest {
                protocol_version: CONTROL_PLANE_VERSION,
                requester_id,
                requester_public_key: trust.identity.public_key_hex(),
                pairing_code: code,
            };
            let client = reqwest::Client::new();
            let resp = dispatch_pair(&client, &host, &pair_req, &trust.identity).await?;
            if !resp.success {
                eprintln!("Pairing failed: {}", resp.message);
                std::process::exit(1);
            }
            config
                .network
                .security
                .record_pair(resp.node_id, resp.public_key);
            config.save()?;
            println!("Paired with node {} ({})", resp.node_id, host);
        }

        Commands::Host {
            model,
            host,
            port,
            ctx,
            binary,
        } => {
            let config = NexusConfig::load()?;
            let trust = TrustBootstrap::load(config.clone())?;
            let profile = SystemProfile::probe();
            let discovery = Arc::new(DiscoveryService::with_shared_config(
                trust.config.clone(),
                None,
            ));
            let _registry = spawn_registry_runtime(
                discovery.clone(),
                trust.identity.clone(),
                discovery.node_uuid(),
            );
            let _broadcaster = discovery.clone().start_broadcaster();
            let _listener = discovery.clone().start_listener();
            let _mdns = discovery.clone().start_mdns();

            let api_host = host.unwrap_or_else(|| config.network.api_host.clone());
            let api_port = port.unwrap_or(config.network.api_port);
            let supervisor = SupervisorManager::new();
            let control_ctx = Arc::new(
                ControlPlaneContext::new(
                    discovery.node_uuid(),
                    NodeRole::from_str_role(&config.node.role),
                    supervisor.clone(),
                    api_host.clone(),
                    api_port,
                    binary.clone(),
                    trust.identity.clone(),
                    trust.config.clone(),
                    trust.config_path.clone(),
                )
                .with_discovery(discovery.clone())
                .with_capabilities(vec!["inference".to_string(), "host".to_string()])
                .with_memory_policy(
                    config.hardware.safety.mmap,
                    config.hardware.safety.max_ram_usage_percent,
                ),
            );
            let control_handle = spawn_control_plane(
                SocketAddr::from(([0, 0, 0, 0], config.network.control_port)),
                control_ctx,
            );
            println!(
                "Control-plane listening on port {}",
                config.network.control_port
            );

            let _gateway_handle = if config.network.gateway_enabled {
                let gateway_ctx = Arc::new(
                    GatewayContext::new(
                        supervisor.clone(),
                        api_port,
                        PathBuf::from(&config.node.models_dir),
                        discovery.node_uuid(),
                        trust.config.clone(),
                    )
                    .with_discovery(discovery.clone()),
                );
                let handle = spawn_gateway(
                    SocketAddr::from(([0, 0, 0, 0], config.network.gateway_port)),
                    gateway_ctx,
                );
                println!(
                    "Mesh gateway listening on port {}",
                    config.network.gateway_port
                );
                Some(handle)
            } else {
                None
            };

            let server_cfg = LlamaServerConfig {
                binary_path: binary,
                model_path: model,
                host: api_host,
                port: api_port,
                gpu_layers: if config.hardware.acceleration.prefer_gpu {
                    config.hardware.acceleration.gpu_layers
                } else {
                    0
                },
                threads: profile.recommended_threads,
                context_size: ctx,
                extra_args: Vec::new(),
                use_mmap: config.hardware.safety.mmap,
                use_mlock: false,
                cpu_threads_batch: 6,
                fallback_to_cpu: true,
                cache_type_k: None,
                cache_type_v: None,
                memory_budget_percent: config.hardware.safety.max_ram_usage_percent,
                tags: Vec::new(),
            };
            supervisor.spawn(server_cfg).await?;
            let mut tick = tokio::time::interval(Duration::from_millis(500));
            loop {
                tokio::select! {
                    _ = tokio::signal::ctrl_c() => { break; }
                    _ = tick.tick() => {
                        if let Ok(Some(_)) = supervisor.check_status().await {
                            break;
                        }
                    }
                }
            }
            let _ = supervisor.stop().await;
            control_handle.abort();
        }

        Commands::Info => {
            let profile = SystemProfile::probe();
            let max_allowed_mb = profile.max_allowed_memory_bytes() / (1024 * 1024);

            println!("=== Nexus-LLM System Profile ===");
            println!("Architecture Target:   {}", std::env::consts::ARCH);
            println!("Total Memory:          {} MB", profile.total_ram_mb);
            println!("Available Memory:      {} MB", profile.available_ram_mb);
            println!("LMK Safety Cap (75%):  {} MB", max_allowed_mb);
            println!("Acceleration Backend:  {}", profile.detected_backend);
            println!(
                "Vulkan Runtime:        {}",
                if SystemProfile::probe_vulkan() {
                    "Detected"
                } else {
                    "Not Found"
                }
            );
            println!("Recommended Threads:   {}", profile.recommended_threads);
        }

        Commands::Check { model, ctx } => {
            let profile = SystemProfile::probe();
            let metadata = std::fs::metadata(&model)?;
            let file_size_bytes = metadata.len();
            let file_size_mb = file_size_bytes / (1024 * 1024);
            let kv_cache_bytes = SystemProfile::estimate_kv_cache_bytes(ctx);
            let kv_cache_mb = kv_cache_bytes / (1024 * 1024);
            let total_required_mb = (file_size_bytes + kv_cache_bytes) / (1024 * 1024);
            let max_allowed_mb = profile.max_allowed_memory_bytes() / (1024 * 1024);

            println!("=== Android LMK Memory Safety Guard ===");
            println!("Model File:            {:?}", model);
            println!("Model Size:            {} MB", file_size_mb);
            println!("Context Tokens:        {}", ctx);
            println!("Estimated KV Cache:    {} MB", kv_cache_mb);
            println!("Total Memory Required: {} MB", total_required_mb);
            println!("Available System RAM:  {} MB", profile.available_ram_mb);
            println!("75% Memory Ceiling:    {} MB", max_allowed_mb);

            if profile.can_safely_load(file_size_bytes, ctx) {
                let headroom = max_allowed_mb.saturating_sub(total_required_mb);
                println!(
                    "Result:                PASSED [OK] (Headroom: {} MB)",
                    headroom
                );
            } else {
                let deficit = total_required_mb.saturating_sub(max_allowed_mb);
                eprintln!(
                    "Result:                REJECTED [FAIL] (Exceeds ceiling by {} MB)",
                    deficit
                );
                std::process::exit(1);
            }
        }

        Commands::Inspect { model, ctx } => {
            println!("=== Zero-Copy GGUF Inspection ===");
            println!("File Path:             {:?}", model);

            let gguf = GgufMetadata::open(&model)?;
            let exact_kv_bytes = gguf.exact_kv_cache_bytes(ctx);
            let exact_kv_mb = exact_kv_bytes / (1024 * 1024);
            let file_mb = gguf.file_size_bytes / (1024 * 1024);

            println!("Format Version:        GGUF v{}", gguf.version);
            println!("Tensor Count:          {}", gguf.tensor_count);
            println!("Metadata KV Pairs:     {}", gguf.kv_count);
            println!(
                "Architecture:          {}",
                gguf.architecture.as_deref().unwrap_or("unknown")
            );
            println!(
                "Model Name:            {}",
                gguf.model_name.as_deref().unwrap_or("unnamed")
            );
            println!(
                "Max Context Length:    {}",
                gguf.context_length
                    .map(|c| c.to_string())
                    .unwrap_or_else(|| "unknown".to_string())
            );
            println!(
                "Transformer Layers:    {}",
                gguf.block_count
                    .map(|b| b.to_string())
                    .unwrap_or_else(|| "unknown".to_string())
            );
            println!(
                "Attention Heads:       {}",
                gguf.head_count
                    .map(|h| h.to_string())
                    .unwrap_or_else(|| "unknown".to_string())
            );
            println!(
                "KV Attention Heads:    {}",
                gguf.head_count_kv
                    .map(|h| h.to_string())
                    .unwrap_or_else(|| "unknown".to_string())
            );
            println!(
                "Embedding Length:      {}",
                gguf.embedding_length
                    .map(|e| e.to_string())
                    .unwrap_or_else(|| "unknown".to_string())
            );
            println!("Model File Size:       {} MB", file_mb);
            println!("Exact KV Cache ({} t): {} MB", ctx, exact_kv_mb);

            let profile = SystemProfile::probe();
            let safe = profile.can_safely_load_gguf(&gguf, ctx);
            println!(
                "Android LMK Guard:     {}",
                if safe {
                    "PASSED [OK]"
                } else {
                    "BLOCKED [INSUFFICIENT RAM]"
                }
            );
        }

        Commands::Download {
            url,
            output,
            sha256,
        } => {
            println!("=== Resumable Model Downloader ===");
            println!("Source URL:            {}", url);
            println!("Destination Path:      {:?}", output);
            if let Some(hash) = &sha256 {
                println!("Expected SHA-256:      {}", hash);
            }

            let downloader = ModelDownloader::new();
            downloader
                .download(&url, &output, sha256.as_deref(), |prog| {
                    let speed_mb = prog.speed_bytes_per_sec / (1024.0 * 1024.0);
                    let down_mb = prog.downloaded_bytes / (1024 * 1024);
                    if let Some(pct) = prog.percent {
                        let total_mb = prog.total_bytes.unwrap_or(0) / (1024 * 1024);
                        print!(
                            "\rDownloading: {:.1}% ({}/{} MB) - {:.2} MB/s   ",
                            pct, down_mb, total_mb, speed_mb
                        );
                    } else {
                        print!("\rDownloading: {} MB - {:.2} MB/s   ", down_mb, speed_mb);
                    }
                    let _ = std::io::stdout().flush();
                })
                .await?;

            println!("\nDownload finished.");
        }

        Commands::Config { file } => {
            let config = if let Some(path) = file {
                NexusConfig::load_from_path(path)?
            } else {
                NexusConfig::load()?
            };
            println!("{}", toml::to_string_pretty(&config)?);
        }

        Commands::Discover { timeout } => {
            let config = NexusConfig::load().unwrap_or_default();
            let discovery = Arc::new(DiscoveryService::new(config, None));
            let _listener = discovery.clone().start_listener();
            let _mdns = discovery.clone().start_mdns();
            discovery.send_probe().await;

            println!(
                "Listening for discovery beacons on UDP 9999 & mDNS for {}s...",
                timeout
            );
            tokio::time::sleep(Duration::from_secs(timeout)).await;

            let peers = discovery.get_active_peers().await;
            if peers.is_empty() {
                println!("No active cluster nodes discovered.");
            } else {
                println!("\nDiscovered Cluster Nodes ({}):", peers.len());
                println!(
                    "{:<38} {:<22} {:<10} {:<10} {:<10}",
                    "Node UUID", "Endpoint", "Role", "Free RAM", "Backend"
                );
                println!("{}", "-".repeat(95));
                for p in peers {
                    let role = if p.role.is_host() { "Host" } else { "Client" };
                    println!(
                        "{:<38} {:<22} {:<10} {:<10} {:<10}",
                        p.uuid.to_string(),
                        p.api_endpoint(),
                        role,
                        format!("{} MB", p.free_ram_mb),
                        p.backend.to_string()
                    );
                }
            }
        }

        Commands::Dashboard => {
            let config = NexusConfig::load().unwrap_or_default();
            let discovery = Arc::new(DiscoveryService::new(config, None));
            let _broadcaster = discovery.clone().start_broadcaster();
            let _listener = discovery.clone().start_listener();
            let _mdns = discovery.clone().start_mdns();
            run_dashboard_tui(discovery).await?;
        }

        Commands::Models { dir } => {
            let config = NexusConfig::load().unwrap_or_default();
            let models_dir = dir.unwrap_or(config.node.models_dir);
            println!("Scanning models directory: {:?}", models_dir);

            let models = scan_models_dir(&models_dir);
            if models.is_empty() {
                println!("No .gguf models found in {:?}", models_dir);
            } else {
                println!("\nDiscovered Local Models ({}):", models.len());
                println!(
                    "{:<30} {:<12} {:<10} {:<8} {:<12} {:<10}",
                    "Filename", "Size", "Arch", "Context", "KV (4k)", "LMK Guard"
                );
                println!("{}", "-".repeat(88));
                for m in models {
                    let lmk_status = if m.lmk_compatible {
                        "Compatible"
                    } else {
                        "Exceeds RAM"
                    };
                    println!(
                        "{:<30} {:<12} {:<10} {:<8} {:<12} {:<10}",
                        m.filename,
                        format!("{} MB", m.size_mb),
                        m.architecture,
                        m.context_length,
                        format!("{} MB", m.exact_kv_mb),
                        lmk_status
                    );
                }
            }
        }

        Commands::Worker { port, mem, binary } => {
            println!("=== Nexus-LLM RPC Compute Worker ===");
            let profile = SystemProfile::probe();
            let safe = profile.max_allowed_memory_bytes() / (1024 * 1024);
            if mem > safe {
                eprintln!(
                    "WARNING: RAM allocation ({} MB) exceeds this node's LMK-safe budget ({} MB).",
                    mem, safe
                );
            }
            println!(
                "Binding rpc-server on port {} with max memory {} MB...",
                port, mem
            );

            let mut config = NexusConfig::load().unwrap_or_default();
            config.node.role = "client".to_string();
            let control_port = config.network.control_port;
            let api_host = config.network.api_host.clone();
            let api_port = config.network.api_port;
            let llama_binary = PathBuf::from(&config.node.llama_server_binary);
            let rpc_binary =
                binary.unwrap_or_else(|| PathBuf::from(&config.node.rpc_server_binary));
            let use_mmap = config.hardware.safety.mmap;
            let memory_budget_percent = config.hardware.safety.max_ram_usage_percent;

            // Start discovery service advertising RPC worker readiness
            let trust = TrustBootstrap::load(config.clone())?;
            let discovery = Arc::new(DiscoveryService::with_shared_config(
                trust.config.clone(),
                None,
            ));
            discovery.set_rpc_status(true, port).await;
            let _registry = spawn_registry_runtime(
                discovery.clone(),
                trust.identity.clone(),
                discovery.node_uuid(),
            );
            let _broadcaster = discovery.clone().start_broadcaster();
            let _listener = discovery.clone().start_listener();
            let _mdns = discovery.clone().start_mdns();

            // Workers expose a control plane for state probes; model/load may fail
            // without a local llama-server binary, which is intentional.
            let supervisor = SupervisorManager::new();
            let control_ctx = Arc::new(
                ControlPlaneContext::new(
                    discovery.node_uuid(),
                    NodeRole::CLIENT,
                    supervisor.clone(),
                    api_host,
                    api_port,
                    llama_binary,
                    trust.identity.clone(),
                    trust.config.clone(),
                    trust.config_path.clone(),
                )
                .with_discovery(discovery.clone())
                .with_rpc_ready(true)
                .with_capabilities(vec!["rpc".to_string(), "worker".to_string()])
                .with_memory_policy(use_mmap, memory_budget_percent),
            );
            let control_handle =
                spawn_control_plane(SocketAddr::from(([0, 0, 0, 0], control_port)), control_ctx);
            println!(
                "Broadcasting RPC worker beacon on UDP 9999 (Port: {}, Status: RPC_READY)",
                port
            );
            println!("Control-plane listening on port {}", control_port);

            let child = tokio::process::Command::new(&rpc_binary)
                .args([
                    "-H",
                    "0.0.0.0",
                    "-p",
                    &port.to_string(),
                    "-m",
                    &mem.to_string(),
                ])
                .spawn();

            match child {
                Ok(mut proc) => {
                    println!("rpc-server active (PID: {:?}). Waiting for layer offload connections from Node A...", proc.id());
                    println!("Press Ctrl+C to terminate worker.");

                    tokio::select! {
                        _ = tokio::signal::ctrl_c() => {
                            println!("\nShutdown signal received. Stopping rpc-server...");
                            let _ = proc.kill().await;
                        }
                        exit = proc.wait() => {
                            println!("rpc-server exited: {:?}", exit);
                        }
                    }
                    control_handle.abort();
                    let _ = supervisor.stop().await;
                }
                Err(e) => {
                    control_handle.abort();
                    eprintln!("Failed to spawn {:?}: {}. Please check that rpc-server is built and in PATH.", rpc_binary, e);
                    std::process::exit(1);
                }
            }
        }

        Commands::Tunnel {
            action,
            api_port,
            rpc_port,
        } => {
            println!("=== ADB USB Tunnel Supervisor ===");
            match action.to_lowercase().as_str() {
                "setup" | "start" => {
                    match AdbTunnelSupervisor::setup_tunnel(api_port, rpc_port, None) {
                        Ok(status) => {
                            println!("ADB Tunnel established successfully!");
                            println!(
                                "  API Forward:  127.0.0.1:{} -> Device:{}",
                                status.api_port, status.api_port
                            );
                            println!(
                                "  RPC Reverse:  Device:{} -> 127.0.0.1:{}",
                                status.rpc_port, status.rpc_port
                            );
                            if let Some(dev) = status.device {
                                println!(
                                    "  Device:       {} ({:?})",
                                    dev.serial,
                                    dev.model.unwrap_or_default()
                                );
                            }
                        }
                        Err(e) => eprintln!("Tunnel setup failed: {}", e),
                    }
                }
                "teardown" | "stop" => {
                    let _ = AdbTunnelSupervisor::teardown_tunnel(api_port, rpc_port);
                    println!("ADB tunnels removed.");
                }
                _ => {
                    if !AdbTunnelSupervisor::is_adb_available() {
                        println!("ADB binary: Not found in PATH");
                    } else {
                        println!("ADB binary: Available");
                        match AdbTunnelSupervisor::list_devices() {
                            Ok(devices) => {
                                println!("Connected USB Devices ({}):", devices.len());
                                for d in devices {
                                    println!(
                                        "  - Serial: {}, State: {}, Model: {:?}",
                                        d.serial,
                                        if d.authorized {
                                            "Authorized"
                                        } else {
                                            "Unauthorized"
                                        },
                                        d.model
                                    );
                                }
                            }
                            Err(e) => eprintln!("Failed to list devices: {}", e),
                        }
                    }
                }
            }
        }

        Commands::Client {
            host,
            node,
            transport,
            prompt,
            model,
            preset,
        } => {
            let config = NexusConfig::load()?;
            let trans_mode = transport
                .parse::<TransportMode>()
                .unwrap_or(TransportMode::Auto);

            let (usb_endpoint, is_usb) = if host.is_none() && config.cluster.prefer_adb_tunnel {
                AdbTunnelSupervisor::resolve_transport_endpoint(
                    trans_mode,
                    config.network.api_port,
                    config.cluster.rpc_port,
                )
            } else {
                (None, false)
            };

            let client = match host
                .or(config.network.default_host.clone())
                .or(usb_endpoint)
            {
                Some(h) => {
                    if is_usb {
                        println!("Connected via low-latency USB Cable (ADB Tunnel localhost:8080)");
                    }
                    NexusClient::new(h)
                }
                None => {
                    let discovery = Arc::new(DiscoveryService::new(config.clone(), None));
                    let _listener = discovery.clone().start_listener();
                    let _mdns = discovery.clone().start_mdns();
                    discovery.send_probe().await;
                    println!("Auto-discovering compute host on subnet (Wi-Fi)...");
                    if let Some(node_selector) = node {
                        tokio::time::sleep(Duration::from_secs(2)).await;
                        let selector = node_selector.to_lowercase();
                        let peer = discovery
                            .get_active_peers()
                            .await
                            .into_iter()
                            .find(|peer| peer.uuid.to_string().to_lowercase() == selector)
                            .ok_or_else(|| {
                                std::io::Error::new(
                                    std::io::ErrorKind::NotFound,
                                    format!("No discovered node matches '{}'", node_selector),
                                )
                            })?;
                        NexusClient::new(peer.api_endpoint())
                    } else {
                        NexusClient::resolve_from_discovery(&discovery, Duration::from_secs(10))
                            .await?
                    }
                }
            };

            // Load preset persona if specified
            let active_preset = if let Some(p_name) = &preset {
                let p = Preset::load_by_name(p_name, &config.node.presets_dir)?;
                Some(p)
            } else {
                None
            };

            let sys_prompt = active_preset.map(|p| p.system_prompt);

            // If prompt is specified, run CLI batch stream
            if let Some(prompt_text) = prompt {
                println!("Connected to Nexus host at: {}", client.endpoint());
                println!("Sending prompt: \"{}\"\n", prompt_text);

                let mut messages = Vec::new();
                if let Some(sys) = &sys_prompt {
                    messages.push(ChatMessage::system(sys));
                }
                messages.push(ChatMessage::user(prompt_text));

                let req = ChatCompletionRequest {
                    model,
                    messages,
                    temperature: Some(0.7),
                    top_p: Some(0.9),
                    max_tokens: Some(512),
                    stream: true,
                };

                let mut stream = client.stream_chat(req).await?;
                print!("Response: ");
                std::io::stdout().flush()?;

                while let Some(chunk) = stream.next().await {
                    match chunk {
                        Ok(text) => {
                            print!("{}", text);
                            std::io::stdout().flush()?;
                        }
                        Err(e) => {
                            eprintln!("\nStreaming error: {}", e);
                            break;
                        }
                    }
                }
                println!();
            } else {
                // Interactive split-screen TUI mode
                let app = ChatApp::new(client, model, sys_prompt);
                run_chat_tui(app).await?;
            }
        }

        Commands::Doctor => {
            let config = NexusConfig::load().unwrap_or_default();
            let report = nexus::doctor::run_doctor(&config);
            report.print();
            if let Some(path) = &log_path {
                println!("Log file: {}", path.display());
            }
            std::process::exit(report.exit_code());
        }
        Commands::Bench {
            endpoint,
            model,
            ctx,
            runs,
            prompt,
            max_tokens,
            node,
            backend,
        } => {
            use nexus::bench::{
                backend_key, parse_backend_key, run_benchmark, BenchRunConfig, BenchStore,
            };

            let backend = backend
                .as_deref()
                .and_then(parse_backend_key)
                .unwrap_or_else(|| SystemProfile::probe().detected_backend);

            let path = BenchStore::default_path();
            let mut store = BenchStore::load(&path)?;
            println!(
                "Bench: endpoint={} model={} node={} backend={} ctx={} runs={}",
                endpoint,
                model,
                node,
                backend_key(backend),
                ctx,
                runs
            );
            let cfg = BenchRunConfig {
                endpoint: &endpoint,
                model: &model,
                node_id: &node,
                backend,
                context_size: ctx,
                runs,
                prompt: &prompt,
                max_tokens,
            };
            let entry = run_benchmark(&mut store, &cfg).await?;
            store.save(&path)?;
            println!(
                "Recorded gen_tok_s={:.2} ttft_ms={:?} prompt_tok_s={:?} → {}",
                entry.gen_tok_s,
                entry.ttft_ms,
                entry.prompt_tok_s,
                path.display()
            );
            for (i, sample) in entry.samples.iter().enumerate() {
                println!(
                    "  run {}: gen_tok_s={:.2} ttft_ms={:?}",
                    i + 1,
                    sample.gen_tok_s,
                    sample.ttft_ms
                );
            }
        }
    }

    Ok(())
}
