---

## Status legend

- ✅ **Done** — shipped (Phase 1: `cursor/phase1-p0-bugs-probe-cache-80f9`; Phase 2: `cursor/phase2-model-loop-0d08`)
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
**Done:** `open_target_selection` now uses `file_name()` (e.g. `foo.gguf`), matching Cluster `L`. Full peer catalog remains #9.

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

**9. ⬜ Remote model awareness.** Right now you can only browse *local* `.gguf` files, and remote-load blindly hopes the peer has the same file. Add a `GET /cluster/models` control-plane endpoint so the Models tab can show a merged view: local models + each peer's models (with host column). That also completes the long-term fix for #4 — you'd dispatch a path the peer actually has. Longer term: a `push`-style model transfer or a shared "download on target" command.

**10. ⬜ In-TUI downloads.** `nexus download` exists but the TUI has no way to fetch a model — a user who finds an empty Models tab (`No .gguf models found in ...`) hits a dead end. Add `[D] Download` in the Models tab: URL input + a progress gauge (downloader already supports resume/SHA-256; wire its progress into a modal). The empty-state message should also *say* this ("press D to download, or see `nexus download --help`").

**11. ⬜ Model unload parity for remote nodes.** You have `/cluster/model/unload` in the control plane, but the UI only unloads locally (`u`/`Ctrl+U`). Add remote unload in the Cluster view, and show the active model + host persistently in the footer — currently `active_model_name` gets overwritten by whichever peer you last chatted with, so the footer can claim a remote model is "the" active model while your local server is also running.

**12. ✅ Unify the two remote-dispatch code paths.** ~~Duplicated `ModelLoadRequest` construction.~~
**Done:** `HubApp::dispatch_remote_load` shared by target-selection Remote arm and Cluster `L`; callers map errors to hub vs cluster status (Cluster keeps connection-refused hint).

---

## 🟡 P2 — Chat usability

**13. ⬜ Real input editing.** The input box only supports push/pop of chars — no cursor, no Left/Right/Home/End, no Ctrl+W/Ctrl+U, no multiline, no prompt history. Add at minimum: cursor movement + prompt history on `Alt+↑/↓` (since ↑/↓ scroll). Consider `tui-textarea` rather than hand-rolling — it's a small dependency and handles paste properly (bracketed paste currently sprays `Char` events).

**14. ✅ Slash-command system.** ~~Only `/unload` special-cased.~~
**Done:** `ui/slash.rs` parser + `/`-triggered hint popup; Hub Chat Enter dispatches `/unload`, `/preset`, `/host`, `/context`, `/temp`, `/clear`, `/help`.

**15. ✅ Stop filtering banners by string prefix.** ~~Prefix-based `is_conversation_message`.~~
**Done:** `ChatMessage::status` (`role: "status"`) for UI chrome; API filter drops status by role; history renders `[Status]` dim/italic. User text like `Model 'x' is great` is preserved.

**16. ⬜ Generation context display.** You track tokens/s — also show token count vs. context budget (`1,240 / 4,096 ctx`), ideally colored as it approaches the limit, since llama.cpp silently truncates. And label the metric honestly: you're counting SSE chunks, which is usually tokens for llama-server, but say "tok/s" only if verified.

**17. ⬜ Markdown-ish rendering.** Assistant output renders as raw text — fenced code blocks lose all affordance. Even lightweight styling (dim the ` fences, background-color code spans, bold headers) makes long coding answers dramatically more readable. `tui-markdown` or a small custom highlighter.

**18. ⬜ Retry/regenerate + clear.** `[R]`egenerate last response (pop last assistant message, resend) and `/clear` are the two most-missed chat affordances. Also: pressing Enter while streaming should queue or visibly refuse — right now input is silently ignored. *(`/clear` now exists via #14; regenerate remains open.)*

---

## 🟢 P3 — Navigation, feedback & polish

**19. ⬜ Consistent keybinding scheme.** Bare `1–4` switch tabs in Models/Cluster but type digits in Chat; `Tab` cycles tabs from Chat but also from Models; `Alt+C` connects in Chat while bare `C` does nothing. Pick one global scheme (F-keys + `Alt+1..4` + `Tab`/`Shift+Tab` everywhere) and make tab-local keys non-conflicting mnemonics. Add a `?` / `F12` **help modal** listing keys for the current view — discoverability is currently 100% README-dependent, and the per-view footers already disagree with reality (#1, #2 — footers for those are now accurate).

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
| 3 | #9, #10, #13, #17, #19 | Completeness: remote catalogs, downloads, real input | ⬜ Next |
| 4 | #16, #18, #20, #22–#25 | Polish | ⬜ Open |
