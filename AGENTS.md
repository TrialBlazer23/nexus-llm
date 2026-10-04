AGENTS.md - Google Antigravity Agent Guidelines for Nexus-LLM
Project Identity & Target Scope
Nexus-LLM is an ultra-lightweight distributed LLM orchestrator and terminal interface written in Rust. It runs across two asymmetric hardware targets:
Node A (Primary Engine): Samsung Galaxy S23 Ultra (Snapdragon 8 Gen 2, 12GB RAM, Android 14+ Termux ARM64).
Node B (Workstation/Client): Apple MacBook (macrowave, Intel Core 2 Duo P7550 @ 2.26GHz, 3.6 GiB RAM, Debian 13 Trixie x86-64).
Development begins natively inside Termux on Node A, followed by cloning, building, and running on Debian 13 on Node B.
Non-Negotiable Engineering Directives
1. Legacy x86 Hardware Safety (Mac Core 2 Duo P7550)
NEVER emit or enable AVX, AVX2, FMA, F16C, POPCNT, or SSE4.2 instructions in build configurations or C dependencies. The Intel P7550 is a Penryn-generation CPU that will crash with SIGILL on any instruction newer than SSE4.1.
In .cargo/config.toml, configure the x86_64 target baseline explicitly:
[target.x86_64-unknown-linux-gnu]
rustflags = ["-C", "target-feature=-avx,-avx2,-fma,-sse4.2"]


When generating CMake scripts for llama.cpp on the legacy node, always include: -DGGML_AVX=OFF -DGGML_AVX2=OFF -DGGML_FMA=OFF -DGGML_NATIVE=OFF -DCMAKE_C_FLAGS="-march=x86-64 -msse4.1 -mno-sse4.2 -mno-popcnt -mno-avx -mno-avx2".
2. Android Bionic & Memory Safety (Galaxy S23 Ultra)
Avoid glibc assumptions: Code targeting ARM64 must link cleanly against Android's Bionic libc.
Android LMK Guard: Android terminates processes with SIGKILL (Signal 9) if available memory runs low. The application must read /proc/meminfo and block model loads where Model Size + KV Cache > 0.75 * MemAvailable.
Take advantage of Snapdragon 8 Gen 2 hardware acceleration by configuring armv8.2-a+dotprod+i8mm.
3. Asymmetric Compute Rules
Do not attempt tensor-parallel splitting (--split-mode row) over network sockets. Always default to sequential layer pipelining (--split-mode layer).
Models that fit within Node A's memory budget (up to 8.5 GB) must run 100% locally on Node A. Never offload layers to Node B unless Node A's RAM capacity is exceeded.
Node B's total RAM allocation for rpc-server must never exceed 1800 MB to keep Debian 13 and Xfce responsive.
4. Dependency Constraints
Rely on standard Rust crates: tokio (runtime), ratatui + crossterm (TUI), reqwest (HTTP client), serde + toml + serde_yaml (serialization), crc16 (checksums).
Do not introduce complex C/C++ bindings into the Rust codebase; interface with llama-server and rpc-server via non-blocking sub-processes and standard network protocols.
Workspace Directory Layout
~/local-server/
├── Cargo.toml
├── .cargo/
│   └── config.toml
├── AGENTS.md
├── DESIGN_SPEC.md
├── BUILD_PLAN.md
├── src/
│   ├── main.rs         # CLI entry point & TUI router
│   ├── daemon.rs       # Headless supervisor binary (nexusd)
│   ├── config.rs       # TOML configuration parser
│   ├── sysinfo.rs      # /proc parser & memory safety guard
│   ├── supervisor.rs   # Subprocess manager for llama.cpp
│   ├── discovery.rs    # UDP 9999 beacon & ARP scanner
│   ├── client.rs       # OpenAI HTTP/SSE client
│   ├── gguf.rs         # Zero-copy GGUF header parser
│   ├── downloader.rs   # Chunked HTTP resume engine
│   ├── preset.rs       # YAML persona & chat template engine
│   ├── cluster.rs      # Distributed RPC coordinator
│   ├── tunnel.rs       # ADB forward/reverse supervisor
│   └── ui/
│       ├── chat.rs      # Interactive streaming TUI
│       ├── dashboard.rs # Cluster performance monitor
│       └── models.rs    # Local model browser
└── presets/
    ├── coder.yaml
    └── general.yaml


Agent Operational Directives
Non-Interactive Execution: Never execute interactive commands (e.g., nano, vim, interactive prompts). Run all tooling in non-interactive batch mode.
Phase Boundary Enforcement: Follow the phased milestones outlined in BUILD_PLAN.md. Do not implement code planned for Phase 3 or Phase 4 during Phase 1.
Verification Discipline: After generating or editing code, run cargo check and the phase's designated unit tests to confirm compilation and memory assertions pass before marking a task complete.
Target Architecture Verification: Ensure any build instructions emitted or invoked check the current architecture (uname -m) to apply the appropriate Snapdragon or Penryn compiler flags.
Target Build & Test Reference
Termux (Node A - Galaxy S23 Ultra)
# Verify syntax & compilation
cargo check --target aarch64-linux-android
cargo test

# Build optimized binary
cargo build --release


Debian 13 Trixie (Node B - Mac Core 2 Duo)
# Verify clean compilation without modern instruction dependencies
cargo check --target x86_64-unknown-linux-gnu
cargo test

# Run interactive TUI client connecting to Node A
cargo run --release -- client --host <NODE_A_IP>:8080


