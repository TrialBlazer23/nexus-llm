# Nexus-LLM Network Expansion Findings

**Scope:** Current architecture and a staged design for a decentralized, local-Wi-Fi N-node network, with Node A (Galaxy S23 Ultra / Termux / ARM64) and Node B (Debian x86_64 MacBook) retained as the primary compute and client anchors. The same Nexus binary should also support additional Termux devices, native Windows PowerShell, and Windows WSL by selecting a runtime role at startup.

**Research checked:** October 4, 2026. This is an analysis and implementation plan only; it does not change runtime behavior or add dependencies.

## Executive recommendation

Adopt a **hybrid control plane** rather than replacing the current data plane:

1. Add **`mdns-sd`** as the primary zero-configuration service-discovery mechanism on a local link. Publish a small Nexus service record with stable node identity, protocol version, and API/RPC endpoints. It is a pure-Rust mDNS/DNS-SD implementation, has no async-runtime requirement, and documents macOS/Linux support. Android/Termux is not expressly listed as supported, so make real-device validation a release gate.
2. Retain and evolve the **custom UDP beacon** as a compact telemetry/heartbeat and compatibility fallback. Keep existing version-1 decoding during migration; add versioned fields rather than treating its 64-byte packet as the permanent general-purpose peer protocol.
3. Maintain a **dynamic peer registry** in Nexus with stable identities, capabilities, health/expiry, address updates, and change notifications. Use unicast control-plane exchanges to validate discovered endpoints and synchronize detailed state; do not mistake mDNS or CRC-checked beacons for authentication.
4. Keep **HTTP/OpenAI-compatible REST and SSE** for inference requests and token streaming, and the existing llama.cpp RPC transport for layer offload. Discovery/state traffic is a separate control plane. Defer libp2p or Zenoh adoption until measured scale or multi-subnet requirements justify their larger protocol surface.

“Decentralized” here means that every Nexus member can advertise and discover peers without a mandatory central discovery service. It does **not** mean that all compute roles become equal: Node A remains the preferred primary inference host, Node B remains the primary operator/TUI anchor, and extra devices are eligible workers/clients only under explicit capability and memory policies.

## 1. Current architecture and execution goals

### Intended topology and execution policy

The workspace is a Rust/Tokio application with two binaries: `nexusd` (headless model/process supervisor, intended to run on Node A) and `nexus` (CLI/TUI, intended to run on Node B). The project optimizes Node A for local inference, Vulkan/ARM acceleration, and Android’s low-memory-kill constraints. The configured LMK safety threshold is 75% of available memory; the cluster design caps Node A standalone use at 8.5 GB. Node B is a lightweight client by default and has a hard 1,800 MB RPC-worker memory ceiling. If a model fits Node A’s safe budget, the policy is to keep it wholly on Node A. If it does not, the current distributed strategy is sequential layer pipelining (`--split-mode layer`), never network row/tensor parallelism.

### Runtime role selection

Do not permanently equate a physical device with one role. Termux, Debian, native Windows, and WSL should all be able to run the same role-aware Nexus executable. The physical device, stable node identity, runtime role, capabilities, and anchor designation are separate concepts:

| Concept | Examples | Purpose |
|---|---|---|
| Device/runtime | S23 Termux, Debian MacBook, Windows PowerShell, WSL2 | Platform and process constraints |
| Runtime role | `host`, `client`, `worker`, `member` | Services started by this invocation |
| Capability | `inference`, `client`, `rpc_worker`, `discovery` | What the node can offer |
| Anchor | `primary_compute`, `primary_client`, `member` | Cluster policy and preferred placement |

The preferred CLI is explicit subcommands:

```text
nexus host       # Start local llama-server and advertise inference
nexus client     # Start the TUI and connect to a selected host
nexus worker     # Start an RPC worker, subject to local memory policy
nexus discover   # Inspect discovered and configured peers
```

`nexusd` should remain a compatibility alias for `nexus host`, so existing Termux launch scripts continue to work. Running `nexus` without a subcommand should preserve the unified local hub UX, but it must use the same endpoint-resolution service as `nexus client`.

Recommended examples:

```bash
# Primary model host on Termux
nexus host --model ~/nexus-models/qwen2.5-coder-7b.gguf --ctx 4096

# Client on Debian, native Windows, or WSL
nexus client

# Additional Termux host
nexus host --model ~/nexus-models/another-model.gguf --name termux-worker-02

# Optional RPC worker
nexus worker --rpc-port 50052 --mem 1800
```

Role precedence should be explicit and predictable: command-line subcommand, then configured default role, then the no-subcommand hub behavior. Host-only options such as `--model` must not be accepted as implicit client behavior. A client should support `--host`, `--node`, and `--model` selection, with manual endpoint precedence over configured defaults, paired anchors, and automatic peer scoring.

### Present network planes

| Plane | Current mechanism | Current purpose / limitation |
|---|---|---|
| Discovery and telemetry | IPv4 UDP broadcast, port 9999; fixed 64-byte `NXUS` v1 packet with CRC-16-CCITT | Announcements, probes, RAM/backend/model/RPC status, and a short-lived local cache. It is not an authenticated protocol or a general peer-state synchronization service. |
| Inference | HTTP to the selected node’s llama-server API (normally TCP 8080); `/health`, `/v1/models`, `/v1/chat/completions` | OpenAI-compatible requests. Streaming responses are parsed as SSE by `reqwest` plus `eventsource-stream`. A `NexusClient` targets one base URL at a time. |
| Distributed model offload | llama.cpp `rpc-server` endpoint (normally TCP 50052), optionally reached over the existing ADB tunnel | Current planning chooses one RPC-ready peer and emits a single `--rpc` endpoint. The code has no N-worker scheduler or aggregate peer budget. |
| USB alternative | ADB port forwarding/reverse forwarding | Useful for the established phone/Mac topology, but not a general onboarding/discovery protocol for arbitrary Wi-Fi peers. |

The SSE implementation is a **data-plane stream for one chat completion**, not a live cluster event feed. It parses generated chunks and yields text; it does not carry node membership, model inventory changes, health, peer joins/leaves, or worker scheduling state.

### What the source currently does

- `src/discovery.rs` serializes a fixed packet containing magic/version, role/status bits, UUID, API/RPC ports, memory, backend, thermal index, a 24-byte model label, and CRC. The listener binds IPv4 `0.0.0.0:<discovery_port>` (default 9999), validates exact packet length, CRC, magic, and version, and derives the peer IP from the UDP sender. The broadcaster targets limited broadcast, IPv4 directed broadcasts found through Unix `getifaddrs`, loopback, and configured static peers. A host replies by unicast when it receives a client-role beacon/probe.
- Each `DiscoveryService` instance owns an in-memory `HashMap<Uuid, PeerNode>` behind `Arc<RwLock<_>>`. Receiving a beacon replaces the row for that UUID and refreshes `last_seen`; stale rows are removed when `get_active_peers()` is called, using the configured 6-second default timeout. The cache is neither persistent nor shared between processes and has no watch/change-event API.
- `DiscoveryService::new(config, None)` generates a fresh random UUID. Although `NodeConfig` has an `id` field defaulted to `"auto"`, the discovery constructor does not use it. A restart therefore changes identity; distinct Nexus processes on the same physical machine can also advertise different identities.
- The peer role is a small host/client/standalone bitmask, not a model of heterogeneous capabilities. `find_best_host()` returns one highest-scoring host; `find_best_rpc_peer()` returns one RPC-ready peer with the most advertised free RAM. The RPC chooser does not allocate across several workers.
- `nexus client` can probe and resolve one host, then streams chat from that selected endpoint. Static peers and `default_host` are manual fallbacks. The dashboard can render a peer list, but the chat/client path does not offer a live, selectable peer/worker table.
- The no-subcommand TUI path is not equivalent to the explicit discovery path: it starts a listener, then initializes its client from `default_host`, ADB, or `127.0.0.1`; it does not autonomously resolve a discovered host before building the hub.
- `nexusd` starts the broadcaster and listener, then optionally launches one llama-server process. `ProcessSupervisor` supervises that local child and checks its `/health` endpoint; it is not a multi-node supervisor and does not establish or monitor a registry of remote workers. The RPC command separately advertises readiness and launches one `rpc-server` child.
- Current discovery labels and scoring are not a trust policy. CRC detects accidental corruption but does not authenticate a sender. A forged or replayed LAN beacon can claim an API/RPC endpoint or misleading resource telemetry. The inference and RPC connections are also not protected by an application-level peer identity in this code.

### Scaling bottlenecks and edge cases

1. **Broadcast-only first contact.** The implementation sends IPv4 broadcasts; directed broadcast delivery is often disabled by routers, and Wi-Fi guest/AP client isolation, multicast suppression, VLAN boundaries, firewalls, VPN interfaces, and mobile network behavior can prevent peer packets. IPv4-only addressing excludes IPv6-only links. The current implementation has no DNS-SD service browsing or alternate onboarding discovery protocol. Static IPs and explicit `--host` remain workarounds, not zero-configuration discovery.
2. **Discovery transport is conflated with state.** The beacon is suitable for compact periodic telemetry but has fixed fields and no generic capabilities, protocol negotiation, endpoint set, service lifecycle, detailed model inventory, or schema evolution beyond a single version byte. The 24-byte active-model field is especially unsuitable as a general metadata channel. mDNS can find a service but does not itself synchronize a complete, dynamic peer state table.
3. **Ephemeral identity and process-local state.** Random per-process UUIDs and a process-local cache make restarts appear as new nodes and prevent stable selection/pairing. Peer expiry is lazy (only on cache reads), there is no explicit “goodbye”/down transition, no subscriptions for UI updates, and no conflict policy for duplicate identities or multiple interfaces.
4. **Single-host and single-worker assumptions.** Host selection is a scalar “best host”; the inference client connects to one endpoint; the RPC planner accepts one remote endpoint and applies fixed Node A/Node B memory assumptions. Additional Android/Linux peers may have different capabilities and safe budgets. The daemon uses configured `max_rpc_ram_mb` as the remote budget rather than a per-peer validated allocatable budget. No N-worker placement, health-based failover, protocol compatibility check, or anchor-preserving scheduler exists.
5. **The anchors are conventions, not enforced identity.** Several paths default node role to `host`; roles are not a rich typed capability model, and the default `node.id` is not wired into peer identity. Nothing in the registry currently guarantees that Node A stays the primary inference anchor or Node B the primary operator/client anchor when more host-like peers appear.
6. **Shared UDP port behavior deserves care.** Listener sockets use `SO_REUSEADDR` and attempt `SO_REUSEPORT` on Unix. Semantics vary across platforms; multiple local processes binding the same port can result in packets being delivered to one socket rather than copied to every process. Define one discovery owner per process/host and test coexistence of `nexusd`, `nexus rpc`, and client/dashboard modes on each target.
7. **False-ready telemetry.** The discovery service initializes READY from configuration, and daemon model metadata can be set before the server has passed health checks. Dynamic statuses are not comprehensively tied to supervisor lifecycle. A multi-node scheduler must not treat an unverified advertisement as proof that an endpoint is ready.
8. **Zero-configuration is not zero-trust.** Automatically finding a peer on Wi-Fi must not automatically grant it model/RPC access. Discovery must be followed by endpoint validation, identity verification/pairing, and resource freshness checks. Plain HTTP/SSE and the RPC socket should be considered trusted-LAN-only until an authentication/encryption plan exists.

## 2. Candidate technologies and comparison

### A. `mdns-sd` — recommended first integration for LAN discovery

`mdns-sd` (MIT OR Apache-2.0) provides a safe-Rust Multicast DNS/DNS-SD daemon. A node can register a service and TXT properties; another node can browse the Nexus service type and receive resolved service events/addresses. It uses a daemon thread and channels, supports synchronous or async callers without requiring Tokio, and avoids a Bonjour/Avahi library binding. Its repository describes macOS and Linux support and reports compatibility testing with Avahi on Linux, `dns-sd` on macOS, and Bonjour on iOS. It supports IPv4 and IPv6.

**Advantages:** zero-configuration service naming and endpoint discovery on the same multicast link; service add/remove and address-change events map well to peer lifecycle; modest/simple API compared with a full P2P stack; no need for a separate router or C/C++ binding; integrates alongside Tokio via async channel receives.

**Limits and caveats:** DNS-SD provides discovery and service metadata, not a replicated Nexus registry, authentication, gossip, or inference transport. Keep TXT records small (stable peer ID, API/RPC port, protocol version, role/capability hints); fetch/verify dynamic and security-sensitive state separately. mDNS uses link-local multicast (normally UDP 5353) and does not cross routed subnets by default; AP isolation or multicast filtering can still block it. The crate’s published platform list does **not specifically promise Android/Termux support**. Termux is not the same as a native Android application with an explicit Android Wi-Fi multicast lock API, so compile and runtime behavior must be tested on the actual target; report daemon monitor errors and retain a fallback. The crate documents itself as beta and documents RFC 6762 compliance limitations (for example, not every optional unicast behavior is implemented).

**Recommendation:** prototype this crate first, feature-gate it if appropriate, publish `_nexus._tcp.local.` (or a versioned Nexus service type), and keep UDP v1/v2 as fallback during a migration window.

### B. Selective `libp2p` primitives — possible future peer-state overlay

Rust `libp2p` is a modular P2P stack. For a future overlay, relevant components are **mDNS** for discovering libp2p peers on the local network, **Identify** for exchanging peer/protocol identity information, **GossipSub** for disseminating small peer-state or membership events, and **request-response** for direct queries. A libp2p peer ID and signed messages can improve identity semantics over unauthenticated beacons. Features are selectable, but the integration still adds a swarm, transport, identity, protocol behavior, event handling, and operational tuning.

**Advantages:** designed for peer-to-peer connections and protocol identity; GossipSub can relay updates when not every peer has direct contact; protocol composition provides a path to a true mesh or routed overlay if the network later extends beyond one LAN.

**Limits and caveats:** mDNS discovers only local-network libp2p peers; GossipSub is a state dissemination/routing protocol, **not peer discovery itself**. It is more architecture than required for a small same-Wi-Fi cluster and creates substantially more dependency/runtime/configuration surface than service discovery plus direct HTTP. Restrict payloads to membership/telemetry events—never stream model tensors or token traffic through gossip. Rust source portability does not prove Android Termux socket, multicast, entropy, and radio behavior; compile the exact `aarch64-linux-android` target and test on-device. Select only needed crate features and benchmark binary size, idle RSS, CPU, and battery before adoption. Preserve the x86_64 Penryn build baseline; do not use `target-cpu=native` or enable AVX/FMA/SSE4.2.

**Recommendation:** defer. Consider a minimal `mdns + identify + request-response` prototype only if same-link mDNS plus direct state exchange demonstrably fails scale/roaming needs. Add GossipSub only when multi-hop dissemination is a real requirement. LAN mDNS does not make a multi-subnet topology automatically work; that needs explicit routing/relay/bootstrap design.

### C. Eclipse `zenoh` — capable pub/sub/query alternative, likely excessive initially

Zenoh is a Rust-first pub/sub and query/reply network with peer, router, and client roles, arbitrary graph topologies, and multicast scouting options. It offers a more integrated network for exchanging state and querying nodes than the current custom beacon. Its Rust API and `zenohd` router are separate choices: using the client library does not require deploying the standalone router on every node.

**Advantages:** discovery/scouting plus state pub/sub and query/reply can reduce custom control-plane code; peer/router/client modes can represent mesh or anchored-router layouts; useful if Nexus grows into a broader edge-data system.

**Limits and caveats:** the current crate has a broad dependency graph and multiple optional transports/features; this is materially heavier and more complex than a small discovery component. Do not add a Zenoh router as a hidden prerequisite on the 12-GB phone or 3.6-GiB legacy workstation without measuring its persistent memory/CPU cost. The project documents macOS and Debian prebuilt router installation; that does **not** establish a prebuilt Termux `zenohd`. Zenoh is Rust-native, but the sources checked here do not guarantee the Android Termux target/runtime combination. Prototype `zenoh` client compilation and multicast scouting on Android rather than assuming support. Even where it works, Wi-Fi multicast filtering/AP isolation still applies. The inference HTTP/SSE and llama.cpp RPC planes would remain separate unless intentionally migrated.

**Recommendation:** do not introduce in the first expansion. Revisit if the system needs a shared pub/sub/query fabric beyond local discovery and bounded cluster telemetry, and only after device-level resource and Android-target tests.

### Other option considered: `astro-dnssd`

`astro-dnssd` is a Rust wrapper around DNS-SD/Bonjour APIs, but its documented build requirements include the Bonjour SDK and Linux’s `avahi-compat-libdns_sd` compatibility library. It is not an attractive uniform dependency for Termux, macOS, and Debian when the goal is to avoid native service-library installation/bindings. Prefer `mdns-sd` for the first prototype; keep the system Bonjour/Avahi implementation as an interoperability reference, not a mandatory Nexus runtime dependency.

### Keep custom UDP as a complementary methodology, not the sole discovery path

The current Tokio UDP approach has very low protocol and payload overhead, already carries memory/thermal/RPC telemetry, and uses dependencies already present. It is easy to preserve and can operate as a periodic heartbeat/fallback. Tokio exposes broadcast and multicast socket controls, but correct per-interface address selection, broadcast permissions, socket reuse, multicast membership, IPv6, firewall behavior, and Android radio policy remain OS/network concerns. Broadcast delivery is not guaranteed by “same Wi-Fi.” A CRC is not a signature. Continue to support explicit endpoints/static peers and, if helpful, ADB; no mDNS, UDP, libp2p, or Zenoh approach can bypass Wi-Fi client isolation or discover across a router without additional network configuration.

| Option | LAN zero-config discovery | Dynamic peer-state sync | Runtime/dependency burden | macOS + Debian | Android/Termux confidence | Fit for first step |
|---|---|---|---|---|---|---|
| Current UDP beacon | Yes, when IPv4 broadcast reaches the peer | Heartbeat snapshot only; local cache | Lowest; already implemented | Existing code is Unix-oriented, with a non-Unix broadcast-address fallback | Needs actual device/network testing | Retain as fallback/telemetry |
| `mdns-sd` + Nexus registry | Yes, same link | Nexus implements this separately; mDNS gives service events/metadata | Low-to-moderate; Rust thread/channels, no native daemon required | Documented macOS/Linux support | Not explicitly guaranteed by crate docs; prototype | **Recommended** |
| Selective libp2p | Yes via libp2p mDNS | Identify/request-response; GossipSub for dissemination | Highest of these options; transport/swarm/protocol composition | Mature cross-platform Rust ecosystem | Exact Android target and multicast behavior must be validated | Defer until mesh requirements justify |
| Zenoh | Yes where multicast scouting/network permits | Built-in pub/sub and query/reply | Broad stack/configuration; router optional but may add an operation | macOS/Debian router packages; Rust client ecosystem | Termux router/runtime availability not established by the sources checked | Defer; evaluate as a later control-plane alternative |

## 3. Proposed target architecture

```text
                       local Wi-Fi / one L2 multicast domain
       mDNS service discovery + optional legacy/versioned UDP heartbeat
     ┌──────────────┐     ┌──────────────┐     ┌─────────────────┐
     │ Node A       │     │ Node B       │     │ Additional peers│
     │ primary      │     │ primary      │     │ Android/Linux   │
     │ compute      │     │ operator/TUI │     │ member/worker   │
     └──────┬───────┘     └──────┬───────┘     └────────┬────────┘
            └──────────── peer registry/control plane ────────────┘
                 direct health/state exchange; paired identity

       Inference data plane: HTTP REST + SSE to selected host
       Model offload data plane: llama.cpp RPC, layer mode only
       Optional existing USB data path: ADB forward/reverse
```

Every Nexus runtime runs the same discovery/registry logic; there is no required central discovery server. Node A and Node B remain pinned **anchors** in policy and UX. The registry can mark other nodes as `member`, `client`, or `rpc_worker` (possibly with several capabilities), but it must not silently elect an additional Android/Linux node as primary compute or demote Node A. For workloads, a policy engine considers Node A first, preserves whole-model-local execution whenever it fits Node A’s guarded budget, and only considers workers if Node A’s budget is exceeded. Node B remains the primary TUI/operator anchor; an extra client does not replace that role.

### Cross-platform startup targets

Native platform support should be incremental and should not weaken the existing hardware safeguards:

- **Termux/Android:** `nexus host` is the primary deployment. Preserve the `/proc/meminfo` guard, Vulkan/ARM acceleration, and local-first inference policy. Validate mDNS and UDP behavior on real Galaxy hardware rather than assuming that Linux support implies Android support.
- **Debian/macOS:** `nexus client` is the primary operator deployment. Preserve the Penryn-safe x86 baseline and the 1,800 MB RPC-worker ceiling when the Mac is used as a worker.
- **Native Windows/PowerShell:** build a Windows binary and allow `nexus client`, `nexus host`, or `nexus worker`. Document Windows Firewall rules for TCP 8080, optional TCP 50052, UDP 9999, and mDNS. Do not assume that a firewall automatically permits discovery.
- **Windows WSL2:** treat WSL as a separate Linux runtime, not as native Windows networking. WSL2 NAT can prevent LAN broadcast/mDNS reachability, and Windows Firewall can still affect the process. Keep `--host`, `static_peers`, and `default_host` as first-class fallbacks; automatic discovery is a validation item for each WSL networking mode.

Discovery failures must be visible and actionable. A client should report whether it used mDNS, UDP, a configured peer, ADB, or an explicit host, rather than silently falling back to an unexpected endpoint.

### Peer record and lifecycle

Replace the informal “one beacon equals a peer row” model with an explicit `PeerRecord`/registry concept containing:

- Stable `NodeId` persisted once (UUID) and human-readable name; cluster ID and protocol/schema versions.
- Anchor designation (`primary_compute`, `primary_client`, or ordinary member) separate from capabilities and runtime role.
- A set of resolved addresses/endpoints with transport, interface/scope, API port, RPC port, and supported protocol versions; do not infer reachability solely from the UDP source address.
- Capabilities and validated resource facts: inference/RPC/client, backend/architecture, model/RPC compatibility, available and allocatable memory, thermal/health state, and observation timestamp.
- Lifecycle (`discovered`, `unverified`, `paired`, `healthy`, `stale`, `removed`) with monotonic last-seen timestamps, duplicate-ID/address conflict handling, and explicit expiry/withdrawal.
- Change notifications to consumers (e.g. Tokio `watch`/`broadcast` or an internal channel), so TUI and scheduler update incrementally rather than poll unrelated snapshots.

mDNS TXT data should be intentionally small and non-secret. On service resolution, connect to a Nexus-owned control-plane handshake/health endpoint to fetch a versioned structured state snapshot and verify the advertised identity, endpoints, and capabilities. Update dynamic state through direct periodic heartbeat/state exchanges; retain the existing UDP packet as an economical snapshot/compatibility channel. If later choosing GossipSub or Zenoh, publish the same versioned peer events through that layer rather than changing the UI or inference API.

**Security boundary:** discover first, trust second. Pair Node A and Node B as anchors; present an approval/pairing flow for new peers or require a pre-provisioned cluster credential. Store paired public identities/keys. Sign or authenticate state/control exchanges and authenticate RPC eligibility before scheduling. Do not broadcast secrets or authorize a peer based solely on CRC, TXT metadata, UUID, or a claimed free-RAM value. “Automatic discovery” can be autonomous while new-peer trust remains explicit.

## 4. Configuration evolution proposal

Extend `~/.nexus/config.toml` additively and use Serde defaults so current configs continue loading. Keep the existing `[network]` endpoint values, `static_peers`, and `default_host` as compatibility/manual override inputs. Generate and persist a UUID on first run when `node.id = "auto"`; do not generate a new identity on every `DiscoveryService` construction. Prefer typed enums for role/anchor rather than expanding string matching ad hoc.

Illustrative proposed configuration (field names are design suggestions, not existing fields):

```toml
[node]
id = "persisted-uuid-created-on-first-run"
name = "nexus-phone"
role = "host"                   # host, client, worker, member
anchor = "primary_compute"       # primary_compute, primary_client, member

[network]
api_host = "0.0.0.0"
api_port = 8080
discovery_port = 9999
static_peers = []                 # retained for manual fallback
default_host = ""                 # retained; optional operator override

[network.discovery]
mode = "hybrid"                   # mdns, udp, hybrid, off
mdns_enabled = true
udp_fallback = true
service_type = "_nexus._tcp.local."
advertise_interval_ms = 2000
peer_timeout_ms = 6000
ipv4_broadcast = true
ipv6_multicast = true              # enable only after platform tests

[network.security]
trust_mode = "paired"             # paired, pre_shared_cluster, open_read_only
require_peer_auth = true
# No secret belongs in mDNS/TXT or a beacon.

[cluster]
primary_compute_id = "persisted-node-a-uuid"
primary_client_id = "persisted-node-b-uuid"
auto_offload = true
prefer_adb_tunnel = true
rpc_port = 50052
max_rpc_ram_mb = 1800              # preserve hard Node B ceiling
max_workers = 1                    # conservative until N-worker llama.cpp is validated
```

The role in configuration is a default only; the startup subcommand overrides it. For example, the same Windows or Termux installation may use `nexus client` during normal operation and `nexus host` when it is temporarily serving a local model. Capabilities should be advertised independently of the selected role because a node may provide both inference and RPC services.

Additional constraints for implementation:

- Keep Node A’s configured/model-fit rule and 8.5 GB standalone cap. Resource telemetry must not override `MemAvailable` and the 75% LMK guard.
- Keep Node B’s RPC worker at or below 1,800 MB regardless of advertised RAM. For other workers, advertise and validate a local policy-capped **allocatable** value; total free RAM is not the same as available RPC budget.
- Treat `primary_compute_id` and `primary_client_id` as pinned identity references, not “best peer” scores. During initial setup, create/pair the two anchors; newly seen nodes default to ordinary members.
- Version the new discovery subtable and preserve defaults for absent keys. Do not make a new mDNS dependency or multicast socket mandatory for a user who disabled discovery or relies on USB/static endpoints.
- Keep network bind addresses distinct from advertised addresses. Support multiple IPv4/IPv6/interface addresses, but reject link-local IPv6 without its interface scope and verify a peer by a real health/control exchange.

## 5. Actionable implementation roadmap

### Phase 0 — Baseline and compatibility tests

1. Record current behavior and packet fixtures for UDP v1, peer expiry, RPC-ready selection, host scoring, SSE chunk parsing, ADB routing, and config serialization. Add explicit assertions for stable node identity across process restarts once persistence is implemented.
2. Add a small LAN test matrix: Node A Termux ↔ Node B Debian, secondary Termux device, Linux peer; check Wi-Fi client isolation, multicast/broadcast filtering, multiple interfaces, IPv4 and IPv6, and concurrent local listener behavior.
3. Measure release binary size and idle RSS/CPU for `nexusd` on Node A and `nexus`/RPC worker on Node B before adding a discovery dependency. Keep the Penryn target flags intact and ensure no modern x86 instructions are enabled.

### Phase 1 — Stable identity and backward-compatible config (`src/config.rs`)

1. Add typed role/anchor/capability/config types and the `[network.discovery]` and `[network.security]` sections with Serde defaults. Continue reading existing `[network]` values and preserving `static_peers`/`default_host` fallbacks.
2. On first run, create and persist a stable node UUID; use it in every service advertisement, beacon, registry key, and pairing record. Avoid constructors silently generating a different ID for each subsystem.
3. Add config validation for reserved ports, legal timing, explicit anchor IDs, peer policy, and per-node memory ceilings. Treat malformed discovery config as a reported configuration error, not as an unlogged silent fallback.
4. Set anchor identities for the S23 Ultra and Node B as an operator-controlled bootstrap step. Additional nodes join as members and cannot claim either anchor ID.
5. Add explicit `host`, `client`, and `worker` startup commands in `src/main.rs`. Share host startup between `nexus host` and the existing `nexusd` binary/entry point; keep `nexus` with no subcommand as the unified hub.
6. Add client selection controls: `--host <URL>` for a direct endpoint, `--node <ID-or-name>` for a discovered node, and an interactive host list when neither is supplied. Preserve `default_host`, static peers, ADB, and localhost as ordered fallback paths.

### Phase 2 — Discovery abstraction and mDNS prototype (`src/discovery.rs` or `src/network/`)

1. Introduce a narrow discovery backend interface that emits `ServiceFound`, `ServiceUpdated`, `ServiceRemoved`, and backend-health events. Keep UDP and mDNS implementations separate behind this boundary.
2. Add `mdns-sd` with the smallest suitable feature/dependency footprint. Register/browse a versioned Nexus service type; advertise stable node ID, cluster ID, control/API/RPC ports, and short protocol/role hints. Never put secrets or dynamic large telemetry into TXT data.
3. Consume resolved addresses and service removal events. Listen to `mdns-sd`’s daemon monitor and surface multicast initialization/runtime errors. On error or unsupported platform, log the reason and continue with UDP/static/ADB fallback.
4. Preserve 64-byte UDP v1 decoding and broadcasting during rollout. Define a v2 extension/typed telemetry schema for capabilities and allocatable resources; do not repurpose v1 bytes incompatibly. Keep v1/v2 packets bounded and reject unknown versions safely.
5. Persist per-node identity while allowing distinct service addresses per interface. Test Termux Bionic, actual Galaxy Wi-Fi behavior, macOS/Debian interoperability, and mDNS against Bonjour/Avahi before enabling it by default on Android.

### Phase 3 — Dynamic peer registry and control-plane validation

1. Add a registry owned by one Nexus runtime per host, not independent uncoordinated discovery tasks for each view. It should merge service events, UDP observations, configured static peers, and verified control-plane replies by stable node ID.
2. Add an event-driven cache with explicit stale/removal lifecycle, endpoint conflict handling, last-seen timestamps, protocol compatibility, and bounds on peer count/metadata. Expire entries in a periodic maintenance task as well as when queried.
3. Add a lightweight Nexus control-plane handshake and versioned peer-state response, owned by `nexusd`/the node runtime rather than assuming llama-server provides cluster-management endpoints. It should validate node ID, protocol version, role/capabilities, readiness, and policy-capped allocatable memory. Keep llama-server’s existing health/model/chat API and SSE stream as the inference data plane.
4. Tie `READY`, `INFERRING`, `RPC_READY`, and active-model state to supervisor/RPC child lifecycle and successful health probes. Do not advertise ready before health succeeds; clear readiness before shutdown and after child exit.
5. Add pairing/authentication before enabling inference/RPC access. Reject spoofed/unpaired peers for privileged worker use, rate-limit handshakes, and treat LAN telemetry as untrusted input.

### Phase 4 — Daemon, supervisor, and safe worker selection

1. Refactor `nexusd` startup to build one node identity, registry, discovery backend(s), and local service manager; retain cancellation/join handling for background discovery tasks. The daemon should still launch/manage a local llama-server first and must preserve Node A’s LMK preflight.
2. Refactor `ProcessSupervisor` only where required to report lifecycle/readiness and bind state changes to the registry. Keep the existing subprocess boundary; do not introduce C/C++ networking bindings into Rust.
3. Replace `find_best_host` as the global authority with two separate APIs: resolve the pinned primary compute anchor for normal inference, and enumerate eligible peers for a specific task. Other compute-capable nodes are not promoted to primary merely because they advertise more RAM/Vulkan.
4. Change RPC worker selection from one max-free-RAM peer to a policy-filtered candidate list: paired/healthy, compatible RPC protocol, correct architecture/runtime, measured allocatable budget, thermal/health state, and per-node cap. Initially choose one eligible peer deterministically; record rationale and fallback on failure.
5. Keep existing host-first behavior: if Node A can safely hold model plus KV cache, do not offload. Preserve sequential layer mode. Do not claim N-worker inference until the deployed llama.cpp `--rpc` semantics and tensor/layer allocation have been experimentally verified; the current code emits one endpoint and Node B’s 1,800 MB cap remains binding.

### Phase 5 — TUI/client onboarding and live peer UX (`src/main.rs`, `src/client.rs`, `src/ui/`)

1. Route both `nexus client` and the no-subcommand hub through the same endpoint-resolution service. Fix the default hub path so it can resolve the pinned Node A anchor before falling back to `default_host`, ADB, or localhost.
2. Add a live peer/anchor chooser to the dashboard or a dedicated TUI view. Show stable name/ID, anchor badge, role/capabilities, API/RPC endpoints, protocol version, verified/paired status, readiness, available **allocatable** memory, thermal state, and stale/health reason.
3. Make chat connect to the selected/pinned compute host, and allow an explicit failover only after displaying a clear primary-anchor status. Keep manual `--host`, `default_host`, static peers, and ADB as escape hatches.
4. Listen to registry change events so the TUI updates on joins/leaves/address changes without restarting. Avoid excessive dashboard polling on Node B’s legacy Core 2 Duo.
5. Keep `NexusClient` and its REST/SSE response parser transport-agnostic at the API level: resolve a verified base URL first, then use existing reqwest requests and SSE stream. Add reconnect/clear failure behavior without changing the OpenAI-compatible request format.
6. Add platform runbooks and validation to the release checklist: Termux Android, Debian/macOS, native Windows PowerShell, and WSL2. For WSL2, verify both discovery behavior and explicit-endpoint operation under the active NAT/firewall mode.

### Phase 6 — Scale gates and optional overlay evaluation

1. Load-test discovery, registry churn, and UI with realistic counts (for example 2, 5, 10, then 25 peers), duplicate/stale advertisements, node sleep/wake, address changes, and noisy/untrusted beacons. Measure packet rate and idle overhead.
2. Test multiple subnets separately. If service discovery across routed networks becomes a hard requirement, add a deliberate bootstrap/relay/router and firewall configuration story; do not promise that local mDNS or broadcast traverses routers.
3. Only if direct state exchange is no longer adequate, prototype selective libp2p behaviors or Zenoh against the same peer-record API and compare binary size, RSS, battery, build time, target reliability, operational simplicity, and security. Keep the implementation behind a backend boundary so the TUI and anchors remain unchanged.
4. Approve multi-worker/offload expansion only after a llama.cpp RPC integration test demonstrates compatible N-worker layer allocation, failure handling, aggregate memory safety, and no network row-splitting. Run `cargo test`, `cargo check --target aarch64-linux-android`, and `cargo check --target x86_64-unknown-linux-gnu` on their actual target environments; verify Node B’s 1,800 MB cap and Node A’s 75% available-memory rule.

## 6. Acceptance criteria

- A fresh Nexus member on the same Wi-Fi link can find the cluster with no manually entered IP when mDNS is available; when it is blocked/unsupported, the client reports the reason and attempts configured UDP/static/ADB fallbacks.
- Peer identity remains stable across process restart. Multiple interface addresses and service updates merge into one registry record; stale peers transition out predictably and notify the TUI.
- Discovery alone cannot authorize inference or RPC. A paired peer’s advertised endpoint and capabilities are confirmed through an authenticated/versioned control exchange.
- Node A and Node B are visibly and configurably pinned as primary compute/operator anchors; adding a better-resourced peer cannot silently replace them.
- Models within Node A’s safe local budget remain 100% on Node A. Node A’s 8.5 GB standalone cap, Android 75% LMK guard, sequential `--split-mode layer` requirement, and Node B’s 1,800 MB RPC maximum remain enforced.
- HTTP REST/SSE inference behavior remains compatible, while peer membership and state are delivered through the control plane rather than chat streams.
- The same Wi-Fi behavior is exercised on real Termux ARM64 and Debian x86_64 devices; macOS interoperability is separately verified. Cross-compilation alone is not accepted as proof of Android multicast support.
- Resource measurements show that idle discovery/registry overhead fits the project’s ultra-lightweight goal on both anchors.

## 7. Research references

- Current project source: [`src/discovery.rs`](src/discovery.rs), [`src/daemon.rs`](src/daemon.rs), [`src/supervisor.rs`](src/supervisor.rs), [`src/config.rs`](src/config.rs), [`src/client.rs`](src/client.rs), [`src/cluster.rs`](src/cluster.rs), [`src/main.rs`](src/main.rs), [`src/ui/hub.rs`](src/ui/hub.rs), [`src/ui/dashboard.rs`](src/ui/dashboard.rs).
- Project constraints and phase contracts: [`AGENTS.md`](AGENTS.md), [`DESIGN_SPEC.md`](DESIGN_SPEC.md), [`BUILD_PLAN.md`](BUILD_PLAN.md), [`README.md`](README.md).
- `mdns-sd` crate and platform notes: <https://docs.rs/mdns-sd/latest/mdns_sd/> and <https://github.com/keepsimple1/mdns-sd>.
- `mdns-sd` daemon/socket/error API: <https://docs.rs/mdns-sd/latest/mdns_sd/struct.ServiceDaemon.html> and <https://docs.rs/mdns-sd/latest/mdns_sd/enum.DaemonEvent.html>.
- `astro-dnssd` native DNS-SD requirements: <https://github.com/astrohq/dnssd-rs>.
- Rust libp2p overview and feature composition: <https://docs.rs/libp2p/latest/libp2p/> and <https://github.com/libp2p/rust-libp2p>.
- libp2p local discovery and gossip semantics: <https://docs.rs/libp2p-mdns/latest/libp2p_mdns/> and <https://docs.rs/libp2p-gossipsub/latest/libp2p_gossipsub/>.
- Zenoh Rust API and installation/router availability: <https://docs.rs/zenoh/latest/zenoh/>, <https://docs.rs/zenoh/latest/zenoh/config/>, and <https://zenoh.io/docs/getting-started/installation/>.
- Tokio UDP API (broadcast, multicast, and socket behavior): <https://docs.rs/tokio/latest/tokio/net/struct.UdpSocket.html>.
