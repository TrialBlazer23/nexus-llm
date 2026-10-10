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
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct NexusConfig {
    #[serde(default)]
    pub node: NodeConfig,

    #[serde(default)]
    pub hardware: HardwareConfig,

    #[serde(default)]
    pub network: NetworkConfig,

    #[serde(default)]
    pub cluster: ClusterConfig,

    #[serde(default)]
    pub ui: UiConfig,

    #[serde(default)]
    pub huggingface: HuggingFaceConfig,

    #[serde(default)]
    pub inference: InferenceConfig,
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
            info!(
                "Configuration file not found at {:?}. Generating default configuration.",
                path
            );
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

    /// Resolve Hugging Face Personal Access Token.
    /// Checks hierarchy:
    /// 1. Explicitly configured token in `config.toml` (`[huggingface] token`)
    /// 2. `HF_TOKEN` environment variable
    /// 3. `HUGGING_FACE_HUB_TOKEN` environment variable
    /// 4. Standard CLI cache file at `~/.cache/huggingface/token`
    pub fn resolved_hf_token(&self) -> Option<String> {
        if let Some(tok) = &self.huggingface.token {
            let t = tok.trim();
            if !t.is_empty() {
                return Some(t.to_string());
            }
        }
        if let Ok(tok) = std::env::var("HF_TOKEN") {
            let t = tok.trim();
            if !t.is_empty() {
                return Some(t.to_string());
            }
        }
        if let Ok(tok) = std::env::var("HUGGING_FACE_HUB_TOKEN") {
            let t = tok.trim();
            if !t.is_empty() {
                return Some(t.to_string());
            }
        }
        if let Ok(home) = std::env::var("HOME") {
            let path = Path::new(&home)
                .join(".cache")
                .join("huggingface")
                .join("token");
            if let Ok(content) = fs::read_to_string(path) {
                let t = content.trim();
                if !t.is_empty() {
                    return Some(t.to_string());
                }
            }
        }
        None
    }

    pub fn validate(&self) -> Result<(), ConfigError> {
        if !matches!(
            self.node.role.as_str(),
            "host" | "client" | "worker" | "member" | "standalone"
        ) {
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
                "network.anchors.primary_compute_id and primary_client_id must be distinct"
                    .to_string(),
            ));
        }
        if self.cluster.max_rpc_ram_mb == 0 {
            return Err(ConfigError::Invalid(
                "cluster.max_rpc_ram_mb must be greater than 0".to_string(),
            ));
        }
        // No global 1800 MB ceiling — worker caps are per-node (Phase 11 §3.3).
        self.inference.moe.validate()?;
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

    /// Battery floor percent on mobile devices (default 20%).
    #[serde(default = "default_battery_floor_percent")]
    pub battery_floor_percent: u8,

    /// Action when below battery floor: "decline", "throttle", or "ignore".
    #[serde(default = "default_battery_action")]
    pub battery_action: String,
}

impl Default for SafetyConfig {
    fn default() -> Self {
        Self {
            max_ram_usage_percent: default_max_ram_usage_percent(),
            mmap: default_true(),
            mlock: default_false(),
            battery_floor_percent: default_battery_floor_percent(),
            battery_action: default_battery_action(),
        }
    }
}

fn default_battery_floor_percent() -> u8 {
    20
}

fn default_battery_action() -> String {
    "decline".to_string()
}

/// Inference engine settings and KV cache slot persistence (Phase 12 §5.2).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct InferenceConfig {
    #[serde(default)]
    pub cache: PromptCacheConfig,

    /// MoE flash-streaming backend (BigMoeOnEdge `bmoe-cli`). Phase 16.
    #[serde(default)]
    pub moe: MoeConfig,
}

/// Which side of the MoE budget to protect when context and cache cannot both fit.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum MoeCachePreference {
    /// Keep the requested context. Shrink the expert cache before stepping context down.
    #[default]
    Context,
    /// Keep the working-set cache. Step context down before shrinking that cache.
    Cache,
}

/// Quality mode for MoE streaming knobs.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum MoeQualityMode {
    /// Lossless: byte-identical to fully resident inference.
    #[default]
    Lossless,
    /// Allows lossy / experimental knobs (non-reproducible).
    Lossy,
}

/// BigMoeOnEdge expert-streaming configuration (`[inference.moe]`).
///
/// Distinct from [`PromptCacheConfig::max_cache_mb`] (prompt-slot *disk* quota).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct MoeConfig {
    // --- Operator knobs (setup defaults are enough for most nodes) ---
    /// When true, prefer `bmoe-cli --moe-stream` for streamable MoE GGUFs that exceed dense RAM.
    #[serde(default = "default_true")]
    pub enabled: bool,

    /// Path or PATH name of the BigMoeOnEdge CLI binary (`scripts/setup.sh` writes `~/.nexus/bin/bmoe-cli`).
    #[serde(default = "default_bmoe_binary")]
    pub bmoe_binary: String,

    /// Expert LRU cache budget: `"auto"`, `"0"`, or an integer MiB string (≥1500 when set).
    #[serde(default = "default_moe_cache_mb")]
    pub cache_mb: String,

    // --- Expert knobs (TOML-only; leave at defaults unless tuning) ---
    /// RAM left free under `auto` cache sizing (MiB).
    #[serde(default = "default_moe_cache_floor_mb")]
    pub cache_floor_mb: u64,

    /// Operator hard cap (MiB). `0` means no cap beyond the LMK room and working set.
    #[serde(default)]
    pub cache_ceil_mb: u64,

    /// Smallest legal non-zero expert cache (MiB). BigMoe rejects the open interval below this.
    /// `0` disables the floor. Otherwise must be >= 1500.
    #[serde(default = "default_moe_min_cache_mb")]
    pub min_cache_mb: u64,

    /// `context` keeps the requested context and shrinks the expert cache first.
    /// `cache` holds the working-set cache and steps context down first.
    #[serde(default)]
    pub prefer: MoeCachePreference,

    /// When true, bench hit-rate may raise, hold, or (only to recover context) lower the cache target.
    #[serde(default = "default_true")]
    pub adapt: bool,

    /// At or above this hit percent, do not shrink cache below the measured warm size to buy context.
    #[serde(default = "default_moe_warm_hit_pct")]
    pub warm_hit_pct: u8,

    /// Below this hit percent, raise the cache target toward the LMK room.
    #[serde(default = "default_moe_cold_hit_pct")]
    pub cold_hit_pct: u8,

    /// Below this hit percent, with enough samples, a lossy session may drop cold experts.
    #[serde(default = "default_moe_chronic_hit_pct")]
    pub chronic_hit_pct: u8,

    /// Hit samples required before the chronic lossy overlay.
    #[serde(default = "default_moe_chronic_min_samples")]
    pub chronic_min_samples: u16,

    /// Session-only `--drop-cold-experts` value used when the chronic rule fires.
    #[serde(default = "default_moe_chronic_drop_cold")]
    pub chronic_drop_cold: String,

    /// Cold-start multiplier on one-step active expert bytes. Bench hit-rate replaces it.
    #[serde(default = "default_moe_working_set_factor")]
    pub working_set_factor: u32,

    /// Cold-start tok/s prior in milli-tokens/sec (2200 = 2.20). Ignored once a sample exists.
    #[serde(default = "default_moe_reference_tok_millis")]
    pub reference_tok_millis: u32,

    /// Top-k used when the GGUF omits `expert_used_count`.
    #[serde(default = "default_moe_reference_active_experts")]
    pub reference_active_experts: u32,

    /// Expert count used when the GGUF omits `expert_count`.
    #[serde(default = "default_moe_reference_expert_count")]
    pub reference_expert_count: u32,

    /// Parallel O_DIRECT read lanes (1–8).
    #[serde(default = "default_moe_io_threads")]
    pub io_threads: u8,

    /// Dense weight residency: `mmap` | `warm` | `anon` | `ahwb`.
    #[serde(default = "default_moe_dense_weights")]
    pub dense_weights: String,

    /// Overlap expert I/O with FFN compute (requires Helldez expert-ready llama.cpp build).
    #[serde(default)]
    pub overlap: bool,

    /// Lossy: skip cold cache-miss experts below threshold (0.0–1.0 as percent*100 stored? use string).
    /// Stored as basis points of the BigMoe `F` factor ×1000 for serde-eq friendliness; prefer string.
    #[serde(default)]
    pub drop_cold_experts: Option<String>,

    /// Lossy: prefer cached experts within margin L.
    #[serde(default)]
    pub expert_substitute: Option<String>,

    /// Lossy/experimental: commit routing N layers early.
    #[serde(default)]
    pub route_ahead: Option<u32>,

    #[serde(default)]
    pub quality_mode: MoeQualityMode,
}

impl Default for MoeConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            bmoe_binary: default_bmoe_binary(),
            cache_mb: default_moe_cache_mb(),
            cache_floor_mb: default_moe_cache_floor_mb(),
            cache_ceil_mb: 0,
            min_cache_mb: default_moe_min_cache_mb(),
            prefer: MoeCachePreference::Context,
            adapt: true,
            warm_hit_pct: default_moe_warm_hit_pct(),
            cold_hit_pct: default_moe_cold_hit_pct(),
            chronic_hit_pct: default_moe_chronic_hit_pct(),
            chronic_min_samples: default_moe_chronic_min_samples(),
            chronic_drop_cold: default_moe_chronic_drop_cold(),
            working_set_factor: default_moe_working_set_factor(),
            reference_tok_millis: default_moe_reference_tok_millis(),
            reference_active_experts: default_moe_reference_active_experts(),
            reference_expert_count: default_moe_reference_expert_count(),
            io_threads: default_moe_io_threads(),
            dense_weights: default_moe_dense_weights(),
            overlap: false,
            drop_cold_experts: None,
            expert_substitute: None,
            route_ahead: None,
            quality_mode: MoeQualityMode::Lossless,
        }
    }
}

impl MoeConfig {
    /// Cold-start tok/s prior. Measured bench samples replace this.
    pub fn reference_tok_s(&self) -> f32 {
        self.reference_tok_millis as f32 / 1000.0
    }

    /// Resolve `--cache-mb` CLI value (passes through `auto` / `0` / integer).
    pub fn resolved_cache_mb_arg(&self) -> String {
        let trimmed = self.cache_mb.trim();
        if trimmed.is_empty() {
            "auto".to_string()
        } else {
            trimmed.to_string()
        }
    }

    /// Whether lossy MoE knobs may be applied.
    pub fn lossy_allowed(&self) -> bool {
        matches!(self.quality_mode, MoeQualityMode::Lossy)
    }

    pub fn validate(&self) -> Result<(), ConfigError> {
        if self.bmoe_binary.trim().is_empty() {
            return Err(ConfigError::Invalid(
                "inference.moe.bmoe_binary must not be empty".into(),
            ));
        }
        if !(1..=8).contains(&self.io_threads) {
            return Err(ConfigError::Invalid(
                "inference.moe.io_threads must be 1..=8".into(),
            ));
        }
        let dense = self.dense_weights.as_str();
        if !matches!(dense, "mmap" | "warm" | "anon" | "ahwb") {
            return Err(ConfigError::Invalid(format!(
                "inference.moe.dense_weights must be mmap|warm|anon|ahwb (got {dense})"
            )));
        }
        let cache = self.cache_mb.trim();
        if cache != "auto" && cache != "0" {
            let n: u64 = cache.parse().map_err(|_| {
                ConfigError::Invalid(
                    "inference.moe.cache_mb must be auto, 0, or an integer MiB".into(),
                )
            })?;
            let floor = if self.min_cache_mb == 0 {
                1
            } else {
                self.min_cache_mb
            };
            if (1..floor).contains(&n) {
                return Err(ConfigError::Invalid(format!(
                    "inference.moe.cache_mb must be 0 or >= {floor} (min_cache_mb)"
                )));
            }
        }
        if self.min_cache_mb != 0 && self.min_cache_mb < 1500 {
            return Err(ConfigError::Invalid(
                "inference.moe.min_cache_mb must be 0 or >= 1500".into(),
            ));
        }
        if self.working_set_factor < 1 {
            return Err(ConfigError::Invalid(
                "inference.moe.working_set_factor must be >= 1".into(),
            ));
        }
        if self.reference_tok_millis < 1 {
            return Err(ConfigError::Invalid(
                "inference.moe.reference_tok_millis must be >= 1".into(),
            ));
        }
        if self.reference_active_experts < 1 || self.reference_expert_count < 1 {
            return Err(ConfigError::Invalid(
                "inference.moe reference expert counts must be >= 1".into(),
            ));
        }
        if !(self.warm_hit_pct > self.cold_hit_pct && self.cold_hit_pct > self.chronic_hit_pct) {
            return Err(ConfigError::Invalid(
                "inference.moe hit percents must satisfy warm > cold > chronic".into(),
            ));
        }
        match self.chronic_drop_cold.trim().parse::<f32>() {
            Ok(v) if v >= 0.0 => {}
            _ => {
                return Err(ConfigError::Invalid(
                    "inference.moe.chronic_drop_cold must be a non-negative number".into(),
                ));
            }
        }
        if matches!(self.quality_mode, MoeQualityMode::Lossless)
            && (self.drop_cold_experts.is_some()
                || self.expert_substitute.is_some()
                || self.route_ahead.is_some_and(|n| n > 0))
        {
            return Err(ConfigError::Invalid(
                "lossy MoE knobs require inference.moe.quality_mode = \"lossy\"".into(),
            ));
        }
        Ok(())
    }
}

fn default_bmoe_binary() -> String {
    "bmoe-cli".to_string()
}

fn default_moe_cache_mb() -> String {
    "auto".to_string()
}

fn default_moe_cache_floor_mb() -> u64 {
    1536
}

fn default_moe_min_cache_mb() -> u64 {
    2000
}

fn default_moe_warm_hit_pct() -> u8 {
    70
}

fn default_moe_cold_hit_pct() -> u8 {
    40
}

fn default_moe_chronic_hit_pct() -> u8 {
    25
}

fn default_moe_chronic_min_samples() -> u16 {
    3
}

fn default_moe_chronic_drop_cold() -> String {
    "0.85".to_string()
}

fn default_moe_working_set_factor() -> u32 {
    4
}

fn default_moe_reference_tok_millis() -> u32 {
    2200
}

fn default_moe_reference_active_experts() -> u32 {
    8
}

fn default_moe_reference_expert_count() -> u32 {
    128
}

fn default_moe_io_threads() -> u8 {
    4
}

fn default_moe_dense_weights() -> String {
    "anon".to_string()
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PromptCacheConfig {
    /// Directory where llama-server persists prompt cache slots (--slot-save-path).
    #[serde(default = "default_slot_save_path")]
    pub slot_save_path: String,

    /// Whether prompt cache slot saving is enabled.
    #[serde(default = "default_true")]
    pub prompt_cache_enabled: bool,

    /// Maximum disk space (MB) allowed for prompt cache slots before LRU eviction.
    #[serde(default = "default_max_cache_mb")]
    pub max_cache_mb: u64,
}

impl Default for PromptCacheConfig {
    fn default() -> Self {
        Self {
            slot_save_path: default_slot_save_path(),
            prompt_cache_enabled: default_true(),
            max_cache_mb: default_max_cache_mb(),
        }
    }
}

fn default_slot_save_path() -> String {
    "~/.nexus/slots".to_string()
}

fn default_max_cache_mb() -> u64 {
    2048
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

    /// Mesh OpenAI gateway port (Phase 12 §5.1). Reverse-proxies `/v1/*` to the
    /// active model holder. Distinct from `api_port` / `control_port` / `discovery_port`.
    #[serde(default = "default_gateway_port")]
    pub gateway_port: u16,

    /// When true, hub/nexusd/host spawn the mesh gateway on `gateway_port`.
    #[serde(default = "default_true")]
    pub gateway_enabled: bool,

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
            gateway_port: default_gateway_port(),
            gateway_enabled: true,
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
        if self.gateway_enabled
            && (self.gateway_port == 0
                || self.gateway_port == self.api_port
                || self.gateway_port == self.control_port
                || self.gateway_port == self.discovery_port)
        {
            return Err(ConfigError::Invalid(
                "network.gateway_port must be non-zero and distinct from api, control, and discovery ports"
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
                "network.discovery.peer_timeout_ms must be at least the broadcast interval"
                    .to_string(),
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
#[derive(Default)]
pub enum RuntimeRole {
    #[default]
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
    /// Node can host BigMoeOnEdge flash-streaming MoE sessions.
    MoeStream,
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
    pub allow_unpaired_lan: bool,
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
            allow_unpaired_lan: true,
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

/// UI and terminal display preferences.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct UiConfig {
    #[serde(default = "default_layout_mode")]
    pub layout_mode: String,
}

impl Default for UiConfig {
    fn default() -> Self {
        Self {
            layout_mode: default_layout_mode(),
        }
    }
}

/// Hugging Face hub integration settings.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct HuggingFaceConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token: Option<String>,
}

// Default helper functions
fn default_layout_mode() -> String {
    "auto".to_string()
}

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

fn default_gateway_port() -> u16 {
    8090
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
