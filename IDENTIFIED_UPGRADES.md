---

## Status legend

- ✅ **Done** — shipped (Phase 1: `cursor/phase1-p0-bugs-probe-cache-80f9`; Phase 2: `cursor/phase2-model-loop-0d08`; Phase 3: `cursor/phase3-completeness-6a2f`; Phase 4: `cursor/phase4-polish-e793`)
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
**Done:** Hyper control-plane server on `network.control_port` serves `GET /nexus/control/v1/models` (+ alias `/cluster/models`) with content digests. Models tab merges local + peer catalogs (holders column). Phase 10 adds `GET /blob/{digest}` Range transfer and `[T]`/`[S]` pull/push.

**10. ✅ In-TUI downloads.** ~~Empty Models tab was a dead end.~~
**Done:** Models `[D]` opens a URL modal; hardened `ModelDownloader` runs with a progress Gauge into `models_dir`; `[T]` pulls by digest from a peer; `[S]` asks a peer to fetch via `POST /blob/fetch`.

**11. ⬜ Model unload parity for remote nodes.** You have `/cluster/model/unload` in the control plane, but the UI only unloads locally (`u`/`Ctrl+U`). Add remote unload in the Cluster view, and show the active model + host persistently in the footer — currently `active_model_name` gets overwritten by whichever peer you last chatted with, so the footer can claim a remote model is "the" active model while your local server is also running. *(Deferred past Phase 4 polish.)*

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

**16. ✅ Generation context display.** ~~SSE rate labeled as tokens/s; no used/budget.~~
**Done:** Header shows `{used}/{budget} ctx` (char/4 estimate) with yellow ≥70% / red ≥90%; live rate labeled `chunks/s` (SSE chunk count). `ChatApp::context_budget` synced from Hub `selected_context`.

**17. ✅ Markdown-ish rendering.** ~~Assistant output was raw text.~~
**Done:** `ui/markdown.rs` + `pulldown-cmark` with fenced code boxes, bold/italic/headers; chat history and streaming use `render_markdown`. Status lines stay plain.

**18. ✅ Retry/regenerate + clear.** ~~No regenerate; Enter while streaming silently ignored.~~
**Done:** Chat `Ctrl+R` → `regenerate_last` (pop trailing status + last assistant, resend). Enter while streaming sets visible "Busy — Esc to abort, or wait". `/clear` already from #14.

---

## 🟢 P3 — Navigation, feedback & polish

**19. ✅ Consistent keybinding scheme.** ~~Bare `1–4` conflicted with Chat digits; no help modal.~~
**Done:** Global tabs are F1–F4 / Alt+1–4 / Tab / BackTab only (bare digits removed from Models/Cluster/Settings). Models `D` = download; Cluster `D` = disconnect. `?` / F12 opens a per-tab help modal; footer shows `[?] Help`. Chat connect remains Alt+C.

**20. ✅ Status messages that expire.** ~~Hub `status_message` written but never rendered; toasts never expired.~~
**Done:** Footer renders hub toast when set; non-red messages auto-clear after 5s on the 500ms tick (`tick_status`); Red stays sticky.

**21. ✅ `SystemProfile::probe()` runs every frame.** ~~`render_model_details` called it inside `render()`.~~
**Done:** `ModelsView` caches `cached_profile` and exposes `refresh_profile()`; Hub refreshes it on the existing 500ms tick when the Models tab is active (same pattern as `ClusterView::refresh`).

**22. ✅ Scroll offset overflow.** ~~`scroll_offset: u16` truncated at 65,535.~~
**Done:** `scroll_offset: usize` with scroll math in `usize`; clamped to `u16::MAX` only at `Paragraph::scroll`.

**23. ✅ Mouse support.** ~~No `EnableMouseCapture`.~~
**Done:** Hub + standalone chat enable mouse capture; `ui/mouse.rs` hit-tests for tab / list clicks; wheel scrolls chat. Mouse ignored while modals are open. Panic hook already disables mouse.

**24. ✅ Friendly peer names.** ~~Always `Node-<uuid8>`.~~
**Done:** mDNS TXT `name=` advertises `config.node.name` (when set and not `auto`); `PeerNode.display_name` + `friendly_name()`. Beacon wire format unchanged (no spare bytes). UDP-only peers keep `Node-{uuid8}` fallback.

**25. ✅ Small robustness wins.** ~~Missing `models_dir` → empty pane; modal Clear audit.~~
**Done:** All Hub/Cluster modals already `Clear` first. `HubApp::new` and `nexusd` call `create_dir_all` on `models_dir` (warn on failure, no panic).

---

## Suggested order of attack

| Phase | Items | Why | Status |
| --- | --- | --- | --- |
| 1 | #1–#6 (bugs) + #21 | Restores promised behavior, cheap | ✅ Complete |
| 2 | #7, #8, #12, #14, #15 | Core model-handling loop becomes trustworthy | ✅ Complete |
| 3 | #9, #10, #13, #17, #19 | Completeness: remote catalogs, downloads, real input | ✅ Complete |
| 4 | #16, #18, #20, #22–#25 | Polish | ✅ Complete |
