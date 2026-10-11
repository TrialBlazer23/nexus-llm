//! Unified Nexus Hub TUI (Phase 8 — responsive command/event architecture).

pub mod commands;
pub mod keymap;
pub mod modals;
pub mod palette;
pub mod render;
pub mod runner;
pub mod status;

pub use modals::*;
pub use palette::*;
pub use runner::run_hub_tui;

use crate::appliance::hub_chat_target_after_load;
use crate::client::NexusClient;
use crate::config::NexusConfig;
use crate::control_plane::{dispatch_pair, PairRequest, CONTROL_PLANE_VERSION};
use crate::discovery::DiscoveryService;
use crate::node_identity::NodeIdentity;
use crate::supervisor::SupervisorManager;
use crate::ui::chat::{ChatApp, ChatEntry};
use crate::ui::cluster_view::ClusterView;
use crate::ui::models_view::ModelsView;
use crate::ui::settings_view::SettingsView;
use commands::{HubEvent, HubWorkerCtx};
use ratatui::style::Color;
use std::path::PathBuf;
use std::sync::Arc;
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
    /// Chat target host. `"Local"` is the front door; a peer label is a burst.
    pub chat_host: String,
    /// Model name shown for the chat target. Independent of `active_model_name`.
    pub chat_model: String,
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
    /// Enhanced model download modal state.
    pub pending_download_modal: Option<DownloadModalState>,
    /// Local model import modal state.
    pub pending_import_modal: Option<ImportModalState>,
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
        models_view.set_fit_policy(
            config.inference.appliance.floor_tok_s(),
            config.hardware.safety.max_ram_usage_percent,
        );
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
            chat_host: "Local".into(),
            chat_model: "None (Idle)".into(),
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
            pending_download_modal: None,
            pending_import_modal: None,
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
        let desired_context = self.models_view.selected_context;
        self.pending_target_selection =
            Some(commands::build_target_selection(&ctx, model_path, desired_context).await);
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

    /// Gateway base and appliance model id while both flags are on and no host is pinned.
    pub(crate) fn chat_target_after_load(
        &self,
        endpoint: &str,
        model_name: &str,
    ) -> (String, String) {
        let (default_host, appliance, gateway, port) = if let Ok(cfg) = self.shared_config.read() {
            (
                cfg.network.default_host.clone(),
                cfg.inference.appliance.enabled,
                cfg.network.gateway_enabled,
                cfg.network.gateway_port,
            )
        } else {
            (
                self.config.network.default_host.clone(),
                self.config.inference.appliance.enabled,
                self.config.network.gateway_enabled,
                self.config.network.gateway_port,
            )
        };
        hub_chat_target_after_load(
            endpoint,
            model_name,
            default_host.as_deref(),
            appliance,
            gateway,
            port,
        )
    }

    pub(crate) fn session(&mut self) -> status::SessionView {
        status::SessionView {
            local_model: self.active_model_name.clone(),
            chat_host: self.chat_host.clone(),
            chat_model: self.chat_model.clone(),
        }
    }

    pub(crate) fn store_session(&mut self, view: status::SessionView) {
        self.active_model_name = view.local_model;
        self.chat_host = view.chat_host;
        self.chat_model = view.chat_model;
    }

    /// Cluster `d`. Chat goes home. Does not stop the supervisor or unload a peer.
    pub fn disconnect_chat_target(&mut self) {
        let mut view = self.session();
        status::on_cluster_disconnect(&mut view);
        self.store_session(view);
        let endpoint = format!("http://127.0.0.1:{}", self.config.network.api_port);
        let (base, chat_model) = self.chat_target_after_load(&endpoint, &self.active_model_name);
        self.chat.client = NexusClient::new(base.clone());
        self.chat.model_name = chat_model;
        self.chat
            .set_target_hardware("Local Host", "Local".to_string());
        self.status_message = Some(("Reset chat target to local node".to_string(), Color::Green));
        self.chat.messages.push(ChatEntry::notice(format!(
            "Disconnected from peer. Reverted to local endpoint: {base}"
        )));
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
                let mut view = self.session();
                status::on_local_load(&mut view, &model_name);
                self.store_session(view);
                let (base, chat_model) = self.chat_target_after_load(&endpoint, &model_name);
                self.chat.client = NexusClient::new(base);
                self.chat.model_name = chat_model;
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
                let mut view = self.session();
                status::on_local_unload(&mut view);
                self.store_session(view);
                if self.chat_host.eq_ignore_ascii_case("local") {
                    self.chat.model_name = "default".to_string();
                }
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
            HubEvent::PeerUnloaded {
                name,
                success,
                message,
            } => {
                let restore_front_door = success && self.chat_host == name;
                let mut view = self.session();
                status::on_peer_unloaded(&mut view, &name, success);
                if success {
                    self.store_session(view);
                    if restore_front_door {
                        let endpoint = format!("http://127.0.0.1:{}", self.config.network.api_port);
                        let (base, chat_model) =
                            self.chat_target_after_load(&endpoint, &self.active_model_name);
                        self.chat.client = NexusClient::new(base);
                        self.chat.model_name = chat_model;
                        self.chat
                            .set_target_hardware("Local Host", "Local".to_string());
                    }
                    let text = if message.is_empty() {
                        format!("Unloaded model on {name}")
                    } else {
                        message
                    };
                    self.status_message = Some((text, Color::Cyan));
                } else {
                    self.status_message = Some((message, Color::Red));
                }
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
                let mut view = self.session();
                status::on_remote_load(&mut view, &name, &model_name);
                self.store_session(view);
                let (base, chat_model) = self.chat_target_after_load(&api_endpoint, &model_name);
                self.chat.client = NexusClient::new(base);
                self.chat.model_name = chat_model;
                self.chat.set_target_hardware(&name, backend);
                self.chat.messages.clear();
                self.chat.messages.push(ChatEntry::notice(notice));
                self.status_message =
                    Some((format!("Active on {name}: {model_name}"), Color::Green));
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
                let mut view = self.session();
                status::on_local_unload(&mut view);
                self.store_session(view);
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
}
