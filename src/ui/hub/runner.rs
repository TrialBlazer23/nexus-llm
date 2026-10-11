//! Terminal event loop and lifecycle runner for the Unified Nexus Hub.

use std::io::stdout;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crossterm::{
    event::{Event, EventStream, KeyCode, KeyModifiers},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use futures_util::StreamExt;
use ratatui::{backend::CrosstermBackend, style::Color, Terminal};
use tokio::sync::mpsc;
use tracing::{debug, info};

use super::commands::{
    request_load_or_hot_swap, request_load_or_hot_swap_with_args, spawn_hub_worker, HubCommand,
    HubEvent, HubWorkerCtx,
};
use super::keymap::{resolve, HubAction, KeyScope};
use super::modals::*;
use super::status;
use super::{HubApp, HubTab};
use crate::client::NexusClient;
use crate::control_plane_server::{spawn as spawn_control_plane, ControlPlaneContext};
use crate::discovery::NodeRole;
use crate::gateway::load_appliance_front_door;
use crate::gateway::{spawn as spawn_gateway, unload_idle_burst_peers, GatewayContext};
use crate::registry_runtime::spawn_registry_runtime;
use crate::ui::chat::{ChatEntry, StreamMsg};

/// Tunnel tab refresh. Probes every authorized serial immediately, then redraws host ports.
async fn probe_tunnel_tab(hub: &mut HubApp) {
    let forwards = crate::registry_runtime::probe_usb_peers(
        &hub.discovery,
        hub.identity.as_ref(),
        hub.discovery.node_uuid(),
        true,
    )
    .await;
    hub.tunnel_view.apply_forwards(forwards);
    hub.tunnel_view.refresh();
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

    let (gateway_handle, gateway_idle) = if hub.config.network.gateway_enabled {
        let gateway_addr = SocketAddr::from(([0, 0, 0, 0], hub.config.network.gateway_port));
        let gateway_ctx = Arc::new(
            GatewayContext::new(
                hub.supervisor.clone(),
                hub.config.network.api_port,
                PathBuf::from(&hub.config.node.models_dir),
                hub.discovery.node_uuid(),
                hub.shared_config.clone(),
            )
            .with_discovery(hub.discovery.clone())
            .with_identity(hub.identity.clone()),
        );
        let idle = gateway_ctx.clone();
        let handle = spawn_gateway(gateway_addr, gateway_ctx);
        info!(
            "Hub mesh gateway listening on port {}",
            hub.config.network.gateway_port
        );
        (Some(handle), Some(idle))
    } else {
        (None, None)
    };
    if hub.config.inference.appliance.enabled {
        if let Some(name) = load_appliance_front_door(
            &hub.supervisor,
            &hub.discovery,
            &hub.config,
            Path::new(&hub.config.node.llama_server_binary),
            &hub.config.network.api_host,
            hub.config.network.api_port,
        )
        .await
        {
            let mut view = hub.session();
            status::on_local_load(&mut view, &name);
            hub.store_session(view);
        }
    }

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
                    if let Some(modal) = &mut hub.pending_download_modal {
                        modal.custom_url.push_str(text.trim());
                        modal.mode = DownloadModalMode::CustomUrl;
                        continue;
                    }
                    if let Some(modal) = &mut hub.pending_import_modal {
                        modal.custom_path.push_str(text.trim());
                        modal.in_custom_path_mode = true;
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

                    // Download modal (Curated starters / Custom URL)
                    if let Some(modal) = &mut hub.pending_download_modal {
                        match key.code {
                            KeyCode::Esc => {
                                hub.pending_download_modal = None;
                            }
                            KeyCode::Tab => {
                                modal.mode = match modal.mode {
                                    DownloadModalMode::Starters => DownloadModalMode::CustomUrl,
                                    DownloadModalMode::CustomUrl => DownloadModalMode::Starters,
                                };
                            }
                            KeyCode::Up | KeyCode::Char('k') if modal.mode == DownloadModalMode::Starters => {
                                if modal.selected_starter_idx > 0 {
                                    modal.selected_starter_idx -= 1;
                                } else if !modal.starters.is_empty() {
                                    modal.selected_starter_idx = modal.starters.len() - 1;
                                }
                            }
                            KeyCode::Down | KeyCode::Char('j') if modal.mode == DownloadModalMode::Starters => {
                                if !modal.starters.is_empty() {
                                    modal.selected_starter_idx =
                                        (modal.selected_starter_idx + 1) % modal.starters.len();
                                }
                            }
                            KeyCode::Enter => {
                                match modal.mode {
                                    DownloadModalMode::Starters => {
                                        if let Some(starter) = modal.starters.get(modal.selected_starter_idx) {
                                            let url = starter.download_url.to_string();
                                            hub.status_message = Some((
                                                format!("Starting download: {}...", starter.name),
                                                Color::Cyan,
                                            ));
                                            let _ = cmd_tx.try_send(HubCommand::StartDownload { url });
                                            hub.pending_download_modal = None;
                                        }
                                    }
                                    DownloadModalMode::CustomUrl => {
                                        let url = modal.custom_url.trim().to_string();
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
                                            hub.pending_download_modal = None;
                                        }
                                    }
                                }
                            }
                            KeyCode::Backspace if modal.mode == DownloadModalMode::CustomUrl => {
                                modal.custom_url.pop();
                            }
                            KeyCode::Char(c) if modal.mode == DownloadModalMode::CustomUrl => {
                                modal.custom_url.push(c);
                            }
                            _ => {}
                        }
                        continue;
                    }

                    // Local model import modal
                    if let Some(modal) = &mut hub.pending_import_modal {
                        match key.code {
                            KeyCode::Esc => {
                                hub.pending_import_modal = None;
                            }
                            KeyCode::Tab => {
                                modal.in_custom_path_mode = !modal.in_custom_path_mode;
                            }
                            KeyCode::Up | KeyCode::Char('k') if !modal.in_custom_path_mode => {
                                if modal.selected_idx > 0 {
                                    modal.selected_idx -= 1;
                                } else if !modal.candidates.is_empty() {
                                    modal.selected_idx = modal.candidates.len() - 1;
                                }
                            }
                            KeyCode::Down | KeyCode::Char('j') if !modal.in_custom_path_mode => {
                                if !modal.candidates.is_empty() {
                                    modal.selected_idx =
                                        (modal.selected_idx + 1) % modal.candidates.len();
                                }
                            }
                            KeyCode::Enter => {
                                if modal.in_custom_path_mode {
                                    let input_str = modal.custom_path.trim().to_string();
                                    if !input_str.is_empty() {
                                        let path = PathBuf::from(&input_str);
                                        if path.is_dir() {
                                            let candidates = crate::import::scan_directory(&path, &hub.config.node.models_dir);
                                            if candidates.is_empty() {
                                                hub.status_message = Some((
                                                    format!("No .gguf models found in '{}'", path.display()),
                                                    Color::Yellow,
                                                ));
                                            } else {
                                                let count = candidates.len();
                                                modal.candidates = candidates;
                                                modal.selected_idx = 0;
                                                modal.in_custom_path_mode = false;
                                                hub.status_message = Some((
                                                    format!("Found {count} model(s) in '{}'", path.display()),
                                                    Color::Green,
                                                ));
                                            }
                                        } else if path.is_file() {
                                            match crate::import::import_file(&path, &hub.config.node.models_dir, crate::import::ImportMode::Symlink) {
                                                Ok(outcome) => {
                                                    hub.status_message = Some((
                                                        format!("Imported '{}' ({})", outcome.filename, outcome.mode_used),
                                                        Color::Green,
                                                    ));
                                                    hub.models_view.refresh();
                                                    hub.pending_import_modal = None;
                                                }
                                                Err(e) => {
                                                    hub.status_message = Some((
                                                        format!("Failed to import: {e}"),
                                                        Color::Red,
                                                    ));
                                                }
                                            }
                                        } else {
                                            hub.status_message = Some((
                                                format!("Path does not exist: '{}'", path.display()),
                                                Color::Red,
                                            ));
                                        }
                                    }
                                } else if let Some(cand) = modal.candidates.get(modal.selected_idx) {
                                    if !cand.is_valid_gguf {
                                        hub.status_message = Some((
                                            format!("Cannot import corrupt model: {}", cand.validation_error.as_deref().unwrap_or("invalid")),
                                            Color::Red,
                                        ));
                                    } else {
                                        let src = cand.source_path.clone();
                                        match crate::import::import_file(&src, &hub.config.node.models_dir, crate::import::ImportMode::Symlink) {
                                            Ok(outcome) => {
                                                hub.status_message = Some((
                                                    format!("Imported '{}' ({})", outcome.filename, outcome.mode_used),
                                                    Color::Green,
                                                ));
                                                hub.models_view.refresh();
                                                hub.pending_import_modal = None;
                                            }
                                            Err(e) => {
                                                hub.status_message = Some((
                                                    format!("Failed to import: {e}"),
                                                    Color::Red,
                                                ));
                                            }
                                        }
                                    }
                                }
                            }
                            KeyCode::Backspace if modal.in_custom_path_mode => {
                                modal.custom_path.pop();
                            }
                            KeyCode::Char(c) if modal.in_custom_path_mode => {
                                modal.custom_path.push(c);
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
                                            TargetExecutionNode::LocalMoeStream {
                                                ceil_mb,
                                                context_size: moe_ctx,
                                                ..
                                            } => {
                                                hub.chat.set_target_hardware(
                                                    "Local Host",
                                                    format!(
                                                        "Local MoE flash-stream (ctx {} | ceil {} MB)",
                                                        moe_ctx, ceil_mb
                                                    ),
                                                );
                                                match request_load_or_hot_swap_with_args(
                                                    &hub.supervisor,
                                                    state.model_path.clone(),
                                                    Some(0),
                                                    *moe_ctx,
                                                    Vec::new(),
                                                    Some(*ceil_mb),
                                                )
                                                .await
                                                {
                                                    Ok(cmd) => {
                                                        hub.load_phase =
                                                            Some("Queued MoE stream load…".into());
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
                                                    moe_cache_ceil_mb: None,
                                                });
                                            }
                                            TargetExecutionNode::RemoteMoeStream {
                                                endpoint,
                                                api_endpoint,
                                                name,
                                                context_size: moe_ctx,
                                                ceil_mb,
                                                ..
                                            } => {
                                                hub.chat.set_target_hardware(
                                                    name,
                                                    format!(
                                                        "Remote MoE flash-stream (ctx {} | ceil {} MB)",
                                                        moe_ctx, ceil_mb
                                                    ),
                                                );
                                                hub.load_phase =
                                                    Some(format!("Queued remote MoE stream on {name}…"));
                                                let _ = cmd_tx.try_send(HubCommand::LoadModelRemote {
                                                    endpoint: endpoint.clone(),
                                                    api_endpoint: api_endpoint.clone(),
                                                    name: name.clone(),
                                                    backend: "bmoe".into(),
                                                    model_name: state.model_name.clone(),
                                                    context_size: *moe_ctx,
                                                    gpu_layers: 0,
                                                    moe_cache_ceil_mb: Some(*ceil_mb),
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
                                                    None,
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
                                            extra_args: intent.extra_args,
                                            moe_cache_ceil_mb: intent.moe_cache_ceil_mb,
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
                                    extra_args: intent.extra_args,
                                    moe_cache_ceil_mb: intent.moe_cache_ceil_mb,
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
                            HubAction::TabTunnel => {
                                hub.set_tab(HubTab::Tunnel);
                                probe_tunnel_tab(&mut hub).await;
                            },
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
                                    let _ = cmd_tx.try_send(HubCommand::OpenTargetSelection {
                                        model_path: path,
                                        desired_context: hub.models_view.selected_context,
                                    });
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
                                hub.pending_download_modal = Some(DownloadModalState {
                                    custom_url: String::new(),
                                    starters: crate::hf::curated_starter_models(),
                                    selected_starter_idx: 0,
                                    mode: DownloadModalMode::Starters,
                                });
                            }
                            HubAction::ModelsImport => {
                                let candidates = crate::import::scan_all_candidate_locations(&hub.models_view.models_dir);
                                hub.pending_import_modal = Some(ImportModalState {
                                    candidates,
                                    selected_idx: 0,
                                    custom_path: String::new(),
                                    in_custom_path_mode: false,
                                });
                            }
                            HubAction::CleanCorruptedModels => {
                                let corrupted = if hub.models_view.corrupted_models.is_empty() {
                                    crate::import::find_corrupted_models(&hub.models_view.models_dir)
                                } else {
                                    hub.models_view.corrupted_models.clone()
                                };
                                match crate::import::delete_corrupted_models(&corrupted, &hub.models_view.models_dir) {
                                    Ok(deleted) if deleted > 0 => {
                                        hub.status_message = Some((
                                            format!("Cleaned up {deleted} corrupted model file(s)"),
                                            Color::Green,
                                        ));
                                        hub.models_view.refresh();
                                    }
                                    Ok(_) => {
                                        hub.status_message = Some((
                                            "No corrupted model files found".into(),
                                            Color::DarkGray,
                                        ));
                                    }
                                    Err(e) => {
                                        hub.status_message = Some((
                                            format!("Failed to clean corrupted models: {e}"),
                                            Color::Red,
                                        ));
                                    }
                                }
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
                                    let peer_ip = active_peer.addr.ip();
                                    let model = if !active_peer.active_model.is_empty() {
                                        active_peer.active_model.clone()
                                    } else {
                                        "cluster-model".to_string()
                                    };
                                    let peer_label = active_peer.label();
                                    let backend = active_peer.backend.to_string();
                                    hub.discovery.send_direct_probe_to_ip(peer_ip).await;
                                    hub.chat.client = NexusClient::new(ep.clone());
                                    hub.chat.model_name = model.clone();
                                    let mut view = hub.session();
                                    status::on_remote_load(&mut view, &peer_label, &model);
                                    hub.store_session(view);
                                    hub.chat.set_target_hardware(&peer_label, backend);
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
                                            let peer_ip = peer.addr.ip();
                                            let model = if !peer.active_model.is_empty() {
                                                peer.active_model.clone()
                                            } else {
                                                "cluster-peer".to_string()
                                            };
                                            let peer_name = peer.label();
                                            let backend = peer.backend.to_string();
                                            hub.discovery.send_direct_probe_to_ip(peer_ip).await;
                                            hub.chat.client = NexusClient::new(ep.clone());
                                            hub.chat.model_name = model.clone();
                                            let mut view = hub.session();
                                            status::on_remote_load(&mut view, &peer_name, &model);
                                            hub.store_session(view);
                                            hub.chat.set_target_hardware(&peer_name, backend);
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
                                                    moe_cache_ceil_mb: None,
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
                                        hub.disconnect_chat_target();
                                    }
                                    KeyCode::Char('u') | KeyCode::Char('U') => {
                                        if let Some(peer) = hub.cluster_view.selected_peer() {
                                            let local = hub.discovery.node_uuid();
                                            match crate::ui::hub::status::choose_unload(
                                                peer.uuid,
                                                local,
                                                &peer.control_endpoint(),
                                            ) {
                                                crate::ui::hub::status::UnloadChoice::Local => {
                                                    let _ = cmd_tx.try_send(HubCommand::Unload {
                                                        active_model_name: hub
                                                            .active_model_name
                                                            .clone(),
                                                    });
                                                }
                                                crate::ui::hub::status::UnloadChoice::Peer {
                                                    endpoint,
                                                } => {
                                                    let _ = cmd_tx.try_send(HubCommand::UnloadPeer {
                                                        endpoint,
                                                        name: peer.label(),
                                                    });
                                                }
                                            }
                                        }
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
                            KeyCode::Char('r') | KeyCode::Char('R') => probe_tunnel_tab(&mut hub).await,
                            KeyCode::Char('f')
                            | KeyCode::Char('F')
                            | KeyCode::Char('s')
                            | KeyCode::Char('S') => probe_tunnel_tab(&mut hub).await,
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
                    StreamMsg::Status(text) => {
                        hub.chat.pending_status.push(text);
                    }
                    StreamMsg::Route(header) => {
                        hub.chat.pending_route = Some(header);
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
                if let Some(ctx) = &gateway_idle {
                    unload_idle_burst_peers(ctx);
                }
                let modal_open = hub.pending_download_modal.is_some()
                    || hub.pending_import_modal.is_some()
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
