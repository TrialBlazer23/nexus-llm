# Nexus-LLM

Nexus-LLM is a distributed LLM orchestrator, symmetric peer mesh, multi-model execution router, headless compute daemon, and terminal/web interface written in Rust.

It coordinates heterogeneous personal devices into a decentralized local inference network where connected nodes can serve as an **Inference Host**, an **RPC Worker**, an **Orchestration Router**, or an **Interactive Client**:

- **Android Devices (e.g. Samsung Galaxy S23 Ultra)**: Qualcomm Snapdragon 8 Gen 2, Adreno 740 Vulkan acceleration, 12 GB RAM, Android 14+ Termux ARM64.
- **Legacy x86 Workstations (e.g. Apple MacBook)**: Intel Core 2 Duo P7550 @ 2.26 GHz, 3.6 GiB RAM, Debian 13 Trixie x86-64 (guaranteed Penryn-safe SSE4.1 execution, zero AVX/AVX2/FMA/SSE4.2 opcodes).
- **Modern PCs & Laptops**: Linux x86-64, Windows Subsystem for Linux (WSL2), or native environments.

Hardware capabilities, memory budgets, and link latencies are probed dynamically at runtime. You can dispatch models across devices from the Terminal User Interface (TUI) or embedded Web Interface, stream generations from any node, offload layers across devices, or run large Mixture-of-Experts (MoE) models directly on mobile storage.

---

## Quickstart

Nexus-LLM is a Rust orchestrator that supervises external inference backends (`llama-server`, `rpc-server`, and `bmoe-cli`). The bootstrap script detects your platform, installs dependencies, compiles backends with hardware-safe flags, and places binaries in `~/.nexus/bin`.

### Linux / macOS / Android Termux

```bash
git clone https://github.com/TrialBlazer23/nexus-llm.git ~/nexus-llm
cd ~/nexus-llm
bash scripts/setup.sh
export PATH="$HOME/.nexus/bin:$PATH"

# Run system diagnostics & security check
nexus doctor

# Launch full-screen interactive TUI
nexus
```

### Windows (PowerShell → WSL2)

```powershell
git clone https://github.com/TrialBlazer23/nexus-llm.git C:\nexus-llm
cd C:\nexus-llm
.\scripts\setup.ps1
```

### Common Commands

```bash
# Launch interactive TUI Hub (default)
nexus

# Launch embedded Web Interface & OpenAI Gateway (:8090)
nexus web

# Run headless supervisor daemon
nexusd

# Run as a dedicated RPC compute worker to receive offloaded layers
nexus worker --port 50052 --mem 1800

# Connect to a compute host and stream a prompt
nexus client --prompt "Explain Rust ownership in two sentences."
nexus client --host http://192.168.1.100:8080

# Check memory safety for a model and context length
nexus check -m ~/.nexus/models/model.gguf -c 4096

# Inspect GGUF metadata, architecture, and tensors without loading weights
nexus inspect -m ~/.nexus/models/model.gguf -c 4096

# Download a GGUF model with HTTP Range resume and validation
nexus download <URL> -o ~/.nexus/models/model.gguf

# Pair with a remote node using its rotating 6-digit code
nexus pair --host http://192.168.1.50:9998 --code 123456

# Run preflight system, hardware, and security diagnostics
nexus doctor
```

---

## MoE Flash Streaming (BigMoeOnEdge Integration)

### Running Models Larger than Physical RAM

Mixture-of-Experts (MoE) architectures achieve high capacity through sparse expert routing. However, running large MoE models like `Qwen3-Coder-30B-A3B-Instruct` (an 18.55 GB GGUF model with 48 layers and 128 experts) on a memory-constrained device such as a 12 GB smartphone typically causes out-of-memory crashes: standard dense execution maps all weights into RAM, exceeding system limits and triggering immediate termination by the Android Low Memory Killer (LMK).

Nexus-LLM integrates MoE flash streaming by supervising the [`bmoe-cli`](https://github.com/Helldez/BigMoeOnEdge) engine:
1. **Dynamic Expert LRU Caching**: Only active expert weights are loaded into an in-memory cache (e.g. 2.0 GB RAM) on demand.
2. **Flash Row Streaming**: Non-resident weights are streamed directly from fast flash storage (e.g. UFS 4.0) with asynchronous I/O overlapping compute and transfer.
3. **MoE Stream LMK Guard**: Nexus dynamically calculates memory headroom as `Resident Weights + Expert Cache + KV Cache Overhead` rather than total file size, allowing models larger than physical RAM to run safely without triggering OS termination.
4. **OpenAI / SSE Streaming Adapter**: Nexus translates internal `bmoe-cli` token events into standard OpenAI-compatible Server-Sent Events (SSE) for transparent client integration.

### Hardware Verification

| Parameter | Observed Metric |
|---|---|
| **Hardware** | Samsung Galaxy S23 Ultra (Snapdragon 8 Gen 2, 12 GB RAM) |
| **Operating System** | Android 14 via Termux aarch64 |
| **Model** | `Qwen3-Coder-30B-A3B-Instruct-Q4_K_M.gguf` (18.55 GB) |
| **Resident Footprint** | ~2.0 GB LRU expert cache + ~950 MB buffers (< 3.5 GB total) |
| **Throughput** | ~2.35 tokens/sec sustained decode |
| **Stability** | 0 major page faults, sustained execution within Android LMK limits |

*Credit to [Helldez](https://github.com/Helldez) and the [BigMoeOnEdge](https://github.com/Helldez/BigMoeOnEdge) project for pioneering flash-streaming MoE execution on commodity edge hardware.*

---

## Mesh Architecture

```mermaid
flowchart TD
    subgraph Nexus Peer Mesh
        subgraph Node A: Android Device / Termux
            A1[nexus / nexusd]
            A2[Vulkan Adreno 740 / CPU]
            A3[BigMoe Flash Streaming bmoe-cli]
            A4[Android LMK 75% Guard]
            A1 --- A2
            A1 --- A3
            A1 --- A4
        end

        subgraph Node B: Legacy x86 Workstation / Debian
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
    ControlPlane((Control Plane\nHTTP :9998 / Ed25519 Signatures))
    Gateway((Mesh Gateway\nOpenAI API :8090))

    Node A <--> Discovery
    Node B <--> Discovery
    Node C <--> Discovery

    Node C ===|1. Authenticated Load Dispatch| ControlPlane ===> Node A
    Node C ===|2. Streaming Chat / Completions| Gateway ===> Node A
    Node A -.->|3. Sequential Layer Offload| B4
```

### Core Components

1. **Terminal Interface (Ratatui TUI)**: Full-screen terminal hub supporting Chat (`F1`), Models catalog (`F2`), Cluster telemetry (`F3`), Settings (`F4`), ADB USB Tunnel (`F5`), Agent bus (`F6`), Diagnostic logs (`F7`), and fuzzy command palette (`Ctrl+P`).
2. **Embedded Web Hub & Gateway (`nexus web`)**: Zero-dependency web UI embedded directly in the binary, served on port `8090` with 6-digit PIN authentication.
3. **Two-Tier Router (`src/router.rs`)**:
   - *Tier 1 (Deterministic)*: Fast routing based on configurable keywords and YAML presets (`presets/*.yaml`).
   - *Tier 2 (Orchestrator-Assisted)*: Complex queries route to an orchestrator model for classification into structured JSON.
   - *Application Veto*: Rust runtime enforces validation against actual node capabilities, rejecting invalid model assignments.
4. **Sequential Layer Pipelining (`src/cluster/`)**: Offloads model layers across devices using `llama.cpp` `rpc-server` with `--split-mode layer`.
5. **Zero-Config Discovery**: 64-byte UDP broadcast beacons over port `9999` with CRC-16-CCITT, mDNS-SD service discovery, and static IP fallback.
6. **Embedded Knowledge Base (`src/kb/`)**: Single-file transactional `redb` database storing content-addressed document chunks, versioned personas, and vector embeddings.

---

## Security & Network Posture

Nexus-LLM is engineered primarily for trusted local area networks (LANs). Please review our full [SECURITY.md](SECURITY.md) for details on threat modeling and configuration.

### Network Transport & Pairing Defaults

- **Cleartext Transport**: HTTP and WebSocket communication is unencrypted across the local network. Chat prompts, model weights, and knowledge-base data traverse the LAN in cleartext. Do not expose control or inference ports directly to untrusted public networks without a secure tunnel (e.g. WireGuard or SSH).
- **Default Open LAN Mesh**: By default (`require_pairing = false`), discovered LAN nodes can query status and dispatch models without cryptographic verification to allow zero-configuration setup.
- **Signed Security Mode**: When pairing is enforced (`require_pairing = true` in `config.toml` or after running `nexus pair`):
  - Every node uses an Ed25519 keypair (`~/.nexus/node_key.json` created with `0600` permissions).
  - Control plane requests require valid Ed25519 signatures, timestamp replay protection (±120s tolerance), and unique nonces.
  - Node trust is established out-of-band using rotating 6-digit PINs with progressive exponential backoff against brute-force attacks.
- **Diagnostic Verification**: Run `nexus doctor` to audit your security posture. It reports pairing enforcement, file permissions, and interface binding alerts.

---

## Hardware Safety & Baseline Enforcements

- **Legacy x86 Penryn Safety**: Explicitly disables AVX, AVX2, FMA, and SSE4.2 in `.cargo/config.toml` to guarantee code will not crash Penryn Core 2 Duo CPUs with `SIGILL`. Release binaries are scanned with `scripts/check_penryn_opcodes.sh`.
- **Android Low Memory Killer Guard**: Enforces `(Model Size + KV Cache) < 0.75 * MemAvailable` to prevent Android from terminating the inference process.
- **Worker Allocation Caps**: Nodes acting as RPC workers enforce safe allocation limits (e.g. 1800 MB on 4GB systems) to keep host operating systems responsive.
- **Battery Safety Threshold**: Halts or rejects background operations when device battery levels drop below configured safety limits.

---

## Configuration (`~/.nexus/config.toml`)

Configuration is persisted to `~/.nexus/config.toml` with live hot-reloading:

```toml
[node]
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

[security]
require_pairing = false                       # set true to enforce Ed25519 signed requests
allow_unpaired_lan = true                     # warn via nexus doctor if true

[inference]
threads = 4
context_size = 4096
gpu_layers = 99                               # set 0 for CPU-only mode
fallback_to_cpu = true

[inference.moe]
enabled = true
cache_mb = "auto"                             # planner assigns expert cache under the LMK; 0 is off
cache_ceil_mb = 0                             # 0 = no operator cap; a positive MiB value is a hard ceiling

[inference.cache]
enabled = true
max_cache_mb = 4096                           # prompt cache slot persistence quota

[hardware.safety]
battery_threshold_percent = 15                # throttle heavy loads on low battery

[hf]
token = ""                                    # optional Hugging Face token for gated models
```

---

## Verification & Test Suite

The test suite includes 25 test suites, 250+ unit and integration tests, and adversarial property tests:

```bash
# Run unit, integration, and property test suites
cargo test --locked

# Check formatting and strict lints
cargo fmt --all -- --check
cargo clippy --locked --all-targets -- -D warnings

# Run security and system diagnostics
cargo run --locked --bin nexus -- doctor

# Scan release binary to guarantee no illegal Penryn opcodes (AVX/AVX2/FMA/SSE4.2)
bash scripts/check_penryn_opcodes.sh target/release/nexus
```

---

## Troubleshooting

### 1. "No host peer discovered on the network"
- Ensure devices are connected to the same Wi-Fi subnet.
- If your router blocks UDP broadcast (client isolation), add static peers in `config.toml` (`static_peers = ["192.168.1.50"]`) or connect via USB cable using ADB tethering (`nexus tunnel setup`).

### 2. Android Termux terminates `llama-server` (Low Memory Killer)
- Verify available RAM with `nexus inspect -m <model>`.
- The Android LMK guard blocks model loads when `(Model Size + KV Cache) > 0.75 * MemAvailable`. Reduce context length (`-c 2048`), offload layers to an RPC worker, or use MoE flash streaming (`bmoe-cli`) for Mixture-of-Experts architectures.

### 3. macOS / Linux shows `SIGILL (Illegal Instruction)`
- Occurs if code was compiled with modern CPU instructions unsupported by older hardware. Verify `.cargo/config.toml` includes `-C target-feature=-avx,-avx2,-fma,-sse4.2` and rebuild with `cargo build --release`.

### 4. PowerShell "command not found: cargo"
- Windows PowerShell does not have Rust/Cargo in PATH by default. Execute Cargo commands inside WSL: `wsl bash -l -c "cd /mnt/c/nexus-llm && cargo test"`.

---

## Acknowledgments

- **[Helldez](https://github.com/Helldez)** for **[BigMoeOnEdge](https://github.com/Helldez/BigMoeOnEdge)**: Pioneered flash-streaming MoE execution, expert LRU caching, and low-latency storage streaming for edge devices.
- **[Georgi Gerganov](https://github.com/ggerganov)** & the **[llama.cpp](https://github.com/ggerganov/llama.cpp)** / **[ggml](https://github.com/ggerganov/ggml)** community for portable local LLM inference and the RPC server.
- **[Ratatui](https://github.com/ratatui/ratatui)** & **[crossterm](https://github.com/crossterm-rs/crossterm)** for terminal UI infrastructure in Rust.
- **[redb](https://github.com/cberner/redb)** for the embedded pure-Rust transactional database.
