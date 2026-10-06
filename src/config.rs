use crate::node_identity::NodeIdentity;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};
use thiserror::Error;
use tracing::{debug, info};
use uuid::Uuid;

#[derive(Error, Debug)]
pub enum ConfigError {
    #[error("I/O error while accessing config file: {0}")]
    Io(#[from] std::io::Error),

    #[error("Failed to parse TOML configuration: {0}")]
    TomlParse(#[from] toml::de::Error),

    #[error("Failed to serialize TOML configuration: {0}")]
    TomlSerialize(#[from] toml::ser::Error),

    #[error("Invalid configuration: {0}")]
    Invalid(String),
}

/// Root configuration representation for ~/.nexus/config.toml
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct NexusConfig {
    #[serde(default)]
    pub node: NodeConfig,

    #[serde(default)]
    pub hardware: HardwareConfig,

    #[serde(default)]
    pub network: NetworkConfig,

    #[serde(default)]
    pub cluster: ClusterConfig,
}

impl Default for NexusConfig {
    fn default() -> Self {
        Self {
            node: NodeConfig::default(),
            hardware: HardwareConfig::default(),
            network: NetworkConfig::default(),
            cluster: ClusterConfig::default(),
        }
    }
}

impl NexusConfig {
    /// Load configuration from default location (~/.nexus/config.toml or $NEXUS_CONFIG).
    /// If the file does not exist, default configuration is saved and returned.
    pub fn load() -> Result<Self, ConfigError> {
        let path = if let Ok(custom_path) = std::env::var("NEXUS_CONFIG") {
            PathBuf::from(custom_path)
        } else {
            Self::default_config_path()
        };

        if !path.exists() {
            info!("Configuration file not found at {:?}. Generating default configuration.", path);
            let mut default_cfg = Self::default();
            default_cfg.ensure_identity()?;
            default_cfg.validate()?;
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent)?;
            }
            default_cfg.save_to_path(&path)?;
            return Ok(default_cfg);
        }

        Self::load_from_path(&path)
    }

    /// Load configuration from a specified file path.
    pub fn load_from_path<P: AsRef<Path>>(path: P) -> Result<Self, ConfigError> {
        let content = fs::read_to_string(path.as_ref())?;
        let mut config: NexusConfig = toml::from_str(&content)?;
        config.ensure_identity()?;
        config.validate()?;
        if config.node.id != config.original_id_from_content(&content)? {
            config.save_to_path(path.as_ref())?;
        }
        debug!("Loaded configuration: {:?}", config);
        Ok(config)
    }

    fn original_id_from_content(&self, content: &str) -> Result<String, ConfigError> {
        let raw: NexusConfig = toml::from_str(content)?;
        Ok(raw.node.id)
    }

    pub fn ensure_identity(&mut self) -> Result<(), ConfigError> {
        let identity = NodeIdentity::load_or_create(None)
            .map_err(|e| ConfigError::Invalid(format!("node identity: {e}")))?;
        if self.node.id.trim().is_empty() || self.node.id == "auto" {
            self.node.id = identity.node_id_from_public_key().to_string();
        }
        Uuid::parse_str(&self.node.id)
            .map_err(|_| ConfigError::Invalid("node.id must be a UUID".to_string()))?;
        Ok(())
    }

    /// Load Ed25519 identity and align `node.id` for fresh installs.
    pub fn load_node_identity(&self) -> Result<NodeIdentity, ConfigError> {
        NodeIdentity::load_or_create(None)
            .map_err(|e| ConfigError::Invalid(format!("node identity: {e}")))
    }

    pub fn node_uuid(&self) -> Result<Uuid, ConfigError> {
        Uuid::parse_str(&self.node.id)
            .map_err(|_| ConfigError::Invalid("node.id must be a UUID".to_string()))
    }

    /// Human-readable mesh name for mDNS TXT `name=` and local identity.
    /// Resolves `auto` / empty to the system hostname, then `nexus-<id8>`.
    pub fn resolved_display_name(&self) -> String {
        let name = self.node.name.trim();
        if !name.is_empty() && name != "auto" {
            return name.to_string();
        }
        if let Ok(hostname) = fs::read_to_string("/etc/hostname") {
            let hostname = hostname.trim();
            if !hostname.is_empty() {
                return hostname.to_string();
            }
        }
        let id = self.node.id.trim();
        let short = if id.len() >= 8 { &id[..8] } else { id };
        format!("nexus-{}", short)
    }

    pub fn validate(&self) -> Result<(), ConfigError> {
        if !matches!(self.node.role.as_str(), "host" | "client" | "worker" | "member" | "standalone") {
            return Err(ConfigError::Invalid(format!(
                "node.role must be host, client, worker, member, or standalone (got {})",
                self.node.role
            )));
        }
        self.network.validate()?;
        if self.network.anchors.primary_compute_id.is_some()
            && self.network.anchors.primary_compute_id == self.network.anchors.primary_client_id
        {
            return Err(ConfigError::Invalid(
                "network.anchors.primary_compute_id and primary_client_id must be distinct".to_string(),
            ));
        }
        if self.cluster.max_rpc_ram_mb > 1800 {
            return Err(ConfigError::Invalid(
                "cluster.max_rpc_ram_mb cannot exceed 1800 MB".to_string(),
            ));
        }
        Ok(())
    }

    /// Save configuration to a specified file path.
    pub fn save_to_path<P: AsRef<Path>>(&self, path: P) -> Result<(), ConfigError> {
        let toml_str = toml::to_string_pretty(self)?;
        fs::write(path, toml_str)?;
        Ok(())
    }

    /// Save configuration to default path (~/.nexus/config.toml or $NEXUS_CONFIG).
    pub fn save(&self) -> Result<(), ConfigError> {
        let path = if let Ok(custom_path) = std::env::var("NEXUS_CONFIG") {
            PathBuf::from(custom_path)
        } else {
            Self::default_config_path()
        };
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        self.save_to_path(&path)
    }

    /// Returns the default configuration path (~/.nexus/config.toml).
    pub fn default_config_path() -> PathBuf {
        let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
        PathBuf::from(home).join(".nexus").join("config.toml")
    }
}

/// Node identification and path configurations.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct NodeConfig {
    #[serde(default = "default_auto")]
    pub id: String,

    #[serde(default = "default_auto")]
    pub name: String,

    #[serde(default = "default_role")]
    pub role: String, // "host" on S23 Ultra, "client" on Mac

    #[serde(default)]
    pub runtime_role: RuntimeRole,

    #[serde(default)]
    pub capabilities: Vec<NodeCapability>,

    #[serde(default = "default_models_dir")]
    pub models_dir: PathBuf,

    #[serde(default = "default_presets_dir")]
    pub presets_dir: PathBuf,

    #[serde(default = "default_llama_server_binary")]
    pub llama_server_binary: String,

    #[serde(default = "default_rpc_server_binary")]
    pub rpc_server_binary: String,
}

impl Default for NodeConfig {
    fn default() -> Self {
        Self {
            id: default_auto(),
            name: default_auto(),
            role: default_role(),
            runtime_role: RuntimeRole::default(),
            capabilities: vec![NodeCapability::Discovery],
            models_dir: default_models_dir(),
            presets_dir: default_presets_dir(),
            llama_server_binary: default_llama_server_binary(),
            rpc_server_binary: default_rpc_server_binary(),
        }
    }
}

/// Hardware policies including acceleration and memory safety.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct HardwareConfig {
    #[serde(default)]
    pub acceleration: AccelerationConfig,

    #[serde(default)]
    pub safety: SafetyConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AccelerationConfig {
    #[serde(default = "default_true")]
    pub prefer_gpu: bool,

    #[serde(default = "default_gpu_layers")]
    pub gpu_layers: u32,

    #[serde(default = "default_true")]
    pub fallback_to_cpu: bool,

    #[serde(default = "default_cpu_threads")]
    pub cpu_threads: usize,

    #[serde(default = "default_cpu_threads_batch")]
    pub cpu_threads_batch: usize,
}

impl Default for AccelerationConfig {
    fn default() -> Self {
        Self {
            prefer_gpu: default_true(),
            gpu_layers: default_gpu_layers(),
            fallback_to_cpu: default_true(),
            cpu_threads: default_cpu_threads(),
            cpu_threads_batch: default_cpu_threads_batch(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SafetyConfig {
    #[serde(default = "default_max_ram_usage_percent")]
    pub max_ram_usage_percent: u8,

    #[serde(default = "default_true")]
    pub mmap: bool,

    #[serde(default = "default_false")]
    pub mlock: bool,
}

impl Default for SafetyConfig {
    fn default() -> Self {
        Self {
            max_ram_usage_percent: default_max_ram_usage_percent(),
            mmap: default_true(),
            mlock: default_false(),
        }
    }
}

/// Network endpoints and discovery settings.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct NetworkConfig {
    #[serde(default = "default_api_host")]
    pub api_host: String,

    #[serde(default = "default_api_port")]
    pub api_port: u16,

    /// Dedicated HTTP control-plane port (model load/unload/state). Distinct from
    /// `api_port` (llama-server OpenAI surface) and `discovery_port` (UDP beacon).
    #[serde(default = "default_control_port")]
    pub control_port: u16,

    #[serde(default = "default_discovery_port")]
    pub discovery_port: u16,

    #[serde(default = "default_broadcast_interval_ms")]
    pub broadcast_interval_ms: u64,

    #[serde(default = "default_peer_timeout_ms")]
    pub peer_timeout_ms: u64,

    #[serde(default)]
    pub static_peers: Vec<String>,

    #[serde(default)]
    pub default_host: Option<String>,

    #[serde(default)]
    pub discovery: DiscoveryConfig,

    #[serde(default)]
    pub security: SecurityConfig,

    #[serde(default)]
    pub anchors: AnchorConfig,
}

impl Default for NetworkConfig {
    fn default() -> Self {
        Self {
            api_host: default_api_host(),
            api_port: default_api_port(),
            control_port: default_control_port(),
            discovery_port: default_discovery_port(),
            broadcast_interval_ms: default_broadcast_interval_ms(),
            peer_timeout_ms: default_peer_timeout_ms(),
            static_peers: Vec::new(),
            default_host: None,
            discovery: DiscoveryConfig::default(),
            security: SecurityConfig::default(),
            anchors: AnchorConfig::default(),
        }
    }
}

impl NetworkConfig {
    fn validate(&self) -> Result<(), ConfigError> {
        if self.api_port == 0
            || self.control_port == 0
            || self.discovery_port == 0
            || self.discovery_port == self.api_port
            || self.control_port == self.api_port
            || self.control_port == self.discovery_port
        {
            return Err(ConfigError::Invalid(
                "network API, control, and discovery ports must be non-zero and pairwise distinct"
                    .to_string(),
            ));
        }
        if self.discovery.broadcast_interval_ms == 0 {
            return Err(ConfigError::Invalid(
                "network.discovery.broadcast_interval_ms must be greater than zero".to_string(),
            ));
        }
        if self.discovery.peer_timeout_ms < self.discovery.broadcast_interval_ms {
            return Err(ConfigError::Invalid(
                "network.discovery.peer_timeout_ms must be at least the broadcast interval".to_string(),
            ));
        }
        if self.discovery.max_peers == 0 {
            return Err(ConfigError::Invalid(
                "network.discovery.max_peers must be greater than zero".to_string(),
            ));
        }
        if !self.discovery.mdns.service_type.ends_with("._tcp.local.")
            && !self.discovery.mdns.service_type.ends_with("._udp.local.")
        {
            return Err(ConfigError::Invalid(
                "network.discovery.mdns.service_type must end with ._tcp.local. or ._udp.local."
                    .to_string(),
            ));
        }
        if self.security.protocol_version == 0 {
            return Err(ConfigError::Invalid(
                "network.security.protocol_version must be greater than zero".to_string(),
            ));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum RuntimeRole {
    Host,
    Client,
    Worker,
    Member,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum NodeCapability {
    Inference,
    Client,
    RpcWorker,
    Discovery,
}

impl Default for RuntimeRole {
    fn default() -> Self {
        Self::Host
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DiscoveryConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default = "default_discovery_version")]
    pub protocol_version: u8,
    #[serde(default = "default_broadcast_interval_ms")]
    pub broadcast_interval_ms: u64,
    #[serde(default = "default_peer_timeout_ms")]
    pub peer_timeout_ms: u64,
    #[serde(default = "default_max_peers")]
    pub max_peers: usize,
    #[serde(default)]
    pub mdns: MdnsConfig,
}

impl Default for DiscoveryConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            protocol_version: default_discovery_version(),
            broadcast_interval_ms: default_broadcast_interval_ms(),
            peer_timeout_ms: default_peer_timeout_ms(),
            max_peers: default_max_peers(),
            mdns: MdnsConfig::default(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct MdnsConfig {
    #[serde(default = "default_mdns_enabled")]
    pub enabled: bool,
    #[serde(default = "default_mdns_service_type")]
    pub service_type: String,
}

impl Default for MdnsConfig {
    fn default() -> Self {
        Self {
            enabled: default_mdns_enabled(),
            service_type: default_mdns_service_type(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PairedPeer {
    pub node_id: Uuid,
    pub public_key_hex: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SecurityConfig {
    #[serde(default = "default_security_protocol_version")]
    pub protocol_version: u16,
    #[serde(default)]
    pub require_pairing: bool,
    #[serde(default)]
    pub allowed_peer_ids: Vec<Uuid>,
    #[serde(default)]
    pub paired_peers: Vec<PairedPeer>,
}

impl Default for SecurityConfig {
    fn default() -> Self {
        Self {
            protocol_version: default_security_protocol_version(),
            require_pairing: false,
            allowed_peer_ids: Vec::new(),
            paired_peers: Vec::new(),
        }
    }
}

impl SecurityConfig {
    pub fn pairing_enforced(&self) -> bool {
        self.require_pairing || !self.allowed_peer_ids.is_empty()
    }

    pub fn public_key_for(&self, node_id: Uuid) -> Option<&str> {
        self.paired_peers
            .iter()
            .find(|peer| peer.node_id == node_id)
            .map(|peer| peer.public_key_hex.as_str())
    }

    pub fn record_pair(&mut self, node_id: Uuid, public_key_hex: String) {
        if !self.allowed_peer_ids.contains(&node_id) {
            self.allowed_peer_ids.push(node_id);
        }
        if let Some(existing) = self.paired_peers.iter_mut().find(|p| p.node_id == node_id) {
            existing.public_key_hex = public_key_hex;
        } else {
            self.paired_peers.push(PairedPeer {
                node_id,
                public_key_hex,
            });
        }
        self.require_pairing = true;
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct AnchorConfig {
    #[serde(default)]
    pub primary_compute_id: Option<Uuid>,
    #[serde(default)]
    pub primary_client_id: Option<Uuid>,
}

/// Cluster and distributed RPC layer offload configurations.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ClusterConfig {
    #[serde(default = "default_true")]
    pub enable_rpc: bool,

    #[serde(default = "default_rpc_port")]
    pub rpc_port: u16,

    #[serde(default = "default_max_rpc_ram_mb")]
    pub max_rpc_ram_mb: u64,

    #[serde(default = "default_true")]
    pub auto_offload: bool,

    #[serde(default = "default_true")]
    pub prefer_adb_tunnel: bool,
}

impl Default for ClusterConfig {
    fn default() -> Self {
        Self {
            enable_rpc: true,
            rpc_port: default_rpc_port(),
            max_rpc_ram_mb: default_max_rpc_ram_mb(),
            auto_offload: true,
            prefer_adb_tunnel: true,
        }
    }
}

// Default helper functions
fn default_auto() -> String {
    "auto".to_string()
}

fn default_role() -> String {
    "host".to_string()
}

fn default_models_dir() -> PathBuf {
    expand_tilde("~/nexus-models")
}

fn default_presets_dir() -> PathBuf {
    expand_tilde("~/.nexus/presets")
}

fn default_llama_server_binary() -> String {
    "llama-server".to_string()
}

fn default_rpc_server_binary() -> String {
    "rpc-server".to_string()
}

fn default_true() -> bool {
    true
}

fn default_false() -> bool {
    false
}

fn default_gpu_layers() -> u32 {
    99
}

fn default_cpu_threads() -> usize {
    6
}

fn default_cpu_threads_batch() -> usize {
    6
}

fn default_max_ram_usage_percent() -> u8 {
    75
}

fn default_api_host() -> String {
    "0.0.0.0".to_string()
}

fn default_api_port() -> u16 {
    8080
}

fn default_control_port() -> u16 {
    9998
}

fn default_discovery_port() -> u16 {
    9999
}

fn default_broadcast_interval_ms() -> u64 {
    2000
}

fn default_peer_timeout_ms() -> u64 {
    6000
}

fn default_rpc_port() -> u16 {
    50052
}

fn default_max_rpc_ram_mb() -> u64 {
    1800
}

fn default_discovery_version() -> u8 {
    1
}

fn default_security_protocol_version() -> u16 {
    1
}

fn default_max_peers() -> usize {
    64
}

fn default_mdns_enabled() -> bool {
    true
}

fn default_mdns_service_type() -> String {
    "_nexus._tcp.local.".to_string()
}

/// Expand tilde prefix in paths to $HOME.
pub fn expand_tilde<P: AsRef<Path>>(path: P) -> PathBuf {
    let p = path.as_ref();
    if let Ok(stripped) = p.strip_prefix("~") {
        let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
        PathBuf::from(home).join(stripped)
    } else {
        p.to_path_buf()
    }
}
