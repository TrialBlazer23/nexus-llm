//! Modal views and states for the Unified Nexus Hub TUI.

use ratatui::{
    layout::{Alignment, Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, Gauge, List, ListItem, Paragraph},
    Frame,
};
use std::path::PathBuf;
use uuid::Uuid;

use super::{keymap, render::centered_rect, HubApp};

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
    LocalMoeStream {
        allocatable_mb: u64,
        cache_mb: u64,
        ceil_mb: u64,
        /// Planned context size from the joint MoE knob planner.
        context_size: usize,
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
    RemoteMoeStream {
        uuid: Uuid,
        name: String,
        endpoint: String,
        api_endpoint: String,
        free_ram_mb: u32,
        cache_mb: u64,
        ceil_mb: u64,
        context_size: usize,
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
            Self::LocalMoeStream {
                allocatable_mb,
                cache_mb,
                ceil_mb,
                context_size,
                predicted_label,
            } => {
                format!(
                    "💾 Local MoE stream (ctx {} | cache {} / ceil {} MB) - {} MB LMK | {}",
                    context_size, cache_mb, ceil_mb, allocatable_mb, predicted_label
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
            Self::RemoteMoeStream {
                name,
                cache_mb,
                ceil_mb,
                context_size,
                free_ram_mb,
                predicted_label,
                ..
            } => {
                format!(
                    "💾 {} MoE stream (ctx {} | cache {} / ceil {} MB) - {} MB free | {}",
                    name, context_size, cache_mb, ceil_mb, free_ram_mb, predicted_label
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
    /// Planned BigMoe cache ceiling when loading via MoE stream (`None` = derive).
    pub moe_cache_ceil_mb: Option<u64>,
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DownloadModalMode {
    Starters,
    CustomUrl,
}

#[derive(Debug, Clone)]
pub struct DownloadModalState {
    pub custom_url: String,
    pub starters: Vec<crate::hf::CuratedModel>,
    pub selected_starter_idx: usize,
    pub mode: DownloadModalMode,
}

#[derive(Debug, Clone)]
pub struct ImportModalState {
    pub candidates: Vec<crate::import::ImportCandidate>,
    pub selected_idx: usize,
    pub custom_path: String,
    pub in_custom_path_mode: bool,
}

impl HubApp {
    pub(crate) fn render_download_progress(
        &self,
        frame: &mut Frame,
        area: Rect,
        progress: &(String, Option<f32>, u64, Option<u64>, f64),
    ) {
        let (label, percent, downloaded, total, speed) = progress;
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

    pub(crate) fn render_delete_model_modal(
        &self,
        frame: &mut Frame,
        area: Rect,
        data: &(crate::ui::models::ModelEntry, Vec<PathBuf>, u64),
    ) {
        let (entry, shards, total_bytes) = data;
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

    pub(crate) fn render_download_modal(
        &self,
        frame: &mut Frame,
        area: Rect,
        modal_state: &DownloadModalState,
    ) {
        let modal = centered_rect(82, 60, area);
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

        let header_block = Block::default()
            .title(" Download Model / Hugging Face Resolve ")
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::Cyan));
        frame.render_widget(header_block, modal);

        let tab_starters = if modal_state.mode == DownloadModalMode::Starters {
            Span::styled(
                " [1] Curated Starters (1-Click) ",
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            )
        } else {
            Span::styled(
                " [1] Curated Starters ",
                Style::default().fg(Color::DarkGray),
            )
        };
        let tab_custom = if modal_state.mode == DownloadModalMode::CustomUrl {
            Span::styled(
                " [2] Custom URL / HF Repo ",
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            )
        } else {
            Span::styled(
                " [2] Custom URL / HF Repo ",
                Style::default().fg(Color::DarkGray),
            )
        };
        let tabs_line = Line::from(vec![
            Span::raw(" Mode: "),
            tab_starters,
            Span::raw(" | "),
            tab_custom,
            Span::styled(
                "   (Press [Tab] to switch)",
                Style::default().fg(Color::DarkGray),
            ),
        ]);
        frame.render_widget(Paragraph::new(tabs_line), chunks[0]);

        match modal_state.mode {
            DownloadModalMode::Starters => {
                let items: Vec<ListItem> = modal_state
                    .starters
                    .iter()
                    .enumerate()
                    .map(|(i, s)| {
                        let is_sel = i == modal_state.selected_starter_idx;
                        let cursor = if is_sel { "> " } else { "  " };
                        let name_style = if is_sel {
                            Style::default()
                                .fg(Color::Yellow)
                                .add_modifier(Modifier::BOLD)
                        } else {
                            Style::default().fg(Color::White)
                        };
                        let size_gb = format!("{:.1} GB", s.approx_size_mb as f64 / 1024.0);
                        let line1 = Line::from(vec![
                            Span::styled(cursor, Style::default().fg(Color::Yellow)),
                            Span::styled(s.name, name_style),
                            Span::styled(
                                format!(" [{}] ", s.quant_label),
                                Style::default().fg(Color::Cyan),
                            ),
                            Span::styled(
                                format!("({})", size_gb),
                                Style::default().fg(Color::Green),
                            ),
                        ]);
                        let line2 = Line::from(vec![
                            Span::raw("    "),
                            Span::styled(s.description, Style::default().fg(Color::DarkGray)),
                        ]);
                        ListItem::new(vec![line1, line2])
                    })
                    .collect();
                let list = List::new(items).block(Block::default().borders(Borders::NONE));
                frame.render_widget(list, chunks[1]);

                let footer = Paragraph::new(Line::from(vec![Span::styled(
                    " [↑/↓] Navigate   [Enter] Download Starter   [Tab] Custom URL   [Esc] Cancel ",
                    Style::default().fg(Color::Yellow),
                )]))
                .alignment(Alignment::Center);
                frame.render_widget(footer, chunks[2]);
            }
            DownloadModalMode::CustomUrl => {
                let input_p = Paragraph::new(vec![
                    Line::from(Span::styled(" Enter Hugging Face repo ID or direct GGUF URL:", Style::default().fg(Color::LightBlue))),
                    Line::from(Span::styled("   e.g. bartowski/Llama-3.2-3B-Instruct-GGUF or https://hf.co/...", Style::default().fg(Color::DarkGray))),
                    Line::from(""),
                    Line::from(vec![
                        Span::styled(" > ", Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)),
                        Span::styled(&modal_state.custom_url, Style::default().fg(Color::White).add_modifier(Modifier::BOLD)),
                        Span::styled("_", Style::default().fg(Color::Cyan)),
                    ]),
                    Line::from(""),
                    Line::from(Span::styled(" • Hugging Face /blob/ URLs are automatically rewritten to direct /resolve/ streams.", Style::default().fg(Color::DarkGray))),
                    Line::from(Span::styled(" • HTML pages or non-GGUF payloads are rejected immediately to prevent 0-byte models.", Style::default().fg(Color::DarkGray))),
                ]);
                frame.render_widget(input_p, chunks[1]);

                let footer = Paragraph::new(Line::from(vec![Span::styled(
                    " [Enter] Download / Resolve   [Tab] Curated Starters   [Esc] Cancel ",
                    Style::default().fg(Color::Yellow),
                )]))
                .alignment(Alignment::Center);
                frame.render_widget(footer, chunks[2]);
            }
        }
    }

    pub(crate) fn render_import_modal(
        &self,
        frame: &mut Frame,
        area: Rect,
        modal_state: &ImportModalState,
    ) {
        let modal = centered_rect(84, 65, area);
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

        let header_block = Block::default()
            .title(" Import Local Model from Storage ")
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::Green));
        frame.render_widget(header_block, modal);

        let tab_discovered = if !modal_state.in_custom_path_mode {
            Span::styled(
                " [1] Discovered Files ",
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            )
        } else {
            Span::styled(
                " [1] Discovered Files ",
                Style::default().fg(Color::DarkGray),
            )
        };
        let tab_custom = if modal_state.in_custom_path_mode {
            Span::styled(
                " [2] Custom Path Input ",
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            )
        } else {
            Span::styled(
                " [2] Custom Path Input ",
                Style::default().fg(Color::DarkGray),
            )
        };
        let tabs_line = Line::from(vec![
            Span::raw(" View: "),
            tab_discovered,
            Span::raw(" | "),
            tab_custom,
            Span::styled(
                "   (Press [Tab] to switch)",
                Style::default().fg(Color::DarkGray),
            ),
        ]);
        frame.render_widget(Paragraph::new(tabs_line), chunks[0]);

        if !modal_state.in_custom_path_mode {
            if modal_state.candidates.is_empty() {
                let empty_msg = Paragraph::new(vec![
                    Line::from(""),
                    Line::from(Span::styled(
                        " No .gguf models found in standard download locations.",
                        Style::default().fg(Color::Yellow),
                    )),
                    Line::from(Span::styled(
                        " Scanned: /sdcard/Download, ~/Downloads, ~/storage/downloads",
                        Style::default().fg(Color::DarkGray),
                    )),
                    Line::from(""),
                    Line::from(Span::styled(
                        " Press [Tab] to type a custom file or directory path.",
                        Style::default().fg(Color::Cyan),
                    )),
                ])
                .alignment(Alignment::Center);
                frame.render_widget(empty_msg, chunks[1]);
            } else {
                let items: Vec<ListItem> = modal_state
                    .candidates
                    .iter()
                    .enumerate()
                    .map(|(i, c)| {
                        let is_sel = i == modal_state.selected_idx;
                        let cursor = if is_sel { "> " } else { "  " };
                        let (badge_text, badge_color) = if !c.is_valid_gguf {
                            ("[CORRUPT]", Color::Red)
                        } else {
                            match c.fit_status {
                                crate::hf::FitStatus::Fits => ("[OK]", Color::Green),
                                crate::hf::FitStatus::OffloadRequired => ("[RPC]", Color::Yellow),
                                crate::hf::FitStatus::Exceeds => ("[EXCEEDS]", Color::Red),
                            }
                        };
                        let size_str = format!("{:.1} GB", c.size_mb as f64 / 1024.0);
                        let exist_str = if c.already_in_models_dir {
                            " (already in models)"
                        } else {
                            ""
                        };
                        let arch_str = c.architecture.as_deref().unwrap_or("unknown");
                        let name_style = if is_sel {
                            Style::default()
                                .fg(Color::Yellow)
                                .add_modifier(Modifier::BOLD)
                        } else {
                            Style::default().fg(Color::White)
                        };

                        let line1 = Line::from(vec![
                            Span::styled(cursor, Style::default().fg(Color::Yellow)),
                            Span::styled(
                                badge_text,
                                Style::default()
                                    .fg(badge_color)
                                    .add_modifier(Modifier::BOLD),
                            ),
                            Span::styled(format!(" {} ", c.filename), name_style),
                            Span::styled(
                                format!("({} | {})", size_str, arch_str),
                                Style::default().fg(Color::DarkGray),
                            ),
                            Span::styled(exist_str, Style::default().fg(Color::DarkGray)),
                        ]);
                        let line2 = Line::from(vec![
                            Span::raw("    "),
                            Span::styled(
                                c.source_path.to_string_lossy().to_string(),
                                Style::default().fg(Color::DarkGray),
                            ),
                        ]);
                        ListItem::new(vec![line1, line2])
                    })
                    .collect();
                let list = List::new(items).block(Block::default().borders(Borders::NONE));
                frame.render_widget(list, chunks[1]);
            }

            let footer = Paragraph::new(Line::from(vec![Span::styled(
                " [↑/↓] Select   [Enter] Import Selected   [Tab] Custom Path   [Esc] Cancel ",
                Style::default().fg(Color::Yellow),
            )]))
            .alignment(Alignment::Center);
            frame.render_widget(footer, chunks[2]);
        } else {
            let input_p = Paragraph::new(vec![
                Line::from(Span::styled(" Enter absolute path to a .gguf file or directory to scan:", Style::default().fg(Color::LightBlue))),
                Line::from(""),
                Line::from(vec![
                    Span::styled(" > ", Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)),
                    Span::styled(&modal_state.custom_path, Style::default().fg(Color::White).add_modifier(Modifier::BOLD)),
                    Span::styled("_", Style::default().fg(Color::Cyan)),
                ]),
                Line::from(""),
                Line::from(Span::styled(" • If pointing to a .gguf file, it will be imported directly (symlinked with copy fallback).", Style::default().fg(Color::DarkGray))),
                Line::from(Span::styled(" • If pointing to a directory, it will be scanned for GGUF candidates.", Style::default().fg(Color::DarkGray))),
            ]);
            frame.render_widget(input_p, chunks[1]);

            let footer = Paragraph::new(Line::from(vec![Span::styled(
                " [Enter] Import / Scan   [Tab] Discovered Files   [Esc] Cancel ",
                Style::default().fg(Color::Yellow),
            )]))
            .alignment(Alignment::Center);
            frame.render_widget(footer, chunks[2]);
        }
    }

    pub(crate) fn render_hf_quant_picker(
        &self,
        frame: &mut Frame,
        area: Rect,
        picker: &HfQuantPickerState,
    ) {
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

    pub(crate) fn render_hf_auth_recovery(
        &self,
        frame: &mut Frame,
        area: Rect,
        recovery: &HfAuthRecoveryState,
    ) {
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

    pub(crate) fn render_push_peers(
        &self,
        frame: &mut Frame,
        area: Rect,
        peers: &[(String, String)],
    ) {
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

    pub(crate) fn render_load_phase(&self, frame: &mut Frame, area: Rect, phase: &str) {
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
                phase.to_string(),
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

    pub(crate) fn render_hot_swap_modal(
        &self,
        frame: &mut Frame,
        area: Rect,
        intent: &HotSwapIntent,
    ) {
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

    pub(crate) fn render_target_selection_modal(
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

    pub(crate) fn render_help_modal(&self, frame: &mut Frame, area: Rect) {
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
