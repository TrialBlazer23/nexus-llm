---

## Status legend

- ✅ **Done** — shipped (Phase 1: `cursor/phase1-p0-bugs-probe-cache-80f9`; Phase 2: `cursor/phase2-model-loop-0d08`; Phase 3: `cursor/phase3-completeness-6a2f`)
- ⬜ **Open** — not yet implemented

---

## 🔴 P0 — Actual bugs (broken or misleading behavior)

**1. ✅ The `[P] Persona` key is advertised but dead.** ~~`models_view.rs` renders `[P] Persona` in the Models action bar, but the Models-tab key handler in `hub.rs` has no `Char('p')` arm.~~
**Done:** Models `[P]`/`[p]` cycles presets via `Preset::list_names` + `load_by_name`, applies system prompt / temperature / max_tokens to Hub chat through `ChatApp::apply_preset`, and shows a status toast.

**2. ✅ Esc-to-abort-streaming doesn't exist, despite the README promising it.** ~~Hub Chat Esc was a no-op; stream `JoinHandle` was discarded; standalone Esc quit mid-stream.~~
**Done:** `ChatApp` stores the stream `JoinHandle`; `abort_stream()` on Esc (Hub Chat + standalone). Quit is Ctrl+C / idle `q` only. Late tokens after abort are ignored.

**3. ✅ Hot-swap silently drops your GPU/CPU choice.** ~~Only `pending_hot_swap_path` was stored; confirm called `execute_model_load` and dropped the layer override.~~
**Done:** `pending_hot_swap: Option<(PathBuf, Option<u32>)>`; confirm calls `execute_model_load_with_gpu` so Local CPU Safe Mode (`-ngl 0`) survives.

**4. ✅ Remote load sends an unresolvable model path — inconsistently.** ~~Target selection sent `file_stem`; Cluster `L` sent `filename`.~~
**Done:** Canonical `file_name()` dispatch (Phase 1) plus peer catalogs (#9) so remotes only receive filenames they advertise.

**5. ✅ The transport badge lies.** ~~Any `127.0.0.1`/`localhost` showed `[USB Cable]`.~~
**Done:** Explicit `TransportBadge::{Local, Usb, Wifi}`. Local supervised loads → `[Local]`; ADB forward (cached via `AdbTunnelSupervisor::is_forward_active` on the 500ms tick) → `[USB Cable]`; remote endpoints → `[Wi-Fi]`.

**6. ✅ No panic hook — a crash bricks the terminal.** ~~Raw mode / alternate screen restored only on the happy path.~~
**Done:** `install_tui_panic_hook()` in `ui/mod.rs` (idempotent `Once`), called from `run_hub_tui` and `run_chat_tui`.

---

## 🟠 P1 — Model handling (the "complete and effective" part)

**7. ✅ Show fit-per-target in the node selection modal.** ~~Modal listed free RAM only.~~
**Done:** Shared `cluster::ModelFit::{Fits,NeedsRpc,WontFit}` classifies each candidate from required MB (weights+KV at `selected_context`) vs host LMK budget and `config.cluster.max_rpc_ram_mb`. Modal shows `✅ fits` / `⚠️ needs RPC` / `❌ won't fit`; Models list `[OK]`/`[RPC]`/`[OOM]` uses the same classifier (no more hardcoded `10300`).

**8. ✅ Kill the magic numbers.** ~~`10300`, bare `4096`/`99` in Hub load paths.~~
**Done:** Dynamic host+RPC budgets; `HubApp::selected_context` (default 4096) with Models `[+/-]` and `/context`; remote/local loads use `selected_context` and `config.hardware.acceleration.gpu_layers` (via `configured_gpu_layers()`). Temp/max_tokens remain on `ChatApp` (Phase 1 personas + `/temp`).

**9. ✅ Remote model awareness.** ~~Local-only Models browse; remote load hoped the peer shared the file.~~
**Done:** Hyper control-plane server on `network.control_port` (default 8081) serves `GET /nexus/control/v1/models` (+ alias `GET /cluster/models`), started from Hub and `nexusd`. Models tab merges local + peer catalogs with a host column; target selection / Cluster `L` dispatch via `control_endpoint()` and only offer remotes that advertise the filename. Push/transfer still deferred.

**10. ✅ In-TUI downloads.** ~~Empty Models tab was a dead end.~~
**Done:** Models `[D]` opens a URL modal; `ModelDownloader` runs with a progress Gauge into `models_dir`; empty state tells operators to press `D` or use `nexus download --help`.

**11. ⬜ Model unload parity for remote nodes.** You have `/cluster/model/unload` in the control plane, but the UI only unloads locally (`u`/`Ctrl+U`). Add remote unload in the Cluster view, and show the active model + host persistently in the footer — currently `active_model_name` gets overwritten by whichever peer you last chatted with, so the footer can claim a remote model is "the" active model while your local server is also running.

**12. ✅ Unify the two remote-dispatch code paths.** ~~Duplicated `ModelLoadRequest` construction.~~
**Done:** `HubApp::dispatch_remote_load` shared by target-selection Remote arm and Cluster `L`; callers map errors to hub vs cluster status (Cluster keeps connection-refused hint).

---

## 🟡 P2 — Chat usability

**13. ✅ Real input editing.** ~~Input was end-only push/pop.~~
**Done:** `ChatApp::cursor_idx` with Left/Right/Home/End/Delete; Shift|Alt+Enter newline; `Alt+↑/↓` prompt history. Hand-rolled (no `tui-textarea`). Bracketed paste still arrives as Char events.

**14. ✅ Slash-command system.** ~~Only `/unload` special-cased.~~
**Done:** `ui/slash.rs` parser + `/`-triggered hint popup; Hub Chat Enter dispatches `/unload`, `/preset`, `/host`, `/context`, `/temp`, `/clear`, `/help`.

**15. ✅ Stop filtering banners by string prefix.** ~~Prefix-based `is_conversation_message`.~~
**Done:** `ChatMessage::status` (`role: "status"`) for UI chrome; API filter drops status by role; history renders `[Status]` dim/italic. User text like `Model 'x' is great` is preserved.

**16. ⬜ Generation context display.** You track tokens/s — also show token count vs. context budget (`1,240 / 4,096 ctx`), ideally colored as it approaches the limit, since llama.cpp silently truncates. And label the metric honestly: you're counting SSE chunks, which is usually tokens for llama-server, but say "tok/s" only if verified.

**17. ✅ Markdown-ish rendering.** ~~Assistant output was raw text.~~
**Done:** `ui/markdown.rs` + `pulldown-cmark` with fenced code boxes, bold/italic/headers; chat history and streaming use `render_markdown`. Status lines stay plain.

**18. ⬜ Retry/regenerate + clear.** `[R]`egenerate last response (pop last assistant message, resend) and `/clear` are the two most-missed chat affordances. Also: pressing Enter while streaming should queue or visibly refuse — right now input is silently ignored. *(`/clear` now exists via #14; regenerate remains open.)*

---

## 🟢 P3 — Navigation, feedback & polish

**19. ✅ Consistent keybinding scheme.** ~~Bare `1–4` conflicted with Chat digits; no help modal.~~
**Done:** Global tabs are F1–F4 / Alt+1–4 / Tab / BackTab only (bare digits removed from Models/Cluster/Settings). Models `D` = download; Cluster `D` = disconnect. `?` / F12 opens a per-tab help modal; footer shows `[?] Help`. Chat connect remains Alt+C.

**20. ⬜ Status messages that expire.** `status_message` persists until overwritten — a stale green "Active: model-x" survives the model crashing. Add timestamps and auto-clear info/success messages after 5s (keep errors until dismissed).

**21. ✅ `SystemProfile::probe()` runs every frame.** ~~`render_model_details` called it inside `render()`.~~
**Done:** `ModelsView` caches `cached_profile` and exposes `refresh_profile()`; Hub refreshes it on the existing 500ms tick when the Models tab is active (same pattern as `ClusterView::refresh`).

**22. ⬜ Scroll offset overflow.** `scroll_offset: u16` + `total_lines() as u16` truncates at 65,535 lines — reachable in a long chat with code output. Use `usize` internally, clamp to `u16` only at the `Paragraph::scroll` call.

**23. ⬜ Mouse support.** No `EnableMouseCapture` anywhere. With ratatui/crossterm this is 30 lines: click to select tabs/models/peers, scroll wheel for chat history. Optional, but cheap and expected in modern TUIs.

**24. ⬜ Friendly peer names.** Remote candidates show as `Node-<uuid8>` even though `config.node` has an identity name. Advertise the configured name in the discovery beacon (there's room in/around the 64-byte payload or via mDNS TXT) and fall back to the UUID prefix only if absent.

**25. ⬜ Small robustness wins.** Terminal resize: you're fine (loop redraws), but `frame.render_widget(Clear, ...)` for modals — verify both modals clear first (target-selection does; check hot-swap). And on startup, if `models_dir` doesn't exist, create it or offer to — first-run experience currently shows an empty pane.

---

## Suggested order of attack

| Phase | Items | Why | Status |
| --- | --- | --- | --- |
| 1 | #1–#6 (bugs) + #21 | Restores promised behavior, cheap | ✅ Complete |
| 2 | #7, #8, #12, #14, #15 | Core model-handling loop becomes trustworthy | ✅ Complete |
| 3 | #9, #10, #13, #17, #19 | Completeness: remote catalogs, downloads, real input | ✅ Complete |
| 4 | #16, #18, #20, #22–#25 | Polish | ⬜ Open |
