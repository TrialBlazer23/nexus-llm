use clap::{Parser, Subcommand};
use futures_util::StreamExt;
use nexus::client::{ChatCompletionRequest, ChatMessage, NexusClient};
use nexus::config::NexusConfig;
use nexus::discovery::DiscoveryService;
use nexus::downloader::ModelDownloader;
use nexus::gguf::GgufMetadata;
use nexus::preset::Preset;
use nexus::sysinfo::SystemProfile;
use nexus::ui::chat::{run_chat_tui, ChatApp};
use nexus::ui::dashboard::run_dashboard_tui;
use nexus::ui::models::scan_models_dir;
use std::io::Write;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

#[derive(Parser, Debug)]
#[command(name = "nexus")]
#[command(about = "Nexus-LLM CLI Orchestrator & Client")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand, Debug)]
enum Commands {
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

    /// Connect to compute host and execute chat generation (interactive TUI or CLI stream)
    Client {
        /// Optional host endpoint (e.g. http://192.168.1.100:8080). If omitted, auto-discovers host.
        #[arg(long)]
        host: Option<String>,

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
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let cli = Cli::parse();

    match cli.command {
        Commands::Info => {
            let profile = SystemProfile::probe();
            let max_allowed_mb = profile.max_allowed_memory_bytes() / (1024 * 1024);

            println!("=== Nexus-LLM System Profile ===");
            println!("Architecture Target:   {}", std::env::consts::ARCH);
            println!("Total Memory:          {} MB", profile.total_ram_mb);
            println!("Available Memory:      {} MB", profile.available_ram_mb);
            println!("LMK Safety Cap (75%):  {} MB", max_allowed_mb);
            println!("Acceleration Backend:  {}", profile.detected_backend);
            println!("Vulkan Runtime:        {}", if SystemProfile::probe_vulkan() { "Detected" } else { "Not Found" });
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
                println!("Result:                PASSED [OK] (Headroom: {} MB)", headroom);
            } else {
                let deficit = total_required_mb.saturating_sub(max_allowed_mb);
                eprintln!("Result:                REJECTED [FAIL] (Exceeds ceiling by {} MB)", deficit);
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
            println!("Architecture:          {}", gguf.architecture.as_deref().unwrap_or("unknown"));
            println!("Model Name:            {}", gguf.model_name.as_deref().unwrap_or("unnamed"));
            println!("Max Context Length:    {}", gguf.context_length.map(|c| c.to_string()).unwrap_or_else(|| "unknown".to_string()));
            println!("Transformer Layers:    {}", gguf.block_count.map(|b| b.to_string()).unwrap_or_else(|| "unknown".to_string()));
            println!("Attention Heads:       {}", gguf.head_count.map(|h| h.to_string()).unwrap_or_else(|| "unknown".to_string()));
            println!("KV Attention Heads:    {}", gguf.head_count_kv.map(|h| h.to_string()).unwrap_or_else(|| "unknown".to_string()));
            println!("Embedding Length:      {}", gguf.embedding_length.map(|e| e.to_string()).unwrap_or_else(|| "unknown".to_string()));
            println!("Model File Size:       {} MB", file_mb);
            println!("Exact KV Cache ({} t): {} MB", ctx, exact_kv_mb);

            let profile = SystemProfile::probe();
            let safe = profile.can_safely_load_gguf(&gguf, ctx);
            println!("Android LMK Guard:     {}", if safe { "PASSED [OK]" } else { "BLOCKED [INSUFFICIENT RAM]" });
        }

        Commands::Download { url, output, sha256 } => {
            println!("=== Resumable Model Downloader ===");
            println!("Source URL:            {}", url);
            println!("Destination Path:      {:?}", output);
            if let Some(hash) = &sha256 {
                println!("Expected SHA-256:      {}", hash);
            }

            let downloader = ModelDownloader::new();
            downloader.download(&url, &output, sha256.as_deref(), |prog| {
                let speed_mb = prog.speed_bytes_per_sec / (1024.0 * 1024.0);
                let down_mb = prog.downloaded_bytes / (1024 * 1024);
                if let Some(pct) = prog.percent {
                    let total_mb = prog.total_bytes.unwrap_or(0) / (1024 * 1024);
                    print!("\rDownloading: {:.1}% ({}/{} MB) - {:.2} MB/s   ", pct, down_mb, total_mb, speed_mb);
                } else {
                    print!("\rDownloading: {} MB - {:.2} MB/s   ", down_mb, speed_mb);
                }
                let _ = std::io::stdout().flush();
            }).await?;

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

            println!("Listening for discovery beacons on UDP 9999 for {}s...", timeout);
            tokio::time::sleep(Duration::from_secs(timeout)).await;

            let peers = discovery.get_active_peers().await;
            if peers.is_empty() {
                println!("No active cluster nodes discovered.");
            } else {
                println!("\nDiscovered Cluster Nodes ({}):", peers.len());
                println!("{:<38} {:<22} {:<10} {:<10} {:<10}", "Node UUID", "Endpoint", "Role", "Free RAM", "Backend");
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
            let _listener = discovery.clone().start_listener();
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
                println!("{:<30} {:<12} {:<10} {:<8} {:<12} {:<10}", "Filename", "Size", "Arch", "Context", "KV (4k)", "LMK Guard");
                println!("{}", "-".repeat(88));
                for m in models {
                    let lmk_status = if m.lmk_compatible { "Compatible" } else { "Exceeds RAM" };
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

        Commands::Client { host, prompt, model, preset } => {
            let config = NexusConfig::load().unwrap_or_default();
            let client = match host.or(config.network.default_host.clone()) {
                Some(h) => NexusClient::new(h),
                None => {
                    let discovery = Arc::new(DiscoveryService::new(config.clone(), None));
                    let _listener = discovery.clone().start_listener();
                    discovery.send_probe().await;
                    println!("Auto-discovering compute host on subnet...");
                    NexusClient::resolve_from_discovery(&discovery, Duration::from_secs(10)).await?
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
    }

    Ok(())
}
