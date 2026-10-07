# Nexus-LLM: Phased Execution & Antigravity Build Plan

**Target Workspace:** `c:\nexus-llm` (accessible in WSL via `/mnt/c/nexus-llm`)  
**Tooling:** Rust 1.80+, Cargo, Google Antigravity CLI (`agy`)  
**Host Tooling Invariant:** Windows native PowerShell lacks Cargo in PATH. All Cargo commands MUST be run inside WSL:
```powershell
wsl bash -l -c "cd /mnt/c/nexus-llm && cargo <command>"
```

---

## Phase 1: System Introspection, Memory Safety Guard, & Process Supervisor
**Status:** Completed & Verified (17 tests passing)

### Objectives
- Implement `src/sysinfo.rs`: Parse `/proc/meminfo`, probe Vulkan & ARM dotprod, enforce dynamic 75% available memory ceiling.
- Implement `src/config.rs`: Parse `~/.nexus/config.toml` with default fallbacks and auto-generated UUID persistence.
- Implement `src/supervisor.rs`: Subprocess supervisor for `llama-server` with CPU fallback if Vulkan fails.

### Verification Command
```bash
wsl bash -l -c "cd /mnt/c/nexus-llm && cargo test --test test_phase1"
```

---

## Phase 2: Autonomous Discovery & SSE Streaming Engine
**Status:** Completed & Verified (9 tests passing)

### Objectives
- Implement `src/discovery.rs`: UDP broadcaster and listener on port 9999, binary 64-byte beacon encoder/decoder with CRC-16-CCITT, thread-safe peer cache.
- Implement `src/client.rs`: Asynchronous OpenAI-compatible HTTP client consuming `/v1/chat/completions` with Server-Sent Events (`reqwest` + `eventsource-stream`).

### Verification Command
```bash
wsl bash -l -c "cd /mnt/c/nexus-llm && cargo test --test test_discovery"
```

---

## Phase 3: Peer Registry, Control-Plane Validation, & Model Management
**Status:** Completed & Verified (13 tests passing)

### Objectives
- Implement `src/peer_registry.rs`: Event-driven registry tracking `Discovered`, `Verifying`, `Healthy`, `Stale`, `Removed`, and `Rejected` lifecycle states.
- Implement `src/control_plane.rs`: Validate structured JSON state response for node identity, protocol, readiness, capabilities, and allocatable memory.
- Implement `src/gguf.rs`: Zero-copy binary parser for GGUF headers and exact KV cache sizing.
- Implement `src/preset.rs`: YAML persona and chat template engine.
- Implement `src/downloader.rs`: Resumable chunked model downloader with SHA-256 validation.

### Verification Commands
```bash
wsl bash -l -c "cd /mnt/c/nexus-llm && cargo test --test test_phase3_network"
wsl bash -l -c "cd /mnt/c/nexus-llm && cargo test --test test_gguf_metadata"
```

---

## Phase 4: Interactive Terminal Interface (TUI)
**Status:** Completed & Verified (5 tests passing)

### Objectives
- Implement `src/ui/chat.rs` using `ratatui` + `crossterm`: Split-screen chat interface, streaming markdown rendering, scrolling history.
- Implement `src/ui/dashboard.rs`: Live cluster monitor displaying local telemetry gauges (RAM/LMK, thermal index, threads) and active cluster peers table.

### Verification Command
```bash
wsl bash -l -c "cd /mnt/c/nexus-llm && cargo test --test test_ui"
```

---

## Phase 5: Dynamic Multi-Node Cluster Offload & ADB Tunneling
**Status:** In Progress / Refactoring from Static to Dynamic Mesh

### Objectives
- Refactor `src/cluster.rs`:
  - Eliminate hardcoded `NODE_A_MAX_STANDALONE_MB` and `NODE_B_MAX_RPC_RAM_MB` constants.
  - Dynamically calculate cluster memory budgets from the target host's probed/configured headroom and candidate RPC worker budgets advertised in `PeerRegistry`.
  - Sequential layer pipelining (`--split-mode layer`) using greedy host-first watermarking.
  - Generate `--rpc <PEER:PORT>` arguments for offloaded layers.
- Maintain `src/tunnel.rs`: ADB port forward (`tcp:8080`) and reverse (`tcp:50052`) for low-latency wired USB operation.
- Implement CLI Subcommands:
  - `nexus worker` / `nexus rpc`: Launch `rpc-server` with local memory safety ceiling.
  - `nexus tunnel`: Manage ADB forward/reverse tunnels.

### Verification Command
```bash
wsl bash -l -c "cd /mnt/c/nexus-llm && cargo test --test test_cluster_rpc"
```

---

## Phase 6: Unified Hub UX & TUI Target Node Selection
**Status:** In Progress

### Objectives
- Implement Target Node Selection in `src/ui/models_view.rs`:
  - When pressing `Enter` on a model, present a **Target Execution Node Modal** listing local and discovered cluster nodes.
  - Display available allocatable RAM and acceleration backend for each node.
- Implement Remote Model Dispatch in `src/ui/hub.rs` & `src/control_plane.rs`:
  - If local node selected: spawn local `ProcessSupervisor`.
  - If remote peer selected: dispatch `POST /nexus/control/v1/model/load` to target peer.
- Implement Dynamic Chat Binding in `src/ui/chat.rs`:
  - Dynamically reconfigure chat client endpoint to target the active model host.
- Maintain Unified Hub Navigation:
  - Persistent top navigation tabs shipped today: `[F1: Chat]`, `[F2: Models]`, `[F3: Cluster/Dashboard]`, `[F4: Settings]` (four tabs).
  - `[F4: Tunnel]` from earlier drafts remains deferred — `src/ui/tunnel_view.rs` exists but is not wired into hub F-keys (Continuous doc note; not a Phase 12 feature).
  - Fast keyboard shortcuts and non-blocking event multiplexing.

### Verification Command
```bash
wsl bash -l -c "cd /mnt/c/nexus-llm && cargo test --test test_hub_ui"
```

---

## Full Regression Suite
```bash
wsl bash -l -c "cd /mnt/c/nexus-llm && cargo test"
```
