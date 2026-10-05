AGENTS.md - Google Antigravity Agent Guidelines for Nexus-LLM

# Host Workstation Tooling Directive (CRITICAL)
**PowerShell lacks Cargo / Rust in PATH.** The Windows host environment does not have Rust/Cargo installed in the native PowerShell environment.
**ALL Cargo commands (`cargo check`, `cargo test`, `cargo build`, etc.) MUST be executed inside WSL using a login shell:**
```powershell
wsl bash -l -c "cd /mnt/c/nexus-llm && cargo <command>"
```
Do NOT attempt to run `cargo` directly in PowerShell.

---

# Project Identity & Target Scope
Nexus-LLM is an ultra-lightweight distributed LLM orchestrator and terminal interface written in Rust. It runs across a **heterogeneous N-node symmetric peer mesh** where any connected device can act as an Inference Host, an RPC Worker, or an Interactive TUI Client:
- **Android Targets (e.g. Galaxy S23 Ultra)**: Snapdragon 8 Gen 2, Adreno 740 Vulkan acceleration, 12GB RAM, Android 14+ Termux ARM64.
- **Legacy x86 Workstations (e.g. Apple MacBook)**: Intel Core 2 Duo P7550 @ 2.26GHz, Debian 13 Trixie x86-64.
- **Modern PCs / Workstations**: Windows native PowerShell, Windows WSL2, Linux x86-64.

Every node can advertise its capabilities via zero-configuration service discovery and the binary heartbeat. Operators can launch models on any connected device directly from the TUI and chat from any device.

---

# Non-Negotiable Engineering Directives

## 1. Legacy x86 Hardware Safety (Penryn / Core 2 Duo)
NEVER emit or enable AVX, AVX2, FMA, F16C, POPCNT, or SSE4.2 instructions in build configurations or C dependencies. The Intel P7550 is a Penryn-generation CPU that will crash with `SIGILL` on any instruction newer than SSE4.1.
In `.cargo/config.toml`, configure the x86_64 target baseline explicitly:
```toml
[target.x86_64-unknown-linux-gnu]
rustflags = ["-C", "target-feature=-avx,-avx2,-fma,-sse4.2"]
```
When generating CMake scripts for llama.cpp on legacy nodes, always include:
`-DGGML_AVX=OFF -DGGML_AVX2=OFF -DGGML_FMA=OFF -DGGML_NATIVE=OFF -DCMAKE_C_FLAGS="-march=x86-64 -msse4.1 -mno-sse4.2 -mno-popcnt -mno-avx -mno-avx2"`

## 2. Android Bionic & Memory Safety
- **Avoid glibc assumptions**: Code targeting ARM64 must link cleanly against Android's Bionic libc.
- **Android LMK Guard**: Android terminates processes with `SIGKILL` (Signal 9) if available memory runs low. The application must read `/proc/meminfo` and block model loads where `Model Size + KV Cache > 0.75 * MemAvailable`.
- **Hardware Acceleration**: Take advantage of Snapdragon 8 Gen 2 hardware acceleration by configuring `armv8.2-a+dotprod+i8mm` and Vulkan Adreno offload.

## 3. Dynamic Distributed Compute Rules
- **No Hardcoded Device Constants**: Do not hardcode static RAM limits or fixed node roles. Memory budgets are determined dynamically via `SystemProfile::probe()` and local configuration.
- **Sequential Layer Pipelining Only**: Do not attempt tensor-parallel splitting (`--split-mode row`) over network sockets. Always default to sequential layer pipelining (`--split-mode layer`).
- **Target Node Autonomy**: Models that fit within the chosen target node's safe memory budget run 100% locally on that node. Offload layers to secondary RPC workers only when the target node's safe capacity is exceeded.
- **Worker Allocation Caps**: Nodes acting as RPC workers must enforce safe memory allocation caps (e.g. 1800 MB on memory-constrained 4GB nodes) to keep host OS and desktop environments responsive.

## 4. Dependency Constraints
Rely on standard Rust crates: `tokio` (runtime), `ratatui` + `crossterm` (TUI), `reqwest` (HTTP client), `serde` + `toml` + `serde_yaml` (serialization), `crc16` (checksums), `mdns-sd` (LAN discovery).
Do not introduce complex C/C++ bindings into the Rust codebase; interface with `llama-server` and `rpc-server` via non-blocking sub-processes and standard network protocols.

---

# Workspace Directory Layout
```text
~/local-server/
├── Cargo.toml
├── .cargo/
│   └── config.toml
├── AGENTS.md
├── AGENT_LEARNINGS.md
├── DESIGN_SPEC.md
├── BUILD_PLAN.md
├── NETWORK_EXPANSION_FINDINGS.md
├── src/
│   ├── main.rs            # CLI entry point & TUI router
│   ├── daemon.rs          # Headless supervisor binary (nexusd)
│   ├── config.rs          # TOML configuration parser
│   ├── sysinfo.rs         # /proc parser & memory safety guard
│   ├── supervisor.rs      # Subprocess manager for llama.cpp
│   ├── discovery.rs       # UDP 9999 beacon & ARP scanner
│   ├── mdns.rs            # mDNS-SD zero-config discovery
│   ├── peer_registry.rs   # Dynamic peer lifecycle registry
│   ├── control_plane.rs   # Remote execution & handshake protocol
│   ├── client.rs          # OpenAI HTTP/SSE client
│   ├── gguf.rs            # Zero-copy GGUF header parser
│   ├── downloader.rs      # Chunked HTTP resume engine
│   ├── preset.rs          # YAML persona & chat template engine
│   ├── cluster.rs         # Distributed RPC coordinator
│   ├── tunnel.rs          # ADB forward/reverse supervisor
│   └── ui/
│       ├── chat.rs         # Interactive streaming TUI
│       ├── dashboard.rs    # Cluster performance monitor
│       ├── hub.rs          # Full unified hub with tab routing
│       ├── models_view.rs  # Model browser with target node selector
│       ├── settings_view.rs# Live configuration editor
│       └── tunnel_view.rs  # ADB USB tunnel manager
└── presets/
    ├── coder.yaml
    └── general.yaml
```

---

# Agent Operational Directives
- **Non-Interactive Execution**: Never execute interactive commands (e.g., nano, vim, interactive prompts). Run all tooling in non-interactive batch mode.
- **Phase Boundary Enforcement**: Follow the phased milestones outlined in `BUILD_PLAN.md`.
- **Verification Discipline**: After generating or editing code, run cargo check and designated unit tests via WSL to confirm compilation and memory assertions pass before marking a task complete:
  ```powershell
  wsl bash -l -c "cd /mnt/c/nexus-llm && cargo test"
  ```
- **Target Architecture Verification**: Ensure build instructions check the target architecture (`uname -m`) to apply the appropriate Snapdragon or Penryn compiler flags.

---

# Maintainability and Quality Standards

Keep changes small, cohesive, and easy to review. Prefer clear, idiomatic Rust and existing project patterns over clever abstractions or speculative generalization. Before adding code, search for an existing helper, module, dependency, or documented decision that already solves the problem; extend or reuse it when appropriate rather than duplicating behavior.

Treat 250 lines as a maintainability guideline for source files, not a hard requirement. When a file grows beyond that guideline, split it along meaningful responsibility boundaries without scattering closely related logic across arbitrary files. Keep public interfaces narrow, preserve type safety, validate inputs at boundaries, and make failures explicit and actionable.

Every behavior change should include or update focused tests for the success path, important edge cases, and failure behavior. Run the smallest relevant formatter, lint, build, and test commands via WSL, then record the validation performed. Update directly related documentation and configuration examples when behavior or operator workflows change.

---

# Agent Learnings

Record durable lessons in [AGENT_LEARNINGS.md](AGENT_LEARNINGS.md). Add an entry when a bug, failed command, environment-specific behavior, research finding, or design decision could prevent future agents from repeating the same mistake. Keep entries concise, factual, and actionable. Do not record secrets, credentials, tokens, personal data, or noisy one-off status updates.

Use this structure for each entry:

```text
## YYYY-MM-DD — Short title
- Category: bug | failed-command | research | design-decision | environment
- Context: What was being attempted and where.
- Finding: What happened or what was learned.
- Action: The fix, workaround, or rule to follow.
- Verification: How the result was confirmed, or what remains unverified.
```

Prefer updating an existing entry when new evidence clarifies it. Otherwise append new entries in reverse chronological order.
