//! Unified Nexus Hub TUI (Phase 8 — responsive command/event architecture).

pub mod commands;
pub mod keymap;

use crate::client::NexusClient;
use crate::config::NexusConfig;
use crate::control_plane::{dispatch_pair, PairRequest, CONTROL_PLANE_VERSION};
use crate::control_plane_server::{spawn as spawn_control_plane, ControlPlaneContext};
use crate::discovery::{DiscoveryService, NodeRole};
use crate::gateway::{spawn as spawn_gateway, GatewayContext};
use crate::node_identity::NodeIdentity;
use crate::registry_runtime::spawn_registry_runtime;
use crate::supervisor::SupervisorManager;
use crate::ui::chat::{ChatApp, ChatEntry, StreamMsg};
use crate::ui::cluster_view::ClusterView;
use crate::ui::models_view::ModelsView;
use crate::ui::settings_view::SettingsView;
use commands::{
    request_load_or_hot_swap, request_load_or_hot_swap_with_args, spawn_hub_worker, HubCommand,
    HubEvent, HubWorkerCtx,
};
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
    widgets::{Block, Borders, Clear, Gauge, List, ListItem, Paragraph, Tabs},
    Frame, Terminal,
};
use std::io::stdout;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::mpsc;
use tracing::{debug, info};
use uuid::Uuid;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HubTab {
    Chat = 0,
    Models = 1,
    Cluster = 2,
    Settings = 3,
    Tunnel = 4,
    Agents = 5,
    Logs = 6,
}

impl HubTab {
    pub fn title(&self) -> &'static str {
        match self {
            Self::Chat => " [F1] 💬 Chat ",
            Self::Models => " [F2] 📦 Models ",
            Self::Cluster => " [F3] 🌐 Cluster ",
            Self::Settings => " [F4] ⚙️ Settings ",
            Self::Tunnel => " [F5] 🚇 Tunnel ",
            Self::Agents => " [F6] 🤖 Agents ",
            Self::Logs => " [F7] 📜 Logs ",
        }
    }
}

/// Target action for Command Palette execution.
#[derive(Debug, Clone)]
pub enum PaletteAction {
    SwitchTab(HubTab),
    UnloadModel,
    OpenHelp,
    RefreshCluster,
    RefreshModels,
    DownloadModel,
    ToggleLogTail,
    CycleLogFilter,
    ClearLogs,
    ChatSlashCommand(String),
    SelectModel(PathBuf),
    ConnectPeer(String, String),
}

/// Dynamic entry indexed by the Command Palette fuzzy finder.
#[derive(Debug, Clone)]
pub struct PaletteItem {
    pub label: String,
    pub description: String,
    pub category: &'static str,
    pub action: PaletteAction,
}

/// Target execution node option for running model weights.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TargetExecutionNode {
    Local {
        allocatable_mb: u64,
        backend: String,
        gpu_layers: u32,
        predicted_label: String,
    },
    LocalCpu {
        allocatable_mb: u64,
        backend: String,
        threads: usize,
        predicted_label: String,
    },
    Remote {
        uuid: Uuid,
        name: String,
        endpoint: String,
        api_endpoint: String,
        free_ram_mb: u32,
        backend: String,
        predicted_label: String,
    },
    Distributed {
        worker_names: Vec<String>,
        predicted_label: String,
        rpc_endpoints: Vec<String>,
        extra_args: Vec<String>,
        gpu_layers: u32,
    },
}

impl TargetExecutionNode {
    pub fn display_label(&self) -> String {
        match self {
            Self::Local {
                allocatable_mb,
                backend,
                gpu_layers,
                predicted_label,
            } => {
                format!(
                    "⚡ Local GPU ({} layers) - {} MB | {} | {}",
                    gpu_layers, allocatable_mb, backend, predicted_label
                )
            }
            Self::LocalCpu {
                allocatable_mb,
                backend,
                threads,
                predicted_label,
            } => {
                format!(
                    "🛡️  Local CPU (0 GPU layers, {} threads) - {} MB | {} | {}",
                    threads, allocatable_mb, backend, predicted_label
                )
            }
            Self::Distributed {
                worker_names,
                predicted_label,
                ..
            } => {
                format!(
                    "🔗 Distributed RPC [{}] | {}",
                    worker_names.join(", "),
                    predicted_label
                )
            }
            Self::Remote {
                name,
                endpoint,
                free_ram_mb,
                backend,
                predicted_label,
                ..
            } => {
                format!(
                    "📱 {} ({}) - {} MB free | {} | {}",
                    name, endpoint, free_ram_mb, backend, predicted_label
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
    pub extra_args: Vec<String>,
}

/// State for Hugging Face quant selection modal (Phase 3).
#[derive(Debug, Clone)]
pub struct HfQuantPickerState {
    pub repo_id: String,
    pub groups: Vec<crate::hf::HfGgufGroup>,
    pub selected_idx: usize,
}

/// State for inline Hugging Face token entry & recovery modal (Phase 3).
#[derive(Debug, Clone)]
pub struct HfAuthRecoveryState {
    pub repo_id: String,
    pub retry_download_url: Option<String>,
    pub retry_expected_sha: Option<String>,
    pub input_token: String,
}

pub struct HubApp {
    pub config: NexusConfig,
    pub discovery: Arc<DiscoveryService>,
    pub active_tab: HubTab,
    pub chat: ChatApp,
    pub models_view: ModelsView,
    pub cluster_view: ClusterView,
    pub settings_view: SettingsView,
    pub tunnel_view: crate::ui::tunnel_view::TunnelView,
    pub agents_view: crate::ui::agents_view::AgentsView,
    pub logs_view: crate::ui::logs_view::LogsView,
    pub task_store: Arc<crate::task::TaskStore>,
    pub kb_store: Arc<crate::kb::KnowledgeStore>,
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
    pub show_command_palette: bool,
    pub palette_input: String,
    pub palette_selected_idx: usize,
    /// URL input modal for Models [D].
    pub pending_download_url: Option<String>,
    /// Peer picker for [S] push (list of (label, endpoint)).
    pub pending_push_peers: Option<Vec<(String, String)>>,
    pub push_peer_idx: usize,
    // (label, percent, downloaded, total, speed) — keep compact until a dedicated progress type.
    #[allow(clippy::type_complexity)]
    pub download_progress: Option<(String, Option<f32>, u64, Option<u64>, f64)>,
    /// Pending model deletion: (model_entry, shard_paths, total_bytes).
    #[allow(clippy::type_complexity)]
    pub pending_delete_model: Option<(crate::ui::models::ModelEntry, Vec<std::path::PathBuf>, u64)>,
    /// Hugging Face GGUF quantization picker modal.
    pub pending_hf_quant_picker: Option<HfQuantPickerState>,
    /// Inline Hugging Face authentication token recovery modal.
    pub pending_hf_auth_recovery: Option<HfAuthRecoveryState>,
    pub layout_mode: crate::ui::layout::LayoutMode,
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
        let layout_mode = crate::ui::layout::LayoutMode::from_str_mode(&config.ui.layout_mode);
        let mut models_view = ModelsView::new(config.node.models_dir.clone());
        models_view.layout_mode = layout_mode;
        let mut cluster_view = ClusterView::new(discovery.clone());
        cluster_view.layout_mode = layout_mode;
        let settings_view = SettingsView::new(config.clone());
        let mut chat = ChatApp::new(client, "default", None);
        chat.layout_mode = layout_mode;
        let tunnel_view = crate::ui::tunnel_view::TunnelView::new(
            config.network.api_port,
            crate::tunnel::DEFAULT_RPC_PORT,
        );
        let task_store = Arc::new(
            crate::task::TaskStore::load_or_create(crate::task::TaskStore::default_path())
                .unwrap_or_else(|_| {
                    crate::task::TaskStore::load_or_create(
                        std::env::temp_dir().join("nexus_tasks.json"),
                    )
                    .expect("fallback task store")
                }),
        );
        let kb_store = Arc::new(
            crate::kb::KnowledgeStore::open(crate::kb::KnowledgeStore::default_path())
                .or_else(|_| {
                    let unique_name = format!("nexus_kb_{}.redb", Uuid::new_v4());
                    crate::kb::KnowledgeStore::open(std::env::temp_dir().join(unique_name))
                })
                .expect("fallback kb store"),
        );
        let mut agents_view =
            crate::ui::agents_view::AgentsView::new(task_store.clone(), kb_store.clone());
        agents_view.layout_mode = layout_mode;
        let logs_view = crate::ui::logs_view::LogsView::new();

        Self {
            config,
            discovery,
            active_tab: HubTab::Chat,
            chat,
            models_view,
            cluster_view,
            settings_view,
            tunnel_view,
            agents_view,
            logs_view,
            task_store,
            kb_store,
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
            show_command_palette: false,
            palette_input: String::new(),
            palette_selected_idx: 0,
            pending_download_url: None,
            pending_push_peers: None,
            push_peer_idx: 0,
            download_progress: None,
            pending_delete_model: None,
            pending_hf_quant_picker: None,
            pending_hf_auth_recovery: None,
            layout_mode,
        }
    }

    pub fn sync_layout_mode(&mut self) {
        let mode =
            crate::ui::layout::LayoutMode::from_str_mode(&self.settings_view.config.ui.layout_mode);
        self.layout_mode = mode;
        self.chat.layout_mode = mode;
        self.models_view.layout_mode = mode;
        self.cluster_view.layout_mode = mode;
        self.agents_view.layout_mode = mode;
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
        } else if tab == HubTab::Tunnel {
            self.tunnel_view.refresh();
        } else if tab == HubTab::Agents {
            self.agents_view.refresh();
        } else if tab == HubTab::Logs {
            self.logs_view.refresh();
        }
    }

    pub fn next_tab(&mut self) {
        let next_idx = ((self.active_tab as usize) + 1) % 7;
        self.set_tab(match next_idx {
            0 => HubTab::Chat,
            1 => HubTab::Models,
            2 => HubTab::Cluster,
            3 => HubTab::Settings,
            4 => HubTab::Tunnel,
            5 => HubTab::Agents,
            _ => HubTab::Logs,
        });
    }

    pub fn previous_tab(&mut self) {
        let prev_idx = if (self.active_tab as usize) == 0 {
            6
        } else {
            (self.active_tab as usize) - 1
        };
        self.set_tab(match prev_idx {
            0 => HubTab::Chat,
            1 => HubTab::Models,
            2 => HubTab::Cluster,
            3 => HubTab::Settings,
            4 => HubTab::Tunnel,
            5 => HubTab::Agents,
            _ => HubTab::Logs,
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
            HubEvent::DownloadCancelled { message } => {
                self.download_progress = None;
                self.models_view.refresh();
                self.status_message = Some((message, Color::Yellow));
            }
            HubEvent::ModelDeleted { filename } => {
                self.models_view.refresh();
                self.status_message = Some((format!("Deleted model {filename}"), Color::Green));
            }
            HubEvent::ModelCatalogUpdated { remotes } => {
                self.models_view.apply_remote_catalogs(&remotes);
            }
            HubEvent::HfRepoResolved { repo_id, groups } => {
                self.status_message = None;
                self.pending_hf_quant_picker = Some(HfQuantPickerState {
                    repo_id,
                    groups,
                    selected_idx: 0,
                });
            }
            HubEvent::HfAuthRequired {
                repo_id,
                retry_download_url,
                retry_expected_sha,
            } => {
                self.download_progress = None;
                self.pending_hf_auth_recovery = Some(HfAuthRecoveryState {
                    repo_id,
                    retry_download_url,
                    retry_expected_sha,
                    input_token: String::new(),
                });
            }
            HubEvent::HfError { message } => {
                self.models_view.hf_loading = false;
                self.status_message = Some((message, Color::Red));
            }
            HubEvent::HfModelsLoaded { models } => {
                let count = models.len();
                self.models_view.set_hf_models(models);
                self.status_message = Some((
                    format!("Loaded {} Hugging Face models", count),
                    Color::Green,
                ));
            }
        }
    }

    pub fn render(&mut self, frame: &mut Frame) {
        self.sync_layout_mode();
        let area = frame.area();
        let is_compact = self.layout_mode.is_compact(area);

        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(3),
                Constraint::Min(10),
                Constraint::Length(2),
            ])
            .split(area);

        self.render_top_tabs(frame, chunks[0], is_compact);

        match self.active_tab {
            HubTab::Chat => self.chat.render_in_area(frame, chunks[1]),
            HubTab::Models => self.models_view.render(frame, chunks[1]),
            HubTab::Cluster => self.cluster_view.render(frame, chunks[1]),
            HubTab::Settings => self.settings_view.render(frame, chunks[1]),
            HubTab::Tunnel => self.tunnel_view.render(frame, chunks[1]),
            HubTab::Agents => self.agents_view.render(frame, chunks[1]),
            HubTab::Logs => self.logs_view.render(frame, chunks[1]),
        }

        self.render_footer(frame, chunks[2], is_compact);

        if let Some(intent) = &self.pending_hot_swap {
            self.render_hot_swap_modal(frame, area, intent);
        }

        if let Some(target_state) = &self.pending_target_selection {
            self.render_target_selection_modal(frame, area, target_state);
        }

        if self.show_help {
            self.render_help_modal(frame, area);
        }

        if self.show_command_palette {
            self.render_command_palette(frame, area);
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
            let hint = Paragraph::new(Line::from(Span::styled(
                " [Esc] / [C] Cancel download (saves partial progress) ",
                Style::default().fg(Color::DarkGray),
            )))
            .alignment(Alignment::Center);
            frame.render_widget(hint, chunks[2]);
        }

        if let Some((entry, shards, total_bytes)) = &self.pending_delete_model {
            let modal = centered_rect(60, 24, area);
            frame.render_widget(Clear, modal);
            let size_str = if *total_bytes >= 1024 * 1024 * 1024 {
                format!("{:.2} GB", *total_bytes as f64 / (1024.0 * 1024.0 * 1024.0))
            } else {
                format!("{} MB", *total_bytes / (1024 * 1024))
            };
            let shard_info = if shards.len() > 1 {
                format!(" ({} shards, {})", shards.len(), size_str)
            } else {
                format!(" ({})", size_str)
            };
            let p = Paragraph::new(vec![
                Line::from(""),
                Line::from(Span::styled(
                    " Permanently delete this model from disk? ",
                    Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
                )),
                Line::from(""),
                Line::from(vec![
                    Span::styled(
                        &entry.filename,
                        Style::default()
                            .fg(Color::White)
                            .add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(shard_info, Style::default().fg(Color::Yellow)),
                ]),
                Line::from(""),
                Line::from(Span::styled(
                    " [Y] / [Enter] Confirm Delete    [N] / [Esc] Cancel ",
                    Style::default().fg(Color::Yellow),
                )),
            ])
            .alignment(Alignment::Center)
            .block(
                Block::default()
                    .title(" Delete Model ")
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(Color::Red)),
            );
            frame.render_widget(p, modal);
        }

        if let Some(url) = &self.pending_download_url {
            let modal = centered_rect(72, 20, area);
            frame.render_widget(Clear, modal);
            let p = Paragraph::new(vec![
                Line::from(Span::styled(
                    " Enter GGUF URL or Hugging Face repo (e.g. bartowski/Llama-3.2-3B-Instruct-GGUF) ",
                    Style::default().fg(Color::Yellow),
                )),
                Line::from(""),
                Line::from(Span::styled(url.clone(), Style::default().fg(Color::White))),
            ])
            .block(
                Block::default()
                    .title(" Download / HF Resolve ")
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(Color::Cyan)),
            );
            frame.render_widget(p, modal);
        }

        if let Some(picker) = &self.pending_hf_quant_picker {
            let modal = centered_rect(82, 65, area);
            frame.render_widget(Clear, modal);
            let chunks = Layout::default()
                .direction(Direction::Vertical)
                .constraints([
                    Constraint::Length(3),
                    Constraint::Min(4),
                    Constraint::Length(3),
                ])
                .margin(1)
                .split(modal);

            let header = Paragraph::new(Line::from(vec![
                Span::styled(" Repository: ", Style::default().fg(Color::LightBlue)),
                Span::styled(
                    &picker.repo_id,
                    Style::default()
                        .fg(Color::White)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    format!(" ({} quant options)", picker.groups.len()),
                    Style::default().fg(Color::DarkGray),
                ),
            ]))
            .block(
                Block::default()
                    .title(" Hugging Face Quantization Picker ")
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(Color::Cyan)),
            );
            frame.render_widget(header, modal);

            let items: Vec<ListItem> = picker
                .groups
                .iter()
                .enumerate()
                .map(|(i, group)| {
                    let is_sel = i == picker.selected_idx;
                    let size_str = if group.total_size_bytes >= 1024 * 1024 * 1024 {
                        format!(
                            "{:.2} GB",
                            group.total_size_bytes as f64 / (1024.0 * 1024.0 * 1024.0)
                        )
                    } else {
                        format!("{} MB", group.total_size_bytes / (1024 * 1024))
                    };
                    let shard_str = if group.is_sharded {
                        format!(" ({} shards)", group.files.len())
                    } else {
                        "".to_string()
                    };
                    let (prefix, row_style) = if is_sel {
                        (
                            "> ",
                            Style::default()
                                .fg(Color::Yellow)
                                .add_modifier(Modifier::BOLD),
                        )
                    } else {
                        ("  ", Style::default().fg(Color::White))
                    };

                    ListItem::new(Line::from(vec![
                        Span::styled(prefix, row_style),
                        Span::styled(
                            format!("{:<10}", group.quant_label),
                            Style::default()
                                .fg(Color::Cyan)
                                .add_modifier(Modifier::BOLD),
                        ),
                        Span::styled(
                            format!(" {:<16}", format!("{size_str}{shard_str}")),
                            Style::default().fg(Color::White),
                        ),
                        Span::styled(
                            format!(" {:<14}", group.fit_status.badge_text()),
                            Style::default()
                                .fg(group.fit_status.color())
                                .add_modifier(Modifier::BOLD),
                        ),
                        Span::styled(
                            format!(" {}", group.base_name),
                            Style::default().fg(Color::DarkGray),
                        ),
                    ]))
                })
                .collect();

            let list = List::new(items).block(
                Block::default()
                    .title(" Quantizations ")
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(Color::DarkGray)),
            );
            frame.render_widget(list, chunks[1]);

            let footer = Paragraph::new(Line::from(vec![Span::styled(
                " [↑/k] [↓/j] Select Quant    [Enter] Download    [Esc] Cancel ",
                Style::default().fg(Color::Yellow),
            )]))
            .alignment(Alignment::Center);
            frame.render_widget(footer, chunks[2]);
        }

        if let Some(recovery) = &self.pending_hf_auth_recovery {
            let modal = centered_rect(70, 40, area);
            frame.render_widget(Clear, modal);
            let masked_token = if recovery.input_token.is_empty() {
                "Paste HF Access Token here (hf_••••••••)...".to_string()
            } else {
                "•".repeat(recovery.input_token.len())
            };
            let p = Paragraph::new(vec![
                Line::from(""),
                Line::from(Span::styled(
                    " 🔐 Hugging Face Access Token Required ",
                    Style::default()
                        .fg(Color::Yellow)
                        .add_modifier(Modifier::BOLD),
                )),
                Line::from(""),
                Line::from(vec![
                    Span::styled(" Model repository '", Style::default().fg(Color::DarkGray)),
                    Span::styled(
                        &recovery.repo_id,
                        Style::default()
                            .fg(Color::White)
                            .add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(
                        "' is gated or requires authentication.",
                        Style::default().fg(Color::DarkGray),
                    ),
                ]),
                Line::from(""),
                Line::from(Span::styled(
                    format!(" [ {} ]", masked_token),
                    Style::default().fg(if recovery.input_token.is_empty() {
                        Color::DarkGray
                    } else {
                        Color::Green
                    }),
                )),
                Line::from(""),
                Line::from(Span::styled(
                    " [Enter] Save Token & Retry    [Esc] Cancel ",
                    Style::default().fg(Color::Cyan),
                )),
            ])
            .alignment(Alignment::Center)
            .block(
                Block::default()
                    .title(" Authentication Required ")
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(Color::Yellow)),
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

    fn render_top_tabs(&self, frame: &mut Frame, area: Rect, is_compact: bool) {
        let titles = if is_compact {
            vec![
                "1:Chat", "2:Mod", "3:Clus", "4:Set", "5:Tun", "6:Agnt", "7:Log",
            ]
        } else {
            vec![
                HubTab::Chat.title(),
                HubTab::Models.title(),
                HubTab::Cluster.title(),
                HubTab::Settings.title(),
                HubTab::Tunnel.title(),
                HubTab::Agents.title(),
                HubTab::Logs.title(),
            ]
        };
        let selected = self.active_tab as usize;
        let block = if is_compact {
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::DarkGray))
        } else {
            Block::default()
                .title(" Nexus-LLM Unified Hub ")
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::DarkGray))
        };
        let tabs = Tabs::new(titles)
            .select(selected)
            .block(block)
            .style(Style::default().fg(Color::Gray))
            .highlight_style(
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD)
                    .bg(Color::Rgb(20, 30, 40)),
            );
        frame.render_widget(tabs, area);
    }

    fn render_footer(&self, frame: &mut Frame, area: Rect, is_compact: bool) {
        // Line 1: Rich Telemetry Dock (ORCHESTRATOR_PLAN.md §3.2)
        let prompt_tokens: usize = self
            .chat
            .messages
            .iter()
            .map(|m| (m.message.content.len() / 4).max(1))
            .sum();
        let used_tokens = prompt_tokens + self.chat.tokens_streamed;
        let max_ctx = self.models_view.selected_context.max(512);
        let ratio = ((used_tokens as f64) / (max_ctx as f64)).clamp(0.0, 1.0);
        let pct = (ratio * 100.0) as usize;
        let filled_bars = ((ratio * 10.0).round() as usize).min(10);
        let gauge_str = format!(
            "[{}{}] {}/{} ({}%)",
            "█".repeat(filled_bars),
            "░".repeat(10 - filled_bars),
            used_tokens,
            max_ctx,
            pct
        );

        let speed_str = if self.chat.is_streaming || self.chat.tokens_per_sec > 0.0 {
            format!("⚡ {:.1} t/s", self.chat.tokens_per_sec)
        } else {
            "Idle".to_string()
        };

        let status_badge = if self.chat.is_streaming {
            crate::ui::badges::BADGE_STREAMING.span()
        } else if self.supervisor.is_running_blocking() {
            crate::ui::badges::BADGE_READY.span()
        } else {
            crate::ui::badges::BADGE_OK.span()
        };

        let model_label =
            if self.active_model_name.is_empty() || self.active_model_name == "None (Idle)" {
                "None (Idle)".to_string()
            } else {
                self.active_model_name.clone()
            };

        let line1 = if is_compact {
            let short_model = if model_label.len() > 16 {
                format!("{}…", &model_label[..15])
            } else {
                model_label
            };
            Line::from(vec![
                Span::styled(" ", Style::default()),
                Span::styled(
                    short_model,
                    Style::default()
                        .fg(Color::Yellow)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(" | ", Style::default().fg(Color::DarkGray)),
                Span::styled(
                    speed_str,
                    Style::default()
                        .fg(Color::Green)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(" | Ctx: ", Style::default().fg(Color::LightBlue)),
                Span::styled(
                    format!("{}%", pct),
                    Style::default()
                        .fg(if pct > 85 {
                            Color::Red
                        } else if pct > 65 {
                            Color::Yellow
                        } else {
                            Color::Green
                        })
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(" | ", Style::default().fg(Color::DarkGray)),
                status_badge,
            ])
        } else {
            Line::from(vec![
                Span::styled(
                    " Model: ",
                    Style::default()
                        .fg(Color::Yellow)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    model_label,
                    Style::default()
                        .fg(Color::White)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(" | ", Style::default().fg(Color::DarkGray)),
                Span::styled("Host: ", Style::default().fg(Color::Cyan)),
                Span::styled(
                    self.chat.client.endpoint(),
                    Style::default().fg(Color::White),
                ),
                Span::styled(" | ", Style::default().fg(Color::DarkGray)),
                Span::styled(
                    speed_str,
                    Style::default()
                        .fg(Color::Green)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(" | ", Style::default().fg(Color::DarkGray)),
                Span::styled("Context: ", Style::default().fg(Color::LightBlue)),
                Span::styled(
                    gauge_str,
                    Style::default()
                        .fg(if pct > 85 {
                            Color::Red
                        } else if pct > 65 {
                            Color::Yellow
                        } else {
                            Color::Green
                        })
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(" | ", Style::default().fg(Color::DarkGray)),
                status_badge,
            ])
        };

        // Line 2: Global Navigation, Hotkeys & Transient Alerts
        let mut spans2 = if is_compact {
            vec![
                Span::styled(
                    " [Tab] Next ",
                    Style::default()
                        .fg(Color::Yellow)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    " [^P] Cmd ",
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(" [?] Help ", Style::default().fg(Color::White)),
            ]
        } else {
            vec![
                Span::styled(
                    " [F1-F7] Tabs ",
                    Style::default()
                        .fg(Color::Yellow)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    " [Ctrl+P] Palette ",
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(" [?] Help ", Style::default().fg(Color::White)),
            ]
        };

        if self.supervisor.is_running_blocking() {
            spans2.push(Span::styled(" | ", Style::default().fg(Color::DarkGray)));
            spans2.push(Span::styled(
                " [u] Unload ",
                Style::default()
                    .fg(Color::Magenta)
                    .add_modifier(Modifier::BOLD),
            ));
        }

        if let Some((msg, color)) = &self.status_message {
            spans2.push(Span::styled(" | ", Style::default().fg(Color::DarkGray)));
            spans2.push(Span::styled(
                format!("ALERT: {}", msg),
                Style::default().fg(*color).add_modifier(Modifier::BOLD),
            ));
        }

        frame.render_widget(Paragraph::new(vec![line1, Line::from(spans2)]), area);
    }

    pub fn build_palette_items(&self) -> Vec<PaletteItem> {
        let mut items = Vec::new();
        // Navigation
        items.push(PaletteItem {
            label: "Go to Chat".into(),
            description: "Open interactive chat session [F1]".into(),
            category: "Navigation",
            action: PaletteAction::SwitchTab(HubTab::Chat),
        });
        items.push(PaletteItem {
            label: "Go to Models".into(),
            description: "Browse local models and peer weight transfers [F2]".into(),
            category: "Navigation",
            action: PaletteAction::SwitchTab(HubTab::Models),
        });
        items.push(PaletteItem {
            label: "Go to Cluster".into(),
            description: "View mesh topology, discovery, and pairing [F3]".into(),
            category: "Navigation",
            action: PaletteAction::SwitchTab(HubTab::Cluster),
        });
        items.push(PaletteItem {
            label: "Go to Settings".into(),
            description: "Configure ports, hardware limits, and safety [F4]".into(),
            category: "Navigation",
            action: PaletteAction::SwitchTab(HubTab::Settings),
        });
        items.push(PaletteItem {
            label: "Go to Tunnel".into(),
            description: "ADB and reverse port forwarding manager [F5]".into(),
            category: "Navigation",
            action: PaletteAction::SwitchTab(HubTab::Tunnel),
        });
        items.push(PaletteItem {
            label: "Go to Agents".into(),
            description: "Agent bus tasks, episodic memory, and routing [F6]".into(),
            category: "Navigation",
            action: PaletteAction::SwitchTab(HubTab::Agents),
        });
        items.push(PaletteItem {
            label: "Go to Logs".into(),
            description: "Live diagnostic node and daemon log stream [F7]".into(),
            category: "Navigation",
            action: PaletteAction::SwitchTab(HubTab::Logs),
        });

        // Hub Actions
        items.push(PaletteItem {
            label: "Unload Active Model".into(),
            description: format!("Unload running weights ({}) [u]", self.active_model_name),
            category: "Action",
            action: PaletteAction::UnloadModel,
        });
        items.push(PaletteItem {
            label: "Scan & Refresh Models".into(),
            description: "Re-scan ~/.nexus/models/ for GGUF files [r]".into(),
            category: "Action",
            action: PaletteAction::RefreshModels,
        });
        items.push(PaletteItem {
            label: "Download Model from URL".into(),
            description: "Enter a remote GGUF URL to download [D]".into(),
            category: "Action",
            action: PaletteAction::DownloadModel,
        });
        items.push(PaletteItem {
            label: "Refresh Cluster Peers".into(),
            description: "Broadcast discovery probe and refresh peer list [r]".into(),
            category: "Action",
            action: PaletteAction::RefreshCluster,
        });
        items.push(PaletteItem {
            label: "Toggle Log Auto-Follow".into(),
            description: "Pause or resume real-time log tailing [Space]".into(),
            category: "Action",
            action: PaletteAction::ToggleLogTail,
        });
        items.push(PaletteItem {
            label: "Cycle Log Filter Level".into(),
            description: "Filter logs by ALL / INFO / WARN / ERROR [l]".into(),
            category: "Action",
            action: PaletteAction::CycleLogFilter,
        });
        items.push(PaletteItem {
            label: "Clear Log Buffer".into(),
            description: "Clear in-memory log buffer [c]".into(),
            category: "Action",
            action: PaletteAction::ClearLogs,
        });
        items.push(PaletteItem {
            label: "Keyboard Shortcuts & Help".into(),
            description: "Show global and context keybindings reference [?]".into(),
            category: "Action",
            action: PaletteAction::OpenHelp,
        });

        // Slash commands
        items.push(PaletteItem {
            label: "/reset".into(),
            description: "Clear chat history and reset context".into(),
            category: "Command",
            action: PaletteAction::ChatSlashCommand("/reset".into()),
        });
        items.push(PaletteItem {
            label: "/doctor".into(),
            description: "Diagnose mesh preconditions, ports, and GPU health".into(),
            category: "Command",
            action: PaletteAction::ChatSlashCommand("/doctor".into()),
        });
        items.push(PaletteItem {
            label: "/compact".into(),
            description: "Trigger memory compaction and flush KV cache".into(),
            category: "Command",
            action: PaletteAction::ChatSlashCommand("/compact".into()),
        });

        // Discovered local models
        for m in &self.models_view.models {
            items.push(PaletteItem {
                label: format!("Load Model: {}", m.filename),
                description: format!("Target select and load {} ({} MB)", m.filename, m.size_mb),
                category: "Model",
                action: PaletteAction::SelectModel(m.path.clone()),
            });
        }

        // Discovered peers
        for p in &self.cluster_view.peers {
            items.push(PaletteItem {
                label: format!("Connect Peer: {}", p.label()),
                description: format!(
                    "Direct chat client to {} (Free RAM: {} MB)",
                    p.api_endpoint(),
                    p.free_ram_mb
                ),
                category: "Cluster",
                action: PaletteAction::ConnectPeer(p.label(), p.api_endpoint()),
            });
        }

        items
    }

    pub fn filter_palette_items<'a>(
        &self,
        items: &'a [PaletteItem],
        query: &str,
    ) -> Vec<&'a PaletteItem> {
        let q = query.trim().to_lowercase();
        if q.is_empty() {
            return items.iter().collect();
        }

        let mut scored: Vec<(i32, &'a PaletteItem)> = items
            .iter()
            .filter_map(|item| {
                let label_lower = item.label.to_lowercase();
                let desc_lower = item.description.to_lowercase();
                let cat_lower = item.category.to_lowercase();

                let score = if label_lower.starts_with(&q) {
                    1000 - (label_lower.len() as i32)
                } else if label_lower
                    .split_whitespace()
                    .any(|word| word.starts_with(&q))
                {
                    800
                } else if label_lower.contains(&q) {
                    600
                } else if desc_lower.contains(&q) || cat_lower.contains(&q) {
                    400
                } else if is_subsequence(&q, &label_lower) {
                    200
                } else if is_subsequence(&q, &desc_lower) {
                    100
                } else {
                    return None;
                };

                Some((score, item))
            })
            .collect();

        scored.sort_by_key(|a| std::cmp::Reverse(a.0));
        scored.into_iter().map(|(_, item)| item).collect()
    }

    pub fn execute_palette_action(
        &mut self,
        action: PaletteAction,
        cmd_tx: &mpsc::Sender<HubCommand>,
    ) {
        match action {
            PaletteAction::SwitchTab(tab) => self.set_tab(tab),
            PaletteAction::UnloadModel => {
                let _ = cmd_tx.try_send(HubCommand::Unload {
                    active_model_name: self.active_model_name.clone(),
                });
            }
            PaletteAction::OpenHelp => self.show_help = true,
            PaletteAction::RefreshCluster => {
                self.set_tab(HubTab::Cluster);
                let _ = cmd_tx.try_send(HubCommand::RefreshCluster);
            }
            PaletteAction::RefreshModels => {
                self.set_tab(HubTab::Models);
                self.models_view.refresh();
            }
            PaletteAction::DownloadModel => {
                self.set_tab(HubTab::Models);
                self.pending_download_url = Some(String::new());
            }
            PaletteAction::ToggleLogTail => {
                self.set_tab(HubTab::Logs);
                self.logs_view.toggle_tail();
            }
            PaletteAction::CycleLogFilter => {
                self.set_tab(HubTab::Logs);
                self.logs_view.cycle_filter();
            }
            PaletteAction::ClearLogs => {
                self.set_tab(HubTab::Logs);
                self.logs_view.clear();
            }
            PaletteAction::ChatSlashCommand(cmd) => {
                self.set_tab(HubTab::Chat);
                self.chat.handle_slash_command(&cmd);
            }
            PaletteAction::SelectModel(path) => {
                let _ = cmd_tx.try_send(HubCommand::OpenTargetSelection { model_path: path });
            }
            PaletteAction::ConnectPeer(label, ep) => {
                self.chat.client = NexusClient::new(ep.clone());
                self.chat.model_name = "cluster-model".to_string();
                self.active_model_name = "cluster-model".to_string();
                self.chat
                    .set_target_hardware(&label, "Remote RPC".to_string());
                self.set_tab(HubTab::Chat);
                self.status_message = Some((format!("Connected to peer at {}", ep), Color::Green));
            }
        }
    }

    fn render_command_palette(&self, frame: &mut Frame, area: Rect) {
        let modal = centered_rect(70, 60, area);
        frame.render_widget(Clear, modal);

        let border_block = Block::default()
            .title(" 🧭 Command Palette (Ctrl+P) ")
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::Cyan));
        let inner = border_block.inner(modal);
        frame.render_widget(border_block, modal);

        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(3), // Search input
                Constraint::Min(6),    // Results list
                Constraint::Length(1), // Footer hint
            ])
            .split(inner);

        let input_line = Line::from(vec![
            Span::styled(
                "❯ ",
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                &self.palette_input,
                Style::default()
                    .fg(Color::White)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled("█", Style::default().fg(Color::Cyan)),
        ]);
        let input_block = Block::default()
            .title(" Search commands, tabs, models, peers, shortcuts ")
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::DarkGray));
        frame.render_widget(Paragraph::new(input_line).block(input_block), chunks[0]);

        let items = self.build_palette_items();
        let filtered = self.filter_palette_items(&items, &self.palette_input);

        if filtered.is_empty() {
            let empty_p = Paragraph::new(Line::from(Span::styled(
                "No matching actions found. Press Esc to cancel.",
                Style::default().fg(Color::DarkGray),
            )))
            .alignment(Alignment::Center);
            frame.render_widget(empty_p, chunks[1]);
        } else {
            let height = chunks[1].height as usize;
            let start = if self.palette_selected_idx >= height {
                self.palette_selected_idx - height + 1
            } else {
                0
            };
            let end = (start + height).min(filtered.len());

            let mut lines = Vec::new();
            for (idx, item) in filtered.iter().enumerate().take(end).skip(start) {
                let is_selected = idx == self.palette_selected_idx;
                let cursor = if is_selected { "▶ " } else { "  " };

                let cat_color = match item.category {
                    "Navigation" => Color::Yellow,
                    "Action" => Color::Magenta,
                    "Model" => Color::Green,
                    "Cluster" => Color::Cyan,
                    "Command" => Color::LightBlue,
                    _ => Color::White,
                };

                let item_style = if is_selected {
                    Style::default()
                        .fg(Color::White)
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(Color::Gray)
                };

                let line = Line::from(vec![
                    Span::styled(
                        cursor,
                        if is_selected {
                            Style::default().fg(Color::Cyan)
                        } else {
                            Style::default().fg(Color::DarkGray)
                        },
                    ),
                    Span::styled(
                        format!("[{}] ", item.category),
                        Style::default().fg(cat_color).add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(&item.label, item_style),
                    Span::styled(
                        format!("  — {}", item.description),
                        Style::default().fg(Color::DarkGray),
                    ),
                ]);
                lines.push(line);
            }
            frame.render_widget(Paragraph::new(lines), chunks[1]);
        }

        let hint = Paragraph::new(Line::from(Span::styled(
            " [↑/↓/Ctrl+N/Ctrl+P] Select | [Enter] Run | [Esc] Close ",
            Style::default().fg(Color::DarkGray),
        )))
        .alignment(Alignment::Center);
        frame.render_widget(hint, chunks[2]);
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
    crate::ui::layout::responsive_centered_rect(
        percent_x,
        percent_y,
        r,
        crate::ui::layout::LayoutMode::Auto,
    )
}

fn is_subsequence(needle: &str, haystack: &str) -> bool {
    let mut h_chars = haystack.chars();
    for n_char in needle.chars() {
        if !h_chars.any(|c| c == n_char) {
            return false;
        }
    }
    true
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

    let gateway_handle = if hub.config.network.gateway_enabled {
        let gateway_addr = SocketAddr::from(([0, 0, 0, 0], hub.config.network.gateway_port));
        let gateway_ctx = Arc::new(
            GatewayContext::new(
                hub.supervisor.clone(),
                hub.config.network.api_port,
                PathBuf::from(&hub.config.node.models_dir),
                hub.discovery.node_uuid(),
                hub.shared_config.clone(),
            )
            .with_discovery(hub.discovery.clone()),
        );
        let handle = spawn_gateway(gateway_addr, gateway_ctx);
        info!(
            "Hub mesh gateway listening on port {}",
            hub.config.network.gateway_port
        );
        Some(handle)
    } else {
        None
    };

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
    let mut last_models_catalog_refresh = Instant::now();
    let mut last_models_profile_refresh = Instant::now();

    loop {
        terminal.draw(|f| hub.render(f))?;

        tokio::select! {
            biased;

            Some(event_res) = event_stream.next() => {
                if let Ok(Event::Paste(text)) = &event_res {
                    if let Some(buf) = &mut hub.pending_download_url {
                        buf.push_str(text.trim());
                        continue;
                    }
                }

                if let Ok(Event::Key(key)) = event_res {
                    if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
                        break;
                    }

                    if hub.show_command_palette {
                        let items = hub.build_palette_items();
                        let filtered_count =
                            hub.filter_palette_items(&items, &hub.palette_input).len();
                        match key.code {
                            KeyCode::Esc => {
                                hub.show_command_palette = false;
                                hub.palette_input.clear();
                            }
                            KeyCode::Up => {
                                if filtered_count > 0 {
                                    if hub.palette_selected_idx == 0 {
                                        hub.palette_selected_idx = filtered_count - 1;
                                    } else {
                                        hub.palette_selected_idx -= 1;
                                    }
                                }
                            }
                            KeyCode::Down => {
                                if filtered_count > 0 {
                                    hub.palette_selected_idx =
                                        (hub.palette_selected_idx + 1) % filtered_count;
                                }
                            }
                            KeyCode::Char('p')
                                if key.modifiers.contains(KeyModifiers::CONTROL) =>
                            {
                                if filtered_count > 0 {
                                    if hub.palette_selected_idx == 0 {
                                        hub.palette_selected_idx = filtered_count - 1;
                                    } else {
                                        hub.palette_selected_idx -= 1;
                                    }
                                }
                            }
                            KeyCode::Char('n')
                                if key.modifiers.contains(KeyModifiers::CONTROL) =>
                            {
                                if filtered_count > 0 {
                                    hub.palette_selected_idx =
                                        (hub.palette_selected_idx + 1) % filtered_count;
                                }
                            }
                            KeyCode::Enter => {
                                let filtered =
                                    hub.filter_palette_items(&items, &hub.palette_input);
                                if let Some(selected) = filtered.get(hub.palette_selected_idx) {
                                    let action = selected.action.clone();
                                    hub.show_command_palette = false;
                                    hub.palette_input.clear();
                                    hub.execute_palette_action(action, &cmd_tx);
                                }
                            }
                            KeyCode::Backspace => {
                                hub.palette_input.pop();
                                hub.palette_selected_idx = 0;
                            }
                            KeyCode::Char(c) => {
                                hub.palette_input.push(c);
                                hub.palette_selected_idx = 0;
                            }
                            _ => {}
                        }
                        continue;
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
                                        if let Some(repo_id) = crate::hf::HfClient::parse_repo_id(&url) {
                                            hub.status_message = Some((
                                                format!("Resolving Hugging Face repo '{repo_id}'..."),
                                                Color::Yellow,
                                            ));
                                            let _ = cmd_tx.try_send(HubCommand::ResolveHfRepo { repo_id });
                                        } else {
                                            let _ = cmd_tx.try_send(HubCommand::StartDownload { url });
                                        }
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

                    // HF Quant Picker modal
                    if let Some(picker) = &mut hub.pending_hf_quant_picker {
                        match key.code {
                            KeyCode::Esc => {
                                hub.pending_hf_quant_picker = None;
                            }
                            KeyCode::Up | KeyCode::Char('k') => {
                                if picker.selected_idx > 0 {
                                    picker.selected_idx -= 1;
                                } else if !picker.groups.is_empty() {
                                    picker.selected_idx = picker.groups.len() - 1;
                                }
                            }
                            KeyCode::Down | KeyCode::Char('j') => {
                                if !picker.groups.is_empty() {
                                    picker.selected_idx =
                                        (picker.selected_idx + 1) % picker.groups.len();
                                }
                            }
                            KeyCode::Enter => {
                                if let Some(picker) = hub.pending_hf_quant_picker.take() {
                                    if let Some(group) = picker.groups.get(picker.selected_idx) {
                                        let _ = cmd_tx.try_send(HubCommand::StartDownloadGroup {
                                            files: group.files.clone(),
                                        });
                                    }
                                }
                            }
                            _ => {}
                        }
                        continue;
                    }

                    // HF Auth Recovery modal
                    if let Some(recovery) = &mut hub.pending_hf_auth_recovery {
                        match key.code {
                            KeyCode::Esc => {
                                hub.pending_hf_auth_recovery = None;
                                hub.status_message =
                                    Some(("Authentication cancelled".into(), Color::DarkGray));
                            }
                            KeyCode::Backspace => {
                                recovery.input_token.pop();
                            }
                            KeyCode::Char(c) => {
                                recovery.input_token.push(c);
                            }
                            KeyCode::Enter => {
                                if let Some(recovery) = hub.pending_hf_auth_recovery.take() {
                                    let token = recovery.input_token.trim().to_string();
                                    if !token.is_empty() {
                                        hub.config.huggingface.token = Some(token.clone());
                                        let _ = cmd_tx.try_send(HubCommand::SaveHfToken { token });
                                        hub.status_message = Some((
                                            "Token saved. Retrying request...".into(),
                                            Color::Green,
                                        ));
                                        if let Some(url) = recovery.retry_download_url {
                                            let _ = cmd_tx.try_send(HubCommand::StartDownload { url });
                                        } else {
                                            let _ = cmd_tx.try_send(HubCommand::ResolveHfRepo {
                                                repo_id: recovery.repo_id,
                                            });
                                        }
                                    } else {
                                        hub.status_message =
                                            Some(("No token provided".into(), Color::Yellow));
                                    }
                                }
                            }
                            _ => {}
                        }
                        continue;
                    }

                    // Active download modal cancellation
                    if hub.download_progress.is_some() {
                        match key.code {
                            KeyCode::Esc | KeyCode::Char('c') | KeyCode::Char('C') => {
                                let _ = cmd_tx.try_send(HubCommand::CancelDownload);
                                hub.status_message = Some(("Cancelling download...".into(), Color::Yellow));
                            }
                            _ => {}
                        }
                        continue;
                    }

                    // Delete model confirmation modal
                    if hub.pending_delete_model.is_some() {
                        match key.code {
                            KeyCode::Esc | KeyCode::Char('n') | KeyCode::Char('N') => {
                                hub.pending_delete_model = None;
                                hub.status_message = Some(("Deletion cancelled".into(), Color::DarkGray));
                            }
                            KeyCode::Enter | KeyCode::Char('y') | KeyCode::Char('Y') => {
                                if let Some((entry, shards, _)) = hub.pending_delete_model.take() {
                                    let _ = cmd_tx.try_send(HubCommand::DeleteModel {
                                        path: entry.path,
                                        shards,
                                        filename: entry.filename,
                                    });
                                    hub.status_message = Some(("Deleting model...".into(), Color::Yellow));
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
                                            TargetExecutionNode::Distributed {
                                                extra_args,
                                                gpu_layers,
                                                worker_names,
                                                ..
                                            } => {
                                                hub.chat.set_target_hardware(
                                                    "Distributed",
                                                    format!("RPC [{}]", worker_names.join(", ")),
                                                );
                                                match request_load_or_hot_swap_with_args(
                                                    &hub.supervisor,
                                                    state.model_path.clone(),
                                                    Some(*gpu_layers),
                                                    ctx_size,
                                                    extra_args.clone(),
                                                )
                                                .await
                                                {
                                                    Ok(cmd) => {
                                                        hub.load_phase =
                                                            Some("Queued distributed load…".into());
                                                        let _ = cmd_tx.try_send(cmd);
                                                    }
                                                    Err(intent) => {
                                                        hub.pending_hot_swap = Some(intent);
                                                    }
                                                }
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
                                            extra_args: Vec::new(),
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
                                    extra_args: Vec::new(),
                                });
                            }
                        } else if matches!(key.code, KeyCode::Char('N')) {
                            hub.pending_hot_swap = None;
                        }
                        continue;
                    }

                    // HF Explorer mode interception when on Models tab
                    if hub.active_tab == HubTab::Models
                        && hub.models_view.mode == crate::ui::models_view::ModelsTabMode::HfExplorer
                    {
                        if hub.models_view.hf_is_searching {
                            match key.code {
                                KeyCode::Esc => {
                                    hub.models_view.hf_is_searching = false;
                                }
                                KeyCode::Enter => {
                                    hub.models_view.hf_is_searching = false;
                                    let q = hub.models_view.hf_search_query.trim().to_string();
                                    if !q.is_empty() {
                                        hub.models_view.hf_loading = true;
                                        hub.status_message = Some((
                                            format!("Searching Hugging Face for '{q}'..."),
                                            Color::Yellow,
                                        ));
                                        let _ = cmd_tx.try_send(HubCommand::SearchHfModels {
                                            query: q,
                                            limit: 25,
                                        });
                                    }
                                }
                                KeyCode::Backspace => {
                                    hub.models_view.hf_search_query.pop();
                                }
                                KeyCode::Char(c) => {
                                    hub.models_view.hf_search_query.push(c);
                                }
                                _ => {}
                            }
                            continue;
                        } else {
                            match key.code {
                                KeyCode::Esc | KeyCode::Char('e') | KeyCode::Char('E') => {
                                    hub.models_view.mode = crate::ui::models_view::ModelsTabMode::Local;
                                    continue;
                                }
                                KeyCode::Char('/') => {
                                    hub.models_view.hf_is_searching = true;
                                    continue;
                                }
                                KeyCode::Char('t') | KeyCode::Char('T') => {
                                    hub.models_view.hf_loading = true;
                                    hub.status_message = Some((
                                        "Fetching trending GGUF models...".to_string(),
                                        Color::Yellow,
                                    ));
                                    let _ = cmd_tx.try_send(HubCommand::FetchHfTrending { limit: 25 });
                                    continue;
                                }
                                KeyCode::Up | KeyCode::Char('k') => {
                                    hub.models_view.hf_previous();
                                    continue;
                                }
                                KeyCode::Down | KeyCode::Char('j') => {
                                    hub.models_view.hf_next();
                                    continue;
                                }
                                KeyCode::Enter => {
                                    if let Some(m) = hub.models_view.selected_hf_model() {
                                        let repo_id = m.id.clone();
                                        hub.status_message = Some((
                                            format!("Resolving Hugging Face repo '{repo_id}'..."),
                                            Color::Yellow,
                                        ));
                                        let _ = cmd_tx.try_send(HubCommand::ResolveHfRepo { repo_id });
                                    }
                                    continue;
                                }
                                _ => {}
                            }
                        }
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
                            HubAction::TabModels => {
                                hub.set_tab(HubTab::Models);
                                let _ = cmd_tx.try_send(HubCommand::RefreshModelCatalog);
                                last_models_catalog_refresh = Instant::now();
                            }
                            HubAction::TabCluster => {
                                hub.set_tab(HubTab::Cluster);
                                let _ = cmd_tx.try_send(HubCommand::RefreshCluster);
                            }
                            HubAction::TabSettings => hub.set_tab(HubTab::Settings),
                            HubAction::TabTunnel => hub.set_tab(HubTab::Tunnel),
                            HubAction::TabAgents => {
                                hub.agents_view.refresh();
                                hub.set_tab(HubTab::Agents);
                            }
                            HubAction::TabLogs => {
                                hub.logs_view.refresh();
                                hub.set_tab(HubTab::Logs);
                            }
                            HubAction::OpenCommandPalette => {
                                hub.show_command_palette = true;
                                hub.palette_input.clear();
                                hub.palette_selected_idx = 0;
                            }
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
                            HubAction::ModelsRefresh => {
                                hub.models_view.refresh();
                                let _ = cmd_tx.try_send(HubCommand::RefreshModelCatalog);
                                last_models_catalog_refresh = Instant::now();
                            }
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
                            HubAction::ModelsDelete => {
                                if let Some(row) = hub.models_view.selected_row() {
                                    if let Some(ref entry) = row.local {
                                        let file_stem = entry.path.file_name().unwrap_or_default().to_string_lossy();
                                        let is_active = hub.active_model_name == entry.filename
                                            || hub.active_model_name == file_stem;
                                        if is_active {
                                            hub.status_message = Some((
                                                "Cannot delete active model — please unload [U] first".into(),
                                                Color::Red,
                                            ));
                                        } else {
                                            let shards = crate::ui::models::detect_model_shards(&entry.path);
                                            let total_bytes = crate::ui::models::calculate_shards_total_bytes(&shards);
                                            hub.pending_delete_model = Some((entry.clone(), shards, total_bytes));
                                        }
                                    } else {
                                        hub.status_message = Some((
                                            "Selected model is remote only — cannot delete locally".into(),
                                            Color::DarkGray,
                                        ));
                                    }
                                } else {
                                    hub.status_message = Some(("No model selected to delete".into(), Color::DarkGray));
                                }
                            }
                            HubAction::CancelDownload => {
                                let _ = cmd_tx.try_send(HubCommand::CancelDownload);
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
                            HubAction::ModelsToggleExplorer => {
                                let new_mode = hub.models_view.toggle_mode();
                                if new_mode == crate::ui::models_view::ModelsTabMode::HfExplorer
                                    && hub.models_view.hf_models.is_empty()
                                {
                                    hub.models_view.hf_loading = true;
                                    hub.status_message = Some((
                                        "Fetching trending GGUF models...".to_string(),
                                        Color::Yellow,
                                    ));
                                    let _ = cmd_tx.try_send(HubCommand::FetchHfTrending { limit: 25 });
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
                                    | HubAction::TabTunnel
                                    | HubAction::TabAgents
                                    | HubAction::TabLogs
                                    | HubAction::OpenCommandPalette
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
                        HubTab::Tunnel => match key.code {
                            KeyCode::Char('r') | KeyCode::Char('R') => hub.tunnel_view.refresh(),
                            KeyCode::Char('s') | KeyCode::Char('S') => hub.tunnel_view.setup_tunnel(),
                            KeyCode::Char('t') | KeyCode::Char('T') => {
                                hub.tunnel_view.teardown_tunnel()
                            }
                            KeyCode::Tab => hub.next_tab(),
                            KeyCode::BackTab => hub.previous_tab(),
                            _ => {}
                        },
                        HubTab::Agents => match key.code {
                            KeyCode::Up | KeyCode::Char('k') => hub.agents_view.prev_task(),
                            KeyCode::Down | KeyCode::Char('j') => hub.agents_view.next_task(),
                            KeyCode::Char('r') | KeyCode::Char('R') => hub.agents_view.refresh(),
                            KeyCode::Char('d') | KeyCode::Char('D') => {
                                let embedder = crate::kb::FastPseudoEmbedder::default();
                                let janitor = crate::kb::JanitorAgent::new(
                                    (*hub.kb_store).clone(),
                                    crate::kb::Embedder::Pseudo(embedder),
                                );
                                let chat_turns: Vec<crate::client::ChatMessage> = hub
                                    .chat
                                    .messages
                                    .iter()
                                    .map(|e| e.message.clone())
                                    .collect();
                                let distilled = janitor
                                    .distill_dialogue(&chat_turns, Some("active_session"))
                                    .unwrap_or_default();
                                let count = distilled.len();
                                hub.agents_view.refresh();
                                hub.agents_view.status_message = Some((
                                    format!("Janitor distilled {count} memories from active session"),
                                    Color::Green,
                                ));
                            }
                            KeyCode::Char('s') | KeyCode::Char('S') => {
                                hub.agents_view.status_message = Some((
                                    "Mesh KB sync requested".to_string(),
                                    Color::Cyan,
                                ));
                            }
                            KeyCode::Tab => hub.next_tab(),
                            KeyCode::BackTab => hub.previous_tab(),
                            _ => {}
                        },
                        HubTab::Logs => {
                            if hub.logs_view.is_searching {
                                match key.code {
                                    KeyCode::Enter | KeyCode::Esc => {
                                        hub.logs_view.is_searching = false;
                                    }
                                    KeyCode::Backspace => {
                                        hub.logs_view.search_query.pop();
                                        hub.logs_view.scroll_offset = 0;
                                    }
                                    KeyCode::Char(c) => {
                                        hub.logs_view.search_query.push(c);
                                        hub.logs_view.scroll_offset = 0;
                                    }
                                    _ => {}
                                }
                            } else {
                                match key.code {
                                    KeyCode::Up | KeyCode::Char('k') => hub.logs_view.scroll_up(1),
                                    KeyCode::Down | KeyCode::Char('j') => {
                                        hub.logs_view.scroll_down(1)
                                    }
                                    KeyCode::PageUp => hub.logs_view.scroll_up(10),
                                    KeyCode::PageDown => hub.logs_view.scroll_down(10),
                                    KeyCode::Char(' ') => hub.logs_view.toggle_tail(),
                                    KeyCode::Char('l') | KeyCode::Char('L') => {
                                        hub.logs_view.cycle_filter()
                                    }
                                    KeyCode::Char('c') | KeyCode::Char('C') => {
                                        hub.logs_view.clear()
                                    }
                                    KeyCode::Char('r') | KeyCode::Char('R') => {
                                        hub.logs_view.refresh()
                                    }
                                    KeyCode::Char('/') => {
                                        hub.logs_view.is_searching = true;
                                        hub.logs_view.search_query.clear();
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
            Some(event) = evt_rx.recv() => {
                let needs_cluster = matches!(event, HubEvent::ClusterRefreshed);
                hub.apply_event(event);
                if needs_cluster {
                    // Short discovery snapshot — not a 30s load.
                    hub.cluster_view.refresh().await;
                }
            }
            _ = refresh_interval.tick() => {
                let modal_open = hub.pending_download_url.is_some()
                    || hub.pending_delete_model.is_some()
                    || hub.download_progress.is_some()
                    || hub.pending_hf_quant_picker.is_some()
                    || hub.pending_hf_auth_recovery.is_some()
                    || hub.pending_target_selection.is_some()
                    || hub.pending_push_peers.is_some()
                    || hub.pending_hot_swap.is_some()
                    || (hub.active_tab == HubTab::Models && hub.models_view.hf_is_searching)
                    || hub.show_command_palette
                    || hub.show_help;

                if !modal_open {
                    if hub.active_tab == HubTab::Models
                        && hub.models_view.mode == crate::ui::models_view::ModelsTabMode::Local
                    {
                        if last_models_profile_refresh.elapsed() >= Duration::from_secs(5) {
                            hub.models_view.refresh_profile();
                            last_models_profile_refresh = Instant::now();
                        }
                        if last_models_catalog_refresh.elapsed() >= Duration::from_secs(20) {
                            let _ = cmd_tx.try_send(HubCommand::RefreshModelCatalog);
                            last_models_catalog_refresh = Instant::now();
                        }
                    }
                    if hub.active_tab == HubTab::Cluster {
                        let _ = cmd_tx.try_send(HubCommand::RefreshCluster);
                    }
                    if hub.active_tab == HubTab::Logs {
                        hub.logs_view.refresh();
                    }
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
        }
    }

    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;

    control_handle.abort();
    if let Some(handle) = gateway_handle {
        handle.abort();
    }
    worker.abort();
    if hub.supervisor.is_running().await {
        info!("Stopping active supervisor process on hub exit...");
        let _ = hub.supervisor.stop().await;
    }

    Ok(())
}
