use crate::control_plane::ModelCatalogResponse;
use crate::sysinfo::SystemProfile;
use crate::ui::models::{scan_models_dir, ModelEntry};
use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Gauge, List, ListItem, Paragraph},
    Frame,
};
use std::collections::HashMap;
use std::path::PathBuf;

/// Mesh-aware catalog row keyed by digest when available.
#[derive(Debug, Clone)]
pub struct CatalogRow {
    pub digest: String,
    pub filename: String,
    pub size_mb: u64,
    pub architecture: String,
    pub context_length: usize,
    pub local: Option<ModelEntry>,
    /// Display labels of nodes that hold this digest/filename.
    pub holders: Vec<String>,
    /// Peer control endpoints that advertise this digest (for [T] pull).
    pub peer_endpoints: Vec<String>,
}

/// Interactive split-pane model browser and GGUF inspection widget.
#[derive(Debug, Clone)]
pub struct ModelsView {
    pub models_dir: PathBuf,
    pub models: Vec<ModelEntry>,
    pub catalog: Vec<CatalogRow>,
    pub selected_index: usize,
    pub status_message: Option<String>,
    /// Refreshed on tick / rescan — never inside `render`.
    pub cached_profile: SystemProfile,
    /// Context size used for local loads (Models +/-).
    pub selected_context: usize,
}

impl ModelsView {
    pub fn new(models_dir: PathBuf) -> Self {
        let mut view = Self {
            models_dir,
            models: Vec::new(),
            catalog: Vec::new(),
            selected_index: 0,
            status_message: None,
            cached_profile: SystemProfile::probe(),
            selected_context: 4096,
        };
        view.refresh();
        view
    }

    pub fn refresh_profile(&mut self) {
        self.cached_profile = SystemProfile::probe();
    }

    pub fn refresh(&mut self) {
        self.models = scan_models_dir(&self.models_dir);
        self.rebuild_catalog_local_only();
        self.refresh_profile();
        if self.selected_index >= self.catalog.len() && !self.catalog.is_empty() {
            self.selected_index = self.catalog.len() - 1;
        }
    }

    fn rebuild_catalog_local_only(&mut self) {
        self.catalog = self
            .models
            .iter()
            .map(|m| CatalogRow {
                digest: m.digest.clone(),
                filename: m.filename.clone(),
                size_mb: m.size_mb,
                architecture: m.architecture.clone(),
                context_length: m.context_length,
                local: Some(m.clone()),
                holders: vec!["local".to_string()],
                peer_endpoints: Vec::new(),
            })
            .collect();
    }

    /// Merge remote peer catalogs into the mesh-wide list (keyed by digest, else filename).
    pub fn apply_remote_catalogs(&mut self, remotes: &[(String, String, ModelCatalogResponse)]) {
        // remotes: (label, control_endpoint, catalog)
        self.models = scan_models_dir(&self.models_dir);
        let mut by_key: HashMap<String, CatalogRow> = HashMap::new();

        for m in &self.models {
            let key = if m.digest.is_empty() {
                format!("name:{}", m.filename)
            } else {
                format!("digest:{}", m.digest)
            };
            by_key.insert(
                key,
                CatalogRow {
                    digest: m.digest.clone(),
                    filename: m.filename.clone(),
                    size_mb: m.size_mb,
                    architecture: m.architecture.clone(),
                    context_length: m.context_length,
                    local: Some(m.clone()),
                    holders: vec!["local".to_string()],
                    peer_endpoints: Vec::new(),
                },
            );
        }

        for (label, endpoint, catalog) in remotes {
            for entry in &catalog.models {
                let key = if entry.digest.is_empty() {
                    format!("name:{}", entry.filename)
                } else {
                    format!("digest:{}", entry.digest)
                };
                let row = by_key.entry(key).or_insert_with(|| CatalogRow {
                    digest: entry.digest.clone(),
                    filename: entry.filename.clone(),
                    size_mb: if entry.size_mb > 0 {
                        entry.size_mb
                    } else {
                        entry.size_bytes / (1024 * 1024)
                    },
                    architecture: entry.architecture.clone(),
                    context_length: entry.context_length,
                    local: None,
                    holders: Vec::new(),
                    peer_endpoints: Vec::new(),
                });
                if !row.holders.iter().any(|h| h == label) {
                    row.holders.push(label.clone());
                }
                if !entry.digest.is_empty() && !row.peer_endpoints.iter().any(|e| e == endpoint) {
                    row.peer_endpoints.push(endpoint.clone());
                }
                if row.digest.is_empty() && !entry.digest.is_empty() {
                    row.digest = entry.digest.clone();
                }
            }
        }

        let mut catalog: Vec<CatalogRow> = by_key.into_values().collect();
        catalog.sort_by(|a, b| a.filename.to_lowercase().cmp(&b.filename.to_lowercase()));
        self.catalog = catalog;
        if self.selected_index >= self.catalog.len() && !self.catalog.is_empty() {
            self.selected_index = self.catalog.len() - 1;
        }
    }

    pub fn adjust_context(&mut self, delta: i32) {
        let step = 512i32;
        let next = (self.selected_context as i32 + delta * step).clamp(512, 131_072);
        self.selected_context = next as usize;
        self.status_message = Some(format!("Context size: {} tokens", self.selected_context));
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

    pub fn selected_model(&self) -> Option<&ModelEntry> {
        self.selected_row().and_then(|r| r.local.as_ref())
    }

    pub fn selected_local_path(&self) -> Option<PathBuf> {
        self.selected_model().map(|m| m.path.clone())
    }

    pub fn download_dest_from_url(models_dir: &std::path::Path, url: &str) -> PathBuf {
        let name = url
            .rsplit('/')
            .next()
            .and_then(|s| {
                let s = s.split('?').next().unwrap_or(s);
                if s.is_empty() { None } else { Some(s) }
            })
            .unwrap_or("model.gguf");
        let name = if name.to_ascii_lowercase().ends_with(".gguf") {
            name.to_string()
        } else {
            format!("{name}.gguf")
        };
        models_dir.join(name)
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
        let items: Vec<ListItem> = if self.catalog.is_empty() {
            vec![ListItem::new(Line::from(vec![Span::styled(
                format!(
                    " No models — press [D] to download or use nexus download ({:?})",
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
                    let has_local = m.local.is_some();
                    let (badge_text, badge_color) = if has_local {
                        if m.local.as_ref().map(|e| e.lmk_compatible).unwrap_or(false) {
                            ("[OK]", Color::Green)
                        } else {
                            ("[RPC]", Color::Yellow)
                        }
                    } else {
                        ("[NET]", Color::Cyan)
                    };

                    let prefix = if is_selected { " > " } else { "   " };
                    let style = if is_selected {
                        Style::default()
                            .fg(Color::Cyan)
                            .add_modifier(Modifier::BOLD)
                    } else {
                        Style::default().fg(Color::White)
                    };

                    let holders = m.holders.join(",");
                    let line = Line::from(vec![
                        Span::styled(prefix, style),
                        Span::styled(
                            format!("{:<28}", truncate_string(&m.filename, 26)),
                            style,
                        ),
                        Span::styled(
                            format!("{:>6}MB ", m.size_mb),
                            Style::default().fg(Color::Gray),
                        ),
                        Span::styled(
                            badge_text,
                            Style::default()
                                .fg(badge_color)
                                .add_modifier(Modifier::BOLD),
                        ),
                        Span::styled(
                            format!(" {}", truncate_string(&holders, 16)),
                            Style::default().fg(Color::DarkGray),
                        ),
                    ]);

                    ListItem::new(line)
                })
                .collect()
        };

        let list_title = format!(
            " Mesh Models ({}) | Path: {:?} ",
            self.catalog.len(),
            self.models_dir
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

        if let Some(row) = self.selected_row() {
            let profile = &self.cached_profile;
            let total_ram_mb = profile.total_ram_mb;
            let avail_ram_mb = profile.available_ram_mb;
            let lmk_cap_mb = profile.max_allowed_memory_bytes() / (1024 * 1024);
            let exact_kv_mb = row.local.as_ref().map(|m| m.exact_kv_mb).unwrap_or(0);
            let required_mb = row.size_mb + exact_kv_mb;

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

            let digest_short = if row.digest.len() >= 12 {
                format!("{}…", &row.digest[..12])
            } else if row.digest.is_empty() {
                "(none)".to_string()
            } else {
                row.digest.clone()
            };

            let info_lines = vec![
                Line::from(vec![
                    Span::styled(" Model File:        ", Style::default().fg(Color::LightBlue)),
                    Span::styled(
                        &row.filename,
                        Style::default()
                            .fg(Color::White)
                            .add_modifier(Modifier::BOLD),
                    ),
                ]),
                Line::from(vec![
                    Span::styled(" Digest:            ", Style::default().fg(Color::LightBlue)),
                    Span::styled(digest_short, Style::default().fg(Color::White)),
                ]),
                Line::from(vec![
                    Span::styled(" Architecture:      ", Style::default().fg(Color::LightBlue)),
                    Span::styled(
                        &row.architecture,
                        Style::default()
                            .fg(Color::Cyan)
                            .add_modifier(Modifier::BOLD),
                    ),
                ]),
                Line::from(vec![
                    Span::styled(" Weight Size:       ", Style::default().fg(Color::LightBlue)),
                    Span::styled(
                        format!("{} MB", row.size_mb),
                        Style::default().fg(Color::White),
                    ),
                ]),
                Line::from(vec![
                    Span::styled(" Holders:           ", Style::default().fg(Color::LightBlue)),
                    Span::styled(row.holders.join(", "), Style::default().fg(Color::White)),
                ]),
                Line::from(vec![
                    Span::styled(" Context Limit:     ", Style::default().fg(Color::LightBlue)),
                    Span::styled(
                        format!("{} tokens", row.context_length),
                        Style::default().fg(Color::White),
                    ),
                ]),
                Line::from(vec![
                    Span::styled(" Selected Context:  ", Style::default().fg(Color::LightBlue)),
                    Span::styled(
                        format!("{} tokens (+/-)", self.selected_context),
                        Style::default()
                            .fg(Color::Yellow)
                            .add_modifier(Modifier::BOLD),
                    ),
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

            let status_text = if let Some(msg) = &self.status_message {
                msg.clone()
            } else {
                " [Enter] Load  [D] Download  [T] Pull  [S] Push  [+/-] Ctx  [R] Rescan "
                    .to_string()
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
            let empty_widget = Paragraph::new(
                "No model selected — press [D] to download a GGUF URL",
            )
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
