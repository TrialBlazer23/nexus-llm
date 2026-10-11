use crate::cluster::LinkClass;
use crate::config::NexusConfig;
use crate::peer_registry::{apply_power_hint, ObservationSource, PeerLifecycle, PeerRegistry};
use crate::sysinfo::{AccelerationBackend, PowerSource, SystemProfile};
use crate::trust_auth::pairing_enforced;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};
use thiserror::Error;
use tokio::net::UdpSocket;
use tokio::sync::RwLock;
use tracing::{debug, error, info, trace, warn};
use uuid::Uuid;

pub const NEXUS_MDNS_SERVICE_TYPE: &str = "_nexus._tcp.local.";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackendHealth {
    Started,
    Healthy,
    Failed,
    Stopped,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServiceEndpoint {
    pub node_id: Uuid,
    pub cluster_id: Option<Uuid>,
    pub protocol_version: u16,
    pub role: NodeRole,
    pub capabilities: Vec<String>,
    pub addresses: Vec<SocketAddr>,
    pub api_port: u16,
    pub rpc_port: u16,
    /// HTTP control-plane port (`POST /nexus/control/v1/...`). Defaults to the
    /// local `network.control_port` when a peer is learned only via UDP beacon
    /// (beacon v1 has no on-wire control port; see AGENT_LEARNINGS).
    pub control_port: u16,
    /// Human-readable name from mDNS TXT `name=` (empty when unknown / UDP-only).
    pub display_name: String,
    /// mDNS TXT `bat`. Missing or non-integer values are `None` and do not fail parse.
    pub battery_percent: Option<u8>,
    /// mDNS TXT `pwr` (`ac`, `battery`, `unknown`). A missing key is [`PowerSource::Unknown`].
    pub power_source: PowerSource,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DiscoveryEvent {
    ServiceFound(ServiceEndpoint),
    ServiceUpdated(ServiceEndpoint),
    ServiceRemoved {
        node_id: Uuid,
    },
    BackendHealth {
        backend: &'static str,
        health: BackendHealth,
    },
}

pub const BEACON_MAGIC: u32 = 0x4E585553; // "NXUS"
pub const BEACON_VERSION: u8 = 0x01;
pub const BEACON_PACKET_SIZE: usize = 64;

#[derive(Error, Debug)]
pub enum DiscoveryError {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error("Packet size mismatch: expected {expected} bytes, got {actual}")]
    PacketSizeMismatch { expected: usize, actual: usize },

    #[error("Invalid beacon magic: 0x{0:08X} (expected 0x{BEACON_MAGIC:08X})")]
    InvalidMagic(u32),

    #[error("Unsupported beacon version: {0} (expected {BEACON_VERSION})")]
    UnsupportedVersion(u8),

    #[error("CRC-16-CCITT checksum mismatch: expected 0x{expected:04X}, computed 0x{actual:04X}")]
    ChecksumMismatch { expected: u16, actual: u16 },

    #[error("No host peer discovered on the network")]
    NoHostFound,
}

/// Bitmask representation of node roles.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct NodeRole(pub u8);

impl NodeRole {
    pub const HOST: Self = Self(0x01);
    pub const CLIENT: Self = Self(0x02);
    pub const STANDALONE: Self = Self(0x04);

    pub fn is_host(&self) -> bool {
        (self.0 & Self::HOST.0) != 0
    }

    pub fn is_client(&self) -> bool {
        (self.0 & Self::CLIENT.0) != 0
    }

    pub fn from_str_role(s: &str) -> Self {
        match s.to_lowercase().as_str() {
            "host" => Self::HOST,
            "client" => Self::CLIENT,
            _ => Self::STANDALONE,
        }
    }
}

/// Bitmask representation of node status flags.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct StatusFlags(pub u16);

impl StatusFlags {
    pub const READY: Self = Self(0x0001);
    pub const INFERRING: Self = Self(0x0002);
    pub const VULKAN_ACTIVE: Self = Self(0x0004);
    pub const THERMAL_THROTTLE: Self = Self(0x0008);
    pub const RPC_READY: Self = Self(0x0010);

    pub fn is_ready(&self) -> bool {
        (self.0 & Self::READY.0) != 0
    }

    pub fn is_inferring(&self) -> bool {
        (self.0 & Self::INFERRING.0) != 0
    }

    pub fn is_vulkan_active(&self) -> bool {
        (self.0 & Self::VULKAN_ACTIVE.0) != 0
    }

    pub fn is_thermal_throttled(&self) -> bool {
        (self.0 & Self::THERMAL_THROTTLE.0) != 0
    }

    pub fn is_rpc_ready(&self) -> bool {
        (self.0 & Self::RPC_READY.0) != 0
    }
}

/// Represents the decoded 64-byte beacon payload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BeaconPacket {
    pub magic: u32,
    pub version: u8,
    pub role: NodeRole,
    pub status: StatusFlags,
    pub uuid: Uuid,
    pub api_port: u16,
    pub rpc_port: u16,
    pub total_ram_mb: u32,
    pub free_ram_mb: u32,
    pub backend: AccelerationBackend,
    pub thermal_index: u8,
    pub active_model: String,
}

impl BeaconPacket {
    /// Encode the beacon struct into a 64-byte fixed binary packet with big-endian fields
    /// and CRC-16-CCITT computed across the first 62 bytes.
    pub fn encode(&self) -> [u8; BEACON_PACKET_SIZE] {
        let mut buf = [0u8; BEACON_PACKET_SIZE];

        // 0x00 - 0x03: Magic (uint32 BE)
        buf[0..4].copy_from_slice(&self.magic.to_be_bytes());

        // 0x04: Version (uint8)
        buf[4] = self.version;

        // 0x05: Role (uint8 bitmask)
        buf[5] = self.role.0;

        // 0x06 - 0x07: Status Flags (uint16 BE)
        buf[6..8].copy_from_slice(&self.status.0.to_be_bytes());

        // 0x08 - 0x17: Node UUID (16 bytes)
        buf[8..24].copy_from_slice(self.uuid.as_bytes());

        // 0x18 - 0x19: API Port (uint16 BE)
        buf[24..26].copy_from_slice(&self.api_port.to_be_bytes());

        // 0x1A - 0x1B: RPC Port (uint16 BE, previously reserved)
        buf[26..28].copy_from_slice(&self.rpc_port.to_be_bytes());

        // 0x1C - 0x1F: Total RAM MB (uint32 BE)
        buf[28..32].copy_from_slice(&self.total_ram_mb.to_be_bytes());

        // 0x20 - 0x23: Free RAM MB (uint32 BE)
        buf[32..36].copy_from_slice(&self.free_ram_mb.to_be_bytes());

        // 0x24: Acceleration Tier (uint8)
        buf[36] = match self.backend {
            AccelerationBackend::Vulkan => 0x01,
            AccelerationBackend::ArmCpuDotProd => 0x02,
            AccelerationBackend::X86Baseline => 0x03,
            AccelerationBackend::GenericCpu => 0x00,
        };

        // 0x25: Thermal Index (uint8, 0 to 100)
        buf[37] = self.thermal_index;

        // 0x26 - 0x3D: Active Model (24-byte null-padded ASCII)
        let model_bytes = self.active_model.as_bytes();
        let copy_len = model_bytes.len().min(24);
        buf[38..38 + copy_len].copy_from_slice(&model_bytes[..copy_len]);

        // 0x3E - 0x3F: CRC-16-CCITT computed over bytes 0x00..0x3D (first 62 bytes)
        let checksum = compute_crc16(&buf[0..62]);
        buf[62..64].copy_from_slice(&checksum.to_be_bytes());

        buf
    }

    /// Decode and validate a raw 64-byte buffer into a `BeaconPacket`.
    pub fn decode(buf: &[u8]) -> Result<Self, DiscoveryError> {
        if buf.len() != BEACON_PACKET_SIZE {
            return Err(DiscoveryError::PacketSizeMismatch {
                expected: BEACON_PACKET_SIZE,
                actual: buf.len(),
            });
        }

        // Validate CRC-16-CCITT checksum over first 62 bytes
        let expected_checksum = u16::from_be_bytes([buf[62], buf[63]]);
        let actual_checksum = compute_crc16(&buf[0..62]);
        if expected_checksum != actual_checksum {
            return Err(DiscoveryError::ChecksumMismatch {
                expected: expected_checksum,
                actual: actual_checksum,
            });
        }

        // 0x00 - 0x03: Magic
        let magic = u32::from_be_bytes([buf[0], buf[1], buf[2], buf[3]]);
        if magic != BEACON_MAGIC {
            return Err(DiscoveryError::InvalidMagic(magic));
        }

        // 0x04: Version
        let version = buf[4];
        if version != BEACON_VERSION {
            return Err(DiscoveryError::UnsupportedVersion(version));
        }

        // 0x05: Role
        let role = NodeRole(buf[5]);

        // 0x06 - 0x07: Status
        let status = StatusFlags(u16::from_be_bytes([buf[6], buf[7]]));

        // 0x08 - 0x17: UUID
        let uuid = Uuid::from_slice(&buf[8..24]).unwrap_or_default();

        // 0x18 - 0x19: API Port
        let api_port = u16::from_be_bytes([buf[24], buf[25]]);

        // 0x1A - 0x1B: RPC Port
        let rpc_port = u16::from_be_bytes([buf[26], buf[27]]);

        // 0x1C - 0x1F: Total RAM
        let total_ram_mb = u32::from_be_bytes([buf[28], buf[29], buf[30], buf[31]]);

        // 0x20 - 0x23: Free RAM
        let free_ram_mb = u32::from_be_bytes([buf[32], buf[33], buf[34], buf[35]]);

        // 0x24: Acceleration Tier
        let backend = match buf[36] {
            0x01 => AccelerationBackend::Vulkan,
            0x02 => AccelerationBackend::ArmCpuDotProd,
            0x03 => AccelerationBackend::X86Baseline,
            _ => AccelerationBackend::GenericCpu,
        };

        // 0x25: Thermal Index
        let thermal_index = buf[37];

        // 0x26 - 0x3D: Active Model
        let model_slice = &buf[38..62];
        let null_pos = model_slice
            .iter()
            .position(|&b| b == 0)
            .unwrap_or(model_slice.len());
        let active_model = String::from_utf8_lossy(&model_slice[..null_pos]).to_string();

        Ok(Self {
            magic,
            version,
            role,
            status,
            uuid,
            api_port,
            rpc_port,
            total_ram_mb,
            free_ram_mb,
            backend,
            thermal_index,
            active_model,
        })
    }
}

/// Compute standard CRC-16-CCITT (poly 0x1021, init 0xFFFF).
pub fn compute_crc16(data: &[u8]) -> u16 {
    crc16::State::<crc16::CCITT_FALSE>::calculate(data)
}

/// Discovered peer representation in local peer cache.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PeerNode {
    pub uuid: Uuid,
    pub addr: SocketAddr,
    pub role: NodeRole,
    pub status: StatusFlags,
    pub api_port: u16,
    pub rpc_port: u16,
    #[serde(default = "default_peer_control_port")]
    pub control_port: u16,
    pub total_ram_mb: u32,
    pub free_ram_mb: u32,
    pub backend: AccelerationBackend,
    pub thermal_index: u8,
    pub active_model: String,
    /// From mDNS TXT `name=` when available; empty for UDP-only peers.
    #[serde(default)]
    pub display_name: String,
    /// Peer can host BigMoeOnEdge flash-streaming MoE sessions.
    #[serde(default)]
    pub moe_stream: bool,
    /// Optional advertised MoE expert-cache ceiling (MiB); 0 = unknown.
    #[serde(default)]
    pub moe_cache_ceil_mb: u32,
    /// Live path. Wi-Fi until [`Self::apply_usb_probe`]; disk snapshots default to Wi-Fi.
    #[serde(default)]
    pub link_class: LinkClass,
    /// ADB serial for the live USB forward. Not a routing key by itself.
    #[serde(default)]
    pub usb_serial: Option<String>,
    /// `http://127.0.0.1:<host_api>` while the USB forward is the live link.
    #[serde(default)]
    pub usb_api_endpoint: Option<String>,
    /// `http://127.0.0.1:<host_control>` while the USB forward is the live link.
    #[serde(default)]
    pub usb_control_endpoint: Option<String>,
    /// Last known battery percent. Beacons do not carry it; mDNS TXT `bat` does.
    #[serde(default)]
    pub battery_percent: Option<u8>,
    /// Last known power source. Missing TXT and old snapshots are [`PowerSource::Unknown`].
    #[serde(default)]
    pub power_source: PowerSource,
    #[serde(skip, default = "Instant::now")]
    pub last_seen: Instant,
}

fn default_peer_control_port() -> u16 {
    9998
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RpcSelectionPolicy {
    pub max_thermal_index: u8,
    pub max_allocatable_mb: u64,
    pub require_pairing: bool,
    pub protocol_version: u16,
}

impl Default for RpcSelectionPolicy {
    fn default() -> Self {
        Self {
            max_thermal_index: 75,
            // No global 1800 ceiling — use peer advertised free RAM (Phase 11).
            max_allocatable_mb: u64::MAX,
            require_pairing: false,
            protocol_version: crate::control_plane::CONTROL_PLANE_VERSION,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RpcCandidate {
    pub peer: PeerNode,
    pub allocatable_mb: u64,
    pub rationale: String,
}

impl PeerNode {
    pub fn api_endpoint(&self) -> String {
        match (self.link_class, &self.usb_api_endpoint) {
            (LinkClass::Usb, Some(endpoint)) => endpoint.clone(),
            _ => format!("http://{}:{}", self.addr.ip(), self.api_port),
        }
    }

    pub fn control_endpoint(&self) -> String {
        match (self.link_class, &self.usb_control_endpoint) {
            (LinkClass::Usb, Some(endpoint)) => endpoint.clone(),
            _ => format!("http://{}:{}", self.addr.ip(), self.control_port),
        }
    }

    /// Point this UUID at a localhost ADB forward. `addr` stays the Wi-Fi socket.
    pub fn apply_usb_probe(&mut self, serial: &str, host_api: u16, host_control: u16) {
        self.link_class = LinkClass::Usb;
        self.usb_serial = Some(serial.to_string());
        self.usb_api_endpoint = Some(format!("http://127.0.0.1:{host_api}"));
        self.usb_control_endpoint = Some(format!("http://127.0.0.1:{host_control}"));
    }

    /// Drop the USB forward so endpoints fall back to the Wi-Fi address.
    pub fn clear_usb(&mut self) {
        self.link_class = LinkClass::Wifi;
        self.usb_serial = None;
        self.usb_api_endpoint = None;
        self.usb_control_endpoint = None;
    }
}

/// Point peers at localhost forwards, or clear them, matching cache node ids only.
pub fn sync_peer_usb_links(
    peers: &mut HashMap<Uuid, PeerNode>,
    cache: &crate::adb_links::AdbLinkCache,
    forwards: &[crate::adb_links::HostForward],
) {
    use std::collections::HashSet;
    let present: HashSet<&str> = forwards
        .iter()
        .map(|forward| forward.serial.as_str())
        .collect();
    let mut applied = HashSet::new();
    for forward in forwards {
        let Some(link) = cache
            .links
            .iter()
            .find(|link| link.serial == forward.serial)
        else {
            continue;
        };
        if let Some(peer) = peers.get_mut(&link.node_id) {
            peer.apply_usb_probe(&forward.serial, forward.host_api, forward.host_control);
            applied.insert(link.node_id);
        }
    }
    let gone: Vec<Uuid> = cache
        .links
        .iter()
        .filter(|link| !present.contains(link.serial.as_str()) && !applied.contains(&link.node_id))
        .map(|link| link.node_id)
        .collect();
    for id in gone {
        if let Some(peer) = peers.get_mut(&id) {
            peer.clear_usb();
        }
    }
    let stale: Vec<Uuid> = peers
        .iter()
        .filter_map(|(id, peer)| {
            let serial = peer.usb_serial.as_deref()?;
            if present.contains(serial) {
                None
            } else {
                Some(*id)
            }
        })
        .collect();
    for id in stale {
        if let Some(peer) = peers.get_mut(&id) {
            peer.clear_usb();
        }
    }
}

/// One serial whose control forward answered `POST /nexus/control/v1/state`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UsbProbeHit {
    pub serial: String,
    pub host_api: u16,
    pub host_control: u16,
    pub device_api: u16,
    pub device_control: u16,
    pub device_rpc: u16,
    pub state: crate::control_plane::ControlPlaneState,
}

/// Argv lists to run, peers that were USB-only and are gone, and the serials that won.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UsbFold {
    pub teardown: Vec<Vec<String>>,
    pub dropped: Vec<Uuid>,
    pub winner_serials: Vec<String>,
    pub warnings: Vec<String>,
}

/// Stored device ports, then the local defaults when those differ. One attempt when they match.
pub fn device_port_attempts(
    stored: Option<(u16, u16, u16)>,
    default_api: u16,
    default_control: u16,
    default_rpc: u16,
) -> Vec<(u16, u16, u16)> {
    let defaults = (default_api, default_control, default_rpc);
    match stored {
        Some(stored) if stored != defaults => vec![stored, defaults],
        Some(stored) => vec![stored],
        None => vec![defaults],
    }
}

/// USB forward is the live link. Beacon age does not retire it.
pub fn usb_link_healthy(peer: &PeerNode) -> bool {
    peer.link_class == LinkClass::Usb
        && peer.usb_api_endpoint.is_some()
        && peer.usb_control_endpoint.is_some()
}

/// Keep a peer whose beacon is fresh, or whose USB forward is still healthy.
pub fn peer_still_active(peer: &PeerNode, timeout: Duration, now: Instant) -> bool {
    usb_link_healthy(peer) || now.saturating_duration_since(peer.last_seen) <= timeout
}

fn addr_is_usb_only(addr: SocketAddr) -> bool {
    addr.ip().is_unspecified() || addr.ip().is_loopback()
}

fn push_remove(teardown: &mut Vec<Vec<String>>, serial: &str, port: u16) {
    let argv = crate::adb_links::remove_forward_argv(serial, port);
    if !teardown.contains(&argv) {
        teardown.push(argv);
    }
}

fn mb_u32(mb: u64) -> u32 {
    u32::try_from(mb).unwrap_or(u32::MAX)
}

// USB probe merge reads `state.backend`, not capability strings.
#[allow(dead_code)]
fn backend_hint(capabilities: &[String]) -> Option<AccelerationBackend> {
    capabilities
        .iter()
        .find_map(|cap| match cap.to_ascii_lowercase().as_str() {
            "vulkan" => Some(AccelerationBackend::Vulkan),
            "arm_dotprod" | "arm-dotprod" => Some(AccelerationBackend::ArmCpuDotProd),
            "x86_baseline" | "x86-baseline" => Some(AccelerationBackend::X86Baseline),
            _ => None,
        })
}

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

fn status_from_state(
    state: &crate::control_plane::ControlPlaneState,
    previous: StatusFlags,
) -> StatusFlags {
    let mut bits = previous.0;
    if state.ready {
        bits |= StatusFlags::READY.0;
    } else {
        bits &= !StatusFlags::READY.0;
    }
    if state.inferring {
        bits |= StatusFlags::INFERRING.0;
    } else {
        bits &= !StatusFlags::INFERRING.0;
    }
    if state.rpc_ready {
        bits |= StatusFlags::RPC_READY.0;
    } else {
        bits &= !StatusFlags::RPC_READY.0;
    }
    if state
        .capabilities
        .iter()
        .any(|cap| cap.eq_ignore_ascii_case("vulkan"))
    {
        bits |= StatusFlags::VULKAN_ACTIVE.0;
    }
    StatusFlags(bits)
}

fn stale_seen() -> Instant {
    Instant::now()
        .checked_sub(Duration::from_secs(3_600))
        .unwrap_or_else(Instant::now)
}

fn apply_probe_hit(peers: &mut HashMap<Uuid, PeerNode>, hit: &UsbProbeHit, hub_ram_percent: u8) {
    let id = hit.state.node_id;
    if let Some(peer) = peers.get_mut(&id) {
        if !hit.state.display_name.trim().is_empty() {
            peer.display_name = hit.state.display_name.clone();
        }
        if hit
            .state
            .capabilities
            .iter()
            .any(|cap| cap.eq_ignore_ascii_case("moe_stream"))
        {
            peer.moe_stream = true;
        }
        peer.battery_percent = hit.state.battery_percent;
        peer.power_source = hit.state.power_source;
        peer.role = hit.state.role;
        peer.status = status_from_state(&hit.state, peer.status);
        let free = advertised_free_ram(&hit.state, peer.free_ram_mb, hub_ram_percent);
        peer.free_ram_mb = free;
        peer.total_ram_mb = advertised_total_ram(&hit.state, peer.total_ram_mb, free);
        if let Some(backend) = advertised_backend(&hit.state) {
            peer.backend = backend;
            set_vulkan_flag(&mut peer.status, backend);
        }
        if let Some(model) = &hit.state.active_model {
            peer.active_model = model.clone();
        }
        peer.apply_usb_probe(&hit.serial, hit.host_api, hit.host_control);
        return;
    }

    let free = advertised_free_ram(&hit.state, 0, hub_ram_percent);
    let total = advertised_total_ram(&hit.state, 0, free);
    let backend = advertised_backend(&hit.state).unwrap_or(AccelerationBackend::GenericCpu);
    let mut status = status_from_state(&hit.state, StatusFlags(0));
    set_vulkan_flag(&mut status, backend);
    let rpc_port = if hit.state.rpc_ready {
        hit.device_rpc
    } else {
        0
    };
    let mut peer = PeerNode {
        uuid: id,
        addr: SocketAddr::new(
            std::net::IpAddr::V4(std::net::Ipv4Addr::UNSPECIFIED),
            hit.device_api,
        ),
        role: hit.state.role,
        status,
        api_port: hit.device_api,
        rpc_port,
        control_port: hit.device_control,
        total_ram_mb: total,
        free_ram_mb: free,
        backend,
        thermal_index: 0,
        active_model: hit.state.active_model.clone().unwrap_or_default(),
        display_name: hit.state.display_name.clone(),
        moe_stream: hit
            .state
            .capabilities
            .iter()
            .any(|cap| cap.eq_ignore_ascii_case("moe_stream")),
        moe_cache_ceil_mb: 0,
        link_class: LinkClass::Wifi,
        usb_serial: None,
        usb_api_endpoint: None,
        usb_control_endpoint: None,
        battery_percent: hit.state.battery_percent,
        power_source: hit.state.power_source,
        last_seen: stale_seen(),
    };
    peer.apply_usb_probe(&hit.serial, hit.host_api, hit.host_control);
    peers.insert(id, peer);
}

/// Merge successful probes into peers and the serial cache. Does not spawn `adb`.
///
/// Hits are ordered by serial. The first serial for a UUID wins. Later serials are
/// torn down with `adb -s`. A serial missing from `present_serials` loses its cache
/// row, its forwards, and its USB endpoint. A real Wi-Fi address stays.
pub fn fold_usb_probes(
    peers: &mut HashMap<Uuid, PeerNode>,
    cache: &mut crate::adb_links::AdbLinkCache,
    hits: &[UsbProbeHit],
    present_serials: &[String],
    listed_forwards: &[(String, u16)],
    hub_ram_percent: u8,
) -> UsbFold {
    use std::collections::HashSet;

    let mut ordered: Vec<&UsbProbeHit> = hits.iter().collect();
    ordered.sort_by(|left, right| left.serial.as_bytes().cmp(right.serial.as_bytes()));

    let mut kept_serial: HashMap<Uuid, String> = HashMap::new();
    let mut teardown = Vec::new();
    let mut warnings = Vec::new();
    let mut winner_serials = Vec::new();

    for hit in ordered {
        if let Some(kept) = kept_serial.get(&hit.state.node_id).cloned() {
            let message = format!("usb duplicate uuid: kept {kept}, tore down {}", hit.serial);
            warn!("{message}");
            warnings.push(message);
            cache.upsert(crate::adb_links::AdbLinkRecord {
                serial: hit.serial.clone(),
                node_id: hit.state.node_id,
                device_api_port: hit.device_api,
                device_control_port: hit.device_control,
                device_rpc_port: hit.device_rpc,
            });
            push_remove(&mut teardown, &hit.serial, hit.host_api);
            push_remove(&mut teardown, &hit.serial, hit.host_control);
            continue;
        }
        kept_serial.insert(hit.state.node_id, hit.serial.clone());
        winner_serials.push(hit.serial.clone());
        cache.upsert(crate::adb_links::AdbLinkRecord {
            serial: hit.serial.clone(),
            node_id: hit.state.node_id,
            device_api_port: hit.device_api,
            device_control_port: hit.device_control,
            device_rpc_port: hit.device_rpc,
        });
        apply_probe_hit(peers, hit, hub_ram_percent);
    }

    cache.retain_present(present_serials);

    let present: HashSet<&str> = present_serials.iter().map(String::as_str).collect();
    let winners: HashSet<&str> = winner_serials.iter().map(String::as_str).collect();
    let stale_ids: Vec<Uuid> = peers
        .iter()
        .filter_map(|(id, peer)| {
            let serial = peer.usb_serial.as_deref()?;
            if winners.contains(serial) {
                None
            } else {
                Some(*id)
            }
        })
        .collect();
    let mut dropped = Vec::new();
    for id in stale_ids {
        let usb_only = {
            let Some(peer) = peers.get_mut(&id) else {
                continue;
            };
            let usb_only = addr_is_usb_only(peer.addr);
            peer.clear_usb();
            usb_only
        };
        if usb_only {
            peers.remove(&id);
            dropped.push(id);
        }
    }

    for (serial, port) in listed_forwards {
        if !present.contains(serial.as_str()) {
            push_remove(&mut teardown, serial, *port);
        }
    }

    UsbFold {
        teardown,
        dropped,
        winner_serials,
        warnings,
    }
}

impl PeerNode {
    pub fn is_rpc_ready(&self) -> bool {
        self.status.is_rpc_ready() && self.rpc_port > 0
    }

    pub fn rpc_endpoint(&self) -> String {
        format!("{}:{}", self.addr.ip(), self.rpc_port)
    }

    /// Operator-facing label: display name when known, else `Node-<uuid8>`.
    pub fn label(&self) -> String {
        let name = self.display_name.trim();
        if !name.is_empty() {
            return name.to_string();
        }
        let id = self.uuid.to_string();
        let short = if id.len() >= 8 { &id[..8] } else { &id };
        format!("Node-{}", short)
    }

    pub fn service_endpoint(&self) -> ServiceEndpoint {
        ServiceEndpoint {
            node_id: self.uuid,
            cluster_id: None,
            protocol_version: 1,
            role: self.role,
            capabilities: Vec::new(),
            addresses: vec![self.addr],
            api_port: self.api_port,
            rpc_port: self.rpc_port,
            control_port: self.control_port,
            display_name: self.display_name.clone(),
            battery_percent: self.battery_percent,
            power_source: self.power_source,
        }
    }
}

/// Beacon v1 has no USB or battery fields. Copy them from the peer we already know.
fn peer_from_beacon(
    existing: Option<&PeerNode>,
    beacon: &BeaconPacket,
    peer_ip: std::net::IpAddr,
    default_control_port: u16,
) -> PeerNode {
    let control_port = existing
        .map(|peer| peer.control_port)
        .unwrap_or(default_control_port);
    let display_name = existing
        .map(|peer| peer.display_name.clone())
        .unwrap_or_default();
    PeerNode {
        uuid: beacon.uuid,
        addr: SocketAddr::new(peer_ip, beacon.api_port),
        role: beacon.role,
        status: beacon.status,
        api_port: beacon.api_port,
        rpc_port: beacon.rpc_port,
        control_port,
        total_ram_mb: beacon.total_ram_mb,
        free_ram_mb: beacon.free_ram_mb,
        backend: beacon.backend,
        thermal_index: beacon.thermal_index,
        active_model: beacon.active_model.clone(),
        display_name,
        moe_stream: existing.map(|peer| peer.moe_stream).unwrap_or(false),
        moe_cache_ceil_mb: existing.map(|peer| peer.moe_cache_ceil_mb).unwrap_or(0),
        link_class: existing
            .map(|peer| peer.link_class)
            .unwrap_or(LinkClass::Wifi),
        usb_serial: existing.and_then(|peer| peer.usb_serial.clone()),
        usb_api_endpoint: existing.and_then(|peer| peer.usb_api_endpoint.clone()),
        usb_control_endpoint: existing.and_then(|peer| peer.usb_control_endpoint.clone()),
        battery_percent: existing.and_then(|peer| peer.battery_percent),
        power_source: existing
            .map(|peer| peer.power_source)
            .unwrap_or(PowerSource::Unknown),
        last_seen: Instant::now(),
    }
}

/// Autonomous UDP discovery and peer caching service.
pub struct DiscoveryService {
    config: Arc<std::sync::RwLock<NexusConfig>>,
    node_uuid: Uuid,
    peers: Arc<RwLock<HashMap<Uuid, PeerNode>>>,
    registry: Arc<RwLock<PeerRegistry>>,
    active_model: Arc<RwLock<String>>,
    status_flags: Arc<RwLock<StatusFlags>>,
    rpc_port: Arc<RwLock<u16>>,
    udp_health: Arc<RwLock<BackendHealth>>,
    mdns_health: Arc<RwLock<BackendHealth>>,
    extra_targets: Arc<RwLock<Vec<SocketAddr>>>,
    last_unicast_replies: Arc<RwLock<HashMap<std::net::IpAddr, Instant>>>,
    mdns_enabled: Arc<RwLock<bool>>,
    /// Cached link-quality probes (Phase 11).
    link_quality: Arc<RwLock<HashMap<Uuid, crate::cluster::LinkQuality>>>,
    /// Last time this process spawned `adb` for a USB probe. Hub ticks share it.
    usb_probe_at: std::sync::Mutex<Option<Instant>>,
    /// One probe at a time. The background tick uses `try_lock`; a Tunnel refresh waits.
    usb_probe_lock: tokio::sync::Mutex<()>,
}

impl DiscoveryService {
    pub fn new(config: NexusConfig, custom_uuid: Option<Uuid>) -> Self {
        Self::with_shared_config(Arc::new(std::sync::RwLock::new(config)), custom_uuid)
    }

    pub fn with_shared_config(
        config: Arc<std::sync::RwLock<NexusConfig>>,
        custom_uuid: Option<Uuid>,
    ) -> Self {
        let snapshot = config.read().expect("config lock");
        let node_uuid = custom_uuid
            .or_else(|| snapshot.node_uuid().ok())
            .unwrap_or_else(Uuid::new_v4);
        let max_peers = snapshot.network.discovery.max_peers;
        let peer_timeout = Duration::from_millis(snapshot.network.discovery.peer_timeout_ms);
        let discovery_enabled = snapshot.network.discovery.enabled;
        let mdns_enabled_val =
            snapshot.network.discovery.enabled && snapshot.network.discovery.mdns.enabled;
        drop(snapshot);
        let default_status = StatusFlags(0);
        let udp_health = Arc::new(RwLock::new(if discovery_enabled {
            BackendHealth::Started
        } else {
            BackendHealth::Stopped
        }));
        let mdns_health = Arc::new(RwLock::new(if mdns_enabled_val {
            BackendHealth::Started
        } else {
            BackendHealth::Stopped
        }));

        Self {
            config,
            node_uuid,
            peers: Arc::new(RwLock::new(HashMap::new())),
            registry: Arc::new(RwLock::new(
                PeerRegistry::new(max_peers, peer_timeout)
                    .expect("validated discovery peer limit must be non-zero"),
            )),
            active_model: Arc::new(RwLock::new(String::new())),
            status_flags: Arc::new(RwLock::new(default_status)),
            rpc_port: Arc::new(RwLock::new(0)),
            udp_health,
            mdns_health,
            extra_targets: Arc::new(RwLock::new(Vec::new())),
            last_unicast_replies: Arc::new(RwLock::new(HashMap::new())),
            mdns_enabled: Arc::new(RwLock::new(mdns_enabled_val)),
            link_quality: Arc::new(RwLock::new(HashMap::new())),
            usb_probe_at: std::sync::Mutex::new(None),
            usb_probe_lock: tokio::sync::Mutex::new(()),
        }
    }

    /// `Some` when this caller may spawn `adb`. The guard is held for the whole probe.
    ///
    /// A background tick skips the turn when a probe is already running or the last one
    /// was under 5 seconds ago. `force` waits for the lock and ignores the gap.
    pub async fn usb_probe_permit(&self, force: bool) -> Option<tokio::sync::MutexGuard<'_, ()>> {
        let guard = if force {
            self.usb_probe_lock.lock().await
        } else {
            match self.usb_probe_lock.try_lock() {
                Ok(guard) => guard,
                Err(_) => return None,
            }
        };
        let gap = Duration::from_secs(5);
        let mut last = self
            .usb_probe_at
            .lock()
            .unwrap_or_else(|err| err.into_inner());
        if !force {
            if let Some(prev) = *last {
                if prev.elapsed() < gap {
                    return None;
                }
            }
        }
        *last = Some(Instant::now());
        Some(guard)
    }

    /// Apply a pure USB fold, then drop USB-only peers whose cable is gone.
    pub async fn apply_folded_usb_probes(
        &self,
        cache: &mut crate::adb_links::AdbLinkCache,
        hits: &[UsbProbeHit],
        present_serials: &[String],
        listed_forwards: &[(String, u16)],
    ) -> UsbFold {
        let fold = {
            let mut peers = self.peers.write().await;
            fold_usb_probes(
                &mut peers,
                cache,
                hits,
                present_serials,
                listed_forwards,
                self.cfg().hardware.safety.max_ram_usage_percent,
            )
        };
        for id in &fold.dropped {
            self.remove_peer(*id).await;
        }
        fold
    }

    pub fn node_uuid(&self) -> Uuid {
        self.node_uuid
    }

    pub fn peers(&self) -> Arc<RwLock<HashMap<Uuid, PeerNode>>> {
        self.peers.clone()
    }

    pub fn peer_registry(&self) -> Arc<RwLock<PeerRegistry>> {
        self.registry.clone()
    }

    pub fn udp_health(&self) -> Arc<RwLock<BackendHealth>> {
        self.udp_health.clone()
    }

    pub fn mdns_health(&self) -> Arc<RwLock<BackendHealth>> {
        self.mdns_health.clone()
    }

    pub async fn set_udp_health(&self, health: BackendHealth) {
        *self.udp_health.write().await = health;
    }

    pub async fn set_mdns_health(&self, health: BackendHealth) {
        *self.mdns_health.write().await = health;
    }

    pub async fn is_mdns_enabled(&self) -> bool {
        *self.mdns_enabled.read().await
    }

    pub async fn set_mdns_enabled(self: &Arc<Self>, enabled: bool) {
        let mut mdns_en = self.mdns_enabled.write().await;
        if *mdns_en == enabled {
            return;
        }
        *mdns_en = enabled;
        drop(mdns_en);

        if enabled {
            info!("Hot-reloading discovery: enabling and starting mDNS-SD service");
            self.clone().start_mdns();
        } else {
            info!("Hot-reloading discovery: disabling mDNS-SD service");
            *self.mdns_health.write().await = BackendHealth::Stopped;
        }
    }

    /// Register a peer IP and transmit an immediate targeted discovery probe to its discovery port.
    pub async fn send_direct_probe_to_ip(&self, ip: std::net::IpAddr) {
        let target = SocketAddr::new(ip, self.cfg().network.discovery_port);
        {
            let mut extra = self.extra_targets.write().await;
            if !extra.contains(&target) {
                extra.push(target);
            }
        }
        self.send_probe_to(target).await;
    }

    /// Locally advertised active model name (beacon / mDNS).
    pub async fn get_active_model(&self) -> String {
        self.active_model.read().await.clone()
    }

    pub async fn set_active_model(&self, model_name: impl Into<String>) {
        let mut model = self.active_model.write().await;
        *model = model_name.into();
    }

    pub async fn set_status_flags(&self, flags: StatusFlags) {
        let mut s = self.status_flags.write().await;
        *s = flags;
    }

    pub async fn set_rpc_status(&self, rpc_ready: bool, port: u16) {
        let mut s = self.status_flags.write().await;
        if rpc_ready {
            s.0 |= StatusFlags::RPC_READY.0;
        } else {
            s.0 &= !StatusFlags::RPC_READY.0;
        }
        let mut p = self.rpc_port.write().await;
        *p = port;
    }

    pub async fn get_rpc_port(&self) -> u16 {
        *self.rpc_port.read().await
    }

    pub fn config(&self) -> NexusConfig {
        self.config.read().expect("config lock").clone()
    }

    pub fn shared_config(&self) -> Arc<std::sync::RwLock<NexusConfig>> {
        self.config.clone()
    }

    fn cfg(&self) -> std::sync::RwLockReadGuard<'_, NexusConfig> {
        self.config.read().expect("config lock poisoned")
    }

    /// Read thermal index (0 to 100 scale) as a heuristic indicator for load-balancing.
    ///
    /// # Heuristic Design
    /// Probes Linux `/sys/class/thermal/thermal_zone0/temp` (with fallback to `thermal_zone1`
    /// or `/sys/class/power_supply/battery/temp`), clamping 40°C–80°C to an index of 0–100.
    /// Used by discovery scoring to demote thermally throttled devices. Nominal fallback is 0.
    pub fn probe_thermal_index() -> u8 {
        for path in &[
            "/sys/class/thermal/thermal_zone0/temp",
            "/sys/class/thermal/thermal_zone1/temp",
            "/sys/class/power_supply/battery/temp",
        ] {
            if let Ok(content) = std::fs::read_to_string(path) {
                if let Ok(milli_c) = content.trim().parse::<i64>() {
                    let temp_c = if milli_c > 1000 {
                        milli_c as f32 / 1000.0
                    } else {
                        milli_c as f32 / 10.0
                    };
                    if temp_c <= 40.0 {
                        return 0;
                    } else if temp_c >= 80.0 {
                        return 100;
                    } else {
                        return (((temp_c - 40.0) / 40.0) * 100.0).round() as u8;
                    }
                }
            }
        }
        0 // Nominal fallback if thermal zone unreadable
    }

    /// Build a current 64-byte beacon packet reflecting live hardware profile,
    /// active model, role, status flags, and thermal heuristic.
    pub async fn create_current_beacon(&self) -> BeaconPacket {
        let profile = SystemProfile::probe();
        let thermal_index = Self::probe_thermal_index();
        let active_model = self.active_model.read().await.clone();
        let status = *self.status_flags.read().await;
        let rpc_port = *self.rpc_port.read().await;
        let cfg = self.cfg();
        let role = NodeRole::from_str_role(&cfg.node.role);
        let api_port = cfg.network.api_port;

        BeaconPacket {
            magic: BEACON_MAGIC,
            version: BEACON_VERSION,
            role,
            status,
            uuid: self.node_uuid,
            api_port,
            rpc_port,
            total_ram_mb: profile.total_ram_mb as u32,
            free_ram_mb: profile.available_ram_mb as u32,
            backend: profile.detected_backend,
            thermal_index,
            active_model,
        }
    }

    /// Compute all target addresses (global broadcast, subnet broadcasts, static peers, loopback).
    pub fn get_broadcast_targets(config: &NexusConfig) -> Vec<SocketAddr> {
        let mut targets = Vec::new();

        // 1. Global limited broadcast (255.255.255.255)
        if let Ok(addr) = format!("255.255.255.255:{}", config.network.discovery_port).parse() {
            targets.push(addr);
        }

        // 2. Subnet directed broadcast addresses (e.g. 192.168.6.255)
        for bcast_ip in get_broadcast_addresses() {
            let addr = SocketAddr::new(
                std::net::IpAddr::V4(bcast_ip),
                config.network.discovery_port,
            );
            if !targets.contains(&addr) {
                targets.push(addr);
            }
        }

        // 3. Loopback (127.0.0.1)
        if let Ok(addr) = format!("127.0.0.1:{}", config.network.discovery_port).parse() {
            if !targets.contains(&addr) {
                targets.push(addr);
            }
        }

        // 4. Configured static peers
        for peer in &config.network.static_peers {
            if let Ok(addr) = peer.parse::<SocketAddr>() {
                if !targets.contains(&addr) {
                    targets.push(addr);
                }
            } else if let Ok(ip) = peer.parse::<std::net::IpAddr>() {
                let addr = SocketAddr::new(ip, config.network.discovery_port);
                if !targets.contains(&addr) {
                    targets.push(addr);
                }
            }
        }

        targets
    }

    /// Compute active targets including static peers added at runtime.
    pub async fn broadcast_targets(&self) -> Vec<SocketAddr> {
        let mut targets = Self::get_broadcast_targets(&self.cfg());
        let extra = self.extra_targets.read().await;
        for target in extra.iter() {
            if !targets.contains(target) {
                targets.push(*target);
            }
        }
        targets
    }

    /// Add a static peer address at runtime and send an immediate discovery probe to it.
    pub async fn add_static_peer(&self, peer_str: &str) {
        let target_addr = if let Ok(addr) = peer_str.parse::<SocketAddr>() {
            Some(addr)
        } else if let Ok(ip) = peer_str.parse::<std::net::IpAddr>() {
            Some(SocketAddr::new(ip, self.cfg().network.discovery_port))
        } else {
            None
        };

        if let Some(addr) = target_addr {
            let mut extra = self.extra_targets.write().await;
            if !extra.contains(&addr) {
                extra.push(addr);
            }
            drop(extra);
            self.send_probe_to(addr).await;
        }
    }

    /// Transmit a targeted discovery probe to a specific endpoint.
    pub async fn send_probe_to(&self, target: SocketAddr) {
        let socket = match UdpSocket::bind("0.0.0.0:0").await {
            Ok(s) => s,
            Err(e) => {
                debug!("Failed to bind UDP probe socket: {}", e);
                return;
            }
        };
        let _ = socket.set_broadcast(true);

        let beacon = self.create_current_beacon().await;
        let packet_bytes = beacon.encode();
        let _ = socket.send_to(&packet_bytes, target).await;
    }

    /// Transmit an immediate discovery probe across all broadcast and peer targets.
    pub async fn send_probe(&self) {
        let targets = self.broadcast_targets().await;
        for target in targets {
            self.send_probe_to(target).await;
        }
    }

    /// Spawn asynchronous UDP beacon broadcaster task.
    pub fn start_broadcaster(self: Arc<Self>) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            if !self.cfg().network.discovery.enabled {
                info!("Discovery broadcaster disabled by configuration");
                return;
            }
            let socket = match UdpSocket::bind("0.0.0.0:0").await {
                Ok(s) => s,
                Err(e) => {
                    error!("Failed to bind UDP broadcast socket: {}", e);
                    return;
                }
            };

            if let Err(e) = socket.set_broadcast(true) {
                error!("Failed to enable UDP broadcast: {}", e);
                return;
            }

            let initial_targets = self.broadcast_targets().await;
            info!(
                "Discovery broadcaster active: transmitting beacons to {:?} every {} ms",
                initial_targets,
                self.cfg().network.broadcast_interval_ms
            );

            let interval = Duration::from_millis(self.cfg().network.broadcast_interval_ms);
            let mut ticker = tokio::time::interval(interval);

            loop {
                ticker.tick().await;

                let beacon = self.create_current_beacon().await;
                let packet_bytes = beacon.encode();
                let targets = self.broadcast_targets().await;
                for target in targets {
                    if let Err(e) = socket.send_to(&packet_bytes, target).await {
                        debug!("UDP beacon send to {} failed: {}", target, e);
                    } else {
                        trace!("Sent discovery beacon to {}", target);
                    }
                }
            }
        })
    }

    /// Spawn asynchronous UDP listener task to receive and record beacons.
    pub fn start_listener(self: Arc<Self>) -> tokio::task::JoinHandle<()> {
        Self::start_listener_inner(self, None)
    }

    pub fn start_listener_with_events(
        self: Arc<Self>,
    ) -> (
        tokio::sync::mpsc::Receiver<DiscoveryEvent>,
        tokio::task::JoinHandle<()>,
    ) {
        let (sender, receiver) = tokio::sync::mpsc::channel(32);
        let task = Self::start_listener_inner(self, Some(sender));
        (receiver, task)
    }

    fn start_listener_inner(
        self: Arc<Self>,
        events: Option<tokio::sync::mpsc::Sender<DiscoveryEvent>>,
    ) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            let (discovery_enabled, discovery_port) = {
                let cfg = self.config.read().expect("config lock");
                (cfg.network.discovery.enabled, cfg.network.discovery_port)
            };
            if !discovery_enabled {
                info!("Discovery listener disabled by configuration");
                *self.udp_health.write().await = BackendHealth::Stopped;
                return;
            }
            let socket = match create_listener_socket(discovery_port) {
                Ok(s) => s,
                Err(e) => {
                    error!(
                        "Failed to bind discovery listener to port {}: {}",
                        discovery_port, e
                    );
                    *self.udp_health.write().await = BackendHealth::Failed;
                    if let Some(sender) = &events {
                        let _ = sender
                            .send(DiscoveryEvent::BackendHealth {
                                backend: "udp",
                                health: BackendHealth::Failed,
                            })
                            .await;
                    }
                    return;
                }
            };

            info!("Discovery listener active on port {}", discovery_port);
            *self.udp_health.write().await = BackendHealth::Healthy;
            if let Some(sender) = &events {
                let _ = sender
                    .send(DiscoveryEvent::BackendHealth {
                        backend: "udp",
                        health: BackendHealth::Healthy,
                    })
                    .await;
            }
            let mut buf = [0u8; 128];

            loop {
                match socket.recv_from(&mut buf).await {
                    Ok((len, peer_addr)) => {
                        if len == BEACON_PACKET_SIZE {
                            match BeaconPacket::decode(&buf[..len]) {
                                Ok(beacon) => {
                                    // Skip self-beaconing
                                    if beacon.uuid == self.node_uuid {
                                        continue;
                                    }

                                    debug!(
                                        "Received valid beacon from {:?} ({:?})",
                                        peer_addr, beacon.uuid
                                    );
                                    let event =
                                        self.rebuild_peer_from_beacon(&beacon, peer_addr).await;
                                    if let Some(sender) = &events {
                                        let _ = sender.send(event).await;
                                    }

                                    // Auto-register peer endpoint into extra_targets so ongoing periodic unicast beacons reach it
                                    let peer_discovery_addr =
                                        SocketAddr::new(peer_addr.ip(), discovery_port);
                                    {
                                        let mut extra = self.extra_targets.write().await;
                                        if !extra.contains(&peer_discovery_addr) {
                                            extra.push(peer_discovery_addr);
                                        }
                                    }

                                    // Symmetric Mesh Discovery:
                                    // Respond directly to the sender via unicast UDP with rate-limiting (at most once every 5 seconds per peer IP)
                                    // to prevent recursive ping-pong storms while ensuring reliable discovery across Wi-Fi routers that drop broadcast.
                                    let should_reply = {
                                        let mut replies = self.last_unicast_replies.write().await;
                                        match replies.get(&peer_addr.ip()) {
                                            Some(last)
                                                if last.elapsed() < Duration::from_secs(5) =>
                                            {
                                                false
                                            }
                                            _ => {
                                                replies.insert(peer_addr.ip(), Instant::now());
                                                true
                                            }
                                        }
                                    };

                                    if should_reply {
                                        let reply_beacon = self.create_current_beacon().await;
                                        let reply_bytes = reply_beacon.encode();
                                        let _ =
                                            socket.send_to(&reply_bytes, peer_discovery_addr).await;
                                    }
                                }
                                Err(e) => {
                                    trace!(
                                        "Discarding invalid discovery packet from {}: {}",
                                        peer_addr,
                                        e
                                    );
                                }
                            }
                        }
                    }
                    Err(e) => {
                        warn!("UDP listener recv error: {}", e);
                    }
                }
            }
        })
    }

    /// Apply or clear USB endpoints from the serial cache. Match is the cache node id.
    pub async fn sync_usb_links(
        &self,
        cache: &crate::adb_links::AdbLinkCache,
        forwards: &[crate::adb_links::HostForward],
    ) {
        let mut peers = self.peers.write().await;
        sync_peer_usb_links(&mut peers, cache, forwards);
    }

    /// Retrieve active peers, evicting any whose last beacon is older than `peer_timeout_ms` (6000 ms).
    pub async fn get_active_peers(&self) -> Vec<PeerNode> {
        let timeout = Duration::from_millis(self.cfg().network.peer_timeout_ms);
        let mut peers = self.peers.write().await;

        let now = Instant::now();
        peers.retain(|_, peer| peer_still_active(peer, timeout, now));
        peers.values().cloned().collect()
    }

    /// Select the best compute host peer currently available on the subnet.
    /// Ranking: READY (+1000), Vulkan (+500), free RAM MB, minus thermal index.
    pub async fn find_best_host(&self) -> Option<PeerNode> {
        let active = self.get_active_peers().await;
        active
            .into_iter()
            .filter(|p| p.role.is_host())
            .max_by_key(|p| {
                let mut score = p.free_ram_mb as i64;
                if p.status.is_ready() {
                    score += 1000;
                }
                if p.backend == AccelerationBackend::Vulkan {
                    score += 500;
                }
                score -= p.thermal_index as i64;
                score
            })
    }

    pub async fn is_peer_trusted_for_routing(&self, peer_id: Uuid) -> bool {
        let security = self.config().network.security;
        if !pairing_enforced(&security) {
            return true;
        }
        if !security.allowed_peer_ids.contains(&peer_id) {
            return false;
        }
        let registry = self.registry.read().await;
        registry
            .get(peer_id)
            .map(|record| record.verified && matches!(record.lifecycle, PeerLifecycle::Healthy))
            .unwrap_or(false)
    }

    /// Best ready host that passes pairing + registry verification when enforced.
    pub async fn find_best_trusted_host(&self) -> Option<PeerNode> {
        let mut hosts: Vec<PeerNode> = self
            .get_active_peers()
            .await
            .into_iter()
            .filter(|p| p.role.is_host() && p.status.is_ready())
            .collect();
        hosts.sort_by_key(|p| {
            std::cmp::Reverse({
                let mut score = p.free_ram_mb as i64;
                if p.status.is_ready() {
                    score += 1000;
                }
                if p.backend == AccelerationBackend::Vulkan {
                    score += 500;
                }
                score -= p.thermal_index as i64;
                score
            })
        });
        for host in hosts {
            if self.is_peer_trusted_for_routing(host.uuid).await {
                return Some(host);
            }
        }
        None
    }

    /// Select the best RPC peer node currently available on the subnet (highest free RAM).
    #[deprecated(note = "use select_rpc_candidate")]
    pub async fn find_best_rpc_peer(&self) -> Option<PeerNode> {
        let active = self.get_active_peers().await;
        active
            .into_iter()
            .filter(|p| p.is_rpc_ready())
            .max_by_key(|p| p.free_ram_mb)
    }

    pub async fn resolve_primary_compute_anchor(&self) -> Option<PeerNode> {
        let anchor = self.cfg().network.anchors.primary_compute_id?;
        self.get_active_peers()
            .await
            .into_iter()
            .find(|peer| peer.uuid == anchor && peer.role.is_host() && peer.status.is_ready())
    }

    pub async fn rpc_candidates(&self, policy: RpcSelectionPolicy) -> Vec<RpcCandidate> {
        let security = self.cfg().network.security.clone();
        let enforce_registry = pairing_enforced(&security);
        let eligible_ids: std::collections::HashSet<Uuid> = self
            .registry
            .read()
            .await
            .eligible_rpc_peers(policy.protocol_version)
            .into_iter()
            .map(|record| record.node_id)
            .collect();

        let mut candidates = self
            .get_active_peers()
            .await
            .into_iter()
            .filter(|peer| {
                peer.is_rpc_ready()
                    && peer.status.is_ready()
                    && peer.thermal_index <= policy.max_thermal_index
                    && (!enforce_registry || eligible_ids.contains(&peer.uuid))
                    && (!policy.require_pairing
                        || self
                            .cfg()
                            .network
                            .security
                            .allowed_peer_ids
                            .contains(&peer.uuid))
            })
            .map(|peer| {
                let allocatable_mb = u64::from(peer.free_ram_mb).min(policy.max_allocatable_mb);
                RpcCandidate {
                    rationale: format!(
                        "healthy RPC-ready peer; allocatable budget capped at {} MB",
                        allocatable_mb
                    ),
                    peer,
                    allocatable_mb,
                }
            })
            .filter(|candidate| candidate.allocatable_mb > 0)
            .collect::<Vec<_>>();

        candidates.sort_by_key(|candidate| {
            (
                std::cmp::Reverse(candidate.allocatable_mb),
                candidate.peer.uuid,
            )
        });
        candidates
    }

    pub async fn select_rpc_candidate(&self, policy: RpcSelectionPolicy) -> Option<RpcCandidate> {
        self.select_rpc_candidates(policy).await.into_iter().next()
    }

    /// Ordered list of RPC workers (most allocatable first).
    pub async fn select_rpc_candidates(&self, policy: RpcSelectionPolicy) -> Vec<RpcCandidate> {
        self.rpc_candidates(policy).await
    }

    /// Return cached link quality for a peer, if present and not stale (~60s).
    pub async fn cached_link_quality(&self, peer_id: Uuid) -> Option<crate::cluster::LinkQuality> {
        let guard = self.link_quality.read().await;
        guard.get(&peer_id).and_then(|lq| {
            if lq.is_stale(Duration::from_secs(60)) {
                None
            } else {
                Some(lq.clone())
            }
        })
    }

    /// Store a link-quality probe result for ranking.
    pub async fn record_link_quality(&self, peer_id: Uuid, quality: crate::cluster::LinkQuality) {
        let mut guard = self.link_quality.write().await;
        guard.insert(peer_id, quality);
    }

    /// Lightweight RTT probe against a peer control endpoint (`GET /health` or root).
    pub async fn probe_link_quality(&self, peer: &PeerNode) -> crate::cluster::LinkQuality {
        let url = format!("{}/", peer.control_endpoint().trim_end_matches('/'));
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(2))
            .build();
        let Ok(client) = client else {
            return crate::cluster::LinkQuality::unknown();
        };
        let start = Instant::now();
        match client.get(&url).send().await {
            Ok(resp) => {
                let rtt = start.elapsed();
                let bytes = resp.content_length().unwrap_or(256);
                let quality = crate::cluster::LinkQuality::from_probe(
                    rtt,
                    bytes,
                    rtt.max(Duration::from_millis(1)),
                );
                self.record_link_quality(peer.uuid, quality.clone()).await;
                quality
            }
            Err(_) => {
                let quality = crate::cluster::LinkQuality::unknown();
                self.record_link_quality(peer.uuid, quality.clone()).await;
                quality
            }
        }
    }

    /// Replace the cached peer with this beacon, keeping USB and battery fields.
    async fn rebuild_peer_from_beacon(
        &self,
        beacon: &BeaconPacket,
        peer_addr: SocketAddr,
    ) -> DiscoveryEvent {
        let mut peers = self.peers.write().await;
        let default_control_port = self
            .config
            .read()
            .expect("config lock")
            .network
            .control_port;
        let peer = peer_from_beacon(
            peers.get(&beacon.uuid),
            beacon,
            peer_addr.ip(),
            default_control_port,
        );
        let event = if peers.contains_key(&beacon.uuid) {
            DiscoveryEvent::ServiceUpdated(peer.service_endpoint())
        } else {
            DiscoveryEvent::ServiceFound(peer.service_endpoint())
        };
        peers.insert(beacon.uuid, peer);
        drop(peers);
        if let Err(error) = self.registry.write().await.apply_event(
            event.clone(),
            ObservationSource::Udp,
            Instant::now(),
        ) {
            if matches!(
                error,
                crate::peer_registry::RegistryError::EndpointConflict(_)
            ) {
                self.registry
                    .write()
                    .await
                    .keep_fresh(beacon.uuid, Instant::now());
            }
            warn!("Rejected UDP peer observation: {}", error);
        }
        event
    }

    /// Control-plane state overwrites a TXT battery hint on the registry record and the live peer.
    pub async fn apply_verified_state(
        &self,
        state: &crate::control_plane::ControlPlaneState,
        endpoint: &ServiceEndpoint,
    ) -> Result<(), crate::peer_registry::RegistryError> {
        {
            let mut peers = self.peers.write().await;
            if let Some(peer) = peers.get_mut(&state.node_id) {
                peer.battery_percent = state.battery_percent;
                peer.power_source = state.power_source;
            }
        }
        let verified = crate::control_plane::endpoint_from_state(state, endpoint);
        self.peer_registry()
            .write()
            .await
            .mark_verified(state.node_id, verified, Instant::now())
    }

    /// Record a discovered service endpoint (from mDNS, static config, etc.) and merge into active peers.
    pub async fn record_service_endpoint(
        &self,
        endpoint: ServiceEndpoint,
        source: ObservationSource,
    ) {
        if endpoint.node_id == self.node_uuid {
            return;
        }

        let probe_target = endpoint
            .addresses
            .first()
            .map(|addr| SocketAddr::new(addr.ip(), self.cfg().network.discovery_port));

        let mut peers = self.peers.write().await;
        let event = if let Some(existing) = peers.get_mut(&endpoint.node_id) {
            existing.role = endpoint.role;
            // Leave link_class and the USB endpoints untouched.
            if let Some(addr) = endpoint.addresses.first() {
                existing.addr = *addr;
            }
            existing.api_port = endpoint.api_port;
            existing.rpc_port = endpoint.rpc_port;
            if endpoint.control_port > 0 {
                existing.control_port = endpoint.control_port;
            }
            if !endpoint.display_name.trim().is_empty() {
                existing.display_name = endpoint.display_name.clone();
            }
            if endpoint.rpc_port > 0 {
                existing.status.0 |= StatusFlags::RPC_READY.0;
            }
            // Missing, empty, or out-of-range TXT keys are not hints.
            apply_power_hint(
                &mut existing.battery_percent,
                &mut existing.power_source,
                endpoint.battery_percent,
                endpoint.power_source,
            );
            existing.last_seen = Instant::now();
            DiscoveryEvent::ServiceUpdated(existing.service_endpoint())
        } else {
            let addr = endpoint.addresses.first().cloned().unwrap_or_else(|| {
                SocketAddr::new(
                    std::net::IpAddr::V4(std::net::Ipv4Addr::UNSPECIFIED),
                    endpoint.api_port,
                )
            });
            let mut status = StatusFlags::READY;
            if endpoint.rpc_port > 0 {
                status.0 |= StatusFlags::RPC_READY.0;
            }
            let control_port = if endpoint.control_port > 0 {
                endpoint.control_port
            } else {
                self.cfg().network.control_port
            };
            let peer = PeerNode {
                uuid: endpoint.node_id,
                addr,
                role: endpoint.role,
                status,
                api_port: endpoint.api_port,
                rpc_port: endpoint.rpc_port,
                control_port,
                total_ram_mb: 0,
                free_ram_mb: 0,
                backend: AccelerationBackend::GenericCpu,
                thermal_index: 0,
                active_model: String::new(),
                display_name: endpoint.display_name.clone(),
                moe_stream: endpoint
                    .capabilities
                    .iter()
                    .any(|c| c.eq_ignore_ascii_case("moe_stream")),
                moe_cache_ceil_mb: 0,
                link_class: LinkClass::Wifi,
                usb_serial: None,
                usb_api_endpoint: None,
                usb_control_endpoint: None,
                battery_percent: endpoint.battery_percent,
                power_source: endpoint.power_source,
                last_seen: Instant::now(),
            };
            peers.insert(endpoint.node_id, peer.clone());
            DiscoveryEvent::ServiceFound(endpoint.clone())
        };
        drop(peers);

        if let Err(e) = self
            .registry
            .write()
            .await
            .apply_event(event, source, Instant::now())
        {
            if matches!(e, crate::peer_registry::RegistryError::EndpointConflict(_)) {
                self.registry
                    .write()
                    .await
                    .keep_fresh(endpoint.node_id, Instant::now());
            }
            warn!(
                "Rejected {} peer observation: {}",
                match source {
                    ObservationSource::Udp => "UDP",
                    ObservationSource::Mdns => "mDNS",
                    ObservationSource::Static => "static",
                    ObservationSource::ControlPlane => "control plane",
                },
                e
            );
        }

        // Send a unicast UDP probe to obtain full hardware telemetry if endpoint address is reachable
        if let Some(target) = probe_target {
            self.send_probe_to(target).await;
        }
    }

    /// Remove a decommissioned or timed-out peer.
    pub async fn remove_peer(&self, node_id: Uuid) {
        let mut peers = self.peers.write().await;
        peers.remove(&node_id);
        drop(peers);

        let _ = self.registry.write().await.apply_event(
            DiscoveryEvent::ServiceRemoved { node_id },
            ObservationSource::Mdns,
            Instant::now(),
        );
    }

    /// Spawn asynchronous mDNS advertising and browsing service if enabled.
    pub fn start_mdns(self: Arc<Self>) -> Option<tokio::task::JoinHandle<()>> {
        let enabled = if let Ok(guard) = self.mdns_enabled.try_read() {
            *guard
        } else {
            self.cfg().network.discovery.mdns.enabled
        };

        if !self.cfg().network.discovery.enabled || !enabled {
            info!("mDNS discovery disabled by configuration");
            return None;
        }

        let this = self.clone();
        Some(tokio::spawn(async move {
            let mdns = match crate::mdns::MdnsBackend::new() {
                Ok(m) => m,
                Err(e) => {
                    warn!("Failed to initialize mDNS daemon: {}", e);
                    *this.mdns_health.write().await = BackendHealth::Failed;
                    return;
                }
            };

            let local_ip = get_local_ip();
            let rpc_port = *this.rpc_port.read().await;
            let (role, service_type, api_port, control_port, display_name) = {
                let cfg = this.config.read().expect("config lock");
                (
                    NodeRole::from_str_role(&cfg.node.role),
                    cfg.network.discovery.mdns.service_type.clone(),
                    cfg.network.api_port,
                    cfg.network.control_port,
                    cfg.resolved_display_name(),
                )
            };
            let caps = vec!["inference".to_string(), "rpc".to_string()];

            if let Err(e) = mdns.register(
                &service_type,
                &this.node_uuid.to_string(),
                &format!("{}.local.", this.node_uuid),
                api_port,
                rpc_port,
                control_port,
                this.node_uuid,
                None,
                role,
                &caps,
                &display_name,
                local_ip,
                None,
                PowerSource::Unknown,
            ) {
                warn!("Failed to register mDNS service: {}", e);
                *this.mdns_health.write().await = BackendHealth::Failed;
                return;
            }

            *this.mdns_health.write().await = BackendHealth::Healthy;
            info!("mDNS service registered on {} ({})", local_ip, service_type);

            let (mut events, _browse_handle) = match mdns.browse(&service_type) {
                Ok(res) => res,
                Err(e) => {
                    warn!("Failed to start mDNS browse: {}", e);
                    *this.mdns_health.write().await = BackendHealth::Failed;
                    return;
                }
            };

            while let Some(event) = events.recv().await {
                match event {
                    DiscoveryEvent::ServiceFound(endpoint)
                    | DiscoveryEvent::ServiceUpdated(endpoint) => {
                        debug!("mDNS discovered service: {:?}", endpoint.node_id);
                        this.record_service_endpoint(endpoint, ObservationSource::Mdns)
                            .await;
                    }
                    DiscoveryEvent::ServiceRemoved { node_id } => {
                        debug!("mDNS service removed: {:?}", node_id);
                        this.remove_peer(node_id).await;
                    }
                    DiscoveryEvent::BackendHealth { health, .. } => {
                        *this.mdns_health.write().await = health;
                    }
                }
            }
        }))
    }
}

/// Create a non-blocking UDP socket configured with SO_REUSEADDR and SO_REUSEPORT.
pub fn create_listener_socket(port: u16) -> Result<UdpSocket, std::io::Error> {
    use socket2::{Domain, Protocol, Socket, Type};
    let addr: SocketAddr = format!("0.0.0.0:{}", port)
        .parse()
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e))?;

    let socket = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))?;
    socket.set_reuse_address(true)?;
    let _ = socket.set_broadcast(true);

    #[cfg(unix)]
    unsafe {
        use std::os::unix::io::AsRawFd;
        let optval: libc::c_int = 1;
        let _ = libc::setsockopt(
            socket.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_REUSEPORT,
            &optval as *const _ as *const libc::c_void,
            std::mem::size_of_val(&optval) as libc::socklen_t,
        );
    }

    socket.set_nonblocking(true)?;
    socket.bind(&addr.into())?;

    UdpSocket::from_std(socket.into())
}

/// Query the local system network interfaces using getifaddrs to find all active IPv4 broadcast addresses.
#[cfg(unix)]
pub fn get_broadcast_addresses() -> Vec<std::net::Ipv4Addr> {
    let mut addrs = Vec::new();
    unsafe {
        let mut ifap: *mut libc::ifaddrs = std::ptr::null_mut();
        if libc::getifaddrs(&mut ifap) != 0 {
            return addrs;
        }
        let mut curr = ifap;
        while !curr.is_null() {
            let item = &*curr;
            let flags = item.ifa_flags as i32;
            let is_up = (flags & libc::IFF_UP) != 0;
            let is_loopback = (flags & libc::IFF_LOOPBACK) != 0;
            let is_broadcast = (flags & libc::IFF_BROADCAST) != 0;

            if is_up
                && !is_loopback
                && is_broadcast
                && !item.ifa_addr.is_null()
                && (*item.ifa_addr).sa_family as i32 == libc::AF_INET
            {
                let mut bcast = None;
                if !item.ifa_ifu.is_null() {
                    let bcast_in = &*(item.ifa_ifu as *const libc::sockaddr_in);
                    let bcast_bytes = bcast_in.sin_addr.s_addr.to_ne_bytes();
                    let addr = std::net::Ipv4Addr::from(bcast_bytes);
                    if !addr.is_unspecified() && addr != std::net::Ipv4Addr::new(127, 0, 0, 1) {
                        bcast = Some(addr);
                    }
                }

                if bcast.is_none() && !item.ifa_netmask.is_null() {
                    let sock_in = &*(item.ifa_addr as *const libc::sockaddr_in);
                    let mask_in = &*(item.ifa_netmask as *const libc::sockaddr_in);
                    let ip = sock_in.sin_addr.s_addr.to_ne_bytes();
                    let mask = mask_in.sin_addr.s_addr.to_ne_bytes();
                    let bcast_octets = [
                        ip[0] | !mask[0],
                        ip[1] | !mask[1],
                        ip[2] | !mask[2],
                        ip[3] | !mask[3],
                    ];
                    let addr = std::net::Ipv4Addr::from(bcast_octets);
                    if !addr.is_unspecified() && addr != std::net::Ipv4Addr::new(127, 0, 0, 1) {
                        bcast = Some(addr);
                    }
                }

                if let Some(addr) = bcast {
                    if !addrs.contains(&addr) {
                        addrs.push(addr);
                    }
                }
            }
            curr = item.ifa_next;
        }
        libc::freeifaddrs(ifap);
    }
    addrs
}

#[cfg(not(unix))]
pub fn get_broadcast_addresses() -> Vec<std::net::Ipv4Addr> {
    Vec::new()
}

/// Determine the preferred local IP address for peer advertising.
pub fn get_local_ip() -> std::net::IpAddr {
    if let Ok(socket) = std::net::UdpSocket::bind("0.0.0.0:0") {
        if socket.connect("8.8.8.8:80").is_ok() {
            if let Ok(addr) = socket.local_addr() {
                if !addr.ip().is_loopback() && !addr.ip().is_unspecified() {
                    return addr.ip();
                }
            }
        }
    }

    for bcast in get_broadcast_addresses() {
        if let Ok(socket) = std::net::UdpSocket::bind("0.0.0.0:0") {
            if socket.connect((bcast, 9999)).is_ok() {
                if let Ok(addr) = socket.local_addr() {
                    if !addr.ip().is_loopback() && !addr.ip().is_unspecified() {
                        return addr.ip();
                    }
                }
            }
        }
    }

    std::net::IpAddr::V4(std::net::Ipv4Addr::new(127, 0, 0, 1))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cluster::LinkClass;
    use crate::config::NexusConfig;
    use crate::peer_registry::ObservationSource;
    use crate::sysinfo::{AccelerationBackend, PowerSource};
    use std::time::Instant;
    use uuid::Uuid;

    fn endpoint_from_txt(node_id: Uuid, extra: &[(&str, &str)]) -> ServiceEndpoint {
        let mut properties = std::collections::HashMap::new();
        properties.insert("id".to_string(), node_id.to_string());
        properties.insert("role".to_string(), NodeRole::HOST.0.to_string());
        properties.insert("api".to_string(), "8080".to_string());
        properties.insert("rpc".to_string(), "50052".to_string());
        properties.insert("ctrl".to_string(), "9998".to_string());
        for (key, value) in extra {
            properties.insert((*key).to_string(), (*value).to_string());
        }
        let mut endpoint =
            crate::mdns::endpoint_from_properties(&properties, 8080).expect("txt fixture");
        endpoint.addresses = vec!["192.168.1.20:8080".parse().unwrap()];
        endpoint
    }

    async fn assert_stored_power(
        discovery: &DiscoveryService,
        node_id: Uuid,
        percent: Option<u8>,
        power: PowerSource,
    ) {
        let peer = discovery
            .peers()
            .read()
            .await
            .get(&node_id)
            .cloned()
            .expect("peer");
        assert_eq!(peer.battery_percent, percent);
        assert_eq!(peer.power_source, power);
        let registry = discovery.peer_registry();
        let registry = registry.read().await;
        let record = registry.get(node_id).expect("registry");
        assert_eq!(record.endpoint.battery_percent, percent);
        assert_eq!(record.endpoint.power_source, power);
    }

    #[tokio::test]
    async fn second_observe_without_battery_keys_keeps_stored_hint() {
        let discovery = DiscoveryService::new(NexusConfig::default(), Some(Uuid::from_u128(1)));
        let node_id = Uuid::from_u128(42);
        discovery
            .record_service_endpoint(
                endpoint_from_txt(node_id, &[("bat", "40"), ("pwr", "battery")]),
                ObservationSource::Mdns,
            )
            .await;

        discovery
            .record_service_endpoint(endpoint_from_txt(node_id, &[]), ObservationSource::Mdns)
            .await;
        assert_stored_power(&discovery, node_id, Some(40), PowerSource::Battery).await;

        // Present but not hints: empty bat, out of range, and pwr other than ac/battery.
        discovery
            .record_service_endpoint(
                endpoint_from_txt(node_id, &[("bat", ""), ("pwr", "unknown")]),
                ObservationSource::Mdns,
            )
            .await;
        discovery
            .record_service_endpoint(
                endpoint_from_txt(node_id, &[("bat", "140"), ("pwr", "nope")]),
                ObservationSource::Mdns,
            )
            .await;
        assert_stored_power(&discovery, node_id, Some(40), PowerSource::Battery).await;
    }

    #[tokio::test]
    async fn state_verify_sets_peer_battery_and_beacon_keeps_it() {
        let discovery = DiscoveryService::new(NexusConfig::default(), Some(Uuid::from_u128(1)));
        let node_id = Uuid::from_u128(7);
        discovery
            .record_service_endpoint(
                endpoint_from_txt(node_id, &[("bat", "40"), ("pwr", "battery")]),
                ObservationSource::Mdns,
            )
            .await;
        let endpoint = discovery
            .peer_registry()
            .read()
            .await
            .get(node_id)
            .expect("observed")
            .endpoint
            .clone();
        let state = crate::control_plane::ControlPlaneState {
            node_id,
            protocol_version: 1,
            role: NodeRole::HOST,
            capabilities: Vec::new(),
            ready: true,
            inferring: false,
            rpc_ready: true,
            allocatable_memory_mb: 1000,
            active_model: None,
            loaded_models: Vec::new(),
            signing_public_key: None,
            battery_percent: Some(15),
            power_source: PowerSource::Ac,
            display_name: String::new(),
            available_ram_mb: None,
            total_ram_mb: None,
            backend: None,
            prefill_device: None,
        };
        discovery
            .apply_verified_state(&state, &endpoint)
            .await
            .expect("verify writes state battery");
        assert_stored_power(&discovery, node_id, Some(15), PowerSource::Ac).await;

        let beacon = BeaconPacket {
            magic: BEACON_MAGIC,
            version: BEACON_VERSION,
            role: NodeRole::HOST,
            status: StatusFlags::READY,
            uuid: node_id,
            api_port: 8080,
            rpc_port: 50052,
            total_ram_mb: 8000,
            free_ram_mb: 4000,
            backend: AccelerationBackend::Vulkan,
            thermal_index: 3,
            active_model: String::new(),
        };
        discovery
            .rebuild_peer_from_beacon(&beacon, "192.168.1.20:9999".parse().unwrap())
            .await;
        let peer = discovery
            .peers()
            .read()
            .await
            .get(&node_id)
            .cloned()
            .expect("peer");
        assert_eq!(peer.free_ram_mb, 4000);
        assert_eq!(peer.battery_percent, Some(15));
        assert_eq!(peer.power_source, PowerSource::Ac);
        let registry = discovery.peer_registry();
        let registry = registry.read().await;
        let record = registry.get(node_id).expect("registry");
        assert_eq!(record.endpoint.battery_percent, Some(15));
        assert_eq!(record.endpoint.power_source, PowerSource::Ac);
    }

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

    #[test]
    fn sync_usb_links_follows_cache_uuid() {
        use crate::adb_links::{AdbLinkCache, AdbLinkRecord, HostForward};
        use std::collections::HashMap;

        let id = Uuid::from_u128(7);
        let mut peers = HashMap::new();
        peers.insert(id, usb_fixture());
        let cache = AdbLinkCache {
            links: vec![AdbLinkRecord {
                serial: "serial-a".into(),
                node_id: id,
                device_api_port: 8080,
                device_control_port: 9998,
                device_rpc_port: 50052,
            }],
        };
        let forwards = vec![HostForward {
            serial: "serial-a".into(),
            host_api: 18080,
            host_control: 18081,
        }];
        sync_peer_usb_links(&mut peers, &cache, &forwards);
        assert_eq!(peers[&id].link_class, LinkClass::Usb);
        assert_eq!(peers[&id].control_endpoint(), "http://127.0.0.1:18081");
        sync_peer_usb_links(&mut peers, &cache, &[]);
        assert_eq!(peers[&id].link_class, LinkClass::Wifi);
        assert_eq!(peers[&id].api_endpoint(), "http://192.168.1.20:8080");
    }

    fn probe_state(id: Uuid, name: &str) -> crate::control_plane::ControlPlaneState {
        crate::control_plane::ControlPlaneState {
            node_id: id,
            protocol_version: 1,
            role: NodeRole::HOST,
            capabilities: vec!["inference".into()],
            ready: true,
            inferring: false,
            rpc_ready: false,
            allocatable_memory_mb: 1500,
            active_model: None,
            loaded_models: Vec::new(),
            signing_public_key: None,
            battery_percent: Some(40),
            power_source: PowerSource::Battery,
            display_name: name.into(),
            available_ram_mb: None,
            total_ram_mb: None,
            backend: None,
            prefill_device: None,
        }
    }

    fn hit(
        serial: &str,
        host_api: u16,
        state: crate::control_plane::ControlPlaneState,
    ) -> UsbProbeHit {
        UsbProbeHit {
            serial: serial.into(),
            host_api,
            host_control: host_api + 1,
            device_api: 8080,
            device_control: 9998,
            device_rpc: 50052,
            state,
        }
    }

    #[test]
    fn port_attempts_retry_defaults_once() {
        assert_eq!(
            device_port_attempts(Some((1, 2, 3)), 8080, 9998, 50052),
            vec![(1, 2, 3), (8080, 9998, 50052)]
        );
        assert_eq!(
            device_port_attempts(Some((8080, 9998, 50052)), 8080, 9998, 50052),
            vec![(8080, 9998, 50052)]
        );
        assert_eq!(
            device_port_attempts(None, 8080, 9998, 50052),
            vec![(8080, 9998, 50052)]
        );
    }

    #[test]
    fn usb_peer_survives_beacon_timeout() {
        let mut peer = usb_fixture();
        peer.apply_usb_probe("serial-a", 18080, 18081);
        peer.last_seen = Instant::now() - Duration::from_secs(60);
        let now = Instant::now();
        assert!(peer_still_active(&peer, Duration::from_millis(6000), now));
        assert_eq!(peer.link_class, LinkClass::Usb);
        peer.clear_usb();
        assert!(!peer_still_active(&peer, Duration::from_millis(6000), now));
        peer.last_seen = now;
        assert!(peer_still_active(&peer, Duration::from_millis(6000), now));
    }

    #[test]
    fn two_serial_probe_keeps_first_uuid_and_leaves_wifi_addr() {
        use crate::adb_links::{AdbLinkCache, AdbLinkRecord};
        use std::collections::HashMap;

        let shared = Uuid::from_u128(7);
        let vanished = Uuid::from_u128(8);
        let failed = Uuid::from_u128(9);
        let created = Uuid::from_u128(11);

        let mut wifi = usb_fixture();
        wifi.uuid = shared;
        let seen = wifi.last_seen;
        let mut gone = usb_fixture();
        gone.uuid = vanished;
        gone.addr = "10.0.0.8:8080".parse().unwrap();
        gone.apply_usb_probe("serial-old", 18084, 18085);
        let mut missed = usb_fixture();
        missed.uuid = failed;
        missed.addr = "10.0.0.9:8080".parse().unwrap();
        missed.apply_usb_probe("serial-d", 18086, 18087);

        let mut peers = HashMap::new();
        peers.insert(shared, wifi);
        peers.insert(vanished, gone);
        peers.insert(failed, missed);

        let mut cache = AdbLinkCache {
            links: vec![
                AdbLinkRecord {
                    serial: "serial-old".into(),
                    node_id: vanished,
                    device_api_port: 8080,
                    device_control_port: 9998,
                    device_rpc_port: 50052,
                },
                AdbLinkRecord {
                    serial: "serial-d".into(),
                    node_id: failed,
                    device_api_port: 1,
                    device_control_port: 2,
                    device_rpc_port: 3,
                },
            ],
        };
        let hits = vec![
            hit("serial-b", 18082, probe_state(shared, "phone-b")),
            hit("serial-a", 18080, probe_state(shared, "phone-a")),
            hit("serial-c", 18088, probe_state(created, "phone-c")),
        ];
        let present = vec![
            "serial-a".into(),
            "serial-b".into(),
            "serial-c".into(),
            "serial-d".into(),
        ];
        let listed = vec![
            ("serial-old".into(), 18084u16),
            ("serial-old".into(), 18085),
        ];
        let fold = fold_usb_probes(&mut peers, &mut cache, &hits, &present, &listed, 75);

        let winner = &peers[&shared];
        assert_eq!(winner.link_class, LinkClass::Usb);
        assert_eq!(winner.usb_serial.as_deref(), Some("serial-a"));
        assert_eq!(
            winner.usb_api_endpoint.as_deref(),
            Some("http://127.0.0.1:18080")
        );
        assert_eq!(
            winner.usb_control_endpoint.as_deref(),
            Some("http://127.0.0.1:18081")
        );
        assert_eq!(winner.api_endpoint(), "http://127.0.0.1:18080");
        assert_eq!(winner.addr, "192.168.1.20:9999".parse().unwrap());
        assert_eq!(winner.last_seen, seen);
        assert_eq!(winner.display_name, "phone-a");
        assert_eq!(cache.ports_for("serial-a"), Some((8080, 9998, 50052)));
        assert_eq!(cache.ports_for("serial-b"), Some((8080, 9998, 50052)));
        let doctor = crate::adb_links::duplicate_link_warnings(&cache);
        assert!(doctor
            .iter()
            .any(|line| line.contains("kept serial-a") && line.contains("tore down serial-b")));

        assert!(fold.winner_serials.contains(&"serial-a".to_string()));
        assert!(!fold.winner_serials.contains(&"serial-b".to_string()));
        assert!(fold
            .warnings
            .iter()
            .any(|line| { line.contains("serial-a") && line.contains("serial-b") }));
        assert!(fold.teardown.iter().any(|argv| {
            argv.iter().any(|arg| arg == "-s")
                && argv.iter().any(|arg| arg == "serial-b")
                && argv.iter().any(|arg| arg == "tcp:18082")
        }));
        assert!(!fold
            .teardown
            .iter()
            .any(|argv| { argv.iter().any(|arg| arg == "serial-a") }));
        assert!(fold
            .teardown
            .iter()
            .all(|argv| argv.iter().any(|arg| arg == "-s")));
        assert!(fold.teardown.iter().all(|argv| {
            let remove = argv.iter().position(|arg| arg == "--remove");
            let scoped = argv.iter().position(|arg| arg == "-s");
            match (remove, scoped) {
                (Some(remove_at), Some(serial_at)) => serial_at < remove_at,
                _ => true,
            }
        }));

        let stayed = &peers[&vanished];
        assert_eq!(stayed.link_class, LinkClass::Wifi);
        assert!(stayed.usb_serial.is_none());
        assert!(stayed.usb_api_endpoint.is_none());
        assert_eq!(stayed.addr, "10.0.0.8:8080".parse().unwrap());
        assert_eq!(stayed.api_endpoint(), "http://10.0.0.8:8080");
        assert!(cache.ports_for("serial-old").is_none());
        assert!(fold.teardown.iter().any(|argv| {
            argv.iter().any(|arg| arg == "serial-old") && argv.iter().any(|arg| arg == "-s")
        }));

        assert_eq!(cache.ports_for("serial-d"), Some((1, 2, 3)));
        let skipped = &peers[&failed];
        assert_eq!(skipped.link_class, LinkClass::Wifi);
        assert!(skipped.usb_api_endpoint.is_none());
        assert_eq!(skipped.addr, "10.0.0.9:8080".parse().unwrap());

        let fresh = &peers[&created];
        assert_eq!(fresh.link_class, LinkClass::Usb);
        assert_eq!(fresh.usb_serial.as_deref(), Some("serial-c"));
        assert_eq!(
            fresh.usb_api_endpoint.as_deref(),
            Some("http://127.0.0.1:18088")
        );
        assert!(fresh.addr.ip().is_unspecified());
        assert_eq!(cache.ports_for("serial-c"), Some((8080, 9998, 50052)));

        let present_now = vec!["serial-a".into(), "serial-d".into()];
        let fold_gone = fold_usb_probes(&mut peers, &mut cache, &[], &present_now, &[], 75);
        assert!(!peers.contains_key(&created));
        assert!(fold_gone.dropped.contains(&created));
        assert_eq!(peers[&shared].link_class, LinkClass::Wifi);
        assert_eq!(peers[&shared].addr, "192.168.1.20:9999".parse().unwrap());
    }

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
        apply_probe_hit(
            &mut peers,
            &hit("serial-a", 18080, advertised_state(id)),
            75,
        );
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
        apply_probe_hit(
            &mut peers,
            &hit("serial-a", 18080, advertised_state(id)),
            75,
        );
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
            cfg.write()
                .expect("config")
                .hardware
                .safety
                .max_ram_usage_percent = 60;
        }
        let id = Uuid::from_u128(11);
        let mut cache = AdbLinkCache::default();
        let mut old = probe_state(id, "phone");
        old.allocatable_memory_mb = 9_000;
        let hits = vec![hit("serial-a", 18080, old)];
        discovery
            .apply_folded_usb_probes(&mut cache, &hits, &["serial-a".into()], &[])
            .await;
        let peer = discovery
            .peers()
            .read()
            .await
            .get(&id)
            .cloned()
            .expect("peer");
        assert_eq!(peer.free_ram_mb, 15_000);
        assert_eq!(peer.backend, AccelerationBackend::GenericCpu);
        assert_eq!(budget_mb(&peer, 60), 9_000);
    }
}
