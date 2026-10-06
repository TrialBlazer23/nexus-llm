use crate::client::{ChatCompletionRequest, ChatMessage, NexusClient};
use crate::ui::markdown::render_markdown;
use crossterm::{
    event::{Event, EventStream, KeyCode, KeyModifiers},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use futures_util::StreamExt;
use ratatui::{
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph, Wrap},
    Frame, Terminal,
};
use std::io::stdout;
use std::time::Instant;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

pub enum StreamMsg {
    Token(String),
    Done,
    Error(String),
}

/// How the chat client endpoint is being reached.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransportBadge {
    Local,
    Usb,
    Wifi,
}

impl TransportBadge {
    pub fn label(self) -> &'static str {
        match self {
            Self::Local => "[Local]",
            Self::Usb => "[USB Cable]",
            Self::Wifi => "[Wi-Fi]",
        }
    }

    /// Infer a sensible default from an endpoint URL (never assumes USB).
    pub fn from_endpoint(endpoint: &str) -> Self {
        if endpoint.contains("127.0.0.1") || endpoint.contains("localhost") {
            Self::Local
        } else {
            Self::Wifi
        }
    }
}

/// State container for the interactive TUI chat session.
pub struct ChatApp {
    pub client: NexusClient,
    pub model_name: String,
    pub system_prompt: Option<String>,
    pub persona_name: Option<String>,
    pub temperature: f32,
    pub max_tokens: usize,
    pub messages: Vec<ChatMessage>,
    pub streaming_response: String,
    pub is_streaming: bool,
    pub input_buffer: String,
    /// Character index of the caret within `input_buffer`.
    pub cursor_idx: usize,
    /// Previously submitted prompts for Alt+↑/↓ recall.
    pub prompt_history: Vec<String>,
    /// Current position while browsing `prompt_history` (`None` = not browsing).
    pub history_index: Option<usize>,
    pub scroll_offset: u16,
    pub auto_scroll: bool,
    pub tokens_streamed: usize,
    pub tokens_per_sec: f64,
    pub stream_start_time: Option<Instant>,
    pub status_message: Option<String>,
    pub transport_badge: TransportBadge,
    stream_handle: Option<JoinHandle<()>>,
}

impl ChatApp {
    pub fn new(client: NexusClient, model_name: impl Into<String>, system_prompt: Option<String>) -> Self {
        let transport_badge = TransportBadge::from_endpoint(client.endpoint());
        Self {
            client,
            model_name: model_name.into(),
            system_prompt,
            persona_name: None,
            temperature: 0.7,
            max_tokens: 2048,
            messages: Vec::new(),
            streaming_response: String::new(),
            is_streaming: false,
            input_buffer: String::new(),
            cursor_idx: 0,
            prompt_history: Vec::new(),
            history_index: None,
            scroll_offset: 0,
            auto_scroll: true,
            tokens_streamed: 0,
            tokens_per_sec: 0.0,
            stream_start_time: None,
            status_message: None,
            transport_badge,
            stream_handle: None,
        }
    }

    /// Apply a persona's system prompt and generation hyperparameters.
    pub fn apply_preset(&mut self, name: &str, system_prompt: String, temperature: f32, max_tokens: usize) {
        self.persona_name = Some(name.to_string());
        self.system_prompt = Some(system_prompt);
        self.temperature = temperature;
        self.max_tokens = max_tokens;
    }

    /// Abort an in-flight generation. Returns true if a stream was aborted.
    pub fn abort_stream(&mut self) -> bool {
        let had_handle = self.stream_handle.take().map(|h| {
            h.abort();
            true
        }).unwrap_or(false);

        if self.is_streaming || had_handle {
            self.finalize_stream();
            self.status_message = Some("Generation aborted (Esc)".to_string());
            true
        } else {
            false
        }
    }

    /// Returns true if a message is a genuine dialogue turn rather than UI chrome.
    pub fn is_conversation_message(m: &ChatMessage) -> bool {
        !m.is_status()
    }

    /// Build a cleaned list of messages for sending to OpenAI /v1/chat/completions.
    pub fn clean_conversation_messages(
        messages: &[ChatMessage],
        system_prompt: Option<&str>,
    ) -> Vec<ChatMessage> {
        let mut req_messages = Vec::new();
        if let Some(sys) = system_prompt {
            req_messages.push(ChatMessage::system(sys));
        }
        for m in messages {
            if Self::is_conversation_message(m) {
                req_messages.push(m.clone());
            }
        }
        req_messages
    }

    fn input_char_len(&self) -> usize {
        self.input_buffer.chars().count()
    }

    fn clamp_cursor(&mut self) {
        let len = self.input_char_len();
        if self.cursor_idx > len {
            self.cursor_idx = len;
        }
    }

    fn insert_at_cursor(&mut self, ch: char) {
        let mut chars: Vec<char> = self.input_buffer.chars().collect();
        let idx = self.cursor_idx.min(chars.len());
        chars.insert(idx, ch);
        self.input_buffer = chars.into_iter().collect();
        self.cursor_idx = idx + 1;
        self.history_index = None;
    }

    fn delete_before_cursor(&mut self) {
        if self.cursor_idx == 0 {
            return;
        }
        let mut chars: Vec<char> = self.input_buffer.chars().collect();
        let idx = self.cursor_idx.min(chars.len());
        if idx > 0 {
            chars.remove(idx - 1);
            self.input_buffer = chars.into_iter().collect();
            self.cursor_idx = idx - 1;
            self.history_index = None;
        }
    }

    fn delete_at_cursor(&mut self) {
        let mut chars: Vec<char> = self.input_buffer.chars().collect();
        let idx = self.cursor_idx.min(chars.len());
        if idx < chars.len() {
            chars.remove(idx);
            self.input_buffer = chars.into_iter().collect();
            self.history_index = None;
        }
    }

    fn history_prev(&mut self) {
        if self.prompt_history.is_empty() {
            return;
        }
        let next = match self.history_index {
            None => self.prompt_history.len() - 1,
            Some(0) => 0,
            Some(i) => i - 1,
        };
        self.history_index = Some(next);
        self.input_buffer = self.prompt_history[next].clone();
        self.cursor_idx = self.input_char_len();
    }

    fn history_next(&mut self) {
        let Some(idx) = self.history_index else {
            return;
        };
        if idx + 1 >= self.prompt_history.len() {
            self.history_index = None;
            self.input_buffer.clear();
            self.cursor_idx = 0;
        } else {
            let next = idx + 1;
            self.history_index = Some(next);
            self.input_buffer = self.prompt_history[next].clone();
            self.cursor_idx = self.input_char_len();
        }
    }

    /// Calculate approximate total line count across current conversation.
    /// Non-status messages use markdown-rendered line counts.
    pub fn total_lines(&self) -> usize {
        let mut count = 0;
        if self.system_prompt.is_some() {
            count += 2;
        }
        for msg in &self.messages {
            if msg.is_status() {
                count += 1 + msg.content.lines().count() + 1;
            } else {
                count += 1 + render_markdown(&msg.content).len() + 1;
            }
        }
        if self.is_streaming || !self.streaming_response.is_empty() {
            count += 1 + render_markdown(&self.streaming_response).len();
        }
        count
    }

    /// Process a stream chunk received from background worker.
    pub fn handle_stream_token(&mut self, token: String) {
        if !self.is_streaming {
            return;
        }
        self.streaming_response.push_str(&token);
        self.tokens_streamed += 1;

        if let Some(start) = self.stream_start_time {
            let elapsed = start.elapsed().as_secs_f64();
            if elapsed > 0.0 {
                self.tokens_per_sec = (self.tokens_streamed as f64) / elapsed;
            }
        }
    }

    /// Finalize current stream response into message history.
    pub fn finalize_stream(&mut self) {
        if !self.streaming_response.is_empty() {
            let content = std::mem::take(&mut self.streaming_response);
            self.messages.push(ChatMessage::assistant(content));
        }
        self.is_streaming = false;
        self.stream_start_time = None;
        self.stream_handle = None;
    }

    /// Prepare and render the UI frame for full window.
    pub fn render(&self, frame: &mut Frame) {
        let area = frame.area();
        self.render_in_area(frame, area);
    }

    /// Prepare and render the UI frame within a specified sub-area.
    pub fn render_in_area(&self, frame: &mut Frame, area: Rect) {
        let show_hints = crate::ui::slash::should_show_hints(&self.input_buffer);
        let hint_lines = if show_hints {
            crate::ui::slash::matching_hints(&self.input_buffer).len().min(8) as u16
        } else {
            0
        };

        let mut constraints = vec![
            Constraint::Length(3), // Header bar
            Constraint::Min(8),    // Chat history area
        ];
        if hint_lines > 0 {
            constraints.push(Constraint::Length(hint_lines + 2));
        }
        constraints.push(Constraint::Length(3)); // Input box
        constraints.push(Constraint::Length(1)); // Help / status footer

        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .margin(1)
            .constraints(constraints)
            .split(area);

        self.render_header(frame, chunks[0]);
        self.render_chat_history(frame, chunks[1]);
        let mut idx = 2;
        if hint_lines > 0 {
            self.render_slash_hints(frame, chunks[idx]);
            idx += 1;
        }
        self.render_input_box(frame, chunks[idx]);
        self.render_footer(frame, chunks[idx + 1]);
    }

    fn render_slash_hints(&self, frame: &mut Frame, area: Rect) {
        let hints = crate::ui::slash::matching_hints(&self.input_buffer);
        let lines: Vec<Line> = hints
            .into_iter()
            .map(|h| {
                Line::from(Span::styled(
                    format!("  {}", h),
                    Style::default().fg(Color::DarkGray),
                ))
            })
            .collect();
        let widget = Paragraph::new(lines).block(
            Block::default()
                .title(" Slash Commands ")
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::Magenta)),
        );
        frame.render_widget(widget, area);
    }

    fn spawn_stream(&mut self, tx: &mpsc::Sender<StreamMsg>) {
        let req_messages =
            Self::clean_conversation_messages(&self.messages, self.system_prompt.as_deref());

        let req = ChatCompletionRequest {
            model: self.model_name.clone(),
            messages: req_messages,
            temperature: Some(self.temperature),
            top_p: None,
            max_tokens: Some(self.max_tokens),
            stream: true,
        };

        let client = self.client.clone();
        let tx_clone = tx.clone();

        self.stream_handle = Some(tokio::spawn(async move {
            match client.stream_chat(req).await {
                Ok(mut stream) => {
                    while let Some(chunk) = stream.next().await {
                        match chunk {
                            Ok(token) => {
                                let _ = tx_clone.send(StreamMsg::Token(token)).await;
                            }
                            Err(e) => {
                                let _ = tx_clone.send(StreamMsg::Error(e.to_string())).await;
                                break;
                            }
                        }
                    }
                    let _ = tx_clone.send(StreamMsg::Done).await;
                }
                Err(e) => {
                    let _ = tx_clone.send(StreamMsg::Error(e.to_string())).await;
                }
            }
        }));
    }

    /// Handle key event for input buffering, scrolling, or dispatching streaming requests.
    pub fn handle_key_input(&mut self, key: crossterm::event::KeyEvent, tx: &mpsc::Sender<StreamMsg>) {
        match key.code {
            KeyCode::Esc => {
                let _ = self.abort_stream();
            }
            KeyCode::Left => {
                if self.cursor_idx > 0 {
                    self.cursor_idx -= 1;
                }
            }
            KeyCode::Right => {
                if self.cursor_idx < self.input_char_len() {
                    self.cursor_idx += 1;
                }
            }
            KeyCode::Home => {
                self.cursor_idx = 0;
            }
            KeyCode::End => {
                self.cursor_idx = self.input_char_len();
            }
            KeyCode::Backspace => {
                self.delete_before_cursor();
            }
            KeyCode::Delete => {
                self.delete_at_cursor();
            }
            KeyCode::Char(c) => {
                // Shift+Enter / Alt+Enter insert a newline (some terminals emit Char('\n')).
                if c == '\n' {
                    self.insert_at_cursor('\n');
                } else if !key.modifiers.contains(KeyModifiers::CONTROL)
                    && !key.modifiers.contains(KeyModifiers::ALT)
                {
                    self.insert_at_cursor(c);
                }
            }
            KeyCode::Up if key.modifiers.contains(KeyModifiers::ALT) => {
                self.history_prev();
            }
            KeyCode::Down if key.modifiers.contains(KeyModifiers::ALT) => {
                self.history_next();
            }
            KeyCode::Up => {
                let max = self.total_lines() as u16;
                let current = if self.auto_scroll { max } else { self.scroll_offset };
                self.auto_scroll = false;
                self.scroll_offset = current.saturating_sub(1);
            }
            KeyCode::Down => {
                self.scroll_offset = self.scroll_offset.saturating_add(1);
                if self.scroll_offset >= self.total_lines() as u16 {
                    self.auto_scroll = true;
                }
            }
            KeyCode::PageUp => {
                let max = self.total_lines() as u16;
                let current = if self.auto_scroll { max } else { self.scroll_offset };
                self.auto_scroll = false;
                self.scroll_offset = current.saturating_sub(10);
            }
            KeyCode::PageDown => {
                self.auto_scroll = true;
            }
            KeyCode::Enter
                if key.modifiers.contains(KeyModifiers::SHIFT)
                    || key.modifiers.contains(KeyModifiers::ALT) =>
            {
                self.insert_at_cursor('\n');
            }
            KeyCode::Enter => {
                if !self.is_streaming && !self.input_buffer.trim().is_empty() {
                    let prompt = std::mem::take(&mut self.input_buffer);
                    self.cursor_idx = 0;
                    self.history_index = None;
                    self.prompt_history.push(prompt.clone());
                    self.messages.push(ChatMessage::user(&prompt));
                    self.is_streaming = true;
                    self.auto_scroll = true;
                    self.status_message = None;
                    self.streaming_response.clear();
                    self.tokens_streamed = 0;
                    self.tokens_per_sec = 0.0;
                    self.stream_start_time = Some(Instant::now());
                    self.spawn_stream(tx);
                }
            }
            _ => {}
        }
        self.clamp_cursor();
    }

    fn render_header(&self, frame: &mut Frame, area: Rect) {
        let stream_info = if self.is_streaming {
            format!(" | Generating: {:.1} tokens/s", self.tokens_per_sec)
        } else {
            String::new()
        };

        let title = format!(
            " Nexus-LLM Terminal | Host: {} {} | Model: {}{}",
            self.client.endpoint(),
            self.transport_badge.label(),
            self.model_name,
            stream_info
        );

        let header = Paragraph::new(Line::from(vec![
            Span::styled(title, Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)),
        ]))
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::DarkGray)),
        );

        frame.render_widget(header, area);
    }

    fn render_chat_history(&self, frame: &mut Frame, area: Rect) {
        let mut lines = Vec::new();

        if let Some(sys) = &self.system_prompt {
            lines.push(Line::from(vec![
                Span::styled(" [System] ", Style::default().fg(Color::Magenta).add_modifier(Modifier::BOLD)),
                Span::styled(sys.clone(), Style::default().fg(Color::DarkGray)),
            ]));
            lines.push(Line::from(""));
        }

        for msg in &self.messages {
            if msg.is_status() {
                lines.push(Line::from(vec![
                    Span::styled(
                        " [Status] ",
                        Style::default()
                            .fg(Color::DarkGray)
                            .add_modifier(Modifier::ITALIC),
                    ),
                ]));
                for text_line in msg.content.lines() {
                    lines.push(Line::from(Span::styled(
                        format!("   {}", text_line),
                        Style::default()
                            .fg(Color::DarkGray)
                            .add_modifier(Modifier::ITALIC),
                    )));
                }
                lines.push(Line::from(""));
                continue;
            }

            let (label, color) = if msg.role == "user" {
                (" [You] ", Color::Blue)
            } else {
                (" [Nexus] ", Color::Green)
            };

            lines.push(Line::from(vec![
                Span::styled(label, Style::default().fg(color).add_modifier(Modifier::BOLD)),
            ]));

            for md_line in render_markdown(&msg.content) {
                let mut spans = vec![Span::raw("   ")];
                spans.extend(md_line.spans);
                lines.push(Line::from(spans));
            }
            lines.push(Line::from(""));
        }

        // Display current active streaming generation (markdown-rendered)
        if self.is_streaming || !self.streaming_response.is_empty() {
            lines.push(Line::from(vec![
                Span::styled(" [Nexus] ", Style::default().fg(Color::Green).add_modifier(Modifier::BOLD)),
                Span::styled("(generating...)", Style::default().fg(Color::Yellow)),
            ]));

            for md_line in render_markdown(&self.streaming_response) {
                let mut spans = vec![Span::raw("   ")];
                spans.extend(md_line.spans);
                lines.push(Line::from(spans));
            }
        }

        let total_lines = lines.len() as u16;
        let viewport_height = area.height.saturating_sub(2);
        let max_scroll = total_lines.saturating_sub(viewport_height);

        let scroll_y = if self.auto_scroll {
            max_scroll
        } else {
            self.scroll_offset.min(max_scroll)
        };

        let history = Paragraph::new(lines)
            .block(
                Block::default()
                    .title(" Conversation History ")
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(Color::LightBlue)),
            )
            .scroll((scroll_y, 0))
            .wrap(Wrap { trim: false });

        frame.render_widget(history, area);
    }

    fn render_input_box(&self, frame: &mut Frame, area: Rect) {
        let chars: Vec<char> = self.input_buffer.chars().collect();
        let idx = self.cursor_idx.min(chars.len());

        let mut spans = vec![Span::raw("> ")];
        let before: String = chars[..idx].iter().collect();
        if !before.is_empty() {
            spans.push(Span::raw(before));
        }

        if idx < chars.len() {
            spans.push(Span::styled(
                chars[idx].to_string(),
                Style::default().add_modifier(Modifier::REVERSED),
            ));
            let after: String = chars[idx + 1..].iter().collect();
            if !after.is_empty() {
                spans.push(Span::raw(after));
            }
        } else {
            // Caret at end of buffer
            spans.push(Span::styled(
                " ",
                Style::default().add_modifier(Modifier::REVERSED),
            ));
        }

        let border_color = if self.is_streaming {
            Color::Yellow
        } else {
            Color::White
        };

        let input_widget = Paragraph::new(Line::from(spans))
            .block(
                Block::default()
                    .title(" Prompt Input (Alt+↑/↓ history) ")
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(border_color)),
            );

        frame.render_widget(input_widget, area);
    }

    fn render_footer(&self, frame: &mut Frame, area: Rect) {
        let (text, style) = if let Some(status) = &self.status_message {
            let color = if status.to_lowercase().contains("error") {
                Color::LightRed
            } else {
                Color::Yellow
            };
            (status.clone(), Style::default().fg(color).add_modifier(Modifier::BOLD))
        } else if self.is_streaming {
            (
                "[Esc] Abort generation  |  [Ctrl+C] Quit  |  [Up/Down/PgUp/PgDn] Scroll".to_string(),
                Style::default().fg(Color::DarkGray),
            )
        } else {
            (
                "[Enter] Submit  |  [Esc] Abort (when generating)  |  [Ctrl+C / q] Quit  |  [Up/Down] Scroll"
                    .to_string(),
                Style::default().fg(Color::DarkGray),
            )
        };

        let footer = Paragraph::new(Line::from(vec![
            Span::styled(text, style),
        ]));

        frame.render_widget(footer, area);
    }
}

/// Run full interactive Ratatui TUI chat session.
pub async fn run_chat_tui(mut app: ChatApp) -> Result<(), Box<dyn std::error::Error>> {
    crate::ui::install_tui_panic_hook();
    enable_raw_mode()?;
    let mut stdout = stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let res = event_loop(&mut terminal, &mut app).await;

    // Restore terminal safely
    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;

    res
}

async fn event_loop<B: ratatui::backend::Backend>(
    terminal: &mut Terminal<B>,
    app: &mut ChatApp,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut event_stream = EventStream::new();
    let (tx, mut rx) = mpsc::channel::<StreamMsg>(100);

    // Initial draw
    terminal.draw(|f| app.render(f))?;

    loop {
        tokio::select! {
            // Crossterm keyboard events
            Some(event_res) = event_stream.next() => {
                match event_res {
                    Ok(Event::Key(key)) => {
                        match key.code {
                            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                                break;
                            }
                            KeyCode::Char('q') if !app.is_streaming && app.input_buffer.is_empty() => {
                                break;
                            }
                            KeyCode::Esc => {
                                if app.is_streaming {
                                    let _ = app.abort_stream();
                                }
                                // Esc never quits the standalone chat TUI
                            }
                            _ => {
                                app.handle_key_input(key, &tx);
                            }
                        }
                    }
                    _ => {}
                }
                terminal.draw(|f| app.render(f))?;
            }

            // Streaming tokens from background worker
            Some(msg) = rx.recv() => {
                match msg {
                    StreamMsg::Token(token) => {
                        if app.is_streaming {
                            app.handle_stream_token(token);
                            app.auto_scroll = true;
                        }
                    }
                    StreamMsg::Done => {
                        if app.is_streaming {
                            app.finalize_stream();
                            app.auto_scroll = true;
                        }
                    }
                    StreamMsg::Error(err) => {
                        if app.is_streaming {
                            app.finalize_stream();
                            let hint = if err.contains("Transport Error") || err.contains("error sending request") {
                                "\n💡 Hint: If running on mobile GPU (Vulkan), try unloading ('u') and loading via 'Local (CPU Mode)' in [F2] Models to bypass mobile GPU driver freezes."
                            } else {
                                ""
                            };
                            app.messages.push(ChatMessage::status(format!("⚠️ [Connection / Generation Error]: {}{}", err, hint)));
                            app.status_message = Some(format!("Error: {}", err));
                            app.auto_scroll = true;
                        }
                    }
                }
                terminal.draw(|f| app.render(f))?;
            }

            else => break,
        }
    }

    Ok(())
}
