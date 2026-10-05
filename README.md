# Nexus-LLM

**Nexus-LLM** is an ultra-lightweight distributed LLM orchestrator, headless compute daemon, and terminal user interface (TUI) written in Rust. It turns a heterogeneous collection of personal devices into an **autonomous symmetric peer mesh** where any connected device can act as an **Inference Host**, an **RPC Worker**, or an **Interactive TUI Client**:

- **Android Targets (e.g. Galaxy S23 Ultra)**: Qualcomm Snapdragon 8 Gen 2, Adreno 740 Vulkan acceleration, 12 GB RAM, Android 14+ Termux ARM64.
- **Legacy x86 Workstations (e.g. Apple MacBook)**: Intel Core 2 Duo P7550 @ 2.26 GHz, 3.6 GiB RAM, Debian 13 Trixie x86-64 (guaranteed Penryn-safe SSE4.1 execution).
- **Modern PCs & Laptops**: Linux x86-64, Windows Subsystem for Linux (WSL2), or native environments.

There are **no hardcoded nodes or fixed roles**. The TUI model browser allows you to choose which connected device runs the model with a single keystroke, and you can chat or monitor the cluster from any connected device.

---

## Symmetric Peer Mesh Architecture

```mermaid
flowchart TD
    subgraph Nexus Peer Mesh
        subgraph Node A: Android Phone / Termux
            A1[nexus / nexusd]
            A2[Vulkan Adreno 740 / CPU]
            A3[Android LMK 75% Guard]
            A1 --- A2
            A1 --- A3
        end

        subgraph Node B: Legacy x86 MacBook / Debian
            B1[nexus / nexusd]
            B2[Penryn SSE4.1 CPU Safe]
            B3[Memory Budget Cap Guard]
            B1 --- B2
            B1 --- B3
        end

        subgraph Node C: Workstation / Laptop
            C1[nexus / nexusd]
            C2[Interactive Unified Hub TUI]
            C3[Cluster Dashboard & Chat]
            C1 --- C2
            C1 --- C3
        end
    end

    Discovery((Zero-Config Discovery\nUDP 9999 Beacons / mDNS / Static IP))
    ControlPlane((Control Plane\nHTTP /cluster/model/load))
    Inference((OpenAI API Stream\n/v1/chat/completions))

    Node A <--> Discovery
    Node B <--> Discovery
    Node C <--> Discovery

    Node C ===|1. Target Node Dispatch| ControlPlane ===> Node A
    Node C ===|2. Streaming SSE Chat| Inference ===> Node A
    Node A -.->|3. Sequential Layer Offload| Node B
```

---

## Key Features

1. **In-TUI Target Node Selection**:
   In the Models browser (`F2`), selecting any model and pressing `[Enter]` opens an interactive modal dialog listing the local system and all discovered peers with their hardware specs (RAM, backend, GPU). Choose which device executes the model on the fly.
2. **Universal Chat from Any Device**:
   Chat with your models from any device in the mesh. Switch endpoints at any time in the Chat tab using the `[C]` peer connect hotkey.
3. **Dynamic Capability & Headroom Discovery**:
   Hardware memory budgets and thread allocations are resolved dynamically via `SystemProfile::probe()`. No hardcoded node identities, memory limits, or device assumptions.
4. **Hardware Safety Guards**:
   - **Legacy x86 Hardware Safety (Mac Core 2 Duo Penryn)**: Strictly enforces `-avx,-avx2,-fma,-sse4.2` in `.cargo/config.toml` to guarantee that code will never emit instructions that crash Penryn CPUs with `SIGILL`.
   - **Android LMK Memory Guard**: Enforces `(Model File Size + Exact KV Cache) < 0.75 * MemAvailable` to prevent Android's Low Memory Killer from sending `SIGKILL` (Signal 9).
5. **Distributed Sequential Layer Offload**:
   When a model exceeds the target host's safe headroom, Nexus automatically offloads trailing transformer layers sequentially (`--split-mode layer`) to secondary RPC worker nodes.
6. **Remote Control Plane**:
   Standardized `/cluster/model/load` and `/cluster/model/unload` endpoints enable any node to supervise, hot-swap, or stop models across the cluster.
7. **Hybrid Transport Fallback**:
   Transparent support for Wi-Fi LAN, direct IP, static peer fallback, and zero-latency USB cable tethering via automated ADB port forwarding.

---

## Setup Across Different Systems

### 1. Android (Samsung Galaxy S23 Ultra / Termux ARM64)

#### Prerequisites
Install the required toolchains inside Termux:
```bash
pkg update && pkg install -y rust clang git libllvm vulkan-tools
```

#### Build & Run
```bash
# Clone the repository
git clone https://github.com/<YOUR_USER>/nexus-llm.git ~/nexus-llm
cd ~/nexus-llm

# Build release binaries
cargo build --release

# Download a model (e.g. Qwen 2.5 Coder 1.5B or 7B)
./target/release/nexus download \
  "https://huggingface.co/Qwen/Qwen2.5-Coder-1.5B-Instruct-GGUF/resolve/main/qwen2.5-coder-1.5b-instruct-q4_k_m.gguf" \
  -o ~/nexus-models/qwen2.5-coder-1.5b.gguf

# Option A: Run the interactive Unified Hub TUI
./target/release/nexus

# Option B: Run as a headless daemon ready to accept remote model launches
./target/release/nexusd

# Option C: Run as an RPC compute worker
./target/release/nexus rpc --port 50052
```

---

### 2. Linux / Legacy x86 Workstations (e.g. Apple MacBook / Debian 13)

The build configuration in `.cargo/config.toml` automatically configures rustflags to disable AVX, AVX2, FMA, and SSE4.2, guaranteeing clean execution on older Intel Core 2 Duo (Penryn) processors.

#### Prerequisites
```bash
sudo apt update && sudo apt install -y cargo rustc git build-essential
```

#### Build & Run
```bash
# Clone the repository
git clone https://github.com/<YOUR_USER>/nexus-llm.git ~/nexus-llm
cd ~/nexus-llm

# Build release binaries (guarded by Penryn baseline flags)
cargo build --release

# Option A: Launch the full Unified Hub TUI
./target/release/nexus

# Option B: Launch directly into streaming chat
./target/release/nexus client

# Option C: Join the cluster as an RPC compute worker
./target/release/nexus rpc --port 50052
```

---

### 3. Windows & Windows Subsystem for Linux (WSL2)

> [!IMPORTANT]
> **Windows Host Environment Note**: Native Windows PowerShell typically lacks Rust/Cargo in its system PATH. All Cargo building, linting, and testing should be run inside **WSL2** using a login shell.

#### Building & Running in WSL2
From PowerShell:
```powershell
# Open WSL bash in the repo directory
wsl bash -l -c "cd /mnt/c/nexus-llm && cargo build --release"

# Run tests
wsl bash -l -c "cd /mnt/c/nexus-llm && cargo test"

# Launch the Unified Hub inside WSL terminal
wsl bash -l -c "cd /mnt/c/nexus-llm && ./target/release/nexus"
```

#### Direct Native Windows Execution
If you have a native Rust toolchain installed on Windows:
```powershell
cargo build --release
.\target\release\nexus.exe
```

---

## Interactive Interfaces & Unified Hub

Starting `nexus` with no arguments launches the full-screen Ratatui Unified Hub:
```bash
nexus
```

### Views & Navigation
Use `F1` - `F5`, `Tab`, or `Shift+Tab` to navigate between views:

- **`[F1: 💬 Chat]`**: Full-screen streaming conversation with auto-scroll and multi-turn history.
  - Press `[C]` to switch the chat endpoint to any discovered node or host.
  - Press `[Esc]` to abort active streaming generation.
- **`[F2: 📦 Models]`**: Split-pane model browser and zero-copy GGUF inspector.
  - Displays file size, quantization type, context length, KV cache overhead, and memory badges (`[OK]`, `[RPC]`, `[OOM]`).
  - Press `[Enter]` to open the **Target Node Selection Modal**: choose to load the model locally or dispatch it to any connected peer in the mesh.
- **`[F3: 🖥️ Dashboard]`**: Cluster performance monitor displaying CPU load, RAM utilization, Vulkan/GPU status, and active peer nodes.
- **`[F4: 🔗 USB Tunnel]`**: Live ADB USB status monitor, one-key port forwarding (`F`), and tunnel teardown (`T`).
- **`[F5: ⚙️ Settings]`**: Live configuration editor for GPU layer offload, CPU threads, context window, and RPC limits. Toggle with `Space`, adjust with `Left`/`Right`, save with `S`.

### Safe Model Hot-Swapping
When a model is already active, selecting another model and confirming will gracefully terminate the active process, re-evaluate target device memory headroom, and load the new model without needing to restart the hub.

---

## Standalone Subcommands

For headless deployments, scripts, and dedicated worker nodes:

```bash
# Launch direct TUI chat
nexus client

# Batch prompt execution (pipes or automation)
nexus client --prompt "Explain Rust lifetimes in two sentences."

# Start headless supervisor daemon
nexusd

# Start dedicated RPC compute worker
nexus rpc --port 50052

# Zero-copy GGUF inspection
nexus inspect -m ~/nexus-models/model.gguf -c 4096

# Download a model with resume & SHA-256 validation
nexus download <URL> -o ~/nexus-models/model.gguf

# Manage ADB USB tunnel
nexus tunnel setup
nexus tunnel status
nexus tunnel teardown
```

---

## Peer Discovery & Connection Methods

1. **Autonomous Zero-Config Discovery (Default)**:
   Nodes broadcast periodic 64-byte UDP beacons over port `9999` and advertise services via mDNS. Devices discover each other automatically on the local network.
2. **Direct Host / Peer Specification**:
   If network UDP broadcast is restricted:
   ```bash
   nexus client --host http://<PEER_IP>:8080
   ```
3. **Persistent Config Fallback**:
   In `~/.nexus/config.toml`:
   ```toml
   [network]
   default_host = "http://<PEER_IP>:8080"
   static_peers = ["192.168.1.50", "192.168.1.55"]
   ```
4. **USB Cable / ADB Port Forwarding**:
   Connect via USB cable with USB debugging enabled for ultra-low latency:
   ```bash
   nexus tunnel setup
   nexus client --host http://localhost:8080
   ```

---

## Repository Structure

```text
nexus-llm/
├── Cargo.toml               # Package targets and dependencies
├── .cargo/
│   └── config.toml          # Target compiler flags (Snapdragon vs Penryn SSE4.1)
├── AGENTS.md                # System guidelines and architecture rules
├── DESIGN_SPEC.md           # Network protocols and binary packet layout
├── BUILD_PLAN.md            # Phased execution milestones
├── AGENT_LEARNINGS.md       # Operational lessons and environment notes
├── README.md                # Setup and user documentation
├── presets/
│   ├── coder.yaml           # Systems programming persona (ChatML)
│   └── general.yaml         # Conversational assistant persona (Llama-3)
├── src/
│   ├── main.rs              # CLI router, default Unified Hub entry point
│   ├── daemon.rs            # Headless supervisor daemon (nexusd)
│   ├── config.rs            # TOML configuration engine (~/.nexus/config.toml)
│   ├── sysinfo.rs           # /proc parser & Android LMK memory guard
│   ├── supervisor.rs        # Asynchronous llama-server process manager
│   ├── control_plane.rs     # Remote model load/unload dispatch protocol
│   ├── discovery.rs         # 64-byte UDP beacon protocol & peer cache
│   ├── peer_registry.rs     # Dynamic peer lifecycle and state management
│   ├── mdns.rs              # Zero-config mDNS service discovery
│   ├── client.rs            # OpenAI HTTP/SSE streaming client
│   ├── cluster.rs           # Distributed RPC layer pipelining coordinator
│   ├── tunnel.rs            # ADB USB port forwarding & reverse supervisor
│   ├── gguf.rs              # Zero-copy GGUF v2/v3 metadata parser
│   ├── downloader.rs        # Chunked HTTP resume downloader with SHA-256
│   ├── preset.rs            # YAML persona & prompt formatting templates
│   └── ui/
│       ├── hub.rs           # Unified interactive Ratatui Hub controller
│       ├── chat.rs          # Ratatui split-screen streaming chat view
│       ├── models_view.rs   # Split-pane model browser & GGUF inspector
│       ├── settings_view.rs # In-app configuration editor
│       ├── tunnel_view.rs   # Interactive USB ADB tunnel monitor
│       ├── dashboard.rs     # Cluster performance monitor TUI
│       └── models.rs        # Local model directory scanner
└── tests/
    ├── test_phase1.rs       # System profiling & memory guard test suite
    ├── test_discovery.rs    # UDP 9999 beacon & SSE stream test suite
    ├── test_phase3_network.rs # Control plane & peer registry test suite
    ├── test_cluster_rpc.rs  # Distributed RPC layer offload & ADB tests
    ├── test_gguf_metadata.rs# GGUF parsing & persona templates test suite
    ├── test_ui.rs           # Headless Ratatui widget render test suite
    └── test_hub_ui.rs       # Unified Hub, navigation, and settings test suite
```

---

## Verification & Automated Test Suite

All 60 automated tests pass deterministically across all supported platforms:

```bash
# Run complete test suite (in WSL or Linux)
cargo test
```

### Test Suite Breakdown
| Test Suite | Tests | Description |
| :--- | :---: | :--- |
| `test_phase1` | 17 | Memory guard, system profiling, and supervisor preflight |
| `test_discovery` | 9 | UDP beacon protocol, CRC-16, and peer caching |
| `test_phase3_network` | 7 | Control plane model dispatch, peer registry, and security policies |
| `test_gguf_metadata` | 7 | GGUF parsing, exact KV cache calculation, and chat presets |
| `test_cluster_rpc` | 8 | Dynamic cluster budgeting, layer offload, and RPC allocation caps |
| `test_hub_ui` | 7 | Unified Hub tab cycling, settings mutations, and target node selection modal |
| `test_ui` | 5 | Headless chat streaming, token rendering, and dashboard monitor |
| **Total** | **60** | **100% Pass Rate** |

---

## Troubleshooting

### 1. "No host peer discovered on the network"
- Ensure both devices are connected to the same Wi-Fi subnet.
- Check if your router enforces client isolation (blocks UDP broadcast). If so, specify the IP address with `--host http://<IP>:8080` or use an ADB USB cable (`nexus tunnel setup`).

### 2. Android Termux terminates `llama-server` unexpectedly
- Verify available RAM with `./target/release/nexus inspect -m <model>`.
- The Android LMK guard protects against models where `Model Size + KV Cache > 0.75 * MemAvailable`. If memory is tight, reduce context length (`-c 2048`) or offload layers to an RPC worker.

### 3. Mac shows `SIGILL (Illegal Instruction)`
- This occurs if code was compiled with modern CPU instructions (AVX/AVX2/FMA/SSE4.2). Verify `.cargo/config.toml` includes `-C target-feature=-avx,-avx2,-fma,-sse4.2` and rebuild with `cargo build --release`.

### 4. PowerShell "command not found: cargo"
- Windows PowerShell may not have Rust/Cargo in PATH. Execute all cargo commands through WSL: `wsl bash -l -c "cd /mnt/c/nexus-llm && cargo test"`.
