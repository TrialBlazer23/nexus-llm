use crate::tunnel::{AdbDeviceInfo, AdbTunnelSupervisor, TunnelStatus};
use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph},
    Frame,
};

pub struct TunnelView {
    pub api_port: u16,
    pub rpc_port: u16,
    pub devices: Vec<AdbDeviceInfo>,
    pub status: Option<TunnelStatus>,
    pub status_message: Option<(String, Color)>,
}

impl TunnelView {
    pub fn new(api_port: u16, rpc_port: u16) -> Self {
        let devices = AdbTunnelSupervisor::list_devices().unwrap_or_default();
        Self {
            api_port,
            rpc_port,
            devices,
            status: None,
            status_message: None,
        }
    }

    pub fn refresh(&mut self) {
        self.devices = AdbTunnelSupervisor::list_devices().unwrap_or_default();
    }

    pub fn setup_tunnel(&mut self) {
        match AdbTunnelSupervisor::setup_tunnel(self.api_port, self.rpc_port, None) {
            Ok(status) => {
                self.status = Some(status);
                self.status_message = Some((
                    format!(
                        "Tunnels active: Forward {} -> {}, Reverse {} -> {}",
                        self.api_port, self.api_port, self.rpc_port, self.rpc_port
                    ),
                    Color::Green,
                ));
            }
            Err(e) => {
                self.status_message = Some((format!("Failed to setup tunnel: {}", e), Color::Red));
            }
        }
    }

    pub fn teardown_tunnel(&mut self) {
        let _ = AdbTunnelSupervisor::teardown_tunnel(self.api_port, self.rpc_port);
        self.status = None;
        self.status_message = Some(("Tunnels removed successfully".to_string(), Color::Yellow));
    }

    pub fn render(&self, frame: &mut Frame, area: Rect) {
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(8), // USB Status & Connected Devices
                Constraint::Min(8),    // Active Port Mappings & Latency
                Constraint::Length(4), // Actions & Shortcuts
            ])
            .split(area);

        // 1. Device list / ADB status block
        let is_adb = AdbTunnelSupervisor::is_adb_available();
        let adb_status_span = if is_adb {
            Span::styled(
                "Available (android-tools-adb ready)",
                Style::default().fg(Color::Green),
            )
        } else {
            Span::styled(
                "Not Found in PATH",
                Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
            )
        };

        let mut device_lines = vec![
            Line::from(vec![
                Span::styled(" ADB Runtime:      ", Style::default().fg(Color::LightBlue)),
                adb_status_span,
            ]),
            Line::from(vec![
                Span::styled(" USB Target Nodes: ", Style::default().fg(Color::LightBlue)),
                Span::styled(
                    format!("{} device(s) connected", self.devices.len()),
                    Style::default().fg(Color::White),
                ),
            ]),
        ];

        for d in &self.devices {
            let auth_str = if d.authorized {
                "Authorized"
            } else {
                "Unauthorized (Check Phone Prompt)"
            };
            let auth_color = if d.authorized {
                Color::Green
            } else {
                Color::Yellow
            };
            device_lines.push(Line::from(vec![
                Span::styled(
                    format!("   - Serial: {:<16}", d.serial),
                    Style::default().fg(Color::Cyan),
                ),
                Span::styled(
                    format!("Model: {:<18}", d.model.as_deref().unwrap_or("unknown")),
                    Style::default().fg(Color::White),
                ),
                Span::styled(format!("[{}]", auth_str), Style::default().fg(auth_color)),
            ]));
        }

        let dev_block = Paragraph::new(device_lines).block(
            Block::default()
                .title(" Hardware Transport & USB Device Status ")
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::DarkGray)),
        );
        frame.render_widget(dev_block, chunks[0]);

        // 2. Port Mappings & Tunnel Status block
        let (api_fwd_str, api_color) = if self.status.as_ref().is_some_and(|s| s.api_forwarded) {
            ("ACTIVE: 127.0.0.1:8080 -> Target Phone 8080", Color::Green)
        } else {
            ("INACTIVE (Press 'F' to activate)", Color::DarkGray)
        };

        let (rpc_rev_str, rpc_color) = if self.status.as_ref().is_some_and(|s| s.rpc_reversed) {
            (
                "ACTIVE: Target Phone 50052 -> 127.0.0.1:50052",
                Color::Green,
            )
        } else {
            ("INACTIVE (Press 'F' to activate)", Color::DarkGray)
        };

        let mapping_lines = vec![
            Line::from(vec![
                Span::styled(" API REST / SSE Forward: ", Style::default().fg(Color::LightBlue)),
                Span::styled(api_fwd_str, Style::default().fg(api_color).add_modifier(Modifier::BOLD)),
            ]),
            Line::from(vec![
                Span::styled(" RPC Layer Reverse:      ", Style::default().fg(Color::LightBlue)),
                Span::styled(rpc_rev_str, Style::default().fg(rpc_color).add_modifier(Modifier::BOLD)),
            ]),
            Line::from(vec![
                Span::styled("\n Transport Characteristics:", Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD)),
            ]),
            Line::from(vec![
                Span::styled("   • Zero-latency physical wired transport bypassing Wi-Fi congestion and router packet drops", Style::default().fg(Color::Gray)),
            ]),
            Line::from(vec![
                Span::styled("   • High-bandwidth tensor transfer for RPC layer offloading between Node A and Node B", Style::default().fg(Color::Gray)),
            ]),
        ];

        let map_block = Paragraph::new(mapping_lines).block(
            Block::default()
                .title(" Port Forwarding & Reverse Tunnels ")
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::DarkGray)),
        );
        frame.render_widget(map_block, chunks[1]);

        // 3. Actions / Footer block
        let status_span = if let Some((msg, color)) = &self.status_message {
            Span::styled(
                format!(" Status: {}", msg),
                Style::default().fg(*color).add_modifier(Modifier::BOLD),
            )
        } else {
            Span::styled(
                " [F] Setup All Tunnels  |  [T] Teardown Tunnels  |  [R] Rescan Devices",
                Style::default().fg(Color::White),
            )
        };

        let footer_block = Paragraph::new(Line::from(vec![status_span])).block(
            Block::default()
                .title(" Actions ")
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::DarkGray)),
        );
        frame.render_widget(footer_block, chunks[2]);
    }
}
