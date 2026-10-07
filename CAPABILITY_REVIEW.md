# Nexus-LLM Capability Review & Improvement Proposal

**Document ID:** REVIEW-NEXUS-2026-10-06
**Scope:** TUI, backend and cross-device connection, model handling, model downloading and transferring
**Baseline reviewed:** commit `213ee12` ("feat: Enhance chat functionality and markdown support")
**Verification baseline at time of review:** `cargo test` — 73 tests passing across 7 suites; `cargo clippy --all-targets` — 19 lib warnings, 0 errors; no CI workflows present (`.github/` contains only `agents/*.md`).

**Implementation status (2026-10-06):** Phase 7 mesh work is **shipped** on
`cursor/phase7-control-plane-server-e680` (PR #9) + `cursor/phase7-mesh-remainder-7787`
(PR #10). Phase 9 trust is **shipped** on `cursor/phase9-trust-4865` (Ed25519
identity, signed control plane, TOFU pairing, registry verification at runtime).
Historical findings below are preserved; status markers call out what is done vs
still open. **Recommended next phase: Phase 10 (model store / LAN transfer) then
Phase 11 (placement) on `cursor/phase11-placement-intelligence-d6cb`.** Phase 8 TUI is Done.
Phase 11 placement intelligence is implemented on that branch (MemoryPlan, tensor
GGUF parse, multi-worker ranking, supervisor harden); still needs live llama.cpp
validation on target hardware.
still open. **Phase 10 (model store / LAN transfer) is Done.** Recommended next:
Phase 11 (placement intelligence). Phase 8 TUI is Done.

This document is a design review, not a change set. Every claim below cites the
file it came from so it can be checked independently. Findings are separated from
proposals, and proposals are ordered by how much capability they unlock per unit
of invasiveness.

The call-site claims — which subsystems are unreachable, which config fields are
inert — are mechanically reproducible:

```bash
./scripts/verify_review_findings.sh
```

---

## Status snapshot (post–Phase 9)

| Area | Status | Where |
|---|---|---|
| Control-plane HTTP server on `control_port` 9998 | **Done** | `src/control_plane_server.rs`; hub / nexusd / host / worker |
| mDNS TXT `ctrl` + UDP fallback to local `control_port` | **Done** | `src/mdns.rs`, `src/discovery.rs` (beacon v2 for on-wire ctrl deferred) |
| Human-readable node name (`name=` TXT + `PeerNode.display_name`) | **Done** | mDNS + UI `label()`; beacon still has no name field |
| Zero-config host resolution (`find_best_host` fallback + hub discovery) | **Done** | `src/client.rs`, hub bootstrap in `src/main.rs` |
| File logging + `nexus doctor` | **Done** | `src/logging.rs`, `src/doctor.rs` |
| Settings displayed ⇒ consumed | **Done** for Settings UI | wired ram%/mmap/enable_rpc/prefer_adb/rpc binary/name; hid FallbackCpu; added control_port |
| `GET /models` (filename catalog) | **Done** (restored on main merge) | `control_plane_server` GET `/nexus/control/v1/models` |
| SSE `/events` | **Deferred** | later |
| `POST /pair`, Ed25519, signed control plane | **Done** | `src/node_identity.rs`, `src/trust_auth.rs`, `src/control_plane*.rs` |
| Peer registry as runtime SoT | **Done** | `src/registry_runtime.rs`, `src/discovery.rs`, Cluster UI |
| TUI responsiveness / hub split | **Done — Phase 8** | `src/ui/hub/` command/event + ChatEntry |
| Model store / LAN transfer | **Open — Phase 10** (PR #13 draft) | §4 |
| Placement intelligence | **Done on branch** — Phase 11 | `src/cluster/{memory,split,rank}.rs`, `gguf.rs`, `supervisor.rs` |
| Model store / LAN transfer | **Done — Phase 10** | `src/store.rs`, blob routes, hardened `downloader.rs`, Models `[D]`/`[T]`/`[S]` |

---

## Executive summary

The codebase is well-factored for its size, the binary beacon protocol and GGUF
reader are genuinely good primitives, and the memory-safety framing is the right
instinct for an Android-first mesh. The problems are not code quality problems.
They are integration problems: several subsystems are fully implemented but never
connected to anything, so the headline user-facing promises do not hold at
runtime.

The five findings that mattered most at review time (with current status):

1. ~~**There is no HTTP server for the control plane.**~~ **Resolved (Phase 7 /
   PR #9).** `src/control_plane_server.rs` (hyper) serves
   `POST /nexus/control/v1/{state,model/load,model/unload}` on
   `network.control_port` (default 9998) from hub, nexusd, host, and worker.
   `SupervisorManager` is the shared inference owner. `GET /models` catalog is
   available (filename-based; content digests are Phase 10). SSE `/events` still
   deferred. **`POST /pair` and signed POSTs are Done (Phase 9).**

2. ~~**Auto-discovery of a chat host cannot succeed with a default config.**~~
   **Resolved (Phase 7 remainder / PR #10).** `resolve_from_discovery` prefers a
   pinned ready anchor, then `find_best_host`, then static peers. Hub bootstrap
   order: `default_host` → PreferAdbTunnel USB → discovery → localhost.

3. ~~**Ten configuration options are inert.**~~ **Mostly resolved for Settings
   (PR #10).** Settings now wires `node.name`, `max_ram_usage_percent`, `mmap`,
   `enable_rpc`, `prefer_adb_tunnel`, `rpc_server_binary`, and exposes
   `control_port`. FallbackCpu was removed from the Settings UI. Still unused
   in TOML-only schema (not shown): `runtime_role`, `capabilities`,
   `cpu_threads_batch`, `mlock`, `fallback_to_cpu`.

4. **There is no device-to-device model transfer.** *(Still open — Phase 10.)*
   The user-facing concept of a "personal local AI network" implies that a model
   downloaded once is available everywhere. Today each node must independently
   fetch from the internet, and `ModelDownloader` is reachable only from
   `nexus download` on the CLI — it is not wired into the TUI at all.

5. ~~**Nothing on the wire is authenticated.**~~ **Partially resolved (Phase
   9).** Control-plane `POST` routes require Ed25519 signatures, replay
   protection, and pairing when enforced; chat/RPC routing uses verified registry
   peers plus `allowed_peer_ids`. **Beacons remain advisory** (CRC only; no
   beacon HMAC yet) — forged UUIDs no longer rebind chat when pairing is on.

---

## 0. Cross-cutting: subsystems that are built but not connected

Before the area-by-area review, it is worth naming the pattern, because it
accounts for most of the gap between the documented system and the running one.

| Subsystem | Where it lives | Status at runtime |
|---|---|---|
| Control-plane request handlers | `src/control_plane.rs` + `src/control_plane_server.rs` | **Done** — hyper server binds them on `control_port` |
| Control-plane state fetch | `src/control_plane.rs` (`fetch_state`) | Signed when pairing enforced; drives registry verifier |
| Remote unload dispatch | `src/control_plane.rs` (`dispatch_unload_model`) | Signed when pairing enforced; TUI remote unload still limited |
| Peer lifecycle registry | `src/peer_registry.rs` + `src/registry_runtime.rs` | **Done** — reaper/verifier; RPC/chat gate on `eligible_rpc_peers` |
| Chat template engine | `src/preset.rs` (`format_prompt`, `ChatTemplate`) | No callers; llama-server applies its own template |
| Non-streaming completion | `src/client.rs` (`complete_chat`) | No callers |
| ADB tunnel view | `src/ui/tunnel_view.rs` | Not wired into any hub tab; tested but unreachable |
| Model downloader | `src/downloader.rs` | CLI `nexus download` only; absent from the TUI |
| Structured logging | `src/logging.rs` | **Done** — file subscriber in `nexus` / `nexusd` (`~/.nexus/logs/`) |
| `nexus doctor` | `src/doctor.rs` | **Done** — CLI probes binaries, ports, profile, config |

Two consequences were worth stating at review time. First, the test suite could
pass while the application did not work as documented, because tests exercised
handlers directly rather than through a server — **the control-plane server and
`tests/test_control_plane_server.rs` close that gap for load/unload/state**.
Second, `src/ui/settings_view.rs` was actively misleading — **Phase 7 remainder
enforces displayed ⇒ consumed for Settings fields**; remaining inert keys are
TOML-only and not shown.

**Recommendation.** Treat "is it reachable from a user action?" as an acceptance
criterion for every subsystem, and add an integration test layer that drives
behavior through the real entry points rather than through library functions. A
small fake `llama-server` (an HTTP stub serving `/health`, `/v1/models`, and a
canned SSE stream) would make this cheap and would also let the supervisor,
client, and hub be tested end to end without a GGUF file or a GPU.

---

## 1. Backend and cross-device connection

### 1.1 The control plane is a client without a server (blocker)

> **Status (2026-10-06): Resolved for MVP.** See PR #9 /
> `src/control_plane_server.rs`. Routes served:
> `POST /nexus/control/v1/{state,model/load,model/unload}` on dedicated
> `control_port` (9998). Still deferred: `GET /models`, SSE `/events`,
> `POST /pair`.

`src/control_plane.rs` defines a complete, well-validated protocol: versioned
requests, identity checks, response size caps, and policy validation in
`validate_state`. The client half is wired up — `src/ui/hub.rs` calls
`dispatch_load_model` from both `execute_target_selection` and the Cluster tab's
`[L] Load Model` key. ~~The server half does not exist.~~

The failure is also mis-reported to the operator. `src/ui/hub.rs` maps the
resulting transport error to the hint "node has no active server on port 8080.
Launch model locally on that device first," which sends the operator chasing a
configuration problem that cannot be fixed, because the endpoint is unimplemented
rather than unstarted. *(Hint text updated in PR #9 to reference the control
plane / control_port.)*

**Proposal.** Add a `src/server.rs` that binds a typed HTTP control plane and run
it from every entry point — `nexus` (hub), `nexusd`, and `nexus worker` — not
just the daemon. Endpoints to serve, matching the paths the client already uses:

- `POST /nexus/control/v1/state` → `ControlPlaneState`
- `POST /nexus/control/v1/model/load` → `handle_load_model`
- `POST /nexus/control/v1/model/unload` → `handle_unload_model`

Additions worth making at the same time, because they are cheap once a server
exists and expensive to retrofit:

- `GET /nexus/control/v1/models` — the node's local model catalog (filename,
  size, SHA-256, GGUF architecture, quantization). This is the foundation for
  both the placement planner (§3.8) and model transfer (§4.9).
- `GET /nexus/control/v1/events` — an SSE stream of node events (model loaded,
  crashed, thermal state changed, download progress) so peers observe state
  changes instead of polling.
- `POST /nexus/control/v1/pair` — see §1.5.

On dependency choice: `AGENTS.md` constrains the crate list, and adding `axum`
pulls a large tree. `hyper` is already in the dependency graph transitively via
`reqwest`, so a hand-rolled router over `hyper::server::conn` is viable and keeps
the footprint small. If a framework is preferred, `axum` is the conventional
choice and is pure Rust with no C bindings, so it does not conflict with
Directive 4's intent — but that is a decision worth recording explicitly in
`AGENT_LEARNINGS.md` rather than made implicitly. *(Decision recorded: hand-rolled
hyper in PR #9.)*

### 1.2 The control plane would collide with llama-server on port 8080

> **Status (2026-10-06): Resolved.** Dedicated `network.control_port` (default
> 9998). mDNS TXT `ctrl`; UDP peers fall back to local config (beacon v1 has no
> spare field; beacon v2 deferred).

`PeerNode::api_endpoint()` returns `http://{ip}:{api_port}`, and `api_port`
defaults to 8080 — the same port `llama-server` binds. `dispatch_load_model`
posts the control path to that endpoint. Even with a server implemented, one
process must own the port, and the OpenAI-compatible surface is owned by the
llama.cpp subprocess.

**Proposal.** Give the control plane its own port (for example 9998, adjacent to
the discovery port) and carry it in both transports: a new `ctrl` field in the
beacon's reserved space, and a `ctrl` TXT property in mDNS. Keep `api_port`
meaning strictly "where the OpenAI-compatible inference API is." This also
cleanly separates the two trust domains: the control plane needs authentication,
the inference API may be left open on a trusted LAN.

A reverse proxy on 8080 that fronts both surfaces is the alternative, and it has
the advantage of a single port to forward over ADB — but it puts the Rust process
in the streaming path for every token, which is exactly the overhead the current
design avoids.

### 1.3 Auto-discovery is gated behind an anchor that defaults to unset

> **Status (2026-10-06): Resolved (PR #10).** Anchor preferred when set and
> healthy; otherwise `find_best_host`. Hub no longer skips discovery for a
> silent localhost fallback.

`NexusClient::resolve_from_discovery` loops on `resolve_primary_compute_anchor`,
which short-circuits on `self.config.network.anchors.primary_compute_id?`. With
the default `AnchorConfig::default()` that is `None`, so the loop never resolves
a peer and falls through to probing `static_peers` — also empty by default —
before returning `DiscoveryTimeout` after ten seconds.

The scoring function that used to handle this, `find_best_host`, still exists but
is `#[deprecated]` and has no callers. The migration from "best host by score" to
"explicitly pinned anchor" removed the zero-configuration path without replacing
it.

**Proposal.** Restore automatic selection as the fallback when no anchor is
pinned: prefer a pinned anchor if present and healthy, otherwise rank healthy
hosts. The existing scoring in `find_best_host` (readiness, Vulkan, free RAM,
thermal penalty) is a reasonable starting point, but it should rank on *measured*
capability rather than advertised RAM once §3.8 lands. Undeprecate or replace it,
and add a test asserting that a default-configured client resolves a host that is
only advertising via beacon.

### 1.4 The peer registry does not participate at runtime

> **Status (2026-10-06): Done (Phase 9).** `registry_runtime` reaper/verifier;
> `rpc_candidates` and chat resolution intersect verified + paired peers when
> enforcement is on.

`src/peer_registry.rs` implements the lifecycle the design spec describes —
`Discovered`, `Verifying`, `Healthy`, `Stale`, `Removed`, `Rejected` — with
endpoint-conflict detection and metadata bounds. It is well tested. It is also
inert: `expire`, `remove_terminal`, `mark_verified`, and `eligible_rpc_peers`
have no callers in `src/`.

What actually drives the UI and all selection logic is the parallel
`peers: HashMap<Uuid, PeerNode>` in `DiscoveryService`, pruned opportunistically
inside `get_active_peers` by `retain`. So the system maintains two peer stores
with different semantics, and the one with the weaker guarantees wins. In
particular, `verified` is never set to `true` anywhere, which means
`eligible_rpc_peers` would return an empty list even if it were called, and the
`EndpointConflict` protection in `observe` never engages (it is gated on
`peer.verified`).

**Proposal.** Make the registry the single source of truth and reduce
`DiscoveryService` to a producer of observations:

1. Run a reaper task that calls `expire` and `remove_terminal` on a timer, and
   emit `DiscoveryEvent`s so the UI reacts to transitions instead of polling.
2. Call `mark_verified` from a control-plane verification step: on first
   observation, `fetch_state` the peer, run the existing `validate_state`, and
   promote to `Healthy` on success or `reject` with a reason on failure. This is
   what `fetch_state` and `validate_state` were clearly built for.
3. Change `rpc_candidates` and the target-node selector to source from
   `eligible_rpc_peers` so that unverified peers cannot receive offloaded layers.
4. Surface `PeerLifecycle` and `rejection_reason` in the Cluster tab. Right now a
   peer that fails validation is indistinguishable from one that is merely quiet,
   which makes field debugging guesswork.

### 1.5 Nothing on the wire is authenticated or integrity-protected

> **Status (2026-10-06): Done (Phase 9).** Ed25519 identity (`~/.nexus/node.key`),
> signed control-plane POSTs, `POST /nexus/control/v1/pair` with rotating 6-digit
> code, `paired_peers` in config. Beacons still unauthenticated (deferred HMAC).

The beacon carries a 16-byte UUID and a CRC-16-CCITT over the first 62 bytes.
CRC detects accidental corruption; it provides no defense against a crafted
packet. `start_listener` accepts any well-formed beacon and inserts it into the
peer map keyed by the UUID in the packet. `record_service_endpoint` does the same
for mDNS TXT records, which are equally unauthenticated.

Concretely, on a shared network — a café, a dorm, a conference, a guest VLAN —
an attacker can: forge a beacon with a known node's UUID and a different source
IP, causing every TUI client to rebind its chat endpoint to the attacker and
hand over every prompt and response; advertise `RPC_READY` with a large
`free_ram_mb` to win RPC candidate selection and receive offloaded model layers;
or, once §1.1 lands, drive `POST /model/load` on any node.

`network.security.require_pairing` and `allowed_peer_ids` exist, but
`require_pairing` is consulted in exactly one place — the `rpc_candidates`
filter. It does not gate the peer map, the chat endpoint rebinding, or anything
inbound.

**Proposal.** Add a trust layer sized to a personal mesh — not PKI, but not
nothing either:

1. **Stable node keypair.** Generate an Ed25519 keypair on first run alongside
   the existing `node.id`, and derive the node ID from the public key so identity
   is self-certifying. Store the private key at `~/.nexus/node.key` with `0600`.
2. **Pairing with a short code (trust on first use).** `POST /pair` with a
   6-digit code displayed on the target device's TUI, exchanging public keys over
   the LAN. Persist paired peers to `allowed_peer_ids`. This is the UX people
   already understand from Bluetooth and Chromecast.
3. **Signed control plane.** Require a signature over `(body, timestamp, nonce)`
   on every control-plane request from a paired key, and reject stale timestamps
   to stop replay. Model load, unload, and transfer are the operations that must
   not be open.
4. **Authenticated beacons.** The beacon is 64 bytes and byte-budgeted, so a full
   signature does not fit. Two workable options: truncate an HMAC over the packet
   using a mesh-wide pre-shared key derived at pairing time and put it in the
   reserved bytes; or treat the beacon as strictly advisory — a hint that
   something exists at an address — and require control-plane verification (§1.4)
   before any peer is trusted for routing or offload. The second is simpler and
   composes with the registry work, and is the recommended path.
5. **Make `require_pairing` mean what it says** across every inbound and
   routing decision, and default it to `true` once pairing exists.

The honest framing for the docs: today Nexus-LLM is safe on a network you fully
control and unsafe on any network you share. That should be stated in the README
until the above lands.

### 1.6 Discovery resource hygiene

Several issues that are individually small and collectively meaningful on a
battery-powered phone:

- **`SystemProfile::probe()` on every beacon.** The broadcaster calls it once per
  tick (default 2000 ms), and `send_probe_to` calls it once per target per probe.
  Each call reads `/proc/meminfo`, reads `/proc/cpuinfo`, stats five Vulkan
  library paths, and walks every `PATH` directory looking for `vulkaninfo`. The
  static parts — architecture, backend, thread count, Vulkan presence — cannot
  change during a process lifetime and should be probed once and cached, leaving
  only the `/proc/meminfo` read on the hot path.
- **A fresh socket per probe.** `send_probe_to` binds a new `UdpSocket` for every
  target on every call. `send_probe` then loops over all targets, so a probe
  across a handful of broadcast addresses and static peers creates and drops a
  socket per address. One long-lived send socket is sufficient.
- **Unbounded maps.** `extra_targets` accumulates the discovery address of every
  peer ever seen and is never pruned. `last_unicast_replies` accumulates an entry
  per source IP forever. Both are fed directly from network input, which makes
  them a slow memory leak that a hostile or merely busy network can accelerate.
  Bound both by `max_peers` and evict by age.
- **No jitter on the broadcast interval.** All nodes beaconing on a fixed 2000 ms
  period will phase-lock into bursts. Add ±20% jitter.
- **Beacon model-name truncation can split UTF-8.** `encode` copies
  `min(len, 24)` bytes of `active_model` and `decode` recovers it with
  `from_utf8_lossy`, so a multi-byte character straddling byte 24 becomes a
  replacement character. Truncate on a character boundary when encoding.

### 1.7 mDNS lifecycle is incomplete

- `start_mdns` reads `*self.rpc_port.read().await` once at registration time. A
  node that later becomes an RPC worker via `set_rpc_status` keeps advertising
  `rpc=0` forever, so mDNS-only peers will never see it as offload-capable.
  Re-register on capability change.
- There is no `unregister` or `shutdown` on exit. `MdnsBackend::shutdown` exists
  and is never called, so peers rely on TTL expiry to notice a departed node.
- `set_mdns_enabled(false)` sets health to `Stopped` and returns. It does not
  stop the browse task and does not unregister the service, so the node keeps
  advertising and keeps ingesting peers after the operator turns the feature off
  in Settings. The toggle is cosmetic in the off direction.
- `endpoint_from_resolved` calls `to_socket_addrs()`, a **blocking DNS
  resolution**, inside the async browse task. On a flaky network this stalls a
  Tokio worker thread. Use the `ResolvedService` address list directly, which
  mDNS already provides, instead of re-resolving the hostname.

### 1.8 IPv6 is absent end to end

`create_listener_socket` builds `Domain::IPV4`, `get_broadcast_addresses` filters
to `AF_INET`, and `get_local_ip` returns an IPv4 fallback. IPv4 broadcast is also
the transport most likely to be filtered by consumer access points with client
isolation — the exact failure the project already documented in
`AGENT_LEARNINGS.md`. IPv6 link-local multicast (`ff02::1`) is frequently
permitted where IPv4 broadcast is dropped, and mDNS already works over IPv6.

**Proposal.** Dual-stack the listener, add IPv6 link-local multicast as a third
discovery path alongside UDP broadcast and mDNS, and prefer whichever backend is
reporting `Healthy`. The `BackendHealth` plumbing to display this already exists
in `ClusterView`.

### 1.9 Transport: ADB is the only non-Wi-Fi path

`src/tunnel.rs` is solid for what it does, but the design treats "USB" as
synonymous with "a phone attached to this specific machine over ADB." In an
N-node mesh that is asymmetric: only a node with `adb` and a cable can use it,
and `resolve_transport_endpoint` always forwards to *some* authorized device
rather than to the device the operator actually selected. `setup_tunnel` with
`specific_serial: None` picks `devices.into_iter().find(|d| d.authorized)` — the
first authorized device, which is arbitrary with two phones plugged in.

Worth considering: Wi-Fi Direct or a direct AP-less link for phone-to-phone, and
plain ADB-over-TCP as a lower-friction alternative to cables. At minimum, let the
operator pin a device serial, and surface `TunnelView` (§2.8) so the feature is
reachable.

---

## 2. TUI

The TUI is the strongest part of the project. Markdown rendering with boxed code
blocks, per-turn telemetry badges, stream abort, preset modals, and the unified
tab shell are all genuinely good. The issues are architectural rather than
cosmetic, and they concentrate in one place: the event loop does too much
synchronously.

### 2.1 Long operations run inline in the event loop and freeze the UI

`run_hub_tui` awaits every action directly in the `tokio::select!` arm that
handles the keypress. Several of those actions are slow:

- `ProcessSupervisor::spawn_with_fallback` can take up to 4 s monitoring Vulkan
  init, then up to 15 s in `wait_until_ready` on the GPU path, then up to 20 s
  more on the CPU fallback path. Worst case is roughly 39 seconds.
- `dispatch_load_model` carries a 10 s timeout.
- `open_target_selection` and every `cluster_view.refresh()` take the discovery
  lock and re-probe the system.

During any of these the loop is not draining `event_stream` or the `StreamMsg`
receiver, so the terminal is completely unresponsive: no repaint, no spinner, no
key handling, and in-flight chat tokens back up in the channel. Pressing Enter on
a model gives the operator a frozen screen for up to half a minute with no
feedback, which reads as a crash.

**Proposal.** Introduce a command/event split, which is the standard shape for
this problem:

- A `HubCommand` enum (`LoadModelLocal`, `LoadModelRemote`, `Unload`,
  `RefreshPeers`, `StartDownload`, `TransferModel`, …) sent over an `mpsc` to a
  worker task.
- A `HubEvent` enum (`ModelLoadProgress`, `ModelLoaded`, `ModelFailed`,
  `PeersUpdated`, `DownloadProgress`, …) sent back.
- The event loop only mutates state and draws. No `.await` on anything that is
  not a channel receive.

This unlocks the things currently impossible to express: a progress spinner with
a phase label ("probing Vulkan… / loading weights… / waiting for /health"),
cancellation of an in-flight load, concurrent loads on two different remote
nodes, and downloads that continue while the operator chats.

It also fixes a related problem: `ProcessSupervisor::spawn_with_fallback` reports
nothing until it either succeeds or fails. The phases it already walks through
internally are exactly what the operator needs to see, and a `watch` channel of
`SupervisorState` is already in place to carry them — `subscribe()` exists and
has no callers.

### 2.2 Per-frame filesystem I/O and GGUF parsing

`ModelsView::render_model_details` calls `GgufMetadata::open(&m.path)` *inside
render*. That is a file open plus a full parse of every metadata key-value pair —
which for a modern model includes the tokenizer vocabulary as a `Vec` of
128,000-plus `GgufValue::String` allocations (§3.4) — on every single frame. The
same function also calls `SystemProfile::probe()` per frame, and
`ClusterView::render_local_telemetry` calls `SystemProfile::probe_vulkan()` per
frame.

With a 500 ms refresh tick plus a redraw on every keystroke, the Models tab is
re-parsing a multi-megabyte metadata blob several times per second. On a Core 2
Duo this will be visibly janky; on a phone it is wasted battery.

**Proposal.** Render must be pure. Cache `GgufMetadata` on `ModelEntry` at scan
time (`scan_models_dir` already parses it once — keep the result instead of
discarding it), cache the `SystemProfile` on the view and refresh it on the
existing tick, and cache `probe_vulkan` for the process lifetime. Cache rendered
markdown `Line`s per message too, keyed by message index, so `render_chat_history`
stops re-parsing the entire conversation every frame.

### 2.3 Scroll offsets diverge from what is displayed

`ChatApp::total_lines` counts `msg.content.lines()` — logical lines, before
wrapping. `render_chat_history` builds its own line vector and computes
`max_scroll` from `lines.len()`, then hands the `Paragraph` a
`.wrap(Wrap { trim: false })`, which expands one logical line into many visual
ones. Three different line counts are therefore in play: the one
`total_lines` returns (used by `Up`/`PageUp` key handling), the one render
computes, and the true wrapped count that the widget scrolls by.

The practical symptom is that scrolling back through a conversation containing
long paragraphs or wide code blocks skips content and cannot reach the top.

**Proposal.** Compute the wrapped line count once per frame from the laid-out
lines and the viewport width, store it on the app, and have key handling scroll
against that number. A `ratatui` `Scrollbar` fed from the same value would also
give the operator a position indicator, which the chat view currently lacks
entirely.

### 2.4 `message_metrics` can desynchronize from `messages`

`ChatApp` keeps telemetry in a `Vec<Option<GenerationMetrics>>` parallel to
`messages`, and `render_chat_history` indexes it with the message index. Most
call sites correctly push to both, but `src/ui/hub.rs` has four that do not:

- line 256–257 (`execute_target_selection`): `messages.clear()` then
  `messages.push(...)`, leaving `message_metrics` at its old length
- line 316 (`unload_active_model`): `messages.push(...)` with no metrics push
- line 439–440 (`execute_model_load_with_gpu`): `messages.clear()` then
  `messages.push(...)`, same as above
- line 713 (crash reporting in the refresh tick): `messages.push(...)` with no
  metrics push

After any of these the two vectors are misaligned, and telemetry badges attach to
the wrong responses for the rest of the session.

**Proposal.** Delete the parallel vector. Make metrics a field on a richer
message type so they cannot drift:

```rust
pub struct ChatEntry {
    pub message: ChatMessage,
    pub kind: EntryKind,              // Dialogue | Notice | Error
    pub metrics: Option<GenerationMetrics>,
    pub rendered: OnceCell<Vec<Line<'static>>>,  // also fixes §2.2
}
```

This single change fixes three findings at once — the desync here, the per-frame
markdown re-render in §2.2, and the banner-filtering fragility in §2.5.

### 2.5 System banners are identified by English string prefixes

`ChatApp::is_conversation_message` decides what to send to the model by checking
whether the content starts with `"Model '"`, `"Connected to"`, `"⚠️"`,
`"Model unloaded"`, `"Disconnected from"`, or `"Loaded '"`. The intent is right —
`AGENT_LEARNINGS.md` records the bug it was written to fix — but the mechanism is
fragile in both directions. A model whose answer begins "Connected to the
database…" is silently dropped from its own conversation history, and any new
banner wording added to `hub.rs` is silently included in the prompt until someone
remembers to extend the prefix list. It also breaks entirely if the UI is ever
localized.

**Proposal.** The `EntryKind` field from §2.4. Classification belongs at
construction, not at inspection.

### 2.6 Hot-swap discards the operator's chosen execution target

`request_model_load_with_gpu` stores only the path in `pending_hot_swap_path` when
a model is already loaded, dropping `custom_gpu_layers`. Confirming the modal
calls `execute_model_load(target_path)`, which forwards `None`, and
`execute_model_load_with_gpu` then falls back to the config default.

So the sequence "a model is loaded → pick a new model → choose **Local CPU (Safe
Mode)** → confirm hot-swap" silently launches with GPU layers. That is precisely
the escape hatch `AGENT_LEARNINGS.md` documents for Adreno driver stalls, and it
does not work in the one situation where an operator is most likely to reach for
it — after the GPU path has already misbehaved.

**Proposal.** Make the pending state carry the full intent (path, gpu layers,
context size, target node), and have the hot-swap modal display it so the
operator can confirm what will actually happen. The target-selection and
hot-swap modals should compose rather than overwrite each other.

### 2.7 TUI mode produces no logs at all

> **Status (2026-10-06): Resolved (PR #10).** `src/logging.rs` installs a file
> subscriber writing `~/.nexus/logs/nexus-<pid>.log` from `nexus` and `nexusd`
> (`--log-level` / `RUST_LOG`). In-TUI log viewer still deferred.

`src/main.rs` never installs a `tracing` subscriber. Only `src/daemon.rs` does.
Every `info!`, `warn!`, and `error!` in the discovery service, supervisor, and hub
is therefore discarded when running `nexus`. When a cross-device load fails, the
operator gets a one-line colored status message and nothing else.

A subscriber writing to stdout would be worse than nothing here, since stdout is
the alternate screen.

**Proposal.** Install a file-based subscriber in `main.rs` writing to
`~/.nexus/logs/nexus-<pid>.log` with `RUST_LOG`/`--log-level` control, and add a
log viewer pane to the TUI (or at minimum print the log path on exit). This is
the single highest-leverage debuggability change available, and it is small.

### 2.8 Missing surfaces

- **No download UI.** Fetching a model requires dropping to a shell and running
  `nexus download <url> -o <path>`. For a "personal AI network" the model
  catalog is the primary object the operator manipulates; it should be a
  first-class tab (§4.7).
- **`TunnelView` is unreachable.** It is implemented and tested, but the hub has
  four tabs and none of them is Tunnel. `BUILD_PLAN.md` Phase 6 specifies five
  tabs including `[F4: Tunnel]`; the implementation diverged.
- **Context size is not selectable.** `4096` is hardcoded at four places in
  `hub.rs` (lines 238, 362, 418, 953). Context length is the single most
  important memory/capability tradeoff a local-model operator makes, and it is
  the one knob the TUI does not expose.
- **No session history.** `SessionLogger` writes JSONL to `~/.nexus/history/` and
  nothing ever reads it back. There is no way to resume yesterday's conversation.
- **No generation controls beyond temperature, top-p, and max tokens.** No seed,
  repeat penalty, min-p, stop sequences, or JSON/grammar-constrained output —
  all of which llama-server supports and all of which matter for real use.
- **No model-load progress or error surfacing in Models.** The stderr ring
  buffer in `ProcessSupervisor` (`last_stderr_lines`) is only consulted on crash
  detection; it should be viewable on demand.

### 2.9 Smaller TUI notes

- `render_input_box` renders a single `Line`, so the `Shift+Enter` multi-line
  support in `handle_key_input` inserts a `\n` that is never displayed as a line
  break and the 3-row input box cannot grow. Multi-line input is half-implemented.
- The transport badge in `render_header` infers "[USB Cable]" from the endpoint
  containing `127.0.0.1` or `localhost`. A locally-loaded model is not a USB
  tunnel, so the badge is wrong in the most common case.
- Peers display as `Node-<first 8 hex of UUID>` everywhere, because neither the
  beacon nor the mDNS TXT record carries a name. `node.name` exists in config and
  is inert (§0). Human-readable names are a small change with an outsized effect
  on whether a multi-device mesh feels manageable.
  *(**Resolved for mDNS/UI in PR #10:** TXT `name=` + `PeerNode.display_name` /
  `label()`. Beacon still has no name field; UDP-only peers keep UUID labels
  until mDNS arrives.)*
- `&peer.uuid.to_string()[..8]` and `[..13]` slice a `String` by byte index in
  several places. Safe for a hyphenated UUID, but it is a pattern that panics the
  moment it is pointed at a display name.
- Key handling is duplicated per tab in a ~300-line `match` in `run_hub_tui`, with
  `1`–`4` meaning "switch tab" in some tabs and not others, and `u`/`U` bound in
  three places. A declarative keymap would shrink this and make the help footer
  derivable rather than hand-maintained.

---

## 3. Model handling

### 3.1 The memory model is too crude to be trusted

This matters more than anything else in this section, because the memory guard is
load-bearing: it decides whether a model runs at all, and both over- and
under-estimating have real costs (a refused load the device could handle, or a
`SIGKILL` from the Android LMK).

Four distinct inaccuracies:

- **The fallback KV estimate is a flat 200 KB per token.**
  `SystemProfile::estimate_kv_cache_bytes` returns `ctx * 200 KB` regardless of
  model, so at 4096 tokens it always claims 800 MB. For a 1.5B model with 28
  layers the true FP16 KV cache is roughly 100 MB — an 8× overestimate that
  rejects loads the device handles comfortably. This is the path
  `hub.rs:362` and `supervisor.rs` take, because they use file size and context
  rather than parsed GGUF metadata.
- **`can_safely_load` counts the model file as anonymous memory.** With `mmap`
  enabled (the default), weights are page cache backed by the file, not
  anonymous RSS. They are evictable under pressure and do not count against the
  LMK threshold the same way. Conversely `mlock` makes them unevictable. The
  guard does not distinguish these cases, and the config flags that would select
  between them are inert (§0).
- **Compute buffers are ignored.** llama-server allocates a compute graph and
  batch buffers beyond weights and KV cache — hundreds of megabytes at larger
  batch sizes, and allocated on the GPU under Vulkan rather than in system RAM.
  The guard accounts for neither.
- **KV quantization is not modeled.** `exact_kv_cache_bytes` hardcodes 2 bytes
  per element. `--cache-type-k q8_0`/`q4_0` halves or quarters that, which is the
  standard technique for fitting long contexts on a phone. It is not expressible.

**Proposal.** Replace the single boolean guard with an explicit budget model:

```rust
pub struct MemoryPlan {
    pub weights_mb: u64,          // from GGUF tensor data size
    pub weights_resident_mb: u64, // 0 if mmap && !mlock, else weights_mb
    pub kv_cache_mb: u64,         // from GGUF dims × ctx × kv cache dtype
    pub compute_buffer_mb: u64,   // from batch size and arch, measured empirically
    pub headroom_mb: u64,
    pub verdict: Verdict,         // Fits | FitsWithOffload | FitsIfQuantizedKv | Exceeds
}
```

Then report the breakdown in the Models tab instead of a single green/red badge,
and let the planner suggest the remediation it can see — "reduce context to
2048", "quantize KV to q8_0", "offload 12 layers to <peer>". The data to do this
is already parsed; only the arithmetic and the presentation are missing.

Calibrating `compute_buffer_mb` wants real measurement on both target classes,
and that measurement is worth recording in `AGENT_LEARNINGS.md`.

### 3.2 `max_ram_usage_percent` is ignored; 75% is hardcoded

`SystemProfile::max_allowed_memory_bytes` computes
`(available_bytes * 75) / 100`. The `hardware.safety.max_ram_usage_percent`
config field is adjustable in Settings between 50 and 90 and has no effect. An
operator who lowers it to leave room for a desktop session, or raises it on a
dedicated 12 GB phone, gets no change in behavior.

`SystemProfile` has no access to the config, which is why the constant is
inlined. **Proposal.** Pass the safety percentage into
`max_allowed_memory_bytes`, or construct `SystemProfile` with a policy struct.
Keep `LMK_SAFETY_PERCENT` in `cluster.rs` as the default only.

### 3.3 Hardcoded device constants persist in several places

`AGENTS.md` Directive 3 is explicit that device constants must not be hardcoded,
and `cluster.rs` correctly deprecated `NODE_A_MAX_STANDALONE_MB` and
`NODE_B_MAX_RPC_RAM_MB`. The same numbers survive elsewhere:

- `config.rs` `validate()` **rejects** `cluster.max_rpc_ram_mb > 1800`. A 32 GB
  desktop acting as an RPC worker cannot advertise more than 1.8 GB — the config
  file will not load. This is a Core-2-Duo-specific limit enforced globally.
- `settings_view.rs` clamps the same field to 1800 and labels it "Node B Max RPC
  RAM Cap … capped at 1800 MB for Mac safety".
- `main.rs:410` warns above 1800 MB for "Node B safety".
- `models_view.rs:85` and `:151` compare against a bare `10300` MB to pick badge
  and gauge colors — an S23-Ultra-shaped number with no name and no comment.
- `hub.rs:389` and `daemon.rs:158` default `block_count` to `32` when GGUF
  parsing fails, silently planning a layer split for a model whose geometry is
  unknown.

**Proposal.** Make the worker cap a per-node policy with no global ceiling,
validated only as "less than this node's safe budget". Replace the `10300`
comparisons with the computed cluster budget. On a GGUF parse failure, refuse to
plan rather than guessing 32 layers.

### 3.4 The GGUF reader is neither zero-copy nor bounded

`src/gguf.rs` is described as a "zero-copy GGUF header parser" in `AGENTS.md`
and `DESIGN_SPEC.md`. It is a fully-allocating parser: `read_value` for type 9
(array) builds a `Vec<GgufValue>` of every element, and for a modern tokenizer
that is 128,000-plus heap-allocated `String`s, every one of which is immediately
discarded because only scalar architecture keys are read out. This is the cost
that §2.2 pays per frame.

It is also unbounded on attacker- or corruption-controlled input:

- `HashMap::with_capacity(kv_count as usize)` where `kv_count` is read from the
  file. A corrupt header claiming `u64::MAX` pairs requests an impossible
  allocation and aborts the process.
- `Vec::with_capacity(array_len)` in the array branch, same problem.
- `vec![0u8; len]` in `read_string` with `len` straight from the file.

A truncated download or a hostile file in the models directory can therefore
abort the TUI, and `scan_models_dir` runs this over every file in the directory.

**Proposal.**

1. Skip array payloads by seeking rather than materializing them. The `Seek`
   bound is already on the signature and unused for this purpose. Element sizes
   are known for fixed-width types; strings need a length-prefixed skip loop.
   Keep a `Vec` only for the small arrays actually consumed.
2. Validate `kv_count`, `array_len`, and string lengths against the remaining
   file size before allocating, and return `UnexpectedEof` (already defined and
   never constructed) instead of allocating.
3. Read the **tensor section** too, not just the KV header. It gives exact
   weight bytes per tensor and the quantization type of each — which is what
   §3.1's `weights_mb` and §3.5's non-layer-tensor accounting both need, and
   which also lets the Models tab display "Q4_K_M" instead of nothing.
4. Add a fuzz or property test over the decoder. The beacon decoder deserves the
   same: `tests/test_discovery.rs` covers valid packets, CRC corruption, and a
   stable fixture, but not adversarial structure.

### 3.5 The layer-split plan uses memory fractions as a proxy for layers

`ClusterCoordinator::plan_layer_split` computes
`remote_fraction = overflow_mb / total_required_mb` and multiplies it by
`total_layers`. This assumes every layer has identical cost and that the model is
nothing but layers. Neither holds: token embeddings and the output projection are
large (often 10–20% of a quantized model's bytes) and live on the host
regardless, and the KV cache is in `total_required_mb` but is not split the way
weights are. The result systematically under-offloads, which means the host
exceeds its budget and the load fails for reasons the planner believed it had
avoided.

Separately, the emitted arguments are questionable. `build_llama_args` produces
`--rpc <ep> --split-mode layer --tensor-split <host>,<remote>`. In llama.cpp,
`--tensor-split` distributes across *backend devices* in order, and whether the
host CPU is a device in that list depends on `-ngl` and the backends compiled in.
Combining `-ngl 99` with a two-way tensor split against a single RPC endpoint is
very likely not what is intended, and no test asserts against real llama.cpp
behavior — `tests/test_cluster_rpc.rs` checks the argument strings the planner
emits, not that llama.cpp interprets them as the planner believes.

**Proposal.** Plan on per-tensor bytes from the GGUF tensor section (§3.4): sum
the actual bytes per layer, add non-layer tensors to the host's fixed cost, add
the KV cache for the layers each side holds, then solve for the layer count that
fits. Validate the emitted command line against a real `llama-server` on the
target hardware before trusting it, and record the finding.

### 3.6 Only one RPC worker, and no awareness of link quality

`select_rpc_candidate` returns `rpc_candidates().into_iter().next()` — the single
peer with the most allocatable RAM. `plan_layer_split` takes one
`rpc_endpoint: Option<&str>` and `LayerSplitDecision` holds one
`remote_endpoint`. So the "N-node mesh" supports exactly one offload target.

More importantly, selection is blind to the thing that actually determines
whether offload is worth doing. Sequential layer pipelining over the network
transfers activations per token per boundary. Over gigabit Ethernet that is
viable; over 2.4 GHz Wi-Fi with contention it can be slower than not offloading
at all, and dramatically slower than simply using a smaller quantization. The
planner will nonetheless choose offload whenever memory says it must, with no
estimate of the resulting tokens per second.

**Proposal.**

1. Support an ordered list of workers and emit repeated `--rpc` arguments.
2. Measure each candidate link before planning — RTT and a short throughput
   probe over the control plane — and cache it per peer with an age.
3. Make the objective **predicted tokens per second**, not "does it fit."
   Present the operator the ranked options with their predicted cost: run 100%
   local at Q4 (12 tok/s), offload 14 layers to the desktop (4 tok/s), run on the
   desktop entirely (28 tok/s). That framing is what makes a heterogeneous mesh
   genuinely useful rather than merely possible, and it is the natural home for
   the existing thermal index too.
4. Say plainly in the docs that network offload is a last resort for models that
   do not otherwise fit, not a performance feature.

### 3.7 Supervisor gaps

- **`fallback_to_cpu` is ignored.** `spawn_with_fallback` always falls back. An
  operator who disables it — because a silent CPU fallback on a 7B model means
  minutes per response and they would rather see an error — gets the fallback
  anyway.
- **`mmap`, `mlock`, and `cpu_threads_batch` are never passed through.**
  `build_args` emits `--host --port -m --alias -c -t -ngl` and nothing else, so
  `--no-mmap`, `--mlock`, and `-tb` never appear. Several other flags that matter
  on these targets are also unreachable: `--flash-attn`, `--cache-type-k/v`,
  `--parallel`, `--cont-batching`, `--n-predict`, `--split-mode`.
- **Health check targets the bind address.** `is_healthy` builds
  `http://{config.host}:{port}/health`, and `config.host` defaults to `0.0.0.0`.
  Connecting to `0.0.0.0` happens to work on Linux but is not meaningful; it
  should probe `127.0.0.1`.
- **No restart supervision.** The hub detects a crashed child in its refresh tick
  and reports it, but nothing restarts it. A `SupervisorPolicy` with bounded
  retries and exponential backoff — and automatic demotion from GPU to CPU after
  repeated Vulkan failures, which `AGENT_LEARNINGS.md` shows is the dominant
  failure mode — would make unattended nodes actually unattended.
- **One model per node.** No multi-model residency, no idle eviction, no request
  queueing. For a personal network where a phone holds a small model and a
  desktop holds a large one, per-node single-tenancy is a reasonable v1, but
  idle-timeout unload is worth adding so a phone does not hold 4 GB forever.
- **`Drop` sends `SIGKILL` directly.** `stop()` does the right thing
  (`SIGTERM`, 3 s grace, then `SIGKILL`), but the `Drop` path skips straight to
  `SIGKILL`. On a panic unwind the child dies hard. `kill_on_drop(true)` is also
  already set, making the manual `libc::kill` redundant.
- **Vulkan detection is log scraping.** `monitor_vulkan_init` greps stderr for
  four substrings within a 4-second window. It will drift with llama.cpp log
  changes. Probing `/v1/models` and reading the reported backend, or running
  `llama-server --list-devices` once at startup, is more durable.

### 3.8 Proposal: a single placement planner

The decision "where and how should this model run" is currently spread across
`hub.rs::execute_model_load_with_gpu`, `daemon.rs`, `cluster.rs::plan_layer_split`,
and `supervisor.rs`'s preflight guard, each with its own partial view and its own
hardcoded context size. That is why the constants in §3.3 keep reappearing.

Consolidating it into one module with an explicit input and output — candidate
nodes with measured budgets and link quality, a model with parsed tensor
geometry, an operator policy — and a ranked list of `ExecutionPlan`s with
predicted throughput, would collapse §3.1, §3.3, §3.5, §3.6, and §2.8's missing
context control into one coherent surface. It is also directly testable without
hardware, which none of the current placement logic is.

---

## 4. Model downloading and transferring

### 4.1 The downloader blocks the async runtime

`ModelDownloader::download` is `async`, but its file I/O is synchronous
`std::fs`: `File::create`, `OpenOptions::open`, `file.write_all(&chunk)` inside
the `while let Some(chunk) = stream.next().await` loop, and `file.flush()`.
Each `write_all` blocks the Tokio worker thread that is driving the download.
With a slow SD card or Termux's filesystem this starves every other task on that
worker — including, once downloads are in the TUI (§4.7), the chat stream and the
event loop.

`calculate_sha256` is likewise fully synchronous and reads the entire file, which
for a 7B model is multiple gigabytes of blocking reads on a runtime thread.

**Proposal.** `tokio::fs` with a `BufWriter` for the write path, and
`spawn_blocking` for hashing. Better still, hash incrementally as chunks arrive
so verification costs nothing extra (§4.4).

### 4.2 Resume is unsafe because the partial file has no validator

If `<dest>.part` exists, `download` sends `Range: bytes=<len>-` and appends. It
never records *what* the partial file was. So:

- Resuming with a different URL appends unrelated bytes to the existing partial.
- A server that has since changed the file serves bytes from the new version
  appended to the old prefix.
- A `.part` left by an unrelated interrupted download is silently adopted.

Without `--sha256` the result is a corrupt GGUF that fails to parse, or worse,
parses and produces garbage. With `--sha256` it fails verification after
downloading the whole thing — and then `return Err` leaves the `.part` in place,
so the next attempt resumes from the same poisoned prefix and fails identically,
forever. There is no recovery path short of manually deleting the file.

**Proposal.** Write a `<dest>.part.json` sidecar alongside the partial recording
URL, `ETag`, `Last-Modified`, total size, and expected hash. On resume, re-issue
the request and require the validator to match — using `If-Range` so the server
itself rejects a stale resume — and start over if it does not. On checksum
failure, delete the partial and say so.

### 4.3 No retry, no backoff, single connection

A dropped TCP connection mid-transfer propagates out of the `?` on
`chunk_res` and aborts the whole call. Resume works on the *next manual
invocation*, so a multi-gigabyte download over flaky Wi-Fi requires the operator
to keep re-running the command.

There is also exactly one connection. The module is described as a "chunked HTTP
resume engine" in `AGENTS.md`, but there is no chunking in the parallel sense —
one `GET` with one `Range` header.

**Proposal.** Automatic resume-and-retry with exponential backoff and a cap, and
a stall detector (no bytes for N seconds → reconnect, which matters more than raw
error handling on mobile networks). Parallel ranged connections are a real
throughput win on high-latency links and a complication everywhere else; 2–4
segments is the usual sweet spot, and it should be configurable with a default of
1 on metered or mobile connections.

### 4.4 Verification is a second full pass, and there is no disk-space check

After the stream completes, `calculate_sha256` re-reads the entire file from
disk. For a 7 GB model on a phone that is gigabytes of avoidable I/O. Feeding a
`Sha256` hasher from the chunk loop makes verification free, with the one
wrinkle that a resumed download must hash the existing prefix first.

Nothing checks free disk space before starting. The failure mode on a phone is an
`ENOSPC` partway through a multi-gigabyte download, with the partial file left
occupying whatever space remained.

**Proposal.** Incremental hashing; a preflight free-space check against
`Content-Length` plus a margin; `fsync` before the rename so the atomic move is
actually durable.

### 4.5 No model registry integration

The only way to get a model is a fully-qualified URL the operator has found
themselves. For a project whose audience is running local models on a phone,
Hugging Face integration is the expected path:

- Resolve `TheBloke/Qwen2.5-Coder-7B-GGUF:Q4_K_M` to a download URL.
- List available quantizations for a repo with their sizes, and mark which ones
  fit this node's budget — the memory planner (§3.1) already has everything
  needed to answer that, and it is the single most useful thing the app could
  tell someone choosing a quantization.
- Carry `HF_TOKEN` for gated repos.
- Read the published SHA-256 so verification is automatic rather than something
  the operator has to paste.

### 4.6 Sharded models are not supported

Models above roughly 50 GB are published as `model-00001-of-00003.gguf`. The
downloader handles one URL and one destination, `scan_models_dir` treats every
`.gguf` as an independent model, and passing a shard to `llama-server` requires
the first shard with its siblings present. A multi-part model will appear as
three broken entries in the Models tab.

### 4.7 Downloads are invisible to the TUI

`ModelDownloader` is referenced only from `src/main.rs`. There is no download
surface in the hub at all. This is the most visible gap between the program and
the experience it is aiming at: the Models tab lists what is already on disk and
offers no way to get anything new.

**Proposal.** A Models tab that owns the full lifecycle — browse a registry,
pick a quantization with fit prediction, download with progress and pause/resume,
verify, then load — with downloads running on the worker task from §2.9 so they
continue while the operator chats. `DownloadProgress` already carries bytes,
total, speed, and percent, which is exactly what a `Gauge` needs.

### 4.8 There is no device-to-device model transfer

This is the largest capability gap in the review, and it is the one the user's
question points at most directly. In a mesh where a desktop has already
downloaded a 4 GB Q4_K_M model, a phone on the same LAN must download it again
from the internet. There is no catalog exchange, no transfer endpoint, no
deduplication, and no awareness that a peer holds a file at all.

The waste compounds: every node pays the download cost, nobody can see what the
mesh collectively holds, and a node with a working model cannot help a node
without one even over a gigabit link that would be twenty times faster than the
WAN.

### 4.9 Proposal: a content-addressed model store with LAN sync

The pieces to build this mostly exist; what is missing is identity for model
files and a transfer path.

**1. Content addressing.** Identify models by SHA-256 of their bytes, not by
filename. `calculate_sha256` already exists; make it part of the scan (cached in
an index, since re-hashing every file on every scan is prohibitive) so every node
knows the digest of everything it holds. Content addressing is what makes
deduplication, verified transfer, and "does the mesh have this?" all trivial.

**2. A local model index.** `~/.nexus/models.json` mapping digest → path, size,
GGUF metadata, quantization, source URL, and mtime. This also removes the
per-frame GGUF parsing in §2.2 and the repeated full-directory scans.

**3. Catalog exchange over the control plane.** The
`GET /nexus/control/v1/models` endpoint from §1.1 returns this index. The TUI then
shows a *mesh-wide* model list: which models exist anywhere, which nodes hold
each one, and which nodes can run it. That view is the thing that would make this
feel like a network rather than three separate installs.

**4. Chunked, resumable, verified transfer.** A `GET
/nexus/control/v1/blob/{digest}` endpoint serving `Range` requests, and a client
that reuses the hardened downloader from §4.1–§4.4. Because the digest *is* the
address, the existing SHA-256 verification becomes automatic and the resume
validator problem in §4.2 disappears — a digest cannot go stale. Transfer should
be pull-based (`[T] Fetch from peer` on a mesh catalog entry), with push
(`[S] Send to peer`) as a convenience.

**5. Multi-source and opportunistic fetch.** Once several nodes hold a blob,
ranged requests can be spread across them, and a fetch can prefer LAN peers over
the WAN automatically — download from the internet only when no peer has it. For
a three-device personal mesh the simple version (one peer, sequential) is
sufficient and should come first.

**6. Transfer over whatever link exists.** The ADB tunnel in `src/tunnel.rs`
already gives a phone a high-bandwidth USB path to one machine; blob transfer
over it would beat Wi-Fi substantially and reuses existing code.

Worth stating as a non-goal: this should not become a BitTorrent implementation.
A personal mesh is three to five nodes on one LAN. Pull-based, digest-addressed,
single-source-with-fallback covers it, and the complexity of a real swarm
protocol is not justified.

---

## 5. Capability proposals

Beyond fixing what is broken, these are the additions that would most change what
the program can do. Roughly ordered by value per unit of work.

### 5.1 A mesh gateway: one endpoint, any node, any client

Expose an OpenAI-compatible endpoint on every node that proxies to whichever node
currently holds the requested model, resolving through the peer registry. Any
OpenAI-compatible client — an editor extension, a phone app, a script, another
agent — could then point at `http://any-nexus-node:PORT/v1` and reach the mesh
without knowing its topology.

This is what turns the project from "a TUI that talks to llama.cpp" into actual
local AI infrastructure, and most of the machinery exists: `NexusClient` already
streams SSE, the registry already knows who holds what, and `active_model` is
already in the beacon. The main new work is a streaming reverse proxy and a model
name → node resolution table. It also subsumes the chat-rebinding logic currently
scattered through `hub.rs`.

### 5.2 Prompt cache reuse and session restore

llama-server supports saving and restoring slot KV state (`--slot-save-path`,
`/slots/{id}?action=save|restore`). Reusing the cached prefix across turns
eliminates prompt reprocessing, which on a phone is often the dominant cost of a
long conversation — re-ingesting 3000 tokens of history before generating the
next token. Persisting slot state also makes "resume yesterday's conversation"
fast rather than merely possible, which pairs with the session browser in §2.8.

### 5.3 Speculative decoding

A heterogeneous mesh is an unusually good fit for speculative decoding: a small
draft model (0.5B) on the fast device proposing tokens that the large model
verifies in batches. llama.cpp supports it via `--model-draft`. This is one of
the few available changes that can produce a 1.5–3× wall-clock speedup rather
than a constant-factor improvement, and the placement planner from §3.8 is the
natural place to decide when it is worth it.

### 5.4 Power, thermal, and battery awareness

`probe_thermal_index` reads `thermal_zone0` and the value is advertised in the
beacon, but nothing acts on it. For a phone in the mesh this is the difference
between a useful node and a hot, dead battery:

- Refuse or downgrade loads above a thermal threshold rather than merely
  displaying the number.
- Read `/sys/class/power_supply/*/capacity` and `status`; decline to host
  inference below a battery floor unless charging.
- Throttle `-t` and batch size as temperature rises instead of letting the SoC
  thermally throttle unpredictably.
- Prefer the mains-powered node when the planner ranks equivalent options.

### 5.5 Benchmarking and telemetry history

A `nexus bench` subcommand measuring prompt-eval and token-generation throughput
per (model, node, backend, context) combination, persisted to
`~/.nexus/bench.json`, would give the placement planner (§3.8) real numbers
instead of heuristics — and give the operator a defensible answer to "which
device should run this?" Per-turn metrics are already captured in
`GenerationMetrics`; they are discarded when the session ends.

### 5.6 Embeddings and local retrieval

llama-server exposes `/v1/embeddings`. A small local index over a notes or code
directory, with retrieval injected into the system prompt, is a modest amount of
code and is the feature that makes a personal model genuinely more useful than a
remote one, since the data never leaves the LAN. Best treated as opt-in and
after §5.1, which gives it a stable endpoint to build against.

### 5.7 Operational conveniences

- **`nexus doctor`** — ~~one command that checks every precondition~~ **Done
  (PR #10, `src/doctor.rs`).** Probes config, `llama-server`/`rpc-server` on
  PATH (WARN if missing), models dir, `SystemProfile`, bindability of
  discovery/control/api ports, adb (WARN), log dir, display name. Shell
  completions / systemd units / config schema versioning remain open.
- **Shell completions and a man page** from the existing `clap` definitions.
- **Systemd and Termux boot units** so `nexusd` survives reboots.
- **Config schema versioning** so future config changes migrate rather than fail
  to parse.

---

## 6. Engineering hygiene

- **No CI.** `.github/` contains only agent definitions. A workflow running
  `cargo fmt --check`, `cargo clippy -- -D warnings`, and `cargo test` on
  x86-64 Linux, plus `cargo check --target aarch64-linux-android`, would catch
  both ordinary regressions and Directive 1 violations (the `-avx,-avx2,-fma,
  -sse4.2` baseline in `.cargo/config.toml` is only exercised when someone builds
  on that target). Verifying the Penryn constraint in CI — by disassembling the
  release binary and scanning for AVX opcodes — is unusual but cheap and directly
  protects the project's hardest constraint.
- **19 clippy warnings in the lib**, including two uses of the project's own
  deprecated methods, one `impl` that can be derived, and a function with 11
  arguments. None are severe; gating at zero warnings now keeps it that way.
- **Decoder robustness tests.** `BeaconPacket::decode` and `GgufMetadata::read`
  both parse untrusted bytes. Property-based or fuzz coverage for both, plus the
  allocation bounds from §3.4, would close the clearest crash surface.
- **Integration tests through real entry points.** A fake `llama-server` stub
  (`/health`, `/v1/models`, canned SSE) and a two-node in-process mesh test would
  cover the paths where the current gaps live — exactly the gaps a passing
  73-test suite did not catch.
- **Documentation drift.** Several specifics in the docs no longer match the
  code, which matters because the docs are the design authority here:
  `DESIGN_SPEC.md` §4 documents `POST /cluster/model/load` while the code uses
  `/nexus/control/v1/model/load`; `BUILD_PLAN.md` Phase 6 specifies five tabs
  including `[F4: Tunnel]` while the hub has four and no Tunnel tab; `AGENTS.md`
  and `DESIGN_SPEC.md` describe the GGUF parser as "zero-copy" (§3.4) and
  `downloader.rs` as a "chunked" engine (§4.3); the workspace layout in
  `AGENTS.md` omits `cluster_view.rs`, `markdown.rs`, `models.rs`, and
  `session_logger.rs`. Worth a sweep once the control plane lands.
- **File sizes.** `hub.rs` (1113 lines) and `discovery.rs` (1261 lines) are well
  past the 250-line guideline in `AGENTS.md`. Both have natural seams: `hub.rs`
  splits into shell, keymap, and command handling (§2.1 forces this anyway);
  `discovery.rs` splits into beacon codec, UDP transport, and service
  orchestration.

---

## 7. Suggested phase plan

Continuing the numbering in `BUILD_PLAN.md`. Each phase is independently
shippable and leaves the tree working. No calendar estimates — the sequencing is
driven by dependencies, and the invasiveness note says what each one touches.

### Phase 7 — Make the mesh actually work

> **Status (2026-10-06): Done** — PR #9 (control plane) + PR #10 (remainder).
> Acceptance: remote load path exists; zero-config resolve + hub discovery;
> `nexus doctor`; Settings displayed ⇒ consumed. Remaining Phase 7 nits:
> beacon v2 for on-wire `ctrl`/name; two-device LAN soak still worth a live check
> outside Cloud VMs.

The minimum set that makes the documented features true.

- ~~Control-plane HTTP server on its own port, served from all three entry points
  (§1.1, §1.2)~~ **Done**
- ~~Beacon and mDNS carry the control port and a human-readable node name (§1.2,
  §2.9)~~ **Done for mDNS** (`ctrl`, `name=`); beacon v2 deferred
- ~~Restore zero-config host resolution (§1.3)~~ **Done**
- ~~File-based logging in TUI mode and `nexus doctor` (§2.7, §5.7)~~ **Done**
- ~~Remove or wire up every inert config field; delete dead modules (§0)~~
  **Done for Settings UI**; dead modules (`tunnel_view`, `format_prompt`, etc.)
  intentionally kept for later phases

*Invasiveness:* one new module, small edits across `discovery.rs`, `main.rs`,
`daemon.rs`, `settings_view.rs`. Adds an HTTP server dependency — decision to
record.
*Acceptance:* loading a model on a remote node from the TUI succeeds on a
two-device LAN with default config on both; `nexus doctor` diagnoses a
deliberately broken node; no config field is displayed that has no effect.

### Phase 8 — TUI responsiveness and correctness

> **Status (2026-10-06): Done** — branch `cursor/phase8-tui-responsiveness-4865`.
> HubCommand/HubEvent bus; ChatEntry; wrap scroll; hot-swap preserves `-ngl`;
> GGUF/profile/Vulkan caches; hub split + declarative keymap.

- ~~Command/event architecture; no blocking awaits in the event loop (§2.1)~~ **Done**
- ~~Pure render: cache GGUF metadata, system profile, rendered markdown (§2.2)~~ **Done**
- ~~`ChatEntry` replaces the parallel metrics vector and prefix-based filtering
  (§2.4, §2.5)~~ **Done**
- ~~Wrapping-aware scroll with a scrollbar (§2.3)~~ **Done**
- ~~Context size and generation parameters exposed; hot-swap preserves target
  (§2.6, §2.8)~~ **Done**
- ~~Split `hub.rs`; declarative keymap (§6)~~ **Done** (`src/ui/hub/`)

*Invasiveness:* substantial refactor of `hub.rs` and `chat.rs`; no protocol
changes.
*Acceptance:* the UI stays responsive and keeps streaming during a 30-second
model load; scrolling reaches the top of a conversation with wrapped code blocks;
choosing CPU-safe mode during a hot-swap launches with `-ngl 0`. Covered by unit
tests for ChatEntry/wrap/`effective_ngl(Some(0))`; live 30s load needs
`llama-server` + GGUF outside Cloud VMs.

### Phase 9 — Trust

> **Status (2026-10-06): Done** — branch `cursor/phase9-trust-4865`.
> `require_pairing` defaults false until first successful pair (then auto-enabled).
> Unsigned localhost `POST /state` remains for doctor/tests.

- ~~Ed25519 node identity; pairing with a short code (§1.5)~~ **Done**
- ~~Signed control-plane requests with replay protection (§1.5)~~ **Done**
- ~~Peer verification promotes registry entries; unverified peers cannot be routed
  to or offloaded to (§1.4, §1.5)~~ **Done**
- ~~`require_pairing` enforced on inbound control + routing~~ **Done** (auto-on after pair)

*Invasiveness:* new crypto dependency; touches discovery, registry, control
plane, and the Cluster tab. Needs a compatibility story for unpaired nodes during
rollout.
*Acceptance:* a forged beacon carrying a known UUID does not redirect chat
traffic when pairing is enforced; an unpaired node cannot load a model remotely;
pairing two devices takes one code entry. Covered by `tests/test_trust.rs` and
extended network/control-plane tests.

### Phase 10 — Model store and LAN transfer

> **Status (2026-10-06): Done.** Content-addressed `~/.nexus/models.json` index
> (`src/store.rs`); catalog digests on `GET /models`; privileged
> `GET /nexus/control/v1/blob/{digest}` Range streaming + `POST /blob/fetch`;
> hardened downloader (async I/O, `.part.json`, retry, incremental hash, disk
> preflight); Models mesh catalog with `[D]`/`[T]`/`[S]`. Covered by
> `tests/test_phase10_store.rs`.

- Content-addressed index with cached digests and GGUF metadata (§4.9)
- Catalog endpoint and a mesh-wide model view in the TUI (§4.9)
- Blob transfer with ranged resume and digest verification (§4.9)
- Hardened downloader: async I/O, safe resume, retry and backoff, incremental
  hashing, disk-space preflight (§4.1–§4.4)
- Download and transfer surfaced in the Models tab (§4.7)

*Invasiveness:* new module plus control-plane endpoints; significant rewrite of
`downloader.rs`; Models tab gains a lifecycle.
*Acceptance:* a model downloaded on one node transfers to a second over the LAN,
verifies by digest, and loads; an interrupted transfer resumes; a corrupted
partial is detected and discarded rather than resumed.

### Phase 11 — Placement intelligence — **Done on branch** (`cursor/phase11-placement-intelligence-d6cb`)

- Explicit `MemoryPlan` replacing the boolean guard; `max_ram_usage_percent`
  honored (§3.1, §3.2) — **Done** (`src/cluster/memory.rs`)
- GGUF tensor section parsed; bounded allocations; quantization displayed (§3.4) — **Done**
- Per-tensor layer-split planning; CLI args unit-tested (§3.5) — **Done**;
  **live llama.cpp validation still required** on Penryn + Snapdragon
- Multiple RPC workers; link-quality probing; predicted-throughput ranking (§3.6) — **Done**
- Supervisor: honor `fallback_to_cpu`, plumb memory/performance flags,
  restart with backoff and GPU demotion (§3.7) — **Done**
- Remove the remaining hardcoded device constants (§3.3) — **Done** (no global 1800 ceiling)

*Invasiveness:* the largest change to core logic; consolidates placement into
`src/cluster/`. Requires measurement on both target hardware classes.
*Acceptance:* a 1.5B model is no longer rejected by the 200 KB/token estimate
(unit-tested); a split plan derived from tensor bytes loads successfully where
the fraction-based plan failed (**needs live GGUF**); the operator sees ranked
options with predicted tokens per second; a 32 GB worker can advertise more than
1800 MB (config validate unit-tested).

### Phase 12 — Capability expansion

- Mesh gateway: one OpenAI-compatible endpoint fronting the whole mesh (§5.1)
- Prompt cache reuse and session restore (§5.2, §2.8)
- Speculative decoding with a draft model (§5.3)
- Thermal, power, and battery-aware scheduling (§5.4)
- `nexus bench` feeding the planner (§5.5)
- Optional: embeddings and local retrieval (§5.6)

*Invasiveness:* mostly additive, built on Phases 7–11.
*Acceptance:* an unmodified OpenAI client reaches the active model through any
node; a resumed session skips prompt reprocessing; a hot phone declines a load
instead of thermally throttling mid-generation.

### Continuous

> **Status (2026-10-07): Done on `cursor/continuous-ci-hygiene-d6cb`.**
> CI workflow (fmt / clippy `-D warnings` / `cargo test --locked`), Android
> best-effort `aarch64-linux-android` check, Penryn release-binary opcode scan,
> `proptest` + adversarial coverage for `BeaconPacket::decode` /
> `GgufMetadata::read` (minimal allocation bounds; not Phase 11 tensor parse),
> fake `llama-server` harness + two-node in-process mesh test, light docs drift
> sweep. Out of scope: Phase 10/11/12, beacon v2, SSE `/events`.

- ~~CI: fmt, clippy at zero warnings, tests, Android cross-check, AVX-opcode scan
  (§6)~~ **Done**
- ~~Fuzz and property coverage for the beacon and GGUF decoders (§3.4, §6)~~ **Done**
  (`proptest`; no `cargo-fuzz` CI)
- ~~Fake `llama-server` harness and a two-node in-process mesh test (§0, §6)~~ **Done**
- ~~Documentation sweep after each phase (§6)~~ **Done** (Continuous drift only)

---

## Appendix: findings index

| § | Finding | Severity | Primary file |
|---|---|---|---|
| 1.1 | Control-plane server does not exist | Blocker | `src/control_plane.rs` |
| 1.3 | Auto-discovery impossible with default config | Blocker | `src/client.rs` |
| 4.8 | No device-to-device model transfer | Major gap | — |
| 1.5 | No authentication or integrity on any transport | High | `src/discovery.rs` |
| 0 | Ten config fields and seven modules are inert | High | `src/ui/settings_view.rs` |
| 2.1 | Event loop blocks up to ~39 s on model load | High | `src/ui/hub.rs` |
| 3.1 | Memory model crude in four distinct ways | High | `src/sysinfo.rs` |
| 2.7 | No logging at all in TUI mode | High | `src/main.rs` |
| 4.2 | Resume corrupts and cannot self-recover | High | `src/downloader.rs` |
| 3.4 | GGUF parser unbounded on untrusted input | High | `src/gguf.rs` |
| 1.2 | Control plane collides with llama-server port | Medium | `src/control_plane.rs` |
| 1.4 | Peer registry inert; two peer stores disagree | Medium | `src/peer_registry.rs` |
| 2.2 | GGUF parse and `/proc` reads per frame | Medium | `src/ui/models_view.rs` |
| 2.4 | `message_metrics` desynchronizes | Medium | `src/ui/hub.rs` |
| 2.6 | Hot-swap discards chosen execution target | Medium | `src/ui/hub.rs` |
| 3.3 | Hardcoded 1800 MB cap rejects valid configs | Medium | `src/config.rs` |
| 3.5 | Layer split uses memory fraction as layer proxy | Medium | `src/cluster.rs` |
| 3.6 | Single RPC worker; offload blind to link quality | Medium | `src/discovery.rs` |
| 3.7 | `fallback_to_cpu` ignored; flags not plumbed | Medium | `src/supervisor.rs` |
| 4.1 | Blocking file I/O in async download loop | Medium | `src/downloader.rs` |
| 4.7 | Downloads unreachable from the TUI | Medium | `src/ui/models_view.rs` |
| 6 | No CI; Penryn baseline unverified | Medium | `.github/` |
| 1.6 | Unbounded maps; per-probe sockets; per-tick probes | Low | `src/discovery.rs` |
| 1.7 | mDNS stale TXT, no unregister, blocking resolve | Low | `src/mdns.rs` |
| 1.8 | No IPv6 anywhere | Low | `src/discovery.rs` |
| 2.3 | Scroll math ignores wrapping | Low | `src/ui/chat.rs` |
| 2.5 | Banner filtering by English prefix | Low | `src/ui/chat.rs` |
| 2.9 | Multi-line input not rendered; transport badge wrong | Low | `src/ui/chat.rs` |
| 3.2 | `max_ram_usage_percent` ignored | Low | `src/sysinfo.rs` |
| 4.3 | No retry or backoff on transient failure | Low | `src/downloader.rs` |
| 4.4 | Hash is a second full pass; no disk check | Low | `src/downloader.rs` |
| 4.5 | No model registry integration | Low | `src/downloader.rs` |
| 4.6 | Sharded GGUF models unsupported | Low | `src/ui/models.rs` |
