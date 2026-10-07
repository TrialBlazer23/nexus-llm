use crate::discovery::{DiscoveryService, PeerNode};
use crate::sysinfo::{AccelerationBackend, SystemProfile};
use crossterm::{
    event::{Event, EventStream, KeyCode, KeyModifiers},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use futures_util::StreamExt;
use ratatui::{
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Cell, Gauge, Paragraph, Row, Table},
    Frame, Terminal,
};
use std::io::stdout;
use std::sync::Arc;
use std::time::Duration;

pub struct DashboardApp {
    pub discovery: Arc<DiscoveryService>,
    pub local_profile: SystemProfile,
    pub thermal_index: u8,
    pub peers: Vec<PeerNode>,
    pub transport_info: String,
}

impl DashboardApp {
    pub fn new(discovery: Arc<DiscoveryService>) -> Self {
        let local_profile = SystemProfile::probe();
        let thermal_index = DiscoveryService::probe_thermal_index();
        let transport_info = Self::detect_transport();

        Self {
            discovery,
            local_profile,
            thermal_index,
            peers: Vec::new(),
            transport_info,
        }
    }

    fn detect_transport() -> String {
        if crate::tunnel::AdbTunnelSupervisor::is_adb_available() {
            if let Ok(devices) = crate::tunnel::AdbTunnelSupervisor::list_devices() {
                if let Some(dev) = devices.into_iter().find(|d| d.authorized) {
                    return format!("USB Cable (ADB: {})", dev.serial);
                }
            }
        }
        "Wi-Fi Subnet".to_string()
    }

    pub async fn refresh(&mut self) {
        self.local_profile = SystemProfile::probe();
        self.thermal_index = DiscoveryService::probe_thermal_index();
        self.peers = self.discovery.get_active_peers().await;
        self.transport_info = Self::detect_transport();
    }

    pub fn render(&self, frame: &mut Frame) {
        let area = frame.area();
        self.render_in_area(frame, area);
    }

    pub fn render_in_area(&self, frame: &mut Frame, area: Rect) {
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .margin(1)
            .constraints([
                Constraint::Length(3),  // Header
                Constraint::Length(10), // Node local telemetry gauges
                Constraint::Min(6),     // Discovered peers table
                Constraint::Length(1),  // Footer shortcuts
            ])
            .split(area);

        self.render_header(frame, chunks[0]);
        self.render_local_telemetry(frame, chunks[1]);
        self.render_peers_table(frame, chunks[2]);
        self.render_footer(frame, chunks[3]);
    }

    fn render_header(&self, frame: &mut Frame, area: Rect) {
        let title = format!(
            " Nexus-LLM Heterogeneous Cluster Monitor | Arch: {} | Active Nodes: {} | Transport: {}",
            std::env::consts::ARCH,
            self.peers.len() + 1,
            self.transport_info
        );

        let header = Paragraph::new(Line::from(vec![Span::styled(
            title,
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        )]))
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
                        " Memory Utilization (Used: {} MB / Total: {} MB | LMK Cap: {} MB) ",
                        used_ram, total_ram, lmk_cap
                    ))
                    .borders(Borders::ALL),
            )
            .gauge_style(if ram_percent > 85 {
                Style::default().fg(Color::Red)
            } else if ram_percent > 70 {
                Style::default().fg(Color::Yellow)
            } else {
                Style::default().fg(Color::Green)
            })
            .percent(ram_percent);

        frame.render_widget(mem_gauge, cols[0]);

        // Column 2: Acceleration Tier & Thermal Meter
        let backend_name = match self.local_profile.detected_backend {
            AccelerationBackend::Vulkan => "Vulkan (Adreno 740 GPU Offload)",
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
            ]),
            Line::from(vec![
                Span::styled(
                    " Compute Threads:   ",
                    Style::default().fg(Color::LightBlue),
                ),
                Span::styled(
                    format!(
                        "{} performance threads",
                        self.local_profile.recommended_threads
                    ),
                    Style::default().fg(Color::White),
                ),
            ]),
        ];

        let right_block = Paragraph::new(info_lines).block(
            Block::default()
                .title(" Local Engine Capabilities ")
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::DarkGray)),
        );

        frame.render_widget(right_block, cols[1]);
    }

    fn render_peers_table(&self, frame: &mut Frame, area: Rect) {
        let header = Row::new(vec![
            Cell::from("Node UUID"),
            Cell::from("Endpoint"),
            Cell::from("Role"),
            Cell::from("Free RAM"),
            Cell::from("Backend"),
            Cell::from("Thermal"),
            Cell::from("Active Model"),
        ])
        .style(
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        );

        let mut rows = Vec::new();

        for peer in &self.peers {
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
                if peer.is_rpc_ready() {
                    "RPC Ready".to_string()
                } else {
                    "—".to_string()
                }
            } else {
                peer.active_model.clone()
            };

            rows.push(Row::new(vec![
                Cell::from(peer.label()),
                Cell::from(endpoint_str),
                Cell::from(role_str),
                Cell::from(format!("{} MB", peer.free_ram_mb)),
                Cell::from(backend_str),
                Cell::from(format!("{}/100", peer.thermal_index)),
                Cell::from(model_display),
            ]));
        }

        let table = Table::new(
            rows,
            [
                Constraint::Length(24), // Display name / label
                Constraint::Length(23), // Endpoint
                Constraint::Length(8),  // Role
                Constraint::Length(12), // Free RAM
                Constraint::Length(12), // Backend
                Constraint::Length(9),  // Thermal
                Constraint::Min(12),    // Active Model
            ],
        )
        .header(header)
        .block(
            Block::default()
                .title(format!(
                    " Discovered Cluster Peers (UDP 9999) - {} nodes ",
                    self.peers.len()
                ))
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::LightBlue)),
        );

        frame.render_widget(table, area);
    }

    fn render_footer(&self, frame: &mut Frame, area: Rect) {
        let footer = Paragraph::new(Line::from(vec![Span::styled(
            " [q / Esc] Exit Monitor  |  [r] Manual Refresh ",
            Style::default().fg(Color::DarkGray),
        )]));

        frame.render_widget(footer, area);
    }
}

/// Run full Ratatui TUI dashboard monitor.
pub async fn run_dashboard_tui(
    discovery: Arc<DiscoveryService>,
) -> Result<(), Box<dyn std::error::Error>> {
    enable_raw_mode()?;
    let mut stdout = stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let mut app = DashboardApp::new(discovery);
    let mut event_stream = EventStream::new();
    let mut tick_timer = tokio::time::interval(Duration::from_millis(1000));

    // Initial draw
    app.refresh().await;
    terminal.draw(|f| app.render(f))?;

    loop {
        tokio::select! {
            _ = tick_timer.tick() => {
                app.refresh().await;
                terminal.draw(|f| app.render(f))?;
            }

            Some(event_res) = event_stream.next() => {
                if let Ok(Event::Key(key)) = event_res {
                    match key.code {
                        KeyCode::Char('q') | KeyCode::Esc => break,
                        KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => break,
                        KeyCode::Char('r') => {
                            app.refresh().await;
                            terminal.draw(|f| app.render(f))?;
                        }
                        _ => {}
                    }
                }
            }

            else => break,
        }
    }

    // Teardown
    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;

    Ok(())
}
