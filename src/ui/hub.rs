use crate::client::{ChatMessage, NexusClient};
use crate::config::NexusConfig;
use crate::discovery::DiscoveryService;
use crate::supervisor::{LlamaServerConfig, ProcessSupervisor};
use crate::sysinfo::SystemProfile;
use crate::ui::chat::{ChatApp, StreamMsg};
use crate::ui::cluster_view::ClusterView;
use crate::ui::models_view::ModelsView;
use crate::ui::settings_view::SettingsView;
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
use uuid::Uuid;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HubTab {
    Chat = 0,
    Models = 1,
    Cluster = 2,
    Settings = 3,
}

impl HubTab {
    pub fn title(&self) -> &'static str {
        match self {
            Self::Chat => " [F1] 💬 Chat ",
            Self::Models => " [F2] 📦 Models ",
            Self::Cluster => " [F3] 🌐 Cluster ",
            Self::Settings => " [F4] ⚙️ Settings ",
        }
    }
}

/// Target execution node option for running model weights.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TargetExecutionNode {
    Local {
        allocatable_mb: u64,
        backend: String,
    },
    Remote {
        uuid: Uuid,
        name: String,
        endpoint: String,
        free_ram_mb: u32,
        backend: String,
    },
}

impl TargetExecutionNode {
    pub fn display_label(&self) -> String {
        match self {
            Self::Local { allocatable_mb, backend } => {
                format!("🖥️  Local Machine (This Device) - {} MB allocatable | {}", allocatable_mb, backend)
            }
            Self::Remote { name, endpoint, free_ram_mb, backend, .. } => {
                format!("📱 {} ({}) - {} MB free | {}", name, endpoint, free_ram_mb, backend)
            }
        }
    }
}

/// Active state when the Target Node Selection modal is visible.
#[derive(Debug, Clone)]
pub struct TargetSelectionState {
    pub model_path: PathBuf,
    pub model_name: String,
    pub candidates: Vec<TargetExecutionNode>,
    pub selected_idx: usize,
}

pub struct HubApp {
    pub config: NexusConfig,
    pub discovery: Arc<DiscoveryService>,
    pub active_tab: HubTab,
    pub chat: ChatApp,
    pub models_view: ModelsView,
    pub cluster_view: ClusterView,
    pub settings_view: SettingsView,
    pub supervisor: Option<ProcessSupervisor>,
    pub active_model_name: String,
    pub pending_hot_swap_path: Option<PathBuf>,
    pub pending_target_selection: Option<TargetSelectionState>,
    pub status_message: Option<(String, Color)>,
}

impl HubApp {
    pub fn new(config: NexusConfig, client: NexusClient, discovery: Arc<DiscoveryService>) -> Self {
        let models_view = ModelsView::new(config.node.models_dir.clone());
        let cluster_view = ClusterView::new(discovery.clone());
        let settings_view = SettingsView::new(config.clone());
        let chat = ChatApp::new(client, "default", None);

        Self {
            config,
            discovery,
            active_tab: HubTab::Chat,
            chat,
            models_view,
            cluster_view,
            settings_view,
            supervisor: None,
            active_model_name: "None (Idle)".to_string(),
            pending_hot_swap_path: None,
            pending_target_selection: None,
            status_message: None,
        }
    }

    pub fn set_tab(&mut self, tab: HubTab) {
        self.active_tab = tab;
        if tab == HubTab::Models {
            self.models_view.refresh();
        }
    }

    pub fn next_tab(&mut self) {
        let next_idx = ((self.active_tab as usize) + 1) % 4;
        self.set_tab(match next_idx {
            0 => HubTab::Chat,
            1 => HubTab::Models,
            2 => HubTab::Cluster,
            _ => HubTab::Settings,
        });
    }

    pub fn previous_tab(&mut self) {
        let prev_idx = if (self.active_tab as usize) == 0 {
            3
        } else {
            (self.active_tab as usize) - 1
        };
        self.set_tab(match prev_idx {
            0 => HubTab::Chat,
            1 => HubTab::Models,
            2 => HubTab::Cluster,
            _ => HubTab::Settings,
        });
    }

    /// Open target execution device selector modal for a model.
    pub async fn open_target_selection(&mut self, model_path: PathBuf) {
        let model_name = model_path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("unknown")
            .to_string();

        let profile = SystemProfile::probe();
        let local_cap_mb = profile.max_allowed_memory_bytes() / (1024 * 1024);
        let mut candidates = vec![TargetExecutionNode::Local {
            allocatable_mb: local_cap_mb,
            backend: profile.detected_backend.to_string(),
        }];

        let peers = self.discovery.get_active_peers().await;
        for p in peers {
            if p.status.is_ready() || p.role.is_host() || p.is_rpc_ready() {
                let short_id = p.uuid.to_string();
                let name = format!("Node-{}", &short_id[..8]);
                candidates.push(TargetExecutionNode::Remote {
                    uuid: p.uuid,
                    name,
                    endpoint: p.api_endpoint(),
                    free_ram_mb: p.free_ram_mb,
                    backend: p.backend.to_string(),
                });
            }
        }

        self.pending_target_selection = Some(TargetSelectionState {
            model_path,
            model_name,
            candidates,
            selected_idx: 0,
        });
    }

    /// Confirm and execute the selected target node.
    pub async fn execute_target_selection(&mut self) {
        if let Some(state) = self.pending_target_selection.take() {
            if let Some(target) = state.candidates.get(state.selected_idx) {
                match target {
                    TargetExecutionNode::Local { .. } => {
                        self.request_model_load(state.model_path).await;
                    }
                    TargetExecutionNode::Remote { endpoint, name, .. } => {
                        let client = reqwest::Client::new();
                        let req = crate::control_plane::ModelLoadRequest {
                            protocol_version: crate::control_plane::CONTROL_PLANE_VERSION,
                            requester_id: self.config.node_uuid().unwrap_or_else(|_| Uuid::new_v4()),
                            model_path: state.model_name.clone(),
                            context_size: 4096,
                            gpu_layers: if self.config.hardware.acceleration.prefer_gpu { 99 } else { 0 },
                            threads: self.config.hardware.acceleration.cpu_threads,
                            rpc_workers: Vec::new(),
                        };

                        info!("Dispatching remote model load to {}: {:?}", endpoint, req);
                        match crate::control_plane::dispatch_load_model(&client, endpoint, &req).await {
                            Ok(resp) if resp.success => {
                                self.active_model_name = state.model_name.clone();
                                let target_api = if !resp.api_endpoint.is_empty() {
                                    resp.api_endpoint
                                } else {
                                    endpoint.clone()
                                };
                                self.chat.client = NexusClient::new(target_api.clone());
                                self.chat.model_name = state.model_name.clone();
                                self.chat.messages.clear();
                                self.chat.messages.push(ChatMessage::assistant(format!(
                                    "Connected to remote model '{}' running on {}. Ready for inference.",
                                    state.model_name, name
                                )));
                                self.status_message = Some((
                                    format!("Active on {}: {}", name, state.model_name),
                                    Color::Green,
                                ));
                                self.set_tab(HubTab::Chat);
                            }
                            Ok(resp) => {
                                let err = resp.error_message.unwrap_or_else(|| "Unknown error".to_string());
                                self.status_message = Some((
                                    format!("Remote load failed on {}: {}", name, err),
                                    Color::Red,
                                ));
                            }
                            Err(e) => {
                                self.status_message = Some((
                                    format!("Remote dispatch failed to {}: {}", name, e),
                                    Color::Red,
                                ));
                            }
                        }
                    }
                }
            }
        }
    }

    /// Request to launch a model. If a model is already active, prompts for hot-swap confirmation.
    pub async fn request_model_load(&mut self, model_path: PathBuf) {
        if self.supervisor.is_some() {
            self.pending_hot_swap_path = Some(model_path);
        } else {
            self.execute_model_load(model_path).await;
        }
    }

    /// Stop any active supervisor and load the new model process locally.
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

        // Check if model overflows local host memory budget
        let model_size_bytes = std::fs::metadata(&model_path).map(|m| m.len()).unwrap_or(0);
        let kv_bytes = SystemProfile::estimate_kv_cache_bytes(4096);
        let total_required_mb = (model_size_bytes + kv_bytes) / (1024 * 1024);
        let host_cap_mb = profile.max_allowed_memory_bytes() / (1024 * 1024);

        let mut extra_args = Vec::new();

        if total_required_mb > host_cap_mb {
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
            binary_path: PathBuf::from(&self.config.node.llama_server_binary),
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
            HubTab::Cluster => self.cluster_view.render(frame, chunks[1]),
            HubTab::Settings => self.settings_view.render(frame, chunks[1]),
        }

        self.render_footer(frame, chunks[2]);

        // Render hot-swap confirmation modal if triggered
        if let Some(target_path) = &self.pending_hot_swap_path {
            self.render_hot_swap_modal(frame, area, target_path);
        }

        // Render target node selection modal if triggered
        if let Some(target_state) = &self.pending_target_selection {
            self.render_target_selection_modal(frame, area, target_state);
        }
    }

    fn render_top_tabs(&self, frame: &mut Frame, area: Rect) {
        let titles = vec![
            HubTab::Chat.title(),
            HubTab::Models.title(),
            HubTab::Cluster.title(),
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
                " [F1-F4] Tabs ",
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(" | ", Style::default().fg(Color::DarkGray)),
            Span::styled(active_str, Style::default().fg(Color::White)),
            Span::styled(" | ", Style::default().fg(Color::DarkGray)),
            Span::styled(" [C] Connect Peer ", Style::default().fg(Color::Cyan)),
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

    fn render_target_selection_modal(
        &self,
        frame: &mut Frame,
        area: Rect,
        state: &TargetSelectionState,
    ) {
        let modal_area = centered_rect(75, 45, area);
        frame.render_widget(Clear, modal_area);

        let mut lines = vec![
            Line::from(vec![Span::styled(
                " Select Target Execution Device ",
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            )]),
            Line::from(vec![Span::styled(
                format!("Model: {} | Choose device to run weights:\n", state.model_name),
                Style::default().fg(Color::White),
            )]),
        ];

        for (i, candidate) in state.candidates.iter().enumerate() {
            let is_sel = i == state.selected_idx;
            let prefix = if is_sel { " > " } else { "   " };
            let style = if is_sel {
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(Color::Gray)
            };
            lines.push(Line::from(vec![
                Span::styled(prefix, style),
                Span::styled(format!("[{}] {}", i, candidate.display_label()), style),
            ]));
        }

        lines.push(Line::from(""));
        lines.push(Line::from(vec![Span::styled(
            " [↑ / ↓] Navigate  |  [Enter] Confirm & Launch  |  [Esc] Cancel ",
            Style::default()
                .fg(Color::DarkGray)
                .add_modifier(Modifier::BOLD),
        )]));

        let block = Paragraph::new(lines).block(
            Block::default()
                .title(" Target Node Selection ")
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::Cyan)),
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
                if hub.active_tab == HubTab::Cluster {
                    hub.cluster_view.refresh().await;
                }
            }
            Some(event_res) = event_stream.next() => {
                if let Ok(Event::Key(key)) = event_res {
                    // Global quit: Ctrl+C
                    if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
                        break;
                    }

                    // Target Selection modal handling
                    if let Some(target_state) = &mut hub.pending_target_selection {
                        match key.code {
                            KeyCode::Up | KeyCode::Char('k') => {
                                if target_state.selected_idx > 0 {
                                    target_state.selected_idx -= 1;
                                } else if !target_state.candidates.is_empty() {
                                    target_state.selected_idx = target_state.candidates.len() - 1;
                                }
                                continue;
                            }
                            KeyCode::Down | KeyCode::Char('j') => {
                                if !target_state.candidates.is_empty() {
                                    target_state.selected_idx = (target_state.selected_idx + 1) % target_state.candidates.len();
                                }
                                continue;
                            }
                            KeyCode::Enter => {
                                hub.execute_target_selection().await;
                                continue;
                            }
                            KeyCode::Esc => {
                                hub.pending_target_selection = None;
                                hub.status_message = Some(("Target selection cancelled".to_string(), Color::DarkGray));
                                continue;
                            }
                            _ => {
                                continue;
                            }
                        }
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

                    // Global function keys F1..F4
                    match key.code {
                        KeyCode::F(1) => { hub.set_tab(HubTab::Chat); continue; }
                        KeyCode::F(2) => { hub.set_tab(HubTab::Models); continue; }
                        KeyCode::F(3) => { hub.set_tab(HubTab::Cluster); hub.cluster_view.refresh().await; continue; }
                        KeyCode::F(4) => { hub.set_tab(HubTab::Settings); continue; }
                        _ => {}
                    }

                    // Tab-specific key routing
                    match hub.active_tab {
                        HubTab::Chat => {
                            if key.code == KeyCode::Tab {
                                hub.next_tab();
                            } else if key.code == KeyCode::BackTab {
                                hub.previous_tab();
                            } else if key.code == KeyCode::Char('c') || key.code == KeyCode::Char('C') {
                                let peers = hub.discovery.get_active_peers().await;
                                if let Some(active_peer) = peers.iter().find(|p| !p.active_model.is_empty() || p.status.is_ready()) {
                                    let ep = active_peer.api_endpoint();
                                    let model = if !active_peer.active_model.is_empty() {
                                        active_peer.active_model.clone()
                                    } else {
                                        "cluster-model".to_string()
                                    };
                                    hub.chat.client = NexusClient::new(ep.clone());
                                    hub.chat.model_name = model.clone();
                                    hub.active_model_name = model.clone();
                                    hub.status_message = Some((format!("Connected to cluster host at {}", ep), Color::Green));
                                    hub.chat.messages.push(ChatMessage::assistant(format!(
                                        "Connected to active cluster host at {}. Ready for chat.", ep
                                    )));
                                } else {
                                    hub.status_message = Some(("No active cluster host found".to_string(), Color::Yellow));
                                }
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
                                        hub.open_target_selection(path).await;
                                    }
                                }
                                KeyCode::Tab => hub.next_tab(),
                                KeyCode::BackTab => hub.previous_tab(),
                                _ => {}
                            }
                        }
                        HubTab::Cluster => {
                            if hub.cluster_view.adding_peer {
                                match key.code {
                                    KeyCode::Enter => {
                                        let input = hub.cluster_view.add_peer_input.trim().to_string();
                                        if !input.is_empty() {
                                            if !hub.config.network.static_peers.contains(&input) {
                                                hub.config.network.static_peers.push(input.clone());
                                                let _ = hub.config.save();
                                            }
                                            hub.discovery.add_static_peer(&input).await;
                                            hub.cluster_view.adding_peer = false;
                                            hub.cluster_view.add_peer_input.clear();
                                            hub.cluster_view.status_message = Some((
                                                format!("Static peer '{}' added & probed", input),
                                                Color::Green,
                                            ));
                                            hub.cluster_view.refresh().await;
                                        } else {
                                            hub.cluster_view.cancel_add_peer();
                                        }
                                    }
                                    KeyCode::Esc => {
                                        hub.cluster_view.cancel_add_peer();
                                    }
                                    KeyCode::Backspace => {
                                        hub.cluster_view.backspace_add_char();
                                    }
                                    KeyCode::Char(c) => {
                                        hub.cluster_view.push_add_char(c);
                                    }
                                    _ => {}
                                }
                            } else if hub.cluster_view.show_info_modal {
                                match key.code {
                                    KeyCode::Char('i') | KeyCode::Char('I') | KeyCode::Esc => {
                                        hub.cluster_view.toggle_info_modal();
                                    }
                                    _ => {}
                                }
                            } else {
                                match key.code {
                                    KeyCode::Up | KeyCode::Char('k') => hub.cluster_view.previous(),
                                    KeyCode::Down | KeyCode::Char('j') => hub.cluster_view.next(),
                                    KeyCode::Enter => {
                                        if let Some(peer) = hub.cluster_view.selected_peer() {
                                            let ep = peer.api_endpoint();
                                            let model = if !peer.active_model.is_empty() {
                                                peer.active_model.clone()
                                            } else {
                                                "cluster-peer".to_string()
                                            };
                                            hub.chat.client = NexusClient::new(ep.clone());
                                            hub.chat.model_name = model.clone();
                                            hub.active_model_name = model.clone();
                                            hub.status_message = Some((format!("Connected to peer at {}", ep), Color::Green));
                                            hub.chat.messages.push(ChatMessage::assistant(format!(
                                                "Connected to remote peer '{}' at {}. Ready for chat.",
                                                peer.uuid, ep
                                            )));
                                            hub.set_tab(HubTab::Chat);
                                        } else {
                                            hub.cluster_view.status_message = Some(("No peer selected to connect".to_string(), Color::Yellow));
                                        }
                                    }
                                    KeyCode::Char('l') | KeyCode::Char('L') => {
                                        if let Some(peer) = hub.cluster_view.selected_peer() {
                                            let peer_uuid = peer.uuid;
                                            let peer_name = format!("Node-{}", &peer_uuid.to_string()[..8]);
                                            let peer_ep = peer.api_endpoint();
                                            if let Some(m) = hub.models_view.selected_model() {
                                                let client = reqwest::Client::new();
                                                let model_name = m.filename.clone();
                                                let req = crate::control_plane::ModelLoadRequest {
                                                    protocol_version: crate::control_plane::CONTROL_PLANE_VERSION,
                                                    requester_id: hub.config.node_uuid().unwrap_or_else(|_| Uuid::new_v4()),
                                                    model_path: model_name.clone(),
                                                    context_size: 4096,
                                                    gpu_layers: if hub.config.hardware.acceleration.prefer_gpu { 99 } else { 0 },
                                                    threads: hub.config.hardware.acceleration.cpu_threads,
                                                    rpc_workers: Vec::new(),
                                                };
                                                match crate::control_plane::dispatch_load_model(&client, &peer_ep, &req).await {
                                                    Ok(resp) if resp.success => {
                                                        hub.active_model_name = model_name.clone();
                                                        let target_api = if !resp.api_endpoint.is_empty() { resp.api_endpoint } else { peer_ep.clone() };
                                                        hub.chat.client = NexusClient::new(target_api);
                                                        hub.chat.model_name = model_name.clone();
                                                        hub.chat.messages.push(ChatMessage::assistant(format!(
                                                            "Loaded '{}' on remote node {}.", model_name, peer_name
                                                        )));
                                                        hub.status_message = Some((format!("Active on {}: {}", peer_name, model_name), Color::Green));
                                                        hub.set_tab(HubTab::Chat);
                                                    }
                                                    Ok(resp) => {
                                                        let err = resp.error_message.unwrap_or_else(|| "Unknown error".to_string());
                                                        hub.cluster_view.status_message = Some((format!("Load failed on {}: {}", peer_name, err), Color::Red));
                                                    }
                                                    Err(e) => {
                                                        hub.cluster_view.status_message = Some((format!("Dispatch error to {}: {}", peer_name, e), Color::Red));
                                                    }
                                                }
                                            } else {
                                                hub.cluster_view.status_message = Some(("Select a model in [F2] Models first".to_string(), Color::Yellow));
                                            }
                                        }
                                    }
                                    KeyCode::Char('w') | KeyCode::Char('W') => {
                                        if let Some(peer) = hub.cluster_view.selected_peer() {
                                            let ep = peer.api_endpoint();
                                            let short_id = &peer.uuid.to_string()[..8];
                                            hub.cluster_view.status_message = Some((
                                                format!("Requested Node-{} at {} to stand by for RPC worker offload", short_id, ep),
                                                Color::Cyan,
                                            ));
                                        }
                                    }
                                    KeyCode::Char('i') | KeyCode::Char('I') => hub.cluster_view.toggle_info_modal(),
                                    KeyCode::Char('a') | KeyCode::Char('A') => hub.cluster_view.start_add_peer(),
                                    KeyCode::Char('d') | KeyCode::Char('D') => {
                                        let local_ep = format!("http://127.0.0.1:{}", hub.config.network.api_port);
                                        hub.chat.client = NexusClient::new(local_ep.clone());
                                        hub.status_message = Some(("Reset chat target to local node".to_string(), Color::Green));
                                        hub.chat.messages.push(ChatMessage::assistant(format!("Disconnected from peer. Reverted to local endpoint: {}", local_ep)));
                                    }
                                    KeyCode::Char('r') | KeyCode::Char('R') => hub.cluster_view.refresh().await,
                                    KeyCode::Tab => hub.next_tab(),
                                    KeyCode::BackTab => hub.previous_tab(),
                                    _ => {}
                                }
                            }
                        }
                        HubTab::Settings => {
                            if hub.settings_view.editing_text {
                                match key.code {
                                    KeyCode::Enter => hub.settings_view.commit_text(),
                                    KeyCode::Esc => hub.settings_view.cancel_text(),
                                    KeyCode::Backspace => hub.settings_view.backspace_text(),
                                    KeyCode::Char(c) => hub.settings_view.push_char(c),
                                    _ => {}
                                }
                            } else {
                                match key.code {
                                    KeyCode::Up | KeyCode::Char('k') => hub.settings_view.previous(),
                                    KeyCode::Down | KeyCode::Char('j') => hub.settings_view.next(),
                                    KeyCode::Enter => {
                                        if hub.settings_view.is_current_text() {
                                            hub.settings_view.start_editing();
                                        } else {
                                            hub.settings_view.toggle_or_adjust(false, false);
                                        }
                                    }
                                    KeyCode::Char(' ') => hub.settings_view.toggle_or_adjust(false, false),
                                    KeyCode::Left | KeyCode::Char('h') => hub.settings_view.toggle_or_adjust(true, false),
                                    KeyCode::Right | KeyCode::Char('l') => hub.settings_view.toggle_or_adjust(false, true),
                                    KeyCode::Char('s') | KeyCode::Char('S') => {
                                        if let Ok(()) = hub.settings_view.save() {
                                            hub.config = hub.settings_view.config.clone();
                                            hub.models_view.models_dir = hub.config.node.models_dir.clone();
                                            hub.models_view.refresh();
                                            if let Some(host) = &hub.config.network.default_host {
                                                hub.chat.client = NexusClient::new(host.clone());
                                            }
                                            hub.status_message = Some(("Settings saved & hot-reloaded".to_string(), Color::Green));
                                        }
                                    }
                                    KeyCode::Tab => hub.next_tab(),
                                    KeyCode::BackTab => hub.previous_tab(),
                                    _ => {}
                                }
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
