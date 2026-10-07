use crate::discovery::{BackendHealth, DiscoveryService, PeerNode};
use crate::node_identity::NodeIdentity;
use crate::peer_registry::PeerLifecycle;
use crate::sysinfo::{AccelerationBackend, SystemProfile};
use ratatui::{
    layout::{Alignment, Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Cell, Clear, Gauge, Paragraph, Row, Table},
    Frame,
};
use std::collections::HashMap;
use std::sync::Arc;
use uuid::Uuid;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackendStatus {
    Starting,
    Active,
    Failed,
    Disabled,
}

impl BackendStatus {
    pub fn display(&self) -> (&'static str, Color) {
        match self {
            Self::Active => ("Active", Color::Green),
            Self::Starting => ("Starting", Color::Yellow),
            Self::Failed => ("Failed", Color::Red),
            Self::Disabled => ("Disabled", Color::DarkGray),
        }
    }
}

pub struct ClusterView {
    pub discovery: Arc<DiscoveryService>,
    pub local_profile: SystemProfile,
    pub thermal_index: u8,
    pub peers: Vec<PeerNode>,
    pub selected_index: usize,
    pub show_info_modal: bool,
    pub adding_peer: bool,
    pub add_peer_input: String,
    pub status_message: Option<(String, Color)>,
    pub udp_status: BackendStatus,
    pub mdns_status: BackendStatus,
    pub registry_trust: HashMap<Uuid, (PeerLifecycle, bool, Option<String>)>,
    pub peer_states: HashMap<Uuid, crate::control_plane::ControlPlaneState>,
    pub show_pair_code: bool,
    pub enter_pair_code: bool,
    pub pair_code_input: String,
    pub local_identity: Option<std::sync::Arc<NodeIdentity>>,
    pub link_qualities: HashMap<Uuid, crate::cluster::LinkQuality>,
    pub layout_mode: crate::ui::layout::LayoutMode,
}

impl ClusterView {
    pub fn new(discovery: Arc<DiscoveryService>) -> Self {
        let local_profile = SystemProfile::probe();
        let thermal_index = DiscoveryService::probe_thermal_index();
        let udp_status = if discovery.config().network.discovery.enabled {
            BackendStatus::Starting
        } else {
            BackendStatus::Disabled
        };
        let mdns_status = if discovery.config().network.discovery.enabled
            && discovery.config().network.discovery.mdns.enabled
        {
            BackendStatus::Starting
        } else {
            BackendStatus::Disabled
        };

        Self {
            discovery,
            local_profile,
            thermal_index,
            peers: Vec::new(),
            selected_index: 0,
            show_info_modal: false,
            adding_peer: false,
            add_peer_input: String::new(),
            status_message: None,
            udp_status,
            mdns_status,
            registry_trust: HashMap::new(),
            peer_states: HashMap::new(),
            show_pair_code: false,
            enter_pair_code: false,
            pair_code_input: String::new(),
            local_identity: None,
            link_qualities: HashMap::new(),
            layout_mode: crate::ui::layout::LayoutMode::Auto,
        }
    }

    pub fn set_local_identity(&mut self, identity: std::sync::Arc<NodeIdentity>) {
        self.local_identity = Some(identity);
    }

    pub fn record_peer_state(&mut self, state: crate::control_plane::ControlPlaneState) {
        self.peer_states.insert(state.node_id, state);
    }

    pub fn record_link_quality(&mut self, peer_id: Uuid, quality: crate::cluster::LinkQuality) {
        self.link_qualities.insert(peer_id, quality);
    }

    pub async fn refresh(&mut self) {
        self.local_profile = SystemProfile::probe();
        self.thermal_index = DiscoveryService::probe_thermal_index();
        self.peers = self.discovery.get_active_peers().await;
        self.registry_trust.clear();
        self.link_qualities.clear();
        for peer in &self.peers {
            if let Some(lq) = self.discovery.cached_link_quality(peer.uuid).await {
                self.link_qualities.insert(peer.uuid, lq);
            }
        }
        let registry_arc = self.discovery.peer_registry();
        let registry = registry_arc.read().await;
        for record in registry.records() {
            self.registry_trust.insert(
                record.node_id,
                (
                    record.lifecycle,
                    record.verified,
                    record.rejection_reason.clone(),
                ),
            );
        }
        drop(registry);
        if self.selected_index >= self.peers.len() && !self.peers.is_empty() {
            self.selected_index = self.peers.len() - 1;
        }

        if !self.discovery.config().network.discovery.enabled {
            self.udp_status = BackendStatus::Disabled;
            self.mdns_status = BackendStatus::Disabled;
        } else {
            let udp_health = *self.discovery.udp_health().read().await;
            self.udp_status = match udp_health {
                BackendHealth::Started => BackendStatus::Starting,
                BackendHealth::Healthy => BackendStatus::Active,
                BackendHealth::Failed => BackendStatus::Failed,
                BackendHealth::Stopped => BackendStatus::Disabled,
            };

            let mdns_enabled = self.discovery.is_mdns_enabled().await;
            if !mdns_enabled {
                self.mdns_status = BackendStatus::Disabled;
            } else {
                let mdns_health = *self.discovery.mdns_health().read().await;
                self.mdns_status = match mdns_health {
                    BackendHealth::Started => BackendStatus::Starting,
                    BackendHealth::Healthy => BackendStatus::Active,
                    BackendHealth::Failed => BackendStatus::Failed,
                    BackendHealth::Stopped => BackendStatus::Disabled,
                };
            }
        }
    }

    pub fn next(&mut self) {
        if self.peers.is_empty() {
            return;
        }
        self.selected_index = (self.selected_index + 1) % self.peers.len();
    }

    pub fn previous(&mut self) {
        if self.peers.is_empty() {
            return;
        }
        if self.selected_index == 0 {
            self.selected_index = self.peers.len() - 1;
        } else {
            self.selected_index -= 1;
        }
    }

    pub fn selected_peer(&self) -> Option<&PeerNode> {
        self.peers.get(self.selected_index)
    }

    pub fn toggle_info_modal(&mut self) {
        self.show_info_modal = !self.show_info_modal;
    }

    pub fn start_add_peer(&mut self) {
        self.adding_peer = true;
        self.add_peer_input.clear();
        self.status_message = Some((
            "Enter peer endpoint (e.g. 192.168.1.50:8080) | [Enter] Add | [Esc] Cancel".to_string(),
            Color::Yellow,
        ));
    }

    pub fn push_add_char(&mut self, c: char) {
        if self.adding_peer {
            self.add_peer_input.push(c);
        }
    }

    pub fn backspace_add_char(&mut self) {
        if self.adding_peer {
            self.add_peer_input.pop();
        }
    }

    pub fn cancel_add_peer(&mut self) {
        self.adding_peer = false;
        self.add_peer_input.clear();
        self.status_message = Some(("Add peer cancelled".to_string(), Color::DarkGray));
    }

    pub fn render(&self, frame: &mut Frame, area: Rect) {
        let is_compact = self.layout_mode.is_compact(area);
        let chunks = if is_compact {
            Layout::default()
                .direction(Direction::Vertical)
                .margin(0)
                .constraints([
                    Constraint::Length(3), // Header
                    Constraint::Length(5), // Local telemetry (compact)
                    Constraint::Min(6),    // Discovered peers table
                    Constraint::Length(2), // Action shortcuts & status
                ])
                .split(area)
        } else {
            Layout::default()
                .direction(Direction::Vertical)
                .margin(1)
                .constraints([
                    Constraint::Length(3), // Header
                    Constraint::Length(9), // Local telemetry
                    Constraint::Min(6),    // Discovered peers table
                    Constraint::Length(2), // Action shortcuts & status
                ])
                .split(area)
        };

        self.render_header(frame, chunks[0]);
        self.render_local_telemetry(frame, chunks[1], is_compact);
        self.render_peers_table(frame, chunks[2], is_compact);
        self.render_footer(frame, chunks[3]);

        if self.show_info_modal {
            self.render_peer_info_modal(frame, area);
        }

        if self.adding_peer {
            self.render_add_peer_modal(frame, area);
        }

        if self.show_pair_code {
            self.render_pair_code_modal(frame, area);
        }

        if self.enter_pair_code {
            self.render_enter_pair_code_modal(frame, area);
        }
    }

    fn trust_label(&self, peer_id: Uuid) -> String {
        let security = self.discovery.config().network.security;
        if !security.pairing_enforced() {
            return "open".to_string();
        }
        if !security.allowed_peer_ids.contains(&peer_id) {
            return "unpaired".to_string();
        }
        match self.registry_trust.get(&peer_id) {
            Some((PeerLifecycle::Healthy, true, _)) => "verified".to_string(),
            Some((PeerLifecycle::Rejected, _, reason)) => {
                format!("rejected: {}", reason.as_deref().unwrap_or("?"))
            }
            Some((lifecycle, _, _)) => format!("{:?}", lifecycle).to_lowercase(),
            None => "unknown".to_string(),
        }
    }

    fn render_pair_code_modal(&self, frame: &mut Frame, area: Rect) {
        let modal_area = centered_rect(50, 30, area);
        frame.render_widget(Clear, modal_area);
        let (code, remaining) = if let Some(id) = &self.local_identity {
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0);
            (
                id.current_pairing_code(now),
                crate::node_identity::pairing_code_remaining_secs(now),
            )
        } else {
            ("------".to_string(), 0)
        };
        let lines = vec![
            Line::from(Span::styled(
                " Pairing code (show on this device)",
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            )),
            Line::from(""),
            Line::from(Span::styled(
                code,
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            )),
            Line::from(Span::styled(
                format!("Rotates in {}s | [Esc] close", remaining),
                Style::default().fg(Color::DarkGray),
            )),
        ];
        let block = Paragraph::new(lines).alignment(Alignment::Center).block(
            Block::default()
                .title(" Trust / Pair ")
                .borders(Borders::ALL),
        );
        frame.render_widget(block, modal_area);
    }

    fn render_enter_pair_code_modal(&self, frame: &mut Frame, area: Rect) {
        let modal_area = centered_rect(55, 25, area);
        frame.render_widget(Clear, modal_area);
        let lines = vec![
            Line::from(" Enter pairing code from remote device"),
            Line::from(""),
            Line::from(Span::styled(
                format!("[ {} ]", self.pair_code_input),
                Style::default()
                    .fg(Color::Green)
                    .add_modifier(Modifier::BOLD),
            )),
        ];
        let block = Paragraph::new(lines).alignment(Alignment::Center).block(
            Block::default()
                .title(" Pair with peer ")
                .borders(Borders::ALL),
        );
        frame.render_widget(block, modal_area);
    }

    fn render_header(&self, frame: &mut Frame, area: Rect) {
        let (udp_text, udp_color) = self.udp_status.display();
        let (mdns_text, mdns_color) = self.mdns_status.display();

        let header_spans = vec![
            Span::styled(
                " Nexus-LLM Mesh Coordinator ",
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!("({}) ", std::env::consts::ARCH),
                Style::default().fg(Color::DarkGray),
            ),
            Span::styled("| ", Style::default().fg(Color::DarkGray)),
            Span::styled("● ", Style::default().fg(udp_color)),
            Span::styled(
                format!("UDP: {}  ", udp_text),
                Style::default().fg(Color::White),
            ),
            Span::styled("● ", Style::default().fg(mdns_color)),
            Span::styled(
                format!("mDNS: {}  ", mdns_text),
                Style::default().fg(Color::White),
            ),
            Span::styled("| ", Style::default().fg(Color::DarkGray)),
            Span::styled(
                format!("Peers: {}  ", self.peers.len()),
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled("| ", Style::default().fg(Color::DarkGray)),
            Span::styled(
                if self.discovery.config().network.discovery.enabled {
                    "↻ Broadcasting"
                } else {
                    "Discovery Off"
                },
                Style::default().fg(if self.discovery.config().network.discovery.enabled {
                    Color::LightGreen
                } else {
                    Color::DarkGray
                }),
            ),
        ];

        let header = Paragraph::new(Line::from(header_spans)).block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::DarkGray)),
        );

        frame.render_widget(header, area);
    }

    fn render_local_telemetry(&self, frame: &mut Frame, area: Rect, is_compact: bool) {
        // RAM & LMK Memory Guard Gauge
        let total_ram = self.local_profile.total_ram_mb;
        let avail_ram = self.local_profile.available_ram_mb;
        let used_ram = total_ram.saturating_sub(avail_ram);
        let ram_ratio = if total_ram > 0 {
            (used_ram as f64) / (total_ram as f64)
        } else {
            0.0
        };

        let ram_percent = (ram_ratio * 100.0).clamp(0.0, 100.0) as u16;
        let lmk_cap = self.local_profile.max_allowed_memory_bytes() / (1024 * 1024);

        let ram_title = if is_compact {
            format!(" RAM: {}/{}MB (Cap: {}MB) ", used_ram, total_ram, lmk_cap)
        } else {
            format!(
                " Local RAM (Used: {} MB / Total: {} MB | LMK Cap: {} MB) ",
                used_ram, total_ram, lmk_cap
            )
        };

        let mem_gauge = Gauge::default()
            .block(Block::default().title(ram_title).borders(Borders::ALL))
            .gauge_style(if ram_percent > 85 {
                Style::default().fg(Color::Red)
            } else if ram_percent > 70 {
                Style::default().fg(Color::Yellow)
            } else {
                Style::default().fg(Color::Green)
            })
            .percent(ram_percent);

        let backend_name = match self.local_profile.detected_backend {
            AccelerationBackend::Vulkan => "Vulkan (Adreno GPU Offload)",
            AccelerationBackend::ArmCpuDotProd => "ARMv8.2-A CPU (DotProd/I8MM)",
            AccelerationBackend::X86Baseline => "x86 Baseline (Penryn SSE4.1)",
            AccelerationBackend::GenericCpu => "Generic CPU",
        };

        let thermal_style = if self.thermal_index > 75 {
            Style::default().fg(Color::Red)
        } else if self.thermal_index > 50 {
            Style::default().fg(Color::Yellow)
        } else {
            Style::default().fg(Color::Green)
        };

        if is_compact {
            let rows = Layout::default()
                .direction(Direction::Vertical)
                .constraints([Constraint::Length(3), Constraint::Length(2)])
                .split(area);

            frame.render_widget(mem_gauge, rows[0]);

            let short_backend = match self.local_profile.detected_backend {
                AccelerationBackend::Vulkan => "Vulkan",
                AccelerationBackend::ArmCpuDotProd => "ARM CPU",
                AccelerationBackend::X86Baseline => "x86 Baseline",
                AccelerationBackend::GenericCpu => "Generic",
            };
            let summary_line = Line::from(vec![
                Span::styled(
                    " [Engine] ",
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(short_backend, Style::default().fg(Color::White)),
                Span::styled(" | Therm: ", Style::default().fg(Color::LightBlue)),
                Span::styled(format!("{}/100", self.thermal_index), thermal_style),
                Span::styled(
                    format!(" | Th: {}", self.local_profile.recommended_threads),
                    Style::default().fg(Color::Gray),
                ),
            ]);
            let block = Paragraph::new(summary_line);
            frame.render_widget(block, rows[1]);
        } else {
            let cols = Layout::default()
                .direction(Direction::Horizontal)
                .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
                .split(area);

            frame.render_widget(mem_gauge, cols[0]);

            let info_lines = vec![
                Line::from(vec![
                    Span::styled(
                        " Acceleration Tier: ",
                        Style::default().fg(Color::LightBlue),
                    ),
                    Span::styled(
                        backend_name,
                        Style::default()
                            .fg(Color::White)
                            .add_modifier(Modifier::BOLD),
                    ),
                ]),
                Line::from(vec![
                    Span::styled(
                        " Vulkan Runtime:    ",
                        Style::default().fg(Color::LightBlue),
                    ),
                    Span::styled(
                        if SystemProfile::probe_vulkan() {
                            "Active / Initialized"
                        } else {
                            "Not Available"
                        },
                        Style::default().fg(Color::LightGreen),
                    ),
                ]),
                Line::from(vec![
                    Span::styled(
                        " Thermal Index:     ",
                        Style::default().fg(Color::LightBlue),
                    ),
                    Span::styled(
                        format!("{}/100", self.thermal_index),
                        thermal_style.add_modifier(Modifier::BOLD),
                    ),
                    Span::styled("  |  Threads: ", Style::default().fg(Color::LightBlue)),
                    Span::styled(
                        format!("{}", self.local_profile.recommended_threads),
                        Style::default().fg(Color::White),
                    ),
                ]),
            ];

            let right_block = Paragraph::new(info_lines).block(
                Block::default()
                    .title(" Local Compute Engine ")
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(Color::DarkGray)),
            );

            frame.render_widget(right_block, cols[1]);
        }
    }

    fn render_peers_table(&self, frame: &mut Frame, area: Rect, is_compact: bool) {
        let header = if is_compact {
            Row::new(vec![
                Cell::from("  Node / UUID"),
                Cell::from("Endpoint"),
                Cell::from("Role"),
                Cell::from("Free RAM"),
            ])
            .style(
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            )
        } else {
            Row::new(vec![
                Cell::from("  Node Name / UUID"),
                Cell::from("Endpoint"),
                Cell::from("Role"),
                Cell::from("Free RAM"),
                Cell::from("Backend"),
                Cell::from("Link Quality"),
                Cell::from("Thermal"),
                Cell::from("Trust"),
            ])
            .style(
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            )
        };

        let mut rows = Vec::new();

        for (i, peer) in self.peers.iter().enumerate() {
            let is_selected = i == self.selected_index;
            let cursor = if is_selected { "▶ " } else { "  " };

            let role_str = if peer.is_rpc_ready() {
                "RPC Worker"
            } else if peer.role.is_host() {
                "Host"
            } else {
                "Client"
            };

            let endpoint_str = if peer.is_rpc_ready() {
                format!("RPC: {}", peer.rpc_endpoint())
            } else {
                peer.api_endpoint()
            };

            let label = format!("{}{}", cursor, peer.label());

            let row_style = if is_selected {
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(Color::White)
            };

            if is_compact {
                rows.push(
                    Row::new(vec![
                        Cell::from(label),
                        Cell::from(endpoint_str),
                        Cell::from(role_str),
                        Cell::from(format!("{} MB", peer.free_ram_mb)),
                    ])
                    .style(row_style),
                );
            } else {
                let backend_str = match peer.backend {
                    AccelerationBackend::Vulkan => "Vulkan",
                    AccelerationBackend::ArmCpuDotProd => "ARM CPU",
                    AccelerationBackend::X86Baseline => "x86 SSE4.1",
                    AccelerationBackend::GenericCpu => "Generic",
                };

                let link_cell = if let Some(lq) = self.link_qualities.get(&peer.uuid) {
                    Cell::from(crate::ui::badges::format_link_quality(
                        lq.rtt_ms,
                        lq.throughput_bps,
                        lq.unknown,
                    ))
                } else {
                    Cell::from(crate::ui::badges::BADGE_UNPROBED.span())
                };

                rows.push(
                    Row::new(vec![
                        Cell::from(label),
                        Cell::from(endpoint_str),
                        Cell::from(role_str),
                        Cell::from(format!("{} MB", peer.free_ram_mb)),
                        Cell::from(backend_str),
                        link_cell,
                        Cell::from(format!("{}/100", peer.thermal_index)),
                        Cell::from(self.trust_label(peer.uuid)),
                    ])
                    .style(row_style),
                );
            }
        }

        let (title, widths) = if is_compact {
            (
                format!(" Mesh Peers ({}) ", self.peers.len()),
                vec![
                    Constraint::Length(16),
                    Constraint::Length(20),
                    Constraint::Length(10),
                    Constraint::Min(8),
                ],
            )
        } else {
            (
                format!(
                    " Discovered Mesh Peers (UDP 9999 + mDNS) - {} Nodes ",
                    self.peers.len()
                ),
                vec![
                    Constraint::Length(18), // Node label
                    Constraint::Length(23), // Endpoint
                    Constraint::Length(12), // Role
                    Constraint::Length(11), // Free RAM
                    Constraint::Length(11), // Backend
                    Constraint::Length(18), // Link Quality
                    Constraint::Length(8),  // Thermal
                    Constraint::Min(12),    // Trust / Active Model
                ],
            )
        };

        let table = Table::new(rows, widths).header(header).block(
            Block::default()
                .title(title)
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::LightBlue)),
        );

        frame.render_widget(table, area);
    }

    fn render_footer(&self, frame: &mut Frame, area: Rect) {
        let status_span = if let Some((msg, color)) = &self.status_message {
            Span::styled(
                format!(" Status: {}", msg),
                Style::default().fg(*color).add_modifier(Modifier::BOLD),
            )
        } else {
            Span::styled(
                " Ready. Select a peer to manage.",
                Style::default().fg(Color::DarkGray),
            )
        };

        let shortcuts_line = Line::from(vec![
            Span::styled(
                " [Enter] Connect ",
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(" [L] Load Model ", Style::default().fg(Color::Yellow)),
            Span::styled(" [W] Request Worker ", Style::default().fg(Color::Magenta)),
            Span::styled(" [I] Inspect ", Style::default().fg(Color::Green)),
            Span::styled(" [A] Add Static ", Style::default().fg(Color::LightCyan)),
            Span::styled(" [D] Disconnect ", Style::default().fg(Color::Red)),
            Span::styled(" [P] Show code ", Style::default().fg(Color::LightGreen)),
            Span::styled(" [O] Pair ", Style::default().fg(Color::LightGreen)),
            Span::styled(" [R] Refresh", Style::default().fg(Color::DarkGray)),
        ]);

        let p = Paragraph::new(vec![shortcuts_line, Line::from(vec![status_span])]);
        frame.render_widget(p, area);
    }

    fn render_peer_info_modal(&self, frame: &mut Frame, area: Rect) {
        let modal_area = centered_rect(65, 45, area);
        frame.render_widget(Clear, modal_area);

        let peer = match self.selected_peer() {
            Some(p) => p,
            None => {
                let p = Paragraph::new("No peer selected").block(
                    Block::default()
                        .title(" Peer Inspection ")
                        .borders(Borders::ALL)
                        .border_style(Style::default().fg(Color::Yellow)),
                );
                frame.render_widget(p, modal_area);
                return;
            }
        };

        let backend_str = match peer.backend {
            AccelerationBackend::Vulkan => "Vulkan (Adreno GPU Offload)",
            AccelerationBackend::ArmCpuDotProd => "ARM CPU (DotProd / I8MM)",
            AccelerationBackend::X86Baseline => "x86 Baseline (Penryn SSE4.1)",
            AccelerationBackend::GenericCpu => "Generic CPU",
        };

        let lines = vec![
            Line::from(vec![Span::styled(
                format!(" Node Inspection: {}", peer.uuid),
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            )]),
            Line::from(""),
            Line::from(vec![
                Span::styled(" API Endpoint:    ", Style::default().fg(Color::LightBlue)),
                Span::styled(peer.api_endpoint(), Style::default().fg(Color::White)),
            ]),
            Line::from(vec![
                Span::styled(" RPC Endpoint:    ", Style::default().fg(Color::LightBlue)),
                Span::styled(
                    if peer.is_rpc_ready() {
                        peer.rpc_endpoint()
                    } else {
                        "Disabled / Not Listening".to_string()
                    },
                    Style::default().fg(if peer.is_rpc_ready() {
                        Color::Green
                    } else {
                        Color::DarkGray
                    }),
                ),
            ]),
            Line::from(vec![
                Span::styled(" Operational Role:", Style::default().fg(Color::LightBlue)),
                Span::styled(
                    format!("{:?}", peer.role),
                    Style::default().fg(Color::White),
                ),
            ]),
            Line::from(vec![
                Span::styled(" Allocatable RAM: ", Style::default().fg(Color::LightBlue)),
                Span::styled(
                    format!("{} MB free", peer.free_ram_mb),
                    Style::default().fg(Color::Green),
                ),
            ]),
            Line::from(vec![
                Span::styled(" Backend Tier:    ", Style::default().fg(Color::LightBlue)),
                Span::styled(backend_str, Style::default().fg(Color::White)),
            ]),
            Line::from(vec![
                Span::styled(" Active Model:    ", Style::default().fg(Color::LightBlue)),
                Span::styled(
                    if peer.active_model.is_empty() {
                        "None (Idle)".to_string()
                    } else {
                        peer.active_model.clone()
                    },
                    Style::default().fg(Color::Yellow),
                ),
            ]),
            Line::from(vec![
                Span::styled(" Thermal Status:  ", Style::default().fg(Color::LightBlue)),
                Span::styled(
                    format!("{}/100", peer.thermal_index),
                    Style::default().fg(Color::White),
                ),
            ]),
        ];

        let mut lines = lines;
        if let Some(lq) = self.link_qualities.get(&peer.uuid) {
            let mb_s = lq.throughput_bps / (1024.0 * 1024.0);
            let age_secs = lq.measured_at.elapsed().as_secs();
            let status_badge = if lq.unknown {
                crate::ui::badges::BADGE_UNPROBED.span()
            } else if lq.rtt_ms < 10.0 {
                crate::ui::badges::BADGE_FAST.span()
            } else if lq.rtt_ms < 50.0 {
                crate::ui::badges::BADGE_LAN.span()
            } else {
                crate::ui::badges::BADGE_SLOW.span()
            };
            lines.push(Line::from(vec![
                Span::styled(" Link Telemetry:  ", Style::default().fg(Color::LightBlue)),
                status_badge,
                Span::styled(
                    format!(
                        " {:.1}ms · {:.1}MB/s (probed {}s ago)",
                        lq.rtt_ms, mb_s, age_secs
                    ),
                    Style::default().fg(Color::White),
                ),
            ]));
        } else {
            lines.push(Line::from(vec![
                Span::styled(" Link Telemetry:  ", Style::default().fg(Color::LightBlue)),
                crate::ui::badges::BADGE_UNPROBED.span(),
                Span::styled(" (no probe cached)", Style::default().fg(Color::DarkGray)),
            ]));
        }
        if let Some(state) = self.peer_states.get(&peer.uuid) {
            if !state.loaded_models.is_empty() {
                lines.push(Line::from(""));
                lines.push(Line::from(vec![Span::styled(
                    " Active Model Services (Multi-Slot):",
                    Style::default()
                        .fg(Color::LightCyan)
                        .add_modifier(Modifier::BOLD),
                )]));
                for (idx, slot) in state.loaded_models.iter().enumerate() {
                    let tags_str = if slot.tags.is_empty() {
                        String::new()
                    } else {
                        format!(" [{}]", slot.tags.join(", "))
                    };
                    lines.push(Line::from(vec![
                        Span::styled(
                            format!("   Slot #{}: ", idx + 1),
                            Style::default().fg(Color::DarkGray),
                        ),
                        Span::styled(
                            &slot.model,
                            Style::default()
                                .fg(Color::Yellow)
                                .add_modifier(Modifier::BOLD),
                        ),
                        Span::styled(
                            format!(" -> {} ({} MB){}", slot.endpoint, slot.memory_mb, tags_str),
                            Style::default().fg(Color::Green),
                        ),
                    ]));
                }
            }
        }

        lines.push(Line::from(""));
        lines.push(Line::from(vec![Span::styled(
            " Press [I] or [Esc] to close inspection panel ",
            Style::default()
                .fg(Color::DarkGray)
                .add_modifier(Modifier::BOLD),
        )]));

        let block = Paragraph::new(lines).block(
            Block::default()
                .title(" Peer Hardware & Capabilities ")
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::Cyan)),
        );

        frame.render_widget(block, modal_area);
    }

    fn render_add_peer_modal(&self, frame: &mut Frame, area: Rect) {
        let modal_area = centered_rect(55, 20, area);
        frame.render_widget(Clear, modal_area);

        let lines = vec![
            Line::from(vec![Span::styled(
                " Add Static Cluster Peer ",
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            )]),
            Line::from(" Enter IP:Port address of remote node:"),
            Line::from(""),
            Line::from(vec![
                Span::styled(" > ", Style::default().fg(Color::Yellow)),
                Span::styled(
                    format!("{}█", self.add_peer_input),
                    Style::default()
                        .fg(Color::White)
                        .add_modifier(Modifier::BOLD),
                ),
            ]),
            Line::from(""),
            Line::from(vec![Span::styled(
                " [Enter] Confirm  |  [Esc] Cancel ",
                Style::default().fg(Color::DarkGray),
            )]),
        ];

        let block = Paragraph::new(lines).block(
            Block::default()
                .title(" Manual Static Peer ")
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::Yellow)),
        );

        frame.render_widget(block, modal_area);
    }
}

fn centered_rect(percent_x: u16, percent_y: u16, r: Rect) -> Rect {
    crate::ui::layout::responsive_centered_rect(
        percent_x,
        percent_y,
        r,
        crate::ui::layout::LayoutMode::Auto,
    )
}
