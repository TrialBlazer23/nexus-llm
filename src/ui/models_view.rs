use crate::gguf::GgufMetadata;
use crate::sysinfo::SystemProfile;
use crate::ui::models::{scan_models_dir, ModelEntry};
use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Gauge, List, ListItem, Paragraph},
    Frame,
};
use std::path::PathBuf;

/// Interactive split-pane model browser and GGUF inspection widget.
#[derive(Debug, Clone)]
pub struct ModelsView {
    pub models_dir: PathBuf,
    pub models: Vec<ModelEntry>,
    pub selected_index: usize,
    pub status_message: Option<String>,
}

impl ModelsView {
    pub fn new(models_dir: PathBuf) -> Self {
        let models = scan_models_dir(&models_dir);
        Self {
            models_dir,
            models,
            selected_index: 0,
            status_message: None,
        }
    }

    pub fn refresh(&mut self) {
        self.models = scan_models_dir(&self.models_dir);
        if self.selected_index >= self.models.len() && !self.models.is_empty() {
            self.selected_index = self.models.len() - 1;
        }
    }

    pub fn next(&mut self) {
        if !self.models.is_empty() {
            self.selected_index = (self.selected_index + 1) % self.models.len();
        }
    }

    pub fn previous(&mut self) {
        if !self.models.is_empty() {
            if self.selected_index == 0 {
                self.selected_index = self.models.len() - 1;
            } else {
                self.selected_index -= 1;
            }
        }
    }

    pub fn selected_model(&self) -> Option<&ModelEntry> {
        self.models.get(self.selected_index)
    }

    pub fn render(&self, frame: &mut Frame, area: Rect) {
        let chunks = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
            .split(area);

        self.render_model_list(frame, chunks[0]);
        self.render_model_details(frame, chunks[1]);
    }

    fn render_model_list(&self, frame: &mut Frame, area: Rect) {
        let items: Vec<ListItem> = if self.models.is_empty() {
            vec![ListItem::new(Line::from(vec![Span::styled(
                format!(" No .gguf models found in {:?}", self.models_dir),
                Style::default().fg(Color::DarkGray),
            )]))]
        } else {
            self.models
                .iter()
                .enumerate()
                .map(|(i, m)| {
                    let is_selected = i == self.selected_index;
                    let (badge_text, badge_color) = if m.lmk_compatible {
                        ("[OK]", Color::Green)
                    } else if m.size_mb <= 10300 {
                        ("[RPC]", Color::Yellow)
                    } else {
                        ("[OOM]", Color::Red)
                    };

                    let prefix = if is_selected { " > " } else { "   " };
                    let style = if is_selected {
                        Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)
                    } else {
                        Style::default().fg(Color::White)
                    };

                    let line = Line::from(vec![
                        Span::styled(prefix, style),
                        Span::styled(format!("{:<32}", truncate_string(&m.filename, 30)), style),
                        Span::styled(format!("{:>6} MB ", m.size_mb), Style::default().fg(Color::Gray)),
                        Span::styled(badge_text, Style::default().fg(badge_color).add_modifier(Modifier::BOLD)),
                    ]);

                    ListItem::new(line)
                })
                .collect()
        };

        let list_title = format!(" Local Models ({}) | Path: {:?} ", self.models.len(), self.models_dir);
        let list_widget = List::new(items).block(
            Block::default()
                .title(list_title)
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::DarkGray)),
        );

        frame.render_widget(list_widget, area);
    }

    fn render_model_details(&self, frame: &mut Frame, area: Rect) {
        let right_chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Min(12),   // Metadata attributes
                Constraint::Length(5),  // RAM Health & LMK Gauge
                Constraint::Length(4),  // Action bar
            ])
            .split(area);

        if let Some(m) = self.selected_model() {
            let meta = GgufMetadata::open(&m.path).ok();
            let block_count = meta.as_ref().and_then(|g| g.block_count).unwrap_or(0);
            let head_count = meta.as_ref().and_then(|g| g.head_count).unwrap_or(0);
            let embed_len = meta.as_ref().and_then(|g| g.embedding_length).unwrap_or(0);
            let version = meta.as_ref().map(|g| g.version).unwrap_or(3);

            let profile = SystemProfile::probe();
            let total_ram_mb = profile.total_ram_mb;
            let avail_ram_mb = profile.available_ram_mb;
            let lmk_cap_mb = profile.max_allowed_memory_bytes() / (1024 * 1024);
            let required_mb = m.size_mb + m.exact_kv_mb;

            let ram_ratio = if lmk_cap_mb > 0 {
                ((required_mb as f64) / (lmk_cap_mb as f64)).min(1.0)
            } else {
                0.0
            };

            let gauge_color = if required_mb <= lmk_cap_mb {
                Color::Green
            } else if required_mb <= 10300 {
                Color::Yellow
            } else {
                Color::Red
            };

            let info_lines = vec![
                Line::from(vec![
                    Span::styled(" Model File:        ", Style::default().fg(Color::LightBlue)),
                    Span::styled(&m.filename, Style::default().fg(Color::White).add_modifier(Modifier::BOLD)),
                ]),
                Line::from(vec![
                    Span::styled(" Format Version:    ", Style::default().fg(Color::LightBlue)),
                    Span::styled(format!("GGUF v{}", version), Style::default().fg(Color::White)),
                ]),
                Line::from(vec![
                    Span::styled(" Architecture:      ", Style::default().fg(Color::LightBlue)),
                    Span::styled(&m.architecture, Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)),
                ]),
                Line::from(vec![
                    Span::styled(" Weight Size:       ", Style::default().fg(Color::LightBlue)),
                    Span::styled(format!("{} MB", m.size_mb), Style::default().fg(Color::White)),
                ]),
                Line::from(vec![
                    Span::styled(" Transformer Layers:", Style::default().fg(Color::LightBlue)),
                    Span::styled(format!("{}", block_count), Style::default().fg(Color::White)),
                ]),
                Line::from(vec![
                    Span::styled(" Attention Heads:   ", Style::default().fg(Color::LightBlue)),
                    Span::styled(format!("{}", head_count), Style::default().fg(Color::White)),
                ]),
                Line::from(vec![
                    Span::styled(" Embedding Length:  ", Style::default().fg(Color::LightBlue)),
                    Span::styled(format!("{}", embed_len), Style::default().fg(Color::White)),
                ]),
                Line::from(vec![
                    Span::styled(" Context Limit:     ", Style::default().fg(Color::LightBlue)),
                    Span::styled(format!("{} tokens", m.context_length), Style::default().fg(Color::White)),
                ]),
                Line::from(vec![
                    Span::styled(" Exact KV Cache (4k):", Style::default().fg(Color::LightBlue)),
                    Span::styled(format!("{} MB", m.exact_kv_mb), Style::default().fg(Color::White)),
                ]),
            ];

            let meta_widget = Paragraph::new(info_lines).block(
                Block::default()
                    .title(" Model Architecture & Metadata ")
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(Color::DarkGray)),
            );
            frame.render_widget(meta_widget, right_chunks[0]);

            // RAM Health Block
            let gauge_label = format!(
                "{} MB / {} MB Cap (Available: {} MB of {} MB)",
                required_mb, lmk_cap_mb, avail_ram_mb, total_ram_mb
            );

            let gauge_widget = Gauge::default()
                .block(
                    Block::default()
                        .title(" Android LMK 75% Safety Guard ")
                        .borders(Borders::ALL)
                        .border_style(Style::default().fg(Color::DarkGray)),
                )
                .gauge_style(Style::default().fg(gauge_color).bg(Color::DarkGray))
                .ratio(ram_ratio)
                .label(gauge_label);
            frame.render_widget(gauge_widget, right_chunks[1]);

            // Action / Status bar
            let status_text = if let Some(msg) = &self.status_message {
                msg.clone()
            } else if m.lmk_compatible {
                " [Enter] Load & Chat  |  [P] Switch Preset Persona  |  [R] Rescan ".to_string()
            } else if required_mb <= 10300 {
                " [Enter] Offload to Node B (RPC)  |  [P] Preset  |  [R] Rescan ".to_string()
            } else {
                " [!] Exceeds Memory Budget  |  [R] Rescan ".to_string()
            };

            let action_widget = Paragraph::new(Line::from(vec![Span::styled(
                status_text,
                Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD),
            )]))
            .block(
                Block::default()
                    .title(" Actions ")
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(Color::DarkGray)),
            );
            frame.render_widget(action_widget, right_chunks[2]);
        } else {
            let empty_widget = Paragraph::new("No model selected")
                .block(Block::default().title(" Details ").borders(Borders::ALL));
            frame.render_widget(empty_widget, area);
        }
    }
}

fn truncate_string(s: &str, max_chars: usize) -> String {
    if s.chars().count() <= max_chars {
        s.to_string()
    } else {
        let mut truncated: String = s.chars().take(max_chars - 3).collect();
        truncated.push_str("...");
        truncated
    }
}
