use crate::client::{ChatCompletionRequest, ChatMessage, NexusClient};
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

pub enum StreamMsg {
    Token(String),
    Done,
    Error(String),
}

/// State container for the interactive TUI chat session.
pub struct ChatApp {
    pub client: NexusClient,
    pub model_name: String,
    pub system_prompt: Option<String>,
    pub messages: Vec<ChatMessage>,
    pub streaming_response: String,
    pub is_streaming: bool,
    pub input_buffer: String,
    pub scroll_offset: u16,
    pub tokens_streamed: usize,
    pub tokens_per_sec: f64,
    pub stream_start_time: Option<Instant>,
    pub status_message: Option<String>,
}

impl ChatApp {
    pub fn new(client: NexusClient, model_name: impl Into<String>, system_prompt: Option<String>) -> Self {
        Self {
            client,
            model_name: model_name.into(),
            system_prompt,
            messages: Vec::new(),
            streaming_response: String::new(),
            is_streaming: false,
            input_buffer: String::new(),
            scroll_offset: 0,
            tokens_streamed: 0,
            tokens_per_sec: 0.0,
            stream_start_time: None,
            status_message: None,
        }
    }

    /// Process a stream chunk received from background worker.
    pub fn handle_stream_token(&mut self, token: String) {
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
    }

    /// Prepare and render the UI frame.
    pub fn render(&self, frame: &mut Frame) {
        let area = frame.area();

        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .margin(1)
            .constraints([
                Constraint::Length(3), // Header bar
                Constraint::Min(8),    // Chat history area
                Constraint::Length(3), // Input box
                Constraint::Length(1), // Help / status footer
            ])
            .split(area);

        self.render_header(frame, chunks[0]);
        self.render_chat_history(frame, chunks[1]);
        self.render_input_box(frame, chunks[2]);
        self.render_footer(frame, chunks[3]);
    }

    fn render_header(&self, frame: &mut Frame, area: Rect) {
        let stream_info = if self.is_streaming {
            format!(" | Generating: {:.1} tokens/s", self.tokens_per_sec)
        } else {
            String::new()
        };

        let title = format!(
            " Nexus-LLM Terminal | Host: {} | Model: {}{}",
            self.client.endpoint(),
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
                Span::styled(sys, Style::default().fg(Color::DarkGray)),
            ]));
            lines.push(Line::from(""));
        }

        for msg in &self.messages {
            let (label, color) = if msg.role == "user" {
                (" [You] ", Color::Blue)
            } else {
                (" [Nexus] ", Color::Green)
            };

            lines.push(Line::from(vec![
                Span::styled(label, Style::default().fg(color).add_modifier(Modifier::BOLD)),
            ]));

            for text_line in msg.content.lines() {
                lines.push(Line::from(Span::styled(
                    format!("   {}", text_line),
                    Style::default().fg(Color::White),
                )));
            }
            lines.push(Line::from(""));
        }

        // Display current active streaming generation
        if self.is_streaming || !self.streaming_response.is_empty() {
            lines.push(Line::from(vec![
                Span::styled(" [Nexus] ", Style::default().fg(Color::Green).add_modifier(Modifier::BOLD)),
                Span::styled("(generating...)", Style::default().fg(Color::Yellow)),
            ]));

            for text_line in self.streaming_response.lines() {
                lines.push(Line::from(Span::styled(
                    format!("   {}", text_line),
                    Style::default().fg(Color::LightGreen),
                )));
            }
        }

        let history = Paragraph::new(lines)
            .block(
                Block::default()
                    .title(" Conversation History ")
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(Color::LightBlue)),
            )
            .scroll((self.scroll_offset, 0))
            .wrap(Wrap { trim: false });

        frame.render_widget(history, area);
    }

    fn render_input_box(&self, frame: &mut Frame, area: Rect) {
        let input_text = format!("> {}", self.input_buffer);
        let border_color = if self.is_streaming {
            Color::Yellow
        } else {
            Color::White
        };

        let input_widget = Paragraph::new(input_text)
            .block(
                Block::default()
                    .title(" Prompt Input ")
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(border_color)),
            );

        frame.render_widget(input_widget, area);
    }

    fn render_footer(&self, frame: &mut Frame, area: Rect) {
        let text = if let Some(status) = &self.status_message {
            status.clone()
        } else {
            "[Enter] Submit  |  [Esc / Ctrl+C] Exit  |  [Up/Down] Scroll".to_string()
        };

        let footer = Paragraph::new(Line::from(vec![
            Span::styled(text, Style::default().fg(Color::DarkGray)),
        ]));

        frame.render_widget(footer, area);
    }
}

/// Run full interactive Ratatui TUI chat session.
pub async fn run_chat_tui(mut app: ChatApp) -> Result<(), Box<dyn std::error::Error>> {
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
                            KeyCode::Esc => {
                                break;
                            }
                            KeyCode::Char(c) => {
                                app.input_buffer.push(c);
                            }
                            KeyCode::Backspace => {
                                app.input_buffer.pop();
                            }
                            KeyCode::Up => {
                                app.scroll_offset = app.scroll_offset.saturating_add(1);
                            }
                            KeyCode::Down => {
                                app.scroll_offset = app.scroll_offset.saturating_sub(1);
                            }
                            KeyCode::Enter => {
                                if !app.is_streaming && !app.input_buffer.trim().is_empty() {
                                    let prompt = std::mem::take(&mut app.input_buffer);
                                    app.messages.push(ChatMessage::user(&prompt));
                                    app.is_streaming = true;
                                    app.streaming_response.clear();
                                    app.tokens_streamed = 0;
                                    app.tokens_per_sec = 0.0;
                                    app.stream_start_time = Some(Instant::now());

                                    // Build full request messages
                                    let mut req_messages = Vec::new();
                                    if let Some(sys) = &app.system_prompt {
                                        req_messages.push(ChatMessage::system(sys));
                                    }
                                    req_messages.extend(app.messages.clone());

                                    let req = ChatCompletionRequest {
                                        model: app.model_name.clone(),
                                        messages: req_messages,
                                        temperature: Some(0.7),
                                        max_tokens: Some(2048),
                                        stream: true,
                                    };

                                    let client = app.client.clone();
                                    let tx_clone = tx.clone();

                                    tokio::spawn(async move {
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
                                    });
                                }
                            }
                            _ => {}
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
                        app.handle_stream_token(token);
                    }
                    StreamMsg::Done => {
                        app.finalize_stream();
                    }
                    StreamMsg::Error(err) => {
                        app.finalize_stream();
                        app.status_message = Some(format!("Error: {}", err));
                    }
                }
                terminal.draw(|f| app.render(f))?;
            }

            else => break,
        }
    }

    Ok(())
}
