use crate::config::{ConfigError, NexusConfig};
use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph},
    Frame,
};

pub const VALID_ROLES: &[&str] = &["host", "client", "worker", "member", "standalone"];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettingType {
    // Node Identity
    NodeName,
    NodeRole,
    // Paths & Binaries
    ModelsDir,
    PresetsDir,
    LlamaServerBinary,
    RpcServerBinary,
    // Network & Transport
    DefaultHost,
    StaticPeers,
    EnableMdns,
    DiscoveryPort,
    ApiPort,
    ControlPort,
    PreferAdbTunnel,
    // Hardware & Acceleration
    PreferGpu,
    GpuLayers,
    CpuThreads,
    // Memory & Android LMK Safeguards
    MaxRamPercent,
    Mmap,
    // Distributed Cluster RPC
    EnableRpc,
    MaxRpcRamMb,
    AutoOffload,
}

#[derive(Debug, Clone)]
pub struct SettingItem {
    pub category: &'static str,
    pub name: &'static str,
    pub description: &'static str,
    pub setting_type: SettingType,
}

pub const SETTING_ITEMS: &[SettingItem] = &[
    // Node Identity
    SettingItem {
        category: "Node Identity",
        name: "Node Hostname / Name",
        description: "Unique human-readable identifier for this node in the mesh",
        setting_type: SettingType::NodeName,
    },
    SettingItem {
        category: "Node Identity",
        name: "Node Mesh Role",
        description: "Operational role (host, client, worker, member, standalone)",
        setting_type: SettingType::NodeRole,
    },
    // Paths & Binaries
    SettingItem {
        category: "Paths & Binaries",
        name: "Models Storage Directory",
        description: "Filesystem directory scanned for .gguf model weights",
        setting_type: SettingType::ModelsDir,
    },
    SettingItem {
        category: "Paths & Binaries",
        name: "Presets Directory",
        description: "Directory storing chat templates and persona YAML definitions",
        setting_type: SettingType::PresetsDir,
    },
    SettingItem {
        category: "Paths & Binaries",
        name: "llama-server Binary Path",
        description: "Path or executable name for local llama.cpp HTTP server",
        setting_type: SettingType::LlamaServerBinary,
    },
    SettingItem {
        category: "Paths & Binaries",
        name: "rpc-server Binary Path",
        description: "Path or executable name for llama.cpp distributed rpc-server",
        setting_type: SettingType::RpcServerBinary,
    },
    // Network & Transport
    SettingItem {
        category: "Network & Transport",
        name: "Default Inference Host Endpoint",
        description: "Static HTTP API endpoint (or 'none' for auto-discovery)",
        setting_type: SettingType::DefaultHost,
    },
    SettingItem {
        category: "Network & Transport",
        name: "Static Peer IP:Port List",
        description: "Comma-separated peer endpoints for non-broadcast subnets",
        setting_type: SettingType::StaticPeers,
    },
    SettingItem {
        category: "Network & Transport",
        name: "Enable mDNS Service Discovery",
        description: "Zero-config multicast DNS LAN discovery (_nexus._tcp.local.)",
        setting_type: SettingType::EnableMdns,
    },
    SettingItem {
        category: "Network & Transport",
        name: "UDP Discovery Port",
        description: "UDP port for heartbeat beacons (default: 9999)",
        setting_type: SettingType::DiscoveryPort,
    },
    SettingItem {
        category: "Network & Transport",
        name: "API Port (OpenAI REST / SSE)",
        description: "Port for llama-server HTTP API (default: 8080)",
        setting_type: SettingType::ApiPort,
    },
    SettingItem {
        category: "Network & Transport",
        name: "Control Plane Port",
        description: "HTTP control-plane port for remote load/unload (default: 9998)",
        setting_type: SettingType::ControlPort,
    },
    SettingItem {
        category: "Network & Transport",
        name: "Prefer USB Cable Tunnel (ADB)",
        description: "Prioritize low-latency localhost USB tunnel when cable is connected",
        setting_type: SettingType::PreferAdbTunnel,
    },
    // Hardware & Acceleration
    SettingItem {
        category: "Hardware & Acceleration",
        name: "Prefer Vulkan GPU Acceleration",
        description: "Offload compute layers to Adreno 740 GPU via Vulkan runtime",
        setting_type: SettingType::PreferGpu,
    },
    SettingItem {
        category: "Hardware & Acceleration",
        name: "GPU Offload Layer Count (-ngl)",
        description: "Number of transformer layers offloaded to GPU (99 for full offload)",
        setting_type: SettingType::GpuLayers,
    },
    SettingItem {
        category: "Hardware & Acceleration",
        name: "CPU Compute Threads (-t)",
        description: "Number of performance threads (recommended: 6 for Snapdragon 8 Gen 2)",
        setting_type: SettingType::CpuThreads,
    },
    // Memory & Android LMK Safeguards
    SettingItem {
        category: "Memory & Android LMK Safeguards",
        name: "Max RAM Safety Ceiling",
        description: "Guard against Android LMK SIGKILL (default: 75% of available RAM)",
        setting_type: SettingType::MaxRamPercent,
    },
    SettingItem {
        category: "Memory & Android LMK Safeguards",
        name: "Memory-Mapped Weights (mmap)",
        description: "Use mmap for zero-copy weight paging into memory",
        setting_type: SettingType::Mmap,
    },
    // Distributed Cluster RPC
    SettingItem {
        category: "Distributed Cluster RPC",
        name: "Enable Cluster RPC Offload",
        description: "Allow pipelining overflow model layers to Node B over network",
        setting_type: SettingType::EnableRpc,
    },
    SettingItem {
        category: "Distributed Cluster RPC",
        name: "Worker Max RPC RAM Cap",
        description: "Per-node RPC worker allocation default (no global ceiling; raise for large hosts)",
        setting_type: SettingType::MaxRpcRamMb,
    },
    SettingItem {
        category: "Distributed Cluster RPC",
        name: "Automatic Layer Offload Planning",
        description: "Auto-calculate tensor-byte layer split when model exceeds host LMK budget",
        setting_type: SettingType::AutoOffload,
    },
];

pub struct SettingsView {
    pub config: NexusConfig,
    pub selected_index: usize,
    pub status_message: Option<(String, Color)>,
    pub editing_text: bool,
    pub text_buffer: String,
}

impl SettingsView {
    pub fn new(config: NexusConfig) -> Self {
        Self {
            config,
            selected_index: 0,
            status_message: None,
            editing_text: false,
            text_buffer: String::new(),
        }
    }

    pub fn next(&mut self) {
        if self.editing_text {
            return;
        }
        if self.selected_index + 1 < SETTING_ITEMS.len() {
            self.selected_index += 1;
        } else {
            self.selected_index = 0;
        }
    }

    pub fn previous(&mut self) {
        if self.editing_text {
            return;
        }
        if self.selected_index == 0 {
            self.selected_index = SETTING_ITEMS.len() - 1;
        } else {
            self.selected_index -= 1;
        }
    }

    pub fn is_current_text(&self) -> bool {
        matches!(
            SETTING_ITEMS[self.selected_index].setting_type,
            SettingType::NodeName
                | SettingType::ModelsDir
                | SettingType::PresetsDir
                | SettingType::LlamaServerBinary
                | SettingType::RpcServerBinary
                | SettingType::DefaultHost
                | SettingType::StaticPeers
        )
    }

    pub fn start_editing(&mut self) {
        if !self.is_current_text() {
            return;
        }
        let cur_val = match SETTING_ITEMS[self.selected_index].setting_type {
            SettingType::NodeName => self.config.node.name.clone(),
            SettingType::ModelsDir => self.config.node.models_dir.to_string_lossy().to_string(),
            SettingType::PresetsDir => self.config.node.presets_dir.to_string_lossy().to_string(),
            SettingType::LlamaServerBinary => self.config.node.llama_server_binary.clone(),
            SettingType::RpcServerBinary => self.config.node.rpc_server_binary.clone(),
            SettingType::DefaultHost => self.config.network.default_host.clone().unwrap_or_default(),
            SettingType::StaticPeers => self.config.network.static_peers.join(", "),
            _ => String::new(),
        };
        self.text_buffer = cur_val;
        self.editing_text = true;
        self.status_message = Some(("Editing... [Enter] Commit | [Esc] Cancel".to_string(), Color::Yellow));
    }

    pub fn commit_text(&mut self) {
        if !self.editing_text {
            return;
        }
        let val = self.text_buffer.trim().to_string();
        match SETTING_ITEMS[self.selected_index].setting_type {
            SettingType::NodeName => {
                if !val.is_empty() {
                    self.config.node.name = val;
                }
            }
            SettingType::ModelsDir => {
                if !val.is_empty() {
                    self.config.node.models_dir = std::path::PathBuf::from(val);
                }
            }
            SettingType::PresetsDir => {
                if !val.is_empty() {
                    self.config.node.presets_dir = std::path::PathBuf::from(val);
                }
            }
            SettingType::LlamaServerBinary => {
                if !val.is_empty() {
                    self.config.node.llama_server_binary = val;
                }
            }
            SettingType::RpcServerBinary => {
                if !val.is_empty() {
                    self.config.node.rpc_server_binary = val;
                }
            }
            SettingType::DefaultHost => {
                if val.is_empty() || val == "none" || val == "auto" {
                    self.config.network.default_host = None;
                } else {
                    self.config.network.default_host = Some(val);
                }
            }
            SettingType::StaticPeers => {
                self.config.network.static_peers = val
                    .split(',')
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
                    .collect();
            }
            _ => {}
        }
        self.editing_text = false;
        self.text_buffer.clear();
        self.status_message = Some(("Modified (Press 'S' to save)".to_string(), Color::Yellow));
    }

    pub fn cancel_text(&mut self) {
        self.editing_text = false;
        self.text_buffer.clear();
        self.status_message = Some(("Edit cancelled".to_string(), Color::DarkGray));
    }

    pub fn push_char(&mut self, c: char) {
        if self.editing_text {
            self.text_buffer.push(c);
        }
    }

    pub fn backspace_text(&mut self) {
        if self.editing_text {
            self.text_buffer.pop();
        }
    }

    pub fn toggle_or_adjust(&mut self, is_left: bool, is_right: bool) {
        if self.editing_text {
            return;
        }
        let item = &SETTING_ITEMS[self.selected_index];
        match item.setting_type {
            SettingType::NodeName
            | SettingType::ModelsDir
            | SettingType::PresetsDir
            | SettingType::LlamaServerBinary
            | SettingType::RpcServerBinary
            | SettingType::DefaultHost
            | SettingType::StaticPeers => {
                self.start_editing();
                return;
            }
            SettingType::NodeRole => {
                let cur = self.config.node.role.as_str();
                let idx = VALID_ROLES.iter().position(|r| *r == cur).unwrap_or(0);
                let new_idx = if is_left {
                    if idx == 0 { VALID_ROLES.len() - 1 } else { idx - 1 }
                } else {
                    (idx + 1) % VALID_ROLES.len()
                };
                self.config.node.role = VALID_ROLES[new_idx].to_string();
            }
            SettingType::EnableMdns => {
                self.config.network.discovery.mdns.enabled = !self.config.network.discovery.mdns.enabled;
            }
            SettingType::DiscoveryPort => {
                if is_left {
                    self.config.network.discovery_port = self.config.network.discovery_port.saturating_sub(1);
                } else if is_right {
                    self.config.network.discovery_port = self.config.network.discovery_port.saturating_add(1);
                }
            }
            SettingType::PreferGpu => {
                self.config.hardware.acceleration.prefer_gpu = !self.config.hardware.acceleration.prefer_gpu;
            }
            SettingType::GpuLayers => {
                if is_left {
                    self.config.hardware.acceleration.gpu_layers = self.config.hardware.acceleration.gpu_layers.saturating_sub(10);
                } else if is_right {
                    self.config.hardware.acceleration.gpu_layers = (self.config.hardware.acceleration.gpu_layers + 10).min(99);
                }
            }
            SettingType::CpuThreads => {
                if is_left {
                    self.config.hardware.acceleration.cpu_threads = self.config.hardware.acceleration.cpu_threads.saturating_sub(1).max(1);
                } else if is_right {
                    self.config.hardware.acceleration.cpu_threads = (self.config.hardware.acceleration.cpu_threads + 1).min(16);
                }
            }
            SettingType::MaxRamPercent => {
                if is_left {
                    self.config.hardware.safety.max_ram_usage_percent = self.config.hardware.safety.max_ram_usage_percent.saturating_sub(5).max(50);
                } else if is_right {
                    self.config.hardware.safety.max_ram_usage_percent = (self.config.hardware.safety.max_ram_usage_percent + 5).min(90);
                }
            }
            SettingType::Mmap => {
                self.config.hardware.safety.mmap = !self.config.hardware.safety.mmap;
            }
            SettingType::ApiPort => {
                if is_left {
                    self.config.network.api_port = self.config.network.api_port.saturating_sub(1);
                } else if is_right {
                    self.config.network.api_port = self.config.network.api_port.saturating_add(1);
                }
            }
            SettingType::ControlPort => {
                if is_left {
                    self.config.network.control_port = self.config.network.control_port.saturating_sub(1);
                } else if is_right {
                    self.config.network.control_port = self.config.network.control_port.saturating_add(1);
                }
            }
            SettingType::PreferAdbTunnel => {
                self.config.cluster.prefer_adb_tunnel = !self.config.cluster.prefer_adb_tunnel;
            }
            SettingType::EnableRpc => {
                self.config.cluster.enable_rpc = !self.config.cluster.enable_rpc;
            }
            SettingType::MaxRpcRamMb => {
                if is_left {
                    self.config.cluster.max_rpc_ram_mb =
                        self.config.cluster.max_rpc_ram_mb.saturating_sub(100).max(256);
                } else if is_right {
                    // No global 1800 MB ceiling — per-node worker caps (Phase 11).
                    self.config.cluster.max_rpc_ram_mb =
                        self.config.cluster.max_rpc_ram_mb.saturating_add(100).min(262_144);
                }
            }
            SettingType::AutoOffload => {
                self.config.cluster.auto_offload = !self.config.cluster.auto_offload;
            }
        }
        self.status_message = Some(("Modified (Press 'S' to save)".to_string(), Color::Yellow));
    }

    pub fn item_count(&self) -> usize {
        SETTING_ITEMS.len()
    }

    pub fn current_item(&self) -> &'static SettingItem {
        &SETTING_ITEMS[self.selected_index]
    }

    pub fn items() -> &'static [SettingItem] {
        SETTING_ITEMS
    }

    pub fn save(&mut self) -> Result<(), ConfigError> {
        let path = NexusConfig::default_config_path();
        self.save_to(&path)
    }

    pub fn save_to(&mut self, path: &std::path::Path) -> Result<(), ConfigError> {
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        self.config.save_to_path(path)?;
        self.status_message = Some((format!("Saved successfully to {:?}", path), Color::Green));
        Ok(())
    }

    pub fn render(&self, frame: &mut Frame, area: Rect) {
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Min(16),  // Setting list
                Constraint::Length(4), // Description & Status footer
            ])
            .split(area);

        let mut lines = Vec::new();
        let mut current_cat = "";

        for (i, item) in SETTING_ITEMS.iter().enumerate() {
            if item.category != current_cat {
                current_cat = item.category;
                lines.push(Line::from(vec![
                    Span::styled(format!("\n [ {} ]", current_cat), Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)),
                ]));
            }

            let is_selected = i == self.selected_index;
            let val_str = if is_selected && self.editing_text {
                format!("[ {}█ ]", self.text_buffer)
            } else {
                match item.setting_type {
                    SettingType::NodeName => format!("[ {} ]", self.config.node.name),
                    SettingType::NodeRole => format!("[ {} ]", self.config.node.role),
                    SettingType::ModelsDir => format!("[ {} ]", self.config.node.models_dir.display()),
                    SettingType::PresetsDir => format!("[ {} ]", self.config.node.presets_dir.display()),
                    SettingType::LlamaServerBinary => format!("[ {} ]", self.config.node.llama_server_binary),
                    SettingType::RpcServerBinary => format!("[ {} ]", self.config.node.rpc_server_binary),
                    SettingType::DefaultHost => format!("[ {} ]", self.config.network.default_host.as_deref().unwrap_or("none")),
                    SettingType::StaticPeers => {
                        if self.config.network.static_peers.is_empty() {
                            "[ none ]".to_string()
                        } else {
                            format!("[ {} ]", self.config.network.static_peers.join(", "))
                        }
                    }
                    SettingType::EnableMdns => format!("[ {} ]", if self.config.network.discovery.mdns.enabled { "ON" } else { "OFF" }),
                    SettingType::DiscoveryPort => format!("[ {} ]", self.config.network.discovery_port),
                    SettingType::PreferGpu => format!("[ {} ]", if self.config.hardware.acceleration.prefer_gpu { "ON" } else { "OFF" }),
                    SettingType::GpuLayers => format!("[ {} layers ]", self.config.hardware.acceleration.gpu_layers),
                    SettingType::CpuThreads => format!("[ {} threads ]", self.config.hardware.acceleration.cpu_threads),
                    SettingType::MaxRamPercent => format!("[ {}% ]", self.config.hardware.safety.max_ram_usage_percent),
                    SettingType::Mmap => format!("[ {} ]", if self.config.hardware.safety.mmap { "ON" } else { "OFF" }),
                    SettingType::ApiPort => format!("[ {} ]", self.config.network.api_port),
                    SettingType::ControlPort => format!("[ {} ]", self.config.network.control_port),
                    SettingType::PreferAdbTunnel => format!("[ {} ]", if self.config.cluster.prefer_adb_tunnel { "ON" } else { "OFF" }),
                    SettingType::EnableRpc => format!("[ {} ]", if self.config.cluster.enable_rpc { "ON" } else { "OFF" }),
                    SettingType::MaxRpcRamMb => format!("[ {} MB ]", self.config.cluster.max_rpc_ram_mb),
                    SettingType::AutoOffload => format!("[ {} ]", if self.config.cluster.auto_offload { "ON" } else { "OFF" }),
                }
            };

            let prefix = if is_selected { " > " } else { "   " };
            let style = if is_selected {
                Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(Color::White)
            };

            let val_style = if is_selected {
                if self.editing_text {
                    Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)
                }
            } else {
                Style::default().fg(Color::Green)
            };

            lines.push(Line::from(vec![
                Span::styled(prefix, style),
                Span::styled(format!("{:<38}", item.name), style),
                Span::styled(val_str, val_style),
            ]));
        }

        let settings_widget = Paragraph::new(lines).block(
            Block::default()
                .title(" Configuration & Settings (~/.nexus/config.toml) ")
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::DarkGray)),
        );
        frame.render_widget(settings_widget, chunks[0]);

        // Description & Actions block
        let cur_item = &SETTING_ITEMS[self.selected_index];
        let status_span = if let Some((msg, color)) = &self.status_message {
            Span::styled(format!(" Status: {}", msg), Style::default().fg(*color).add_modifier(Modifier::BOLD))
        } else if self.editing_text {
            Span::styled(" [Enter] Commit | [Esc] Cancel | [Backspace] Delete", Style::default().fg(Color::Yellow))
        } else if self.is_current_text() {
            Span::styled(" [Enter] Edit Text | [S] Save | [R] Reload", Style::default().fg(Color::DarkGray))
        } else {
            Span::styled(" [Space/Enter] Toggle | [Left/Right] Adjust/Cycle | [S] Save", Style::default().fg(Color::DarkGray))
        };

        let footer_lines = vec![
            Line::from(vec![
                Span::styled(" Info: ", Style::default().fg(Color::LightBlue)),
                Span::styled(cur_item.description, Style::default().fg(Color::White)),
            ]),
            Line::from(vec![status_span]),
        ];

        let footer_widget = Paragraph::new(footer_lines).block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::DarkGray)),
        );
        frame.render_widget(footer_widget, chunks[1]);
    }
}
