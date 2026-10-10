//! Pure edge-appliance decisions: route stay, front-door pick, burst score, hub URL.
//!
//! Callers open GGUF files, run the fit ladder, and perform control-plane I/O.
//! [`select_front_door`] does not touch the filesystem.

use crate::cluster::{
    cold_start_scale, plan_fit_ladder, plan_moe_spawn, BatteryGate, BurstScore, LinkClass,
    MemoryPolicy, MoeCacheCap,
};
use crate::config::NexusConfig;
use crate::gguf::{GgufMetadata, KvCacheDtype};
use crate::preset::Preset;
use crate::store::ModelIndex;
use crate::sysinfo::{AccelerationBackend, SystemProfile};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, Once};
use std::time::{Duration, Instant};
use uuid::Uuid;

/// Why a prompt stayed on the local front door.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RouteStay {
    Tier1,
    Tier2,
    Fallback,
    NoInteractivePeer,
}

impl RouteStay {
    /// Operator-facing reason. `NoInteractivePeer` is the words `no interactive peer`.
    pub fn as_reason(self) -> &'static str {
        match self {
            Self::Tier1 => "tier1",
            Self::Tier2 => "tier2",
            Self::Fallback => "fallback",
            Self::NoInteractivePeer => "no interactive peer",
        }
    }
}

/// Where an appliance prompt goes before burst ranking.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApplianceDecision {
    Local { reason: RouteStay },
    Escalate { tag: String },
    Unavailable { tag: String },
}

/// One local GGUF the front-door picker can see. The caller fills the fit flags.
#[derive(Debug, Clone, Copy)]
pub struct FrontDoorFile<'a> {
    pub name: &'a str,
    pub weights_bytes: u64,
    pub preset_tags: &'a [String],
    pub fits_pin_policy: bool,
    pub fits_1024_q8: bool,
}

/// Status text appended when an upstream generation socket closes early.
pub const PEER_LOST_STATUS: &str = "peer lost";

/// Logged when section 10.2 finds no file the ladder accepts.
pub const FRONT_DOOR_MISS: &str = "appliance: no local model fits";

/// `general` and `chat`, plus any preset tags that are not already present.
pub fn front_door_tags(preset_tags: &[String]) -> Vec<String> {
    let mut tags = preset_tags.to_vec();
    for extra in ["general", "chat"] {
        if !tags.iter().any(|tag| tag.eq_ignore_ascii_case(extra)) {
            tags.push(extra.to_string());
        }
    }
    tags
}

/// Apply the section 10.3 order. Ranking is the caller's job after `Escalate`.
pub fn decide_appliance(
    local_tags: &[String],
    local_loaded: bool,
    tier1: Option<&str>,
    tier2: Option<Result<&str, ()>>,
) -> ApplianceDecision {
    let carries = |tag: &str| local_tags.iter().any(|have| have.eq_ignore_ascii_case(tag));
    if let Some(tag) = tier1 {
        return if carries(tag) {
            ApplianceDecision::Local {
                reason: RouteStay::Tier1,
            }
        } else {
            ApplianceDecision::Escalate {
                tag: tag.to_string(),
            }
        };
    }
    if let Some(tier2) = tier2 {
        return match tier2 {
            Ok(tag) if carries(tag) => ApplianceDecision::Local {
                reason: RouteStay::Tier2,
            },
            Ok(tag) => ApplianceDecision::Escalate {
                tag: tag.to_string(),
            },
            Err(()) if local_loaded => ApplianceDecision::Local {
                reason: RouteStay::Fallback,
            },
            Err(()) => ApplianceDecision::Unavailable {
                tag: "fallback".to_string(),
            },
        };
    }
    if local_loaded {
        ApplianceDecision::Local {
            reason: RouteStay::Tier1,
        }
    } else {
        ApplianceDecision::Unavailable {
            tag: "general".to_string(),
        }
    }
}

/// `label; link=<local|usb|wifi>; model=<name>; route=<tag>`.
pub fn format_route_header(label: &str, link: &str, model: &str, route: &str) -> String {
    format!("{label}; link={link}; model={model}; route={route}")
}

/// Status line: label, model, link word, rounded tok/s, then `extra` after ` · `.
pub fn format_route_status(
    label: &str,
    model: &str,
    link: &str,
    tok_s: f32,
    extra: Option<&str>,
) -> String {
    let mut line = format!("{label} · {model} · {link} · {tok_s:.0} tok/s");
    if let Some(extra) = extra {
        line.push_str(" · ");
        line.push_str(extra);
    }
    line
}

/// Header `link=` token. Display words for the status line are separate.
pub fn link_header_token(link: LinkClass) -> &'static str {
    match link {
        LinkClass::Local => "local",
        LinkClass::Usb => "usb",
        LinkClass::Wifi => "wifi",
    }
}

/// Status-line link word (`local`, `USB`, `Wi-Fi`).
pub fn link_status_word(link: LinkClass) -> &'static str {
    match link {
        LinkClass::Local => "local",
        LinkClass::Usb => "USB",
        LinkClass::Wifi => "Wi-Fi",
    }
}

/// Pinned `default_host` wins. Otherwise the local gateway when both flags are on.
/// `None` means the caller keeps today's discovery and USB client fallback.
pub fn hub_client_base_url(
    default_host: Option<&str>,
    appliance_enabled: bool,
    gateway_enabled: bool,
    gateway_port: u16,
) -> Option<String> {
    if let Some(host) = default_host {
        if !host.trim().is_empty() {
            return Some(host.to_string());
        }
    }
    if appliance_enabled && gateway_enabled {
        return Some(format!("http://127.0.0.1:{gateway_port}"));
    }
    None
}

/// After a local or remote load. Appliance plus gateway, with no pinned host,
/// keeps the hub on the gateway and on an id that enters the appliance path.
pub fn hub_chat_target_after_load(
    inference_endpoint: &str,
    loaded_name: &str,
    default_host: Option<&str>,
    appliance_enabled: bool,
    gateway_enabled: bool,
    gateway_port: u16,
) -> (String, String) {
    let pinned = default_host.map(str::trim).filter(|host| !host.is_empty());
    if appliance_enabled && gateway_enabled && pinned.is_none() {
        return (
            format!("http://127.0.0.1:{gateway_port}"),
            "default".to_string(),
        );
    }
    (inference_endpoint.to_string(), loaded_name.to_string())
}

/// OpenAI model id for a tier-2 classify call. Llama-server's alias is the file stem.
pub fn tier2_model_id(loaded_model: &str) -> String {
    let name = loaded_model.trim();
    if name.is_empty() || name.eq_ignore_ascii_case("local") {
        return "orchestrator".to_string();
    }
    Path::new(name)
        .file_stem()
        .and_then(|stem| stem.to_str())
        .filter(|stem| !stem.is_empty())
        .unwrap_or(name)
        .to_string()
}

/// How long to wait for `{digest}.gguf` to show up in the peer catalog.
pub const PANTRY_CATALOG_WAIT: Duration = Duration::from_secs(15);
/// Pause between catalog polls. The wait above is the bound.
pub const PANTRY_CATALOG_INTERVAL: Duration = Duration::from_millis(400);

/// Device port for `adb reverse` of the host control plane.
/// Never one of the phone's Nexus listen ports.
pub fn pantry_reverse_device_port(device_api: u16, device_control: u16, device_rpc: u16) -> u16 {
    const BASE: u16 = 28180;
    let avoid = [device_api, device_control, device_rpc];
    let mut port = BASE;
    loop {
        if !avoid.contains(&port) {
            return port;
        }
        match port.checked_add(1) {
            Some(next) => port = next,
            None => return BASE,
        }
    }
}

/// Base URL the peer GETs. Wi-Fi uses the host LAN address. USB uses the
/// reversed device port on the phone's loopback, not the Mac control port.
pub fn pantry_source_base(
    link: LinkClass,
    lan_ip: &str,
    mac_control_port: u16,
    usb_device_port: u16,
) -> Result<String, ()> {
    match link {
        LinkClass::Usb => {
            if usb_device_port == 0 {
                return Err(());
            }
            Ok(format!("http://127.0.0.1:{usb_device_port}"))
        }
        LinkClass::Wifi | LinkClass::Local => {
            let ip = lan_ip.trim();
            if ip.is_empty() || ip == "127.0.0.1" || ip == "::1" || ip == "0.0.0.0" || ip == "::" {
                return Err(());
            }
            Ok(format!("http://{ip}:{mac_control_port}"))
        }
    }
}

/// True when the peer catalog already has this digest or the written file name.
pub fn catalog_lists_digest<'a, I>(entries: I, digest: &str) -> bool
where
    I: IntoIterator<Item = (&'a str, &'a str)>,
{
    let digest = digest.trim();
    if digest.is_empty() {
        return false;
    }
    let written = format!("{}.gguf", digest.to_lowercase());
    entries.into_iter().any(|(filename, entry_digest)| {
        (!entry_digest.is_empty() && entry_digest.eq_ignore_ascii_case(digest))
            || filename.eq_ignore_ascii_case(&written)
    })
}

/// Load name after a pantry fetch. `accepted` does not select `source_filename`.
/// The name is `{digest}.gguf` only once the catalog lists that digest.
pub fn pantry_load_name(
    digest: &str,
    source_filename: &str,
    accepted: bool,
    catalog_lists: bool,
) -> Option<String> {
    let digest = digest.trim().to_lowercase();
    if digest.is_empty() || !catalog_lists {
        let _queued_source = (accepted, source_filename);
        return None;
    }
    let _source = source_filename;
    Some(format!("{digest}.gguf"))
}

/// Burst peers only. The front door never goes idle from this check.
pub fn burst_idle_elapsed(idle_for: Duration, burst_idle_secs: u64, is_front_door: bool) -> bool {
    !is_front_door && idle_for >= Duration::from_secs(burst_idle_secs)
}

/// Thousandths of a token/s. Negative rates clamp to zero before the cast.
pub fn score_millis(tok_s: f32) -> u32 {
    (tok_s.max(0.0) * 1000.0) as u32
}

/// Section 10.2. `pin` may be a file name or a path whose final component is the name.
pub fn select_front_door<'a>(pin: &str, files: &'a [FrontDoorFile<'a>]) -> Option<&'a str> {
    let pin = pin.trim();
    if !pin.is_empty() {
        if let Some(file) = files
            .iter()
            .find(|file| file.fits_pin_policy && pin_names(pin, file.name))
        {
            return Some(file.name);
        }
    }
    if let Some(file) = files
        .iter()
        .filter(|file| file.fits_pin_policy && has_router_tag(file.preset_tags))
        .min_by_key(|file| file.weights_bytes)
    {
        return Some(file.name);
    }
    files
        .iter()
        .filter(|file| file.fits_1024_q8)
        .min_by_key(|file| file.weights_bytes)
        .map(|file| file.name)
}

/// One burst row. `Throttle` integer-divides millis by 2. A missing plan,
/// a batch-only plan, thermal above 75, or `Decline` clears `eligible`.
pub fn score_burst_candidate(
    name: impl Into<String>,
    link: LinkClass,
    predicted_tok_s: Option<f32>,
    interactive: bool,
    thermal_index: u8,
    battery_percent: Option<u8>,
    battery_floor: u8,
    battery_action: &str,
) -> BurstScore {
    let gate = crate::cluster::battery_gate(battery_percent, battery_floor, battery_action);
    let thermal_ok = crate::cluster::thermal_eligible(thermal_index);
    let eligible =
        predicted_tok_s.is_some() && interactive && thermal_ok && gate != BatteryGate::Decline;
    let mut millis = predicted_tok_s.map(score_millis).unwrap_or(0);
    if gate == BatteryGate::Throttle {
        millis /= 2;
    }
    BurstScore {
        name: name.into(),
        link,
        score_millis: millis,
        eligible,
    }
}

/// Cold-start tok/s. A positive measurement skips the size scale and quant boost.
pub fn cold_predict_tok_s(
    backend: AccelerationBackend,
    weights_bytes: u64,
    thermal: u8,
    quant: Option<&str>,
    measured: Option<f32>,
) -> f32 {
    let measured = measured.filter(|value| *value > 0.0);
    let weights_gib = weights_bytes as f32 / (1024.0 * 1024.0 * 1024.0);
    let base = if let Some(measured) = measured {
        measured
    } else {
        let prior = match backend {
            AccelerationBackend::Vulkan => 28.0,
            AccelerationBackend::ArmCpuDotProd => 12.0,
            AccelerationBackend::X86Baseline => 4.0,
            AccelerationBackend::GenericCpu => 6.0,
        };
        prior * cold_start_scale(weights_gib, false)
    };
    let quant_boost = if measured.is_some() {
        1.0
    } else {
        match quant.unwrap_or("") {
            label if label.contains("Q4") => 1.15,
            label if label.contains("Q5") => 1.05,
            label if label.contains("Q8") || label.contains("F16") => 0.85,
            _ => 1.0,
        }
    };
    let thermal_pen = if thermal > 75 {
        0.6
    } else if thermal > 50 {
        0.85
    } else {
        1.0
    };
    (base * quant_boost * thermal_pen).max(0.1)
}

/// Presets on disk, plus the built-in coder and general personas when unnamed.
pub fn load_presets(dir: &Path) -> Vec<Preset> {
    let mut presets = Vec::new();
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            let yaml = path
                .extension()
                .and_then(|ext| ext.to_str())
                .is_some_and(|ext| ext == "yaml" || ext == "yml");
            if yaml {
                if let Ok(preset) = Preset::load_from_file(&path) {
                    presets.push(preset);
                }
            }
        }
    }
    if !presets
        .iter()
        .any(|preset| preset.name.eq_ignore_ascii_case("coder"))
    {
        presets.push(Preset::coder());
    }
    if !presets
        .iter()
        .any(|preset| preset.name.eq_ignore_ascii_case("general"))
    {
        presets.push(Preset::general());
    }
    presets
}

/// Tags from presets whose name or tag is a filename or `general.name` component.
pub fn preset_tags_for_file(
    filename: &str,
    model_name: Option<&str>,
    presets: &[Preset],
) -> Vec<String> {
    let mut parts = components(filename);
    if let Some(name) = model_name {
        parts.extend(components(name));
    }
    let mut tags: Vec<String> = Vec::new();
    for preset in presets {
        let name_hit = parts
            .iter()
            .any(|part| part.eq_ignore_ascii_case(&preset.name));
        let tag_hit = preset
            .tags
            .iter()
            .any(|tag| parts.iter().any(|part| part.eq_ignore_ascii_case(tag)));
        if name_hit || tag_hit {
            for tag in &preset.tags {
                if !tags.iter().any(|have| have.eq_ignore_ascii_case(tag)) {
                    tags.push(tag.clone());
                }
            }
        }
    }
    tags
}

#[derive(Default)]
struct BurstInner {
    seen: HashMap<Uuid, Instant>,
    unloading: HashSet<Uuid>,
}

/// Last time a burst peer received an escalated request. The front door is never removed.
#[derive(Clone, Default)]
pub struct BurstActivity {
    inner: Arc<Mutex<BurstInner>>,
}

impl BurstActivity {
    pub fn touch(&self, id: Uuid) {
        if let Ok(mut inner) = self.inner.lock() {
            inner.seen.insert(id, Instant::now());
        }
    }

    #[cfg(test)]
    fn remember(&self, id: Uuid, at: Instant) {
        if let Ok(mut inner) = self.inner.lock() {
            inner.seen.insert(id, at);
        }
    }

    #[cfg(test)]
    fn forget(&self, id: Uuid) {
        if let Ok(mut inner) = self.inner.lock() {
            inner.seen.remove(&id);
        }
    }

    #[cfg(test)]
    fn is_tracked(&self, id: Uuid) -> bool {
        self.inner
            .lock()
            .map(|inner| inner.seen.contains_key(&id))
            .unwrap_or(false)
    }

    /// Idle burst peers. Does not remove them. `local_node` is the front door.
    pub fn idle_candidates(&self, burst_idle_secs: u64, local_node: Uuid) -> Vec<(Uuid, Instant)> {
        let Ok(inner) = self.inner.lock() else {
            return Vec::new();
        };
        inner
            .seen
            .iter()
            .filter_map(|(id, at)| {
                if *id == local_node || inner.unloading.contains(id) {
                    return None;
                }
                if burst_idle_elapsed(at.elapsed(), burst_idle_secs, false) {
                    Some((*id, *at))
                } else {
                    None
                }
            })
            .collect()
    }

    /// Same ids as [`Self::idle_candidates`]. Entries stay until [`Self::finish_idle_unload`].
    pub fn take_idle(&self, burst_idle_secs: u64, local_node: Uuid) -> Vec<Uuid> {
        self.idle_candidates(burst_idle_secs, local_node)
            .into_iter()
            .map(|(id, _)| id)
            .collect()
    }

    /// True when this task owns the in-flight unload for `id`.
    pub fn claim_unload(&self, id: Uuid) -> bool {
        let Ok(mut inner) = self.inner.lock() else {
            return false;
        };
        inner.unloading.insert(id)
    }

    /// The tracked instant is still `started` and still idle. A newer `touch` returns false.
    pub fn still_the_same_idle(
        &self,
        id: Uuid,
        started: Instant,
        burst_idle_secs: u64,
        local_node: Uuid,
    ) -> bool {
        if id == local_node {
            return false;
        }
        let Ok(inner) = self.inner.lock() else {
            return false;
        };
        match inner.seen.get(&id) {
            Some(at) if *at == started => burst_idle_elapsed(at.elapsed(), burst_idle_secs, false),
            _ => false,
        }
    }

    /// Re-check idle under the lock. A failed unload, or a missing peer, puts `id` back.
    /// A `touch` that landed during the HTTP call keeps the new timestamp.
    /// The local front door is never dropped.
    pub fn finish_idle_unload(
        &self,
        id: Uuid,
        started: Instant,
        unload_ok: bool,
        burst_idle_secs: u64,
        local_node: Uuid,
    ) {
        let Ok(mut inner) = self.inner.lock() else {
            return;
        };
        inner.unloading.remove(&id);
        if id == local_node {
            return;
        }
        match inner.seen.get(&id).copied() {
            Some(current) if current != started => {}
            Some(current) => {
                let still = burst_idle_elapsed(current.elapsed(), burst_idle_secs, false);
                if unload_ok && still {
                    inner.seen.remove(&id);
                }
            }
            None => {
                if !unload_ok {
                    inner.seen.insert(id, started);
                }
            }
        }
    }
}

/// SSE chunk with a status role so `peer lost` is not model context. One event, then `[DONE]`.
pub fn peer_lost_sse() -> String {
    let payload = serde_json::json!({
        "choices": [{
            "index": 0,
            "delta": {"role": "status", "content": PEER_LOST_STATUS}
        }]
    });
    format!("data: {payload}\n\ndata: [DONE]\n\n")
}

/// Fold one upstream chunk into `carry`. True once the window contains `[DONE]`.
pub fn observe_stream_done(carry: &mut String, chunk: &[u8]) -> bool {
    carry.push_str(&String::from_utf8_lossy(chunk));
    let found = carry.contains("[DONE]");
    const KEEP: usize = 16;
    if carry.len() > KEEP {
        let mut start = carry.len() - KEEP;
        while start < carry.len() && !carry.is_char_boundary(start) {
            start += 1;
        }
        carry.replace_range(..start, "");
    }
    found
}

/// Section 10.2 spawn plan. Does not start a server.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FrontDoorPlan {
    pub filename: String,
    pub model_path: PathBuf,
    pub tags: Vec<String>,
    pub context_size: usize,
    /// True when the file only fits the 1024 / Q8 KV rung.
    pub kv_q8: bool,
}

/// Section 10.2 choice, including the path and the rung that fit.
pub fn plan_front_door(cfg: &NexusConfig) -> Option<FrontDoorPlan> {
    let index = ModelIndex::reconcile_default(Path::new(&cfg.node.models_dir)).unwrap_or_default();
    let presets = load_presets(&cfg.node.presets_dir);
    let profile = SystemProfile::probe();
    let floor = cfg.inference.appliance.floor_tok_s();
    let operator = MemoryPolicy {
        max_ram_usage_percent: cfg.hardware.safety.max_ram_usage_percent,
        mmap: cfg.hardware.safety.mmap,
        mlock: cfg.hardware.safety.mlock,
        kv_dtype: KvCacheDtype::F16,
        context_size: 4096,
        batch_size: 512,
    };
    let q8 = MemoryPolicy {
        context_size: 1024,
        kv_dtype: KvCacheDtype::Q8_0,
        ..operator.clone()
    };
    let bench = crate::bench::BenchStore::load_default().ok();
    struct Owned {
        name: String,
        path: PathBuf,
        weights_bytes: u64,
        tags: Vec<String>,
        fits_pin_policy: bool,
        fits_1024_q8: bool,
    }
    let mut owned = Vec::new();
    for entry in &index.models {
        let Ok(meta) = GgufMetadata::open(&entry.path) else {
            continue;
        };
        let fits = |policy: &MemoryPolicy| -> bool {
            if meta.streamable_moe() {
                let key = crate::cluster::moe_model_key(&meta);
                return plan_moe_spawn(
                    &meta,
                    &profile,
                    &cfg.inference.moe,
                    policy.max_ram_usage_percent,
                    policy.context_size,
                    bench.as_ref(),
                    &key,
                    "local",
                    MoeCacheCap::None,
                )
                .is_some();
            }
            plan_fit_ladder(
                std::slice::from_ref(&meta),
                &profile,
                policy,
                floor,
                |_, _, _| floor,
            )
            .is_some()
        };
        owned.push(Owned {
            name: entry.filename.clone(),
            path: entry.path.clone(),
            weights_bytes: meta.weights_bytes().max(entry.size_bytes),
            tags: preset_tags_for_file(&entry.filename, entry.model_name.as_deref(), &presets),
            fits_pin_policy: fits(&operator),
            fits_1024_q8: fits(&q8),
        });
    }
    let views: Vec<FrontDoorFile<'_>> = owned
        .iter()
        .map(|file| FrontDoorFile {
            name: &file.name,
            weights_bytes: file.weights_bytes,
            preset_tags: &file.tags,
            fits_pin_policy: file.fits_pin_policy,
            fits_1024_q8: file.fits_1024_q8,
        })
        .collect();
    let name = select_front_door(&cfg.inference.appliance.orchestrator, &views)?;
    let file = owned.iter().find(|file| file.name == name)?;
    let pin_fit = file.fits_pin_policy;
    Some(FrontDoorPlan {
        filename: file.name.clone(),
        model_path: file.path.clone(),
        tags: front_door_tags(&file.tags),
        context_size: if pin_fit { 4096 } else { 1024 },
        kv_q8: !pin_fit,
    })
}

/// Section 10.2 name, if any file fits. Does not spawn a server.
pub fn resolve_front_door_name(cfg: &NexusConfig) -> Option<String> {
    plan_front_door(cfg).map(|plan| plan.filename)
}

/// Log the front-door choice once per process. `None` prints [`FRONT_DOOR_MISS`].
pub fn warn_front_door_once(cfg: &NexusConfig) {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| match resolve_front_door_name(cfg) {
        Some(name) => tracing::info!("appliance front door: {name}"),
        None => {
            tracing::warn!("{FRONT_DOOR_MISS}");
            eprintln!("{FRONT_DOOR_MISS}");
        }
    });
}

fn pin_names(pin: &str, name: &str) -> bool {
    if pin == name {
        return true;
    }
    Path::new(pin).file_name().and_then(|part| part.to_str()) == Some(name)
}

fn has_router_tag(tags: &[String]) -> bool {
    tags.iter().any(|tag| {
        let tag = tag.trim();
        tag.eq_ignore_ascii_case("orchestrator") || tag.eq_ignore_ascii_case("router")
    })
}

fn components(text: &str) -> Vec<String> {
    text.split(|ch: char| !ch.is_ascii_alphanumeric())
        .filter(|part| !part.is_empty())
        .map(|part| part.to_ascii_lowercase())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn general_stays_local_and_coder_escalates() {
        let tags = front_door_tags(&[]);
        assert!(tags.iter().any(|t| t == "general"));
        let stay = decide_appliance(&tags, true, Some("general"), None);
        assert!(matches!(
            stay,
            ApplianceDecision::Local {
                reason: RouteStay::Tier1
            }
        ));
        let go = decide_appliance(&tags, true, Some("coder"), None);
        assert_eq!(
            go,
            ApplianceDecision::Escalate {
                tag: "coder".into()
            }
        );
    }

    #[test]
    fn header_and_status_line() {
        assert_eq!(
            format_route_header("phone-a", "usb", "coder-7b", "coder"),
            "phone-a; link=usb; model=coder-7b; route=coder"
        );
        let line = format_route_status(
            "phone-a",
            "coder-7b",
            "USB",
            18.2,
            Some("escalated, route=coder"),
        );
        assert_eq!(
            line,
            "phone-a · coder-7b · USB · 18 tok/s · escalated, route=coder"
        );
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
            FrontDoorFile {
                name: "big.gguf",
                weights_bytes: 5_000,
                preset_tags: &[],
                fits_pin_policy: true,
                fits_1024_q8: true,
            },
            FrontDoorFile {
                name: "orch.gguf",
                weights_bytes: 2_000,
                preset_tags: std::slice::from_ref(&orch),
                fits_pin_policy: true,
                fits_1024_q8: true,
            },
            FrontDoorFile {
                name: "tiny.gguf",
                weights_bytes: 800,
                preset_tags: &[],
                fits_pin_policy: false,
                fits_1024_q8: true,
            },
        ];
        assert_eq!(select_front_door("big.gguf", &tagged), Some("big.gguf"));
        assert_eq!(
            select_front_door("missing.gguf", &tagged),
            Some("orch.gguf")
        );
        assert_eq!(select_front_door("", &tagged), Some("orch.gguf"));

        let untagged = [
            FrontDoorFile {
                name: "big.gguf",
                weights_bytes: 5_000,
                preset_tags: &[],
                fits_pin_policy: false,
                fits_1024_q8: true,
            },
            FrontDoorFile {
                name: "tiny.gguf",
                weights_bytes: 800,
                preset_tags: &[],
                fits_pin_policy: false,
                fits_1024_q8: true,
            },
        ];
        assert_eq!(select_front_door("", &untagged), Some("tiny.gguf"));
        assert_eq!(select_front_door("", &[]), None);
    }

    #[test]
    fn appliance_gateway_keeps_hub_client_after_load() {
        let local = hub_chat_target_after_load(
            "http://127.0.0.1:8080",
            "front.gguf",
            None,
            true,
            true,
            8090,
        );
        assert_eq!(local.0, "http://127.0.0.1:8090");
        assert_eq!(local.1, "default");
        let remote = hub_chat_target_after_load(
            "http://10.1.2.3:8080",
            "coder-7b.gguf",
            None,
            true,
            true,
            8090,
        );
        assert_eq!(remote.0, "http://127.0.0.1:8090");
        assert_eq!(remote.1, "default");
        assert!(!remote.0.contains("10.1.2.3"));
        let pinned = hub_chat_target_after_load(
            "http://10.1.2.3:8080",
            "coder-7b.gguf",
            Some("http://10.0.0.9:8080"),
            true,
            true,
            8090,
        );
        assert_eq!(pinned.0, "http://10.1.2.3:8080");
        assert_eq!(pinned.1, "coder-7b.gguf");
    }

    #[test]
    fn accepted_fetch_does_not_choose_source_filename() {
        let digest = "abCDef";
        let source = "coder-7b.gguf";
        assert_eq!(pantry_load_name(digest, source, true, false), None);
        assert_eq!(
            pantry_load_name(digest, source, true, true).as_deref(),
            Some("abcdef.gguf")
        );
        assert_ne!(
            pantry_load_name(digest, source, true, true).as_deref(),
            Some(source)
        );
        let rows = [("coder-7b.gguf", "abcdef")];
        assert!(catalog_lists_digest(rows, digest));
        assert!(!catalog_lists_digest([("coder-7b.gguf", "other")], digest));
        assert!(catalog_lists_digest([("abcdef.gguf", "")], digest));
    }

    #[test]
    fn failed_unload_leaves_the_burst_peer_tracked() {
        let activity = BurstActivity::default();
        let id = Uuid::new_v4();
        let local = Uuid::new_v4();
        let started = Instant::now() - Duration::from_secs(1_000);
        activity.remember(id, started);
        assert!(activity.claim_unload(id));
        activity.forget(id);
        activity.finish_idle_unload(id, started, false, 600, local);
        assert!(activity.is_tracked(id));

        activity.remember(id, started);
        activity.finish_idle_unload(id, started, true, 600, local);
        assert!(!activity.is_tracked(id));

        activity.remember(id, started);
        activity.touch(id);
        activity.finish_idle_unload(id, started, true, 600, local);
        assert!(activity.is_tracked(id));

        activity.remember(local, started);
        activity.finish_idle_unload(local, started, true, 600, local);
        assert!(activity.is_tracked(local));
        assert!(activity.idle_candidates(600, local).is_empty());
    }

    #[test]
    fn tier2_sends_loaded_alias_not_the_orchestrator_label() {
        assert_eq!(tier2_model_id("front-door.gguf"), "front-door");
        assert_ne!(tier2_model_id("front-door.gguf"), "orchestrator");
    }

    #[test]
    fn usb_pantry_source_skips_the_phone_listen_port() {
        let phone_control = 9998u16;
        let port = pantry_reverse_device_port(8080, phone_control, 50052);
        assert_ne!(port, phone_control);
        assert_ne!(port, 8080);
        assert_ne!(port, 50052);
        let argv = crate::tunnel::control_reverse_argv("serial-a", port, phone_control);
        assert_eq!(
            argv,
            vec![
                "-s".to_string(),
                "serial-a".to_string(),
                "reverse".to_string(),
                format!("tcp:{port}"),
                format!("tcp:{phone_control}"),
            ]
        );
        let wifi = pantry_source_base(LinkClass::Wifi, "10.0.0.4", phone_control, port).unwrap();
        assert_eq!(wifi, "http://10.0.0.4:9998");
        assert!(pantry_source_base(LinkClass::Wifi, "127.0.0.1", phone_control, port).is_err());
        let usb = pantry_source_base(LinkClass::Usb, "10.0.0.4", phone_control, port).unwrap();
        assert_eq!(usb, format!("http://127.0.0.1:{port}"));
    }
}
