//! Command Palette structures, fuzzy matching, execution, and rendering.

use ratatui::{
    layout::{Alignment, Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, Paragraph},
    Frame,
};
use std::path::PathBuf;
use tokio::sync::mpsc;

use crate::client::NexusClient;
use crate::ui::hub::commands::HubCommand;
use crate::ui::hub::modals::{DownloadModalMode, DownloadModalState, ImportModalState};
use crate::ui::hub::render::centered_rect;
use crate::ui::hub::status;
use crate::ui::hub::{HubApp, HubTab};

/// Target action for Command Palette execution.
#[derive(Debug, Clone)]
pub enum PaletteAction {
    SwitchTab(HubTab),
    UnloadModel,
    OpenHelp,
    RefreshCluster,
    RefreshModels,
    DownloadModel,
    ImportModel,
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

pub fn is_subsequence(needle: &str, haystack: &str) -> bool {
    let mut h_chars = haystack.chars();
    for n_char in needle.chars() {
        if !h_chars.any(|c| c == n_char) {
            return false;
        }
    }
    true
}

impl HubApp {
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
            label: "Import Local Model".into(),
            description: "Scan download folders or import custom GGUF model [i]".into(),
            category: "Action",
            action: PaletteAction::ImportModel,
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
                self.pending_download_modal = Some(DownloadModalState {
                    custom_url: String::new(),
                    starters: crate::hf::curated_starter_models(),
                    selected_starter_idx: 0,
                    mode: DownloadModalMode::Starters,
                });
            }
            PaletteAction::ImportModel => {
                self.set_tab(HubTab::Models);
                let candidates =
                    crate::import::scan_all_candidate_locations(&self.models_view.models_dir);
                self.pending_import_modal = Some(ImportModalState {
                    candidates,
                    selected_idx: 0,
                    custom_path: String::new(),
                    in_custom_path_mode: false,
                });
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
                let _ = cmd_tx.try_send(HubCommand::OpenTargetSelection {
                    model_path: path,
                    desired_context: self.models_view.selected_context,
                });
            }
            PaletteAction::ConnectPeer(label, ep) => {
                self.chat.client = NexusClient::new(ep.clone());
                self.chat.model_name = "cluster-model".to_string();
                let mut view = self.session();
                status::on_remote_load(&mut view, &label, "cluster-model");
                self.store_session(view);
                self.chat
                    .set_target_hardware(&label, "Remote RPC".to_string());
                self.set_tab(HubTab::Chat);
                self.status_message = Some((format!("Connected to peer at {}", ep), Color::Green));
            }
        }
    }

    pub(crate) fn render_command_palette(&self, frame: &mut Frame, area: Rect) {
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
}
