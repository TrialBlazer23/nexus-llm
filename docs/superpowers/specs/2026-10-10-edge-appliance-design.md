# Edge appliance design

**Date:** 2026-10-10
**Status:** Locked 2026-10-10. Operator approved this file. Implementation follows `docs/superpowers/plans/2026-10-10-edge-appliance.md`.
**Scope:** Always-on front door on the local node, burst inference on any Android peer, honest fit, USB and Wi-Fi as links

This spec turns the mesh into one prompt box. The machine you type on answers ordinary prompts. Any Android phone running Nexus in Termux can take a heavy prompt. A cable is a fast link. Wi-Fi is another link. Product names, RAM sizes, and DeX are not inputs to the code.

## 1. Problem

Placement still treats every peer as a slice of one model. `prefer_adb_tunnel` makes that worse on a mixed desk: `AdbTunnelSupervisor::setup_tunnel` with no serial keeps the first authorized device, and hub startup in `src/main.rs` replaces the chat host with a single `http://127.0.0.1:8080`. A second cabled phone is ignored. A Wi-Fi phone disappears from the default route whenever any cable is plugged in.

Fit is optimistic on small RAM. With `mmap` and no `mlock`, `MemoryPlan` counts about 10% of the weight file as pressure (`src/cluster/memory.rs`). A dense 7B can be marked `Fits` on a 4 GB CPU and then swap or trip the Android low-memory killer.

`battery_floor_percent` and `battery_action` are stored in config and echoed by the gateway settings API. Nothing reads battery capacity before a load. The 64-byte beacon has no spare byte for it.

Throughput priors treat every `x86_64` CPU as `X86Baseline` at 4 tokens/s, independent of model size (`predict_local_tok_s`). Measured samples in `~/.nexus/bench.json` override that only after a bench exists.

## 2. Decisions

1. A node id is the UUID Nexus already persists on that machine. An ADB serial identifies a cable. The `model:` token from `adb devices -l` is a Tunnel-tab label and nothing else.
2. Every authorized ADB device gets its own localhost forward. LAN discovery keeps running beside those forwards.
3. Peers merge on UUID. One node has one registry entry. While its USB forward answers, chat and blob traffic use the localhost endpoint.
4. The hub chats through the local mesh gateway when `network.gateway_enabled` is true. The gateway chooses the node. USB no longer replaces the gateway URL.
5. Ordinary prompts stay on a small model on the local node when one fits. Heavy routes escalate to the best eligible peer.
6. A healthy USB peer beats a healthy Wi-Fi peer only inside a 15% predicted-tokens/s band. A hot or low-battery USB peer loses. A USB peer more than 15% slower loses.
7. Dense fit checks count the full weight file plus KV plus the compute buffer. MoE flash streaming keeps its resident-set planner.
8. Battery and power source travel in control-plane state and mDNS TXT. Beacon v1 stays 64 bytes.

## 3. Out of scope

- Beacon version 2, networked MoE expert routing, and new discovery stacks.
- Cross-compiling or pushing `llama-server` binaries to old machines.
- A DeX mode, a display-mirroring mode, or any phone-specific preset.
- Downloading a model from the internet as part of the fit ladder. The ladder chooses files already in the pantry.
- Changing Penryn rustflags or the Android 75% memory ceiling.

## 4. Terms

| Term | Meaning |
|---|---|
| Local node | The machine running the hub or `nexusd` the operator is using. Its budget comes from `SystemProfile::probe()`. |
| USB peer | An authorized `adb devices` row whose forwarded control port answered `POST /nexus/control/v1/state`. |
| Wi-Fi peer | A node learned from the UDP beacon or mDNS. |
| Pantry | The local content-addressed model store (`src/store.rs`) plus files in `models_dir`. |
| Interactive plan | A placement whose predicted tokens/s is at least `interactive_floor_tok_s`. |
| Batch-only plan | A placement that fits in RAM and is slower than the floor. Chat will not select it. |
| Burst peer | A USB or Wi-Fi peer chosen for one heavy route. |

## 5. Topology

Three kinds of participant, distinguished by probe and link:

- **Local node.** Runs the gateway, the knowledge base, the pantry, and, when the probed budget allows, one small orchestrator model. It stays loaded while the hub or daemon is up.
- **USB Android.** Any Termux device `adb` lists as `device`. Nexus on that phone binds its normal `api_port` and `control_port`. The local node forwards unique host ports to those device ports.
- **Wi-Fi Android.** Any Termux device that beacons. An external display, a broken panel, or DeX does not change the protocol. The pairing code is whatever that phone already prints on the display attached to it.

A phone that is both cabled and on the LAN is one peer. Unplugging the cable drops the USB endpoint. If the beacon is still fresh, the same UUID remains reachable on Wi-Fi. Plugging in a different phone adds a peer after that phone's own state probe returns its UUID.

## 6. Identity and what must not be hardcoded

Routing, fit, and rank read only:

- node UUID, display name, role, and capabilities from the peer's own Nexus state
- `total_ram_mb`, `free_ram_mb`, and `allocatable_memory_mb` from that peer's probe
- `AccelerationBackend` from that peer's probe
- thermal index, battery percent, and power source from that peer
- link class and measured or assigned link quality
- GGUF metadata and bench samples

The following are forbidden as behavior inputs: marketing names, `ro.product.model`, ADB `product:` strings, fixed RAM constants for a known device, and a built-in table of serials. Tests use synthetic serials such as `serial-a`. They must not branch on a real product string.

`config.node.name` on each phone remains the operator-facing label (`PeerNode.display_name`). If the name is empty, the label stays `Node-<uuid8>`.

## 7. Multi-device ADB

`AdbTunnelSupervisor::list_devices` already parses `adb devices -l` into serial, state, `model:`, and `product:`. Extend it. Do not replace it with a device database.

### 7.1 Which rows become links

- State `device` is authorized and eligible.
- Any other state (`unauthorized`, `offline`, empty) is shown in the Tunnel tab and is not forwarded.
- Sort eligible serials by raw serial bytes so host ports stay stable across refreshes.
- Cap the list at `cluster.adb.max_devices` (default 8). If `adb` returns more, forward the first 8 in sorted order and record a doctor warning that names the skipped serials.

### 7.2 Host ports

Device-side ports stay the phone's ports. Two phones both listening on device port 8080 need two different host ports.

For sorted index `i` (starting at 0):

- `host_api = host_port_base + i * 2`
- `host_control = host_api + 1`

`host_port_base` defaults to `18080`. The allocator refuses a base that collides with `network.api_port`, `network.control_port`, `network.gateway_port`, `network.discovery_port`, or `cluster.rpc_port`. Startup fails the ADB subsystem with a config error and leaves Wi-Fi discovery up.

Device-side ports, in order:

1. The ports stored for this serial in `~/.nexus/adb-links.json` after a previous successful probe.
2. Otherwise the local node's `network.api_port` and `network.control_port`. Setup writes the same defaults on every node, so first contact uses those.

Commands, per serial:

```text
adb -s <serial> forward tcp:<host_api> tcp:<device_api>
adb -s <serial> forward tcp:<host_control> tcp:<device_control>
```

Forwards bind through ADB's default localhost listener. This spec does not add a wildcard bind.

RPC reverse stays per serial so today's layer-split path still has a route, and it is not required for a burst peer:

```text
adb -s <serial> reverse tcp:<device_rpc> tcp:<local_rpc>
```

`device_rpc` follows the same stored-then-default rule, using `cluster.rpc_port`. Reverse listeners live on the phone, so every phone may use the same device rpc port.

### 7.3 Proving the cable is Nexus

After the control forward succeeds, `POST /nexus/control/v1/state` on `http://127.0.0.1:<host_control>`. Signing follows the existing rule: sign when `require_pairing` is true, allow the current unsigned request when it is false.

A successful body supplies the UUID, display name, allocatable memory, capabilities, and the new battery fields. Persist:

```json
{
  "links": [
    {
      "serial": "serial-a",
      "node_id": "uuid",
      "device_api_port": 8080,
      "device_control_port": 9998,
      "device_rpc_port": 50052
    }
  ]
}
```

The file is a cache of cables this machine has already probed. It is not a list of supported phones. On startup, ignore entries whose serial is absent from the current `adb devices` output. A serial with no successful probe is "cable up, Nexus not answering" and is not a placement candidate.

If the stored ports fail, retry once with the local default ports. If that also fails, leave the cache unchanged and skip the serial until the next refresh.

Refresh on the existing hub tick that already calls tunnel status (500 ms is too hot for `adb` process spawns). Probe ADB at most every 5 seconds, and immediately when the Tunnel tab requests a refresh.

### 7.4 Teardown

Removing a forward uses `adb -s <serial> forward --remove tcp:<host_api>` and the matching control port. `forward --remove` without a serial is not used, because it would drop every phone. A serial that vanishes from `adb devices` loses its forwards and its USB endpoint. The Wi-Fi record for that UUID is left in place.

## 8. Peer merge

`PeerNode` gains:

- `link_class`: `Local`, `Usb`, or `Wifi`
- `usb_serial`: set only for a live USB endpoint
- `usb_api_endpoint` and `usb_control_endpoint`: `http://127.0.0.1:<host_port>` while the forward is healthy
- `battery_percent`: `Option<u8>`
- `power_source`: `Ac`, `Battery`, or `Unknown`

Wi-Fi discovery is unchanged and still fills `addr`, ports, RAM, backend, and thermal index.

Merge key is the UUID.

- USB probe creates or updates the entry and sets `link_class = Usb`.
- A beacon or mDNS record for the same UUID updates RAM, backend, thermal, name, and the Wi-Fi address, and does not clear the USB endpoints.
- `api_endpoint()` and `control_endpoint()` return the localhost USB endpoints while `link_class` is `Usb` and the forward is healthy.
- When the forward is lost and the beacon is still inside `peer_timeout_ms`, set `link_class = Wifi` and use the beacon address.
- When both are gone, the existing reaper removes the peer.

mDNS TXT gains two optional keys, both ignored when absent:

- `bat`: integer 0 through 100
- `pwr`: `ac` or `battery`

TXT is a hint before the first state fetch. A later control-plane state overwrites it.

## 9. Battery and thermal

Each node probes itself.

Battery percent is the integer in the first readable file among `/sys/class/power_supply/*/capacity` whose sibling `type` file equals `Battery`. Power source is `ac` when any supply reports `status` of `Charging`, `Full`, or `Not charging`. It is `battery` when the battery status is `Discharging`. Machines with no battery directory report `battery_percent: null` and `power_source: ac`. A read error reports `unknown` and null percent. Null means eligible, not declined.

`ControlPlaneState` gains optional fields with serde defaults so older peers still parse:

```json
{
  "battery_percent": 40,
  "power_source": "battery"
}
```

`power_source` is the string `ac`, `battery`, or `unknown`.

The beacon layout is unchanged. UDP-only peers have battery data only after a state fetch. The registry verifier already fetches state. That fetch fills battery fields.

Thermal eligibility is the existing `max_thermal_index` of 75. A peer above that index is not offered a new load. The local orchestrator, once loaded, is not killed mid-reply by a thermal tick. The next new heavy load skips that peer.

`hardware.safety.battery_floor_percent` and `battery_action` apply at placement time:

| `battery_action` | When percent is below the floor | When percent is null |
|---|---|---|
| `decline` | Peer is ineligible for a new load | Eligible |
| `throttle` | Multiply that peer's predicted tokens/s by 0.5 | Eligible |
| `ignore` | No battery effect | Eligible |

Default values stay the ones already in `NexusConfig`. This spec does not change the default floor.

## 10. Appliance routing

Enabled by `inference.appliance.enabled` (default true).

### 10.1 Chat entry

When the appliance is enabled and `network.gateway_enabled` is true, the hub's client base URL is `http://127.0.0.1:<gateway_port>`. Startup does not call `resolve_transport_endpoint` to replace that URL. `prefer_adb_tunnel` still means "build USB links". It does not mean "the first phone is the only host".

When the gateway is disabled, keep today's explicit host selection, including a single USB endpoint if the operator asked for USB transport on `nexus client`.

### 10.2 Local orchestrator

On hub and `nexusd` start, if the appliance is enabled, load one local model:

1. The file named by `inference.appliance.orchestrator`, if it is set and the fit ladder accepts it on the local probe.
2. Otherwise the smallest local GGUF, by weight bytes, that the ladder accepts and whose matched preset tags include `orchestrator` or `router`.
3. Otherwise the smallest local GGUF the ladder accepts at context 1024 and Q8 KV.

If none fit, skip tier-2 classification. Tier-1 keyword routes and peer escalation still run. `nexus doctor` prints `appliance: no local model fits`.

The reference class for a Penryn-budget machine is a 1B–2B Q4 GGUF tagged `orchestrator`. That sentence is operator guidance. The binary does not embed a filename, a URL, or a parameter count.

The local model stays loaded for the life of the process. Escalating a prompt does not unload it.

Whichever file section 10.2 loads is the front door. For routing, it carries `general` and `chat` plus any tags from its preset. A small GGUF with no preset still keeps ordinary chat on the local node.

### 10.3 Who answers

The router already returns a tag. Apply it in this order:

1. Tier-1 keyword or `@tag` match. If the loaded local model carries that tag, answer locally. If it does not, escalate. The candidate set is every eligible peer whose pantry, or the local pantry, has a GGUF whose preset carries that tag. An already-loaded match is one candidate. It uses the same rank rules as the others.
2. If tier 1 misses and a local orchestrator is loaded, ask it (tier 2). A route whose tag the local model carries stays local. Any other route escalates.
3. A tier-2 parse failure or veto stays local and records the reason `fallback`.
4. If no burst peer has an interactive plan for that tag, stay local when the local model is loaded, and return a gateway error when it is not. Both outcomes name the tag and the reason `no interactive peer`. The status line and the `X-Nexus-Route` header carry that reason when the answer stays local only because escalation failed.

General chat tags (`general`, `chat`) stay local whenever the local model carries them. A faster phone does not steal ordinary prompts.

Escalation picks the best interactive plan from section 12. The chosen peer loads the selected GGUF if it is not already the active model. Blob transfer, when needed, uses section 13.

### 10.4 Burst idle

A model loaded on a burst peer for an escalation unloads after `inference.appliance.burst_idle_secs` with no new request to that model (default 600). The local orchestrator is exempt. Unload uses the existing `POST /nexus/control/v1/model/unload` against that peer's current control endpoint (USB localhost or Wi-Fi).

## 11. Fit ladder

Run this ladder per candidate before rank. Stop at the first step that fits. That step is the plan. If its predicted rate is below the floor, the candidate is batch-only. The ladder does not continue in search of a faster step, because context cuts do not change the cold-start prior. The requested context is the operator's current context, default 4096.

Dense load estimate, in MB, is:

```text
weights_mb + kv_mb + compute_buffer_mb
```

`weights_mb` is the full GGUF weight-byte size divided by 1024². The 10% mmap soft pressure is not used for the verdict. Runtime may still pass `--mmap` when config asks for it. `mlock` continues to count full weights as well. Vulkan on a phone shares system RAM, so a full-file count is the low-memory-killer check. MoE models that `GgufMetadata::streamable_moe()` accepts skip this dense estimate and use the existing `plan_moe_spawn` resident path (resident weights, expert cache, KV).

Context steps, in order, each tried with the current quant:

1. Requested context, KV dtype F16.
2. Same context, KV dtype Q8_0. The supervisor already emits `--cache-type-k`. Also emit `--cache-type-v q8_0` when the dtype is Q8_0.
3. Walk context downward through the subset of `[4096, 2048, 1024, 512]` that is strictly below the requested context. At each size, try F16 KV and then Q8_0 KV.

If the dense file still does not fit, repeat the same context and KV walk for sibling quants already in the pantry. A sibling is another GGUF whose `general.architecture` equals this file's architecture and whose `general.name` equals this file's name. When `general.name` is missing, the stem is the filename with a trailing quant token removed. Quant tokens are the substrings `Q2`, `Q3`, `Q4`, `Q5`, `Q6`, `Q8`, `F16`, `F32`, and `IQ`, matched case-insensitively at a component boundary (`-`, `_`, or `.`). Siblings are tried from largest weight-byte size to smallest.

A step fits when the estimate is less than or equal to the candidate budget:

```text
budget_mb = probe.max_allowed_memory_bytes_pct(max_ram_usage_percent) / (1024 * 1024)
```

Remote candidates use advertised free RAM and total RAM via `SystemProfile::from_advertised`. The peer re-checks on load with its own probe, as it does today, and may reject. A rejection surfaces as a failed escalation, then the next interactive plan is tried once. If that also rejects, the request follows the section 10.3 stay-local rule.

Plans that fit and predict at least `interactive_floor_tok_s` (default 3.0) are interactive. Plans that fit and predict less are batch-only. Chat escalation ignores batch-only plans. The Models tab may still show them with the label `batch`.

`predict_local_tok_s` scales the backend prior by weight size. `weights_gib` is `weights_mb / 1024`:

```text
prior_tok_s * (1.0 / max(weights_gib, 0.25))
```

The existing backend priors stay (Vulkan 28, ArmCpuDotProd 12, X86Baseline 4, GenericCpu 6). A measured bench sample for that model id, node, backend, and context still replaces the prior and does not get the size scale applied a second time. Quant and thermal multipliers stay as they are.

This size scale is a cold-start guard so a 7B file does not inherit a 1B prior. It is not a benchmark claim.

## 12. Ranking

Eligible burst candidates:

- state probe or beacon says the node is ready
- thermal index at or below 75
- battery rule in section 9 leaves them eligible
- the ladder produced an interactive plan

Score is `predicted_tok_s` after the battery throttle, then capped by `network_bound_tok_s` when the plan is a layer split. Phone-local generation (the model runs on that phone) is not layer-split, so the network cap is not applied to its generation rate.

Tie-break, after scores exist. The local orchestrator is not in this list. Section 10 decides whether the prompt stays local before rank runs.

- Let `best_wifi` be the highest Wi-Fi score, or 0 if there is no Wi-Fi plan.
- Let `usb_band` be every USB plan whose score is at least `0.85 * best_wifi`.
- If `usb_band` is non-empty, the winner is the highest score inside it. Two cabled phones are ordered by score. The band only lifts USB plans above Wi-Fi plans.
- If `usb_band` is empty, the winner is the highest score among the remaining plans.

Worked examples, using scores after penalties:

| USB score | Wi-Fi score | Winner | Reason |
|---|---|---|---|
| 8 | 8 | USB | Inside the 15% band |
| 7.0 | 8.0 | USB | 7.0 >= 6.8 |
| 6.0 | 8.0 | Wi-Fi | 6.0 < 6.8 |
| ineligible, thermal 80 | 8 | Wi-Fi | USB not offered |
| ineligible, battery decline | 8 | Wi-Fi | USB not offered |
| 4 after 0.5 throttle | 8 | Wi-Fi | Throttled score falls outside the band |

No extra bonus is added for a product name or for "this is the phone that was plugged in first".

## 13. Pantry pull

If the chosen digest is already in the target peer's catalog (`GET /nexus/control/v1/models`), load it by filename. If it is not, and the local pantry has the digest, ask the peer to fetch it:

- `POST /nexus/control/v1/blob/fetch` on that peer's current control endpoint
- the source URL is `blob_url` of the local control plane for that digest

For a USB peer the control endpoint is `http://127.0.0.1:<host_control>`, so the phone reaches back to the Mac through the ADB forward the Mac already holds. The phone's Wi-Fi IP is not required for that pull. For a Wi-Fi peer the endpoint stays `http://<beacon-ip>:<control_port>`.

The fetch uses the existing blob handler. This spec does not add a second transfer protocol. The load request is sent after the catalog on the peer lists the digest, or after the fetch response reports the bytes written. A fetch error fails that peer's plan and tries the next interactive plan once, then falls back as in section 10.3.

## 14. What the operator sees

TUI status line after a reply, one line, status role so it stays out of the model context:

```text
macbook · general-1.7b · local · 6 tok/s
```

```text
phone-a · coder-7b · USB · 18 tok/s · escalated, route=coder
```

```text
phone-b · coder-7b · Wi-Fi · 16 tok/s · escalated, route=coder
```

Names are `PeerNode::label()` values. Link words are `local`, `USB`, and `Wi-Fi`.

The gateway adds a response header on chat completions:

```text
X-Nexus-Route: <label>; link=<local|usb|wifi>; model=<name>; route=<tag>
```

The Tunnel tab lists every `adb devices -l` row: serial, authorized or not, ADB `model:` as a subtitle, and either the Nexus node label or `Nexus not answering`. The subtitle is not copied into the peer registry's display name.

`nexus doctor` adds an appliance section: local orchestrator file or `none`, interactive floor, each USB serial with its node label or the not-answering state, and each Wi-Fi peer label. It does not classify devices by product.

The Models node picker keeps showing fit (`Fits`, `RPC`, `OOM`) and adds `batch` when the only passing plan is below the floor.

## 15. Config

New keys, with defaults:

```toml
[inference.appliance]
enabled = true
# Empty: pick by the rules in section 10.2. Operator may set a filename or path.
orchestrator = ""
interactive_floor_tok_s = 3.0
burst_idle_secs = 600

[cluster.adb]
# prefer_adb_tunnel remains the master switch for building USB links.
host_port_base = 18080
max_devices = 8
```

Existing keys reused unchanged:

- `cluster.prefer_adb_tunnel`
- `hardware.safety.battery_floor_percent`
- `hardware.safety.battery_action`
- `hardware.safety.max_ram_usage_percent`
- `network.gateway_enabled`
- `network.gateway_port`

`inference.appliance.orchestrator` accepts an empty string, a file name inside `models_dir`, or an absolute path. Any other form fails config load with the path text in the error.

## 16. Failure behavior

| Condition | Result |
|---|---|
| `adb` missing from `PATH` | No USB peers. Wi-Fi peers and the local model still serve. Doctor says ADB is absent. |
| No authorized device | No USB peers. Tunnel tab shows unauthorized rows when present. |
| One serial's forward fails | That serial is skipped. Other serials are still forwarded. |
| Control probe fails on a serial | Tunnel shows `Nexus not answering`. The serial is not a burst candidate. |
| Two probes return the same UUID | Keep the first serial in sorted order. Doctor warns and names both serials. The second forward is torn down. |
| Wi-Fi beacon timeout while USB is healthy | Peer stays, link remains USB. |
| USB unplug while beacon is fresh | Link flips to Wi-Fi. In-flight request is not migrated. The next request uses the Wi-Fi endpoint. |
| Gateway disabled | Appliance auto-route does not run. Existing client host selection remains. |
| No interactive plan for an escalated tag | Stay on the local model if it is loaded. Otherwise HTTP 503 from the gateway with the tag and `no interactive peer`. |

An in-flight generation that loses its peer ends the stream with a status line `peer lost`. The client may send the prompt again. This spec does not add transparent retry mid-stream.

## 17. Tests

Unit tests, no device and no `adb` binary required. Command output is injected as strings.

- `adb devices -l` fixture with two `device` rows and one `unauthorized` row yields two links, sorted by serial, on distinct host ports. The unauthorized serial is listed and not forwarded.
- Port math: index 0 uses `18080` and `18081`, index 1 uses `18082` and `18083`. A base that equals `gateway_port` is a config error.
- Merge: a USB state body and a beacon with the same UUID produce one `PeerNode` whose API endpoint is `127.0.0.1` on the host API port. Clearing the forward restores the beacon IP.
- Fit: a CPU profile with 4096 MB available rejects a dense fixture whose weights are 4500 MB. The same profile accepts a 900 MB fixture. A 12000 MB profile accepts the 4500 MB fixture. A streamable MoE fixture uses the MoE resident estimate, not the full file size.
- Ladder: a model that misses at 4096 F16 and fits at 2048 Q8 records context 2048 and dtype Q8_0.
- Rank table in section 12, including thermal exclusion and battery `decline`.
- Appliance route: a `general` tag with a local model tagged `general` does not select a faster USB peer. A `coder` tag does, and the USB peer wins when its score is inside the band.
- A regression test scans the appliance and tunnel decision functions for a ban list of product substrings (`nothing`, `s23`, `galaxy`, `sm-s`) so a future shortcut fails CI. The ban list lives in the test, not in the router.

## 18. Acceptance on the current desk

This section is a lab script. The names are not constants and must not appear in source, default config, or fixtures.

The desk has three machines:

- The Mac, running the hub. It is the local node. Its budget is whatever `SystemProfile::probe()` reports on that boot.
- A Nothing Phone 3 (16 GB) in Termux, attached to the Mac by USB, authorized in `adb devices`.
- An S23 Ultra (12 GB) in Termux, on an external display because the panel is broken, joined over Wi-Fi. It is not expected to appear in `adb devices` on the Mac.

Pass conditions:

1. The hub shows three nodes: the Mac, the cabled phone under the name configured on that phone, and the Wi-Fi phone under the name configured on that phone.
2. A prompt that matches the `general` keywords is answered on the Mac. The status line says `local`.
3. A prompt that matches the `coder` keywords is answered on one of the two phones. With both phones idle, cool, and above the battery floor, the status line says `USB`.
4. Draining the cabled phone under the battery floor with `battery_action = "decline"`, or raising its thermal index above 75, moves the next coder prompt to the Wi-Fi phone. The status line says `Wi-Fi`.
5. Unplugging the cable leaves the Wi-Fi phone usable. Plugging in a different authorized Android, with Nexus running in Termux, adds a USB peer under that phone's own node name without a code change.
6. `rg -i 'nothing phone|sm-s918|galaxy s23' src tests` returns no matches introduced for this work. Documentation of this lab script is the exception, and it lives only in this file.

## 19. Code map

| Area | Change |
|---|---|
| `src/tunnel.rs` | Multi-serial forward, per-serial remove, injected command output for tests. |
| `src/config.rs` | `[inference.appliance]` and `[cluster.adb]`. |
| `src/sysinfo.rs` | Local battery and power-source probe. |
| `src/control_plane.rs` | Optional `battery_percent` and `power_source` on state. |
| `src/discovery.rs`, `src/mdns.rs` | Merge fields, TXT `bat` and `pwr`. Beacon codec untouched. |
| `src/cluster/memory.rs` | Full-weight dense estimate. MoE path untouched. |
| `src/cluster/rank.rs` | Size-scaled prior, interactive floor, USB/Wi-Fi band, battery and thermal gates. |
| `src/gateway.rs` | Section 10 route, `X-Nexus-Route`, 503 when no interactive peer exists. |
| `src/main.rs` | Gateway URL stays the hub client when the appliance and gateway are on. |
| `src/doctor.rs`, Tunnel tab, chat status | Section 14. |
| `~/.nexus/adb-links.json` | Serial cache from section 7.3. |

## 20. Locked arithmetic

- The 15% band is `score >= 0.85 * best_wifi`. A USB peer that is exactly 15% slower still wins. A USB peer any slower than that loses. Several USB peers inside the band are ordered by score.
- Beacon v1 is unchanged. A Wi-Fi peer's battery stays unknown, and therefore eligible, until `POST /nexus/control/v1/state` returns.
- Full-weight accounting applies to dense GGUFs on every backend, including phone Vulkan, because Adreno uses system RAM.
- The lab phones in section 18 are an acceptance script. Section 6 and the test ban list are what keep them out of the program.
