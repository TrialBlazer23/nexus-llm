# Edge Appliance Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make the local node the always-on front door, and let any Termux Android join as a USB peer (one forward per ADB serial) or a Wi-Fi peer, with honest dense fit and a 15% USB rank band.

**Architecture:** Pure functions land first: config, battery parsing, full-weight fit, the context/KV ladder, burst ranking, ADB port allocation, and the route decision. `PeerNode` then stores the live link. The gateway calls the route decision and stops treating the first cabled phone as the only host. The spec is `docs/superpowers/specs/2026-10-10-edge-appliance-design.md`. Do not re-decide numbers that the spec locks.

**Tech Stack:** Rust 1.85+ (this repo resolves on 1.99), tokio, hyper, serde, toml, ratatui. No new crates. Cargo from the repository root on Linux. On the Windows host, run the same command through `wsl bash -l -c "cd /mnt/c/nexus-llm && <command>"`.

## Global Constraints

- A node id is the UUID Nexus already persists. An ADB serial identifies a cable. The `model:` token from `adb devices -l` is a Tunnel-tab label only.
- Every authorized ADB device gets its own localhost forward. LAN discovery keeps running.
- Peers merge on UUID. While the USB forward answers, `api_endpoint()` and `control_endpoint()` return `http://127.0.0.1:<host_port>`.
- With `inference.appliance.enabled` and `network.gateway_enabled`, hub startup does not replace the client URL with a USB endpoint. `network.default_host`, when set, remains an explicit pin and still wins.
- Ordinary prompts whose tag the local model carries stay local. The front-door model carries `general` and `chat` plus its preset tags.
- A USB plan wins over Wi-Fi only when its score is `>= 0.85 * best_wifi`. Several USB plans inside that band are ordered by score. A USB plan below the band loses.
- Dense fit is `weights_mb + kv_mb + compute_buffer_mb` against `max_ram_usage_percent` of available RAM. The 10% mmap discount is not used for the verdict. MoE files that `streamable_moe()` accepts keep `plan_moe_spawn`.
- Battery and power source travel on `ControlPlaneState` and mDNS TXT keys `bat` and `pwr`. The 64-byte beacon codec is unchanged.
- Interactive floor default is 3.0 tokens/s. Thermal cutoff stays 75. Battery actions stay `decline`, `throttle` (score × 0.5), and `ignore`. Null battery stays eligible.
- Host ports start at `18080`, stride 2, `max_devices` 8. Forwards use `adb -s <serial>`. `forward --remove` without a serial is forbidden.
- No product name, RAM constant, or serial table is a routing input. The ban test scans only the new decision files listed in Task 9. Do not edit the existing `galaxy-s23` fixture in `tests/test_discovery.rs`.
- Penryn rustflags and the Android 75% ceiling stay as they are. No beacon v2, no networked MoE, no new crates, no internet download inside the ladder.

This is one plan because each task consumes the previous task's types. Do not start Task 5 before Task 3, or Task 8 before Tasks 5 and 7.

---

### Task 1: Appliance and ADB config

**Files:**
- Modify: `src/config.rs` ( `InferenceConfig` near line 359, `ClusterConfig` near line 987, `NexusConfig::validate` near line 175)
- Test: `src/config.rs` module `tests` (create it if the file has none)

**Interfaces:**
- Consumes: existing `NexusConfig`, `ConfigError::Invalid`
- Produces:
  - `ApplianceConfig { enabled: bool, orchestrator: String, interactive_floor_millis: u32, burst_idle_secs: u64 }`
  - `ApplianceConfig::floor_tok_s(&self) -> f32` returns `interactive_floor_millis as f32 / 1000.0`
  - `AdbConfig { host_port_base: u16, max_devices: u8 }`
  - `InferenceConfig.appliance: ApplianceConfig`
  - `ClusterConfig.adb: AdbConfig`
  - `NexusConfig::validate` also checks the rules below

`NexusConfig` is `Eq`. Do not store `f32`. Deserialize TOML `interactive_floor_tok_s` (a float such as `3.0`) into `interactive_floor_millis` (`3.0` → `3000`). Serialize it back as that float. Reject non-finite values, `0`, and values `>= 10000`.

- [ ] **Step 1: Write the failing test**

Add this module at the bottom of `src/config.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn appliance_defaults_parse_from_empty_toml() {
        let cfg: NexusConfig = toml::from_str("").expect("empty toml");
        assert!(cfg.inference.appliance.enabled);
        assert_eq!(cfg.inference.appliance.orchestrator, "");
        assert_eq!(cfg.inference.appliance.interactive_floor_millis, 3000);
        assert!((cfg.inference.appliance.floor_tok_s() - 3.0).abs() < 0.001);
        assert_eq!(cfg.inference.appliance.burst_idle_secs, 600);
        assert_eq!(cfg.cluster.adb.host_port_base, 18080);
        assert_eq!(cfg.cluster.adb.max_devices, 8);
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn floor_toml_float_round_trips() {
        let raw = "\
[inference.appliance]
interactive_floor_tok_s = 3.5
";
        let cfg: NexusConfig = toml::from_str(raw).expect("parse");
        assert_eq!(cfg.inference.appliance.interactive_floor_millis, 3500);
        let out = toml::to_string(&cfg).expect("serialize");
        assert!(out.contains("interactive_floor_tok_s = 3.5"), "{out}");
    }

    #[test]
    fn rejects_bad_orchestrator_and_port_base() {
        let mut cfg = NexusConfig::default();
        cfg.inference.appliance.orchestrator = "nested/model.gguf".into();
        let err = cfg.validate().unwrap_err().to_string();
        assert!(err.contains("nested/model.gguf"), "{err}");

        cfg.inference.appliance.orchestrator.clear();
        cfg.cluster.adb.host_port_base = 8090;
        let err = cfg.validate().unwrap_err().to_string();
        assert!(err.contains("18080") || err.contains("8090"), "{err}");

        cfg.cluster.adb.host_port_base = 18080;
        cfg.hardware.safety.battery_action = "sleep".into();
        let err = cfg.validate().unwrap_err().to_string();
        assert!(err.contains("sleep"), "{err}");
    }
}
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test --locked --lib config::tests -- --test-threads=8`

Expected: FAIL compiling, missing `appliance` and `adb` fields.

- [ ] **Step 3: Implement**

`interactive_floor_millis` uses a pair of serde helpers. Default helper returns `3000`. Deserialize with `f64`, then `round` to millis. `ApplianceConfig` and `AdbConfig` implement `Default` and are `#[serde(default)]` on the parent structs.

`validate` adds:

- `orchestrator` empty, or `Path::new(&orchestrator).is_absolute()`, or a single path component with no parent. Otherwise `ConfigError::Invalid` containing the raw string.
- `burst_idle_secs >= 1`.
- `max_devices >= 1`.
- `hardware.safety.battery_action` is `decline`, `throttle`, or `ignore`. The error contains the raw value.
- For `i` in `0..max_devices`, `host_api = host_port_base + i * 2` and `host_control = host_api + 1` fit in `u16` and are none of `network.api_port` (8080), `network.control_port` (9998), `network.gateway_port` (8090), `network.discovery_port` (9999), `cluster.rpc_port` (50052). The error names `host_port_base` and the reserved port.

Update `ClusterConfig::default` and `InferenceConfig`'s derived `Default` by giving both new structs a `Default` impl. `InferenceConfig` already derives `Default`, so `ApplianceConfig: Default` is enough.

- [ ] **Step 4: Run the test to verify it passes**

Run: `cargo test --locked --lib config::tests -- --test-threads=8`

Expected: PASS, 3 tests.

- [ ] **Step 5: Commit**

```bash
git add src/config.rs
git commit -m "feat(config): add appliance and adb link settings"
```

---

### Task 2: Battery probe and control-plane fields

**Files:**
- Modify: `src/sysinfo.rs`
- Modify: `src/control_plane.rs` (`ControlPlaneState` near line 28, `build_control_plane_state_with_host` near line 157)
- Test: `src/sysinfo.rs` `mod tests`, `src/control_plane.rs` existing tests or a new `mod tests`

**Interfaces:**
- Consumes: nothing from Task 1
- Produces:
  - `PowerSource` enum: `Ac`, `Battery`, `Unknown` with serde rename `ac`, `battery`, `unknown`
  - `PowerReading { battery_percent: Option<u8>, power_source: PowerSource }`
  - `parse_power_supply(entries: &[(&str, &str, &str)]) -> PowerReading` where each tuple is `(type_text, capacity_text, status_text)`
  - `ControlPlaneState.battery_percent: Option<u8>` with `#[serde(default)]`
  - `ControlPlaneState.power_source: PowerSource` with `#[serde(default)]` so missing JSON becomes `Unknown`
  - `build_control_plane_state_with_host` sets them from `probe_power()`, which reads `/sys/class/power_supply` and calls `parse_power_supply`

- [ ] **Step 1: Write the failing test**

```rust
#[test]
fn battery_parse_prefers_battery_type() {
    use crate::sysinfo::{parse_power_supply, PowerSource};
    // External power wins over a discharging battery entry.
    let plugged = [
        ("Mains", "", "Charging"),
        ("Battery", "40", "Discharging"),
    ];
    let reading = parse_power_supply(&plugged);
    assert_eq!(reading.battery_percent, Some(40));
    assert_eq!(reading.power_source, PowerSource::Ac);
    let unplugged = [("Battery", "40", "Discharging")];
    assert_eq!(parse_power_supply(&unplugged).power_source, PowerSource::Battery);
}

#[test]
fn ac_when_battery_is_charging_or_absent() {
    use crate::sysinfo::{parse_power_supply, PowerSource};
    let charging = [("Battery", "80", "Charging")];
    assert_eq!(parse_power_supply(&charging).power_source, PowerSource::Ac);
    assert_eq!(parse_power_supply(&[]).battery_percent, None);
    assert_eq!(parse_power_supply(&[]).power_source, PowerSource::Ac);
}

#[test]
fn old_state_json_defaults_battery() {
    use crate::control_plane::ControlPlaneState;
    let raw = r#"{
        "node_id": "00000000-0000-0000-0000-000000000001",
        "protocol_version": 1,
        "role": 1,
        "capabilities": [],
        "ready": true,
        "inferring": false,
        "rpc_ready": false,
        "allocatable_memory_mb": 1000
    }"#;
    let state: ControlPlaneState = serde_json::from_str(raw).expect("old peers still parse");
    assert_eq!(state.battery_percent, None);
    assert_eq!(state.power_source, crate::sysinfo::PowerSource::Unknown);
}
```

Match `NodeRole`'s real serde shape if the JSON fixture fails on `role`. Copy a known-good state fixture from `src/control_plane.rs` tests and only add the assertion on the new fields. The battery assertions are the contract. If `role` is a struct newtype, use the encoding those tests already use.

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test --locked --lib sysinfo::tests::battery_parse_prefers_battery_type -- --test-threads=8`

Expected: FAIL compiling, `parse_power_supply` missing.

- [ ] **Step 3: Implement**

`parse_power_supply` rules, in order:

- No entries: `battery_percent: None`, `power_source: Ac`.
- First entry whose type equals `Battery` (ASCII case-insensitive) supplies the percent. Capacity parses as `u8`. A non-integer capacity yields `None` and does not fail the function.
- `power_source` is `Ac` when any entry's status is `Charging`, `Full`, or `Not charging`.
- Otherwise, if the battery status is `Discharging`, `power_source` is `Battery`.
- Otherwise `Unknown` when a battery entry exists but the status is empty or unrecognized, and `Ac` when no battery entry exists.

`probe_power` reads directories under `/sys/class/power_supply`, skips unreadable files, and passes the collected triples to `parse_power_supply`. A missing directory returns the empty-input result (`Ac`, null percent).

Add the two fields to every `ControlPlaneState { ... }` literal in the crate. `cargo test --lib` will list the exhaustiveness errors. Set them from `probe_power()` in `build_control_plane_state_with_host`. Test constructors may use `None` and `PowerSource::Unknown`.

Do not add bytes to `BeaconPacket::encode` or `decode`.

- [ ] **Step 4: Run the test to verify it passes**

Run: `cargo test --locked --lib sysinfo::tests -- --test-threads=8` and `cargo test --locked --lib control_plane:: -- --test-threads=8`

Expected: PASS, including the old state JSON test.

- [ ] **Step 5: Commit**

```bash
git add src/sysinfo.rs src/control_plane.rs
git commit -m "feat(sysinfo): advertise battery on control-plane state"
```

---

### Task 3: Dense fit counts the full weight file

**Files:**
- Modify: `src/cluster/memory.rs` (`from_gguf_budget`, the mmap branch near line 127)
- Modify: `tests/test_placement.rs` (`memory_plan_beats_heuristic_for_small_model`)
- Test: `src/cluster/memory.rs` `mod tests`

**Interfaces:**
- Consumes: `MemoryPlan`, `MemoryPolicy`, `Verdict`, `GgufMetadata::weights_bytes`, `estimate_compute_buffer_mb`
- Produces: dense `load_estimate_mb = weights_mb + kv_cache_mb + compute_buffer_mb` for both the F16 check and the Q8 check. `weights_resident_mb` may still be `0` when mmap is on and mlock is off, because that field describes anonymous RSS, not the fit verdict. `streamable_moe()` handling stays in the existing branch and is not given the full-file estimate.

- [ ] **Step 1: Write the failing test**

Replace `one_point_five_b_fits_with_mmap_not_rejected_by_heuristic` with:

```rust
#[test]
fn dense_mmap_counts_full_weights() {
    let mut heavy = tiny_1p5b();
    heavy.file_size_bytes = 4_500 * 1024 * 1024;
    let mut light = tiny_1p5b();
    light.file_size_bytes = 900 * 1024 * 1024;
    let cpu = SystemProfile {
        total_ram_mb: 6_000,
        available_ram_mb: 4_096,
        detected_backend: AccelerationBackend::X86Baseline,
        recommended_threads: 2,
    };
    let policy = MemoryPolicy {
        max_ram_usage_percent: 75,
        mmap: true,
        mlock: false,
        kv_dtype: KvCacheDtype::F16,
        context_size: 512,
        batch_size: 512,
    };
    let rejected = MemoryPlan::from_gguf(&heavy, &cpu, &policy, false);
    assert_eq!(rejected.verdict, Verdict::Exceeds, "headroom {}", rejected.headroom_mb);
    let accepted = MemoryPlan::from_gguf(&light, &cpu, &policy, false);
    assert!(
        matches!(accepted.verdict, Verdict::Fits | Verdict::FitsIfQuantizedKv),
        "{:?}",
        accepted.verdict
    );
    let phone = SystemProfile {
        total_ram_mb: 16_000,
        available_ram_mb: 12_000,
        detected_backend: AccelerationBackend::Vulkan,
        recommended_threads: 4,
    };
    let on_phone = MemoryPlan::from_gguf(&heavy, &phone, &policy, false);
    assert!(
        matches!(on_phone.verdict, Verdict::Fits | Verdict::FitsIfQuantizedKv),
        "{:?}",
        on_phone.verdict
    );
}
```

Context is 512 so the KV term does not hide the weight-file rule. 75% of 4096 MB is 3072 MB. 4500 MB of weights exceeds it. 900 MB of weights plus a 512-token KV plus the 256 MB x86 compute buffer fits. 75% of 12000 MB is 9000 MB, so the same 4500 MB file fits there.

In `tests/test_placement.rs`, change `memory_plan_beats_heuristic_for_small_model` so the 2000 MB available / 75% profile shows the F16 full-file load over the 1500 MB budget (`headroom_mb` negative). Q8 KV at that geometry is about 336 MB, so full weights plus Q8 KV plus the 256 MB compute buffer is about 1497 MB and the verdict is `FitsIfQuantizedKv`, not `Exceeds`. Assert `FitsIfQuantizedKv`. Keep `assert!(plan.kv_cache_mb < 800)`. Keep `assert!(!profile.can_safely_load(...))`. Do not restore the 10% discount to force `Exceeds`.

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test --locked --lib cluster::memory::tests::dense_mmap_counts_full_weights -- --test-threads=8`

Expected: FAIL because the heavy file is still `Fits` under the 10% discount.

- [ ] **Step 3: Implement**

In `from_gguf_budget`, set `load_estimate_mb` to `weights_mb + kv_cache_mb + compute_buffer_mb` on every path, including mmap without mlock. Build the Q8 comparison from full `weights_mb` plus the Q8 KV plus the compute buffer. Delete `soft_weight_pressure` if nothing else reads it. Leave the MoE `EnableMoeStream` branch in place.

- [ ] **Step 4: Run the test to verify it passes**

Run: `cargo test --locked --lib cluster::memory::tests -- --test-threads=8` and `cargo test --locked --test test_placement -- --test-threads=8`

Expected: PASS. If another test asserted the 10% discount, update that assertion to the full-weight rule. Do not put the discount back.

- [ ] **Step 5: Commit**

```bash
git add src/cluster/memory.rs tests/test_placement.rs
git commit -m "fix(cluster): count full dense weights in the fit verdict"
```

---

### Task 4: Context and KV ladder

**Files:**
- Create: `src/cluster/ladder.rs`
- Modify: `src/cluster/mod.rs` (`mod ladder;` and `pub use`)
- Test: `src/cluster/ladder.rs` `mod tests`

**Interfaces:**
- Consumes: `MemoryPlan::from_gguf_budget`, `KvCacheDtype`, `GgufMetadata`, `SystemProfile`, `MemoryPolicy`
- Produces:
  - `LadderChoice { context_size: usize, kv_dtype: KvCacheDtype, file_index: usize, interactive: bool, predicted_tok_s: f32 }`
  - `fn quant_stem(file_name: &str) -> String`
  - `fn context_steps(requested: usize) -> Vec<usize>` — requested, then each of `4096, 2048, 1024, 512` that is strictly smaller
  - `fn plan_fit_ladder(files: &[GgufMetadata], profile: &SystemProfile, policy: &MemoryPolicy, floor_tok_s: f32, predict: impl Fn(usize, KvCacheDtype, &GgufMetadata) -> f32) -> Option<LadderChoice>`
  - `files[0]` is the requested model. Later entries are already-loaded sibling metadata. The function does not read the disk.

Sibling rule: same `architecture` and same `model_name` when both names are `Some`. When `model_name` is `None`, same `quant_stem`. `quant_stem` strips a final component that contains `Q2`, `Q3`, `Q4`, `Q5`, `Q6`, `Q8`, `F16`, `F32`, or `IQ` at a `-`, `_`, or `.` boundary, case-insensitive, and strips one trailing `.gguf`.

`plan_fit_ladder` tries the requested file, then siblings from largest `weights_bytes` to smallest. For each file it walks `context_steps` and, at each context, F16 then Q8_0. The first `Fits` or `FitsIfQuantizedKv` wins. It does not keep walking after that fit. `interactive` is `predicted_tok_s >= floor_tok_s`. Pass `cluster_offload_available: false` into the memory plan. This ladder does not invent an RPC split.

- [ ] **Step 1: Write the failing test**

```rust
fn geo(file_size_bytes: u64) -> GgufMetadata {
    GgufMetadata {
        version: 3,
        tensor_count: 0,
        kv_count: 0,
        metadata: HashMap::new(),
        architecture: Some("llama".into()),
        model_name: Some("coder".into()),
        context_length: Some(4096),
        block_count: Some(28),
        head_count: Some(12),
        head_count_kv: Some(12),
        embedding_length: Some(1536),
        expert_count: None,
        expert_used_count: None,
        file_size_bytes,
        tensors: vec![],
        quant_label: Some("Q4_K".into()),
    }
}

#[test]
fn quant_stem_strips_quant_token() {
    assert_eq!(quant_stem("coder-7b-Q4_K_M.gguf"), "coder-7b");
    assert_eq!(quant_stem("coder-7b-Q8_0.gguf"), "coder-7b");
    assert_eq!(quant_stem("plain.gguf"), "plain");
}

#[test]
fn context_steps_stay_at_or_below_request() {
    assert_eq!(context_steps(4096), vec![4096, 2048, 1024, 512]);
    assert_eq!(context_steps(2048), vec![2048, 1024, 512]);
    assert_eq!(context_steps(512), vec![512]);
}

#[test]
fn ladder_drops_to_2048_q8_then_stops() {
    // 950 MiB weights. Exact KV at this geometry is ~672 MB at 4096 F16,
    // ~336 MB at 4096 Q8 and at 2048 F16, ~168 MB at 2048 Q8.
    // x86 compute buffer is 256 MB. Budget is 75% of 2000 MB = 1500 MB.
    // 4096 F16 = 1878, 4096 Q8 = 1542, 2048 F16 = 1542, 2048 Q8 = 1374.
    let files = [geo(950 * 1024 * 1024)];
    let profile = SystemProfile {
        total_ram_mb: 4_000,
        available_ram_mb: 2_000,
        detected_backend: AccelerationBackend::X86Baseline,
        recommended_threads: 2,
    };
    let policy = MemoryPolicy {
        max_ram_usage_percent: 75,
        mmap: true,
        mlock: false,
        kv_dtype: KvCacheDtype::F16,
        context_size: 4096,
        batch_size: 512,
    };
    let choice = plan_fit_ladder(&files, &profile, &policy, 3.0, |_ctx, _kv, _gguf| 4.0)
        .expect("2048 q8 fits");
    assert_eq!(choice.context_size, 2048);
    assert_eq!(choice.kv_dtype, KvCacheDtype::Q8_0);
    assert_eq!(choice.file_index, 0);
    assert!(choice.interactive);
}

#[test]
fn slow_fit_is_batch_only() {
    let files = [geo(200 * 1024 * 1024)];
    let profile = SystemProfile {
        total_ram_mb: 8_000,
        available_ram_mb: 6_000,
        detected_backend: AccelerationBackend::X86Baseline,
        recommended_threads: 2,
    };
    let policy = MemoryPolicy {
        max_ram_usage_percent: 75,
        mmap: true,
        mlock: false,
        kv_dtype: KvCacheDtype::F16,
        context_size: 512,
        batch_size: 512,
    };
    let choice = plan_fit_ladder(&files, &profile, &policy, 3.0, |_ctx, _kv, _gguf| 1.0)
        .expect("small file fits");
    assert!(!choice.interactive);
    assert_eq!(choice.context_size, 512);
}
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test --locked --lib cluster::ladder::tests -- --test-threads=8`

Expected: FAIL compiling, module missing.

- [ ] **Step 3: Implement `src/cluster/ladder.rs` and export it**

`pub use ladder::{context_steps, plan_fit_ladder, quant_stem, LadderChoice};` from `src/cluster/mod.rs`.

KV arg strings for later tasks come from `KvCacheDtype::as_llama_arg` (`f16`, `q8_0`). Do not spawn `llama-server` here.

- [ ] **Step 4: Run the test to verify it passes**

Run: `cargo test --locked --lib cluster::ladder::tests -- --test-threads=8`

Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src/cluster/ladder.rs src/cluster/mod.rs
git commit -m "feat(cluster): add the context and quant fit ladder"
```

---

### Task 5: Rank floor, size scale, and the USB band

**Files:**
- Modify: `src/cluster/rank.rs` (`predict_local_tok_s` near line 549)
- Modify: `src/cluster/mod.rs` re-exports
- Test: `src/cluster/rank.rs` `mod tests`

**Interfaces:**
- Consumes: `LinkClass` is defined in this task and later stored on `PeerNode` in Task 7. Define it in `src/cluster/rank.rs` and re-export it from `cluster/mod.rs` so Task 7 can use `crate::cluster::LinkClass`.
- Produces:
  - `enum LinkClass { Local, Usb, Wifi }`
  - `struct BurstScore { name: String, link: LinkClass, score_millis: u32, eligible: bool }`
  - `fn select_burst_winner(plans: &[BurstScore]) -> Option<usize>`
  - `fn cold_start_scale(weights_gib: f32, measured: bool) -> f32` — `1.0` when `measured` is true, otherwise `1.0 / weights_gib.max(0.25)`
  - `predict_local_tok_s` takes `weights_gib: f32` and multiplies the unmeasured prior by `cold_start_scale`. A `measured` sample is not scaled again. Existing quant and thermal multipliers stay.
  - `enum BatteryGate { Eligible, Throttle, Decline }`
  - `fn battery_gate(percent: Option<u8>, floor: u8, action: &str) -> BatteryGate`
  - `fn thermal_eligible(thermal_index: u8) -> bool` is `thermal_index <= 75`

`score_millis` avoids `f32` equality in tests. `select_burst_winner` compares `score_millis as f32 / 1000.0`. The band is `score >= 0.85 * best_wifi`, with `best_wifi = 0` when no eligible Wi-Fi plan exists. Ineligible rows are skipped. Callers set `eligible: false` when `thermal_eligible` is false or `battery_gate` is `Decline`. When the gate is `Throttle`, the caller passes `score_millis / 2`. A `None` percent returns `Eligible` for every action.

- [ ] **Step 1: Write the failing test**

```rust
#[test]
fn cold_start_shrinks_with_weight_gib() {
    assert!((cold_start_scale(1.0, false) - 1.0).abs() < 0.001);
    assert!((cold_start_scale(4.0, false) - 0.25).abs() < 0.001);
    assert!((cold_start_scale(4.0, true) - 1.0).abs() < 0.001);
    assert!((cold_start_scale(0.1, false) - 4.0).abs() < 0.001);
}

#[test]
fn usb_band_matches_the_spec_table() {
    let plans = [
        BurstScore { name: "usb".into(), link: LinkClass::Usb, score_millis: 8000, eligible: true },
        BurstScore { name: "wifi".into(), link: LinkClass::Wifi, score_millis: 8000, eligible: true },
    ];
    assert_eq!(select_burst_winner(&plans).unwrap(), 0);

    let slower_usb = [
        BurstScore { name: "usb".into(), link: LinkClass::Usb, score_millis: 6000, eligible: true },
        BurstScore { name: "wifi".into(), link: LinkClass::Wifi, score_millis: 8000, eligible: true },
    ];
    assert_eq!(select_burst_winner(&slower_usb).map(|i| plans_name(&slower_usb, i)), Some("wifi"));

    let edge = [
        BurstScore { name: "usb".into(), link: LinkClass::Usb, score_millis: 7000, eligible: true },
        BurstScore { name: "wifi".into(), link: LinkClass::Wifi, score_millis: 8000, eligible: true },
    ];
    // 7000 >= 0.85 * 8000
    assert_eq!(select_burst_winner(&edge).unwrap(), 0);

    let hot = [
        BurstScore { name: "usb".into(), link: LinkClass::Usb, score_millis: 8000, eligible: false },
        BurstScore { name: "wifi".into(), link: LinkClass::Wifi, score_millis: 8000, eligible: true },
    ];
    assert_eq!(select_burst_winner(&hot).unwrap(), 1);

    let two_usb = [
        BurstScore { name: "slow-cable".into(), link: LinkClass::Usb, score_millis: 7000, eligible: true },
        BurstScore { name: "fast-cable".into(), link: LinkClass::Usb, score_millis: 9000, eligible: true },
        BurstScore { name: "wifi".into(), link: LinkClass::Wifi, score_millis: 8000, eligible: true },
    ];
    assert_eq!(select_burst_winner(&two_usb).unwrap(), 1);
}

fn plans_name(plans: &[BurstScore], index: usize) -> &str {
    &plans[index].name
}

#[test]
fn battery_and_thermal_gates() {
    assert_eq!(battery_gate(None, 20, "decline"), BatteryGate::Eligible);
    assert_eq!(battery_gate(Some(19), 20, "decline"), BatteryGate::Decline);
    assert_eq!(battery_gate(Some(19), 20, "throttle"), BatteryGate::Throttle);
    assert_eq!(battery_gate(Some(19), 20, "ignore"), BatteryGate::Eligible);
    assert_eq!(battery_gate(Some(20), 20, "decline"), BatteryGate::Eligible);
    assert!(!thermal_eligible(76));
    assert!(thermal_eligible(75));
}
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test --locked --lib cluster::rank::tests::usb_band_matches_the_spec_table -- --test-threads=8`

Expected: FAIL compiling.

- [ ] **Step 3: Implement**

Update every call of `predict_local_tok_s` inside `rank.rs` to pass `weights_gib`, computed as `gguf.weights_bytes() as f32 / (1024.0 * 1024.0 * 1024.0)`.

Export `select_burst_winner`, `BurstScore`, `LinkClass`, `cold_start_scale`, `battery_gate`, `BatteryGate`, and `thermal_eligible` from `cluster/mod.rs`.

Do not add a product-name bonus. Do not change `network_bound_tok_s`.

- [ ] **Step 4: Run the test to verify it passes**

Run: `cargo test --locked --lib cluster::rank::tests -- --test-threads=8`

Expected: PASS, including the pre-existing rank tests. Those tests only require `predicted_tok_s > 0` or an exact MoE measurement. The MoE measurement path must stay unscaled.

- [ ] **Step 5: Commit**

```bash
git add src/cluster/rank.rs src/cluster/mod.rs
git commit -m "feat(cluster): rank burst peers with the usb score band"
```

---

### Task 6: ADB listing, host ports, and the serial cache

**Files:**
- Modify: `src/tunnel.rs` (`list_devices` near line 90, `setup_tunnel` near line 143)
- Create: `src/adb_links.rs`
- Modify: `src/lib.rs` (`pub mod adb_links;`)
- Test: `src/tunnel.rs` and `src/adb_links.rs`

**Interfaces:**
- Consumes: `AdbDeviceInfo`, `AdbConfig::host_port_base`, `AdbConfig::max_devices`
- Produces:
  - `fn parse_adb_devices_l(stdout: &str) -> Vec<AdbDeviceInfo>`
  - `struct HostForward { serial: String, host_api: u16, host_control: u16 }`
  - `fn allocate_forwards(devices: &[AdbDeviceInfo], host_port_base: u16, max_devices: u8) -> (Vec<HostForward>, Vec<String>)`
  - `AdbLinkRecord { serial: String, node_id: Uuid, device_api_port: u16, device_control_port: u16, device_rpc_port: u16 }`
  - `AdbLinkCache { links: Vec<AdbLinkRecord> }` with `load`, `save`, `retain_present`, `ports_for`, `upsert`, `duplicate_uuid_losers`

`list_devices` reads `adb` and calls `parse_adb_devices_l`. Tests call the parser only.

`allocate_forwards` keeps rows with `authorized == true`, sorts by serial bytes, and keeps the first `max_devices`. The returned `Vec<String>` is the skipped authorized serials, in sorted order. Index `i` uses `host_api = base + i * 2` and `host_control = host_api + 1`.

`duplicate_uuid_losers` takes `&[(String, Uuid)]` already sorted by serial and returns every serial after the first for a repeated UUID.

`AdbLinkCache::load` on a missing file returns an empty cache. `retain_present` drops records whose serial is absent from the current authorized list. `ports_for` returns the stored device ports.

`setup_tunnel(api_port, rpc_port, None)` today picks the first device. Change its body so a `None` serial returns `TunnelError::NoDeviceConnected` when zero authorized devices exist, and otherwise forwards only `allocate_forwards(...)[0]` when the caller still wants one tunnel. Add `setup_all_tunnels(devices, host_port_base, max_devices, device_api, device_control) -> Vec<HostForward>` that builds the forward argument list without running `adb` when `NEXUS_ADB_DRY_RUN=1`. The production path runs:

```text
adb -s <serial> forward tcp:<host_api> tcp:<device_api>
adb -s <serial> forward tcp:<host_control> tcp:<device_control>
```

Device ports come from `ports_for`, else the local `api_port` and `control_port` passed in. Teardown of one serial is `adb -s <serial> forward --remove tcp:<host_port>` for both host ports. Do not call `adb forward --remove` without `-s`.

- [ ] **Step 1: Write the failing test**

```rust
#[test]
fn parse_keeps_unauthorized_and_allocate_skips_them() {
    let stdout = "\
List of devices attached
serial-b device usb:1-1 product:foo model:Bar
serial-a device usb:1-2 product:foo model:Baz
serial-c unauthorized usb:1-3
";
    let devices = parse_adb_devices_l(stdout);
    assert_eq!(devices.len(), 3);
    let (forwards, skipped) = allocate_forwards(&devices, 18080, 8);
    assert!(skipped.is_empty());
    assert_eq!(forwards[0].serial, "serial-a");
    assert_eq!(forwards[0].host_api, 18080);
    assert_eq!(forwards[0].host_control, 18081);
    assert_eq!(forwards[1].serial, "serial-b");
    assert_eq!(forwards[1].host_api, 18082);
    assert_eq!(forwards[1].host_control, 18083);
    assert!(!forwards.iter().any(|f| f.serial == "serial-c"));
}

#[test]
fn max_devices_reports_skipped_serials() {
    let stdout = "\
serial-a device
serial-b device
serial-c device
";
    let devices = parse_adb_devices_l(stdout);
    let (forwards, skipped) = allocate_forwards(&devices, 18080, 2);
    assert_eq!(forwards.len(), 2);
    assert_eq!(skipped, vec!["serial-c".to_string()]);
}
```

In `adb_links.rs` tests, round-trip a cache through a temp file created with `std::env::temp_dir()`, drop a serial via `retain_present`, and assert `duplicate_uuid_losers` keeps the first serial in sorted order.

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test --locked --lib tunnel::tests::parse_keeps_unauthorized_and_allocate_skips_them -- --test-threads=8`

Expected: FAIL compiling.

- [ ] **Step 3: Implement the parser, allocator, cache, and per-serial forward commands**

Move the existing line parser in `list_devices` into `parse_adb_devices_l` without changing which tokens it reads (`model:`, `product:`, state `device`).

- [ ] **Step 4: Run the test to verify it passes**

Run: `cargo test --locked --lib tunnel::tests -- --test-threads=8` and `cargo test --locked --lib adb_links:: -- --test-threads=8`

Expected: PASS. No test invokes the `adb` binary.

- [ ] **Step 5: Commit**

```bash
git add src/tunnel.rs src/adb_links.rs src/lib.rs
git commit -m "feat(tunnel): allocate one localhost forward per adb serial"
```

---

### Task 7: Merge USB and Wi-Fi onto one peer

**Files:**
- Modify: `src/discovery.rs` (`PeerNode` near line 308, `api_endpoint` / `control_endpoint` near line 367)
- Modify: `src/mdns.rs` (TXT property map near line 50, and the parser that reads those keys)
- Test: `src/discovery.rs` tests and `src/mdns.rs` tests

**Interfaces:**
- Consumes: `crate::cluster::LinkClass`, `crate::sysinfo::PowerSource`
- Produces on `PeerNode`, each `#[serde(default)]`:
  - `link_class: LinkClass` default `Wifi`
  - `usb_serial: Option<String>`
  - `usb_api_endpoint: Option<String>`
  - `usb_control_endpoint: Option<String>`
  - `battery_percent: Option<u8>`
  - `power_source: PowerSource` default `Unknown`
  - `fn apply_usb_probe(&mut self, serial: &str, host_api: u16, host_control: u16)`
  - `fn clear_usb(&mut self)` sets `link_class` to `Wifi` and clears the three USB fields
  - `api_endpoint` and `control_endpoint` return the localhost URL while `link_class == Usb` and the matching `Option` is `Some`

mDNS register adds `bat` (empty when percent is `None`, otherwise the decimal integer) and `pwr` (`ac`, `battery`, or `unknown`). The browse parser writes those into the peer and ignores a missing key.

- [ ] **Step 1: Write the failing test**

```rust
fn usb_fixture() -> PeerNode {
    PeerNode {
        uuid: Uuid::from_u128(7),
        addr: "192.168.1.20:9999".parse().unwrap(),
        role: NodeRole::from_str_role("host"),
        status: StatusFlags::READY,
        api_port: 8080,
        rpc_port: 50052,
        control_port: 9998,
        total_ram_mb: 0,
        free_ram_mb: 0,
        backend: AccelerationBackend::Vulkan,
        thermal_index: 0,
        active_model: String::new(),
        display_name: "node-a".into(),
        moe_stream: false,
        moe_cache_ceil_mb: 0,
        last_seen: Instant::now(),
        link_class: LinkClass::Wifi,
        usb_serial: None,
        usb_api_endpoint: None,
        usb_control_endpoint: None,
        battery_percent: None,
        power_source: PowerSource::Unknown,
    }
}

#[test]
fn usb_probe_and_beacon_share_one_endpoint_rule() {
    let mut peer = usb_fixture();
    peer.apply_usb_probe("serial-a", 18080, 18081);
    assert_eq!(peer.api_endpoint(), "http://127.0.0.1:18080");
    assert_eq!(peer.control_endpoint(), "http://127.0.0.1:18081");
    assert_eq!(peer.link_class, LinkClass::Usb);
    peer.free_ram_mb = 4000;
    assert_eq!(peer.api_endpoint(), "http://127.0.0.1:18080");
    peer.clear_usb();
    assert_eq!(peer.link_class, LinkClass::Wifi);
    assert_eq!(peer.api_endpoint(), "http://192.168.1.20:8080");
}
```

`NodeRole`, `StatusFlags`, `LinkClass`, and `PowerSource` are the types from `discovery.rs`, `cluster`, and `sysinfo`. If `StatusFlags::READY` is a const with a different path, use the same value the beacon tests already use. Do not drop fields from this literal. The beacon-update site near `existing.role = endpoint.role` in `src/discovery.rs` must leave `link_class` and the USB endpoints untouched. Add a one-line comment there that says so.

Add an mDNS test that a property map containing `bat=40` and `pwr=battery` parses to `Some(40)` and `PowerSource::Battery`, and that a map without those keys leaves percent `None`.

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test --locked --lib discovery:: -- usb_probe_and_beacon -- --test-threads=8`

Expected: FAIL compiling, `apply_usb_probe` missing.

- [ ] **Step 3: Implement**

`apply_usb_probe` sets `link_class = Usb`, `usb_serial`, and the two `http://127.0.0.1` endpoints. It does not change `addr`. Beacon update code that assigns `existing.role` and RAM must not assign `link_class` or the USB endpoints.

Every `PeerNode { ... }` literal in the crate needs the new fields or `..Default` where a default exists. `PeerNode` does not implement `Default` today. Prefer filling the new fields explicitly so `cargo test --lib` lists every site. `#[serde(default)]` covers disk snapshots.

- [ ] **Step 4: Run the test to verify it passes**

Run: `cargo test --locked --lib discovery:: -- --test-threads=8` and `cargo test --locked --lib mdns:: -- --test-threads=8`

Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src/discovery.rs src/mdns.rs
git commit -m "feat(discovery): prefer a live usb forward for the same node"
```

---

### Task 8: Route decision, gateway header, and hub URL

**Files:**
- Create: `src/appliance.rs`
- Modify: `src/lib.rs`
- Modify: `src/gateway.rs` (`handle_chat_completions` near line 1304, `resolve_model_upstream` near line 172)
- Modify: `src/main.rs` (hub host selection near line 309)
- Test: `src/appliance.rs`, `src/gateway.rs` `mod unit_tests`

**Interfaces:**
- Consumes: `ApplianceConfig::floor_tok_s`, `LinkClass`, `select_burst_winner`, `BurstScore`, `plan_fit_ladder`, `PeerNode::control_endpoint`, `PeerNode::label`
- Produces:
  - `enum RouteStay { Tier1, Tier2, Fallback, NoInteractivePeer }`
  - `enum ApplianceDecision { Local { reason: RouteStay }, Escalate { tag: String }, Unavailable { tag: String } }`
  - `fn front_door_tags(preset_tags: &[String]) -> Vec<String>` appends `general` and `chat` if missing
  - `fn decide_appliance(local_tags: &[String], local_loaded: bool, tier1: Option<&str>, tier2: Option<Result<&str, ()>>) -> ApplianceDecision`
  - `fn format_route_header(label: &str, link: &str, model: &str, route: &str) -> String`
  - `fn format_route_status(label: &str, model: &str, link: &str, tok_s: f32, extra: Option<&str>) -> String`
  - `fn hub_client_base_url(default_host: Option<&str>, appliance_enabled: bool, gateway_enabled: bool, gateway_port: u16) -> Option<String>`
  - `fn burst_idle_elapsed(idle_for: Duration, burst_idle_secs: u64, is_front_door: bool) -> bool`
  - `struct FrontDoorFile<'a> { pub name: &'a str, pub weights_bytes: u64, pub preset_tags: &'a [String], pub fits_pin_policy: bool, pub fits_1024_q8: bool }`
  - `fn select_front_door<'a>(pin: &str, files: &'a [FrontDoorFile<'a>]) -> Option<&'a str>`
  - `fn score_millis(tok_s: f32) -> u32` returns `(tok_s.max(0.0) * 1000.0) as u32`
  - `const PEER_LOST_STATUS: &str = "peer lost"`

`decide_appliance` order:

1. `tier1 == Some(tag)` and `local_tags` contains it → `Local { Tier1 }`.
2. `tier1 == Some(tag)` and it does not → `Escalate { tag }`.
3. `tier2 == Some(Ok(tag))` and local tags contain it → `Local { Tier2 }`.
4. `tier2 == Some(Ok(tag))` and they do not → `Escalate { tag }`.
5. `tier2 == Some(Err(()))` and `local_loaded` → `Local { Fallback }`.
6. `tier2 == Some(Err(()))` and not loaded → `Unavailable { tag: "fallback" }`.
7. No tier1 and no tier2, local loaded → `Local { Tier1 }`.
8. Otherwise `Unavailable { tag: "general" }`.

The caller turns `Escalate` into `Local { NoInteractivePeer }` when `local_loaded` is true and `select_burst_winner` returns `None`. The caller returns HTTP 503 when `local_loaded` is false. 503 body:

```json
{"error":{"message":"no interactive peer for tag coder","type":"no_interactive_peer"}}
```

The message contains the tag. Header value from `format_route_header` is `label; link=usb; model=name; route=tag` with `link` one of `local`, `usb`, `wifi`. Status line examples are in spec section 14. `format_route_status` joins label, model, link word (`local`, `USB`, `Wi-Fi`), and `{tok:.0} tok/s`, then appends `extra` after ` · ` when `Some`.

`hub_client_base_url` returns `Some(default_host)` when that pin is set. Else `Some(http://127.0.0.1:{gateway_port})` when both flags are true. Else `None`, and `main.rs` keeps today's discovery and USB fallback for `nexus client` and for a hub whose gateway is off.

`burst_idle_elapsed` is true only when `!is_front_door` and `idle_for >= Duration::from_secs(burst_idle_secs)`.

`select_front_door` implements spec section 10.2:

1. If `pin` is non-empty and a file with that `name` has `fits_pin_policy`, return that name.
2. Else the smallest `weights_bytes` among files whose `preset_tags` contain `orchestrator` or `router` and whose `fits_pin_policy` is true.
3. Else the smallest `weights_bytes` among files with `fits_1024_q8`.
4. Else `None`. The caller prints `appliance: no local model fits` and skips tier 2.

The caller fills `fits_pin_policy` by running `plan_fit_ladder` at the operator context, and `fits_1024_q8` by running it with `MemoryPolicy.context_size = 1024` and `kv_dtype = Q8_0`. `select_front_door` itself does not open files.

On `Escalate`, build each `BurstScore` with `score_millis(choice.predicted_tok_s)` after applying `BatteryGate::Throttle` as integer division by 2. `eligible` is false when the ladder choice is missing, `interactive` is false, `!thermal_eligible`, or the gate is `Decline`.

A stream whose upstream socket closes before the done event appends one status message whose text is `PEER_LOST_STATUS`. Do not send the prompt again inside that function.

Gateway chat, when appliance is enabled:

- Build front-door tags from the loaded local model's preset.
- Run the existing router for tier 1. Call tier 2 only when tier 1 misses and a local model is loaded.
- On `Escalate`, build `BurstScore` rows from peers that are ready, thermal `<= 75`, and battery-eligible. Score comes from `plan_fit_ladder` on pantry metadata for GGUFs whose preset carries the tag. A plan with `interactive: false` is stored with `eligible: false`.
- Winner's control URL is `peer.control_endpoint()`. If its catalog lacks the digest, call the existing `request_blob_fetch` with `blob_url` of the local control plane. Then send the existing model-load request. Do not add a second transfer protocol.
- Set response header `X-Nexus-Route` from `format_route_header`.
- Record `Instant` per burst peer uuid. On the hub's existing periodic tick, call `burst_idle_elapsed` and `dispatch_unload_model` for peers that have been idle. Do not unload the front door.

- [ ] **Step 1: Write the failing test**

```rust
#[test]
fn general_stays_local_and_coder_escalates() {
    let tags = front_door_tags(&[]);
    assert!(tags.iter().any(|t| t == "general"));
    let stay = decide_appliance(&tags, true, Some("general"), None);
    assert!(matches!(stay, ApplianceDecision::Local { reason: RouteStay::Tier1 }));
    let go = decide_appliance(&tags, true, Some("coder"), None);
    assert_eq!(go, ApplianceDecision::Escalate { tag: "coder".into() });
}

#[test]
fn header_and_status_line() {
    assert_eq!(
        format_route_header("phone-a", "usb", "coder-7b", "coder"),
        "phone-a; link=usb; model=coder-7b; route=coder"
    );
    let line = format_route_status("phone-a", "coder-7b", "USB", 18.2, Some("escalated, route=coder"));
    assert_eq!(line, "phone-a · coder-7b · USB · 18 tok/s · escalated, route=coder");
}

#[test]
fn hub_url_ignores_usb_when_appliance_gateway_is_on() {
    assert_eq!(
        hub_client_base_url(None, true, true, 8090),
        Some("http://127.0.0.1:8090".into())
    );
    assert_eq!(
        hub_client_base_url(Some("http://10.0.0.2:8080"), true, true, 8090),
        Some("http://10.0.0.2:8080".into())
    );
    assert_eq!(hub_client_base_url(None, true, false, 8090), None);
}

#[test]
fn front_door_is_exempt_from_burst_idle() {
    assert!(!burst_idle_elapsed(Duration::from_secs(10_000), 600, true));
    assert!(burst_idle_elapsed(Duration::from_secs(600), 600, false));
    assert!(!burst_idle_elapsed(Duration::from_secs(599), 600, false));
}

#[test]
fn front_door_pick_order() {
    let orch = "orchestrator".to_string();
    let tagged = [
        FrontDoorFile { name: "big.gguf", weights_bytes: 5_000, preset_tags: &[], fits_pin_policy: true, fits_1024_q8: true },
        FrontDoorFile { name: "orch.gguf", weights_bytes: 2_000, preset_tags: std::slice::from_ref(&orch), fits_pin_policy: true, fits_1024_q8: true },
        FrontDoorFile { name: "tiny.gguf", weights_bytes: 800, preset_tags: &[], fits_pin_policy: false, fits_1024_q8: true },
    ];
    assert_eq!(select_front_door("big.gguf", &tagged), Some("big.gguf"));
    assert_eq!(select_front_door("missing.gguf", &tagged), Some("orch.gguf"));
    assert_eq!(select_front_door("", &tagged), Some("orch.gguf"));

    let untagged = [
        FrontDoorFile { name: "big.gguf", weights_bytes: 5_000, preset_tags: &[], fits_pin_policy: false, fits_1024_q8: true },
        FrontDoorFile { name: "tiny.gguf", weights_bytes: 800, preset_tags: &[], fits_pin_policy: false, fits_1024_q8: true },
    ];
    assert_eq!(select_front_door("", &untagged), Some("tiny.gguf"));
    assert_eq!(select_front_door("", &[]), None);
}
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test --locked --lib appliance:: -- --test-threads=8`

Expected: FAIL compiling, module missing.

- [ ] **Step 3: Implement the pure functions, then wire gateway and `main.rs`**

In `main.rs`, replace the USB-before-gateway branch with:

```rust
let host = if let Some(pinned) = hub_client_base_url(
    default_host.as_deref(),
    appliance_enabled,
    gateway_enabled,
    gateway_port,
) {
    pinned
} else if /* gateway disabled: existing discovery / USB client path */ {
```

Read `appliance_enabled` from `cfg.inference.appliance.enabled` in the same config lock that already reads `gateway_enabled`. Do not call `resolve_transport_endpoint` on the hub path when that function returns `Some`.

Add a gateway unit test that `decide_appliance` plus an empty winner list and `local_loaded: true` is reported as reason `no interactive peer`, and that `local_loaded: false` maps to status 503 with `type` `no_interactive_peer`. Build that mapping as `fn decision_after_rank(decision: ApplianceDecision, winner: Option<usize>, local_loaded: bool) -> Result<ApplianceDecision, StatusCode>` so the test does not open a socket.

- [ ] **Step 4: Run the test to verify it passes**

Run: `cargo test --locked --lib appliance:: -- --test-threads=8` and `cargo test --locked --lib gateway:: -- --test-threads=8`

Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src/appliance.rs src/lib.rs src/gateway.rs src/main.rs
git commit -m "feat(appliance): keep ordinary chat on the local gateway"
```

---

### Task 9: Doctor, Tunnel tab, batch badge, and the product-name ban

**Files:**
- Modify: `src/doctor.rs`
- Modify: `src/ui/tunnel_view.rs`
- Modify: `src/cluster/mod.rs` or the Models badge helper that maps `Verdict` to `[OK]` / `[RPC]` / `[OOM]`
- Create: `tests/test_appliance_ban.rs`
- Test: `src/doctor.rs` tests, `src/ui/tunnel_view.rs` if it has a pure helper, `tests/test_appliance_ban.rs`

**Interfaces:**
- Consumes: `format_route_status`, `AdbDeviceInfo`, `HostForward`, `PeerNode::label`, `LadderChoice.interactive`
- Produces:
  - `fn doctor_appliance_lines(orchestrator: Option<&str>, floor_tok_s: f32, usb: &[(&str, Option<&str>)], wifi_labels: &[&str], skipped_serials: &[&str]) -> Vec<String>`
  - `fn tunnel_row_line(serial: &str, authorized: bool, adb_model: Option<&str>, nexus_label: Option<&str>) -> String`
  - Models badge `batch` when the only passing ladder choice has `interactive == false`

`doctor_appliance_lines` first line is `appliance: <file>` or `appliance: no local model fits`. Second line is `interactive floor: {floor:.1} tok/s`. Then one line per USB pair: `usb <serial>: <label>` or `usb <serial>: Nexus not answering`. Then `wifi: <label>` lines. Then `adb skipped: <serial>` when `max_devices` overflowed.

`tunnel_row_line` for an authorized phone that answered is `<serial>  <nexus_label>  (<adb_model>)`. For an authorized phone that did not answer: `<serial>  Nexus not answering  (<adb_model>)`. For unauthorized: `<serial>  unauthorized`. The ADB model is only in this line. `PeerNode.display_name` is not set from it.

The ban test reads these files as text and fails if any line contains `nothing`, `s23`, `galaxy`, or `sm-s`, case-insensitive:

- `src/appliance.rs`
- `src/tunnel.rs`
- `src/adb_links.rs`
- `src/cluster/ladder.rs`
- `src/cluster/rank.rs`
- `src/gateway.rs`
- `src/discovery.rs`
- `src/doctor.rs`

It does not read `tests/test_discovery.rs`, `README.md`, or this plan. Those files already contain lab names.

- [ ] **Step 1: Write the failing test**

```rust
#[test]
fn doctor_lines_name_serials_not_products() {
    let lines = doctor_appliance_lines(
        None,
        3.0,
        &[("serial-a", None), ("serial-b", Some("node-bbbbbbbb"))],
        &["node-cccccccc"],
        &["serial-d"],
    );
    assert_eq!(lines[0], "appliance: no local model fits");
    assert!(lines.iter().any(|l| l == "usb serial-a: Nexus not answering"));
    assert!(lines.iter().any(|l| l == "usb serial-b: node-bbbbbbbb"));
    assert!(lines.iter().any(|l| l == "wifi: node-cccccccc"));
    assert!(lines.iter().any(|l| l == "adb skipped: serial-d"));
}

#[test]
fn tunnel_row_keeps_adb_model_out_of_the_node_name() {
    let line = tunnel_row_line("serial-a", true, Some("Bar"), None);
    assert_eq!(line, "serial-a  Nexus not answering  (Bar)");
    let denied = tunnel_row_line("serial-c", false, None, None);
    assert_eq!(denied, "serial-c  unauthorized");
}
```

`tests/test_appliance_ban.rs`:

```rust
#[test]
fn decision_files_do_not_name_products() {
    const FILES: &[&str] = &[
        "src/appliance.rs",
        "src/tunnel.rs",
        "src/adb_links.rs",
        "src/cluster/ladder.rs",
        "src/cluster/rank.rs",
        "src/gateway.rs",
        "src/discovery.rs",
        "src/doctor.rs",
    ];
    const BANNED: &[&str] = &["nothing", "s23", "galaxy", "sm-s"];
    for path in FILES {
        let text = std::fs::read_to_string(path).unwrap_or_else(|e| panic!("read {path}: {e}"));
        let lower = text.to_ascii_lowercase();
        for word in BANNED {
            assert!(!lower.contains(word), "{path} contains {word}");
        }
    }
}
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test --locked --lib doctor:: -- doctor_lines_name_serials -- --test-threads=8`

Expected: FAIL compiling.

- [ ] **Step 3: Implement the line builders and call them from `nexus doctor`, the Tunnel tab, and the Models badge**

Chat status uses `format_route_status` from Task 8 and pushes a `role: "status"` message. Do not append that line to the model prompt.

- [ ] **Step 4: Run the test to verify it passes**

Run: `cargo test --locked --lib doctor:: -- --test-threads=8` and `cargo test --locked --test test_appliance_ban -- --test-threads=8`

Expected: PASS. Then run the acceptance search from the repo root:

```bash
rg -i 'nothing phone|sm-s918|galaxy s23' src tests
```

Expected: no matches. A hit in `docs/` is allowed. A hit under `src/` or `tests/` fails the task.

- [ ] **Step 5: Commit**

```bash
git add src/doctor.rs src/ui/tunnel_view.rs src/cluster/mod.rs tests/test_appliance_ban.rs
git commit -m "feat(doctor): report usb serials and block product-name routing"
```

The badge string is `batch`. Put `fn fit_badge(interactive: bool, verdict_label: &str) -> &str` in `src/cluster/mod.rs`. It returns `batch` when `!interactive`, otherwise `verdict_label`. Call it from the Models list. `git add` includes `src/cluster/mod.rs`.

---

### Task 10: Full library test

**Files:** none new.

- [ ] **Step 1: Run the suite**

Run: `cargo test --locked -- --test-threads=8`

Expected: PASS. This is the gate for the plan. Do not start a follow-up feature in this task.

- [ ] **Step 2: Fix only failures caused by the new fields**

`PeerNode` and `ControlPlaneState` literals in tests need the new serde defaults or explicit fields. A failure that asks for the 10% mmap discount is a Task 3 regression. Restore the full-weight assertion, not the discount.

- [ ] **Step 3: Commit if Step 2 changed files**

```bash
git add -u
git commit -m "test: update fixtures for appliance peer fields"
```

Skip the commit when Step 1 passed with a clean tree.
