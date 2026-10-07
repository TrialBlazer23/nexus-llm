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

## 2026-10-07 — Phase 12 §5.1 mesh gateway on dedicated gateway_port
- Category: design-decision
- Context: CAPABILITY_REVIEW §5.1 / Phase 12 MVP on `cursor/phase12-mesh-gateway-5f3d` from `origin/main` @ `038eaa4`.
- Finding:
  1. Do not multiplex the mesh OpenAI front door onto `api_port` (llama-server owns 8080) or `control_port` (signed trust domain on 9998). Use `network.gateway_port` default 8090 + `gateway_enabled`.
  2. Inbound `/v1/*` stays unauthenticated so unmodified OpenAI clients work; when pairing is enforced, resolution only considers verified/trusted peers.
  3. Prefer byte-stream reverse proxy for `/v1/chat/completions` over re-tokenizing via `NexusClient::stream_chat` (that API yields text tokens only and would break OpenAI chunk shape).
  4. `PeerRegistry` has no model fields — resolve via local `SupervisorManager`/`DiscoveryService` `active_model` plus peer beacon `active_model`. Auto-load on miss and full hub rebind collapse are follow-ons.
- Action: Added `src/gateway.rs`, config/Settings/doctor wiring, hub bootstrap prefers local gateway, `tests/test_gateway.rs` with multi-holder `fake_llama`.
- Verification: `cargo fmt --check`, `cargo clippy --locked --all-targets -- -D warnings`, `cargo test --locked` (incl. gateway suite). Live multi-node LAN soak still required.

## 2026-10-07 — Continuous: CI, Penryn opcode scan, decoder property tests, fake llama
- Category: design-decision | environment
- Context: CAPABILITY_REVIEW §6 / §7 Continuous on `cursor/continuous-ci-hygiene-d6cb` from `origin/main` @ `2e9003f`. Not Phase 10/11/12.
- Finding:
  1. No CI existed (`.github/` had agent defs only). GitHub Actions needs fmt + clippy `-D warnings` + `cargo test --locked`, plus a best-effort Android job (`continue-on-error`) because runners often lack an NDK linker.
  2. Penryn Directive 1: scan `objdump -d` mnemonics for AVX/AVX2/FMA/SSE4.2 via `scripts/check_penryn_opcodes.sh` after `cargo build --release --bin nexus`. `ring`, `rand_chacha`, `memchr`, and `httparse` still embed CPUID-gated AVX kernels (`#[target_feature]` / asm) that rustc `-C target-feature=-avx,…` does not strip — the script allowlists those symbols and **fails on any other** forbidden mnemonic (plus checks `.cargo/config.toml` rustflags are present).
  3. `BeaconPacket::decode` is fixed 64 bytes (no alloc risk); `GgufMetadata::read` needed remaining-size / max-element guards before `with_capacity` / `vec![0; len]` so adversarial headers return `InvalidLength` instead of panic-allocating. Tensor-section parse stays Phase 11.
  4. Prefer `proptest` under `[dev-dependencies]` over `cargo-fuzz` so coverage runs in normal `cargo test --locked` without nightly.
  5. Fake llama (`tests/support/fake_llama.rs`) + two-node UDP/control-plane mesh cover client SSE and discovery↔control paths that unit suites missed; live GGUF/`llama-server` still required for real load soaks.
- Action: Ship `.github/workflows/ci.yml`, Penryn script, GGUF bounds, property/adversarial tests, harnesses, clippy zero, doc drift fixes (`DESIGN_SPEC` control paths, `AGENTS.md` layout / non-zero-copy GGUF, Tunnel tab deferred note).
- Verification: `cargo fmt --check`, `cargo clippy --locked --all-targets -- -D warnings`, `cargo test --locked`, `./scripts/check_penryn_opcodes.sh target/release/nexus`.
## 2026-10-06 — Phase 10 model store / LAN blob transfer
- Category: design-decision
- Context: CAPABILITY_REVIEW Phase 10 on `cursor/phase10-model-store-d6cb`.
- Finding: Control-plane JSON uses `MAX_CONTROL_RESPONSE_BYTES` (16 KiB); blob bodies must stream with a separate body type and never wrap in `Limited`. Phase 9 text requires transfer to be privileged like load/unload — `GET /blob/{digest}` and `POST /blob/fetch` use `verify_control_request` + `authorize_privileged_signer`. Filename-only catalogs are insufficient for verified LAN sync; digests live in `~/.nexus/models.json` (override with `NEXUS_MODELS_INDEX`). Concurrent reconcile of the shared index needs a process lock or tests flake.
- Action: Added `src/store.rs`, digest fields on `ModelCatalogEntry`, streaming blob route, hardened `downloader.rs` (`.part.json`, retry, incremental hash, disk preflight), Models `[D]`/`[T]`/`[S]` via hub worker commands.
- Verification: `cargo test --locked` including `tests/test_phase10_store.rs`.
## 2026-10-07 — Phase 11 placement: MemoryPlan, tensor split, ranking, supervisor
- Category: design-decision
- Context: CAPABILITY_REVIEW §3 / Phase 11 on `cursor/phase11-placement-intelligence-d6cb` from `origin/main` @ `2e9003f`. Phase 10 PR #13 still draft — branch does not depend on the model store.
- Finding:
  1. Boolean LMK + 200 KB/token heuristic over-rejects 1.5B-class models; `MemoryPlan` with GGUF dims + mmap-resident weights + kv_dtype is the right API.
  2. Fraction×layers under-offloads when embeddings/output are large; plan from per-tensor `blk.N.*` bytes and refuse missing `block_count` (no more `unwrap_or(32)`).
  3. Global `max_rpc_ram_mb > 1800` reject blocked 32 GB workers — keep 1800 as default only, not a ceiling.
  4. Ranking objective is predicted tok/s (`min(compute, network)`); unknown link quality demotes offload. Network offload is last-resort for fit, not a perf feature.
  5. Supervisor must honor `fallback_to_cpu=false`, probe health on `127.0.0.1`, pass `--mlock`/`-tb`/`--cache-type-k/v`, and expose restart backoff + GPU demotion policy.
- Action: Grew `src/cluster/{mod,memory,split,rank}.rs`; hardened `gguf.rs`; wired hub target modal + Models badges; lifted 1800 clamps.
- Verification: `cargo test --locked` green. **Still unverified live:** tensor-byte `--rpc --split-mode layer` (+ optional `--tensor-split`) against real `llama-server`/`rpc-server` on Penryn and Snapdragon; calibrate `compute_buffer_mb` defaults and record measured numbers here.

### Live validation runbook (Phase 11)
```bash
# On each target class, with a GGUF that fraction-planning under-offloaded:
cargo run --locked --bin nexus -- info
# Load via hub target modal; confirm ranked tok/s labels and distributed option.
# Compare emitted args to a manual llama-server invocation:
#   llama-server -m MODEL -c 2048 -ngl 99 --rpc WORKER:50052 --split-mode layer [--tensor-split H,W]
# Confirm load succeeds; note RSS vs MemoryPlan; update compute_buffer_mb if far off.
```

## 2026-10-06 — Phase 8 TUI: command/event bus, ChatEntry, hot-swap -ngl
- Category: design-decision
- Context: CAPABILITY_REVIEW §2 Phase 8 on `cursor/phase8-tui-responsiveness-4865` stacked on Phase 9 trust.
- Finding:
  1. Long loads must not `.await` on the hub key path — `HubCommand`/`HubEvent` + `spawn_hub_worker` keep `run_hub_tui` draining crossterm/stream while `SupervisorManager::spawn` runs; `subscribe`/`state` feed phase labels.
  2. `ChatEntry { kind, metrics, rendered }` replaces parallel `message_metrics` and English prefix banner filters; notices that say "Connected to…" stay out of the OpenAI prompt.
  3. Hot-swap must carry full intent (`path`, `gpu_layers`, `context_size`) so CPU Safe Mode confirms with `-ngl 0` (`effective_ngl(Some(0), …) == 0`).
  4. Render must be pure: GGUF fields cached on `ModelEntry` at scan; `ModelsView.cached_profile` on tick; `SystemProfile::probe_vulkan` process-cached via `OnceLock`; wrap scroll uses one `wrapped_line_count` shared by keys and Paragraph.
- Action: Split `src/ui/hub/{mod,commands,keymap}.rs`; reserve `StartDownload`/`TransferModel` commands for Phase 10.
- Verification: `cargo test --locked` (ChatEntry/wrap/hot-swap/keymap + existing suites). Live ~30s load soak needs `llama-server` + GGUF.

## 2026-10-06 — Phase 9 trust: signing canonical, node.key, pairing migration
- Category: design-decision
- Context: CAPABILITY_REVIEW §1.4 + §1.5 — authenticated control plane and registry as runtime source of truth on `cursor/phase9-trust-4865`.
- Finding:
  1. Control-plane auth uses header-based envelopes (`Nexus-Signature-*`) and canonical string `nexus-control-v1\n{METHOD}\n{PATH}\n{sha256_hex(body)}\n{timestamp}\n{nonce}\n{signer_id}` with ±120s skew and a per-peer nonce LRU (~10 min).
  2. `~/.nexus/node.key` stores the Ed25519 secret with Unix mode **0600**; existing `node.id` UUIDs are preserved; fresh installs get UUID v5 from the public key.
  3. Pairing codes are HMAC-SHA256 over 5-minute windows (6 digits, zero-padded). First successful pair appends `allowed_peer_ids` + `paired_peers` and sets `require_pairing = true`.
  4. Beacons stay advisory; when pairing is enforced, chat/RPC use `registry_runtime` verification + `eligible_rpc_peers` / `find_best_trusted_host`, not beacon UUID alone.
  5. `DiscoveryService` holds `Arc<RwLock<NexusConfig>>` — clone security policy before `.await` in listener tasks (`RwLockReadGuard` is not `Send`).
- Action: Modules `node_identity`, `trust_auth`, `registry_runtime`; signed client POSTs; server gate on state/load/unload/pair; Cluster `[P]`/`[O]` and `nexus pair --host --code`.
- Verification: `cargo test --locked` including `tests/test_trust.rs` (signatures, replay, forged beacon, pair-then-load).

## 2026-10-06 — Phase 7 remainder: names, zero-config resolve, doctor, settings honesty
- Category: design-decision
- Context: Finishing CAPABILITY_REVIEW §7 Phase 7 acceptance after PR #9 shipped the control-plane server.
- Finding:
  1. Beacon v1 still has no room for a display name; mDNS TXT `name=` + `PeerNode.display_name` / `label()` is enough. UDP-only peers keep `Node-<uuid8>` until mDNS arrives.
  2. `resolve_from_discovery` only consulted the unset primary-compute anchor. Undeprecating `find_best_host` as fallback restores default-config auto-connect. The hub path never called discovery at all (localhost fallback) — that was the larger gap for two-device LAN acceptance.
  3. TUI had no tracing subscriber; file logs under `~/.nexus/logs/nexus-<pid>.log` via `src/logging.rs` keep the alternate screen clean. `nexus doctor` treats missing `llama-server`/`rpc-server`/`adb` as WARN (exit 0) and bind/config failures as FAIL (exit 1).
  4. Settings honesty: wire `max_ram_usage_percent`, `mmap`, `enable_rpc`, `prefer_adb_tunnel`, `rpc_server_binary`, `node.name`; hide inert `FallbackCpu`; add live `control_port`. Leave TOML-only `runtime_role` / `capabilities` / `cpu_threads_batch` / `mlock` unsuffixed.
- Action: Implemented on `cursor/phase7-mesh-remainder-7787` stacked on the Phase 7 control-plane tip. Next phase should be **Phase 9 Trust** (pairing / signed control plane / registry verification) before Phase 8 TUI responsiveness — remote `model/load` is now unauthenticated on the LAN.
- Verification: `cargo test --locked`; `nexus doctor` WARN-only exit 0 without llama binaries; log files created under `~/.nexus/logs/`; `scripts/verify_review_findings.sh` updated for wired Settings fields + file logging.

## 2026-10-06 — Phase 7: control plane HTTP server on dedicated port
- Category: design-decision
- Context: Implementing CAPABILITY_REVIEW.md §1.1 + §1.2 (Phase 7 MVP) so remote model load from the TUI can succeed.
- Finding:
  1. Serving control routes on `api_port` (8080) collides with llama-server; clients must target a dedicated `network.control_port` (default 9998).
  2. Beacon v1 is a full 64-byte layout with no spare field for a control port. Advertising `ctrl` via mDNS TXT plus falling back to the local config default for UDP-only peers is enough for Phase 7; a beacon v2 layout is deferred.
  3. Hub previously owned `Option<ProcessSupervisor>` while handlers used `SupervisorManager`. Without unifying those, a remote load and a local load would manage different subprocess slots.
  4. `hyper` was only transitive via `reqwest`; an explicit `hyper` + `hyper-util` + `http-body-util` dependency is required for a hand-rolled server and should stay preferred over `axum` to keep the footprint small (AGENTS.md Directive 4 intent).
- Action: Added `src/control_plane_server.rs`, `network.control_port`, mDNS `ctrl` TXT, `PeerNode::control_endpoint()`, hub/nexusd/worker lifecycle startup, and SupervisorManager as the shared inference owner. Deferred SSE events, pairing, and catalog GET to later phases.
- Verification: `cargo test --locked` (including new `tests/test_control_plane_server.rs` loopback HTTP round-trips) and `scripts/verify_review_findings.sh` S1.1 positive wiring checks.

- Category: research
- Context: End-to-end review of the TUI, cross-device control plane, model handling, and model download/transfer paths at commit `213ee12`, recorded in [CAPABILITY_REVIEW.md](CAPABILITY_REVIEW.md).
- Finding:
  1. The dominant defect class is not bad code but unreachable code. `handle_load_model`/`handle_unload_model`/`fetch_state`/`dispatch_unload_model` (`src/control_plane.rs`), the entire `PeerRegistry` lifecycle (`expire`, `remove_terminal`, `mark_verified`, `eligible_rpc_peers`), `Preset::format_prompt`, `NexusClient::complete_chat`, `TunnelView`, and `ModelDownloader` (outside the CLI) have no callers in `src/`. No HTTP server binds the control-plane paths, so every remote model-load path in `src/ui/hub.rs` dispatches to an endpoint that cannot answer.
  2. `cargo test` passed all 73 tests against this state, because the tests call library functions directly rather than driving behavior through `main.rs`/`nexusd` entry points. A green suite is not evidence that a documented feature is reachable.
  3. Ten configuration fields are read and written only by `src/ui/settings_view.rs` and never consulted elsewhere: `node.name`, `node.runtime_role`, `node.capabilities`, `acceleration.fallback_to_cpu`, `acceleration.cpu_threads_batch`, `safety.mmap`, `safety.mlock`, `safety.max_ram_usage_percent`, `cluster.enable_rpc`, `cluster.prefer_adb_tunnel`. `SystemProfile::max_allowed_memory_bytes` hardcodes 75% and ignores `safety.max_ram_usage_percent` because `SystemProfile` has no access to config.
  4. `NexusClient::resolve_from_discovery` only consults `resolve_primary_compute_anchor`, which returns `None` unless `network.anchors.primary_compute_id` is set. The default config therefore cannot auto-discover a host; the migration to pinned anchors deprecated `find_best_host` without replacing the zero-config path.
  5. `src/main.rs` installs no `tracing` subscriber, so every log line emitted in TUI mode is discarded. Only `src/daemon.rs` configures one.
  6. Hardcoded device constants survive outside `cluster.rs` despite Directive 3: `config.rs::validate()` rejects `cluster.max_rpc_ram_mb > 1800` (a Core 2 Duo limit enforced globally, so a 32 GB worker's config will not load), and `models_view.rs` compares against a bare `10300` MB for badge colors.
  7. `src/ui/models_view.rs::render_model_details` calls `GgufMetadata::open()` and `SystemProfile::probe()` inside render, so a multi-megabyte tokenizer array is parsed several times per second. `src/gguf.rs` is documented as "zero-copy" but materializes every metadata array, and sizes `HashMap::with_capacity`/`Vec::with_capacity`/`vec![0u8; len]` directly from file-supplied lengths, so a corrupt GGUF can abort the process.
  8. `message_metrics` is a `Vec` parallel to `messages` indexed positionally at render time; `src/ui/hub.rs` lines 256–257, 316, 439–440, and 713 mutate `messages` without the matching metrics operation, permanently misaligning telemetry badges.
- Action: Recorded findings, proposals, and a Phase 7–12 plan in `CAPABILITY_REVIEW.md`. Two rules worth carrying forward: (a) treat "reachable from a user action" as an acceptance criterion alongside "unit tested", and build a fake `llama-server` harness so integration tests can drive real entry points; (b) when a config field is added to `SettingsView`, the same change must wire it into behavior or it must not be displayed.
- Verification: Claims were each confirmed by `rg` over `src/` and `tests/` for call sites, and by reading the cited files. Baseline recorded: `cargo test` 73 passing, `cargo clippy --all-targets` 19 lib warnings and 0 errors, no CI workflows in `.github/`. The proposals themselves are unimplemented and unverified.

## 2026-10-05 — Chat TUI Overhaul: Pure-Rust Markdown, Cursor Ergonomics, Stream Abort, and Telemetry Badges
- Category: design-decision
- Context: Upgrading the interactive Chat TUI (`src/ui/chat.rs`) following successful cross-device GPU offload across Snapdragon 8 Gen 2 and legacy MacBook nodes.
- Finding:
  1. Heavy syntax highlighters like `syntect` pull in C Oniguruma regex bindings which risk violating Directive 1 (Penryn SSE4.1 instructions) and Directive 2 (Android Bionic linking). Pure-Rust `pulldown-cmark` coupled with a lightweight multi-language keyword lexer provides rich boxed code block formatting (`┌─ rust ─...─┐`) with zero C dependencies.
  2. Previously, pressing `Esc` during generation terminated the TUI event loop instead of aborting the active HTTP/SSE generation stream. A oneshot abort channel allows `Esc` (or `Ctrl+C`) to cleanly halt the stream and keep the app interactive.
  3. Generation metrics (tokens/sec, total tokens, TTFT) previously vanished once streaming ended. Attaching `GenerationMetrics` to assistant turns persists telemetry badges directly underneath responses in conversation history.
  4. Header title strings can exceed 100 columns when target hardware details are included; headless rendering tests must allocate adequate column width (e.g., 140 columns) to prevent assertion truncation failures.
- Action:
  1. Implemented `src/ui/markdown.rs` and `src/ui/session_logger.rs`, keeping `chat.rs` modular and responsive.
  2. Added horizontal cursor navigation (`Left`/`Right`/`Home`/`End`), mid-buffer editing, and `Shift+Enter` multi-line insertion.
  3. Added dual preset controls: `[Alt+P / F5]` Preset Picker modal and slash commands (`/preset`, `/temp`, `/top_p`, `/max_tokens`, `/system`, `/clear`, `/export`).
  4. Wired active generation hyperparameters through `ChatCompletionRequest`.
- Verification: Validated with `wsl bash -l -c "cd /mnt/c/nexus-llm && cargo test"` with all 66 tests passing across all integration test suites.

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

