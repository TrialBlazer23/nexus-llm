//! Agents & Learning Dashboard View (Phase 15 — F6).
//!
//! Visualizes agent bus tasks, orchestrator route statuses, and
//! distilled episodic memories stored in the Knowledge Base.

use crate::kb::{EpisodicKind, EpisodicMemory, KnowledgeStore};
use crate::task::{TaskRecord, TaskStatus, TaskStore};
use ratatui::{
    layout::{Alignment, Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Cell, Paragraph, Row, Table},
    Frame,
};
use std::sync::Arc;

/// Dedicated view component for monitoring agent tasks and knowledge memory.
pub struct AgentsView {
    pub task_store: Arc<TaskStore>,
    pub kb_store: Arc<KnowledgeStore>,
    pub tasks: Vec<TaskRecord>,
    pub memories: Vec<EpisodicMemory>,
    pub selected_task_idx: usize,
    pub status_message: Option<(String, Color)>,
}

impl AgentsView {
    /// Initialize AgentsView with references to the task store and knowledge base.
    pub fn new(task_store: Arc<TaskStore>, kb_store: Arc<KnowledgeStore>) -> Self {
        let tasks = task_store.list_tasks();
        let memories = kb_store.list_memories(None).unwrap_or_default();
        Self {
            task_store,
            kb_store,
            tasks,
            memories,
            selected_task_idx: 0,
            status_message: None,
        }
    }

    /// Refresh task and memory lists from underlying stores.
    pub fn refresh(&mut self) {
        self.tasks = self.task_store.list_tasks();
        self.memories = self.kb_store.list_memories(None).unwrap_or_default();
        if self.selected_task_idx >= self.tasks.len() && !self.tasks.is_empty() {
            self.selected_task_idx = self.tasks.len() - 1;
        }
    }

    /// Move selection cursor down in the tasks table.
    pub fn next_task(&mut self) {
        if !self.tasks.is_empty() {
            self.selected_task_idx = (self.selected_task_idx + 1) % self.tasks.len();
        }
    }

    /// Move selection cursor up in the tasks table.
    pub fn prev_task(&mut self) {
        if !self.tasks.is_empty() {
            if self.selected_task_idx == 0 {
                self.selected_task_idx = self.tasks.len() - 1;
            } else {
                self.selected_task_idx -= 1;
            }
        }
    }

    /// Return the currently selected task record.
    pub fn selected_task(&self) -> Option<&TaskRecord> {
        self.tasks.get(self.selected_task_idx)
    }

    /// Render the Agents View into the specified terminal frame area.
    pub fn render(&self, frame: &mut Frame, area: Rect) {
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(3),      // Header telemetry
                Constraint::Percentage(55), // Agent Tasks table
                Constraint::Min(8),         // Memories and route summary
                Constraint::Length(3),      // Status & keybinding bar
            ])
            .split(area);

        self.render_header(frame, chunks[0]);
        self.render_tasks_table(frame, chunks[1]);
        self.render_memories_and_routes(frame, chunks[2]);
        self.render_footer(frame, chunks[3]);
    }

    fn render_header(&self, frame: &mut Frame, area: Rect) {
        let completed = self
            .tasks
            .iter()
            .filter(|t| t.status == TaskStatus::Completed)
            .count();
        let running = self
            .tasks
            .iter()
            .filter(|t| t.status == TaskStatus::Running)
            .count();
        let failed = self
            .tasks
            .iter()
            .filter(|t| t.status == TaskStatus::Failed)
            .count();

        let header_text = vec![Line::from(vec![
            Span::styled(
                " 🤖 Agent Bus & Knowledge Mesh ",
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(" | ", Style::default().fg(Color::DarkGray)),
            Span::styled(
                format!("Tasks: {} total", self.tasks.len()),
                Style::default().fg(Color::White),
            ),
            Span::styled(" (", Style::default().fg(Color::DarkGray)),
            Span::styled(format!("{completed} done"), Style::default().fg(Color::Green)),
            Span::styled(", ", Style::default().fg(Color::DarkGray)),
            Span::styled(format!("{running} active"), Style::default().fg(Color::Yellow)),
            Span::styled(", ", Style::default().fg(Color::DarkGray)),
            Span::styled(format!("{failed} failed"), Style::default().fg(Color::Red)),
            Span::styled(") | ", Style::default().fg(Color::DarkGray)),
            Span::styled(
                format!("KB Memories: {}", self.memories.len()),
                Style::default().fg(Color::Magenta),
            ),
        ])];

        let p = Paragraph::new(header_text).block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::Cyan)),
        );
        frame.render_widget(p, area);
    }

    fn render_tasks_table(&self, frame: &mut Frame, area: Rect) {
        let header_cells = [
            "Task ID",
            "From",
            "Route",
            "Status",
            "Prompt Preview",
            "Output Preview",
        ]
        .iter()
        .map(|h| {
            Cell::from(*h).style(
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            )
        });
        let header = Row::new(header_cells).height(1);

        let rows = self.tasks.iter().enumerate().map(|(idx, task)| {
            let is_selected = idx == self.selected_task_idx;
            let (status_badge, status_color) = match task.status {
                TaskStatus::Pending => ("[WAIT]", Color::Yellow),
                TaskStatus::Running => ("[RUN]", Color::Cyan),
                TaskStatus::Completed => ("[OK]", Color::Green),
                TaskStatus::Failed => ("[ERR]", Color::Red),
            };

            let short_id = task.task_id.to_string()[..8].to_string();
            let short_from = task.from_node.to_string()[..8].to_string();
            let prompt_preview = if task.prompt.len() > 30 {
                format!("{}…", &task.prompt[..30])
            } else {
                task.prompt.clone()
            };
            let output_preview = task
                .output
                .as_deref()
                .map(|o| {
                    if o.len() > 30 {
                        format!("{}…", &o[..30])
                    } else {
                        o.to_string()
                    }
                })
                .unwrap_or_else(|| "-".to_string());

            let row_style = if is_selected {
                Style::default()
                    .bg(Color::Rgb(30, 45, 65))
                    .fg(Color::White)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(Color::Gray)
            };

            Row::new(vec![
                Cell::from(short_id),
                Cell::from(short_from),
                Cell::from(task.to_route.clone()),
                Cell::from(status_badge).style(Style::default().fg(status_color)),
                Cell::from(prompt_preview),
                Cell::from(output_preview),
            ])
            .style(row_style)
        });

        let widths = [
            Constraint::Length(10),
            Constraint::Length(10),
            Constraint::Length(12),
            Constraint::Length(8),
            Constraint::Percentage(35),
            Constraint::Percentage(35),
        ];

        let table = Table::new(rows, widths)
            .header(header)
            .block(
                Block::default()
                    .title(" Agent Bus Tasks ")
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(Color::White)),
            );

        frame.render_widget(table, area);
    }

    fn render_memories_and_routes(&self, frame: &mut Frame, area: Rect) {
        let cols = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(60), Constraint::Percentage(40)])
            .split(area);

        // Memories table
        let mem_rows = self.memories.iter().rev().take(8).map(|m| {
            let (badge, color) = match m.kind {
                EpisodicKind::Fact => ("[FACT]", Color::Cyan),
                EpisodicKind::Preference => ("[PREF]", Color::Magenta),
                EpisodicKind::Summary => ("[SUMM]", Color::Green),
                EpisodicKind::Entity => ("[ENT]", Color::Yellow),
            };
            let title = if m.title.len() > 24 {
                format!("{}…", &m.title[..24])
            } else {
                m.title.clone()
            };
            let summary = if m.summary.len() > 32 {
                format!("{}…", &m.summary[..32])
            } else {
                m.summary.clone()
            };

            Row::new(vec![
                Cell::from(badge).style(Style::default().fg(color)),
                Cell::from(title),
                Cell::from(summary),
            ])
        });

        let mem_table = Table::new(
            mem_rows,
            [
                Constraint::Length(8),
                Constraint::Length(26),
                Constraint::Percentage(60),
            ],
        )
        .header(
            Row::new(vec![
                Cell::from("Kind").style(Style::default().fg(Color::Yellow)),
                Cell::from("Title").style(Style::default().fg(Color::Yellow)),
                Cell::from("Summary").style(Style::default().fg(Color::Yellow)),
            ])
            .height(1),
        )
        .block(
            Block::default()
                .title(" Distilled Knowledge Base Memories ")
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::Magenta)),
        );
        frame.render_widget(mem_table, cols[0]);

        // Route overview
        let selected_details = if let Some(task) = self.selected_task() {
            vec![
                Line::from(vec![
                    Span::styled("Selected Task: ", Style::default().fg(Color::Cyan)),
                    Span::styled(task.task_id.to_string(), Style::default().fg(Color::White)),
                ]),
                Line::from(vec![
                    Span::styled("Route Target:  ", Style::default().fg(Color::Cyan)),
                    Span::styled(&task.to_route, Style::default().fg(Color::Yellow)),
                ]),
                Line::from(vec![
                    Span::styled("Full Prompt:   ", Style::default().fg(Color::Cyan)),
                    Span::styled(&task.prompt, Style::default().fg(Color::Gray)),
                ]),
                Line::from(vec![
                    Span::styled("Full Result:   ", Style::default().fg(Color::Cyan)),
                    Span::styled(
                        task.output.as_deref().unwrap_or("-"),
                        Style::default().fg(Color::Green),
                    ),
                ]),
            ]
        } else {
            vec![Line::from(Span::styled(
                "No tasks recorded in agent bus.",
                Style::default().fg(Color::DarkGray),
            ))]
        };

        let details_p = Paragraph::new(selected_details).block(
            Block::default()
                .title(" Selected Task Details ")
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::Yellow)),
        );
        frame.render_widget(details_p, cols[1]);
    }

    fn render_footer(&self, frame: &mut Frame, area: Rect) {
        let mut spans = vec![
            Span::styled(
                " [↑/↓] Navigate ",
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(" | ", Style::default().fg(Color::DarkGray)),
            Span::styled(
                " [D] Distill (Janitor) ",
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(" | ", Style::default().fg(Color::DarkGray)),
            Span::styled(
                " [S] Sync Mesh KB ",
                Style::default()
                    .fg(Color::Magenta)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(" | ", Style::default().fg(Color::DarkGray)),
            Span::styled(" [R] Rescan ", Style::default().fg(Color::Green)),
        ];

        if let Some((msg, color)) = &self.status_message {
            spans.push(Span::styled(" | ", Style::default().fg(Color::DarkGray)));
            spans.push(Span::styled(msg.clone(), Style::default().fg(*color)));
        }

        let p = Paragraph::new(Line::from(spans)).alignment(Alignment::Left);
        frame.render_widget(p, area);
    }
}
