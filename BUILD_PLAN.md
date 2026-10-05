<!-- STREAMING_CHUNK:Outlining build plan workspace and tooling requirements -->
Nexus-LLM: Phased Execution & Antigravity Build Plan
Target Workspace: ~/local-server/ Tooling: Google Antigravity CLI (agy), Cargo, Rust 1.80+ Execution Order: Node A (Termux ARM64) Host Daemon -> Node B (Debian 13 Penryn) TUI Client
<!-- STREAMING_CHUNK:Specifying Phase 1 foundation, memory guard, and supervisor -->
Phase 1: System Introspection, Memory Safety Guard, & Process Supervisor
Antigravity Directive
antigravity run --task "Phase 1: Foundation, Profiler, and Process Supervisor"


Rust Interface Contracts
// src/sysinfo.rs
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AccelerationBackend {
    Vulkan,
    ArmCpuDotProd,
    X86Baseline,
    GenericCpu,
}

#[derive(Debug, Clone)]
pub struct SystemProfile {
    pub total_ram_mb: u64,
    pub available_ram_mb: u64,
    pub detected_backend: AccelerationBackend,
    pub recommended_threads: usize,
}

impl SystemProfile {
    pub fn probe() -> Self;
    pub fn can_safely_load(&self, model_file_size_bytes: u64, context_size: usize) -> bool;
}

// src/supervisor.rs
#[derive(Debug, Clone)]
pub struct LlamaServerConfig {
    pub binary_path: std::path::PathBuf,
    pub model_path: std::path::PathBuf,
    pub host: String,
    pub port: u16,
    pub gpu_layers: u32,       // 99 for Vulkan offload, 0 for CPU
    pub threads: usize,
    pub context_size: usize,
}

pub struct ProcessSupervisor {
    // Manages child tokio::process::Child with automatic SIGTERM / SIGKILL cleanup
}

impl ProcessSupervisor {
    pub async fn spawn_with_fallback(config: LlamaServerConfig) -> Result<Self, SupervisorError>;
    pub async fn is_healthy(&self) -> bool;
    pub async fn stop(&mut self) -> Result<(), SupervisorError>;
}


Objectives
Initialize Cargo workspace with dependencies: tokio (full), serde, serde_yaml, toml, clap, tracing, tracing-subscriber.
Implement src/sysinfo.rs:
Parse /proc/meminfo (MemTotal, MemAvailable).
Probe for Vulkan support: check for Vulkan runtime presence (dynamic library probe or which vulkaninfo) and inspect /proc/cpuinfo for ARM asimddp/i8mm.
Enforce the 75% memory ceiling: fail early if (model_size + context_kv) > 0.75 * MemAvailable.
Implement src/config.rs:
Parse ~/.nexus/config.toml with default configuration fallbacks.
Implement src/supervisor.rs:
Spawn llama-server with --host 0.0.0.0, --port 8080, -m <PATH>, -c <CTX>.
If prefer_gpu is enabled, attempt launch with -ngl 99. Monitor startup logs; if Vulkan fails to initialize, gracefully terminate the process and restart in CPU mode (-ngl 0 -t 6).
Verification Command
cargo test --test test_phase1


<!-- STREAMING_CHUNK:Specifying Phase 2 discovery daemon and SSE client -->
Phase 2: Autonomous Discovery & SSE Streaming Engine
Antigravity Directive
antigravity run --task "Phase 2: Local Discovery Daemon & SSE Streaming Client"


Objectives
Implement src/discovery.rs:
Asynchronous UDP broadcaster and listener on 0.0.0.0:9999 using tokio::net::UdpSocket.
Binary 64-byte beacon encoder/decoder with CRC-16-CCITT validation.
Telemetry payload reporting: active backend (Vulkan vs. CPU), free RAM, thermal index, and active model.
Thread-safe peer cache (Arc<RwLock<HashMap<Uuid, PeerNode>>>) with 6,000 ms stale detection.
Implement src/client.rs:
Asynchronous OpenAI-compatible HTTP client consuming /v1/chat/completions with Server-Sent Events (reqwest-eventsource).
Automatic host endpoint resolution via discovery.rs (discovering Node A from Node B without manual IP entry).
Verification Command
cargo test --test test_discovery


<!-- STREAMING_CHUNK:Specifying merged Phase 3 registry, control plane, and model management -->
Phase 3: Peer Registry, Control-Plane Validation, & Model Management
Antigravity Directive
antigravity run --task "Phase 3: Peer Registry, Control Plane, GGUF Metadata, and Persona Engine"


Objectives
Implement src/peer_registry.rs:
Maintain one authoritative event-driven peer registry per runtime, merging UDP, mDNS, static, and verified control-plane observations by stable node UUID.
Track discovered, verifying, healthy, stale, removed, and rejected lifecycle states with bounded metadata, endpoint conflict handling, and expiry.
Implement src/control_plane.rs:
Validate a bounded versioned Nexus-owned control-plane state response for identity, protocol, role, readiness, capabilities, and policy-capped allocatable memory.
Keep control-plane management separate from llama-server inference and SSE endpoints.
Implement src/gguf.rs:
Zero-copy binary parser for GGUF headers and key-value metadata arrays.
Extract architecture (llama, qwen2, gemma2), context length, and tensor count directly without loading weights into RAM.
Implement src/preset.rs:
YAML persona engine loading system prompts, temperature, top_p, and chat formatting templates (ChatML, Llama-3, Alpaca).
Implement src/downloader.rs:
Resumable chunked model downloader with SHA-256 validation.
Verification Commands
cargo test --test test_phase3_network
cargo test --test test_gguf_metadata


<!-- STREAMING_CHUNK:Specifying Phase 4 terminal client for Node B -->
Phase 4: Interactive Terminal Interface (TUI)
Antigravity Directive
antigravity run --task "Phase 4: Ratatui Interactive Client and Dashboard"


Objectives
Implement src/ui/chat.rs using ratatui + crossterm:
Interactive, responsive split-screen chat interface.
Streaming markdown rendering with scrolling history and prompt input line.
Implement src/ui/dashboard.rs:
Node status monitor: live display of Node A's GPU/Vulkan status, CPU temperature, RAM consumption, and generation speed (tokens/sec).
Ensure all TUI rendering runs with minimal CPU usage on Node B (Debian 13 Core 2 Duo).
Verification Command
# On Termux (Node A):
cargo run --bin nexusd

# On Debian 13 Mac (Node B):
cargo run --bin nexus -- client


<!-- STREAMING_CHUNK:Specifying Phase 5 Distributed RPC Pipelining and ADB Tunneling -->
Phase 5: Distributed RPC Layer Pipelining & ADB Tunneling
Antigravity Directive
antigravity run --task "Phase 5: Distributed RPC Pipelining & ADB Tunneling"


Objectives
Implement src/cluster.rs:
- Calculate cluster memory budgets (Node A max 8.5 GB standalone / 75% available RAM; Node B capped at 1800 MB).
- Greedy host-first layer watermarking with sequential layer pipelining (--split-mode layer).
- Automatic generation of --rpc <PEER:PORT> and --tensor-split arguments.
- Early rejection of models exceeding total cluster budget.
Implement src/tunnel.rs:
- Automated USB device detection and management via adb forward tcp:8080 tcp:8080 and adb reverse tcp:50052 tcp:50052.
- Transport mode selection: auto (USB priority with Wi-Fi fallback), usb (force ADB), wifi (subnet only).
Extend Discovery Beacon Protocol:
- Add StatusFlags::RPC_READY bit (0x0010) and rpc_port field in bytes 0x1A..0x1B.
- Provide find_best_rpc_peer() for autonomous offload targeting.
Implement Subcommands:
- nexus rpc: Worker process manager for Node B with 1800 MB hard cap and beacon broadcaster.
- nexus tunnel: Status, setup, and teardown of ADB USB tunnels.
- Update nexus client and nexus dashboard with transport badges and RPC worker status.
Verification Command
cargo test --test test_cluster_rpc


<!-- STREAMING_CHUNK:Specifying Phase 6 Unified Hub and Quick-Start UX -->
Phase 6: Unified Hub & Quick-Start UX
Antigravity Directive
antigravity run --task "Phase 6: Unified Hub & Quick-Start UX"


Objectives
Implement src/ui/hub.rs:
- Default execution of `nexus` (with no CLI subcommands) launches a full-screen interactive Ratatui Unified Hub.
- Persistent top navigation bar with function keys and tab cycling: `[F1: 💬 Chat]`, `[F2: 📦 Models]`, `[F3: 🖥️ Dashboard]`, `[F4: 🔗 Tunnel]`, `[F5: ⚙️ Settings]`.
- Asynchronous non-blocking event stream multiplexing keyboard input, periodic 500 ms status refresh ticks, and token streaming `mpsc` receiver.
- Safe in-app model hot-swapping with confirmation modal (`[Y / N]`) terminating running server, revalidating memory headroom, and spawning new model.
Implement src/ui/models_view.rs:
- Split-pane model browser with list navigation, zero-copy GGUF architecture inspection, and real-time KV cache calculation.
- Android LMK 75% memory health badges (`[OK]`, `[RPC]`, `[OOM]`).
- One-click `[Enter]` to load model and automatically switch to Chat.
Implement src/ui/settings_view.rs:
- In-app configuration editor displaying categorized settings (Hardware & Acceleration, Memory & LMK Safeguards, Network, Cluster RPC).
- Interactive boolean toggling (`Space`), numeric adjustment (`Left` / `Right`), and atomic persistence to `~/.nexus/config.toml` (`S`).
Implement src/ui/tunnel_view.rs:
- Interactive USB ADB device monitor, one-key port forward and reverse tunnel setup (`F`) and teardown (`T`).
Verification Command
cargo test --test test_hub_ui



