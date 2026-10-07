use crate::client::{ChatCompletionRequest, ChatMessage, NexusClient};
use crate::preset::Preset;
use crate::sysinfo::AccelerationBackend;
use crate::ui::markdown::render_markdown;
use crate::ui::session_logger::SessionLogger;
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
    widgets::{Block, Borders, Clear, Paragraph, Wrap},
    Frame, Terminal,
};
use std::io::stdout;
use std::path::Path;
use std::time::Instant;
use tokio::sync::{mpsc, oneshot};

pub enum StreamMsg {
    Token(String),
    Done,
    Error(String),
    Abort,
}

fn backend_from_label(label: &str) -> AccelerationBackend {
    let l = label.to_lowercase();
    if l.contains("vulkan") {
        AccelerationBackend::Vulkan
    } else if l.contains("arm") || l.contains("dotprod") {
        AccelerationBackend::ArmCpuDotProd
    } else if l.contains("x86") || l.contains("sse") {
        AccelerationBackend::X86Baseline
    } else {
        AccelerationBackend::GenericCpu
    }
}

/// Telemetry metrics recorded for each assistant generation turn.
#[derive(Debug, Clone, PartialEq)]
pub struct GenerationMetrics {
    pub tokens: usize,
    pub duration_secs: f64,
    pub tokens_per_sec: f64,
    pub ttft_ms: Option<u64>,
}

/// Classification for history entries — set at construction, never by string prefix.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntryKind {
    Dialogue,
    Notice,
    Error,
}

/// Single chat history row: message + kind + optional metrics + cached markdown lines.
#[derive(Debug, Clone)]
pub struct ChatEntry {
    pub message: ChatMessage,
    pub kind: EntryKind,
    pub metrics: Option<GenerationMetrics>,
    pub rendered: Option<Vec<Line<'static>>>,
}

impl ChatEntry {
    pub fn dialogue(message: ChatMessage) -> Self {
        Self {
            message,
            kind: EntryKind::Dialogue,
            metrics: None,
            rendered: None,
        }
    }

    pub fn user(content: impl Into<String>) -> Self {
        Self::dialogue(ChatMessage::user(content))
    }

    pub fn assistant(content: impl Into<String>) -> Self {
        Self::dialogue(ChatMessage::assistant(content))
    }

    pub fn notice(content: impl Into<String>) -> Self {
        Self {
            message: ChatMessage::assistant(content),
            kind: EntryKind::Notice,
            metrics: None,
            rendered: None,
        }
    }

    pub fn error(content: impl Into<String>) -> Self {
        Self {
            message: ChatMessage::assistant(content),
            kind: EntryKind::Error,
            metrics: None,
            rendered: None,
        }
    }

    pub fn with_metrics(mut self, metrics: GenerationMetrics) -> Self {
        self.metrics = Some(metrics);
        self
    }

    pub fn is_dialogue(&self) -> bool {
        self.kind == EntryKind::Dialogue
    }

    /// Lazily cache markdown render; safe because message content is immutable after push.
    pub fn ensure_rendered(&mut self) -> &[Line<'static>] {
        if self.rendered.is_none() {
            self.rendered = Some(render_markdown(&self.message.content));
        }
        self.rendered.as_ref().unwrap()
    }

    pub fn rendered_lines(&self) -> Vec<Line<'static>> {
        if let Some(lines) = &self.rendered {
            lines.clone()
        } else {
            render_markdown(&self.message.content)
        }
    }
}

/// Count visual lines after wrapping `lines` to `width` columns.
pub fn wrapped_line_count(lines: &[Line<'_>], width: u16) -> usize {
    let w = width.max(1) as usize;
    lines
        .iter()
        .map(|line| {
            let lw = line.width().max(1);
            lw.div_ceil(w)
        })
        .sum()
}

/// State container for the interactive TUI chat session.
pub struct ChatApp {
    pub client: NexusClient,
    pub model_name: String,
    pub system_prompt: Option<String>,
    pub messages: Vec<ChatEntry>,
    pub streaming_response: String,
    pub streaming_rendered: Option<Vec<Line<'static>>>,
    pub is_streaming: bool,
    pub input_buffer: String,
    pub cursor_idx: usize,
    pub scroll_offset: usize,
    /// Last computed wrapped history length (shared by keys + render).
    pub last_wrapped_lines: usize,
    /// Viewport inner width used for wrap math.
    pub last_viewport_width: u16,
    pub auto_scroll: bool,
    pub tokens_streamed: usize,
    pub tokens_per_sec: f64,
    pub stream_start_time: Option<Instant>,
    pub first_token_time: Option<Instant>,
    pub ttft_ms: Option<u64>,
    pub status_message: Option<String>,

    // Hyperparameters & Preset Configuration
    pub active_preset: Option<Preset>,
    pub temperature: f32,
    pub top_p: f32,
    pub max_tokens: usize,

    // Hardware Telemetry
    pub target_device_name: String,
    pub target_backend: String,

    // Stream Cancellation
    pub abort_tx: Option<oneshot::Sender<()>>,

    // Session Persistence
    pub session_logger: SessionLogger,

    // Preset Selection Modal
    pub show_preset_modal: bool,
    pub preset_candidates: Vec<String>,
    pub selected_preset_idx: usize,
}

impl ChatApp {
    pub fn new(
        client: NexusClient,
        model_name: impl Into<String>,
        system_prompt: Option<String>,
    ) -> Self {
        let model_str = model_name.into();
        let session_logger = SessionLogger::new(&model_str, client.endpoint());

        let mut app = Self {
            client,
            model_name: model_str,
            system_prompt,
            messages: Vec::new(),
            streaming_response: String::new(),
            streaming_rendered: None,
            is_streaming: false,
            input_buffer: String::new(),
            cursor_idx: 0,
            scroll_offset: 0,
            last_wrapped_lines: 0,
            last_viewport_width: 80,
            auto_scroll: true,
            tokens_streamed: 0,
            tokens_per_sec: 0.0,
            stream_start_time: None,
            first_token_time: None,
            ttft_ms: None,
            status_message: None,
            active_preset: None,
            temperature: 0.7,
            top_p: 0.9,
            max_tokens: 2048,
            target_device_name: "Local Host".to_string(),
            target_backend: "Auto".to_string(),
            abort_tx: None,
            session_logger,
            show_preset_modal: false,
            preset_candidates: vec!["general".to_string(), "coder".to_string()],
            selected_preset_idx: 0,
        };
        app.refresh_preset_candidates();
        app
    }

    /// Update target node hardware labels (e.g. S23 Ultra Vulkan GPU).
    pub fn set_target_hardware(&mut self, device: impl Into<String>, backend: impl Into<String>) {
        self.target_device_name = device.into();
        self.target_backend = backend.into();
    }

    /// Refresh list of available presets from `presets/` directory.
    pub fn refresh_preset_candidates(&mut self) {
        let mut list = vec!["general".to_string(), "coder".to_string()];
        let presets_dir = Path::new("presets");
        if let Ok(entries) = std::fs::read_dir(presets_dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if let Some(ext) = path.extension() {
                    if ext == "yaml" || ext == "yml" {
                        if let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
                            let s = stem.to_string();
                            if !list.contains(&s) {
                                list.push(s);
                            }
                        }
                    }
                }
            }
        }
        self.preset_candidates = list;
    }

    /// Apply persona preset configurations to active chat generation.
    pub fn apply_preset(&mut self, preset: Preset) {
        self.system_prompt = Some(preset.system_prompt.clone());
        self.temperature = preset.temperature;
        self.top_p = preset.top_p;
        self.max_tokens = preset.max_tokens;
        let p_name = preset.name.clone();
        self.active_preset = Some(preset);
        self.status_message = Some(format!(
            "Preset '{}' applied (temp: {}, top_p: {}, max_tokens: {})",
            p_name, self.temperature, self.top_p, self.max_tokens
        ));
    }

    /// Load preset by name from filesystem or built-ins.
    pub fn load_preset_by_name(&mut self, name: &str) -> Result<(), String> {
        let preset = Preset::load_by_name(name, Path::new("presets"))
            .map_err(|e| format!("Failed to load preset '{}': {}", name, e))?;
        self.apply_preset(preset);
        Ok(())
    }

    /// Abort active streaming generation gracefully.
    pub fn abort_generation(&mut self) {
        if let Some(tx) = self.abort_tx.take() {
            let _ = tx.send(());
        }
        if self.is_streaming {
            if !self.streaming_response.is_empty() {
                self.streaming_response
                    .push_str("\n\n*[Generation stopped by operator]*");
            }
            self.finalize_stream();
            self.status_message = Some("Generation stopped by operator".to_string());
        }
    }

    /// Process slash commands entered in prompt buffer.
    pub fn handle_slash_command(&mut self, input: &str) -> bool {
        let trimmed = input.trim();
        if !trimmed.starts_with('/') {
            return false;
        }

        let parts: Vec<&str> = trimmed.split_whitespace().collect();
        let cmd = parts[0].to_lowercase();

        match cmd.as_str() {
            "/help" => {
                let help_text = "Nexus-LLM Chat Commands:\n\
                                 • /preset <name>       Switch persona (e.g. coder, general)\n\
                                 • /temp <0.0-2.0>      Set generation temperature (current: self.temp)\n\
                                 • /top_p <0.0-1.0>     Set nucleus sampling top_p (current: self.top_p)\n\
                                 • /max_tokens <num>    Set max response token cap\n\
                                 • /system <prompt>     Set active system instruction\n\
                                 • /clear               Reset chat dialogue history\n\
                                 • /export [path]       Export formatted Markdown transcript\n\
                                 • /stop or /cancel     Halt active token generation\n\
                                 • /unload              Unload active model\n\
                                 Hotkeys: [Alt+P / F5] Preset Picker | [Esc] Abort Stream | [Shift+Enter] Newline";
                self.messages.push(ChatEntry::notice(help_text));
            }

            "/clear" => {
                self.messages.clear();
                self.status_message = Some("Conversation history cleared".to_string());
            }

            "/stop" | "/cancel" => {
                self.abort_generation();
            }

            "/preset" => {
                if parts.len() < 2 {
                    self.status_message = Some(format!(
                        "Active preset: {}. Available: {}",
                        self.active_preset
                            .as_ref()
                            .map(|p| p.name.as_str())
                            .unwrap_or("none"),
                        self.preset_candidates.join(", ")
                    ));
                } else {
                    let target = parts[1];
                    match self.load_preset_by_name(target) {
                        Ok(()) => {}
                        Err(e) => {
                            self.status_message = Some(e);
                        }
                    }
                }
            }

            "/temp" | "/temperature" => {
                if let Some(val_str) = parts.get(1) {
                    if let Ok(val) = val_str.parse::<f32>() {
                        self.temperature = val.clamp(0.0, 2.0);
                        self.status_message =
                            Some(format!("Temperature set to {:.2}", self.temperature));
                    } else {
                        self.status_message =
                            Some("Invalid float for /temp (e.g. /temp 0.7)".to_string());
                    }
                } else {
                    self.status_message =
                        Some(format!("Current temperature: {:.2}", self.temperature));
                }
            }

            "/top_p" => {
                if let Some(val_str) = parts.get(1) {
                    if let Ok(val) = val_str.parse::<f32>() {
                        self.top_p = val.clamp(0.0, 1.0);
                        self.status_message = Some(format!("Top-p set to {:.2}", self.top_p));
                    } else {
                        self.status_message =
                            Some("Invalid float for /top_p (e.g. /top_p 0.9)".to_string());
                    }
                } else {
                    self.status_message = Some(format!("Current top_p: {:.2}", self.top_p));
                }
            }

            "/max_tokens" => {
                if let Some(val_str) = parts.get(1) {
                    if let Ok(val) = val_str.parse::<usize>() {
                        self.max_tokens = val;
                        self.status_message =
                            Some(format!("Max tokens set to {}", self.max_tokens));
                    } else {
                        self.status_message = Some(
                            "Invalid number for /max_tokens (e.g. /max_tokens 2048)".to_string(),
                        );
                    }
                } else {
                    self.status_message = Some(format!("Current max_tokens: {}", self.max_tokens));
                }
            }

            "/system" => {
                if parts.len() < 2 {
                    self.status_message = Some(format!(
                        "System prompt: {}",
                        self.system_prompt.as_deref().unwrap_or("none")
                    ));
                } else {
                    let prompt = parts[1..].join(" ");
                    self.system_prompt = Some(prompt.clone());
                    self.status_message = Some("System prompt updated".to_string());
                }
            }

            "/export" => {
                let default_name = format!("chats/export-{}.md", chrono_placeholder());
                let path = parts.get(1).copied().unwrap_or(&default_name);
                let export_msgs: Vec<ChatMessage> =
                    self.messages.iter().map(|e| e.message.clone()).collect();
                match SessionLogger::export_to_markdown(
                    &export_msgs,
                    &self.model_name,
                    self.client.endpoint(),
                    &self.target_backend,
                    path,
                ) {
                    Ok(p) => {
                        self.status_message = Some(format!("Exported chat to {:?}", p));
                    }
                    Err(e) => {
                        self.status_message = Some(format!("Export failed: {}", e));
                    }
                }
            }

            _ => {
                return false;
            }
        }

        true
    }

    /// Build a cleaned list of dialogue messages for OpenAI /v1/chat/completions.
    pub fn clean_conversation_messages(
        messages: &[ChatEntry],
        system_prompt: Option<&str>,
    ) -> Vec<ChatMessage> {
        let mut req_messages = Vec::new();
        if let Some(sys) = system_prompt {
            req_messages.push(ChatMessage::system(sys));
        }
        for entry in messages {
            if entry.is_dialogue() {
                req_messages.push(entry.message.clone());
            }
        }
        req_messages
    }

    /// Build laid-out history lines (for render + wrap math). Mutates render caches.
    pub fn build_history_lines(&mut self) -> Vec<Line<'static>> {
        let mut lines = Vec::new();

        if let Some(sys) = &self.system_prompt {
            lines.push(Line::from(vec![
                Span::styled(
                    " [System] ",
                    Style::default()
                        .fg(Color::Magenta)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(sys.clone(), Style::default().fg(Color::DarkGray)),
            ]));
            lines.push(Line::from(""));
        }

        for entry in &mut self.messages {
            let (label, color) = match entry.kind {
                EntryKind::Dialogue if entry.message.role == "user" => (" [You] ", Color::Blue),
                EntryKind::Dialogue => (" [Nexus] ", Color::Green),
                EntryKind::Notice => (" [Status] ", Color::Cyan),
                EntryKind::Error => (" [Error] ", Color::LightRed),
            };

            lines.push(Line::from(vec![Span::styled(
                label,
                Style::default().fg(color).add_modifier(Modifier::BOLD),
            )]));

            let rendered = entry.ensure_rendered().to_vec();
            for m_line in rendered {
                let mut indented_spans = vec![Span::raw("   ")];
                indented_spans.extend(m_line.spans);
                lines.push(Line::from(indented_spans));
            }

            if entry.message.role == "assistant" {
                if let Some(metrics) = &entry.metrics {
                    let ttft_label = if let Some(ttft) = metrics.ttft_ms {
                        format!(" · TTFT: {}ms", ttft)
                    } else {
                        String::new()
                    };
                    let badge_text = format!(
                        "   ⚡ {:.1} t/s · {} tokens · {:.2}s{}",
                        metrics.tokens_per_sec, metrics.tokens, metrics.duration_secs, ttft_label
                    );
                    lines.push(Line::from(Span::styled(
                        badge_text,
                        Style::default()
                            .fg(Color::Rgb(140, 160, 180))
                            .add_modifier(Modifier::DIM),
                    )));
                }
            }

            lines.push(Line::from(""));
        }

        if self.is_streaming || !self.streaming_response.is_empty() {
            lines.push(Line::from(vec![
                Span::styled(
                    " [Nexus] ",
                    Style::default()
                        .fg(Color::Green)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled("(generating...)", Style::default().fg(Color::Yellow)),
            ]));

            if self.streaming_rendered.is_none() {
                self.streaming_rendered = Some(render_markdown(&self.streaming_response));
            }
            // Re-render streaming text each token (content changes)
            let rendered_markdown = render_markdown(&self.streaming_response);
            self.streaming_rendered = Some(rendered_markdown.clone());
            for m_line in rendered_markdown {
                let mut indented_spans = vec![Span::raw("   ")];
                indented_spans.extend(m_line.spans);
                lines.push(Line::from(indented_spans));
            }
        }

        lines
    }

    /// Wrapped visual line count for the last laid-out history (call after build/update).
    pub fn total_lines(&self) -> usize {
        if self.last_wrapped_lines > 0 {
            self.last_wrapped_lines
        } else {
            // Fallback before first render
            let mut count = 0;
            if self.system_prompt.is_some() {
                count += 2;
            }
            for entry in &self.messages {
                count += 1 + entry.message.content.lines().count() + 2;
            }
            if self.is_streaming || !self.streaming_response.is_empty() {
                count += 1 + self.streaming_response.lines().count() + 1;
            }
            count
        }
    }

    /// Sync wrap metrics from a viewport width (used by keys after render updates).
    pub fn update_wrap_metrics(&mut self, lines: &[Line<'static>], width: u16) {
        self.last_viewport_width = width.max(1);
        self.last_wrapped_lines = wrapped_line_count(lines, self.last_viewport_width);
        if self.auto_scroll {
            self.scroll_offset = self.last_wrapped_lines;
        } else {
            self.scroll_offset = self.scroll_offset.min(self.last_wrapped_lines);
        }
    }

    /// Process a stream chunk received from background worker.
    pub fn handle_stream_token(&mut self, token: String) {
        if self.first_token_time.is_none() {
            self.first_token_time = Some(Instant::now());
            if let Some(start) = self.stream_start_time {
                self.ttft_ms = Some(start.elapsed().as_millis() as u64);
            }
        }

        self.streaming_response.push_str(&token);
        self.streaming_rendered = None;
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
            let elapsed = self
                .stream_start_time
                .map(|s| s.elapsed().as_secs_f64())
                .unwrap_or(0.0);

            let metrics = GenerationMetrics {
                tokens: self.tokens_streamed,
                duration_secs: elapsed,
                tokens_per_sec: self.tokens_per_sec,
                ttft_ms: self.ttft_ms,
            };

            // Log turn to persistent history
            self.session_logger.log_turn(
                &self.model_name,
                self.client.endpoint(),
                "assistant",
                &content,
                Some(self.tokens_per_sec),
                Some(self.tokens_streamed),
            );

            // Best-effort Phase 12 §5.5 telemetry into ~/.nexus/bench.json
            if metrics.tokens > 0 && metrics.tokens_per_sec > 0.0 {
                let backend = backend_from_label(&self.target_backend);
                let prompt_tok_s = metrics.ttft_ms.map(|ms| {
                    let secs = (ms as f32 / 1000.0).max(1e-3);
                    // Rough prefill estimate from TTFT alone (prompt size unknown here).
                    1.0 / secs
                });
                crate::bench::BenchStore::record_default_best_effort(
                    &self.model_name,
                    &self.target_device_name,
                    backend,
                    2048,
                    crate::bench::BenchSample {
                        gen_tok_s: metrics.tokens_per_sec as f32,
                        ttft_ms: metrics.ttft_ms,
                        prompt_tok_s,
                        measured_at: crate::bench::unix_now(),
                    },
                );
            }

            self.messages
                .push(ChatEntry::assistant(content).with_metrics(metrics));
        }

        self.is_streaming = false;
        self.streaming_rendered = None;
        self.stream_start_time = None;
        self.first_token_time = None;
        self.abort_tx = None;
    }

    /// Prepare and render the UI frame for full window.
    pub fn render(&mut self, frame: &mut Frame) {
        let area = frame.area();
        self.render_in_area(frame, area);
    }

    /// Prepare and render the UI frame within a specified sub-area.
    pub fn render_in_area(&mut self, frame: &mut Frame, area: Rect) {
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

        if self.show_preset_modal {
            self.render_preset_modal(frame, area);
        }
    }

    /// Handle key event for input buffering, scrolling, or dispatching streaming requests.
    pub fn handle_key_input(
        &mut self,
        key: crossterm::event::KeyEvent,
        tx: &mpsc::Sender<StreamMsg>,
    ) {
        // Preset modal interaction
        if self.show_preset_modal {
            match key.code {
                KeyCode::Up | KeyCode::Char('k') => {
                    if self.selected_preset_idx > 0 {
                        self.selected_preset_idx -= 1;
                    } else if !self.preset_candidates.is_empty() {
                        self.selected_preset_idx = self.preset_candidates.len() - 1;
                    }
                    return;
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    if !self.preset_candidates.is_empty() {
                        self.selected_preset_idx =
                            (self.selected_preset_idx + 1) % self.preset_candidates.len();
                    }
                    return;
                }
                KeyCode::Enter => {
                    if let Some(target) = self
                        .preset_candidates
                        .get(self.selected_preset_idx)
                        .cloned()
                    {
                        let _ = self.load_preset_by_name(&target);
                    }
                    self.show_preset_modal = false;
                    return;
                }
                KeyCode::Esc => {
                    self.show_preset_modal = false;
                    return;
                }
                _ => return,
            }
        }

        // Preset Modal hotkey: Alt+P or F5
        if (key.modifiers.contains(KeyModifiers::ALT)
            && (key.code == KeyCode::Char('p') || key.code == KeyCode::Char('P')))
            || key.code == KeyCode::F(5)
        {
            self.refresh_preset_candidates();
            self.show_preset_modal = !self.show_preset_modal;
            return;
        }

        // Active Stream Abort: Esc cancels generation without quitting app
        if self.is_streaming && key.code == KeyCode::Esc {
            self.abort_generation();
            return;
        }

        match key.code {
            KeyCode::Left => {
                self.cursor_idx = self.cursor_idx.saturating_sub(1);
            }
            KeyCode::Right => {
                let char_count = self.input_buffer.chars().count();
                if self.cursor_idx < char_count {
                    self.cursor_idx += 1;
                }
            }
            KeyCode::Home => {
                self.cursor_idx = 0;
            }
            KeyCode::End => {
                self.cursor_idx = self.input_buffer.chars().count();
            }
            KeyCode::Backspace => {
                if self.cursor_idx > 0 {
                    let mut chars: Vec<char> = self.input_buffer.chars().collect();
                    chars.remove(self.cursor_idx - 1);
                    self.input_buffer = chars.into_iter().collect();
                    self.cursor_idx -= 1;
                }
            }
            KeyCode::Delete => {
                let char_count = self.input_buffer.chars().count();
                if self.cursor_idx < char_count {
                    let mut chars: Vec<char> = self.input_buffer.chars().collect();
                    chars.remove(self.cursor_idx);
                    self.input_buffer = chars.into_iter().collect();
                }
            }
            KeyCode::Char(c) => {
                let mut chars: Vec<char> = self.input_buffer.chars().collect();
                chars.insert(self.cursor_idx, c);
                self.input_buffer = chars.into_iter().collect();
                self.cursor_idx += 1;
            }
            KeyCode::Up => {
                let max = self.total_lines();
                let current = if self.auto_scroll {
                    max
                } else {
                    self.scroll_offset
                };
                self.auto_scroll = false;
                self.scroll_offset = current.saturating_sub(1);
            }
            KeyCode::Down => {
                self.scroll_offset = self.scroll_offset.saturating_add(1);
                if self.scroll_offset >= self.total_lines() {
                    self.auto_scroll = true;
                }
            }
            KeyCode::PageUp => {
                let max = self.total_lines();
                let current = if self.auto_scroll {
                    max
                } else {
                    self.scroll_offset
                };
                self.auto_scroll = false;
                self.scroll_offset = current.saturating_sub(10);
            }
            KeyCode::PageDown => {
                self.auto_scroll = true;
            }
            KeyCode::Enter => {
                // Multi-line continuation with Shift or Alt, or trailing '\'
                if key.modifiers.contains(KeyModifiers::SHIFT)
                    || key.modifiers.contains(KeyModifiers::ALT)
                {
                    let mut chars: Vec<char> = self.input_buffer.chars().collect();
                    chars.insert(self.cursor_idx, '\n');
                    self.input_buffer = chars.into_iter().collect();
                    self.cursor_idx += 1;
                    return;
                }

                if !self.is_streaming && !self.input_buffer.trim().is_empty() {
                    let input = std::mem::take(&mut self.input_buffer);
                    self.cursor_idx = 0;

                    // Check if it's a slash command
                    if self.handle_slash_command(&input) {
                        return;
                    }

                    // Log user turn
                    self.session_logger.log_turn(
                        &self.model_name,
                        self.client.endpoint(),
                        "user",
                        &input,
                        None,
                        None,
                    );

                    self.messages.push(ChatEntry::user(&input));

                    self.is_streaming = true;
                    self.auto_scroll = true;
                    self.status_message = None;
                    self.streaming_response.clear();
                    self.tokens_streamed = 0;
                    self.tokens_per_sec = 0.0;
                    self.stream_start_time = Some(Instant::now());
                    self.first_token_time = None;
                    self.ttft_ms = None;

                    let req_messages = Self::clean_conversation_messages(
                        &self.messages,
                        self.system_prompt.as_deref(),
                    );

                    let req = ChatCompletionRequest {
                        model: self.model_name.clone(),
                        messages: req_messages,
                        temperature: Some(self.temperature),
                        top_p: Some(self.top_p),
                        max_tokens: Some(self.max_tokens),
                        stream: true,
                    };

                    let (abort_tx, mut abort_rx) = oneshot::channel::<()>();
                    self.abort_tx = Some(abort_tx);

                    let client = self.client.clone();
                    let tx_clone = tx.clone();

                    tokio::spawn(async move {
                        match client.stream_chat(req).await {
                            Ok(mut stream) => loop {
                                tokio::select! {
                                    _ = &mut abort_rx => {
                                        let _ = tx_clone.send(StreamMsg::Abort).await;
                                        break;
                                    }
                                    maybe_chunk = stream.next() => {
                                        match maybe_chunk {
                                            Some(Ok(token)) => {
                                                let _ = tx_clone.send(StreamMsg::Token(token)).await;
                                            }
                                            Some(Err(e)) => {
                                                let _ = tx_clone.send(StreamMsg::Error(e.to_string())).await;
                                                break;
                                            }
                                            None => {
                                                let _ = tx_clone.send(StreamMsg::Done).await;
                                                break;
                                            }
                                        }
                                    }
                                }
                            },
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

    fn render_header(&self, frame: &mut Frame, area: Rect) {
        let stream_info = if self.is_streaming {
            let ttft_str = if let Some(ttft) = self.ttft_ms {
                format!(" | TTFT: {}ms", ttft)
            } else {
                String::new()
            };
            format!(" | ⚡ {:.1} t/s{}", self.tokens_per_sec, ttft_str)
        } else {
            String::new()
        };

        let transport_badge = if self.client.endpoint().contains("127.0.0.1")
            || self.client.endpoint().contains("localhost")
        {
            "[USB Cable]"
        } else {
            "[Wi-Fi]"
        };

        let preset_badge = if let Some(p) = &self.active_preset {
            format!(" | Preset: {}", p.name)
        } else {
            String::new()
        };

        let title = format!(
            " Nexus-LLM Terminal | Host: {} {} | [{}: {}] | Model: {}{}{}",
            self.client.endpoint(),
            transport_badge,
            self.target_device_name,
            self.target_backend,
            self.model_name,
            preset_badge,
            stream_info
        );

        let header = Paragraph::new(Line::from(vec![Span::styled(
            title,
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        )]))
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::DarkGray)),
        );

        frame.render_widget(header, area);
    }

    fn render_chat_history(&mut self, frame: &mut Frame, area: Rect) {
        let lines = self.build_history_lines();
        // Inner width accounts for borders + indent margin used by Paragraph
        let inner_width = area.width.saturating_sub(4).max(1);
        self.update_wrap_metrics(&lines, inner_width);

        let viewport_height = area.height.saturating_sub(2) as usize;
        let visual_total = self.last_wrapped_lines;
        let max_scroll = visual_total.saturating_sub(viewport_height);

        let scroll_y = if self.auto_scroll {
            max_scroll
        } else {
            self.scroll_offset.min(max_scroll)
        };
        self.scroll_offset = scroll_y;

        let history = Paragraph::new(lines.clone())
            .block(
                Block::default()
                    .title(" Conversation History ")
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(Color::LightBlue)),
            )
            .scroll((scroll_y.min(u16::MAX as usize) as u16, 0))
            .wrap(Wrap { trim: false });

        frame.render_widget(history, area);

        // Scrollbar grounded on the same wrapped count as key handling
        if visual_total > viewport_height && area.width > 2 {
            let scrollbar_area = Rect {
                x: area.x + area.width.saturating_sub(1),
                y: area.y.saturating_add(1),
                width: 1,
                height: area.height.saturating_sub(2),
            };
            let mut state =
                ratatui::widgets::ScrollbarState::new(max_scroll.max(1)).position(scroll_y);
            frame.render_stateful_widget(
                ratatui::widgets::Scrollbar::new(
                    ratatui::widgets::ScrollbarOrientation::VerticalRight,
                )
                .begin_symbol(Some("↑"))
                .end_symbol(Some("↓")),
                scrollbar_area,
                &mut state,
            );
        }
    }
    fn render_input_box(&self, frame: &mut Frame, area: Rect) {
        let border_color = if self.is_streaming {
            Color::Yellow
        } else {
            Color::White
        };

        // Render input buffer with visible cursor position
        let mut spans = vec![Span::raw("> ")];
        let chars: Vec<char> = self.input_buffer.chars().collect();
        for (i, &c) in chars.iter().enumerate() {
            if i == self.cursor_idx {
                spans.push(Span::styled(
                    c.to_string(),
                    Style::default().fg(Color::Black).bg(Color::White),
                ));
            } else {
                spans.push(Span::raw(c.to_string()));
            }
        }
        if self.cursor_idx >= chars.len() {
            spans.push(Span::styled(
                " ",
                Style::default().fg(Color::Black).bg(Color::White),
            ));
        }

        let input_widget = Paragraph::new(Line::from(spans)).block(
            Block::default()
                .title(" Prompt Input (Type /help for commands) ")
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
            (
                status.clone(),
                Style::default().fg(color).add_modifier(Modifier::BOLD),
            )
        } else {
            (
                format!(
                    "[Enter] Send | Esc Abort | Alt+P Preset | temp={:.2} top_p={:.2} max={} | /help",
                    self.temperature, self.top_p, self.max_tokens
                ),
                Style::default().fg(Color::DarkGray),
            )
        };

        let footer = Paragraph::new(Line::from(vec![Span::styled(text, style)]));

        frame.render_widget(footer, area);
    }

    fn render_preset_modal(&self, frame: &mut Frame, area: Rect) {
        let modal_area = centered_rect(50, 40, area);
        frame.render_widget(Clear, modal_area);

        let mut lines = vec![
            Line::from(vec![Span::styled(
                " Select Persona Preset ",
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            )]),
            Line::from(""),
        ];

        for (i, name) in self.preset_candidates.iter().enumerate() {
            let is_sel = i == self.selected_preset_idx;
            let prefix = if is_sel { " > " } else { "   " };
            let style = if is_sel {
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(Color::White)
            };
            lines.push(Line::from(vec![
                Span::styled(prefix, style),
                Span::styled(format!("[{}] {}", i + 1, name), style),
            ]));
        }

        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            " [↑/↓] Navigate | [Enter] Apply Preset | [Esc] Close ",
            Style::default().fg(Color::DarkGray),
        )));

        let block = Paragraph::new(lines).block(
            Block::default()
                .title(" Persona Presets ")
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::Yellow)),
        );

        frame.render_widget(block, modal_area);
    }
}

fn centered_rect(percent_x: u16, percent_y: u16, r: Rect) -> Rect {
    let popup_layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage((100 - percent_y) / 2),
            Constraint::Percentage(percent_y),
            Constraint::Percentage((100 - percent_y) / 2),
        ])
        .split(r);

    Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - percent_x) / 2),
            Constraint::Percentage(percent_x),
            Constraint::Percentage((100 - percent_x) / 2),
        ])
        .split(popup_layout[1])[1]
}

fn chrono_placeholder() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
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
                if let Ok(Event::Key(key)) = event_res {
                    // Global quit on Ctrl+C when NOT streaming
                    if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
                        if app.is_streaming {
                            app.abort_generation();
                        } else {
                            break;
                        }
                    } else if key.code == KeyCode::Esc && !app.is_streaming && !app.show_preset_modal {
                        break;
                    } else {
                        app.handle_key_input(key, &tx);
                    }
                }
                terminal.draw(|f| app.render(f))?;
            }

            // Streaming tokens from background worker
            Some(msg) = rx.recv() => {
                match msg {
                    StreamMsg::Token(token) => {
                        app.handle_stream_token(token);
                        app.auto_scroll = true;
                    }
                    StreamMsg::Done => {
                        app.finalize_stream();
                        app.auto_scroll = true;
                    }
                    StreamMsg::Abort => {
                        app.finalize_stream();
                        app.status_message = Some("Generation aborted".to_string());
                        app.auto_scroll = true;
                    }
                    StreamMsg::Error(err) => {
                        app.finalize_stream();
                        let hint = if err.contains("Transport Error") || err.contains("error sending request") {
                            "\n💡 Hint: If running on mobile GPU (Vulkan), try unloading ('u') and loading via 'Local (CPU Mode)' in [F2] Models to bypass mobile GPU driver freezes."
                        } else {
                            ""
                        };
                        app.messages.push(ChatEntry::error(format!("⚠️ [Connection / Generation Error]: {}{}", err, hint)));
                        app.status_message = Some(format!("Error: {}", err));
                        app.auto_scroll = true;
                    }
                }
                terminal.draw(|f| app.render(f))?;
            }

            else => break,
        }
    }

    Ok(())
}
