use crate::config::{ConfigError, NexusConfig};
use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph},
    Frame,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettingType {
    PreferGpu,
    GpuLayers,
    FallbackCpu,
    CpuThreads,
    MaxRamPercent,
    Mmap,
    ApiPort,
    PreferAdbTunnel,
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
        name: "Automatic CPU Fallback",
        description: "Fallback to ARM CPU dotprod if Vulkan fails to initialize",
        setting_type: SettingType::FallbackCpu,
    },
    SettingItem {
        category: "Hardware & Acceleration",
        name: "CPU Compute Threads (-t)",
        description: "Number of performance threads (recommended: 6 for Snapdragon 8 Gen 2)",
        setting_type: SettingType::CpuThreads,
    },
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
    SettingItem {
        category: "Network & Transport",
        name: "API Port (OpenAI REST / SSE)",
        description: "Port for llama-server HTTP API (default: 8080)",
        setting_type: SettingType::ApiPort,
    },
    SettingItem {
        category: "Network & Transport",
        name: "Prefer USB Cable Tunnel (ADB)",
        description: "Prioritize low-latency localhost USB tunnel when cable is connected",
        setting_type: SettingType::PreferAdbTunnel,
    },
    SettingItem {
        category: "Distributed Cluster RPC",
        name: "Enable Cluster RPC Offload",
        description: "Allow pipelining overflow model layers to Node B over network",
        setting_type: SettingType::EnableRpc,
    },
    SettingItem {
        category: "Distributed Cluster RPC",
        name: "Node B Max RPC RAM Cap",
        description: "Strict memory budget for Node B (capped at 1800 MB for Mac safety)",
        setting_type: SettingType::MaxRpcRamMb,
    },
    SettingItem {
        category: "Distributed Cluster RPC",
        name: "Automatic Layer Offload Planning",
        description: "Auto-calculate layer split when model exceeds Node A's 8.5 GB budget",
        setting_type: SettingType::AutoOffload,
    },
];

pub struct SettingsView {
    pub config: NexusConfig,
    pub selected_index: usize,
    pub status_message: Option<(String, Color)>,
}

impl SettingsView {
    pub fn new(config: NexusConfig) -> Self {
        Self {
            config,
            selected_index: 0,
            status_message: None,
        }
    }

    pub fn next(&mut self) {
        if self.selected_index + 1 < SETTING_ITEMS.len() {
            self.selected_index += 1;
        } else {
            self.selected_index = 0;
        }
    }

    pub fn previous(&mut self) {
        if self.selected_index == 0 {
            self.selected_index = SETTING_ITEMS.len() - 1;
        } else {
            self.selected_index -= 1;
        }
    }

    pub fn toggle_or_adjust(&mut self, is_left: bool, is_right: bool) {
        let item = &SETTING_ITEMS[self.selected_index];
        match item.setting_type {
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
            SettingType::FallbackCpu => {
                self.config.hardware.acceleration.fallback_to_cpu = !self.config.hardware.acceleration.fallback_to_cpu;
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
            SettingType::PreferAdbTunnel => {
                self.config.cluster.prefer_adb_tunnel = !self.config.cluster.prefer_adb_tunnel;
            }
            SettingType::EnableRpc => {
                self.config.cluster.enable_rpc = !self.config.cluster.enable_rpc;
            }
            SettingType::MaxRpcRamMb => {
                if is_left {
                    self.config.cluster.max_rpc_ram_mb = self.config.cluster.max_rpc_ram_mb.saturating_sub(100).max(500);
                } else if is_right {
                    self.config.cluster.max_rpc_ram_mb = (self.config.cluster.max_rpc_ram_mb + 100).min(1800);
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
            let val_str = match item.setting_type {
                SettingType::PreferGpu => format!("[ {} ]", if self.config.hardware.acceleration.prefer_gpu { "ON" } else { "OFF" }),
                SettingType::GpuLayers => format!("[ {} layers ]", self.config.hardware.acceleration.gpu_layers),
                SettingType::FallbackCpu => format!("[ {} ]", if self.config.hardware.acceleration.fallback_to_cpu { "ON" } else { "OFF" }),
                SettingType::CpuThreads => format!("[ {} threads ]", self.config.hardware.acceleration.cpu_threads),
                SettingType::MaxRamPercent => format!("[ {}% ]", self.config.hardware.safety.max_ram_usage_percent),
                SettingType::Mmap => format!("[ {} ]", if self.config.hardware.safety.mmap { "ON" } else { "OFF" }),
                SettingType::ApiPort => format!("[ {} ]", self.config.network.api_port),
                SettingType::PreferAdbTunnel => format!("[ {} ]", if self.config.cluster.prefer_adb_tunnel { "ON" } else { "OFF" }),
                SettingType::EnableRpc => format!("[ {} ]", if self.config.cluster.enable_rpc { "ON" } else { "OFF" }),
                SettingType::MaxRpcRamMb => format!("[ {} MB ]", self.config.cluster.max_rpc_ram_mb),
                SettingType::AutoOffload => format!("[ {} ]", if self.config.cluster.auto_offload { "ON" } else { "OFF" }),
            };

            let prefix = if is_selected { " > " } else { "   " };
            let style = if is_selected {
                Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(Color::White)
            };

            let val_style = if is_selected {
                Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(Color::Green)
            };

            lines.push(Line::from(vec![
                Span::styled(prefix, style),
                Span::styled(format!("{:<40}", item.name), style),
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
        } else {
            Span::styled(" [Space] Toggle | [Left/Right] Adjust | [S] Save | [R] Reload", Style::default().fg(Color::DarkGray))
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
