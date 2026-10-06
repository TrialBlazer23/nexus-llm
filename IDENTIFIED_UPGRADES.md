---

## 🔴 P0 — Actual bugs (broken or misleading behavior)

**1. The `[P] Persona` key is advertised but dead.**
`models_view.rs` renders `[P] Persona` in the Models action bar, but the Models-tab key handler in `hub.rs` (line 827) has no `Char('p')` arm. Worse, the whole preset system (`preset.rs`, `presets/*.yaml`) is only wired into the standalone `nexus client --preset` path — the Hub creates chat with `ChatApp::new(client, "default", None)`, so personas/system prompts are unreachable in the main interface.

**2. Esc-to-abort-streaming doesn't exist, despite the README promising it.**
In the Hub's Chat tab, `Esc` falls through every handler and does nothing (only the modals and settings handle Esc). There's also no cancellation machinery at all: the stream is fired with `tokio::spawn`, the `JoinHandle` is discarded, and there's no `CancellationToken` — so a runaway 2048-token generation can't be stopped. Meanwhile in standalone `run_chat_tui`, `Esc` **quits the whole app** even mid-stream. Fix: store the stream's `AbortHandle`/token in `ChatApp`, bind `Esc` to abort (not quit), and bind quit to `Ctrl+C`/`q` only when idle.

**3. Hot-swap silently drops your GPU/CPU choice.**
`open_target_selection` → pick "🛡️ Local CPU (Safe Mode)" → `request_model_load_with_gpu(path, Some(0))` → but if a model is already active, only `pending_hot_swap_path = Some(path)` is stored — the layer override is discarded. Confirming with `y` calls `execute_model_load(path)` which falls back to `config.prefer_gpu`. On a Vulkan-flaky Android target this is exactly the failure mode your own error hint warns about. Store `(path, custom_gpu_layers)` in the pending state.

**4. Remote load sends an unresolvable model path — inconsistently.**
In `execute_target_selection`, the request uses `model_path: state.model_name.clone()` — the file **stem** (no `.gguf`, no directory). In the Cluster-view `L` handler it sends `m.filename` (with extension). A remote node can only resolve these by luck. At minimum send the canonical filename consistently; better, exchange a model catalog (see P1).

**5. The transport badge lies.**
`chat.rs` shows `[USB Cable]` for any endpoint containing `127.0.0.1`/`localhost` — so plain local inference is labeled "USB Cable". Since you have a real tunnel module (`tunnel.rs`), badge off actual tunnel state (query `tunnel status` / a flag set by ADB forwarding), and show `[Local]` for the supervised local server.

**6. No panic hook — a crash bricks the terminal.**
`run_hub_tui` / `run_chat_tui` restore the terminal only on the happy path. A panic while in raw mode + alternate screen leaves the user's shell garbled. Install the standard ratatui panic hook (`std::panic::set_hook` that disables raw mode and leaves the alternate screen) at startup.

---

## 🟠 P1 — Model handling (the "complete and effective" part)

**7. Show fit-per-target in the node selection modal.** The modal lists candidates with free RAM, but doesn't say whether *this model* fits there. You already compute `[OK]/[RPC]/[OOM]` badges for the local host — extend that to each candidate (you have `free_ram_mb` from beacons): `✅ fits`, `⚠️ needs RPC offload`, `❌ won't fit`. This turns the modal from a list into a decision tool, which is your project's core pitch.

**8. Kill the magic numbers.** `10300` MB as the RPC/OOM threshold (models_view), `4096` context (hub.rs, 4+ call sites), `0.7` temp / `2048` max_tokens (chat.rs), `99` GPU layers — all hardcoded. Context size especially should be per-model adjustable (a `+`/`-` or `[C]` context selector in the model details pane, defaulting from the GGUF's `context_length` metadata you already parse, capped by the KV budget math you already have).

**9. Remote model awareness.** Right now you can only browse *local* `.gguf` files, and remote-load blindly hopes the peer has the same file. Add a `GET /cluster/models` control-plane endpoint so the Models tab can show a merged view: local models + each peer's models (with host column). That also fixes bug #4 — you'd dispatch a path the peer actually has. Longer term: a `push`-style model transfer or a shared "download on target" command.

**10. In-TUI downloads.** `nexus download` exists but the TUI has no way to fetch a model — a user who finds an empty Models tab (`No .gguf models found in ...`) hits a dead end. Add `[D] Download` in the Models tab: URL input + a progress gauge (downloader already supports resume/SHA-256; wire its progress into a modal). The empty-state message should also *say* this ("press D to download, or see `nexus download --help`").

**11. Model unload parity for remote nodes.** You have `/cluster/model/unload` in the control plane, but the UI only unloads locally (`u`/`Ctrl+U`). Add remote unload in the Cluster view, and show the active model + host persistently in the footer — currently `active_model_name` gets overwritten by whichever peer you last chatted with, so the footer can claim a remote model is "the" active model while your local server is also running.

**12. Unify the two remote-dispatch code paths.** `execute_target_selection` (Remote arm) and the Cluster-view `L` handler duplicate the same `ModelLoadRequest` construction with divergent error handling. Extract one `dispatch_remote_load(peer, model, params) -> Result<...>` helper so fixes apply once.

---

## 🟡 P2 — Chat usability

**13. Real input editing.** The input box only supports push/pop of chars — no cursor, no Left/Right/Home/End, no Ctrl+W/Ctrl+U, no multiline, no prompt history. Add at minimum: cursor movement + prompt history on `Alt+↑/↓` (since ↑/↓ scroll). Consider `tui-textarea` rather than hand-rolling — it's a small dependency and handles paste properly (bracketed paste currently sprays `Char` events).

**14. Slash-command system.** `/unload` is special-cased as a raw string compare in the hub's key router — the only command, and invisible to users. Add a `SlashCommand` parser (`/unload`, `/preset <name>`, `/host <endpoint>`, `/context <n>`, `/temp <f>`, `/clear`, `/help`) with a `/`-triggered hint popup. This is also the natural UI for fixing #1: `/preset coder` applies `presets/coder.yaml`'s system prompt + temperature to the hub chat.

**15. Stop filtering banners by string prefix.** `is_conversation_message` drops any message starting with `"Model '"`, `"Connected to"`, `"⚠️"`, etc. — fragile (a *user* typing "Model 'x' is great" loses their message) and format-coupled. Give status events their own role (`Role::System`-style enum or a separate `events: Vec<StatusEvent>` rendered inline) so conversation history is cleanly separated from UI chrome.

**16. Generation context display.** You track tokens/s — also show token count vs. context budget (`1,240 / 4,096 ctx`), ideally colored as it approaches the limit, since llama.cpp silently truncates. And label the metric honestly: you're counting SSE chunks, which is usually tokens for llama-server, but say "tok/s" only if verified.

**17. Markdown-ish rendering.** Assistant output renders as raw text — fenced code blocks lose all affordance. Even lightweight styling (dim the ` fences, background-color code spans, bold headers) makes long coding answers dramatically more readable. `tui-markdown` or a small custom highlighter.

**18. Retry/regenerate + clear.** `[R]`egenerate last response (pop last assistant message, resend) and `/clear` are the two most-missed chat affordances. Also: pressing Enter while streaming should queue or visibly refuse — right now input is silently ignored.

---

## 🟢 P3 — Navigation, feedback & polish

**19. Consistent keybinding scheme.** Bare `1–4` switch tabs in Models/Cluster but type digits in Chat; `Tab` cycles tabs from Chat but also from Models; `Alt+C` connects in Chat while bare `C` does nothing. Pick one global scheme (F-keys + `Alt+1..4` + `Tab`/`Shift+Tab` everywhere) and make tab-local keys non-conflicting mnemonics. Add a `?` / `F12` **help modal** listing keys for the current view — discoverability is currently 100% README-dependent, and the per-view footers already disagree with reality (#1, #2).

**20. Status messages that expire.** `status_message` persists until overwritten — a stale green "Active: model-x" survives the model crashing. Add timestamps and auto-clear info/success messages after 5s (keep errors until dismissed).

**21. `SystemProfile::probe()` runs every frame.** `render_model_details` calls it inside `render()` — probing `/proc` on every redraw. Cache it in the view struct and refresh on the existing 500ms tick (you already do this correctly in `ClusterView::refresh`).

**22. Scroll offset overflow.** `scroll_offset: u16` + `total_lines() as u16` truncates at 65,535 lines — reachable in a long chat with code output. Use `usize` internally, clamp to `u16` only at the `Paragraph::scroll` call.

**23. Mouse support.** No `EnableMouseCapture` anywhere. With ratatui/crossterm this is 30 lines: click to select tabs/models/peers, scroll wheel for chat history. Optional, but cheap and expected in modern TUIs.

**24. Friendly peer names.** Remote candidates show as `Node-<uuid8>` even though `config.node` has an identity name. Advertise the configured name in the discovery beacon (there's room in/around the 64-byte payload or via mDNS TXT) and fall back to the UUID prefix only if absent.

**25. Small robustness wins.** Terminal resize: you're fine (loop redraws), but `frame.render_widget(Clear, ...)` for modals — verify both modals clear first (target-selection does; check hot-swap). And on startup, if `models_dir` doesn't exist, create it or offer to — first-run experience currently shows an empty pane.

---

## Suggested order of attack

| Phase | Items | Why |
| --- | --- | --- |
| 1 | #1–#6 (bugs) + #21 | Restores promised behavior, cheap |
| 2 | #7, #8, #12, #14, #15 | Core model-handling loop becomes trustworthy |
| 3 | #9, #10, #13, #17, #19 | Completeness: remote catalogs, downloads, real input |
| 4 | #16, #18, #20, #22–#25 | Polish |.
