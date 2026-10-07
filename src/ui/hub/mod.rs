//! Unified Nexus Hub TUI (Phase 8 — responsive command/event architecture).

pub mod commands;
pub mod keymap;

use crate::client::NexusClient;
use crate::config::NexusConfig;
use crate::control_plane::{dispatch_pair, PairRequest, CONTROL_PLANE_VERSION};
use crate::control_plane_server::{spawn as spawn_control_plane, ControlPlaneContext};
use crate::discovery::{DiscoveryService, NodeRole};
use crate::node_identity::NodeIdentity;
use crate::registry_runtime::spawn_registry_runtime;
use crate::supervisor::SupervisorManager;
use crate::ui::chat::{ChatApp, ChatEntry, StreamMsg};
use crate::ui::cluster_view::ClusterView;
use crate::ui::models_view::ModelsView;
use crate::ui::settings_view::SettingsView;
use commands::{request_load_or_hot_swap, spawn_hub_worker, HubCommand, HubEvent, HubWorkerCtx};
use crossterm::{
    event::{Event, EventStream, KeyCode, KeyModifiers},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use futures_util::StreamExt;
use keymap::{resolve, HubAction, KeyScope};
use ratatui::{
    backend::CrosstermBackend,
    layout::{Alignment, Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, Gauge, Paragraph, Tabs},
    Frame, Terminal,
};
use std::io::stdout;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;
use tracing::{debug, info};
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
        gpu_layers: u32,
    },
    LocalCpu {
        allocatable_mb: u64,
        backend: String,
        threads: usize,
    },
    Remote {
        uuid: Uuid,
        name: String,
        endpoint: String,
        api_endpoint: String,
        free_ram_mb: u32,
        backend: String,
    },
}

impl TargetExecutionNode {
    pub fn display_label(&self) -> String {
        match self {
            Self::Local {
                allocatable_mb,
                backend,
                gpu_layers,
            } => {
                format!(
                    "⚡ Local GPU (Accelerated - {} layers) - {} MB allocatable | {}",
                    gpu_layers, allocatable_mb, backend
                )
            }
            Self::LocalCpu {
                allocatable_mb,
                backend,
                threads,
            } => {
                format!(
                    "🛡️  Local CPU (Safe Mode - 0 GPU layers, {} threads) - {} MB allocatable | {}",
                    threads, allocatable_mb, backend
                )
            }
            Self::Remote {
                name,
                endpoint,
                free_ram_mb,
                backend,
                ..
            } => {
                format!(
                    "📱 {} ({}) - {} MB free | {}",
                    name, endpoint, free_ram_mb, backend
                )
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

/// Full hot-swap intent — path + GPU layers + context (CAPABILITY_REVIEW §2.6).
#[derive(Debug, Clone)]
pub struct HotSwapIntent {
    pub path: PathBuf,
    pub gpu_layers: Option<u32>,
    pub context_size: usize,
}

pub struct HubApp {
    pub config: NexusConfig,
    pub discovery: Arc<DiscoveryService>,
    pub active_tab: HubTab,
    pub chat: ChatApp,
    pub models_view: ModelsView,
    pub cluster_view: ClusterView,
    pub settings_view: SettingsView,
    pub supervisor: SupervisorManager,
    pub active_model_name: String,
    pub pending_hot_swap: Option<HotSwapIntent>,
    pub pending_target_selection: Option<TargetSelectionState>,
    pub status_message: Option<(String, Color)>,
    pub identity: Arc<NodeIdentity>,
    pub shared_config: Arc<std::sync::RwLock<NexusConfig>>,
    pub config_path: std::path::PathBuf,
    pub load_phase: Option<String>,
    pub show_help: bool,
    /// URL input modal for Models [D].
    pub pending_download_url: Option<String>,
    /// Peer picker for [S] push (list of (label, endpoint)).
    pub pending_push_peers: Option<Vec<(String, String)>>,
    pub push_peer_idx: usize,
    // (label, percent, downloaded, total, speed) — keep compact until a dedicated progress type.
    #[allow(clippy::type_complexity)]
    pub download_progress: Option<(String, Option<f32>, u64, Option<u64>, f64)>,
}

impl HubApp {
    pub fn new(
        config: NexusConfig,
        client: NexusClient,
        discovery: Arc<DiscoveryService>,
        identity: Arc<NodeIdentity>,
        shared_config: Arc<std::sync::RwLock<NexusConfig>>,
        config_path: std::path::PathBuf,
    ) -> Self {
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
            supervisor: SupervisorManager::new(),
            active_model_name: "None (Idle)".to_string(),
            pending_hot_swap: None,
            pending_target_selection: None,
            status_message: None,
            identity,
            shared_config,
            config_path,
            load_phase: None,
            show_help: false,
            pending_download_url: None,
            pending_push_peers: None,
            push_peer_idx: 0,
            download_progress: None,
        }
    }

    pub fn sync_shared_config(&mut self) {
        if let Ok(mut shared) = self.shared_config.write() {
            *shared = self.config.clone();
        }
    }

    pub async fn submit_pairing_code(&mut self, code: String) -> Result<(), String> {
        let peer = self
            .cluster_view
            .selected_peer()
            .ok_or_else(|| "No peer selected".to_string())?
            .clone();
        let control_ep = peer.control_endpoint();
        let requester_id = self.config.node_uuid().map_err(|e| e.to_string())?;
        let pair_req = PairRequest {
            protocol_version: CONTROL_PLANE_VERSION,
            requester_id,
            requester_public_key: self.identity.public_key_hex(),
            pairing_code: code,
        };
        let client = reqwest::Client::new();
        let resp = dispatch_pair(&client, &control_ep, &pair_req, &self.identity)
            .await
            .map_err(|e| e.to_string())?;
        if !resp.success {
            return Err(resp.message);
        }
        self.config
            .network
            .security
            .record_pair(resp.node_id, resp.public_key);
        self.config
            .save_to_path(&self.config_path)
            .map_err(|e| e.to_string())?;
        self.sync_shared_config();
        Ok(())
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

    /// Build target-selection candidates (used by tests and as a sync fallback).
    pub async fn open_target_selection(&mut self, model_path: PathBuf) {
        let ctx = HubWorkerCtx {
            config: self.config.clone(),
            discovery: self.discovery.clone(),
            supervisor: self.supervisor.clone(),
            identity: self.identity.clone(),
        };
        self.pending_target_selection =
            Some(commands::build_target_selection(&ctx, model_path).await);
    }

    /// Unload the active model (tests + direct callers). Event-loop path uses HubCommand::Unload.
    pub async fn unload_active_model(&mut self) {
        let name = self.active_model_name.clone();
        let had_supervisor = self.supervisor.is_running().await;
        let had_named = name != "None (Idle)";
        if had_supervisor {
            let _ = self.supervisor.stop().await;
        }
        if had_supervisor || had_named {
            self.discovery.set_active_model("").await;
            self.discovery
                .set_status_flags(crate::discovery::StatusFlags(0))
                .await;
            self.apply_event(HubEvent::ModelUnloaded {
                unloaded_model: name,
            });
        } else {
            self.apply_event(HubEvent::UnloadNoop);
        }
    }

    /// Apply a worker event on the UI thread (no awaits).
    pub fn apply_event(&mut self, event: HubEvent) {
        match event {
            HubEvent::Status { message, color } => {
                self.status_message = Some((message, color));
            }
            HubEvent::ModelLoadProgress { phase } => {
                self.load_phase = Some(phase.clone());
                self.status_message = Some((phase, Color::Yellow));
            }
            HubEvent::ModelLoaded {
                model_name,
                endpoint,
                backend_label,
                notice,
            } => {
                self.load_phase = None;
                self.active_model_name = model_name.clone();
                self.chat.client = NexusClient::new(endpoint);
                self.chat.model_name = model_name;
                self.chat.set_target_hardware("Local Host", backend_label);
                self.chat.messages.clear();
                self.chat.messages.push(ChatEntry::notice(notice));
                self.status_message =
                    Some((format!("Active: {}", self.active_model_name), Color::Green));
                self.set_tab(HubTab::Chat);
            }
            HubEvent::ModelFailed { message } => {
                self.load_phase = None;
                self.status_message = Some((message, Color::Red));
            }
            HubEvent::ModelUnloaded { unloaded_model } => {
                self.active_model_name = "None (Idle)".to_string();
                self.chat.model_name = "default".to_string();
                self.chat.messages.push(ChatEntry::notice(format!(
                    "Model '{}' unloaded. Local inference engine is idle.",
                    unloaded_model
                )));
                self.status_message =
                    Some((format!("Unloaded model '{}'", unloaded_model), Color::Cyan));
            }
            HubEvent::UnloadNoop => {
                self.status_message = Some((
                    "No local model is currently active to unload".to_string(),
                    Color::Yellow,
                ));
            }
            HubEvent::TargetSelectionReady(state) => {
                self.pending_target_selection = Some(state);
            }
            HubEvent::RemoteLoadSucceeded {
                model_name,
                name,
                backend,
                api_endpoint,
                notice,
            } => {
                self.load_phase = None;
                self.active_model_name = model_name.clone();
                self.chat.client = NexusClient::new(api_endpoint);
                self.chat.model_name = model_name;
                self.chat.set_target_hardware(&name, backend);
                self.chat.messages.clear();
                self.chat.messages.push(ChatEntry::notice(notice));
                self.status_message = Some((
                    format!("Active on {}: {}", name, self.active_model_name),
                    Color::Green,
                ));
                self.set_tab(HubTab::Chat);
            }
            HubEvent::RemoteLoadFailed { message } => {
                self.load_phase = None;
                self.status_message = Some((message, Color::Red));
            }
            HubEvent::ClusterRefreshed => {
                // Actual peer snapshot refresh happens via async helper outside apply;
                // status only — see run_hub_tui which awaits refresh when this arrives.
            }
            HubEvent::SupervisorCrashed {
                model,
                code,
                stderr,
            } => {
                self.active_model_name = "None (Idle)".to_string();
                self.chat.messages.push(ChatEntry::error(format!(
                    "⚠️ Local llama-server process terminated unexpectedly (code: {:?}). Stderr: {}",
                    code, stderr
                )));
                self.status_message = Some((
                    format!("Local server crashed for '{}' ({:?})", model, code),
                    Color::Red,
                ));
            }
            HubEvent::DownloadProgress {
                downloaded_bytes,
                total_bytes,
                percent,
                speed_bytes_per_sec,
                label,
            } => {
                self.download_progress = Some((
                    label,
                    percent,
                    downloaded_bytes,
                    total_bytes,
                    speed_bytes_per_sec,
                ));
            }
            HubEvent::DownloadFinished { message } => {
                self.download_progress = None;
                self.models_view.refresh();
                self.status_message = Some((message, Color::Green));
            }
            HubEvent::DownloadFailed { message } => {
                self.download_progress = None;
                self.status_message = Some((message, Color::Red));
            }
            HubEvent::ModelCatalogUpdated { remotes } => {
                self.models_view.apply_remote_catalogs(&remotes);
            }
        }
    }

    pub fn render(&mut self, frame: &mut Frame) {
        let area = frame.area();

        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(3),
                Constraint::Min(10),
                Constraint::Length(1),
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

        if let Some(intent) = &self.pending_hot_swap {
            self.render_hot_swap_modal(frame, area, intent);
        }

        if let Some(target_state) = &self.pending_target_selection {
            self.render_target_selection_modal(frame, area, target_state);
        }

        if self.show_help {
            self.render_help_modal(frame, area);
        }

        if let Some((label, percent, downloaded, total, speed)) = &self.download_progress {
            let modal = centered_rect(60, 20, area);
            frame.render_widget(Clear, modal);
            let ratio = percent
                .map(|p| (p as f64 / 100.0).clamp(0.0, 1.0))
                .unwrap_or(0.0);
            let chunks = Layout::default()
                .direction(Direction::Vertical)
                .constraints([
                    Constraint::Length(3),
                    Constraint::Length(3),
                    Constraint::Min(1),
                ])
                .margin(1)
                .split(modal);
            let title = Paragraph::new(Line::from(Span::styled(
                format!(" {label} "),
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            )))
            .alignment(Alignment::Center)
            .block(
                Block::default()
                    .title(" Transfer ")
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(Color::Yellow)),
            );
            frame.render_widget(title, modal);
            let gauge = Gauge::default()
                .gauge_style(Style::default().fg(Color::Cyan).bg(Color::DarkGray))
                .ratio(ratio)
                .label(format!(
                    "{:.1}%  {} / {}  ({:.1} MB/s)",
                    percent.unwrap_or(0.0),
                    downloaded,
                    total.map(|t| t.to_string()).unwrap_or_else(|| "?".into()),
                    speed / (1024.0 * 1024.0)
                ));
            frame.render_widget(gauge, chunks[1]);
        }

        if let Some(url) = &self.pending_download_url {
            let modal = centered_rect(70, 20, area);
            frame.render_widget(Clear, modal);
            let p = Paragraph::new(vec![
                Line::from(Span::styled(
                    " Enter GGUF URL (Enter=start, Esc=cancel) ",
                    Style::default().fg(Color::Yellow),
                )),
                Line::from(""),
                Line::from(Span::styled(url.clone(), Style::default().fg(Color::White))),
            ])
            .block(
                Block::default()
                    .title(" Download ")
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(Color::Cyan)),
            );
            frame.render_widget(p, modal);
        }

        if let Some(peers) = &self.pending_push_peers {
            let modal = centered_rect(50, 40, area);
            frame.render_widget(Clear, modal);
            let mut lines = vec![Line::from(Span::styled(
                " Select peer to receive model (Enter/Esc) ",
                Style::default().fg(Color::Yellow),
            ))];
            for (i, (label, _)) in peers.iter().enumerate() {
                let style = if i == self.push_peer_idx {
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(Color::White)
                };
                let prefix = if i == self.push_peer_idx { "> " } else { "  " };
                lines.push(Line::from(Span::styled(format!("{prefix}{label}"), style)));
            }
            let p = Paragraph::new(lines).block(
                Block::default()
                    .title(" Push to Peer ")
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(Color::Cyan)),
            );
            frame.render_widget(p, modal);
        }

        if let Some(phase) = &self.load_phase {
            let modal = centered_rect(50, 15, area);
            frame.render_widget(Clear, modal);
            let p = Paragraph::new(vec![
                Line::from(Span::styled(
                    " Model Load In Progress ",
                    Style::default()
                        .fg(Color::Yellow)
                        .add_modifier(Modifier::BOLD),
                )),
                Line::from(""),
                Line::from(Span::styled(
                    phase.clone(),
                    Style::default().fg(Color::White),
                )),
                Line::from(""),
                Line::from(Span::styled(
                    "(UI remains responsive — chat/stream still drain)",
                    Style::default().fg(Color::DarkGray),
                )),
            ])
            .alignment(Alignment::Center)
            .block(
                Block::default()
                    .title(" Loading ")
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(Color::Yellow)),
            );
            frame.render_widget(p, modal);
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
            "Model: {} | Host: {} | temp={:.2} max={} ctx={}",
            self.active_model_name,
            self.chat.client.endpoint(),
            self.chat.temperature,
            self.chat.max_tokens,
            self.models_view.selected_context,
        );
        let mut spans = vec![
            Span::styled(
                " [F1-F4] Tabs ",
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(" | ", Style::default().fg(Color::DarkGray)),
            Span::styled(active_str, Style::default().fg(Color::White)),
            Span::styled(" | ", Style::default().fg(Color::DarkGray)),
            Span::styled("[?] Help", Style::default().fg(Color::Cyan)),
        ];
        if self.supervisor.is_running_blocking() {
            spans.push(Span::styled(" | ", Style::default().fg(Color::DarkGray)));
            spans.push(Span::styled(
                " [u] Unload ",
                Style::default()
                    .fg(Color::Magenta)
                    .add_modifier(Modifier::BOLD),
            ));
        }
        frame.render_widget(Paragraph::new(Line::from(spans)), area);
    }

    fn render_hot_swap_modal(&self, frame: &mut Frame, area: Rect, intent: &HotSwapIntent) {
        let modal_area = centered_rect(60, 30, area);
        frame.render_widget(Clear, modal_area);
        let target_name = intent
            .path
            .file_name()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_else(|| "new model".to_string());
        let ngl = intent
            .gpu_layers
            .map(|n| n.to_string())
            .unwrap_or_else(|| "default".into());
        let lines = vec![
            Line::from(Span::styled(
                " Model Hot-Swap Confirmation",
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            )),
            Line::from(Span::styled(
                format!("\nActive Model:   {}", self.active_model_name),
                Style::default().fg(Color::White),
            )),
            Line::from(Span::styled(
                format!("Target Model:   {}", target_name),
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            )),
            Line::from(Span::styled(
                format!(
                    "GPU layers (-ngl): {} | Context: {}",
                    ngl, intent.context_size
                ),
                Style::default().fg(Color::Green),
            )),
            Line::from(Span::styled(
                "\nUnload active model and launch new model? [Y / N]",
                Style::default()
                    .fg(Color::Green)
                    .add_modifier(Modifier::BOLD),
            )),
        ];
        frame.render_widget(
            Paragraph::new(lines).alignment(Alignment::Center).block(
                Block::default()
                    .title(" Hot-Swap ")
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(Color::Yellow)),
            ),
            modal_area,
        );
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
            Line::from(Span::styled(
                " Select Target Execution Device ",
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            )),
            Line::from(Span::styled(
                format!(
                    "Model: {} | ctx={} | Choose device:\n",
                    state.model_name, self.models_view.selected_context
                ),
                Style::default().fg(Color::White),
            )),
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
        lines.push(Line::from(Span::styled(
            " [↑ / ↓] Navigate  |  [Enter] Confirm & Launch  |  [Esc] Cancel ",
            Style::default()
                .fg(Color::DarkGray)
                .add_modifier(Modifier::BOLD),
        )));
        frame.render_widget(
            Paragraph::new(lines).block(
                Block::default()
                    .title(" Target Node Selection ")
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(Color::Cyan)),
            ),
            modal_area,
        );
    }

    fn render_help_modal(&self, frame: &mut Frame, area: Rect) {
        let modal = centered_rect(60, 70, area);
        frame.render_widget(Clear, modal);
        let lines: Vec<Line> = keymap::help_lines().into_iter().map(Line::from).collect();
        frame.render_widget(
            Paragraph::new(lines).block(
                Block::default()
                    .title(" Help (?) ")
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(Color::Cyan)),
            ),
            modal,
        );
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

/// Launch and execute the main unified hub event loop.
pub async fn run_hub_tui(mut hub: HubApp) -> Result<(), Box<dyn std::error::Error>> {
    hub.cluster_view.set_local_identity(hub.identity.clone());
    let _registry_runtime = spawn_registry_runtime(
        hub.discovery.clone(),
        hub.identity.clone(),
        hub.discovery.node_uuid(),
    );
    let control_addr = SocketAddr::from(([0, 0, 0, 0], hub.config.network.control_port));
    let control_ctx = Arc::new(
        ControlPlaneContext::new(
            hub.discovery.node_uuid(),
            NodeRole::from_str_role(&hub.config.node.role),
            hub.supervisor.clone(),
            hub.config.network.api_host.clone(),
            hub.config.network.api_port,
            PathBuf::from(&hub.config.node.llama_server_binary),
            hub.identity.clone(),
            hub.shared_config.clone(),
            hub.config_path.clone(),
        )
        .with_discovery(hub.discovery.clone())
        .with_capabilities(vec!["inference".to_string(), "hub".to_string()])
        .with_memory_policy(
            hub.config.hardware.safety.mmap,
            hub.config.hardware.safety.max_ram_usage_percent,
        ),
    );
    let control_handle = spawn_control_plane(control_addr, control_ctx);
    info!(
        "Hub control-plane listening on port {}",
        hub.config.network.control_port
    );

    let (cmd_tx, cmd_rx) = mpsc::channel::<HubCommand>(32);
    let (evt_tx, mut evt_rx) = mpsc::channel::<HubEvent>(64);
    let worker_ctx = HubWorkerCtx {
        config: hub.config.clone(),
        discovery: hub.discovery.clone(),
        supervisor: hub.supervisor.clone(),
        identity: hub.identity.clone(),
    };
    let worker = spawn_hub_worker(cmd_rx, evt_tx, worker_ctx);

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
                if hub.active_tab == HubTab::Models {
                    hub.models_view.refresh_profile();
                    let _ = cmd_tx.try_send(HubCommand::RefreshModelCatalog);
                }
                if hub.active_tab == HubTab::Cluster {
                    let _ = cmd_tx.try_send(HubCommand::RefreshCluster);
                }
                match hub.supervisor.check_status().await {
                    Ok(Some((exit_status, err_lines))) => {
                        let last_err = err_lines
                            .last()
                            .map(|s| s.as_str())
                            .unwrap_or("No stderr output captured")
                            .to_string();
                        let model = std::mem::replace(&mut hub.active_model_name, "None (Idle)".to_string());
                        hub.discovery.set_active_model("").await;
                        hub.discovery.set_status_flags(crate::discovery::StatusFlags(0)).await;
                        hub.apply_event(HubEvent::SupervisorCrashed {
                            model,
                            code: exit_status.code(),
                            stderr: last_err,
                        });
                    }
                    Ok(None) => {}
                    Err(e) => debug!("Failed to check supervisor status: {}", e),
                }
            }
            Some(event) = evt_rx.recv() => {
                let needs_cluster = matches!(event, HubEvent::ClusterRefreshed);
                hub.apply_event(event);
                if needs_cluster {
                    // Short discovery snapshot — not a 30s load.
                    hub.cluster_view.refresh().await;
                }
            }
            Some(event_res) = event_stream.next() => {
                if let Ok(Event::Key(key)) = event_res {
                    if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
                        break;
                    }

                    if hub.show_help {
                        if matches!(key.code, KeyCode::Esc | KeyCode::Char('?') | KeyCode::F(12)) {
                            hub.show_help = false;
                        }
                        continue;
                    }

                    // Download URL modal
                    if hub.pending_download_url.is_some() {
                        match key.code {
                            KeyCode::Esc => {
                                hub.pending_download_url = None;
                            }
                            KeyCode::Enter => {
                                if let Some(url) = hub.pending_download_url.take() {
                                    let url = url.trim().to_string();
                                    if !url.is_empty() {
                                        let _ = cmd_tx.try_send(HubCommand::StartDownload { url });
                                    }
                                }
                            }
                            KeyCode::Backspace => {
                                if let Some(buf) = &mut hub.pending_download_url {
                                    buf.pop();
                                }
                            }
                            KeyCode::Char(c) => {
                                if let Some(buf) = &mut hub.pending_download_url {
                                    buf.push(c);
                                }
                            }
                            _ => {}
                        }
                        continue;
                    }

                    // Push peer picker
                    if let Some(peers) = &hub.pending_push_peers {
                        match key.code {
                            KeyCode::Esc => {
                                hub.pending_push_peers = None;
                            }
                            KeyCode::Up | KeyCode::Char('k') => {
                                if hub.push_peer_idx > 0 {
                                    hub.push_peer_idx -= 1;
                                } else if !peers.is_empty() {
                                    hub.push_peer_idx = peers.len() - 1;
                                }
                            }
                            KeyCode::Down | KeyCode::Char('j') => {
                                if !peers.is_empty() {
                                    hub.push_peer_idx = (hub.push_peer_idx + 1) % peers.len();
                                }
                            }
                            KeyCode::Enter => {
                                if let Some(peers) = hub.pending_push_peers.take() {
                                    if let Some((_, endpoint)) = peers.get(hub.push_peer_idx) {
                                        if let Some(row) = hub.models_view.selected_row() {
                                            if row.local.is_some() && !row.digest.is_empty() {
                                                let source = format!(
                                                    "http://{}:{}",
                                                    hub.config.network.api_host,
                                                    hub.config.network.control_port
                                                );
                                                let source = source.replace("0.0.0.0", "127.0.0.1");
                                                let _ = cmd_tx.try_send(HubCommand::PushModel {
                                                    peer_endpoint: endpoint.clone(),
                                                    digest: row.digest.clone(),
                                                    source_base_url: source,
                                                });
                                            } else {
                                                hub.status_message = Some((
                                                    "Push requires a local model with digest".into(),
                                                    Color::Yellow,
                                                ));
                                            }
                                        }
                                    }
                                }
                            }
                            _ => {}
                        }
                        continue;
                    }

                    // Target Selection modal
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
                                    target_state.selected_idx =
                                        (target_state.selected_idx + 1) % target_state.candidates.len();
                                }
                                continue;
                            }
                            KeyCode::Enter => {
                                if let Some(state) = hub.pending_target_selection.take() {
                                    if let Some(target) = state.candidates.get(state.selected_idx) {
                                        let ctx_size = hub.models_view.selected_context;
                                        match target {
                                            TargetExecutionNode::Local { gpu_layers, .. } => {
                                                hub.chat.set_target_hardware(
                                                    "Local Host",
                                                    format!("Local GPU ({} layers)", gpu_layers),
                                                );
                                                match request_load_or_hot_swap(
                                                    &hub.supervisor,
                                                    state.model_path.clone(),
                                                    Some(*gpu_layers),
                                                    ctx_size,
                                                )
                                                .await
                                                {
                                                    Ok(cmd) => {
                                                        hub.load_phase = Some("Queued local load…".into());
                                                        let _ = cmd_tx.try_send(cmd);
                                                    }
                                                    Err(intent) => {
                                                        hub.pending_hot_swap = Some(intent);
                                                    }
                                                }
                                            }
                                            TargetExecutionNode::LocalCpu { .. } => {
                                                hub.chat.set_target_hardware(
                                                    "Local Host",
                                                    "Local CPU (DotProd / Multi-thread)",
                                                );
                                                match request_load_or_hot_swap(
                                                    &hub.supervisor,
                                                    state.model_path.clone(),
                                                    Some(0),
                                                    ctx_size,
                                                )
                                                .await
                                                {
                                                    Ok(cmd) => {
                                                        hub.load_phase = Some("Queued CPU-safe load…".into());
                                                        let _ = cmd_tx.try_send(cmd);
                                                    }
                                                    Err(intent) => {
                                                        hub.pending_hot_swap = Some(intent);
                                                    }
                                                }
                                            }
                                            TargetExecutionNode::Remote {
                                                endpoint,
                                                api_endpoint,
                                                name,
                                                backend,
                                                ..
                                            } => {
                                                let gpu_layers = if hub.config.hardware.acceleration.prefer_gpu {
                                                    99
                                                } else {
                                                    0
                                                };
                                                hub.load_phase = Some(format!("Queued remote load on {name}…"));
                                                let _ = cmd_tx.try_send(HubCommand::LoadModelRemote {
                                                    endpoint: endpoint.clone(),
                                                    api_endpoint: api_endpoint.clone(),
                                                    name: name.clone(),
                                                    backend: backend.clone(),
                                                    model_name: state.model_name.clone(),
                                                    context_size: ctx_size,
                                                    gpu_layers,
                                                });
                                            }
                                        }
                                    }
                                }
                                continue;
                            }
                            KeyCode::Esc => {
                                hub.pending_target_selection = None;
                                hub.status_message = Some(("Target selection cancelled".into(), Color::DarkGray));
                                continue;
                            }
                            _ => continue,
                        }
                    }

                    // Hot-Swap modal — preserves -ngl via HotSwapIntent
                    if hub.pending_hot_swap.is_some() {
                        if let Some(action) = resolve(KeyScope::HotSwap, key.code, key.modifiers) {
                            match action {
                                HubAction::ConfirmHotSwap => {
                                    if let Some(intent) = hub.pending_hot_swap.take() {
                                        hub.load_phase = Some(format!(
                                            "Hot-swap → -ngl {} …",
                                            intent.gpu_layers.map(|n| n.to_string()).unwrap_or_else(|| "?".into())
                                        ));
                                        let _ = cmd_tx.try_send(HubCommand::LoadModelLocal {
                                            path: intent.path,
                                            gpu_layers: intent.gpu_layers,
                                            context_size: intent.context_size,
                                        });
                                    }
                                }
                                HubAction::CancelHotSwap => {
                                    hub.pending_hot_swap = None;
                                    hub.status_message = Some(("Hot-swap cancelled".into(), Color::DarkGray));
                                }
                                _ => {}
                            }
                        } else if matches!(key.code, KeyCode::Char('Y')) {
                            if let Some(intent) = hub.pending_hot_swap.take() {
                                let _ = cmd_tx.try_send(HubCommand::LoadModelLocal {
                                    path: intent.path,
                                    gpu_layers: intent.gpu_layers,
                                    context_size: intent.context_size,
                                });
                            }
                        } else if matches!(key.code, KeyCode::Char('N')) {
                            hub.pending_hot_swap = None;
                        }
                        continue;
                    }

                    // Global / Models declarative actions
                    if let Some(action) = resolve(KeyScope::Global, key.code, key.modifiers)
                        .or_else(|| {
                            if hub.active_tab == HubTab::Models {
                                resolve(KeyScope::Models, key.code, key.modifiers)
                            } else {
                                None
                            }
                        })
                    {
                        match action {
                            HubAction::Quit => break,
                            HubAction::TabChat => hub.set_tab(HubTab::Chat),
                            HubAction::TabModels => hub.set_tab(HubTab::Models),
                            HubAction::TabCluster => {
                                hub.set_tab(HubTab::Cluster);
                                let _ = cmd_tx.try_send(HubCommand::RefreshCluster);
                            }
                            HubAction::TabSettings => hub.set_tab(HubTab::Settings),
                            HubAction::NextTab => hub.next_tab(),
                            HubAction::PrevTab => hub.previous_tab(),
                            HubAction::Help => hub.show_help = true,
                            HubAction::UnloadModel => {
                                let _ = cmd_tx.try_send(HubCommand::Unload {
                                    active_model_name: hub.active_model_name.clone(),
                                });
                            }
                            HubAction::ModelsNext => hub.models_view.next(),
                            HubAction::ModelsPrev => hub.models_view.previous(),
                            HubAction::ModelsRefresh => hub.models_view.refresh(),
                            HubAction::ModelsEnter => {
                                if let Some(m) = hub.models_view.selected_model() {
                                    let path = m.path.clone();
                                    let _ = cmd_tx.try_send(HubCommand::OpenTargetSelection { model_path: path });
                                } else {
                                    hub.status_message = Some((
                                        "Model not local — press [T] to pull from a peer first".into(),
                                        Color::Yellow,
                                    ));
                                }
                            }
                            HubAction::ModelsContextInc => hub.models_view.adjust_context(1),
                            HubAction::ModelsContextDec => hub.models_view.adjust_context(-1),
                            HubAction::ModelsDownload => {
                                hub.pending_download_url = Some(String::new());
                            }
                            HubAction::ModelsTransfer => {
                                if let Some(row) = hub.models_view.selected_row() {
                                    if row.digest.is_empty() {
                                        hub.status_message = Some((
                                            "Selected model has no digest to transfer".into(),
                                            Color::Yellow,
                                        ));
                                    } else if let Some(endpoint) = row.peer_endpoints.first() {
                                        let _ = cmd_tx.try_send(HubCommand::TransferModel {
                                            peer_endpoint: endpoint.clone(),
                                            digest: row.digest.clone(),
                                        });
                                    } else if row.local.is_some() {
                                        hub.status_message = Some((
                                            "Already local — no remote holder to pull from".into(),
                                            Color::Yellow,
                                        ));
                                    } else {
                                        hub.status_message = Some((
                                            "No peer endpoint advertising this digest".into(),
                                            Color::Yellow,
                                        ));
                                    }
                                }
                            }
                            HubAction::ModelsPush => {
                                if let Some(row) = hub.models_view.selected_row() {
                                    if row.local.is_none() || row.digest.is_empty() {
                                        hub.status_message = Some((
                                            "Push requires a local digested model".into(),
                                            Color::Yellow,
                                        ));
                                    } else {
                                        let list: Vec<(String, String)> = hub
                                            .cluster_view
                                            .peers
                                            .iter()
                                            .map(|p| (p.label(), p.control_endpoint()))
                                            .collect();
                                        if list.is_empty() {
                                            hub.status_message = Some((
                                                "No peers visible — open Cluster tab to refresh".into(),
                                                Color::Yellow,
                                            ));
                                        } else {
                                            hub.push_peer_idx = 0;
                                            hub.pending_push_peers = Some(list);
                                        }
                                    }
                                }
                            }
                            HubAction::ClusterRefresh => {
                                let _ = cmd_tx.try_send(HubCommand::RefreshCluster);
                            }
                            _ => {}
                        }
                        // Models/global actions handled — still allow Chat/Cluster/Settings specifics below
                        if hub.active_tab == HubTab::Models
                            || matches!(
                                action,
                                HubAction::TabChat
                                    | HubAction::TabModels
                                    | HubAction::TabCluster
                                    | HubAction::TabSettings
                                    | HubAction::NextTab
                                    | HubAction::PrevTab
                                    | HubAction::Help
                                    | HubAction::Quit
                            )
                        {
                            continue;
                        }
                    }

                    match hub.active_tab {
                        HubTab::Chat => {
                            if hub.chat.show_preset_modal {
                                hub.chat.handle_key_input(key, &tx);
                            } else if hub.chat.is_streaming && key.code == KeyCode::Esc {
                                hub.chat.abort_generation();
                            } else if key.code == KeyCode::Tab {
                                hub.next_tab();
                            } else if key.code == KeyCode::BackTab {
                                hub.previous_tab();
                            } else if key.modifiers.contains(KeyModifiers::CONTROL)
                                && matches!(key.code, KeyCode::Char('u') | KeyCode::Char('U'))
                            {
                                let _ = cmd_tx.try_send(HubCommand::Unload {
                                    active_model_name: hub.active_model_name.clone(),
                                });
                            } else if key.code == KeyCode::Enter && hub.chat.input_buffer.trim() == "/unload" {
                                hub.chat.input_buffer.clear();
                                hub.chat.cursor_idx = 0;
                                let _ = cmd_tx.try_send(HubCommand::Unload {
                                    active_model_name: hub.active_model_name.clone(),
                                });
                            } else if key.modifiers.contains(KeyModifiers::ALT)
                                && matches!(key.code, KeyCode::Char('c') | KeyCode::Char('C'))
                            {
                                let peers = hub.discovery.get_active_peers().await;
                                if let Some(active_peer) = peers
                                    .iter()
                                    .find(|p| !p.active_model.is_empty() || p.status.is_ready())
                                {
                                    let ep = active_peer.api_endpoint();
                                    hub.discovery.send_direct_probe_to_ip(active_peer.addr.ip()).await;
                                    let model = if !active_peer.active_model.is_empty() {
                                        active_peer.active_model.clone()
                                    } else {
                                        "cluster-model".to_string()
                                    };
                                    hub.chat.client = NexusClient::new(ep.clone());
                                    hub.chat.model_name = model.clone();
                                    hub.active_model_name = model;
                                    let peer_label = active_peer.label();
                                    hub.chat.set_target_hardware(&peer_label, active_peer.backend.to_string());
                                    hub.status_message = Some((
                                        format!("Connected to cluster host at {} (probe sent)", ep),
                                        Color::Green,
                                    ));
                                    hub.chat.messages.push(ChatEntry::notice(format!(
                                        "Connected to active cluster host at {}. Ready for chat.",
                                        ep
                                    )));
                                } else {
                                    hub.status_message =
                                        Some(("No active cluster host found".into(), Color::Yellow));
                                }
                            } else {
                                hub.chat.handle_key_input(key, &tx);
                            }
                        }
                        HubTab::Models => {
                            // Models keys handled via declarative keymap above.
                        }
                        HubTab::Cluster => {
                            if hub.cluster_view.enter_pair_code {
                                match key.code {
                                    KeyCode::Enter => {
                                        let code = hub.cluster_view.pair_code_input.clone();
                                        hub.cluster_view.enter_pair_code = false;
                                        hub.cluster_view.pair_code_input.clear();
                                        match hub.submit_pairing_code(code).await {
                                            Ok(()) => hub.cluster_view.status_message = Some((
                                                "Pairing succeeded".to_string(),
                                                Color::Green,
                                            )),
                                            Err(e) => hub.cluster_view.status_message = Some((
                                                format!("Pairing failed: {e}"),
                                                Color::Red,
                                            )),
                                        }
                                    }
                                    KeyCode::Esc => {
                                        hub.cluster_view.enter_pair_code = false;
                                        hub.cluster_view.pair_code_input.clear();
                                    }
                                    KeyCode::Backspace => {
                                        hub.cluster_view.pair_code_input.pop();
                                    }
                                    KeyCode::Char(c) if c.is_ascii_digit()
                                        && hub.cluster_view.pair_code_input.len() < 6 =>
                                    {
                                        hub.cluster_view.pair_code_input.push(c);
                                    }
                                    _ => {}
                                }
                                continue;
                            } else if hub.cluster_view.adding_peer {
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
                                            let _ = cmd_tx.try_send(HubCommand::RefreshCluster);
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
                            } else if hub.cluster_view.show_pair_code {
                                match key.code {
                                    KeyCode::Esc | KeyCode::Char('p') | KeyCode::Char('P') => {
                                        hub.cluster_view.show_pair_code = false;
                                    }
                                    _ => {}
                                }
                            } else {
                                match key.code {
                                    KeyCode::Char('1') => hub.set_tab(HubTab::Chat),
                                    KeyCode::Char('2') => hub.set_tab(HubTab::Models),
                                    KeyCode::Char('3') => hub.cluster_view.refresh().await,
                                    KeyCode::Char('4') => hub.set_tab(HubTab::Settings),
                                    KeyCode::Up | KeyCode::Char('k') => hub.cluster_view.previous(),
                                    KeyCode::Down | KeyCode::Char('j') => hub.cluster_view.next(),
                                    KeyCode::Enter => {
                                        if let Some(peer) = hub.cluster_view.selected_peer() {
                                            if !hub
                                                .discovery
                                                .is_peer_trusted_for_routing(peer.uuid)
                                                .await
                                            {
                                                hub.cluster_view.status_message = Some((
                                                    "Peer is not paired/verified — use [O] enter pairing code"
                                                        .to_string(),
                                                    Color::Yellow,
                                                ));
                                                continue;
                                            }
                                            let ep = peer.api_endpoint();
                                            hub.discovery.send_direct_probe_to_ip(peer.addr.ip()).await;
                                            let model = if !peer.active_model.is_empty() {
                                                peer.active_model.clone()
                                            } else {
                                                "cluster-peer".to_string()
                                            };
                                            hub.chat.client = NexusClient::new(ep.clone());
                                            hub.chat.model_name = model.clone();
                                            hub.active_model_name = model.clone();
                                            let peer_name = peer.label();
                                            hub.chat.set_target_hardware(&peer_name, peer.backend.to_string());
                                            hub.status_message = Some((format!("Connected to peer at {} (probe sent)", ep), Color::Green));
                                            hub.chat.messages.push(crate::ui::chat::ChatEntry::notice(format!(
                                                "Connected to remote peer '{}' at {}. Ready for chat.",
                                                peer_name, ep
                                            )));

                                            hub.set_tab(HubTab::Chat);
                                        } else {
                                            hub.cluster_view.status_message = Some(("No peer selected to connect".to_string(), Color::Yellow));
                                        }
                                    }
                                    KeyCode::Char('p') | KeyCode::Char('P') => {
                                        hub.cluster_view.show_pair_code = true;
                                    }
                                    KeyCode::Char('o') | KeyCode::Char('O') => {
                                        hub.cluster_view.enter_pair_code = true;
                                        hub.cluster_view.pair_code_input.clear();
                                        hub.cluster_view.status_message = Some((
                                            "Enter 6-digit pairing code from peer | [Enter] submit | [Esc] cancel"
                                                .to_string(),
                                            Color::Yellow,
                                        ));
                                    }
                                    KeyCode::Char('l') | KeyCode::Char('L') => {
                                        if let Some(peer) = hub.cluster_view.selected_peer() {
                                            if !hub
                                                .discovery
                                                .is_peer_trusted_for_routing(peer.uuid)
                                                .await
                                            {
                                                hub.cluster_view.status_message = Some((
                                                    "Cannot load on unpaired/unverified peer".to_string(),
                                                    Color::Red,
                                                ));
                                                continue;
                                            }
                                            let peer_name = peer.label();
                                            let peer_ctrl = peer.control_endpoint();
                                            let peer_api = peer.api_endpoint();
                                            let peer_backend = peer.backend.to_string();
                                            if let Some(m) = hub.models_view.selected_model() {
                                                let model_name = m.filename.clone();
                                                let gpu_layers = if hub.config.hardware.acceleration.prefer_gpu { 99 } else { 0 };
                                                hub.load_phase = Some(format!("Queued remote load on {peer_name}…"));
                                                let _ = cmd_tx.try_send(HubCommand::LoadModelRemote {
                                                    endpoint: peer_ctrl,
                                                    api_endpoint: peer_api,
                                                    name: peer_name,
                                                    backend: peer_backend,
                                                    model_name,
                                                    context_size: hub.models_view.selected_context,
                                                    gpu_layers,
                                                });
                                            } else {
                                                hub.cluster_view.status_message = Some(("Select a model in [F2] Models first".to_string(), Color::Yellow));
                                            }
                                        }
                                    }
                                    KeyCode::Char('w') | KeyCode::Char('W') => {
                                        if let Some(peer) = hub.cluster_view.selected_peer() {
                                            let ep = peer.api_endpoint();
                                            let peer_label = peer.label();
                                            hub.cluster_view.status_message = Some((
                                                format!("Requested {} at {} to stand by for RPC worker offload", peer_label, ep),
                                                Color::Cyan,
                                            ));
                                        }
                                    }
                                    KeyCode::Char('i') | KeyCode::Char('I') => hub.cluster_view.toggle_info_modal(),
                                    KeyCode::Char('a') | KeyCode::Char('A') => hub.cluster_view.start_add_peer(),
                                    KeyCode::Char('d') | KeyCode::Char('D') => {
                                        let local_ep = format!("http://127.0.0.1:{}", hub.config.network.api_port);
                                        hub.chat.client = NexusClient::new(local_ep.clone());
                                        hub.chat.set_target_hardware("Local Host", "Local CPU/GPU");
                                        hub.status_message = Some(("Reset chat target to local node".to_string(), Color::Green));
                                        hub.chat.messages.push(crate::ui::chat::ChatEntry::notice(format!("Disconnected from peer. Reverted to local endpoint: {}", local_ep)));

                                    }
                                    KeyCode::Char('u') | KeyCode::Char('U') => {
                                        let _ = cmd_tx.try_send(HubCommand::Unload {
                                            active_model_name: hub.active_model_name.clone(),
                                        });
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
                                    KeyCode::Char('1') => hub.set_tab(HubTab::Chat),
                                    KeyCode::Char('2') => hub.set_tab(HubTab::Models),
                                    KeyCode::Char('3') => {
                                        hub.set_tab(HubTab::Cluster);
                                        let _ = cmd_tx.try_send(HubCommand::RefreshCluster);
                                    }
                                    KeyCode::Char('4') => {}
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
                                            hub.discovery.set_mdns_enabled(hub.config.network.discovery.mdns.enabled).await;
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
                    StreamMsg::Abort => {
                        hub.chat.finalize_stream();
                        hub.chat.status_message = Some("Generation aborted by operator".to_string());
                        hub.chat.auto_scroll = true;
                    }
                    StreamMsg::Error(err) => {
                        hub.chat.finalize_stream();
                        let hint = if err.contains("Transport Error") || err.contains("error sending request") {
                            "\n💡 Hint: If running on mobile GPU (Vulkan), try unloading ('u') and loading via 'Local (CPU Mode)' in [F2] Models to bypass mobile GPU driver freezes."
                        } else {
                            ""
                        };
                        hub.chat.messages.push(ChatEntry::error(format!(
                            "⚠️ [Connection / Generation Error]: {}{}",
                            err, hint
                        )));
                        hub.chat.status_message = Some(format!("Error: {}", err));
                        hub.chat.auto_scroll = true;
                    }
                }
            }
        }
    }

    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;

    control_handle.abort();
    worker.abort();
    if hub.supervisor.is_running().await {
        info!("Stopping active supervisor process on hub exit...");
        let _ = hub.supervisor.stop().await;
    }

    Ok(())
}
