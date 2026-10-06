use crate::cluster::{ModelFit, DEFAULT_CONTEXT_SIZE};
use crate::control_plane::ModelCatalogEntry;
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
use uuid::Uuid;

/// Unified Models-tab row: local disk entry and/or remote peer catalog entry.
#[derive(Debug, Clone)]
pub struct CatalogRow {
    pub filename: String,
    pub size_mb: u64,
    pub architecture: String,
    pub context_length: usize,
    pub exact_kv_mb: u64,
    pub kv_context: usize,
    pub host_label: String,
    pub local_path: Option<PathBuf>,
    pub peer_uuid: Option<Uuid>,
    pub peer_control_endpoint: Option<String>,
}

impl CatalogRow {
    pub fn from_local(entry: ModelEntry) -> Self {
        Self {
            filename: entry.filename.clone(),
            size_mb: entry.size_mb,
            architecture: entry.architecture.clone(),
            context_length: entry.context_length,
            exact_kv_mb: entry.exact_kv_mb,
            kv_context: entry.kv_context,
            host_label: "local".to_string(),
            local_path: Some(entry.path),
            peer_uuid: None,
            peer_control_endpoint: None,
        }
    }

    pub fn from_remote(
        entry: &ModelCatalogEntry,
        host_label: String,
        peer_uuid: Uuid,
        control_endpoint: String,
    ) -> Self {
        Self {
            filename: entry.filename.clone(),
            size_mb: entry.size_mb,
            architecture: entry.architecture.clone(),
            context_length: entry.context_length,
            exact_kv_mb: entry.size_mb / 4, // coarse KV estimate when peer omits exact KV
            kv_context: DEFAULT_CONTEXT_SIZE,
            host_label,
            local_path: None,
            peer_uuid: Some(peer_uuid),
            peer_control_endpoint: Some(control_endpoint),
        }
    }

    pub fn required_mb_at(&self, context_size: usize) -> u64 {
        let base_ctx = self.kv_context.max(1);
        let kv_mb =
            ((self.exact_kv_mb as f64) * (context_size as f64) / (base_ctx as f64)).ceil() as u64;
        self.size_mb.saturating_add(kv_mb)
    }

    pub fn is_local(&self) -> bool {
        self.local_path.is_some()
    }
}

/// Interactive split-pane model browser and GGUF inspection widget.
#[derive(Debug, Clone)]
pub struct ModelsView {
    pub models_dir: PathBuf,
    /// Local-only scan (kept for Cluster L / legacy callers).
    pub models: Vec<ModelEntry>,
    /// Merged local + remote catalog shown in the list.
    pub catalog: Vec<CatalogRow>,
    pub selected_index: usize,
    pub status_message: Option<String>,
    pub cached_profile: SystemProfile,
    pub selected_context: usize,
    pub max_rpc_ram_mb: u64,
}

impl ModelsView {
    pub fn new(models_dir: PathBuf) -> Self {
        let models = scan_models_dir(&models_dir);
        let catalog = models.iter().cloned().map(CatalogRow::from_local).collect();
        Self {
            models_dir,
            models,
            catalog,
            selected_index: 0,
            status_message: None,
            cached_profile: SystemProfile::probe(),
            selected_context: DEFAULT_CONTEXT_SIZE,
            max_rpc_ram_mb: 1800,
        }
    }

    pub fn refresh(&mut self) {
        self.models = scan_models_dir(&self.models_dir);
        self.rebuild_catalog_local();
        if self.selected_index >= self.catalog.len() && !self.catalog.is_empty() {
            self.selected_index = self.catalog.len() - 1;
        }
        self.refresh_profile();
    }

    fn rebuild_catalog_local(&mut self) {
        // Preserve remote rows; replace local rows from disk scan.
        let remotes: Vec<CatalogRow> = self
            .catalog
            .iter()
            .filter(|r| !r.is_local())
            .cloned()
            .collect();
        let mut catalog: Vec<CatalogRow> = self
            .models
            .iter()
            .cloned()
            .map(CatalogRow::from_local)
            .collect();
        catalog.extend(remotes);
        catalog.sort_by(|a, b| {
            a.filename
                .cmp(&b.filename)
                .then_with(|| a.host_label.cmp(&b.host_label))
        });
        self.catalog = catalog;
    }

    /// Replace remote catalog rows from peer fetch results.
    pub fn apply_remote_catalogs(&mut self, remotes: Vec<CatalogRow>) {
        let locals: Vec<CatalogRow> = self
            .catalog
            .iter()
            .filter(|r| r.is_local())
            .cloned()
            .collect();
        let mut catalog = locals;
        catalog.extend(remotes);
        catalog.sort_by(|a, b| {
            a.filename
                .cmp(&b.filename)
                .then_with(|| a.host_label.cmp(&b.host_label))
        });
        self.catalog = catalog;
        if self.selected_index >= self.catalog.len() && !self.catalog.is_empty() {
            self.selected_index = self.catalog.len() - 1;
        }
    }

    pub fn refresh_profile(&mut self) {
        self.cached_profile = SystemProfile::probe();
    }

    pub fn next(&mut self) {
        if !self.catalog.is_empty() {
            self.selected_index = (self.selected_index + 1) % self.catalog.len();
        }
    }

    pub fn previous(&mut self) {
        if !self.catalog.is_empty() {
            if self.selected_index == 0 {
                self.selected_index = self.catalog.len() - 1;
            } else {
                self.selected_index -= 1;
            }
        }
    }

    pub fn selected_row(&self) -> Option<&CatalogRow> {
        self.catalog.get(self.selected_index)
    }

    /// Local ModelEntry for the current selection (None if remote-only).
    pub fn selected_model(&self) -> Option<&ModelEntry> {
        let row = self.selected_row()?;
        let path = row.local_path.as_ref()?;
        self.models.iter().find(|m| &m.path == path)
    }

    pub fn cluster_caps_mb(&self) -> (u64, u64) {
        let host = self.cached_profile.max_allowed_memory_bytes() / (1024 * 1024);
        let cluster = host.saturating_add(self.max_rpc_ram_mb);
        (host, cluster)
    }

    pub fn fit_for_row(&self, row: &CatalogRow) -> ModelFit {
        let required = row.required_mb_at(self.selected_context);
        let (host, cluster) = self.cluster_caps_mb();
        ModelFit::classify(required, host, cluster)
    }

    pub fn fit_for(&self, model: &ModelEntry) -> ModelFit {
        let required = model.required_mb_at(self.selected_context);
        let (host, cluster) = self.cluster_caps_mb();
        ModelFit::classify(required, host, cluster)
    }

    pub fn render(&self, frame: &mut Frame, area: Rect) {
        let chunks = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(55), Constraint::Percentage(45)])
            .split(area);

        self.render_model_list(frame, chunks[0]);
        self.render_model_details(frame, chunks[1]);
    }

    fn render_model_list(&self, frame: &mut Frame, area: Rect) {
        let items: Vec<ListItem> = if self.catalog.is_empty() {
            vec![ListItem::new(Line::from(vec![Span::styled(
                format!(
                    " No .gguf models found in {:?} — press D to download, or nexus download --help",
                    self.models_dir
                ),
                Style::default().fg(Color::DarkGray),
            )]))]
        } else {
            self.catalog
                .iter()
                .enumerate()
                .map(|(i, m)| {
                    let is_selected = i == self.selected_index;
                    let fit = self.fit_for_row(m);
                    let badge_text = fit.list_badge();
                    let badge_color = match fit {
                        ModelFit::Fits => Color::Green,
                        ModelFit::NeedsRpc => Color::Yellow,
                        ModelFit::WontFit => Color::Red,
                    };

                    let prefix = if is_selected { " > " } else { "   " };
                    let style = if is_selected {
                        Style::default()
                            .fg(Color::Cyan)
                            .add_modifier(Modifier::BOLD)
                    } else {
                        Style::default().fg(Color::White)
                    };

                    let line = Line::from(vec![
                        Span::styled(prefix, style),
                        Span::styled(
                            format!("{:<24}", truncate_string(&m.filename, 22)),
                            style,
                        ),
                        Span::styled(
                            format!("{:<10}", truncate_string(&m.host_label, 9)),
                            Style::default().fg(Color::Magenta),
                        ),
                        Span::styled(
                            format!("{:>6} MB ", m.size_mb),
                            Style::default().fg(Color::Gray),
                        ),
                        Span::styled(
                            badge_text,
                            Style::default()
                                .fg(badge_color)
                                .add_modifier(Modifier::BOLD),
                        ),
                    ]);

                    ListItem::new(line)
                })
                .collect()
        };

        let local_n = self.catalog.iter().filter(|r| r.is_local()).count();
        let remote_n = self.catalog.len().saturating_sub(local_n);
        let list_title = format!(
            " Models (local {} / remote {}) | {:?} ",
            local_n, remote_n, self.models_dir
        );
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
                Constraint::Min(12),
                Constraint::Length(5),
                Constraint::Length(4),
            ])
            .split(area);

        if let Some(m) = self.selected_row() {
            let meta = m
                .local_path
                .as_ref()
                .and_then(|p| GgufMetadata::open(p).ok());
            let block_count = meta.as_ref().and_then(|g| g.block_count).unwrap_or(0);
            let head_count = meta.as_ref().and_then(|g| g.head_count).unwrap_or(0);
            let embed_len = meta.as_ref().and_then(|g| g.embedding_length).unwrap_or(0);
            let version = meta.as_ref().map(|g| g.version).unwrap_or(3);

            let profile = &self.cached_profile;
            let total_ram_mb = profile.total_ram_mb;
            let avail_ram_mb = profile.available_ram_mb;
            let lmk_cap_mb = profile.max_allowed_memory_bytes() / (1024 * 1024);
            let kv_mb = m.required_mb_at(self.selected_context).saturating_sub(m.size_mb);
            let required_mb = m.required_mb_at(self.selected_context);
            let fit = self.fit_for_row(m);

            let ram_ratio = if lmk_cap_mb > 0 {
                ((required_mb as f64) / (lmk_cap_mb as f64)).min(1.0)
            } else {
                0.0
            };

            let gauge_color = match fit {
                ModelFit::Fits => Color::Green,
                ModelFit::NeedsRpc => Color::Yellow,
                ModelFit::WontFit => Color::Red,
            };

            let info_lines = vec![
                Line::from(vec![
                    Span::styled(" Model File:        ", Style::default().fg(Color::LightBlue)),
                    Span::styled(
                        &m.filename,
                        Style::default()
                            .fg(Color::White)
                            .add_modifier(Modifier::BOLD),
                    ),
                ]),
                Line::from(vec![
                    Span::styled(" Host:              ", Style::default().fg(Color::LightBlue)),
                    Span::styled(
                        &m.host_label,
                        Style::default()
                            .fg(Color::Magenta)
                            .add_modifier(Modifier::BOLD),
                    ),
                ]),
                Line::from(vec![
                    Span::styled(" Format Version:    ", Style::default().fg(Color::LightBlue)),
                    Span::styled(format!("GGUF v{}", version), Style::default().fg(Color::White)),
                ]),
                Line::from(vec![
                    Span::styled(" Architecture:      ", Style::default().fg(Color::LightBlue)),
                    Span::styled(
                        &m.architecture,
                        Style::default()
                            .fg(Color::Cyan)
                            .add_modifier(Modifier::BOLD),
                    ),
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
                    Span::styled(
                        format!("{} tokens", m.context_length),
                        Style::default().fg(Color::White),
                    ),
                ]),
                Line::from(vec![
                    Span::styled(" Selected Context:  ", Style::default().fg(Color::LightBlue)),
                    Span::styled(
                        format!("{} tokens [+/-]", self.selected_context),
                        Style::default()
                            .fg(Color::Yellow)
                            .add_modifier(Modifier::BOLD),
                    ),
                ]),
                Line::from(vec![
                    Span::styled(
                        format!(" Exact KV Cache ({}):", format_ctx_short(self.selected_context)),
                        Style::default().fg(Color::LightBlue),
                    ),
                    Span::styled(format!("{} MB", kv_mb), Style::default().fg(Color::White)),
                ]),
            ];

            let meta_widget = Paragraph::new(info_lines).block(
                Block::default()
                    .title(" Model Architecture & Metadata ")
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(Color::DarkGray)),
            );
            frame.render_widget(meta_widget, right_chunks[0]);

            let gauge_label = format!(
                "{} MB / {} MB Cap (Available: {} MB of {} MB) {}",
                required_mb,
                lmk_cap_mb,
                avail_ram_mb,
                total_ram_mb,
                fit.modal_badge()
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

            let status_text = if let Some(msg) = &self.status_message {
                msg.clone()
            } else {
                format!(
                    " [Enter] Target | [D] Download | [+/-] Ctx={} | [u] Unload | [P] Persona | [R] Rescan | [?] Help ",
                    self.selected_context
                )
            };

            let action_widget = Paragraph::new(Line::from(vec![Span::styled(
                status_text,
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            )]))
            .block(
                Block::default()
                    .title(" Actions ")
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(Color::DarkGray)),
            );
            frame.render_widget(action_widget, right_chunks[2]);
        } else {
            let empty_widget = Paragraph::new(Line::from(vec![Span::styled(
                " No model selected — press D to download ",
                Style::default().fg(Color::DarkGray),
            )]))
            .block(Block::default().title(" Details ").borders(Borders::ALL));
            frame.render_widget(empty_widget, area);
        }
    }
}

fn format_ctx_short(ctx: usize) -> String {
    if ctx >= 1024 && ctx % 1024 == 0 {
        format!("{}k", ctx / 1024)
    } else {
        format!("{}", ctx)
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

/// Derive a destination filename under `models_dir` from a download URL.
pub fn download_dest_from_url(models_dir: &std::path::Path, url: &str) -> PathBuf {
    let name = url
        .split('?')
        .next()
        .unwrap_or(url)
        .rsplit('/')
        .next()
        .unwrap_or("model.gguf");
    let filename = if name.to_ascii_lowercase().ends_with(".gguf") {
        name.to_string()
    } else {
        format!("{}.gguf", if name.is_empty() { "downloaded" } else { name })
    };
    models_dir.join(filename)
}
