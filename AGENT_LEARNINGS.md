# Agent Learnings

This file is a concise, append-only record of durable lessons from development,
debugging, validation, and research. It is intended to help future contributors
avoid repeating known mistakes.

## Entry guidelines

- Record bugs, failed commands, environment-specific behavior, research findings,
  and important design decisions that are likely to matter again.
- Include the context, observed finding, resulting action, and verification status.
- Prefer facts and reproducible details over narrative or speculation.
- Update an existing entry when new evidence clarifies it; otherwise add entries
  in reverse chronological order.
- Never include credentials, tokens, secrets, personal data, or sensitive output.

## Entry template

```text
## YYYY-MM-DD — Short title
- Category: bug | failed-command | research | design-decision | environment
- Context: What was being attempted and where.
- Finding: What happened or what was learned.
- Action: The fix, workaround, or rule to follow.
- Verification: How the result was confirmed, or what remains unverified.
```

## 2026-10-06 — Phase 1 P0 Hub/TUI fixes (hot-swap layers, Esc abort, transport badge)
- Category: bug
- Context: Implementing IDENTIFIED_UPGRADES.md Phase 1 (#1–#6 + #21) on the Hub TUI.
- Finding:
  1. Hot-swap stored only the model path, dropping `custom_gpu_layers` — CPU Safe Mode (`-ngl 0`) was silently lost on confirm, reintroducing Android Vulkan freezes.
  2. Any `127.0.0.1` endpoint was labeled `[USB Cable]`; plain local `llama-server` was mislabeled.
  3. Esc quit standalone chat mid-stream and did nothing in Hub Chat; stream `JoinHandle`s were discarded so generation could not be cancelled.
  4. `SystemProfile::probe()` ran inside Models `render()` every frame.
- Action:
  1. Persist `pending_hot_swap: Option<(PathBuf, Option<u32>)>` and confirm via `execute_model_load_with_gpu`.
  2. Explicit `TransportBadge::{Local,Usb,Wifi}` with ADB `forward --list` cached on the 500ms tick.
  3. Store stream `JoinHandle`, Esc aborts; quit via Ctrl+C / idle `q` only. Panic hook restores terminal.
  4. Cache profile on `ModelsView`; wire `[P]` to cycle presets into Hub chat hyperparams.
- Verification: `cargo test --locked` passed (all suites including new abort/badge/persona/hot-swap assertions).

## 2026-10-05 — Mobile Vulkan Adreno Driver Freezes, Model Unload Keybinds, and Chat Sanitization
- Category: bug
- Context: Running Qwen-2.5-3B on Android (Snapdragon 8 Gen 2, Termux Vulkan Adreno 740) caused inference to hang for ~3 minutes on the first prompt before terminating with Transport Error / connection reset.
- Finding:
  1. `ggml-vulkan` compiles compute shaders at first inference prompt execution time, not at model load time. On Adreno 740 mobile GPUs, aggressive full offloading (`-ngl 99`) causes driver stalls in `vkQueueSubmit`/`vkWaitForFences`, tripping Android's GPU watchdog timer (~3 minutes) and sending `SIGKILL` to `llama-server`.
  2. The terminated process severed the remote SSE connection (`Transport Error: error decoding response body`) and rejected host requests (`error sending request`).
  3. HubApp lacked a manual model unload mechanism, leaving nodes stuck in error states until killed.
  4. System notices in chat history were previously passed verbatim to the LLM completion API, corrupting chat templates.
- Action:
  1. Added `[u] / [U]` (and `Ctrl+U` / `/unload`) keybinds across Models, Cluster, and Chat tabs in `HubApp` to cleanly stop `llama-server` and reset discovery advertisements.
  2. Added dual execution target options in [F2] Models: `Local GPU (Accelerated)` and `Local CPU (Safe Mode - DotProd / Multi-thread)` with `-ngl 0` to bypass mobile Vulkan stalls.
  3. Filtered all UI system banners from `messages` in `NexusClient` before sending completion requests via `clean_conversation_messages()`.
  4. Added process stderr ring-buffer capturing to `ProcessSupervisor` to display exact crash causes.
- Verification: Validated with `cargo test` across all 6 test cases in `tests/test_ui.rs` and 10 test cases in `tests/test_hub_ui.rs`.

## 2026-10-05 — Dual Discovery Backends (UDP Beacon + mDNS) and Hub Broadcaster Fix
- Category: bug
- Context: Devices failing to discover each other over Wi-Fi when running the default hub TUI (`nexus`).
- Finding:
  1. Default hub mode (`nexus` with no subcommand) launched the UDP listener but never spawned the broadcaster or mDNS services, leaving the node invisible to peers on the local network.
  2. `default_mdns_enabled()` was disabled (`false`), relying purely on UDP broadcast to port 9999 which is frequently dropped by Wi-Fi routers with client isolation or AP multicast filtering.
  3. Non-Unix platforms returned an empty list of broadcast addresses, restricting UDP discovery solely to limited broadcast (`255.255.255.255`).
  4. The ClusterView header provided no visual indication of whether UDP or mDNS discovery backends were healthy or running.
- Action:
  1. Enabled mDNS-SD by default (`default_mdns_enabled() -> true`) so both UDP beacons and multicast DNS run concurrently.
  2. Spawned the beacon broadcaster and mDNS services in default hub mode, worker mode, and client fallback.
  3. Added `add_static_peer` with runtime dynamic target probing and persistence to `config.network.static_peers` via `config.save()`.
  4. Added `record_service_endpoint` to merge mDNS discoveries into active peers and trigger targeted telemetry exchange probes.
  5. Added real-time backend health indicator badges (`● UDP: Active ● mDNS: Active | Peers: N | ↻ Broadcasting`) to the Cluster tab header.
- Verification: Validated with `cargo test` across all 65 test cases including new tests in `tests/test_discovery.rs` and `tests/test_hub_ui.rs`.

## 2026-10-05 — Windows Host Tooling: Cargo Only Available in WSL
- Category: environment
- Context: Running build and test verification on Windows host.
- Finding: Native Windows PowerShell does not have `cargo` in PATH. Attempting `cargo test` in PowerShell fails with `CommandNotFoundException`. However, WSL has Rust 1.99 and full build tooling installed in `/home/hylan/.cargo/bin`.
- Action: Always invoke Cargo tooling inside WSL using a login shell: `wsl bash -l -c "cd /mnt/c/nexus-llm && cargo <command>"`.
- Verification: Ran `wsl bash -l -c "cd /mnt/c/nexus-llm && cargo test"` and verified all 58 tests passed.

## 2026-10-05 — Unified TUI Startup and In-TUI Settings Architecture
- Category: design-decision
- Context: Unifying the application entry point to start with a single command (`nexus`) while managing full cluster orchestration and configuration inside the TUI.
- Finding: Previously, configuring the node required editing `~/.nexus/config.toml` manually or using separate CLI subcommands (`nexus host`, `nexus worker`). Operators needed an integrated 4-tab UX: [F1] Chat, [F2] Models, [F3] Cluster, [F4] Settings.
- Action:
  1. Consolidated Dashboard and Tunnel into an interactive `ClusterView` tab with peer table selection, local telemetry, and direct remote action keybinds (connect, load model, request worker, inspect, add static peer).
  2. Extended `SettingsView` with Node Identity, Paths & Binaries, and Network & Transport categories, supporting inline text editing and enum cycling.
  3. Enabled hot-reloading upon saving (`S`), immediately updating running models directory, default endpoints, and supervisor paths without restarting the process.
- Verification: Validated via `tests/test_hub_ui.rs` across tab navigation, settings mutation, and headless render. All 58 tests in the test suite pass.

## 2026-10-05 — Transition from Asymmetric Anchors to Symmetric N-Node Mesh
- Category: design-decision
- Context: Planning network expansion and multi-device usability.
- Finding: Hardcoding Node A (S23 Ultra) and Node B (MacBook) as fixed anchors limited flexibility when adding new devices or when the operator wants to choose execution targets and chat from any connected node.
- Action: Transitioned system architecture to a symmetric N-node peer mesh where any node can be an Inference Host, an RPC Worker, or a TUI Client based on dynamic capability advertisement and local memory constraints. Updated `AGENTS.md`, `DESIGN_SPEC.md`, and `BUILD_PLAN.md` to reflect this Single Source of Truth.
- Verification: Confirmed documentation hierarchy is aligned; code changes staged across Phases B through D.

