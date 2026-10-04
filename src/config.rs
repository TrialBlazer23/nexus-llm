use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};
use thiserror::Error;
use tracing::{debug, info};

#[derive(Error, Debug)]
pub enum ConfigError {
    #[error("I/O error while accessing config file: {0}")]
    Io(#[from] std::io::Error),

    #[error("Failed to parse TOML configuration: {0}")]
    TomlParse(#[from] toml::de::Error),

    #[error("Failed to serialize TOML configuration: {0}")]
    TomlSerialize(#[from] toml::ser::Error),
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
}

impl Default for NexusConfig {
    fn default() -> Self {
        Self {
            node: NodeConfig::default(),
            hardware: HardwareConfig::default(),
            network: NetworkConfig::default(),
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
            let default_cfg = Self::default();
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
        let content = fs::read_to_string(path)?;
        let config: NexusConfig = toml::from_str(&content)?;
        debug!("Loaded configuration: {:?}", config);
        Ok(config)
    }

    /// Save configuration to a specified file path.
    pub fn save_to_path<P: AsRef<Path>>(&self, path: P) -> Result<(), ConfigError> {
        let toml_str = toml::to_string_pretty(self)?;
        fs::write(path, toml_str)?;
        Ok(())
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

    #[serde(default = "default_models_dir")]
    pub models_dir: PathBuf,

    #[serde(default = "default_presets_dir")]
    pub presets_dir: PathBuf,
}

impl Default for NodeConfig {
    fn default() -> Self {
        Self {
            id: default_auto(),
            name: default_auto(),
            role: default_role(),
            models_dir: default_models_dir(),
            presets_dir: default_presets_dir(),
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

    #[serde(default = "default_discovery_port")]
    pub discovery_port: u16,

    #[serde(default = "default_broadcast_interval_ms")]
    pub broadcast_interval_ms: u64,

    #[serde(default = "default_peer_timeout_ms")]
    pub peer_timeout_ms: u64,

    #[serde(default)]
    pub static_peers: Vec<String>,
}

impl Default for NetworkConfig {
    fn default() -> Self {
        Self {
            api_host: default_api_host(),
            api_port: default_api_port(),
            discovery_port: default_discovery_port(),
            broadcast_interval_ms: default_broadcast_interval_ms(),
            peer_timeout_ms: default_peer_timeout_ms(),
            static_peers: Vec::new(),
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

fn default_discovery_port() -> u16 {
    9999
}

fn default_broadcast_interval_ms() -> u64 {
    2000
}

fn default_peer_timeout_ms() -> u64 {
    6000
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
