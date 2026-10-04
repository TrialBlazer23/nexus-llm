use crate::config::NexusConfig;
use crate::sysinfo::{AccelerationBackend, SystemProfile};
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
        let null_pos = model_slice.iter().position(|&b| b == 0).unwrap_or(model_slice.len());
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
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PeerNode {
    pub uuid: Uuid,
    pub addr: SocketAddr,
    pub role: NodeRole,
    pub status: StatusFlags,
    pub api_port: u16,
    pub rpc_port: u16,
    pub total_ram_mb: u32,
    pub free_ram_mb: u32,
    pub backend: AccelerationBackend,
    pub thermal_index: u8,
    pub active_model: String,
    #[serde(skip, default = "Instant::now")]
    pub last_seen: Instant,
}

impl PeerNode {
    pub fn api_endpoint(&self) -> String {
        format!("http://{}:{}", self.addr.ip(), self.api_port)
    }

    pub fn is_rpc_ready(&self) -> bool {
        self.status.is_rpc_ready() && self.rpc_port > 0
    }

    pub fn rpc_endpoint(&self) -> String {
        format!("{}:{}", self.addr.ip(), self.rpc_port)
    }
}

/// Autonomous UDP discovery and peer caching service.
pub struct DiscoveryService {
    config: NexusConfig,
    node_uuid: Uuid,
    peers: Arc<RwLock<HashMap<Uuid, PeerNode>>>,
    active_model: Arc<RwLock<String>>,
    status_flags: Arc<RwLock<StatusFlags>>,
    rpc_port: Arc<RwLock<u16>>,
}

impl DiscoveryService {
    pub fn new(config: NexusConfig, custom_uuid: Option<Uuid>) -> Self {
        let node_uuid = custom_uuid.unwrap_or_else(Uuid::new_v4);
        let default_status = if config.hardware.acceleration.prefer_gpu {
            StatusFlags(StatusFlags::READY.0 | StatusFlags::VULKAN_ACTIVE.0)
        } else {
            StatusFlags::READY
        };

        Self {
            config,
            node_uuid,
            peers: Arc::new(RwLock::new(HashMap::new())),
            active_model: Arc::new(RwLock::new(String::new())),
            status_flags: Arc::new(RwLock::new(default_status)),
            rpc_port: Arc::new(RwLock::new(0)),
        }
    }

    pub fn node_uuid(&self) -> Uuid {
        self.node_uuid
    }

    pub fn peers(&self) -> Arc<RwLock<HashMap<Uuid, PeerNode>>> {
        self.peers.clone()
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

    pub fn config(&self) -> &NexusConfig {
        &self.config
    }

    /// Read thermal index from Linux thermal zone (0 to 100 scale).
    pub fn probe_thermal_index() -> u8 {
        // Read /sys/class/thermal/thermal_zone0/temp (in millidegrees C)
        if let Ok(content) = std::fs::read_to_string("/sys/class/thermal/thermal_zone0/temp") {
            if let Ok(milli_c) = content.trim().parse::<i64>() {
                let temp_c = milli_c as f32 / 1000.0;
                if temp_c <= 40.0 {
                    return 0;
                } else if temp_c >= 80.0 {
                    return 100;
                } else {
                    return (((temp_c - 40.0) / 40.0) * 100.0).round() as u8;
                }
            }
        }
        0 // Nominal fallback if thermal zone unreadable
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
            let addr = SocketAddr::new(std::net::IpAddr::V4(bcast_ip), config.network.discovery_port);
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

    /// Transmit an immediate discovery probe across all broadcast and peer targets.
    pub async fn send_probe(&self) {
        let socket = match UdpSocket::bind("0.0.0.0:0").await {
            Ok(s) => s,
            Err(e) => {
                debug!("Failed to bind UDP probe socket: {}", e);
                return;
            }
        };
        let _ = socket.set_broadcast(true);

        let profile = SystemProfile::probe();
        let thermal_index = Self::probe_thermal_index();
        let active_model = self.active_model.read().await.clone();
        let status = *self.status_flags.read().await;
        let rpc_port = *self.rpc_port.read().await;

        let beacon = BeaconPacket {
            magic: BEACON_MAGIC,
            version: BEACON_VERSION,
            role: NodeRole::from_str_role(&self.config.node.role),
            status,
            uuid: self.node_uuid,
            api_port: self.config.network.api_port,
            rpc_port,
            total_ram_mb: profile.total_ram_mb as u32,
            free_ram_mb: profile.available_ram_mb as u32,
            backend: profile.detected_backend,
            thermal_index,
            active_model,
        };

        let packet_bytes = beacon.encode();
        let targets = Self::get_broadcast_targets(&self.config);
        for target in targets {
            let _ = socket.send_to(&packet_bytes, target).await;
        }
    }

    /// Spawn asynchronous UDP beacon broadcaster task.
    pub fn start_broadcaster(self: Arc<Self>) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
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

            let initial_targets = Self::get_broadcast_targets(&self.config);
            info!(
                "Discovery broadcaster active: transmitting beacons to {:?} every {} ms",
                initial_targets, self.config.network.broadcast_interval_ms
            );

            let interval = Duration::from_millis(self.config.network.broadcast_interval_ms);
            let mut ticker = tokio::time::interval(interval);

            loop {
                ticker.tick().await;

                let profile = SystemProfile::probe();
                let thermal_index = Self::probe_thermal_index();
                let active_model = self.active_model.read().await.clone();
                let status = *self.status_flags.read().await;
                let rpc_port = *self.rpc_port.read().await;

                let beacon = BeaconPacket {
                    magic: BEACON_MAGIC,
                    version: BEACON_VERSION,
                    role: NodeRole::from_str_role(&self.config.node.role),
                    status,
                    uuid: self.node_uuid,
                    api_port: self.config.network.api_port,
                    rpc_port,
                    total_ram_mb: profile.total_ram_mb as u32,
                    free_ram_mb: profile.available_ram_mb as u32,
                    backend: profile.detected_backend,
                    thermal_index,
                    active_model,
                };

                let packet_bytes = beacon.encode();
                let targets = Self::get_broadcast_targets(&self.config);
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
        tokio::spawn(async move {
            let socket = match create_listener_socket(self.config.network.discovery_port) {
                Ok(s) => s,
                Err(e) => {
                    error!("Failed to bind discovery listener to port {}: {}", self.config.network.discovery_port, e);
                    return;
                }
            };

            info!("Discovery listener active on port {}", self.config.network.discovery_port);
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

                                    debug!("Received valid beacon from {:?} ({:?})", peer_addr, beacon.uuid);
                                    let peer = PeerNode {
                                        uuid: beacon.uuid,
                                        addr: SocketAddr::new(peer_addr.ip(), beacon.api_port),
                                        role: beacon.role,
                                        status: beacon.status,
                                        api_port: beacon.api_port,
                                        rpc_port: beacon.rpc_port,
                                        total_ram_mb: beacon.total_ram_mb,
                                        free_ram_mb: beacon.free_ram_mb,
                                        backend: beacon.backend,
                                        thermal_index: beacon.thermal_index,
                                        active_model: beacon.active_model,
                                        last_seen: Instant::now(),
                                    };

                                    let mut peers = self.peers.write().await;
                                    peers.insert(beacon.uuid, peer);

                                    // If this node is a host and received a client beacon/probe, reply unicast immediately
                                    let is_host_node = NodeRole::from_str_role(&self.config.node.role).is_host();
                                    if is_host_node && beacon.role.is_client() {
                                        let reply_addr = SocketAddr::new(peer_addr.ip(), self.config.network.discovery_port);
                                        let profile = SystemProfile::probe();
                                        let reply_beacon = BeaconPacket {
                                            magic: BEACON_MAGIC,
                                            version: BEACON_VERSION,
                                            role: NodeRole::HOST,
                                            status: *self.status_flags.read().await,
                                            uuid: self.node_uuid,
                                            api_port: self.config.network.api_port,
                                            rpc_port: *self.rpc_port.read().await,
                                            total_ram_mb: profile.total_ram_mb as u32,
                                            free_ram_mb: profile.available_ram_mb as u32,
                                            backend: profile.detected_backend,
                                            thermal_index: Self::probe_thermal_index(),
                                            active_model: self.active_model.read().await.clone(),
                                        };
                                        let reply_bytes = reply_beacon.encode();
                                        let _ = socket.send_to(&reply_bytes, reply_addr).await;
                                    }
                                }
                                Err(e) => {
                                    trace!("Discarding invalid discovery packet from {}: {}", peer_addr, e);
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

    /// Retrieve active peers, evicting any whose last beacon is older than `peer_timeout_ms` (6000 ms).
    pub async fn get_active_peers(&self) -> Vec<PeerNode> {
        let timeout = Duration::from_millis(self.config.network.peer_timeout_ms);
        let mut peers = self.peers.write().await;

        peers.retain(|_, peer| peer.last_seen.elapsed() <= timeout);
        peers.values().cloned().collect()
    }

    /// Select the best compute host peer currently available on the subnet.
    pub async fn find_best_host(&self) -> Option<PeerNode> {
        let active = self.get_active_peers().await;
        active
            .into_iter()
            .filter(|p| p.role.is_host())
            .max_by_key(|p| {
                // Priority: Ready state (1000 pts) + Vulkan active (500 pts) + free RAM (MB) - thermal index
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

    /// Select the best RPC peer node currently available on the subnet (highest free RAM).
    pub async fn find_best_rpc_peer(&self) -> Option<PeerNode> {
        let active = self.get_active_peers().await;
        active
            .into_iter()
            .filter(|p| p.is_rpc_ready())
            .max_by_key(|p| p.free_ram_mb)
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

            if is_up && !is_loopback && is_broadcast && !item.ifa_addr.is_null() {
                if (*item.ifa_addr).sa_family as i32 == libc::AF_INET {
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


