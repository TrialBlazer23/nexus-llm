# Nexus-LLM

**Nexus-LLM** is an ultra-lightweight distributed LLM orchestrator, autonomous symmetric peer mesh, multi-model execution router, headless compute daemon, and interactive terminal/web interface written in Rust.

It turns a heterogeneous collection of personal devices into a unified, decentralized AI network where any connected node can act as an **Inference Host**, an **RPC Worker**, an **Orchestration Router**, or an **Interactive Client**:

- **Android Targets (e.g. Samsung Galaxy S23 Ultra)**: Qualcomm Snapdragon 8 Gen 2, Adreno 740 Vulkan acceleration, 12 GB RAM, Android 14+ Termux ARM64.
- **Legacy x86 Workstations (e.g. Apple MacBook)**: Intel Core 2 Duo P7550 @ 2.26 GHz, 3.6 GiB RAM, Debian 13 Trixie x86-64 (guaranteed Penryn-safe SSE4.1 execution, zero AVX/AVX2).
- **Modern PCs & Laptops**: Linux x86-64, Windows Subsystem for Linux (WSL2), or native environments.

There are **no hardcoded nodes, fixed roles, or static RAM limits**. Hardware capabilities, memory budgets, and link latencies are probed dynamically. You can dispatch models to any connected device on the fly from the Terminal User Interface (TUI) or embedded Web Interface, stream conversations from anywhere in the mesh, offload layers across devices, or run massive Mixture-of-Experts (MoE) models directly on mobile storage.

---

## Breakthrough: MoE Flash Streaming with BigMoeOnEdge

### Running Models Bigger than Device RAM

Mixture-of-Experts (MoE) architectures provide state-of-the-art coding and reasoning capabilities by using sparse expert routing. However, running a model like **`Qwen3-Coder-30B-A3B-Instruct`** (an 18.55 GB GGUF model with 48 layers and 128 experts) on a memory-constrained phone (e.g. 12 GB RAM) has traditionally been impossible: standard dense execution maps all weights into RAM, exceeding safe memory limits and triggering immediate termination by the **Android Low Memory Killer (LMK)**.

Nexus-LLM integrates **MoE Flash Streaming** by supervising Helldez's **`bmoe-cli`** engine. Instead of buffering the entire model in memory:
1. **Dynamic Expert LRU Caching**: Only active expert weights are loaded into an ultra-compact memory cache (e.g. 2.0 GB RAM) on demand.
2. **Flash Row Streaming & Async I/O**: Weights are streamed directly from fast flash storage (e.g. UFS 4.0) with asynchronous I/O overlapping computation and data transfer.
3. **MoE Stream LMK Guard**: Nexus dynamically budgets memory as `Resident Weights + Expert Cache + KV Cache Overhead` rather than total file size, allowing models larger than physical RAM to run safely without triggering OS out-of-memory kills.
4. **Seamless OpenAI / SSE Streaming Adapter**: Nexus translates internal `bmoe-cli` token events into standard OpenAI-compatible Server-Sent Events (SSE), making streaming chat, web UI, and API integration completely transparent.

### Tested and Hardware-Verified

> **Tested on Physical Hardware:**  
> This feature was tested and verified on a **Samsung Galaxy S23 Ultra** (Qualcomm Snapdragon 8 Gen 2, 12 GB RAM, Android 14 running Termux aarch64):
> - **Model**: `Qwen3-Coder-30B-A3B-Instruct-Q4_K_M.gguf` (**18.55 GB** file size).
> - **Memory Footprint**: Loaded within a **~2.0 GB LRU expert cache** + **~950 MB** anonymous buffers (total resident memory under 3.5 GB), staying far below the Android LMK threshold.
> - **Performance**: Achieved **~2.35 tokens/sec sustained decode**, 0 major page faults, and rock-solid continuous generation without memory crashes.

### Credit & Appreciation to BigMoeOnEdge

Nexus-LLM gives special credit and immense appreciation to **[Helldez](https://github.com/Helldez)** and the **[BigMoeOnEdge](https://github.com/Helldez/BigMoeOnEdge)** project:
> **BigMoeOnEdge** makes it possible for Nexus-LLM to run massive MoE models that are significantly larger than the physical RAM of edge and mobile devices. Helldez's innovative flash-streaming architecture, expert LRU cache governor, and low-latency storage streaming unlock true high-parameter local intelligence on commodity personal hardware.

---

## Mesh Architecture

```mermaid
flowchart TD
    subgraph Nexus Peer Mesh
        subgraph Node A: Android Phone / Termux
            A1[nexus / nexusd]
            A2[Vulkan Adreno 740 / CPU]
            A3[BigMoe Flash Streaming bmoe-cli]
            A4[Android LMK 75% Guard]
            A1 --- A2
            A1 --- A3
            A1 --- A4
        end

        subgraph Node B: Legacy x86 MacBook / Debian
            B1[nexus / nexusd]
            B2[Penryn SSE4.1 CPU Safe]
            B3[Memory Budget Cap Guard]
            B4[llama.cpp rpc-server Worker]
            B1 --- B2
            B1 --- B3
            B1 --- B4
        end

        subgraph Node C: Workstation / Laptop
            C1[nexus / nexusd]
            C2[Interactive Unified Hub TUI & Web Hub]
            C3[2-Tier Orchestrator Router & KB]
            C4[Mesh Gateway :8090]
            C1 --- C2
            C1 --- C3
            C1 --- C4
        end
    end

    Discovery((Zero-Config Discovery\nUDP 9999 Beacons / mDNS / Static IP))
    ControlPlane((Signed Control Plane\nHTTP :9998 / Ed25519 Pair))
    Gateway((Mesh Gateway\nOpenAI API :8090))

    Node A <--> Discovery
    Node B <--> Discovery
    Node C <--> Discovery

    Node C ===|1. Authenticated Load Dispatch| ControlPlane ===> Node A
    Node C ===|2. Streaming Chat / Completions| Gateway ===> Node A
    Node A -.->|3. Sequential Layer Offload| B4
```

---

## What Nexus-LLM Has Built & Current Capabilities

Nexus-LLM has evolved from a simple TUI client into a full-featured distributed inference network:

### 1. Unified Terminal Interface (Ratatui TUI)
Launch `nexus` with no arguments to enter the full-screen terminal hub:
- **`[F1] 💬 Chat`**: Split-screen streaming dialogue, markdown syntax formatting, code blocks, prompt history (`Alt+↑/↓`), generation abort (`Esc`), slash commands, and hot-switching chat endpoints (`Alt+C`).
- **`[F2] 📦 Models`**: Split-pane local catalog and zero-copy GGUF inspector. Includes **dual-mode navigation**:
  - **Local Catalog**: Inspect file sizes, architectures, context limits, KV cache overhead, and memory badges (`[OK]`, `[RPC]`, `[OOM]`, `[STREAM]`).
  - **Hugging Face Explorer (`[H]` / `[Tab]`)**: Search Hugging Face repositories in-TUI, browse GGUF quantizations, view memory fit badges, and queue downloads.
  - **Multi-Shard GGUF Aggregation**: Automatically groups split files (`*-00001-of-00003.gguf`) into unified entries and queues sequential downloads.
  - **Target Node Dispatch (`[Enter]`)**: Interactive modal listing all discovered cluster devices with live RAM and acceleration backends to choose execution target.
  - **Local Model Importer (`[I]`)**: Scans host download folders (or Android `/sdcard/Download`) with automatic cross-filesystem copy fallback.
  - **Corrupted Model Cleaner (`[Shift+X]`)**: Identifies and deletes 0-byte or incomplete files.
- **`[F3] 🌐 Cluster`**: Live WiFi mesh telemetry monitor displaying discovered peers, endpoints, hardware roles, available RAM, and active models.
  - Real-time link quality metrics (RTT and network throughput: `⚡ 3.0ms · 50MB/s`).
  - Actions: Remote Model Load (`[L]`), Request RPC Worker (`[W]`), Inspect Hardware (`[I]`), Add Static Peer (`[A]`), Pair with Code (`[P]`), Connect Chat (`[Enter]`).
- **`[F4] ⚙️ Settings`**: Live configuration editor with immediate hot-reload into `~/.nexus/config.toml`. Configure Node Identity, Binary Paths, Ports, MoE Cache Governor, Hugging Face Bearer Token, Layout Modes, and Battery Safety Ceilings.
- **`[F5] 🚇 Tunnel`**: Dedicated ADB USB port forwarding and reverse tunnel supervisor for zero-latency wired tethering between phone and PC.
- **`[F6] 🤖 Agents`**: Orchestrator agent bus monitor displaying multi-model routing decisions, active task queues (`tasks.json`), inter-model token streams, and background Janitor memory distillation.
- **`[F7] 📜 Logs`**: Live diagnostic log viewer streaming directly from `~/.nexus/logs/` with level filtering (`ALL`/`INFO`/`WARN`/`ERROR`), query buffer, auto-tail follow toggling (`Space`), and scroll navigation (`j`/`k`).
- **Command Palette (`Ctrl+P`)**: Zero-dependency fuzzy finder indexing tabs, local models, mesh peers, hub actions, and slash commands.
- **2-Line Persistent Dock**: Displays active model name, target host endpoint, live tokens/sec, a 10-bar context fill gauge (`[███░░░░░░░] 512/4096 (12%)`), centralized status badges (`[OK]`, `[WARN]`, `[FAIL]`, `[STREAM]`, etc.), and global shortcuts.
- **Adaptive Mobile Layout**: Automatically detects narrow terminals (`width < 85`, `height < 24`) such as Android Termux, condensing tables, stacking split panes, and adapting telemetry.

### 2. Embedded Zero-Dependency Web Hub & Gateway (`nexus web`)
Access the cluster from any browser on your network:
- **Zero-Dependency Web PWA**: Single bundle embedded directly in the binary; served on port `8090` without requiring Node.js, npm, or external web servers.
- **6-Digit Terminal PIN Security**: Secure authentication protecting control and chat endpoints.
- **Web UI Capabilities**: Real-time streaming chat, active model hot-switching, live cluster telemetry, Hugging Face search with 1-click background downloading and progress polling, zero-allocation GGUF metadata inspector, and runtime settings editor.
- **Prompt Cache Slot Persistence**: Implements `--slot-save-path` with LRU disk quota eviction and battery safety thresholds.

### 3. Orchestration, Two-Tier Router & Shared Knowledge Base
- **Multi-Instance Supervisor**: Host multiple specialized models concurrently across dedicated port slots with cumulative memory safety tracking.
- **Two-Tier Router (`src/router.rs`)**:
  - *Tier 1 (Deterministic)*: Instant zero-overhead routing based on tags, keywords, and task presets.
  - *Tier 2 (Orchestrator-Assisted)*: Ambiguous prompts route to an orchestrator model for classification into strict typed JSON choices.
  - *Application Veto*: Rust enforces runtime validation against node capabilities, cleanly falling back to deterministic routing on hallucinations.
- **Model-to-Model Agent Bus (`src/task.rs`)**: Signed inter-model task communication (`POST /nexus/control/v1/agent/message`) with durable tracking in `tasks.json`.
- **Embedded Knowledge Base (`src/kb/`)**: Embedded single-file `redb` database at `~/.nexus/kb/` storing content-addressed document chunks, versioned personas, and vector embeddings with in-process vector retrieval for RAG.
- **Background Janitor Agent & LAN Sync**: Distills conversation history into durable episodic memory and synchronizes KB manifests across the mesh.

### 4. Zero-Config Discovery & Signed Security
- **Triple-Channel Discovery**: 64-byte UDP broadcast beacons over port `9999` with CRC-16-CCITT, mDNS-SD zero-configuration service discovery, and static IP fallback.
- **Ed25519 Cryptographic Trust**: Every node generates an Ed25519 keypair (`node_identity.rs`). Control-plane requests are canonically signed with timestamp replay protection.
- **6-Digit Out-of-Band Pairing**: Mutual trust is established via an interactive pairing code dialog (`nexus pair`).

### 5. Resilient Transfer & Downloader Engine
- **HTTP Range Resume Downloader**: Chunked streaming downloader with `.part.json` sidecar verification, exponential backoff, and 1 MB buffer tuning to prevent socket idle timeouts.
- **Zero-Byte & HTML Rejection**: Inspects initial bytes for `GGUF` magic headers, rejecting HTML error pages (e.g. Hugging Face `/blob/` URLs) before writing invalid files.
- **LAN Blob Transfer**: Content-addressed model distribution over local Wi-Fi (`/nexus/control/v1/blob/{digest}`).

### 6. Hardware Safety & Baseline Enforcements
- **Legacy x86 Penryn Safety**: Strictly disables AVX, AVX2, FMA, and SSE4.2 in `.cargo/config.toml` to guarantee code will never crash Penryn Core 2 Duo CPUs with `SIGILL`. Release binaries are scanned via `scripts/check_penryn_opcodes.sh`.
- **Android LMK Memory Guard**: Enforces `(Model Size + KV Cache) < 0.75 * MemAvailable` to prevent Android's Low Memory Killer from terminating inference.
- **CPU Mode Isolation**: Prevents Adreno Vulkan compute driver leakage when running in CPU-only mode on mobile chipsets.

---

## One-Command Setup

Nexus-LLM is a Rust orchestrator. Inference backends (`llama-server`, `rpc-server`, and `bmoe-cli`) are **external subprocesses** — not vendored into the Rust crate. The unified setup script detects your platform, installs dependencies, compiles backends with hardware-safe flags, builds Nexus, and installs everything under `~/.nexus/bin`.

### Linux / macOS / Android Termux

```bash
git clone https://github.com/TrialBlazer23/nexus-llm.git ~/nexus-llm
cd ~/nexus-llm
bash scripts/setup.sh
export PATH="$HOME/.nexus/bin:$PATH"

# Run diagnostics and launch TUI
nexus doctor
nexus
```

### Windows (PowerShell → WSL2)

```powershell
git clone https://github.com/TrialBlazer23/nexus-llm.git C:\nexus-llm
cd C:\nexus-llm
.\scripts\setup.ps1
```

### Setup Flags

| Flag | Description |
|------|-------------|
| `--dry-run` | Probe platform and print build plan without executing |
| `--skip-moe` | Skip building BigMoeOnEdge (`bmoe-cli`) |
| `--skip-llama` | Skip building stock `llama.cpp` (`llama-server` / `rpc-server`) |
| `--jobs N` | Number of parallel build compilation jobs |
| `--prefix DIR` | Installation root directory (defaults to `~/.nexus`) |
| `--model URL` | Download an initial GGUF model immediately after installation |

After installation, `nexus setup` forwards directly to `scripts/setup.sh`.

---

## CLI Subcommands Reference

For headless nodes, automated scripts, and dedicated compute workers:

```bash
# Launch interactive TUI Hub (default)
nexus

# Launch embedded Web Interface & Mesh Gateway (:8090)
nexus web

# Connect to compute host and execute streaming chat (TUI or CLI stream)
nexus client --prompt "Explain Rust ownership in two sentences."
nexus client --host http://192.168.1.100:8080

# Run headless supervisor daemon
nexusd

# Run as a dedicated RPC compute worker to receive offloaded layers
nexus worker --port 50052 --mem 1800

# Probe hardware memory and acceleration profiles
nexus info

# Check if a model and context safely fit within memory safety limits
nexus check -m ~/.nexus/models/model.gguf -c 4096

# Inspect GGUF headers, architecture, and tensors without loading weights
nexus inspect -m ~/.nexus/models/model.gguf -c 4096

# Download a remote model with resume, SHA-256 validation, and HTML rejection
nexus download <URL> -o ~/.nexus/models/model.gguf

# Import local models from downloads folder or scan for corrupted files
nexus import --scan
nexus import --path ~/Downloads/model.gguf --copy
nexus import --clean

# Pair with a remote node using its 6-digit control-plane code
nexus pair --host http://192.168.1.50:9998 --code 123456

# Measure prompt and generation throughput to feed placement ranking
nexus bench --endpoint http://localhost:8090 --model qwen2.5-coder-1.5b

# Manage ADB USB port forwarding and reverse tunnels for phone tethering
nexus tunnel setup
nexus tunnel status
nexus tunnel teardown

# Run diagnostic preflight checks
nexus doctor
```

---

## Configuration Reference (`~/.nexus/config.toml`)

Nexus-LLM persists configuration to `~/.nexus/config.toml` with automatic live reloading:

```toml
[node]
node_id = "018f3a5e-23a1-7c9d-8d4e-5f1234567890"
node_name = "galaxy-s23-ultra"
runtime_role = "hybrid"                        # host | worker | client | hybrid
models_dir = "~/.nexus/models"
llama_server_binary = "~/.nexus/bin/llama-server"
rpc_server_binary = "~/.nexus/bin/rpc-server"
bmoe_binary = "~/.nexus/bin/bmoe-cli"

[network]
api_host = "0.0.0.0"
api_port = 8080
control_port = 9998
gateway_port = 8090
default_host = "http://127.0.0.1:8080"
static_peers = ["192.168.1.50"]
enable_mdns = true
enable_broadcast = true

[inference]
threads = 4
context_size = 4096
gpu_layers = 99                                # set 0 for pure CPU mode
fallback_to_cpu = true

[inference.moe]
enabled = true
cache_mb = "auto"                              # expert RAM cache governor (or e.g. 2048)

[inference.cache]
enabled = true
max_cache_mb = 4096                            # prompt cache slot persistence quota

[hardware.safety]
battery_threshold_percent = 15                 # throttle or reject heavy loads on low battery

[hf]
token = ""                                     # optional Hugging Face bearer token for gated models

[ui]
layout_mode = "auto"                           # auto | compact | wide
```

---

## Repository Structure

```text
nexus-llm/
├── Cargo.toml               # Package targets and dependencies
├── .cargo/
│   └── config.toml          # Target compiler flags (Snapdragon vs Penryn SSE4.1)
├── AGENTS.md                # System directives and hardware safety rules
├── DESIGN_SPEC.md           # Network protocols, binary beacons, and MoE flash streaming spec
├── BUILD_PLAN.md            # Phased execution milestones and hardware verification records
├── ORCHESTRATOR_PLAN.md     # Multi-instance supervisor and router roadmap
├── AGENT_LEARNINGS.md       # Durable engineering lessons and hardware soak logs
├── README.md                # Project documentation and setup guide
├── scripts/
│   ├── setup.sh             # Unified bootstrap: backends + Nexus -> ~/.nexus/bin
│   ├── setup.ps1            # Windows setup forwarder to WSL
│   ├── versions.env         # Pinned llama.cpp and BigMoeOnEdge refs
│   ├── check_penryn_opcodes.sh # Opcode scan enforcing Penryn SSE4.1 safety
│   ├── build_web.sh         # Web UI build helper
│   └── lib/                 # Platform detect, dep installer, llama, bmoe, and config helpers
├── web/
│   └── dist/
│       ├── index.html       # Embedded zero-dependency Web Hub PWA bundle
│       └── manifest.json    # Progressive Web App manifest
├── presets/
│   ├── coder.yaml           # Systems programming persona (ChatML)
│   └── general.yaml         # Conversational assistant persona (Llama-3)
├── src/
│   ├── main.rs              # CLI router and Unified Hub launcher
│   ├── daemon.rs            # Headless supervisor daemon (nexusd)
│   ├── config.rs            # TOML configuration engine (~/.nexus/config.toml)
│   ├── sysinfo.rs           # /proc parser, Android LMK memory guard, and MoE stream LMK
│   ├── supervisor.rs        # Multi-slot supervisor for llama-server and bmoe-cli
│   ├── bmoe_client.rs       # BigMoe bmoe-cli session client & OpenAI/SSE streaming adapter
│   ├── router.rs            # Two-tier orchestrator router (deterministic + LLM classifier)
│   ├── task.rs              # Inter-model agent bus and task state management
│   ├── gateway.rs           # Mesh OpenAI API Gateway (:8090) and embedded Web server
│   ├── control_plane.rs     # Authenticated remote model load/unload dispatch protocol
│   ├── control_plane_server.rs # HTTP control-plane listener (:9998)
│   ├── discovery.rs         # 64-byte UDP beacon protocol & peer cache
│   ├── mdns.rs              # Zero-config mDNS service discovery
│   ├── peer_registry.rs     # Dynamic peer lifecycle state management
│   ├── registry_runtime.rs  # Runtime registry actor wiring
│   ├── node_identity.rs     # Ed25519 cryptographic key generation and validation
│   ├── trust_auth.rs        # Signed request authentication and 6-digit pairing
│   ├── client.rs            # OpenAI HTTP and SSE streaming client
│   ├── cluster/             # Distributed RPC layer offload & placement intelligence
│   │   ├── memory.rs        # MemoryPlan and safe headroom calculator
│   │   ├── split.rs         # Greedy sequential layer partitioner (--split-mode layer)
│   │   └── rank.rs          # Target node ranker factoring RTT, throughput, and hardware
│   ├── store.rs             # Content-addressed model index (~/.nexus/models.json)
│   ├── bench.rs             # Model benchmark throughput store (~/.nexus/bench.json)
│   ├── hf.rs                # Hugging Face REST API client, repo resolver & quant picker
│   ├── import.rs            # Local model scanner, storage importer, and corrupt GGUF cleaner
│   ├── gguf.rs              # Bounded GGUF header parser & MoE architecture inspector
│   ├── downloader.rs        # Chunked HTTP Range resume downloader with SHA-256
│   ├── doctor.rs            # System and preflight diagnostics engine
│   ├── setup.rs             # Automated setup subcommand and config patcher
│   ├── tunnel.rs            # ADB USB port forwarding and reverse tunnel supervisor
│   ├── logging.rs           # Rotating file logger (~/.nexus/logs/)
│   ├── kb/                  # Embedded redb Knowledge Base, vector search, and sync
│   │   ├── store.rs         # Content-addressed chunk store
│   │   ├── vector.rs        # Cosine similarity and in-process vector retrieval
│   │   ├── embedder.rs      # Vector embedding client
│   │   ├── retriever.rs     # RAG context retriever
│   │   ├── sync.rs          # LAN KB sync protocol
│   │   └── janitor.rs       # Background episodic memory distillation agent
│   └── ui/
│       ├── hub/             # Unified Ratatui Hub controller, command bus, and keymaps
│       ├── chat.rs          # Streaming chat view with markdown parser
│       ├── models_view.rs   # Split-pane model browser & Hugging Face Explorer
│       ├── cluster_view.rs  # Interactive WiFi mesh monitor & link telemetry
│       ├── settings_view.rs # Live configuration editor
│       ├── tunnel_view.rs   # ADB USB tunnel monitor
│       ├── agents_view.rs   # Orchestrator agent bus and routing view
│       ├── logs_view.rs     # Live streaming diagnostic logs tab
│       ├── layout.rs        # Adaptive mobile and narrow display responsive engine
│       ├── badges.rs        # Centralized accessible status badge styles
│       ├── markdown.rs      # ANSI-styled streaming markdown renderer
│       └── dashboard.rs     # Standalone cluster performance monitor
└── tests/                   # 24 integration test suites covering all components
```

---

## Verification & Test Suite

The entire test suite compiles cleanly and passes deterministically across all targets:

```bash
# Run full unit and integration test suite (246 tests)
cargo test --locked

# Scan release binary to guarantee no illegal Penryn opcodes (AVX/AVX2/FMA/SSE4.2)
bash scripts/check_penryn_opcodes.sh
```

---

## Troubleshooting

### 1. "No host peer discovered on the network"
- Ensure devices are connected to the same Wi-Fi subnet.
- Check if your router isolates client devices (blocks UDP broadcast). You can bypass this by configuring static peers (`nexus client --host http://<IP>:8080` or setting `static_peers = ["<IP>"]` in `config.toml`), or by connecting via an ADB USB cable (`nexus tunnel setup`).

### 2. Android Termux terminates `llama-server` (Low Memory Killer)
- Verify available RAM with `nexus inspect -m <model>`.
- The Android LMK guard protects against models where `Model Size + KV Cache > 0.75 * MemAvailable`. If memory is tight, reduce context length (`-c 2048`), offload layers to an RPC worker, or use **MoE flash streaming** (`bmoe-cli`) for Mixture-of-Experts models.

### 3. Mac shows `SIGILL (Illegal Instruction)`
- This occurs if code was compiled with modern CPU instructions (AVX/AVX2/FMA/SSE4.2). Verify `.cargo/config.toml` includes `-C target-feature=-avx,-avx2,-fma,-sse4.2` and rebuild with `cargo build --release`.

### 4. PowerShell "command not found: cargo"
- Windows PowerShell does not have Rust/Cargo in PATH by default. Execute all Cargo commands inside WSL: `wsl bash -l -c "cd /mnt/c/nexus-llm && cargo test"`.

---

## Acknowledgments & Credits

Nexus-LLM is made possible by extraordinary open-source engineering:

- **[Helldez](https://github.com/Helldez)** for **[BigMoeOnEdge](https://github.com/Helldez/BigMoeOnEdge)**:
  Special credit and gratitude to Helldez for pioneering flash-streaming MoE execution. BigMoe's dynamic expert LRU caching, row streaming, and asynchronous I/O overlap make it possible for Nexus-LLM to run massive Mixture-of-Experts models (such as `Qwen3-Coder-30B-A3B-Instruct`) fluidly on mobile devices and edge hardware that have significantly less RAM than the model size.
- **[Georgi Gerganov](https://github.com/ggerganov)** & the **[llama.cpp](https://github.com/ggerganov/llama.cpp)** / **[ggml](https://github.com/ggerganov/ggml)** community for setting the standard for portable local LLM inference.
- **The [Ratatui](https://github.com/ratatui/ratatui)** & **[crossterm](https://github.com/crossterm-rs/crossterm)** teams for providing the premier terminal user interface ecosystem in Rust.
- **[redb](https://github.com/cberner/redb)** for the embedded pure-Rust transactional key-value store.
