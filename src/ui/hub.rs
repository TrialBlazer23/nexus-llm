use crate::client::{ChatMessage, NexusClient};
use crate::config::NexusConfig;
use crate::discovery::DiscoveryService;
use crate::supervisor::{LlamaServerConfig, ProcessSupervisor};
use crate::sysinfo::SystemProfile;
use crate::ui::chat::{ChatApp, StreamMsg};
use crate::ui::dashboard::DashboardApp;
use crate::ui::models_view::ModelsView;
use crate::ui::settings_view::SettingsView;
use crate::ui::tunnel_view::TunnelView;
use crossterm::{
    event::{Event, EventStream, KeyCode, KeyModifiers},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use futures_util::StreamExt;
use ratatui::{
    backend::CrosstermBackend,
    layout::{Alignment, Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, Paragraph, Tabs},
    Frame, Terminal,
};
use std::io::stdout;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;
use tracing::{error, info};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HubTab {
    Chat = 0,
    Models = 1,
    Dashboard = 2,
    Tunnel = 3,
    Settings = 4,
}

impl HubTab {
    pub fn title(&self) -> &'static str {
        match self {
            Self::Chat => " [F1] 💬 Chat ",
            Self::Models => " [F2] 📦 Models ",
            Self::Dashboard => " [F3] 🖥️ Dashboard ",
            Self::Tunnel => " [F4] 🔗 USB Tunnel ",
            Self::Settings => " [F5] ⚙️ Settings ",
        }
    }
}

pub struct HubApp {
    pub config: NexusConfig,
    pub discovery: Arc<DiscoveryService>,
    pub active_tab: HubTab,
    pub chat: ChatApp,
    pub models_view: ModelsView,
    pub dashboard_view: DashboardApp,
    pub tunnel_view: TunnelView,
    pub settings_view: SettingsView,
    pub supervisor: Option<ProcessSupervisor>,
    pub active_model_name: String,
    pub pending_hot_swap_path: Option<PathBuf>,
    pub status_message: Option<(String, Color)>,
}

impl HubApp {
    pub fn new(config: NexusConfig, client: NexusClient, discovery: Arc<DiscoveryService>) -> Self {
        let models_view = ModelsView::new(config.node.models_dir.clone());
        let dashboard_view = DashboardApp::new(discovery.clone());
        let tunnel_view = TunnelView::new(config.network.api_port, config.cluster.rpc_port);
        let settings_view = SettingsView::new(config.clone());
        let chat = ChatApp::new(client, "default", None);

        Self {
            config,
            discovery,
            active_tab: HubTab::Chat,
            chat,
            models_view,
            dashboard_view,
            tunnel_view,
            settings_view,
            supervisor: None,
            active_model_name: "None (Idle)".to_string(),
            pending_hot_swap_path: None,
            status_message: None,
        }
    }

    pub fn set_tab(&mut self, tab: HubTab) {
        self.active_tab = tab;
        if tab == HubTab::Models {
            self.models_view.refresh();
        } else if tab == HubTab::Tunnel {
            self.tunnel_view.refresh();
        }
    }

    pub fn next_tab(&mut self) {
        let next_idx = ((self.active_tab as usize) + 1) % 5;
        self.set_tab(match next_idx {
            0 => HubTab::Chat,
            1 => HubTab::Models,
            2 => HubTab::Dashboard,
            3 => HubTab::Tunnel,
            _ => HubTab::Settings,
        });
    }

    pub fn previous_tab(&mut self) {
        let prev_idx = if (self.active_tab as usize) == 0 {
            4
        } else {
            (self.active_tab as usize) - 1
        };
        self.set_tab(match prev_idx {
            0 => HubTab::Chat,
            1 => HubTab::Models,
            2 => HubTab::Dashboard,
            3 => HubTab::Tunnel,
            _ => HubTab::Settings,
        });
    }

    /// Request to launch a model. If a model is already active, prompts for hot-swap confirmation.
    pub async fn request_model_load(&mut self, model_path: PathBuf) {
        if self.supervisor.is_some() {
            self.pending_hot_swap_path = Some(model_path);
        } else {
            self.execute_model_load(model_path).await;
        }
    }

    /// Stop any active supervisor and load the new model process.
    pub async fn execute_model_load(&mut self, model_path: PathBuf) {
        if let Some(mut old_sup) = self.supervisor.take() {
            info!("Unloading active model supervisor...");
            let _ = old_sup.stop().await;
        }

        let model_name = model_path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("unknown")
            .to_string();

        let profile = SystemProfile::probe();
        let threads = self.config.hardware.acceleration.cpu_threads;
        let gpu_layers = if self.config.hardware.acceleration.prefer_gpu {
            self.config.hardware.acceleration.gpu_layers
        } else {
            0
        };

        // Check if model overflows Node A's memory budget
        let model_size_bytes = std::fs::metadata(&model_path).map(|m| m.len()).unwrap_or(0);
        let kv_bytes = SystemProfile::estimate_kv_cache_bytes(4096);
        let total_required_mb = (model_size_bytes + kv_bytes) / (1024 * 1024);
        let host_cap_mb = profile.max_allowed_memory_bytes() / (1024 * 1024);

        let mut extra_args = Vec::new();

        if total_required_mb > host_cap_mb
            || total_required_mb > crate::cluster::NODE_A_MAX_STANDALONE_MB
        {
            let rpc_peer = self
                .discovery
                .select_rpc_candidate(crate::discovery::RpcSelectionPolicy {
                    max_thermal_index: 75,
                    max_allocatable_mb: self.config.cluster.max_rpc_ram_mb,
                    require_pairing: self.config.network.security.require_pairing,
                })
                .await;
            let rpc_endpoint = rpc_peer.map(|candidate| candidate.peer.rpc_endpoint());
            let remote_ram = if rpc_endpoint.is_some() {
                Some(self.config.cluster.max_rpc_ram_mb)
            } else {
                None
            };
            let budget = crate::cluster::ClusterCoordinator::calculate_budget(
                profile.available_ram_mb,
                remote_ram,
            );

            let total_layers = if let Ok(gguf) = crate::gguf::GgufMetadata::open(&model_path) {
                gguf.block_count.unwrap_or(32) as u32
            } else {
                32
            };

            match crate::cluster::ClusterCoordinator::plan_layer_split(
                model_size_bytes,
                kv_bytes,
                total_layers,
                &budget,
                rpc_endpoint.as_deref(),
            ) {
                Ok(split) => {
                    extra_args = split.build_llama_args();
                }
                Err(e) => {
                    self.status_message = Some((format!("Memory budget error: {}", e), Color::Red));
                    return;
                }
            }
        }

        let server_cfg = LlamaServerConfig {
            binary_path: PathBuf::from("llama-server"),
            model_path: model_path.clone(),
            host: self.config.network.api_host.clone(),
            port: self.config.network.api_port,
            gpu_layers,
            threads,
            context_size: 4096,
            extra_args,
        };

        info!("Spawning llama-server for model: {:?}", model_path);
        match ProcessSupervisor::spawn_with_fallback(server_cfg).await {
            Ok(sup) => {
                self.supervisor = Some(sup);
                self.active_model_name = model_name.clone();
                self.discovery.set_active_model(&model_name).await;

                // Reconfigure chat client to local host
                let endpoint = format!("http://127.0.0.1:{}", self.config.network.api_port);
                self.chat.client = NexusClient::new(endpoint);
                self.chat.model_name = model_name;
                self.chat.messages.clear();
                self.chat.messages.push(ChatMessage::assistant(format!(
                    "Model '{}' loaded successfully and ready for inference.",
                    self.active_model_name
                )));

                self.status_message =
                    Some((format!("Active: {}", self.active_model_name), Color::Green));
                // Automatically switch to Chat tab!
                self.set_tab(HubTab::Chat);
            }
            Err(e) => {
                error!("Failed to launch model supervisor: {}", e);
                self.status_message = Some((format!("Launch failed: {}", e), Color::Red));
            }
        }
    }

    pub fn render(&self, frame: &mut Frame) {
        let area = frame.area();

        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(3), // Top navigation tab bar
                Constraint::Min(10),   // Active tab contents
                Constraint::Length(1), // Global hotkeys footer
            ])
            .split(area);

        self.render_top_tabs(frame, chunks[0]);

        match self.active_tab {
            HubTab::Chat => self.chat.render_in_area(frame, chunks[1]),
            HubTab::Models => self.models_view.render(frame, chunks[1]),
            HubTab::Dashboard => self.dashboard_view.render_in_area(frame, chunks[1]),
            HubTab::Tunnel => self.tunnel_view.render(frame, chunks[1]),
            HubTab::Settings => self.settings_view.render(frame, chunks[1]),
        }

        self.render_footer(frame, chunks[2]);

        // Render hot-swap confirmation modal if triggered
        if let Some(target_path) = &self.pending_hot_swap_path {
            self.render_hot_swap_modal(frame, area, target_path);
        }
    }

    fn render_top_tabs(&self, frame: &mut Frame, area: Rect) {
        let titles = vec![
            HubTab::Chat.title(),
            HubTab::Models.title(),
            HubTab::Dashboard.title(),
            HubTab::Tunnel.title(),
            HubTab::Settings.title(),
        ];

        let selected = self.active_tab as usize;
        let tabs = Tabs::new(titles)
            .select(selected)
            .block(
                Block::default()
                    .title(" Nexus-LLM Unified Hub ")
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(Color::DarkGray)),
            )
            .style(Style::default().fg(Color::Gray))
            .highlight_style(
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD)
                    .bg(Color::Rgb(20, 30, 40)),
            );

        frame.render_widget(tabs, area);
    }

    fn render_footer(&self, frame: &mut Frame, area: Rect) {
        let active_str = format!(
            "Model: {} | Host: {}",
            self.active_model_name,
            self.chat.client.endpoint()
        );
        let footer_line = Line::from(vec![
            Span::styled(
                " [F1-F5] Tabs ",
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(" | ", Style::default().fg(Color::DarkGray)),
            Span::styled(active_str, Style::default().fg(Color::White)),
            Span::styled(" | ", Style::default().fg(Color::DarkGray)),
            Span::styled(" [Ctrl+C] Quit", Style::default().fg(Color::Red)),
        ]);

        let p = Paragraph::new(footer_line);
        frame.render_widget(p, area);
    }

    fn render_hot_swap_modal(&self, frame: &mut Frame, area: Rect, target_path: &PathBuf) {
        let modal_area = centered_rect(60, 25, area);
        frame.render_widget(Clear, modal_area);

        let target_name = target_path
            .file_name()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_else(|| "new model".to_string());

        let lines = vec![
            Line::from(vec![Span::styled(
                " Model Hot-Swap Confirmation",
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            )]),
            Line::from(vec![Span::styled(
                format!("\nActive Model:   {}", self.active_model_name),
                Style::default().fg(Color::White),
            )]),
            Line::from(vec![Span::styled(
                format!("Target Model:   {}", target_name),
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            )]),
            Line::from(vec![Span::styled(
                "\nUnload active model and launch new model? [Y / N]",
                Style::default()
                    .fg(Color::Green)
                    .add_modifier(Modifier::BOLD),
            )]),
        ];

        let block = Paragraph::new(lines).alignment(Alignment::Center).block(
            Block::default()
                .title(" Hot-Swap ")
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::Yellow)),
        );

        frame.render_widget(block, modal_area);
    }
}

/// Helper function to create a centered Rect for modals.
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

/// Launch and execute the main unified hub event loop.
pub async fn run_hub_tui(mut hub: HubApp) -> Result<(), Box<dyn std::error::Error>> {
    enable_raw_mode()?;
    let mut stdout = stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let mut event_stream = EventStream::new();
    let (tx, mut rx) = mpsc::channel::<StreamMsg>(100);
    let mut refresh_interval = tokio::time::interval(Duration::from_millis(500));

    loop {
        terminal.draw(|f| hub.render(f))?;

        tokio::select! {
            _ = refresh_interval.tick() => {
                if hub.active_tab == HubTab::Dashboard {
                    hub.dashboard_view.refresh().await;
                }
            }
            Some(event_res) = event_stream.next() => {
                if let Ok(Event::Key(key)) = event_res {
                    // Global quit: Ctrl+C
                    if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
                        break;
                    }

                    // Hot-Swap modal handling
                    if let Some(target_path) = hub.pending_hot_swap_path.take() {
                        match key.code {
                            KeyCode::Char('y') | KeyCode::Char('Y') | KeyCode::Enter => {
                                hub.execute_model_load(target_path).await;
                            }
                            KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => {
                                hub.status_message = Some(("Hot-swap cancelled".to_string(), Color::DarkGray));
                            }
                            _ => {
                                hub.pending_hot_swap_path = Some(target_path);
                            }
                        }
                        continue;
                    }

                    // Global function keys F1..F5
                    match key.code {
                        KeyCode::F(1) => { hub.set_tab(HubTab::Chat); continue; }
                        KeyCode::F(2) => { hub.set_tab(HubTab::Models); continue; }
                        KeyCode::F(3) => { hub.set_tab(HubTab::Dashboard); continue; }
                        KeyCode::F(4) => { hub.set_tab(HubTab::Tunnel); continue; }
                        KeyCode::F(5) => { hub.set_tab(HubTab::Settings); continue; }
                        _ => {}
                    }

                    // Tab-specific key routing
                    match hub.active_tab {
                        HubTab::Chat => {
                            if key.code == KeyCode::Tab {
                                hub.next_tab();
                            } else if key.code == KeyCode::BackTab {
                                hub.previous_tab();
                            } else {
                                hub.chat.handle_key_input(key, &tx);
                            }
                        }
                        HubTab::Models => {
                            match key.code {
                                KeyCode::Up | KeyCode::Char('k') => hub.models_view.previous(),
                                KeyCode::Down | KeyCode::Char('j') => hub.models_view.next(),
                                KeyCode::Char('r') | KeyCode::Char('R') => hub.models_view.refresh(),
                                KeyCode::Enter => {
                                    if let Some(m) = hub.models_view.selected_model() {
                                        let path = m.path.clone();
                                        hub.request_model_load(path).await;
                                    }
                                }
                                KeyCode::Tab => hub.next_tab(),
                                KeyCode::BackTab => hub.previous_tab(),
                                _ => {}
                            }
                        }
                        HubTab::Dashboard => {
                            match key.code {
                                KeyCode::Char('r') | KeyCode::Char('R') => hub.dashboard_view.refresh().await,
                                KeyCode::Tab => hub.next_tab(),
                                KeyCode::BackTab => hub.previous_tab(),
                                _ => {}
                            }
                        }
                        HubTab::Tunnel => {
                            match key.code {
                                KeyCode::Char('f') | KeyCode::Char('F') => hub.tunnel_view.setup_tunnel(),
                                KeyCode::Char('t') | KeyCode::Char('T') => hub.tunnel_view.teardown_tunnel(),
                                KeyCode::Char('r') | KeyCode::Char('R') => hub.tunnel_view.refresh(),
                                KeyCode::Tab => hub.next_tab(),
                                KeyCode::BackTab => hub.previous_tab(),
                                _ => {}
                            }
                        }
                        HubTab::Settings => {
                            match key.code {
                                KeyCode::Up | KeyCode::Char('k') => hub.settings_view.previous(),
                                KeyCode::Down | KeyCode::Char('j') => hub.settings_view.next(),
                                KeyCode::Char(' ') | KeyCode::Enter => hub.settings_view.toggle_or_adjust(false, false),
                                KeyCode::Left | KeyCode::Char('h') => hub.settings_view.toggle_or_adjust(true, false),
                                KeyCode::Right | KeyCode::Char('l') => hub.settings_view.toggle_or_adjust(false, true),
                                KeyCode::Char('s') | KeyCode::Char('S') => { let _ = hub.settings_view.save(); },
                                KeyCode::Tab => hub.next_tab(),
                                KeyCode::BackTab => hub.previous_tab(),
                                _ => {}
                            }
                        }
                    }
                }
            }
            Some(stream_msg) = rx.recv() => {
                match stream_msg {
                    StreamMsg::Token(token) => {
                        hub.chat.handle_stream_token(token);
                        hub.chat.auto_scroll = true;
                    }
                    StreamMsg::Done => {
                        hub.chat.finalize_stream();
                        hub.chat.auto_scroll = true;
                    }
                    StreamMsg::Error(err) => {
                        hub.chat.finalize_stream();
                        hub.chat.messages.push(ChatMessage::assistant(format!("⚠️ [Connection / Generation Error]: {}", err)));
                        hub.chat.status_message = Some(format!("Error: {}", err));
                        hub.chat.auto_scroll = true;
                    }
                }
            }
        }
    }

    // Cleanup terminal on exit
    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;

    if let Some(mut sup) = hub.supervisor {
        info!("Stopping active supervisor process on hub exit...");
        let _ = sup.stop().await;
    }

    Ok(())
}
