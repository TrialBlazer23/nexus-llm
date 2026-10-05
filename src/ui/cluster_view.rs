use crate::discovery::{DiscoveryService, PeerNode};
use crate::sysinfo::{AccelerationBackend, SystemProfile};
use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Cell, Clear, Gauge, Paragraph, Row, Table},
    Frame,
};
use std::sync::Arc;

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
}

impl ClusterView {
    pub fn new(discovery: Arc<DiscoveryService>) -> Self {
        let local_profile = SystemProfile::probe();
        let thermal_index = DiscoveryService::probe_thermal_index();

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
        }
    }

    pub async fn refresh(&mut self) {
        self.local_profile = SystemProfile::probe();
        self.thermal_index = DiscoveryService::probe_thermal_index();
        self.peers = self.discovery.get_active_peers().await;
        if self.selected_index >= self.peers.len() && !self.peers.is_empty() {
            self.selected_index = self.peers.len() - 1;
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
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .margin(1)
            .constraints([
                Constraint::Length(3),  // Header
                Constraint::Length(9),  // Local telemetry
                Constraint::Min(6),     // Discovered peers table
                Constraint::Length(2),  // Action shortcuts & status
            ])
            .split(area);

        self.render_header(frame, chunks[0]);
        self.render_local_telemetry(frame, chunks[1]);
        self.render_peers_table(frame, chunks[2]);
        self.render_footer(frame, chunks[3]);

        if self.show_info_modal {
            self.render_peer_info_modal(frame, area);
        }

        if self.adding_peer {
            self.render_add_peer_modal(frame, area);
        }
    }

    fn render_header(&self, frame: &mut Frame, area: Rect) {
        let title = format!(
            " Nexus-LLM Mesh Coordinator | Target: {} | Active Nodes: {} (Local + {} Peers)",
            std::env::consts::ARCH,
            self.peers.len() + 1,
            self.peers.len()
        );

        let header = Paragraph::new(Line::from(vec![
            Span::styled(title, Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)),
        ]))
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::DarkGray)),
        );

        frame.render_widget(header, area);
    }

    fn render_local_telemetry(&self, frame: &mut Frame, area: Rect) {
        let cols = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
            .split(area);

        // Column 1: RAM & LMK Memory Guard Gauge
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

        let mem_gauge = Gauge::default()
            .block(
                Block::default()
                    .title(format!(
                        " Local RAM (Used: {} MB / Total: {} MB | LMK Cap: {} MB) ",
                        used_ram, total_ram, lmk_cap
                    ))
                    .borders(Borders::ALL),
            )
            .gauge_style(
                if ram_percent > 85 {
                    Style::default().fg(Color::Red)
                } else if ram_percent > 70 {
                    Style::default().fg(Color::Yellow)
                } else {
                    Style::default().fg(Color::Green)
                },
            )
            .percent(ram_percent);

        frame.render_widget(mem_gauge, cols[0]);

        // Column 2: Acceleration Tier & Thermal Meter
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

        let info_lines = vec![
            Line::from(vec![
                Span::styled(" Acceleration Tier: ", Style::default().fg(Color::LightBlue)),
                Span::styled(backend_name, Style::default().fg(Color::White).add_modifier(Modifier::BOLD)),
            ]),
            Line::from(vec![
                Span::styled(" Vulkan Runtime:    ", Style::default().fg(Color::LightBlue)),
                Span::styled(
                    if SystemProfile::probe_vulkan() { "Active / Initialized" } else { "Not Available" },
                    Style::default().fg(Color::LightGreen),
                ),
            ]),
            Line::from(vec![
                Span::styled(" Thermal Index:     ", Style::default().fg(Color::LightBlue)),
                Span::styled(format!("{}/100", self.thermal_index), thermal_style.add_modifier(Modifier::BOLD)),
                Span::styled("  |  Threads: ", Style::default().fg(Color::LightBlue)),
                Span::styled(format!("{}", self.local_profile.recommended_threads), Style::default().fg(Color::White)),
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

    fn render_peers_table(&self, frame: &mut Frame, area: Rect) {
        let header = Row::new(vec![
            Cell::from("  Node Name / UUID"),
            Cell::from("Endpoint"),
            Cell::from("Role"),
            Cell::from("Free RAM"),
            Cell::from("Backend"),
            Cell::from("Thermal"),
            Cell::from("Active Model"),
        ])
        .style(Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD));

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

            let backend_str = match peer.backend {
                AccelerationBackend::Vulkan => "Vulkan",
                AccelerationBackend::ArmCpuDotProd => "ARM CPU",
                AccelerationBackend::X86Baseline => "x86 SSE4.1",
                AccelerationBackend::GenericCpu => "Generic",
            };

            let endpoint_str = if peer.is_rpc_ready() {
                format!("RPC: {}", peer.rpc_endpoint())
            } else {
                peer.api_endpoint()
            };

            let model_display = if peer.active_model.is_empty() {
                if peer.is_rpc_ready() { "RPC Ready".to_string() } else { "—".to_string() }
            } else {
                peer.active_model.clone()
            };

            let short_id = peer.uuid.to_string();
            let label = format!("{}{}", cursor, &short_id[..13]);

            let row_style = if is_selected {
                Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(Color::White)
            };

            rows.push(
                Row::new(vec![
                    Cell::from(label),
                    Cell::from(endpoint_str),
                    Cell::from(role_str),
                    Cell::from(format!("{} MB", peer.free_ram_mb)),
                    Cell::from(backend_str),
                    Cell::from(format!("{}/100", peer.thermal_index)),
                    Cell::from(model_display),
                ])
                .style(row_style),
            );
        }

        let title = format!(" Discovered Mesh Peers (UDP 9999 + mDNS) - {} Nodes ", self.peers.len());
        let table = Table::new(
            rows,
            [
                Constraint::Length(18), // Node label
                Constraint::Length(23), // Endpoint
                Constraint::Length(12), // Role
                Constraint::Length(12), // Free RAM
                Constraint::Length(12), // Backend
                Constraint::Length(9),  // Thermal
                Constraint::Min(16),    // Active Model
            ],
        )
        .header(header)
        .block(
            Block::default()
                .title(title)
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::LightBlue)),
        );

        frame.render_widget(table, area);
    }

    fn render_footer(&self, frame: &mut Frame, area: Rect) {
        let status_span = if let Some((msg, color)) = &self.status_message {
            Span::styled(format!(" Status: {}", msg), Style::default().fg(*color).add_modifier(Modifier::BOLD))
        } else {
            Span::styled(" Ready. Select a peer to manage.", Style::default().fg(Color::DarkGray))
        };

        let shortcuts_line = Line::from(vec![
            Span::styled(" [Enter] Connect ", Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)),
            Span::styled(" [L] Load Model ", Style::default().fg(Color::Yellow)),
            Span::styled(" [W] Request Worker ", Style::default().fg(Color::Magenta)),
            Span::styled(" [I] Inspect ", Style::default().fg(Color::Green)),
            Span::styled(" [A] Add Static ", Style::default().fg(Color::LightCyan)),
            Span::styled(" [D] Disconnect ", Style::default().fg(Color::Red)),
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
                Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD),
            )]),
            Line::from(""),
            Line::from(vec![
                Span::styled(" API Endpoint:    ", Style::default().fg(Color::LightBlue)),
                Span::styled(peer.api_endpoint(), Style::default().fg(Color::White)),
            ]),
            Line::from(vec![
                Span::styled(" RPC Endpoint:    ", Style::default().fg(Color::LightBlue)),
                Span::styled(
                    if peer.is_rpc_ready() { peer.rpc_endpoint() } else { "Disabled / Not Listening".to_string() },
                    Style::default().fg(if peer.is_rpc_ready() { Color::Green } else { Color::DarkGray }),
                ),
            ]),
            Line::from(vec![
                Span::styled(" Operational Role:", Style::default().fg(Color::LightBlue)),
                Span::styled(format!("{:?}", peer.role), Style::default().fg(Color::White)),
            ]),
            Line::from(vec![
                Span::styled(" Allocatable RAM: ", Style::default().fg(Color::LightBlue)),
                Span::styled(format!("{} MB free", peer.free_ram_mb), Style::default().fg(Color::Green)),
            ]),
            Line::from(vec![
                Span::styled(" Backend Tier:    ", Style::default().fg(Color::LightBlue)),
                Span::styled(backend_str, Style::default().fg(Color::White)),
            ]),
            Line::from(vec![
                Span::styled(" Active Model:    ", Style::default().fg(Color::LightBlue)),
                Span::styled(
                    if peer.active_model.is_empty() { "None (Idle)".to_string() } else { peer.active_model.clone() },
                    Style::default().fg(Color::Yellow),
                ),
            ]),
            Line::from(vec![
                Span::styled(" Thermal Status:  ", Style::default().fg(Color::LightBlue)),
                Span::styled(format!("{}/100", peer.thermal_index), Style::default().fg(Color::White)),
            ]),
            Line::from(""),
            Line::from(vec![Span::styled(
                " Press [I] or [Esc] to close inspection panel ",
                Style::default().fg(Color::DarkGray).add_modifier(Modifier::BOLD),
            )]),
        ];

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
                Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD),
            )]),
            Line::from(" Enter IP:Port address of remote node:"),
            Line::from(""),
            Line::from(vec![
                Span::styled(" > ", Style::default().fg(Color::Yellow)),
                Span::styled(format!("{}█", self.add_peer_input), Style::default().fg(Color::White).add_modifier(Modifier::BOLD)),
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
    let popup_layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage((100 - percent_y) / 2),
            Constraint::Percentage(percent_y),
            Constraint::Percentage((100 - percent_y) / 2),
        ])
        .split(r);

    Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - percent_x) / 2),
            Constraint::Percentage(percent_x),
            Constraint::Percentage((100 - percent_x) / 2),
        ])
        .split(popup_layout[1])[1]
}
