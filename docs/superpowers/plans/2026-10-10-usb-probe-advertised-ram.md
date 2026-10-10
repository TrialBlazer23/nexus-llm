# USB Probe Advertised RAM and Backend Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** A USB state probe stores the same raw RAM and acceleration backend the beacon already stores, so the fit ladder applies `max_ram_usage_percent` once and a Vulkan phone that has never beaconed loads with `gpu_layers` 99.

**Architecture:** `POST /nexus/control/v1/state` gains three optional fields taken from the `SystemProfile` the handler already probes: raw `MemAvailable`, raw `MemTotal`, and the bench backend key. `apply_probe_hit` writes those into `PeerNode.free_ram_mb`, `total_ram_mb`, and `backend`. `plan_fit_ladder`, `SystemProfile::from_advertised`, and `peer_offload_layers` stay as they are. An older state body that omits the fields does not overwrite a positive beacon free-RAM value or an existing backend. A brand-new peer with those fields missing gets `free_ram_mb = allocatable_memory_mb * 100 / hub max_ram_usage_percent` and `AccelerationBackend::GenericCpu`.

**Tech Stack:** Rust, serde_json, existing `SystemProfile` and `bench::backend_key`. No new crates. Cargo runs on this Linux VM from `/root/nexus-llm` (no WSL wrapper).

## Global Constraints

- Do not hardcode phone models, RAM sizes, DeX, or product names. Peer identity stays the node UUID. An ADB serial is only a cable id.
- Do not add `vulkan` (or any backend name) to the capabilities list. Capabilities stay feature flags (`inference`, `host`, `catalog`, `moe_stream`, and the rest).
- Do not change beacon v1 (64 bytes, CRC-16). Do not change `plan_fit_ladder`, `SystemProfile::from_advertised`, or `peer_offload_layers`.
- `free_ram_mb` means raw `MemAvailable` in MB, whether the writer was a beacon or a USB probe. `total_ram_mb` means raw `MemTotal`. `allocatable_memory_mb` stays the phone's own budget and is not copied into `free_ram_mb`.
- The ladder budget stays `from_advertised(free, total).max_allowed_memory_bytes_pct(hub max_ram_usage_percent)`. That percent is applied once.
- Penryn rustflags stay `-C target-feature=-avx,-avx2,-fma,-sse4.2`. Android LMK stays model + KV under 75% of `MemAvailable` on the phone that actually loads. Sequential layer split only.
- No new crate, no beacon version bump, no internet download, no networked MoE.
- Product-name ban: do not put `nothing`, `s23`, `galaxy`, or `sm-s` in `src/discovery.rs` or the other scanned files. Do not edit the existing `galaxy-s23` fixture.
- Do not push. Do not open a PR unless the operator asks.
- Out of scope: thermal index on the state body, the `EndpointConflict` registry address, `adb` disappearing mid-probe, and any second fix to rank or GPU policy.

## File structure

- Modify `src/control_plane.rs`. `ControlPlaneState` gains the three optional fields. `build_control_plane_state_with_host` copies them from a `&SystemProfile`.
- Modify `src/control_plane_server.rs`. The state handler passes the profile it already probed.
- Modify `src/discovery.rs`. Probe merge reads the new fields and the hub RAM percent. Helpers stay private in this file.
- Modify `tests/test_phase3_network.rs` and `tests/test_phase12_multi_instance.rs` only so struct literals and the builder call keep compiling.
- Modify `docs/superpowers/specs/2026-10-10-edge-appliance-design.md` sections 6 and 7.3 so the locked contract matches the wire.
- Modify `AGENT_LEARNINGS.md` with one entry.
- Do not split `discovery.rs` or `gateway.rs` in this change.

## Worked numbers

Hub percent 75. Phone `MemAvailable` 12000 MB, `MemTotal` 16000 MB, so `allocatable_memory_mb` is 9000. Ladder budget must be `12000 * 75 / 100 = 9000`. Storing 9000 in `free_ram_mb` and applying 75% again yields 6750, which is 56.25% of 12000. That is the bug.

| Case | State fields | Previous free / backend | Hub % | `free_ram_mb` | `total_ram_mb` | `backend` | Budget |
|---|---|---|---|---|---|---|---|
| New peer, advertised | available 12000, total 16000, backend `vulkan`, allocatable 9000 | none | 75 | 12000 | 16000 | Vulkan | 9000 |
| New peer, old body | all three absent, allocatable 9000 | none | 75 | 12000 | 12000 | GenericCpu | 9000 |
| New peer, old body | all three absent, allocatable 9000 | none | 60 | 15000 | 15000 | GenericCpu | 9000 |
| Beacon, then old probe | all three absent, allocatable 9000 | 12000 / Vulkan, total 16000 | 75 | 12000 | 16000 | Vulkan | 9000 |
| Refresh | available 11000, total 16000, backend `vulkan` | 12000 / Vulkan | 75 | 11000 | 16000 | Vulkan | 8250 |
| Bad token on existing peer | available 11000, total 16000, backend `nope` | 12000 / Vulkan | 75 | 11000 | 16000 | Vulkan | 8250 |
| Phone reports CPU | available 11000, total 16000, backend `cpu` | 12000 / Vulkan | 75 | 11000 | 16000 | GenericCpu | 8250 |

`available_ram_mb` or `total_ram_mb` of `Some(0)` is absent. An empty or unrecognized backend string is absent. Absent on an existing peer keeps the previous value. Absent free RAM on a peer whose `free_ram_mb` is already 0 uses the inverse. `total_ram_mb` falls back to the previous total when that is positive, otherwise to the free value just chosen. It is never set from `allocatable_memory_mb`.

`parse_backend_key` already accepts `vulkan`, `arm-dotprod`, `arm`, `x86-sse41`, `x86`, `x86-baseline`, `cpu`, `generic`, and `generic-cpu`, case-insensitive. Also accept the serde enum names `Vulkan`, `ArmCpuDotProd`, `X86Baseline`, and `GenericCpu`. The phone sends `bench::backend_key`, which is `vulkan`, `arm-dotprod`, `x86-sse41`, or `cpu`.

---

### Task 1: Advertise raw RAM and backend on control-plane state

**Files:**
- Modify: `src/control_plane.rs` (`ControlPlaneState` around line 29, `build_control_plane_state` around line 144, `build_control_plane_state_with_host` around line 168, test `old_state_json_defaults_battery` around line 1597)
- Modify: `src/control_plane_server.rs` state handler around line 612
- Modify: `src/discovery.rs` struct literals around lines 2127 and 2254
- Modify: `tests/test_phase3_network.rs` around line 99
- Modify: `tests/test_phase12_multi_instance.rs` around line 38
- Test: `src/control_plane.rs` module `tests`

**Interfaces:**
- Consumes: `SystemProfile` (`total_ram_mb`, `available_ram_mb`, `detected_backend`) and `bench::backend_key(AccelerationBackend) -> &'static str`.
- Produces: `ControlPlaneState.available_ram_mb: Option<u64>`, `ControlPlaneState.total_ram_mb: Option<u64>`, `ControlPlaneState.backend: Option<String>`. Missing JSON keys deserialize as `None`. `build_control_plane_state_with_host(..., profile: &SystemProfile)` sets `Some(profile.available_ram_mb)`, `Some(profile.total_ram_mb)`, and `Some(backend_key(profile.detected_backend).to_string())`. `allocatable_memory_mb` is still the caller's argument.

- [ ] **Step 1: Write the failing tests**

Extend `old_state_json_defaults_battery` with:

```rust
assert_eq!(state.available_ram_mb, None);
assert_eq!(state.total_ram_mb, None);
assert_eq!(state.backend, None);
```

Add this test in the same `mod tests`:

```rust
#[tokio::test]
async fn build_state_advertises_probed_ram_and_backend() {
    use crate::control_plane::build_control_plane_state_with_host;
    use crate::discovery::NodeRole;
    use crate::supervisor::SupervisorManager;
    use crate::sysinfo::{AccelerationBackend, SystemProfile};
    use uuid::Uuid;

    let profile = SystemProfile {
        total_ram_mb: 16_000,
        available_ram_mb: 12_000,
        detected_backend: AccelerationBackend::Vulkan,
        recommended_threads: 4,
    };
    let state = build_control_plane_state_with_host(
        Uuid::from_u128(1),
        NodeRole::HOST,
        vec!["inference".into()],
        &SupervisorManager::new(),
        9_000,
        false,
        None,
        "127.0.0.1",
        &profile,
    )
    .await;

    assert_eq!(state.allocatable_memory_mb, 9_000);
    assert_eq!(state.available_ram_mb, Some(12_000));
    assert_eq!(state.total_ram_mb, Some(16_000));
    assert_eq!(state.backend.as_deref(), Some("vulkan"));

    let raw = serde_json::to_string(&state).expect("serialize");
    let parsed: crate::control_plane::ControlPlaneState =
        serde_json::from_str(&raw).expect("round trip");
    assert_eq!(parsed.available_ram_mb, Some(12_000));
    assert_eq!(parsed.total_ram_mb, Some(16_000));
    assert_eq!(parsed.backend.as_deref(), Some("vulkan"));
}
```

- [ ] **Step 2: Run the tests and confirm they fail**

Run:

```bash
cargo test --locked --lib control_plane::tests:: -- --test-threads=8
```

Expected: compile error. `available_ram_mb` is not a field, and `build_control_plane_state_with_host` does not take `&SystemProfile`.

- [ ] **Step 3: Add the fields and fill them from the probed profile**

On `ControlPlaneState`, after `display_name`:

```rust
/// Raw MemAvailable in MB. Same unit as beacon `free_ram_mb`. Older peers omit it.
#[serde(default, skip_serializing_if = "Option::is_none")]
pub available_ram_mb: Option<u64>,
/// Raw MemTotal in MB. Same unit as beacon `total_ram_mb`. Older peers omit it.
#[serde(default, skip_serializing_if = "Option::is_none")]
pub total_ram_mb: Option<u64>,
/// Bench backend key: `vulkan`, `arm-dotprod`, `x86-sse41`, or `cpu`. Older peers omit it.
#[serde(default, skip_serializing_if = "Option::is_none")]
pub backend: Option<String>,
```

Import `SystemProfile` next to the existing `sysinfo` import in `src/control_plane.rs`.

Change `build_control_plane_state` so it probes once and forwards that profile:

```rust
pub async fn build_control_plane_state(
    node_id: Uuid,
    role: NodeRole,
    capabilities: Vec<String>,
    manager: &crate::supervisor::SupervisorManager,
    allocatable_memory_mb: u64,
    rpc_ready: bool,
    signing_public_key: Option<String>,
) -> ControlPlaneState {
    let profile = SystemProfile::probe();
    build_control_plane_state_with_host(
        node_id,
        role,
        capabilities,
        manager,
        allocatable_memory_mb,
        rpc_ready,
        signing_public_key,
        "127.0.0.1",
        &profile,
    )
    .await
}
```

Add `profile: &SystemProfile` as the last parameter of `build_control_plane_state_with_host`. In the struct literal it already builds, set:

```rust
available_ram_mb: Some(profile.available_ram_mb),
total_ram_mb: Some(profile.total_ram_mb),
backend: Some(crate::bench::backend_key(profile.detected_backend).to_string()),
```

Leave `allocatable_memory_mb` as the function argument. Do not recompute it inside the builder.

In `src/control_plane_server.rs` `handle_state` (the function that already does `let profile = SystemProfile::probe()`), pass `&profile` as the new last argument. Do not probe a second time.

Update every other `ControlPlaneState { ... }` literal and the `build_control_plane_state_with_host` call in `tests/test_phase12_multi_instance.rs`. Rust struct literals are exhaustive.

- `src/discovery.rs` `state_verify_sets_peer_battery_and_beacon_keeps_it` and `probe_state`: set `available_ram_mb: None`, `total_ram_mb: None`, `backend: None`. Those fixtures are older bodies.
- `tests/test_phase3_network.rs` `control_plane_validates_identity_protocol_and_policy`: the same three `None` fields.
- `tests/test_phase12_multi_instance.rs`: pass this profile as the last argument:

```rust
&SystemProfile {
    total_ram_mb: 4_096,
    available_ram_mb: 2_048,
    detected_backend: AccelerationBackend::GenericCpu,
    recommended_threads: 2,
}
```

Add the `SystemProfile` and `AccelerationBackend` imports that file needs. Do not change what that test asserts.

`validate_state` does not grow a check on the new fields. `available_ram_mb` may be larger than `allocatable_memory_mb`.

- [ ] **Step 4: Run the tests and confirm they pass**

Run:

```bash
cargo test --locked --lib control_plane::tests:: -- --test-threads=8
cargo test --locked --test control_plane_validates_identity_protocol_and_policy --test control_plane_state_advertises_loaded_models --test build_state_includes_signing_public_key -- --test-threads=8
```

Expected: the lib tests pass, including `old_state_json_defaults_battery` and `build_state_advertises_probed_ram_and_backend`. The three integration filters pass. `cargo test --locked --lib` is not required until Task 2; Task 1 must still leave the crate compiling. If a literal was missed, the compiler names the file. Fix that literal with the three `None` fields and rerun.

- [ ] **Step 5: Commit**

```bash
git add src/control_plane.rs src/control_plane_server.rs src/discovery.rs tests/test_phase3_network.rs tests/test_phase12_multi_instance.rs
git commit -m "$(cat <<'EOF'
fix(control): advertise raw RAM and backend on state

EOF
)"
```

---

### Task 2: Store beacon units when the USB probe merges a peer

**Files:**
- Modify: `src/discovery.rs` (`apply_probe_hit` around line 585, `fold_usb_probes` around line 664, `apply_folded_usb_probes` around line 940, tests from `probe_state` around line 2254)
- Modify: `docs/superpowers/specs/2026-10-10-edge-appliance-design.md` sections 6 and 7.3
- Modify: `AGENT_LEARNINGS.md`
- Test: `src/discovery.rs` module `tests`

**Interfaces:**
- Consumes: Task 1 fields `available_ram_mb`, `total_ram_mb`, and `backend`. `NexusConfig.hardware.safety.max_ram_usage_percent` (default 75, range clamped to 1..=100 at use). `bench::parse_backend_key`.
- Produces: `fold_usb_probes(..., hub_ram_percent: u8) -> UsbFold`. `apply_probe_hit(peers, hit, hub_ram_percent)`. `apply_folded_usb_probes` reads the live config percent and passes it. No new public type.

- [ ] **Step 1: Thread the percent through, still using the old assignment, and write the failing tests**

Add `hub_ram_percent: u8` as the last parameter of `fold_usb_probes` and of `apply_probe_hit`. Pass it from `fold_usb_probes` into `apply_probe_hit`. Do not read it yet. `apply_probe_hit` still assigns `mb_u32(hit.state.allocatable_memory_mb)` to `free_ram_mb` and still uses `backend_hint` only.

In `apply_folded_usb_probes`, pass `self.cfg().hardware.safety.max_ram_usage_percent`.

`apply_probe_hit` does not read `hub_ram_percent` yet. Bind it with `let _ = hub_ram_percent;` so an unused-variable deny still compiles. Step 3 removes that line.

Update the two existing `fold_usb_probes(...)` calls in `two_serial_probe_keeps_first_uuid_and_leaves_wifi_addr` to pass `75`. That test's shared peer starts at `free_ram_mb == 0`, so after Step 3 an old body inverts 1500 MB at 75% to 2000. The test does not assert RAM. Leave it that way.

Add this helper and these tests inside `mod tests` in `src/discovery.rs`. `probe_state` stays an older body (`available_ram_mb: None`, `total_ram_mb: None`, `backend: None`, `allocatable_memory_mb: 1500`).

```rust
fn budget_mb(peer: &PeerNode, percent: u8) -> u64 {
    use crate::sysinfo::SystemProfile;
    SystemProfile::from_advertised(
        u64::from(peer.free_ram_mb),
        u64::from(peer.total_ram_mb),
        peer.backend,
    )
    .max_allowed_memory_bytes_pct(percent)
        / (1024 * 1024)
}

fn advertised_state(id: Uuid) -> crate::control_plane::ControlPlaneState {
    let mut state = probe_state(id, "phone");
    state.allocatable_memory_mb = 9_000;
    state.available_ram_mb = Some(12_000);
    state.total_ram_mb = Some(16_000);
    state.backend = Some("vulkan".into());
    state.capabilities = vec!["inference".into()];
    state
}

#[test]
fn usb_probe_stores_raw_ram_and_vulkan_backend() {
    use std::collections::HashMap;
    let id = Uuid::from_u128(11);
    let mut peers = HashMap::new();
    apply_probe_hit(&mut peers, &hit("serial-a", 18080, advertised_state(id)), 75);
    let peer = &peers[&id];
    assert_eq!(peer.free_ram_mb, 12_000);
    assert_eq!(peer.total_ram_mb, 16_000);
    assert_eq!(peer.backend, AccelerationBackend::Vulkan);
    assert!(peer.status.is_vulkan_active());
    assert_eq!(peer.link_class, LinkClass::Usb);
    assert_eq!(peer.control_endpoint(), "http://127.0.0.1:18081");
    // 75% of raw 12000. Storing allocatable 9000 would budget 6750.
    assert_eq!(budget_mb(peer, 75), 9_000);
}

#[test]
fn usb_probe_accepts_pascal_case_backend_and_refreshes_ram() {
    use std::collections::HashMap;
    let id = Uuid::from_u128(11);
    let mut peers = HashMap::new();
    apply_probe_hit(&mut peers, &hit("serial-a", 18080, advertised_state(id)), 75);
    let mut refreshed = advertised_state(id);
    refreshed.available_ram_mb = Some(11_000);
    refreshed.backend = Some("ArmCpuDotProd".into());
    apply_probe_hit(&mut peers, &hit("serial-a", 18080, refreshed), 75);
    let peer = &peers[&id];
    assert_eq!(peer.free_ram_mb, 11_000);
    assert_eq!(peer.total_ram_mb, 16_000);
    assert_eq!(peer.backend, AccelerationBackend::ArmCpuDotProd);
    assert!(!peer.status.is_vulkan_active());
    assert_eq!(budget_mb(peer, 75), 11_000 * 75 / 100);
}

#[test]
fn old_usb_state_does_not_clobber_beacon_ram_or_backend() {
    use std::collections::HashMap;
    let id = Uuid::from_u128(7);
    let mut beaconed = usb_fixture();
    beaconed.uuid = id;
    beaconed.free_ram_mb = 12_000;
    beaconed.total_ram_mb = 16_000;
    beaconed.backend = AccelerationBackend::Vulkan;
    beaconed.status.0 |= StatusFlags::VULKAN_ACTIVE.0;
    let mut peers = HashMap::new();
    peers.insert(id, beaconed);
    let mut old = probe_state(id, "phone");
    old.allocatable_memory_mb = 9_000;
    old.backend = Some("nope".into());
    apply_probe_hit(&mut peers, &hit("serial-a", 18080, old), 75);
    let peer = &peers[&id];
    assert_eq!(peer.free_ram_mb, 12_000);
    assert_eq!(peer.total_ram_mb, 16_000);
    assert_eq!(peer.backend, AccelerationBackend::Vulkan);
    assert!(peer.status.is_vulkan_active());
    assert_eq!(budget_mb(peer, 75), 9_000);

    let mut refresh = probe_state(id, "phone");
    refresh.available_ram_mb = Some(11_000);
    refresh.total_ram_mb = Some(16_000);
    refresh.backend = Some("nope".into());
    apply_probe_hit(&mut peers, &hit("serial-a", 18080, refresh), 75);
    let peer = &peers[&id];
    assert_eq!(peer.free_ram_mb, 11_000);
    assert_eq!(peer.total_ram_mb, 16_000);
    assert_eq!(peer.backend, AccelerationBackend::Vulkan);
    assert_eq!(budget_mb(peer, 75), 11_000 * 75 / 100);
}

#[test]
fn old_usb_state_can_replace_backend_when_the_token_parses() {
    use std::collections::HashMap;
    let id = Uuid::from_u128(7);
    let mut beaconed = usb_fixture();
    beaconed.uuid = id;
    beaconed.free_ram_mb = 12_000;
    beaconed.total_ram_mb = 16_000;
    beaconed.backend = AccelerationBackend::Vulkan;
    beaconed.status.0 |= StatusFlags::VULKAN_ACTIVE.0;
    let mut peers = HashMap::new();
    peers.insert(id, beaconed);
    let mut state = probe_state(id, "phone");
    state.available_ram_mb = Some(11_000);
    state.total_ram_mb = Some(16_000);
    state.backend = Some("cpu".into());
    apply_probe_hit(&mut peers, &hit("serial-a", 18080, state), 75);
    let peer = &peers[&id];
    assert_eq!(peer.free_ram_mb, 11_000);
    assert_eq!(peer.backend, AccelerationBackend::GenericCpu);
    assert!(!peer.status.is_vulkan_active());
}

#[test]
fn new_peer_from_old_state_inverts_allocatable_once() {
    use std::collections::HashMap;
    let id = Uuid::from_u128(11);
    let mut peers = HashMap::new();
    let mut old = probe_state(id, "phone");
    old.allocatable_memory_mb = 9_000;
    apply_probe_hit(&mut peers, &hit("serial-a", 18080, old.clone()), 75);
    let peer = &peers[&id];
    assert_eq!(peer.free_ram_mb, 12_000);
    assert_eq!(peer.total_ram_mb, 12_000);
    assert_eq!(peer.backend, AccelerationBackend::GenericCpu);
    assert!(!peer.status.is_vulkan_active());
    assert_eq!(budget_mb(peer, 75), 9_000);

    let mut peers = HashMap::new();
    apply_probe_hit(&mut peers, &hit("serial-a", 18080, old), 60);
    let peer = &peers[&id];
    assert_eq!(peer.free_ram_mb, 15_000);
    assert_eq!(budget_mb(peer, 60), 9_000);
}

#[tokio::test]
async fn folded_probe_uses_config_ram_percent() {
    use crate::adb_links::AdbLinkCache;
    let discovery = DiscoveryService::new(NexusConfig::default(), Some(Uuid::from_u128(1)));
    {
        let cfg = discovery.shared_config();
        cfg.write().expect("config").hardware.safety.max_ram_usage_percent = 60;
    }
    let id = Uuid::from_u128(11);
    let mut cache = AdbLinkCache::default();
    let mut old = probe_state(id, "phone");
    old.allocatable_memory_mb = 9_000;
    let hits = vec![hit("serial-a", 18080, old)];
    discovery
        .apply_folded_usb_probes(&mut cache, &hits, &["serial-a".into()], &[])
        .await;
    let peer = discovery.peers().read().await.get(&id).cloned().expect("peer");
    assert_eq!(peer.free_ram_mb, 15_000);
    assert_eq!(peer.backend, AccelerationBackend::GenericCpu);
    assert_eq!(budget_mb(&peer, 60), 9_000);
}
```

`peer_offload_layers` is private to `gateway.rs` and already maps `AccelerationBackend::Vulkan` to 99 and every other backend to 0. Do not make it public. `usb_probe_stores_raw_ram_and_vulkan_backend` locking `backend == Vulkan` is the probe-side half of that contract.

- [ ] **Step 2: Run the new tests and confirm they fail**

Run:

```bash
cargo test --locked --lib discovery::tests::usb_probe_ -- --test-threads=8
cargo test --locked --lib discovery::tests::old_usb_state_ -- --test-threads=8
cargo test --locked --lib discovery::tests::new_peer_from_old_state_inverts_allocatable_once -- --test-threads=8
cargo test --locked --lib discovery::tests::folded_probe_uses_config_ram_percent -- --test-threads=8
```

Expected: compile succeeds, assertions fail. The new advertised peer has `free_ram_mb == 9000` and `backend == GenericCpu`. The old-body new peer has `free_ram_mb == 9000`, not 12000 or 15000. The beacon-clobber test fails because free RAM became 9000. `two_serial_probe_keeps_first_uuid_and_leaves_wifi_addr` still passes.

- [ ] **Step 3: Merge advertised RAM and backend**

Add these private helpers next to `backend_hint` in `src/discovery.rs`:

```rust
fn parse_state_backend(raw: &str) -> Option<AccelerationBackend> {
    let key = raw.trim();
    if key.is_empty() {
        return None;
    }
    if let Some(backend) = crate::bench::parse_backend_key(key) {
        return Some(backend);
    }
    match key {
        "Vulkan" => Some(AccelerationBackend::Vulkan),
        "ArmCpuDotProd" => Some(AccelerationBackend::ArmCpuDotProd),
        "X86Baseline" => Some(AccelerationBackend::X86Baseline),
        "GenericCpu" => Some(AccelerationBackend::GenericCpu),
        _ => None,
    }
}

fn free_ram_from_allocatable(allocatable_mb: u64, hub_percent: u8) -> u32 {
    let pct = u32::from(hub_percent.clamp(1, 100));
    let free = u128::from(allocatable_mb).saturating_mul(100) / u128::from(pct);
    mb_u32(u64::try_from(free).unwrap_or(u64::MAX))
}

fn advertised_free_ram(
    state: &crate::control_plane::ControlPlaneState,
    previous_free: u32,
    hub_percent: u8,
) -> u32 {
    match state.available_ram_mb {
        Some(mb) if mb > 0 => mb_u32(mb),
        _ if previous_free > 0 => previous_free,
        _ => free_ram_from_allocatable(state.allocatable_memory_mb, hub_percent),
    }
}

fn advertised_total_ram(
    state: &crate::control_plane::ControlPlaneState,
    previous_total: u32,
    free: u32,
) -> u32 {
    let total = match state.total_ram_mb {
        Some(mb) if mb > 0 => mb_u32(mb),
        _ if previous_total > 0 => previous_total,
        _ => free,
    };
    total.max(free)
}

fn advertised_backend(
    state: &crate::control_plane::ControlPlaneState,
) -> Option<AccelerationBackend> {
    state.backend.as_deref().and_then(parse_state_backend)
}

fn set_vulkan_flag(status: &mut StatusFlags, backend: AccelerationBackend) {
    if backend == AccelerationBackend::Vulkan {
        status.0 |= StatusFlags::VULKAN_ACTIVE.0;
    } else {
        status.0 &= !StatusFlags::VULKAN_ACTIVE.0;
    }
}
```

In the existing-peer branch of `apply_probe_hit`, delete the block that sets `free_ram_mb` and `total_ram_mb` from `allocatable_memory_mb`, and delete the `backend_hint` assignment. After `peer.status = status_from_state(...)`, write:

```rust
let free = advertised_free_ram(&hit.state, peer.free_ram_mb, hub_ram_percent);
peer.free_ram_mb = free;
peer.total_ram_mb = advertised_total_ram(&hit.state, peer.total_ram_mb, free);
if let Some(backend) = advertised_backend(&hit.state) {
    peer.backend = backend;
    set_vulkan_flag(&mut peer.status, backend);
}
```

Keep display name, moe, battery, power, role, active model, and `apply_usb_probe` as they are. `hub_ram_percent` is now used. `backend_hint` stays for any other caller; do not route the probe through it.

In the new-peer branch, replace the RAM and backend fields with:

```rust
let free = advertised_free_ram(&hit.state, 0, hub_ram_percent);
let total = advertised_total_ram(&hit.state, 0, free);
let backend = advertised_backend(&hit.state).unwrap_or(AccelerationBackend::GenericCpu);
let mut status = status_from_state(&hit.state, StatusFlags(0));
set_vulkan_flag(&mut status, backend);
```

Use `free`, `total`, `backend`, and `status` in the `PeerNode` literal (`total_ram_mb: total`, `free_ram_mb: free`). Do not pass `allocatable_memory_mb` into either RAM field.

- [ ] **Step 4: Run the tests and confirm they pass**

Run:

```bash
cargo test --locked --lib discovery::tests:: -- --test-threads=8
```

Expected: the new tests pass, and `two_serial_probe_keeps_first_uuid_and_leaves_wifi_addr` still passes. Then:

```bash
cargo test --locked -- --test-threads=8
```

Expected: the full suite passes. If the linker aborts with `SmallVector unable to grow` or `Pointer tag ... truncated`, run that same command once more before treating it as a test failure.

New code must not add clippy warnings. A full `cargo clippy --locked --all-targets -- -D warnings` may still fail on the pre-existing `pantry_source_base`, `score_burst_candidate`, and `fit_badge` lints. Do not edit those to make clippy quiet.

- [ ] **Step 5: Update the spec and agent learnings**

In `docs/superpowers/specs/2026-10-10-edge-appliance-design.md`, after the section 6 bullets that list `total_ram_mb`, `free_ram_mb`, `allocatable_memory_mb`, and `AccelerationBackend`, add:

```markdown
`free_ram_mb` and `total_ram_mb` are raw `MemAvailable` and `MemTotal`, the same numbers beacon bytes `0x20` and `0x1C` carry. `allocatable_memory_mb` is that peer's own budget (`MemAvailable * memory_budget_percent / 100`). Fit applies the hub `max_ram_usage_percent` once to raw free RAM through `SystemProfile::from_advertised`. A USB probe does not store `allocatable_memory_mb` in `free_ram_mb`. The backend is the peer's probed `AccelerationBackend`, not a capability string.
```

In section 7.3, after the sentence that lists what a successful body supplies, add:

```markdown
The body also supplies optional `available_ram_mb`, `total_ram_mb`, and `backend` (`vulkan`, `arm-dotprod`, `x86-sse41`, or `cpu`). Missing keys are an older peer. The hub leaves a positive beacon `free_ram_mb`, a positive `total_ram_mb`, and an existing backend in place when the matching key is missing or the backend token does not parse. A new peer with those keys missing stores `free_ram_mb = allocatable_memory_mb * 100 / hub max_ram_usage_percent` and `AccelerationBackend::GenericCpu`. `Some(0)` for either RAM field is treated as missing.
```

Do not change the `adb-links.json` example. That cache is still serial, node id, and ports.

Prepend this entry to `AGENT_LEARNINGS.md`, above the current first dated entry:

```text
## 2026-10-10 — USB probe RAM and backend use beacon units
- Category: bug
- Context: After the edge-appliance merge, `apply_probe_hit` stored `ControlPlaneState.allocatable_memory_mb` in `PeerNode.free_ram_mb`. The ladder applies `max_ram_usage_percent` to `free_ram_mb`, so a 75% phone was planned at about 56% of MemAvailable, and a later probe overwrote a good beacon value. A peer created only from USB state became GenericCpu unless capabilities contained `vulkan`, so `peer_offload_layers` sent `gpu_layers` 0. Phone capabilities are feature flags and do not include `vulkan`.
- Finding: Beacon `free_ram_mb` is raw MemAvailable and beacon `backend` is the probed AccelerationBackend. `allocatable_memory_mb` is already the phone's own percent of MemAvailable. The state handler already calls `SystemProfile::probe()`.
- Action: State carries optional `available_ram_mb`, `total_ram_mb`, and bench `backend`. The probe writes those into the peer. An older body does not clobber positive beacon RAM or an existing backend. A new peer with the fields missing uses `allocatable * 100 / hub max_ram_usage_percent` and GenericCpu. Do not put the backend into capabilities, do not change the ladder percent, and do not change `peer_offload_layers`.
- Verification: `cargo test --locked -- --test-threads=8` on this tree. No live adb and no device.
```

- [ ] **Step 6: Commit**

```bash
git add src/discovery.rs docs/superpowers/specs/2026-10-10-edge-appliance-design.md AGENT_LEARNINGS.md
git commit -m "$(cat <<'EOF'
fix(discovery): keep USB probe RAM and backend in beacon units

EOF
)"
```

---

## Self-review

Spec coverage:

- Raw RAM on the wire is Task 1. Probe merge, inverse fallback, backend parse, Vulkan flag, and config percent are Task 2.
- Ladder and `peer_offload_layers` are intentionally untouched. Vulkan → 99 is already tested in `gateway.rs` `remote_load_request_carries_ladder_kv_dtype`.
- Older JSON still parses because Task 1 uses `#[serde(default)]` and the existing `old_state_json_defaults_battery` fixture has no new keys.
- Both directions of mixed versions are covered: a new phone talking to an old hub is ignored extra JSON; an old phone talking to a new hub hits the inverse and GenericCpu fallback.
- The lab note is operational, not a code task. The RAM correction works from the hub alone against an old phone. GPU offload on a cable that never beacons needs the phone to send `backend`, so the same binary has to be installed on the hub and on that phone. A Wi-Fi-only phone already receives RAM and backend from its beacon.

Placeholder scan: every test and helper is written out. No deferred edge-case step.

Type consistency: field names `available_ram_mb`, `total_ram_mb`, and `backend` match across Task 1 and Task 2. `fold_usb_probes` and `apply_probe_hit` both take `hub_ram_percent: u8`. `budget_mb` uses `from_advertised` plus `max_allowed_memory_bytes_pct`, which is the ladder's budget formula.

## Execution handoff

Copy this plan to `docs/superpowers/plans/2026-10-10-usb-probe-advertised-ram.md` before the first implementation task. Implement on local `main`. Do not push.

**1. Subagent-Driven (recommended)** — one fresh implementer per task, then spec review. Task 2 depends on Task 1.

**2. Inline Execution** — implement both tasks in this session, with a test checkpoint after each commit.
