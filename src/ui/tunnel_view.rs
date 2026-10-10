use crate::adb_links::HostForward;
use crate::tunnel::{AdbDeviceInfo, AdbTunnelSupervisor, TunnelStatus};
use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph},
    Frame,
};
use std::collections::HashMap;

pub struct TunnelView {
    pub api_port: u16,
    pub rpc_port: u16,
    pub devices: Vec<AdbDeviceInfo>,
    pub forwards: Vec<HostForward>,
    pub status: Option<TunnelStatus>,
    pub status_message: Option<(String, Color)>,
    /// `PeerNode::label` by ADB serial. The `model:` token is not stored here.
    nexus_labels: HashMap<String, String>,
}

impl TunnelView {
    pub fn new(api_port: u16, rpc_port: u16) -> Self {
        let devices = AdbTunnelSupervisor::list_devices().unwrap_or_default();
        Self {
            api_port,
            rpc_port,
            devices,
            forwards: Vec::new(),
            status: None,
            status_message: None,
            nexus_labels: HashMap::new(),
        }
    }

    /// Remember labels for peers whose live link is USB. Does not copy `adb model:` into the registry.
    pub fn remember_peer_labels(&mut self, peers: &[crate::discovery::PeerNode]) {
        self.nexus_labels.clear();
        for peer in peers {
            if peer.link_class != crate::cluster::LinkClass::Usb {
                continue;
            }
            let Some(serial) = peer.usb_serial.as_deref() else {
                continue;
            };
            if serial.is_empty() {
                continue;
            }
            let label = peer.label();
            if label.is_empty() {
                continue;
            }
            self.nexus_labels.insert(serial.to_string(), label);
        }
    }

    pub fn refresh(&mut self) {
        self.devices = AdbTunnelSupervisor::list_devices().unwrap_or_default();
    }

    /// Record the host forwards from the all-serial probe. Each row uses that serial's host port.
    pub fn apply_forwards(&mut self, forwards: Vec<HostForward>) {
        let count = forwards.len();
        self.forwards = forwards;
        if self.forwards.is_empty() {
            self.status = None;
            self.status_message = Some((
                "No USB peer answered the control probe".to_string(),
                Color::Yellow,
            ));
            return;
        }
        let host_api = self.forwards[0].host_api;
        self.status = Some(TunnelStatus {
            is_active: true,
            api_forwarded: true,
            rpc_reversed: false,
            api_port: host_api,
            rpc_port: self.rpc_port,
            device: None,
        });
        self.status_message = Some((
            format!("Tunnels active for {count} serial(s)"),
            Color::Green,
        ));
    }

    pub fn teardown_tunnel(&mut self) {
        let _ = AdbTunnelSupervisor::teardown_tunnel(self.api_port, self.rpc_port);
        self.forwards.clear();
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
            let line = tunnel_row_line(
                &d.serial,
                d.authorized,
                d.model.as_deref(),
                self.nexus_labels.get(&d.serial).map(String::as_str),
            );
            let color = if d.authorized && self.nexus_labels.contains_key(&d.serial) {
                Color::Green
            } else {
                Color::Yellow
            };
            device_lines.push(Line::from(Span::styled(
                format!("   {line}"),
                Style::default().fg(color),
            )));
        }

        let dev_block = Paragraph::new(device_lines).block(
            Block::default()
                .title(" Hardware Transport & USB Device Status ")
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::DarkGray)),
        );
        frame.render_widget(dev_block, chunks[0]);

        // 2. Port Mappings & Tunnel Status block. Host ports come from the probe, per serial.
        let mut mapping_lines = Vec::new();
        if self.forwards.is_empty() {
            mapping_lines.push(Line::from(vec![
                Span::styled(
                    " API REST / SSE Forward: ",
                    Style::default().fg(Color::LightBlue),
                ),
                Span::styled(
                    "INACTIVE (Press 'F' to activate)",
                    Style::default().fg(Color::DarkGray),
                ),
            ]));
        } else {
            for line in forward_mapping_lines(&self.forwards) {
                mapping_lines.push(Line::from(Span::styled(
                    format!(" API  {line}"),
                    Style::default()
                        .fg(Color::Green)
                        .add_modifier(Modifier::BOLD),
                )));
            }
        }
        mapping_lines.push(Line::from(vec![
            Span::styled(
                " RPC Layer Reverse:      ",
                Style::default().fg(Color::LightBlue),
            ),
            Span::styled(
                "per serial, not required for a burst peer",
                Style::default().fg(Color::Gray),
            ),
        ]));
        mapping_lines.extend(vec![
            Line::from(vec![
                Span::styled("\n Transport Characteristics:", Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD)),
            ]),
            Line::from(vec![
                Span::styled("   • Zero-latency physical wired transport bypassing Wi-Fi congestion and router packet drops", Style::default().fg(Color::Gray)),
            ]),
            Line::from(vec![
                Span::styled("   • High-bandwidth tensor transfer for RPC layer offloading between Node A and Node B", Style::default().fg(Color::Gray)),
            ]),
        ]);

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
                " [F/S] Setup All Tunnels  |  [T] Teardown Tunnels  |  [R] Rescan Devices",
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

/// One mapping row per serial. The host port is the allocated forward, not a shared 8080.
pub fn forward_mapping_lines(forwards: &[HostForward]) -> Vec<String> {
    forwards
        .iter()
        .map(|forward| format!("{}  127.0.0.1:{}", forward.serial, forward.host_api))
        .collect()
}

/// One Tunnel-tab row. The ADB model is only the parenthetical subtitle.
pub fn tunnel_row_line(
    serial: &str,
    authorized: bool,
    adb_model: Option<&str>,
    nexus_label: Option<&str>,
) -> String {
    if !authorized {
        return format!("{serial}  unauthorized");
    }
    let label = nexus_label
        .map(str::trim)
        .filter(|label| !label.is_empty())
        .unwrap_or("Nexus not answering");
    match adb_model.map(str::trim).filter(|model| !model.is_empty()) {
        Some(model) => format!("{serial}  {label}  ({model})"),
        None => format!("{serial}  {label}"),
    }
}

#[cfg(test)]
mod tests {
    use super::tunnel_row_line;

    #[test]
    fn tunnel_row_keeps_adb_model_out_of_the_node_name() {
        let line = tunnel_row_line("serial-a", true, Some("Bar"), None);
        assert_eq!(line, "serial-a  Nexus not answering  (Bar)");
        let denied = tunnel_row_line("serial-c", false, None, None);
        assert_eq!(denied, "serial-c  unauthorized");
        let named = tunnel_row_line("serial-b", true, Some("Bar"), Some("node-bbbbbbbb"));
        assert_eq!(named, "serial-b  node-bbbbbbbb  (Bar)");
    }

    #[test]
    fn mapping_lines_use_each_serial_host_port() {
        use crate::adb_links::HostForward;
        let lines = super::forward_mapping_lines(&[
            HostForward {
                serial: "serial-a".into(),
                host_api: 18080,
                host_control: 18081,
            },
            HostForward {
                serial: "serial-b".into(),
                host_api: 18082,
                host_control: 18083,
            },
        ]);
        assert_eq!(lines[0], "serial-a  127.0.0.1:18080");
        assert_eq!(lines[1], "serial-b  127.0.0.1:18082");
        assert!(lines.iter().all(|line| !line.contains("127.0.0.1:8080")));
    }
}
