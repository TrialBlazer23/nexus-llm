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

/// Mode of the Models tab (Local mesh catalog vs Hugging Face explorer).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ModelsTabMode {
    #[default]
    Local,
    HfExplorer,
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
    pub layout_mode: crate::ui::layout::LayoutMode,
    /// Active tab mode: Local catalog or Hugging Face Explorer.
    pub mode: ModelsTabMode,
    /// Cached Hugging Face models (trending or search results).
    pub hf_models: Vec<crate::hf::HfModelSummary>,
    pub hf_selected_idx: usize,
    pub hf_search_query: String,
    pub hf_is_searching: bool,
    pub hf_loading: bool,
    pub corrupted_models: Vec<PathBuf>,
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
            layout_mode: crate::ui::layout::LayoutMode::Auto,
            mode: ModelsTabMode::Local,
            hf_models: Vec::new(),
            hf_selected_idx: 0,
            hf_search_query: String::new(),
            hf_is_searching: false,
            hf_loading: false,
            corrupted_models: Vec::new(),
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
        self.corrupted_models = crate::import::find_corrupted_models(&self.models_dir);
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
        catalog.sort_by_key(|a| a.filename.to_lowercase());
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

    pub fn toggle_mode(&mut self) -> ModelsTabMode {
        self.mode = match self.mode {
            ModelsTabMode::Local => ModelsTabMode::HfExplorer,
            ModelsTabMode::HfExplorer => {
                self.hf_is_searching = false;
                ModelsTabMode::Local
            }
        };
        self.mode
    }

    pub fn hf_next(&mut self) {
        if !self.hf_models.is_empty() {
            self.hf_selected_idx = (self.hf_selected_idx + 1) % self.hf_models.len();
        }
    }

    pub fn hf_previous(&mut self) {
        if !self.hf_models.is_empty() {
            if self.hf_selected_idx == 0 {
                self.hf_selected_idx = self.hf_models.len() - 1;
            } else {
                self.hf_selected_idx -= 1;
            }
        }
    }

    pub fn selected_hf_model(&self) -> Option<&crate::hf::HfModelSummary> {
        self.hf_models.get(self.hf_selected_idx)
    }

    pub fn set_hf_models(&mut self, models: Vec<crate::hf::HfModelSummary>) {
        self.hf_models = models;
        self.hf_selected_idx = 0;
        self.hf_loading = false;
    }

    pub fn download_dest_from_url(models_dir: &std::path::Path, url: &str) -> PathBuf {
        let name = url
            .rsplit('/')
            .next()
            .and_then(|s| {
                let s = s.split('?').next().unwrap_or(s);
                if s.is_empty() {
                    None
                } else {
                    Some(s)
                }
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
        match self.mode {
            ModelsTabMode::Local => self.render_local(frame, area),
            ModelsTabMode::HfExplorer => self.render_hf_explorer(frame, area),
        }
    }

    fn render_local(&self, frame: &mut Frame, area: Rect) {
        let is_compact = self.layout_mode.is_compact(area);
        let chunks = if is_compact {
            Layout::default()
                .direction(Direction::Vertical)
                .constraints([Constraint::Percentage(55), Constraint::Percentage(45)])
                .split(area)
        } else {
            Layout::default()
                .direction(Direction::Horizontal)
                .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
                .split(area)
        };

        self.render_model_list(frame, chunks[0]);
        self.render_model_details(frame, chunks[1]);
    }

    fn render_model_list(&self, frame: &mut Frame, area: Rect) {
        let mut items: Vec<ListItem> = if self.catalog.is_empty() {
            vec![ListItem::new(Line::from(vec![Span::styled(
                " No models — press [D] Download, [I] Import local, or [E] HF Explorer",
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
                        Span::styled(format!("{:<28}", truncate_string(&m.filename, 26)), style),
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

        if !self.corrupted_models.is_empty() {
            items.push(ListItem::new(Line::from(vec![Span::styled(
                format!(
                    " [!] {} corrupted / non-GGUF file(s) found — press [Shift+X] to clean",
                    self.corrupted_models.len()
                ),
                Style::default()
                    .fg(Color::LightRed)
                    .add_modifier(Modifier::BOLD),
            )])));
        }

        let list_title = format!(
            " [• Local Catalog]  [E: HF Explorer] | Mesh Models ({}) | Path: {:?} ",
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
                    Span::styled(
                        " Model File:        ",
                        Style::default().fg(Color::LightBlue),
                    ),
                    Span::styled(
                        &row.filename,
                        Style::default()
                            .fg(Color::White)
                            .add_modifier(Modifier::BOLD),
                    ),
                ]),
                Line::from(vec![
                    Span::styled(
                        " Digest:            ",
                        Style::default().fg(Color::LightBlue),
                    ),
                    Span::styled(digest_short, Style::default().fg(Color::White)),
                ]),
                Line::from(vec![
                    Span::styled(
                        " Architecture:      ",
                        Style::default().fg(Color::LightBlue),
                    ),
                    Span::styled(
                        &row.architecture,
                        Style::default()
                            .fg(Color::Cyan)
                            .add_modifier(Modifier::BOLD),
                    ),
                ]),
                Line::from(vec![
                    Span::styled(
                        " Weight Size:       ",
                        Style::default().fg(Color::LightBlue),
                    ),
                    Span::styled(
                        format!("{} MB", row.size_mb),
                        Style::default().fg(Color::White),
                    ),
                ]),
                Line::from(vec![
                    Span::styled(
                        " Holders:           ",
                        Style::default().fg(Color::LightBlue),
                    ),
                    Span::styled(row.holders.join(", "), Style::default().fg(Color::White)),
                ]),
                Line::from(vec![
                    Span::styled(
                        " Context Limit:     ",
                        Style::default().fg(Color::LightBlue),
                    ),
                    Span::styled(
                        format!("{} tokens", row.context_length),
                        Style::default().fg(Color::White),
                    ),
                ]),
                Line::from(vec![
                    Span::styled(
                        " Selected Context:  ",
                        Style::default().fg(Color::LightBlue),
                    ),
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
                " [Enter] Load  [E] HF Explorer  [D] Download  [I] Import  [X] Delete  [T] Pull  [S] Push  [+/-] Ctx  [R] Rescan "
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
            let empty_widget =
                Paragraph::new("No model selected — press [D] to download a GGUF URL or [E] for Hugging Face Explorer")
                    .block(Block::default().title(" Details ").borders(Borders::ALL));
            frame.render_widget(empty_widget, area);
        }
    }

    fn render_hf_explorer(&self, frame: &mut Frame, area: Rect) {
        let is_compact = self.layout_mode.is_compact(area);
        let chunks = if is_compact {
            Layout::default()
                .direction(Direction::Vertical)
                .constraints([Constraint::Percentage(55), Constraint::Percentage(45)])
                .split(area)
        } else {
            Layout::default()
                .direction(Direction::Horizontal)
                .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
                .split(area)
        };

        self.render_hf_list(frame, chunks[0]);
        self.render_hf_details(frame, chunks[1]);
    }

    fn render_hf_list(&self, frame: &mut Frame, area: Rect) {
        let items: Vec<ListItem> = if self.hf_loading {
            vec![ListItem::new(Line::from(vec![Span::styled(
                " Loading Hugging Face models...",
                Style::default().fg(Color::Yellow),
            )]))]
        } else if self.hf_models.is_empty() {
            vec![ListItem::new(Line::from(vec![Span::styled(
                " No models found — press [/] to search or [T] for trending",
                Style::default().fg(Color::DarkGray),
            )]))]
        } else {
            self.hf_models
                .iter()
                .enumerate()
                .map(|(i, m)| {
                    let is_selected = i == self.hf_selected_idx;
                    let prefix = if is_selected { " > " } else { "   " };
                    let style = if is_selected {
                        Style::default()
                            .fg(Color::Cyan)
                            .add_modifier(Modifier::BOLD)
                    } else {
                        Style::default().fg(Color::White)
                    };

                    let dl_str = format!("↓ {}", format_count(m.downloads));
                    let likes_str = format!("♥ {}", format_count(m.likes));

                    let line = Line::from(vec![
                        Span::styled(prefix, style),
                        Span::styled(format!("{:<34}", truncate_string(&m.id, 32)), style),
                        Span::styled(format!("{:>9} ", dl_str), Style::default().fg(Color::Gray)),
                        Span::styled(
                            format!("{:>7}", likes_str),
                            Style::default().fg(Color::LightMagenta),
                        ),
                    ]);

                    ListItem::new(line)
                })
                .collect()
        };

        let search_display = if self.hf_is_searching {
            format!(
                "Search: {}_ (Enter: submit, Esc: cancel)",
                self.hf_search_query
            )
        } else if !self.hf_search_query.is_empty() {
            format!("Query: '{}' ([/] edit)", self.hf_search_query)
        } else {
            "Trending GGUF ([/] Search, [T] Refresh)".to_string()
        };

        let list_title = format!(
            " [E: Local Catalog]  [• HF Explorer] | {} ({}) ",
            search_display,
            self.hf_models.len()
        );

        let list_widget = List::new(items).block(
            Block::default()
                .title(list_title)
                .borders(Borders::ALL)
                .border_style(if self.hf_is_searching {
                    Style::default().fg(Color::Yellow)
                } else {
                    Style::default().fg(Color::DarkGray)
                }),
        );

        frame.render_widget(list_widget, area);
    }

    fn render_hf_details(&self, frame: &mut Frame, area: Rect) {
        let right_chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Min(12), Constraint::Length(4)])
            .split(area);

        if let Some(m) = self.selected_hf_model() {
            let author_str = m
                .author
                .clone()
                .unwrap_or_else(|| m.id.split('/').next().unwrap_or("unknown").to_string());
            let pipeline_str = m
                .pipeline_tag
                .clone()
                .unwrap_or_else(|| "text-generation".to_string());
            let gated_str = if m.gated.is_some() {
                "Yes (Gated / Token Required)"
            } else {
                "No (Public)"
            };
            let tags_str = if m.tags.is_empty() {
                "none".to_string()
            } else {
                m.tags.join(", ")
            };

            let info_lines = vec![
                Line::from(vec![
                    Span::styled(" Repository:    ", Style::default().fg(Color::LightBlue)),
                    Span::styled(
                        &m.id,
                        Style::default()
                            .fg(Color::White)
                            .add_modifier(Modifier::BOLD),
                    ),
                ]),
                Line::from(vec![
                    Span::styled(" Author:        ", Style::default().fg(Color::LightBlue)),
                    Span::styled(author_str, Style::default().fg(Color::Cyan)),
                ]),
                Line::from(vec![
                    Span::styled(" Downloads:     ", Style::default().fg(Color::LightBlue)),
                    Span::styled(format_count(m.downloads), Style::default().fg(Color::White)),
                ]),
                Line::from(vec![
                    Span::styled(" Likes:         ", Style::default().fg(Color::LightBlue)),
                    Span::styled(
                        format_count(m.likes),
                        Style::default().fg(Color::LightMagenta),
                    ),
                ]),
                Line::from(vec![
                    Span::styled(" Pipeline:      ", Style::default().fg(Color::LightBlue)),
                    Span::styled(pipeline_str, Style::default().fg(Color::White)),
                ]),
                Line::from(vec![
                    Span::styled(" Gated Model:   ", Style::default().fg(Color::LightBlue)),
                    Span::styled(
                        gated_str,
                        Style::default().fg(if m.gated.is_some() {
                            Color::Yellow
                        } else {
                            Color::Green
                        }),
                    ),
                ]),
                Line::from(vec![
                    Span::styled(" Tags:          ", Style::default().fg(Color::LightBlue)),
                    Span::styled(
                        truncate_string(&tags_str, 50),
                        Style::default().fg(Color::DarkGray),
                    ),
                ]),
                Line::from(""),
                Line::from(vec![Span::styled(
                    " Press [Enter] to inspect GGUF quants and check RAM fit",
                    Style::default()
                        .fg(Color::Green)
                        .add_modifier(Modifier::BOLD),
                )]),
            ];

            let meta_widget = Paragraph::new(info_lines).block(
                Block::default()
                    .title(" Hugging Face Model Details ")
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(Color::DarkGray)),
            );
            frame.render_widget(meta_widget, right_chunks[0]);

            let action_widget =
                Paragraph::new(Line::from(vec![Span::styled(
                " [Enter] View Quants & Download  [/] Search  [T] Trending  [Esc/E] Local Models",
                Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD),
            )]))
                .block(
                    Block::default()
                        .title(" Actions ")
                        .borders(Borders::ALL)
                        .border_style(Style::default().fg(Color::DarkGray)),
                );
            frame.render_widget(action_widget, right_chunks[1]);
        } else {
            let empty_widget =
                Paragraph::new("No model selected. Press [/] to search or [T] to load trending.")
                    .block(Block::default().title(" Details ").borders(Borders::ALL));
            frame.render_widget(empty_widget, area);
        }
    }
}

fn format_count(n: u64) -> String {
    if n >= 1_000_000 {
        format!("{:.1}M", n as f64 / 1_000_000.0)
    } else if n >= 1_000 {
        format!("{:.1}k", n as f64 / 1_000.0)
    } else {
        n.to_string()
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
