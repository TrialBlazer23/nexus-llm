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

## 2026-10-10 — Active-params + flash I/O MoE cold-start scoring
- Category: design-decision
- Context: Phase 16.5 step 4 — cold-start MoE tok/s was a flat 2.2 for every (ctx, ceil), so joint knobs could not prefer warmer expert caches on throughput grounds, and denser top-k MoEs ranked identically to A3B.
- Finding: GGUF already exposes `expert_count` / `expert_used_count` and expert tensor bytes. Measured `BACKEND_MOE_STREAM` samples must stay authoritative (not rescaled). Soft sqrt demotion on active fraction plus cache-vs-working-set I/O penalty (`0.55 + 0.45*hit`) matches soak (~2.2 A3B warm) while making ceil tok/s-sensitive.
- Action: Add `predict_moe_stream_tok_s` in `cluster/moe_knobs.rs`; wire into `plan_moe_stream_knobs` cold-start path; keep `MOE_COLD_START_TOK_S = 2.2` as A3B reference.
- Verification: Unit tests for measured override, A3B warm ≈2.2, higher top-k demotion, cold < warm, knob planner prefers warm ceil; `cargo test --locked` + clippy `-D warnings`.

## 2026-10-10 — RemoteMoeStream mesh placement (Phase 16.5 step 3)
- Category: design-decision
- Context: Peers advertise `moe_stream` but ranking only offered dense `PlanTarget::Remote`, which mmap-fit checks could mark as Fits for oversize MoE files; Hub remote load always sent `backend=auto` without planned ctx/ceil.
- Finding: Client-side placement needs a synthetic `SystemProfile::from_advertised(free, total, backend)`; remote dense plans for stream-capable peers must be suppressed when dense LMK fails; control-plane load must re-run `plan_moe_stream_knobs` on the peer and spawn via `from_profile_with_ceil`. Cross-node MoE bench stays on each device's `BenchStore` (`node_id=local`) in v1 — remote ranking uses cold-start tok/s unless samples were recorded under the peer label locally.
- Action: Add `PlanTarget::RemoteMoeStream`, `TargetExecutionNode::RemoteMoeStream`, `ModelLoadRequest.moe_cache_ceil_mb`, Hub `LoadModelRemote` with `backend=bmoe`; reject RPC+MoE on control plane.
- Verification: `cargo test --locked` (including `ranks_remote_moe_stream_on_capable_peer_when_local_too_small`, `no_remote_moe_stream_*`, `from_advertised_uses_free_ram_for_lmk`); `cargo clippy --locked --all-targets -- -D warnings` clean.

## 2026-10-10 — Joint MoE (ctx, cache_ceil) planner wires into spawn
- Category: design-decision
- Context: Phase 16.5 step 2 — pick feasible stream LMK knobs and apply them at bmoe-cli spawn.
- Finding: Ranking advertised a single ceil at fixed policy ctx; Hub hardcoded placement ctx=4096; load ignored `LocalMoeStream.ceil_mb` and re-derived via `from_profile`. `--cache-mb auto` could disagree with the LMK ceil commitment.
- Action: Add `cluster::moe_knobs::plan_moe_stream_knobs` (grid + tok/s-then-ctx-then-ceil scoring); rank emits winner ctx/ceil; Hub passes `selected_context` and threads `moe_cache_ceil_mb` through `LoadModelLocal`/`HotSwapIntent`; `BmoeSessionConfig::from_profile_with_ceil` forces integer `--cache-mb` when ceil≥2000; CLI `nexus host` plans before spawn.
- Verification: `cargo test --locked` green (moe_knobs unit tests + placement); `cargo clippy --locked --all-targets -- -D warnings` clean.

## 2026-10-10 — MoE bench feedback closes LocalMoeStream ranking loop
- Category: design-decision
- Context: Implementing Phase 16.5 step 1 — feed BigMoe `BMOE_DONE` metrics into placement ranking.
- Finding: Dense llama-server already wrote wall-clock SSE rates into `BenchStore`; MoE adapter discarded `tok_s`/`cache_hit_pct`, and `LocalMoeStream` always ranked at hardcoded `2.2`. Chat MoE labels mapped to `GenericCpu` ("cpu"), which would poison dense entries if recorded.
- Action: Add `BACKEND_MOE_STREAM = "moe-stream"` string-key APIs + optional `cache_hit_pct` on samples; record from bmoe Done paths with `node_id="local"` and real context; rank via measured lookup with `2.2` cold-start fallback; skip chat bench writes when backend label contains `"moe"`.
- Verification: `cargo test --locked` green (including `local_moe_stream_uses_measured_bench_tok_s`, `moe_stream_key_records_cache_hit_average`, `moe_bench_sample_from_done_filters`); `cargo clippy --locked --all-targets -- -D warnings` clean.

## 2026-10-10 — Adaptive MoE loading roadmap after 30B-A3B soak
- Category: research | design-decision
- Context: Qwen*30B-A3B-class MoE verified on-device via BigMoe flash streaming at >2 tok/s (Phase 16 soak ~2.35 tok/s on S23; operator follow-up with Qwen2.5-30B-A3B). Question: how to make backend model handling more adaptive/smart for larger MoEs.
- Finding: The hard path (stream LMK, `should_prefer_moe_stream`, `PlanTarget::LocalMoeStream`, no RPC+stream) already works. Remaining adaptivity gaps are closed-loop, not architecture:
  1. Placement still hardcodes `predicted_tok_s = 2.2` for MoE stream; `BMOE_DONE.tok_s` / `cache_hit_pct` are not fed into `BenchStore`.
  2. Expert cache ceil is a static ~45% of LMK budget; no joint optimization of `(context_size, cache_ceil)` under `resident + cache + KV`.
  3. Peers advertise `moe_stream` but ranking never emits remote MoE-stream plans — oversize MoEs may still prefer dense RPC incorrectly.
  4. Throughput heuristics ignore `expert_used_count` (A3B ≈ 3B active params + flash I/O), so ranking treats MoE like dense 30B.
  5. Phase D (networked route-ahead, distributed expert affinity, Vulkan experts) remains correctly gated — local flash already wins at verified rates.
- Action: Prefer a “Phase 16.5 adaptive MoE” track before Phase D: (a) MoE-aware bench feedback into `rank_execution_plans`, (b) joint ctx/cache Pareto under stream LMK, (c) `RemoteMoeStream` for `moe_stream` peers, (d) active-params + flash-I/O ranking, (e) hit%-driven cache governor with lossy only when chronically cold. Keep no-RPC+stream and external `bmoe-cli` constraints.
- Verification: Research-only; no code change in this entry. SenseLab key `nexus-llm/moe/decision-adaptive-moe-roadmap-post-30b-a3b`.

## 2026-10-09 — Security Audit Hardening: Non-Poisoning Locks, Preset Routing, Doctor Probes, and Fuzzing
- Category: design-decision | bug
- Context: Addressing external technical audit feedback across security defaults, sync locks in async hyper handlers, dependency drift, pairing rate limiting, and parser attack surfaces.
- Finding:
  1. **Security Posture & Pairing Defaults**: Operating an open mesh by default (`require_pairing = false`, `allow_unpaired_lan = true`) enables zero-config setup on trusted LANs, but readers reasonably assumed Ed25519 was unconditionally enforced. Adding explicit `nexus doctor` warnings, startup logging, and a dedicated `SECURITY.md` transparently documents the trust boundary.
  2. **Async Handler Lock Poisoning**: Calling `.expect("... lock")` across 13 hyper async endpoints created panic cascades if any thread panicked while holding a lock. Adding `read_config()` and `write_config()` helpers with `.unwrap_or_else(|p| p.into_inner())` safely recovers without crashing the executor.
  3. **Dependency Drift**: `serde_yaml 0.9` was archived upstream; migrating to `serde_yml 0.0.12` cleanly preserved YAML serialization across presets and models.
  4. **Pairing Oracle Hardening**: Fixed-rate 429 cutoffs on pairing PIN attempts provided an oracle; implementing progressive exponential backoff (1s delay after 3 failures, 3s after 5, lockout after 10) and constant-time PIN comparison (`constant_time_eq_6`) closes timing and enumeration vectors.
  5. **Parser Robustness**: Testing untrusted byte inputs with `proptest` verified `GgufMetadata::read` safely handles corrupted magic, extreme counts (u64::MAX), and invalid UTF-8 without panicking.
- Action: Published `SECURITY.md`, created `deny.toml`, configured Dependabot, migrated to `serde_yml`, implemented non-poisoning config helpers, added `check_security()` probe to `nexus doctor`, added `tests/test_gguf_properties.rs`, and refactored `README.md` to lead with Quickstart without marketing hype.
- Verification: `cargo fmt --all -- --check`, `cargo clippy --locked --all-targets -- -D warnings`, `cargo test --locked` (249 unit, integration, and property tests passing), `cargo run --locked --bin nexus -- doctor`, and `./scripts/check_penryn_opcodes.sh target/release/nexus`.

## 2026-10-09 — BigMoe Termux Bionic Build, Wait-for-Ready Deadline, and Host CLI MoE Dispatch
- Category: bug | environment | design-decision
- Context: Building `bmoe-cli` natively in Termux and running the 18GB `Qwen3-Coder-30B-A3B-Instruct` model on Android (Galaxy S23 Ultra, Snapdragon 8 Gen 2, 12GB RAM).
- Finding:
  1. **Termux Detection Shadowed**: `scripts/setup.sh` defaults `PREFIX="${NEXUS_PREFIX:-$HOME/.nexus}"`, which shadowed Termux's system `$PREFIX` (`/data/data/com.termux/files/usr`). `detect_platform.sh` checked `[ -x "$PREFIX/bin/pkg" ]` which failed, misidentifying Termux as unknown Linux.
  2. **Android NDK Clang API Target**: Termux clang defaults to `aarch64-linux-android24`. BigMoe's `platform_io.cpp` uses `AHardwareBuffer` APIs which require Android API >= 26 (`__INTRODUCED_IN(26)`), failing compilation. Supplying `-target aarch64-linux-android28` resolves the symbol availability cleanly.
  3. **bmoe-cli stdout Pre-Ready Timeout**: `wait_for_ready` previously used `tokio::time::timeout(Duration::from_secs(30), lines.next_line())`. `bmoe-cli` prints progress to stderr and only writes `BMOE_READY` to stdout after completely mapping and initializing weights. On large 18GB+ GGUFs, cold flash initialization can take >30s, causing false timeout aborts.
  4. **n_predict / Context Clamping**: `handle_chat` in `bmoe_client.rs` defaulted to 512 `n_predict` when unprovided by clients. If context was small, `prompt + 512` exceeded `n_ctx`, causing 500 error from `bmoe-cli`.
- Action:
  1. Added `TERMUX_VERSION` and `/data/data/com.termux` checks to `detect_platform.sh` and adopted existing system `llama-server`.
  2. Configured `-target aarch64-linux-android28` for Termux in `build_bmoe.sh`.
  3. Changed `wait_for_ready` to use the remaining overall deadline (`deadline.saturating_sub(start.elapsed())`).
  4. Clamped `n_predict` to available context (`context_size - prompt_estimate`) and wired `nexus host` to inspect GGUF and automatically route streamable MoEs through `spawn_bmoe`.
- Verification: Built `bmoe-cli` natively on Termux; ran `Qwen3-Coder-30B-A3B-Instruct-Q4_K_M.gguf` under `nexus host` and verified both non-streaming and streaming completions via `nexus client` and curl. All 75 tests passing.

## 2026-10-09 — Unified setup installs backends under ~/.nexus/bin (no C++ in crate)
- Category: design-decision
- Context: Operators needed one clone→ready path for Nexus + llama.cpp + bmoe-cli across Termux, Penryn Linux, WSL, and macOS.
- Finding: Bundling via Cargo `build.rs` / FFI would violate the subprocess rule and break Penryn/Termux baselines. Prebuilt llama/bmoe blobs often ship AVX and are unsafe as the only install path. BigMoe vendors its own Helldez llama.cpp fork; stock `llama-server`/`rpc-server` need a separate tree. Modern llama.cpp may emit `ggml-rpc-server` — install/symlink as `rpc-server` for Nexus config.
- Action: Ship `scripts/setup.sh` (+ `setup.ps1` → WSL) with pinned refs in `scripts/versions.env`; install to `~/.nexus/bin`; `nexus setup __write_bins` rewrites config paths and can set `inference.moe.enabled`. Cloud/CI may use `--skip-llama --skip-moe`. Keep advanced MoE knobs out of the primary Settings surface.
- Verification: `bash scripts/setup.sh --dry-run`; `cargo test --locked --lib setup::`; full backend builds are operator/hardware soak (long compile).

## 2026-10-09 — Downloader Resume Socket Idle Timeout on Large Partial Files & Buffer Tuning
- Category: bug | design-decision
- Context: When resuming large model downloads (e.g. 15GB+ partial files) via `nexus download`, network retries failed with `Reqwest(Decode(hyper::Error(UnexpectedEof, "peer closed connection without sending TLS close_notify")))`.
- Finding:
  1. **Premature Request Dispatch vs Idle Timeout**: `download_once` called `req.send().await?` (opening the HTTP connection and receiving response headers) *before* running `hash_prefix(part_path, downloaded, &mut hasher).await?`.
  2. **Flash Read Stalls Socket**: Hashing 15+ GB of partial file data on disk using a 64 KB buffer took ~70 seconds. During this disk read, zero bytes were consumed from the HTTP response socket stream. Remote CDNs (such as AWS CloudFront / Hugging Face CDN) enforce a ~60s idle response timeout and closed the connection before the first byte could be read.
- Action:
  1. Moved partial prefix hashing before `req.send().await?` so `hasher` is pre-populated from existing disk bytes before the remote HTTP connection is opened. Upon receiving HTTP 206 Partial Content headers, `resp.bytes_stream()` is consumed immediately with zero idle delay.
  2. Increased `hash_prefix` chunk buffer from 64 KB to 1 MB (`1024 * 1024`), significantly reducing disk read syscall overhead.
- Verification: Tested with 15GB `.part` file, confirmed clean resume with immediate streaming and all tests passing.

## 2026-10-09 — BigMoeOnEdge integration: external bmoe-cli, stream LMK, no RPC+stream
- Category: design-decision
- Context: Integrating Helldez/BigMoeOnEdge flash-streaming MoE into nexus-llm orchestration.
- Finding: Nexus has no llama.cpp submodule and must not bind C++. BigMoeOnEdge is a separate `bmoe-cli` binary with `--session` JSON (`BMOE_*` stdout), not a `llama-server` fork; it has no multi-node RPC and streamed experts are CPU-only. Dense LMK (`file + KV`) would wrongly reject >RAM MoEs. `inference.cache.max_cache_mb` is prompt-slot disk quota and must not be overloaded as expert RAM cache.
- Action: Supervise `bmoe-cli --session` with an OpenAI adapter on `api_port`; budget via `moe_resident + cache_ceil + KV`; add `[inference.moe]`; prefer `PlanTarget::LocalMoeStream` over dense `--rpc` for streamable MoE; never combine MoE stream with layer RPC in v1. Lossy knobs require `quality_mode = lossy`.
- Verification: `cargo test --locked --lib` (gguf/sysinfo/bmoe_client/cluster) and `cargo test --locked --test test_placement`; hardware soak with real `bmoe-cli` + MoE GGUF remains operator-side.

## 2026-10-09 — Android CPU Mode Vulkan Device Isolation, SSE Error Surfacing, and Gateway Digest Resolution
- Category: bug
- Context: Model inference failed on both TUI and WebUI in Android Termux (Adreno 740 / Snapdragon 8 Gen 2). In TUI, `(generating...)` vanished immediately with no tokens or errors; in WebUI, `/v1/chat/completions` returned `404 no active holder for model '<digest>'`.
- Finding:
  1. **Vulkan Device Leakage on CPU Mode**: In `llama-server` 0.6.0+, `-ngl 0` only stores 0 layers in VRAM, but `--op-offload` remains enabled by default. On Android Termux with Qualcomm's proprietary driver, `ggml-vulkan` compute pipeline creation fails (`vk::Device::createComputePipeline: ErrorUnknown`). Even after Nexus's supervisor initiated CPU fallback, `llama-server` still called Vulkan for host tensor ops on prompt decode, returning a 500 SSE error.
  2. **SSE Error Swallowing**: In `NexusClient::stream_chat`, SSE lines containing `{"error": ...}` failed `serde_json::from_str::<ChatCompletionChunk>`, were debug-logged, and yielded `None`. The TUI interpreted this as stream completion, clearing `(generating...)` without displaying the error banner.
  3. **Gateway Digest Resolution**: `/v1/models` in `src/gateway.rs` included raw 64-char SHA256 digests in `ids: BTreeSet<String>`. Lexicographical sorting put hex hashes before letter names, causing the WebUI to set `state.activeModel` to the digest. When sent to `/v1/chat/completions`, `resolve_model_upstream` only tested filename/stem equality, failing to resolve the digest to the active local model holder.
- Action:
  1. Updated `LlamaServerConfig::build_args` and `ProcessSupervisor::try_spawn` in `src/supervisor.rs` to set `LLAMA_ARG_DEVICE=none`, `GGML_VK_VISIBLE_DEVICES=""`, and `--device none` when `gpu_layers == 0`, ensuring 100% pure CPU execution with DotProd acceleration.
  2. Updated `NexusClient::stream_chat` in `src/client.rs` to parse `SseErrorChunk` when choice parsing fails and yield `Some(Err(ClientError::ApiError))`.
  3. Updated `src/gateway.rs` to resolve digests to catalog models in `resolve_model_upstream`, prioritized active models in `handle_models`, and updated `web/dist/index.html` to avoid raw hashes as default active model.
- Verification: Tested live `llama-server` inference with `LLAMA_ARG_DEVICE=none`, confirming instant token generation at ~27 tok/s; added `gateway_resolves_model_by_sha256_digest` in `tests/test_gateway.rs` and `fake_llama_sse_error_chunk_surfaced` in `tests/test_fake_llama_client.rs`; verified all test suites pass with `cargo test --locked`.

## 2026-10-08 — GGUF Header Specification: No 32-Byte Alignment Preceding Tensor Info
- Category: bug
- Context: Downloaded models were completely finishing, but immediately flagged with `[!] 1 corrupted / non-GGUF file(s) found — press [Shift+X] to clean` by `find_corrupted_models`.
- Finding:
  1. **GGUF Binary Layout**: According to the official GGML/GGUF specification:
     `header -> metadata_kv[] -> tensor_infos[] -> padding (alignment) -> tensor_data[]`.
     Alignment padding (typically 32 bytes, configured by `general.alignment`) is applied ONLY before `tensor_data` (the binary weight buffers), NOT between the metadata KV dictionary and `tensor_infos`.
  2. **False Corruption Trigger**: An erroneous 32-byte alignment seek `(pos + 31) & !31` was placed between the metadata KV loop and the tensor info loop in `GgufMetadata::read()`. For any real-world GGUF file whose metadata KV section was not a multiple of 32 bytes, this seek jumped into the middle of the first tensor's string header, causing `read_string_bounded` to fail with `InvalidUtf8`. This caused `GgufMetadata::open(&path)` to fail on 100% valid, completed downloads, mistakenly classifying them as corrupted.
- Action: Removed the bogus pre-tensor alignment seek in `src/gguf.rs`, added explicit failure reason logging in `src/import.rs` (`find_corrupted_models`), and added `test_unaligned_kv_metadata_with_tensors` unit test.
- Verification: Tested with official 2GB `Llama-3.2-3B-Instruct-Q4_K_M.gguf` header, confirming all 255 tensors and metadata keys parsed cleanly with `test_unaligned_kv_metadata_with_tensors` and all 160+ unit and integration tests passing.

## 2026-10-08 — Downloader Hardening, Zero-Byte Rejection, Curated Starters, & Local Storage Importer
- Category: bug | design-decision
- Context: Operators experienced 0-byte `.gguf` files when attempting to download models (due to Hugging Face `/blob/` HTML pages being saved or aborted streams being promoted), and lacked a way to import local GGUF models already stored on Android `/sdcard/Download` or host download folders.
- Finding:
  1. **Hugging Face /blob/ vs /resolve/ streams**: When operators paste Hugging Face URLs copied from a browser, the path typically contains `/blob/main/<file>.gguf`, which returns an HTML preview webpage instead of binary model weights. Normalizing `/blob/` to `/resolve/` and converting shortlinks (`hf.co/...` to `huggingface.co/...`) ensures the stream requests raw binary weights.
  2. **First-chunk GGUF magic byte verification**: Checking `first_chunk.starts_with(b"GGUF")` and rejecting `<!DOCTYPE` or `<html` before writing payload data immediately aborts HTML responses before creating invalid files or wasting bandwidth. Non-binary or empty responses must return non-retryable errors (`DownloaderError::InvalidBinaryFormat`, `EmptyResponse`).
  3. **Zero-byte promotion guard**: Verifying `downloaded > 0` and running `GgufMetadata::open()` prior to renaming `.part` files to destination `.gguf` guarantees no 0-byte or truncated files are ever promoted into the active model catalog.
  4. **Cross-filesystem Symlink Fallback**: Symlinking models from Android shared storage (`/sdcard/Download`) into Termux private storage (`~/.nexus/models`) may fail due to Android FUSE/sdcardfs boundary restrictions. Catching `std::io::ErrorKind::Unsupported` or `CrossesDevices` and automatically falling back to file copying allows transparent imports across all storage layouts.
  5. **Curated Starters & Cleanup Workflow**: Providing 1-click starter models (Qwen 2.5 Coder 1.5B, Llama 3.2 1B/3B, Gemma 2 2B, SmolLM2 1.7B) eliminates typing errors for beginners, while `find_corrupted_models()` / `delete_corrupted_models()` cleans up previous 0-byte/HTML files with `nexus import --clean` or TUI `[Shift+X]`.
- Action: Updated `src/downloader.rs`, `src/hf.rs`, implemented `src/import.rs`, added `nexus import` CLI in `src/main.rs`, added Curated Starters and Local Importer modals to TUI in `src/ui/hub/mod.rs` and `src/ui/models_view.rs`.
- Verification: `cargo test --lib`, `cargo test`, verified instant HTML rejection on `https://huggingface.co/`, verified `nexus import --clean` deleted corrupt test files, verified all 160+ unit and integration tests passed.

## 2026-10-08 — Web UI Superpowers (Track A) & Prompt Cache Slot Persistence (Track B)
- Category: design-decision | bug
- Context: Implementing Track A (Hugging Face Search & 1-Click Download, GGUF Header Inspection, Runtime Settings Editor) and Track B (Prompt Cache KV slot persistence `--slot-save-path` and disk quota eviction) with configurable options for inference and battery safety.
- Finding:
  1. **Llama-server Slot Persistence Isolation**: When enabling `--slot-save-path`, llama.cpp expects a dedicated directory where KV state is serialized across turns. Creating this directory asynchronously prior to child process spawn and enforcing an LRU disk quota (`enforce_slot_cache_quota`) prevents cache exhaustion without requiring external daemons.
  2. **Configurable Runtime Policies with Disk Persistence**: Exposing `GET /api/config` and `POST /api/config` allows operators to modify prompt caching and battery safety thresholds dynamically from the Web UI, with automatic serialization to `config.toml` via `NexusConfig::save()`.
  3. **Zero-Allocation GGUF Header Inspection**: `GgufMetadata::open()` reads and deserializes the binary header and tensor metadata dictionary without mapping tensor buffers into RAM, enabling instant client inspection of architecture, dominant quantization, context limit, and layer geometry.
  4. **Background Download Coordination in Gateway**: Running `ModelDownloader` in a background `tokio::spawn` task while exposing atomic progress through `GET /api/models/download/status` decouples file acquisition from HTTP connection lifetimes, preventing connection timeouts on slow or mobile connections.
- Action: Implemented slot cache quota enforcement in `src/supervisor.rs`, wired `slot_save_path` throughout supervisors and CLI/daemon, added gateway endpoints (`/api/hf/*`, `/api/models/*`, `/api/config`), added Settings tab and inspection/HF modals in `web/dist/index.html`, and added comprehensive integration tests in `tests/test_gateway.rs`.
- Verification: `cargo test --locked`, `cargo clippy --locked --all-targets -- -D warnings`, `cargo fmt --check`, `bash scripts/check_penryn_opcodes.sh`, `bash scripts/build_web.sh`.

## 2026-10-08 — Embedded Zero-Dependency Web Interface, Gateway API Routing, & Termux Environment
- Category: design-decision | environment | bug
- Context: Implementing the embedded single-page Web Interface served via the Mesh Gateway (`network.gateway_port`, 8090) with PIN authentication, cluster telemetry SSE, and model orchestration across Termux ARM64 and legacy x86.
- Finding:
  1. **Zero-dependency single-bundle web embedding**: Using `include_str!("../web/dist/index.html")` with an optional `NEXUS_WEB_DIR` override provides sub-millisecond serving directly from hyper without requiring Node.js, npm, or heavy embedding crates (`rust-embed`) on edge targets.
  2. **Raw string literal prefixing in Rust 2021**: In Rust 2021 edition, raw string literals containing `#` followed by characters like `0b...` (e.g. `fill="#0b0f19"`) or `"sans-serif"` trigger compiler errors (`prefix serif is unknown`, `expected operator, found 0b0f19`). Use `r##"..."##` delimiters to escape internal `#` and quotes cleanly.
  3. **RwLock across async awaits**: Hyper route handlers must not hold `std::sync::RwLockReadGuard` across `.await` points (such as `local_active_model().await`), which fails the `Send` trait bound on `tokio::spawn`. Always scope synchronous locks inside local blocks `{ let val = lock.read().unwrap(); ... }`.
  4. **Termux shebang resolution**: Shell scripts on Android Termux fail with `bad interpreter: No such file or directory` if using `/usr/bin/env bash`. Execute scripts via `bash scripts/<script>.sh` or configure Termux-compatible interpreters.
- Action: Implemented single-page Web Hub (`web/dist/index.html`), PWA manifest, `/api/*` telemetry and PIN verification endpoints, `nexus web` CLI subcommand, and `test_gateway_serves_web_ui_and_api` integration tests.
- Verification: `cargo test --locked`, `cargo check --bins`, `bash scripts/build_web.sh`.

## 2026-10-07 — Phase 4: Models Tab Explorer, Sharded GGUF Aggregation, & Sequential Download Queue
- Category: design-decision | bug
- Context: Implementing Models tab dual-mode navigation (Local vs HF Explorer), multi-shard GGUF grouping, and automated sequential shard download queueing.
- Finding:
  1. **Primary shard path preservation**: When aggregating multi-part split GGUFs (`*-00001-of-*.gguf`) into unified catalog rows with combined sizes, the representative `path` stored in `ModelEntry.path` must always point to the first shard (`00001-of-*`), because llama.cpp's `llama-server` requires the path to shard 1 in order to automatically resolve and stream subsequent shards into memory.
  2. **Automated sequential multi-shard queue**: Downloading multi-part models over network connections is most reliable when serialized rather than parallelized on mobile ARM64/Termux targets. Streaming sibling shards sequentially through `HubCommand::StartDownloadGroup` preserves connection stability, enables atomic progress reporting (`[1/3] Downloading shard-1...`), and ensures `.part` resume state is cleanly preserved if cancelled.
  3. **Input mode protection against periodic background ticks**: In dual-mode TUI tabs where one mode accepts text input (search query), `modal_open` in the hub event loop must guard against background ticks while `hf_is_searching` is active. Otherwise, periodic catalog/peer refreshes will steal focus, reset selection indices, or trigger unwanted key aliases.
- Action: Implemented `parse_shard_info`, `aggregate_model_entries`, `ModelsTabMode` dual-mode rendering, `HubCommand::StartDownloadGroup`, and comprehensive unit/integration test coverage.
- Verification: `cargo test --locked`, `cargo clippy --locked --all-targets -- -D warnings`, `cargo fmt --check`.

## 2026-10-07 — Phase 3: Hugging Face API Client, Quant Resolution, & Modal Vertical Height Clipping
- Category: design-decision | bug
- Context: Implementing Hugging Face model resolution, GGUF quant picker with memory fit badges, and interactive token recovery modal.
- Finding:
  1. **Smart repo vs direct file URL detection**: Distinguishing bare repo IDs (`owner/repo`), repo URLs (`https://huggingface.co/owner/repo`), and direct file URLs (`.../resolve/...`) allows the single `[D] Download` entry point to serve both direct downloads and repo exploration seamlessly without separate prompts.
  2. **Ratatui popup percentage clipping**: When rendering text paragraphs inside modal popups sized with `centered_rect(percent_x, percent_y, area)`, `percent_y` must account for terminal height (e.g. 35 rows) and border padding (2 rows). Setting `percent_y = 24` gives only 8 rows total, leaving 6 visible interior lines. An 8-line paragraph will silently clip bottom lines (such as action buttons or hints). Setting `percent_y >= 40` ensures comfortable rendering across all desktop and standard terminal resolutions.
  3. **HTTP Header Case Normalization**: Reqwest normalizes header names case-insensitively. In mock HTTP test listeners, inspecting raw incoming request buffers must use `to_ascii_lowercase()` when matching header lines like `authorization: bearer ...`.
- Action: Implemented `HfClient` with `with_base_url` for mock testing, quant extraction, memory fit calculation, repo resolution in `commands.rs`, and adjusted popup sizing in `HubApp`.
- Verification: `cargo test --locked`, `cargo clippy --locked --all-targets -- -D warnings`, `cargo fmt --check`.

## 2026-10-07 — Phase 2: Non-Blocking Hub Download Worker & Multi-Shard Deletion Cleanup
- Category: design-decision | bug
- Context: Implementing in-TUI model deletion ([X]/[Delete] with confirmation modal) and interactive download cancellation ([Esc]/[C] with resume preservation).
- Finding:
  1. **Non-blocking command worker**: Previously, `spawn_hub_worker` awaited `run_download` synchronously inside its command processing loop. This froze the command receiver, making it impossible to process `HubCommand::CancelDownload` or UI requests while a download was active. Spawning downloads on a separate task and using `tokio::sync::watch::channel` allows instant cancellation signaling while keeping the command bus responsive.
  2. **Multi-shard and sidecar cleanup**: Multi-part models (e.g. `*-00001-of-00003.gguf`) must be detected and deleted as an atomic group along with any `.part` and `.part.json` sidecars to prevent multi-gigabyte disk leaks.
  3. **Download cancellation state preservation**: On receiving a cancellation signal, the downloader must gracefully flush its `BufWriter` and retain `.part` and `.part.json` so that subsequent download attempts immediately resume via HTTP Range requests (`Range: bytes=offset-`).
- Action: Implemented `download_with_cancellation`, `detect_model_shards`, `run_delete_model`, delete confirmation modal with active model protection in `HubApp`, and added integration tests verifying cancellation, resume, and shard cleanup.
- Verification: `cargo test --locked`, `cargo clippy --locked --all-targets -- -D warnings`, `cargo fmt --check`.

## 2026-10-07 — Hugging Face Token Authentication Isolation & Clippy Const Block Assertions
- Category: design-decision | bug
- Context: Adding Hugging Face token support to `NexusConfig`, `SettingsView`, and `ModelDownloader` for authenticated downloads of gated/private models.
- Finding:
  1. **Credential isolation**: Injecting `Authorization: Bearer <token>` indiscriminately into all HTTP GET requests would leak user tokens to third-party CDNs, LAN peers, and custom mirrors. Restricting bearer headers to `*.huggingface.co` and `*.hf.co` ensures security while letting redirect handlers strip auth before hitting signed S3 CDN URLs.
  2. **Resolution hierarchy**: Checking `config.toml` -> `HF_TOKEN` -> `HUGGING_FACE_HUB_TOKEN` -> `~/.cache/huggingface/token` allows zero-friction interoperability for users with existing `huggingface-cli login` installations.
  3. **Rust 1.99 Clippy const block assertion**: Running `assert!(CONST_STRUCT.field)` triggers `clippy::assertions-on-constants`, while changing it to `assert_eq!(CONST_STRUCT.field, true)` triggers `clippy::bool_assert_comparison`. In modern Rust (1.79+), the idiomatic fix is `const { assert!(CONST_STRUCT.field) };`.
- Action: Implemented `resolved_hf_token()` hierarchy, `is_huggingface_url` domain filtering, masked token display in Settings view, and applied `const { assert!(..) }` in tests.
- Verification: `cargo test --locked`, `cargo clippy --locked --all-targets -- -D warnings`, `cargo fmt --check`.

## 2026-10-07 — Mobile Responsive TUI Layout & Ratatui Sub-Area Sizing Gotcha
- Category: design-decision | bug
- Context: Responsive TUI redesign for mobile screens (Termux ARM64 / narrow terminal emulators) where width < 85 or height < 24 caused horizontal and vertical clipping.
- Finding:
  1. Horizontal split screens (Models view 50/50, Agent view 50/50) clip critically on screens < 85 columns; vertical stacking ensures both list and details fit comfortably.
  2. Tables with 8 columns (Cluster view) clip on mobile; reducing to 4 primary columns (Node/UUID, Endpoint, Role, Free RAM) preserves usability.
  3. Redundant multi-line headers (Chat view header) consume ~15% of vertical real estate on small displays (24 rows); hiding headers and compacting tab labels & footers frees up space for conversation and telemetry.
  4. **Ratatui sub-area vs root window gotcha**: When parent views subdivide their root area vertically (e.g. `chunks[0]` for tabs with height 3, `chunks[2]` for footer with height 2), each sub-chunk has `area.height < 24`. If a sub-component checks `self.layout_mode.is_compact(sub_area)`, it erroneously evaluates to compact mode even on full desktop windows (140x35+).
- Action: Implemented `src/ui/layout.rs` with `LayoutMode` (`Auto`, `Compact`, `Wide`), `ui.layout_mode` config setting, responsive popup centering, and passed root-evaluated `is_compact` down from parent render routines into sub-widgets.
- Verification: `cargo test --locked`, `cargo clippy --locked -- -D warnings`, `cargo fmt --check`, and added `test_mobile_responsive_rendering` in `tests/test_hub_ui.rs`.

## 2026-10-07 — Phase 12 §5.5 bench store feeding placement ranker
- Category: design-decision
- Context: CAPABILITY_REVIEW §5.5 on `cursor/phase12-bench-telemetry-5f3d` from `origin/main` (rebased after gateway #16).
- Finding:
  1. `GenerationMetrics` only lived in-session / thin JSONL; placement `predict_local_tok_s` used fixed backend heuristics (Vulkan 28, etc.).
  2. Persist rolling averages to `~/.nexus/bench.json` keyed by `(model_id, node_id, backend, context_size)`; override with `NEXUS_BENCH_PATH`.
  3. Prefer measured `gen_tok_s` as the prediction base (still apply layer_frac + thermal); skip quant boost when measured.
  4. `nexus bench --endpoint` works against any OpenAI surface (gateway/api/fake llama) — no GGUF required for CI.
- Action: Added `src/bench.rs`, CLI `Commands::Bench`, `PlacementRequest.bench`, chat finalize best-effort record, `tests/test_bench.rs`.
- Verification: `cargo fmt`, `clippy -D warnings`, `cargo test --locked` (store + fake-llama measure + ranker prefers 500 tok/s sample).

## 2026-10-07 — Phase 12 §5.1 mesh gateway on dedicated gateway_port
- Category: design-decision
- Context: CAPABILITY_REVIEW §5.1 / Phase 12 MVP on `cursor/phase12-mesh-gateway-5f3d` (merged as PR #16).
- Finding:
  1. Do not multiplex the mesh OpenAI front door onto `api_port` (llama-server owns 8080) or `control_port` (signed trust domain on 9998). Use `network.gateway_port` default 8090 + `gateway_enabled` (8090 avoids multi-slot ports from 8080).
  2. Inbound `/v1/*` stays unauthenticated so unmodified OpenAI clients work; when pairing is enforced, resolution only considers verified/trusted peers.
  3. Prefer byte-stream reverse proxy for `/v1/chat/completions` over re-tokenizing via `NexusClient::stream_chat` (that API yields text tokens only and would break OpenAI chunk shape).
  4. `PeerRegistry` has no model fields — resolve via local `SupervisorManager`/`DiscoveryService` `active_model` plus peer beacon `active_model`. Auto-load on miss and full hub rebind collapse are follow-ons.
- Action: Added `src/gateway.rs`, config/Settings/doctor wiring, hub bootstrap prefers local gateway, `tests/test_gateway.rs` with multi-holder `fake_llama`.
- Verification: `cargo test --locked` (incl. gateway suite). Live multi-node LAN soak still required.

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

