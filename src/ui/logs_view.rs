//! Dedicated node & daemon logs viewer tab (ORCHESTRATOR_PLAN.md §3.5).
//!
//! Streams real-time diagnostic logs directly from `~/.nexus/logs/`,
//! providing level-based filtering (ALL/INFO/WARN/ERROR), live search,
//! auto-tail follow mode, and scroll navigation.

use crate::logging::log_dir;
use ratatui::{
    layout::{Alignment, Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph},
    Frame,
};
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::PathBuf;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogFilterLevel {
    All,
    Info,
    Warn,
    Error,
}

impl LogFilterLevel {
    pub fn next(&self) -> Self {
        match self {
            Self::All => Self::Info,
            Self::Info => Self::Warn,
            Self::Warn => Self::Error,
            Self::Error => Self::All,
        }
    }

    pub fn label(&self) -> &'static str {
        match self {
            Self::All => "ALL",
            Self::Info => "INFO",
            Self::Warn => "WARN",
            Self::Error => "ERROR",
        }
    }

    pub fn matches(&self, line: &str) -> bool {
        match self {
            Self::All => true,
            Self::Info => {
                line.contains("INFO") || line.contains("WARN") || line.contains("ERROR")
            }
            Self::Warn => line.contains("WARN") || line.contains("ERROR"),
            Self::Error => line.contains("ERROR"),
        }
    }
}

pub struct LogsView {
    pub log_file: Option<PathBuf>,
    pub lines: Vec<String>,
    pub scroll_offset: usize,
    pub auto_tail: bool,
    pub filter_level: LogFilterLevel,
    pub search_query: String,
    pub is_searching: bool,
    pub last_read_bytes: u64,
    pub status_message: Option<(String, Color)>,
}

impl LogsView {
    pub fn new() -> Self {
        let dir = log_dir();
        let log_file = Self::find_active_log_file(&dir);
        let mut view = Self {
            log_file,
            lines: Vec::new(),
            scroll_offset: 0,
            auto_tail: true,
            filter_level: LogFilterLevel::All,
            search_query: String::new(),
            is_searching: false,
            last_read_bytes: 0,
            status_message: None,
        };
        view.refresh();
        view
    }

    pub fn for_path(path: PathBuf) -> Self {
        let mut view = Self {
            log_file: Some(path),
            lines: Vec::new(),
            scroll_offset: 0,
            auto_tail: true,
            filter_level: LogFilterLevel::All,
            search_query: String::new(),
            is_searching: false,
            last_read_bytes: 0,
            status_message: None,
        };
        view.refresh();
        view
    }

    fn find_active_log_file(dir: &std::path::Path) -> Option<PathBuf> {
        let pid_log = dir.join(format!("nexus-{}.log", std::process::id()));
        if pid_log.exists() {
            return Some(pid_log);
        }

        // Otherwise find newest log in the directory
        if let Ok(entries) = std::fs::read_dir(dir) {
            let mut logs: Vec<(std::time::SystemTime, PathBuf)> = entries
                .filter_map(|e| e.ok())
                .filter(|e| {
                    e.file_name()
                        .to_string_lossy()
                        .ends_with(".log")
                })
                .filter_map(|e| {
                    let m = e.metadata().ok()?.modified().ok()?;
                    Some((m, e.path()))
                })
                .collect();
            logs.sort_by(|a, b| b.0.cmp(&a.0));
            return logs.into_iter().next().map(|(_, p)| p);
        }
        None
    }

    pub fn refresh(&mut self) {
        let Some(path) = &self.log_file else {
            return;
        };

        if let Ok(mut file) = File::open(path) {
            if let Ok(meta) = file.metadata() {
                let file_len = meta.len();
                if file_len < self.last_read_bytes {
                    // File truncated or rotated, reset
                    self.last_read_bytes = 0;
                    self.lines.clear();
                }

                if let Ok(_) = file.seek(SeekFrom::Start(self.last_read_bytes)) {
                    let mut buf = String::new();
                    if file.read_to_string(&mut buf).is_ok() {
                        for line in buf.lines() {
                            if !line.is_empty() {
                                self.lines.push(line.to_string());
                            }
                        }
                        self.last_read_bytes = file_len;
                    }
                }
            }
        }

        if self.auto_tail {
            self.scroll_offset = 0;
        }
    }

    pub fn cycle_filter(&mut self) {
        self.filter_level = self.filter_level.next();
        self.scroll_offset = 0;
    }

    pub fn toggle_tail(&mut self) {
        self.auto_tail = !self.auto_tail;
        if self.auto_tail {
            self.scroll_offset = 0;
        }
    }

    pub fn scroll_up(&mut self, n: usize) {
        self.auto_tail = false;
        self.scroll_offset = self.scroll_offset.saturating_add(n);
    }

    pub fn scroll_down(&mut self, n: usize) {
        self.scroll_offset = self.scroll_offset.saturating_sub(n);
        if self.scroll_offset == 0 {
            self.auto_tail = true;
        }
    }

    pub fn clear(&mut self) {
        self.lines.clear();
        self.scroll_offset = 0;
        self.status_message = Some(("Log view buffer cleared".into(), Color::Cyan));
    }

    pub fn filtered_lines(&self) -> Vec<&String> {
        self.lines
            .iter()
            .filter(|line| self.filter_level.matches(line))
            .filter(|line| {
                if self.search_query.is_empty() {
                    true
                } else {
                    line.to_lowercase()
                        .contains(&self.search_query.to_lowercase())
                }
            })
            .collect()
    }

    pub fn render(&self, frame: &mut Frame, area: Rect) {
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(3), // Header & telemetry
                Constraint::Min(6),    // Log content
                Constraint::Length(1), // Footer controls / search bar
            ])
            .split(area);

        self.render_header(frame, chunks[0]);
        self.render_viewport(frame, chunks[1]);
        self.render_footer(frame, chunks[2]);
    }

    fn render_header(&self, frame: &mut Frame, area: Rect) {
        let path_label = self
            .log_file
            .as_ref()
            .and_then(|p| p.file_name())
            .map(|f| f.to_string_lossy().to_string())
            .unwrap_or_else(|| "No active log file".to_string());

        let tail_badge = if self.auto_tail {
            Span::styled(
                " [TAILING] ",
                Style::default()
                    .fg(Color::Green)
                    .add_modifier(Modifier::BOLD),
            )
        } else {
            Span::styled(
                " [PAUSED] ",
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            )
        };

        let filter_badge = Span::styled(
            format!(" [FILTER: {}] ", self.filter_level.label()),
            Style::default()
                .fg(match self.filter_level {
                    LogFilterLevel::All => Color::White,
                    LogFilterLevel::Info => Color::Cyan,
                    LogFilterLevel::Warn => Color::Yellow,
                    LogFilterLevel::Error => Color::Red,
                })
                .add_modifier(Modifier::BOLD),
        );

        let count_str = format!("Lines: {}/{}", self.filtered_lines().len(), self.lines.len());

        let title_line = Line::from(vec![
            Span::styled(
                " 📜 Node Diagnostic Logs ",
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(format!("({path_label}) "), Style::default().fg(Color::Gray)),
            tail_badge,
            filter_badge,
            Span::styled(count_str, Style::default().fg(Color::DarkGray)),
        ]);

        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::DarkGray));
        frame.render_widget(Paragraph::new(title_line).block(block), area);
    }

    fn render_viewport(&self, frame: &mut Frame, area: Rect) {
        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::DarkGray))
            .title(" Live Output ");
        let inner = block.inner(area);
        frame.render_widget(block, area);

        let filtered = self.filtered_lines();
        let total_lines = filtered.len();
        let height = inner.height as usize;

        if total_lines == 0 {
            let empty_text = Paragraph::new(Line::from(Span::styled(
                "No log messages match the active filter.",
                Style::default().fg(Color::DarkGray),
            )))
            .alignment(Alignment::Center);
            frame.render_widget(empty_text, inner);
            return;
        }

        // When auto_tail is active, show the latest `height` lines.
        // Otherwise, offset from bottom by scroll_offset.
        let end_idx = total_lines.saturating_sub(self.scroll_offset);
        let start_idx = end_idx.saturating_sub(height);

        let slice = &filtered[start_idx..end_idx];
        let mut lines = Vec::with_capacity(slice.len());

        for line in slice {
            let style = if line.contains("ERROR") {
                Style::default().fg(Color::Red).add_modifier(Modifier::BOLD)
            } else if line.contains("WARN") {
                Style::default().fg(Color::Yellow)
            } else if line.contains("INFO") {
                Style::default().fg(Color::White)
            } else if line.contains("DEBUG") || line.contains("TRACE") {
                Style::default().fg(Color::DarkGray)
            } else {
                Style::default().fg(Color::Gray)
            };
            lines.push(Line::from(Span::styled((*line).clone(), style)));
        }

        frame.render_widget(Paragraph::new(lines), inner);
    }

    fn render_footer(&self, frame: &mut Frame, area: Rect) {
        let spans = if self.is_searching {
            vec![
                Span::styled(
                    " Search: ",
                    Style::default()
                        .fg(Color::Yellow)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(&self.search_query, Style::default().fg(Color::White)),
                Span::styled(
                    " (Enter=confirm, Esc=cancel)",
                    Style::default().fg(Color::DarkGray),
                ),
            ]
        } else {
            let mut items = vec![
                Span::styled(
                    " [j/k/↑/↓] ",
                    Style::default()
                        .fg(Color::Yellow)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled("Scroll | ", Style::default().fg(Color::DarkGray)),
                Span::styled(
                    " [Space] ",
                    Style::default()
                        .fg(Color::Yellow)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled("Pause/Follow | ", Style::default().fg(Color::DarkGray)),
                Span::styled(
                    " [l] ",
                    Style::default()
                        .fg(Color::Yellow)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled("Filter Level | ", Style::default().fg(Color::DarkGray)),
                Span::styled(
                    " [/] ",
                    Style::default()
                        .fg(Color::Yellow)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled("Search | ", Style::default().fg(Color::DarkGray)),
                Span::styled(
                    " [c] ",
                    Style::default()
                        .fg(Color::Yellow)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled("Clear | ", Style::default().fg(Color::DarkGray)),
                Span::styled(
                    " [r] ",
                    Style::default()
                        .fg(Color::Yellow)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled("Reload", Style::default().fg(Color::DarkGray)),
            ];

            if let Some((msg, color)) = &self.status_message {
                items.push(Span::styled(" | ", Style::default().fg(Color::DarkGray)));
                items.push(Span::styled(
                    msg.clone(),
                    Style::default().fg(*color).add_modifier(Modifier::BOLD),
                ));
            }
            items
        };

        frame.render_widget(Paragraph::new(Line::from(spans)), area);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::NamedTempFile;
    use std::io::Write;

    #[test]
    fn test_logs_view_filtering_and_tailing() {
        let mut temp = NamedTempFile::new().unwrap();
        writeln!(temp, "2026-10-07T12:00:00Z INFO nexus: server started").unwrap();
        writeln!(temp, "2026-10-07T12:00:01Z WARN nexus: high memory pressure").unwrap();
        writeln!(temp, "2026-10-07T12:00:02Z ERROR nexus: connection refused").unwrap();
        temp.flush().unwrap();

        let mut view = LogsView::for_path(temp.path().to_path_buf());
        assert_eq!(view.lines.len(), 3);

        // Filter ALL
        assert_eq!(view.filtered_lines().len(), 3);

        // Filter WARN
        view.filter_level = LogFilterLevel::Warn;
        assert_eq!(view.filtered_lines().len(), 2);

        // Filter ERROR
        view.filter_level = LogFilterLevel::Error;
        assert_eq!(view.filtered_lines().len(), 1);

        // Search query
        view.filter_level = LogFilterLevel::All;
        view.search_query = "server".to_string();
        assert_eq!(view.filtered_lines().len(), 1);

        // Auto tail toggling
        assert!(view.auto_tail);
        view.scroll_up(2);
        assert!(!view.auto_tail);
        assert_eq!(view.scroll_offset, 2);
        view.scroll_down(2);
        assert!(view.auto_tail);
        assert_eq!(view.scroll_offset, 0);
    }
}
