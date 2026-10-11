//! Main layout and rendering routines for the Unified Nexus Hub.

use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph, Tabs},
    Frame,
};

use super::status;
use super::{HubApp, HubTab};

/// Responsive centered rectangle calculation helper.
pub fn centered_rect(percent_x: u16, percent_y: u16, r: Rect) -> Rect {
    crate::ui::layout::responsive_centered_rect(
        percent_x,
        percent_y,
        r,
        crate::ui::layout::LayoutMode::Auto,
    )
}

impl HubApp {
    pub fn render(&mut self, frame: &mut Frame) {
        self.sync_layout_mode();
        let area = frame.area();
        let is_compact = self.layout_mode.is_compact(area);

        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(3),
                Constraint::Min(10),
                Constraint::Length(2),
            ])
            .split(area);

        self.render_top_tabs(frame, chunks[0], is_compact);

        match self.active_tab {
            HubTab::Chat => self.chat.render_in_area(frame, chunks[1]),
            HubTab::Models => self.models_view.render(frame, chunks[1]),
            HubTab::Cluster => self.cluster_view.render(frame, chunks[1]),
            HubTab::Settings => self.settings_view.render(frame, chunks[1]),
            HubTab::Tunnel => {
                if let Ok(guard) = self.discovery.peers().try_read() {
                    let peers: Vec<_> = guard.values().cloned().collect();
                    drop(guard);
                    self.tunnel_view.remember_peer_labels(&peers);
                }
                self.tunnel_view.render(frame, chunks[1]);
            }
            HubTab::Agents => self.agents_view.render(frame, chunks[1]),
            HubTab::Logs => self.logs_view.render(frame, chunks[1]),
        }

        self.render_footer(frame, chunks[2], is_compact);

        if let Some(intent) = &self.pending_hot_swap {
            self.render_hot_swap_modal(frame, area, intent);
        }

        if let Some(target_state) = &self.pending_target_selection {
            self.render_target_selection_modal(frame, area, target_state);
        }

        if self.show_help {
            self.render_help_modal(frame, area);
        }

        if self.show_command_palette {
            self.render_command_palette(frame, area);
        }

        if let Some(progress) = &self.download_progress {
            self.render_download_progress(frame, area, progress);
        }

        if let Some(delete_info) = &self.pending_delete_model {
            self.render_delete_model_modal(frame, area, delete_info);
        }

        if let Some(modal_state) = &self.pending_download_modal {
            self.render_download_modal(frame, area, modal_state);
        }

        if let Some(modal_state) = &self.pending_import_modal {
            self.render_import_modal(frame, area, modal_state);
        }

        if let Some(picker) = &self.pending_hf_quant_picker {
            self.render_hf_quant_picker(frame, area, picker);
        }

        if let Some(recovery) = &self.pending_hf_auth_recovery {
            self.render_hf_auth_recovery(frame, area, recovery);
        }

        if let Some(peers) = &self.pending_push_peers {
            self.render_push_peers(frame, area, peers);
        }

        if let Some(phase) = &self.load_phase {
            self.render_load_phase(frame, area, phase);
        }
    }

    fn render_top_tabs(&self, frame: &mut Frame, area: Rect, is_compact: bool) {
        let titles = if is_compact {
            vec![
                "1:Chat", "2:Mod", "3:Clus", "4:Set", "5:Tun", "6:Agnt", "7:Log",
            ]
        } else {
            vec![
                HubTab::Chat.title(),
                HubTab::Models.title(),
                HubTab::Cluster.title(),
                HubTab::Settings.title(),
                HubTab::Tunnel.title(),
                HubTab::Agents.title(),
                HubTab::Logs.title(),
            ]
        };
        let selected = self.active_tab as usize;
        let block = if is_compact {
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::DarkGray))
        } else {
            Block::default()
                .title(" Nexus-LLM Unified Hub ")
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::DarkGray))
        };
        let tabs = Tabs::new(titles)
            .select(selected)
            .block(block)
            .style(Style::default().fg(Color::Gray))
            .highlight_style(
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD)
                    .bg(Color::Rgb(20, 30, 40)),
            );
        frame.render_widget(tabs, area);
    }

    fn render_footer(&self, frame: &mut Frame, area: Rect, is_compact: bool) {
        // Line 1: Rich Telemetry Dock (ORCHESTRATOR_PLAN.md §3.2)
        let prompt_tokens: usize = self
            .chat
            .messages
            .iter()
            .map(|m| (m.message.content.len() / 4).max(1))
            .sum();
        let used_tokens = prompt_tokens + self.chat.tokens_streamed;
        let max_ctx = self.models_view.selected_context.max(512);
        let ratio = ((used_tokens as f64) / (max_ctx as f64)).clamp(0.0, 1.0);
        let pct = (ratio * 100.0) as usize;
        let filled_bars = ((ratio * 10.0).round() as usize).min(10);
        let gauge_str = format!(
            "[{}{}] {}/{} ({}%)",
            "█".repeat(filled_bars),
            "░".repeat(10 - filled_bars),
            used_tokens,
            max_ctx,
            pct
        );

        let speed_str = if self.chat.is_streaming || self.chat.tokens_per_sec > 0.0 {
            format!("⚡ {:.1} t/s", self.chat.tokens_per_sec)
        } else {
            "Idle".to_string()
        };

        let status_badge = if self.chat.is_streaming {
            crate::ui::badges::BADGE_STREAMING.span()
        } else if self.supervisor.is_running_blocking() {
            crate::ui::badges::BADGE_READY.span()
        } else {
            crate::ui::badges::BADGE_OK.span()
        };

        let activity_line = status::format_activity_line(&status::Activity {
            local_model: self.active_model_name.clone(),
            chat_host: self.chat_host.clone(),
            chat_model: self.chat_model.clone(),
        });

        let line1 = if is_compact {
            Line::from(vec![
                Span::styled(" ", Style::default()),
                Span::styled(
                    activity_line.clone(),
                    Style::default()
                        .fg(Color::Yellow)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(" | ", Style::default().fg(Color::DarkGray)),
                Span::styled(
                    speed_str,
                    Style::default()
                        .fg(Color::Green)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(" | Ctx: ", Style::default().fg(Color::LightBlue)),
                Span::styled(
                    format!("{}%", pct),
                    Style::default()
                        .fg(if pct > 85 {
                            Color::Red
                        } else if pct > 65 {
                            Color::Yellow
                        } else {
                            Color::Green
                        })
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(" | ", Style::default().fg(Color::DarkGray)),
                status_badge,
            ])
        } else {
            Line::from(vec![
                Span::styled(
                    " Model: ",
                    Style::default()
                        .fg(Color::Yellow)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    activity_line,
                    Style::default()
                        .fg(Color::White)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(" | ", Style::default().fg(Color::DarkGray)),
                Span::styled("Host: ", Style::default().fg(Color::Cyan)),
                Span::styled(
                    self.chat.client.endpoint(),
                    Style::default().fg(Color::White),
                ),
                Span::styled(" | ", Style::default().fg(Color::DarkGray)),
                Span::styled(
                    speed_str,
                    Style::default()
                        .fg(Color::Green)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(" | ", Style::default().fg(Color::DarkGray)),
                Span::styled("Context: ", Style::default().fg(Color::LightBlue)),
                Span::styled(
                    gauge_str,
                    Style::default()
                        .fg(if pct > 85 {
                            Color::Red
                        } else if pct > 65 {
                            Color::Yellow
                        } else {
                            Color::Green
                        })
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(" | ", Style::default().fg(Color::DarkGray)),
                status_badge,
            ])
        };

        // Line 2: Global Navigation, Hotkeys & Transient Alerts
        let mut spans2 = if is_compact {
            vec![
                Span::styled(
                    " [Tab] Next ",
                    Style::default()
                        .fg(Color::Yellow)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    " [^P] Cmd ",
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(" [?] Help ", Style::default().fg(Color::White)),
            ]
        } else {
            vec![
                Span::styled(
                    " [F1-F7] Tabs ",
                    Style::default()
                        .fg(Color::Yellow)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    " [Ctrl+P] Palette ",
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(" [?] Help ", Style::default().fg(Color::White)),
            ]
        };

        if self.supervisor.is_running_blocking() {
            spans2.push(Span::styled(" | ", Style::default().fg(Color::DarkGray)));
            spans2.push(Span::styled(
                " [u] Unload ",
                Style::default()
                    .fg(Color::Magenta)
                    .add_modifier(Modifier::BOLD),
            ));
        }

        if let Some((msg, color)) = &self.status_message {
            spans2.push(Span::styled(" | ", Style::default().fg(Color::DarkGray)));
            spans2.push(Span::styled(
                format!("ALERT: {}", msg),
                Style::default().fg(*color).add_modifier(Modifier::BOLD),
            ));
        }

        frame.render_widget(Paragraph::new(vec![line1, Line::from(spans2)]), area);
    }
}
