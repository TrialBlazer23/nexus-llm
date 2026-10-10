use nexus::client::NexusClient;
use nexus::config::NexusConfig;
use nexus::discovery::DiscoveryService;
use nexus::ui::chat::{wrapped_line_count, ChatApp, ChatEntry, EntryKind, StreamMsg};
use nexus::ui::dashboard::DashboardApp;
use nexus::ui::models::scan_models_dir;
use ratatui::backend::TestBackend;
use ratatui::Terminal;
use std::fs::File;
use std::io::Write;
use std::sync::Arc;
use std::time::Instant;
use tempfile::tempdir;

#[test]
fn test_chat_app_state_and_token_streaming() {
    let client = NexusClient::new("http://127.0.0.1:8080");
    let mut app = ChatApp::new(
        client,
        "llama-3-8b",
        Some("You are a helpful assistant.".to_string()),
    );

    // User prompt
    app.messages.push(ChatEntry::user("Hello"));
    assert_eq!(app.messages.len(), 1);

    // Simulate streaming tokens
    app.is_streaming = true;
    app.stream_start_time = Some(Instant::now());

    app.handle_stream_token("Hello".to_string());
    app.handle_stream_token(" world!".to_string());

    assert_eq!(app.streaming_response, "Hello world!");
    assert_eq!(app.tokens_streamed, 2);

    // Finalize
    app.finalize_stream();
    assert!(!app.is_streaming);
    assert_eq!(app.messages.len(), 2);
    assert_eq!(app.messages[1].message.role, "assistant");
    assert_eq!(app.messages[1].message.content, "Hello world!");
}

#[test]
fn test_chat_tui_headless_render() {
    let client = NexusClient::new("http://127.0.0.1:8080");
    let mut app = ChatApp::new(client, "llama-3-8b", Some("System instruction".to_string()));
    app.messages.push(ChatEntry::user("Testing TUI layout"));
    app.messages
        .push(ChatEntry::assistant("Response from model"));

    let backend = TestBackend::new(100, 30);
    let mut terminal = Terminal::new(backend).expect("Failed to initialize headless TestBackend");

    terminal
        .draw(|f| app.render(f))
        .expect("Failed to render frame");

    let buffer = terminal.backend().buffer();
    let content = format!("{:?}", buffer);

    assert!(
        content.contains("Nexus-LLM Terminal"),
        "Buffer should contain header title"
    );
    assert!(
        content.contains("Conversation History"),
        "Buffer should contain history block"
    );
    assert!(
        content.contains("Prompt Input"),
        "Buffer should contain input block"
    );
    assert!(
        content.contains("Testing TUI layout"),
        "Buffer should contain user prompt"
    );
    assert!(
        content.contains("Response from model"),
        "Buffer should contain assistant reply"
    );
}

#[test]
fn test_chat_tui_multi_turn_auto_scroll() {
    let client = NexusClient::new("http://127.0.0.1:8080");
    let mut app = ChatApp::new(client, "llama-3-8b", None);

    // Simulate 15 turns of conversation (well over 50 lines)
    for i in 1..=15 {
        app.messages
            .push(ChatEntry::user(format!("Question {}", i)));
        app.messages
            .push(ChatEntry::assistant(format!("Answer {}", i)));
    }
    app.messages.push(ChatEntry::user("Followup question 16"));
    app.messages.push(ChatEntry::assistant("Latest answer 16"));

    assert!(
        app.total_lines() > 50,
        "Total lines should exceed terminal height"
    );
    assert!(app.auto_scroll, "Auto-scroll should be enabled by default");

    let backend = TestBackend::new(100, 20); // Small 20-row terminal
    let mut terminal = Terminal::new(backend).expect("Failed to initialize headless TestBackend");

    terminal
        .draw(|f| app.render(f))
        .expect("Failed to render frame");

    let buffer = terminal.backend().buffer();
    let content = format!("{:?}", buffer);

    // With auto-scroll active, the bottom messages MUST be visible in the buffer
    assert!(
        content.contains("Followup question 16"),
        "Auto-scroll must render latest user followup"
    );
    assert!(
        content.contains("Latest answer 16"),
        "Auto-scroll must render latest assistant reply"
    );
}

#[tokio::test]
async fn test_dashboard_tui_headless_render() {
    let config = NexusConfig::default();
    let discovery = Arc::new(DiscoveryService::new(config, None));
    let mut dashboard = DashboardApp::new(discovery);
    dashboard.refresh().await;

    let backend = TestBackend::new(120, 35);
    let mut terminal = Terminal::new(backend).expect("Failed to initialize headless TestBackend");

    terminal
        .draw(|f| dashboard.render(f))
        .expect("Failed to render dashboard frame");

    let buffer = terminal.backend().buffer();
    let content = format!("{:?}", buffer);

    assert!(
        content.contains("Cluster Monitor"),
        "Buffer should contain cluster monitor title"
    );
    assert!(
        content.contains("Memory Utilization"),
        "Buffer should contain memory gauge"
    );
    assert!(
        content.contains("Acceleration Tier"),
        "Buffer should contain engine capabilities"
    );
    assert!(
        content.contains("Discovered Cluster Peers"),
        "Buffer should contain peers table"
    );
}

#[test]
fn test_models_scanner() {
    let temp_dir = tempdir().expect("Failed to create tempdir");

    // File 1: Non-gguf file (should be ignored)
    let non_gguf = temp_dir.path().join("readme.txt");
    let mut f1 = File::create(&non_gguf).unwrap();
    f1.write_all(b"not a model").unwrap();

    // Scan should find 0 models
    let scanned = scan_models_dir(temp_dir.path());
    assert_eq!(scanned.len(), 0);
}

#[test]
fn test_clean_conversation_messages() {
    let raw_messages = vec![
        ChatEntry::notice("Model 'qwen2.5-3b' loaded successfully and ready for inference."),
        ChatEntry::notice(
            "Connected to remote model 'qwen2.5-3b' running on Node-12345678. Ready for inference.",
        ),
        ChatEntry::error("⚠️ [Connection / Generation Error]: Transport Error"),
        ChatEntry::notice("Model unloaded. Local inference engine is idle."),
        ChatEntry::user("What is the capital of France?"),
        ChatEntry::assistant("The capital of France is Paris."),
        ChatEntry::user("And its population?"),
    ];

    let cleaned =
        ChatApp::clean_conversation_messages(&raw_messages, Some("You are a helpful assistant."));

    assert_eq!(cleaned.len(), 4); // System instruction + 3 genuine dialogue turns
    assert_eq!(cleaned[0].role, "system");
    assert_eq!(cleaned[0].content, "You are a helpful assistant.");
    assert_eq!(cleaned[1].role, "user");
    assert_eq!(cleaned[1].content, "What is the capital of France?");
    assert_eq!(cleaned[2].role, "assistant");
    assert_eq!(cleaned[2].content, "The capital of France is Paris.");
    assert_eq!(cleaned[3].role, "user");
    assert_eq!(cleaned[3].content, "And its population?");

    // Notices/errors must not be classified as dialogue even if content looks conversational
    assert!(!raw_messages[0].is_dialogue());
    assert!(raw_messages[4].is_dialogue());
}

fn key_event(
    code: crossterm::event::KeyCode,
    modifiers: crossterm::event::KeyModifiers,
) -> crossterm::event::KeyEvent {
    crossterm::event::KeyEvent {
        code,
        modifiers,
        kind: crossterm::event::KeyEventKind::Press,
        state: crossterm::event::KeyEventState::empty(),
    }
}

#[test]
fn test_chat_cursor_navigation_and_editing() {
    let client = NexusClient::new("http://127.0.0.1:8080");
    let mut app = ChatApp::new(client, "test-model", None);
    let (tx, _rx) = tokio::sync::mpsc::channel::<StreamMsg>(10);

    // Type "hello"
    for c in "hello".chars() {
        app.handle_key_input(
            key_event(
                crossterm::event::KeyCode::Char(c),
                crossterm::event::KeyModifiers::empty(),
            ),
            &tx,
        );
    }
    assert_eq!(app.input_buffer, "hello");
    assert_eq!(app.cursor_idx, 5);

    // Navigate Left twice
    app.handle_key_input(
        key_event(
            crossterm::event::KeyCode::Left,
            crossterm::event::KeyModifiers::empty(),
        ),
        &tx,
    );
    app.handle_key_input(
        key_event(
            crossterm::event::KeyCode::Left,
            crossterm::event::KeyModifiers::empty(),
        ),
        &tx,
    );
    assert_eq!(app.cursor_idx, 3);

    // Insert 'X' in the middle -> "helXlo"
    app.handle_key_input(
        key_event(
            crossterm::event::KeyCode::Char('X'),
            crossterm::event::KeyModifiers::empty(),
        ),
        &tx,
    );
    assert_eq!(app.input_buffer, "helXlo");
    assert_eq!(app.cursor_idx, 4);

    // Backspace deletes 'X' -> "hello"
    app.handle_key_input(
        key_event(
            crossterm::event::KeyCode::Backspace,
            crossterm::event::KeyModifiers::empty(),
        ),
        &tx,
    );
    assert_eq!(app.input_buffer, "hello");
    assert_eq!(app.cursor_idx, 3);

    // Delete at cursor deletes next 'l' -> "helo"
    app.handle_key_input(
        key_event(
            crossterm::event::KeyCode::Delete,
            crossterm::event::KeyModifiers::empty(),
        ),
        &tx,
    );
    assert_eq!(app.input_buffer, "helo");
    assert_eq!(app.cursor_idx, 3);

    // Home jumps to start
    app.handle_key_input(
        key_event(
            crossterm::event::KeyCode::Home,
            crossterm::event::KeyModifiers::empty(),
        ),
        &tx,
    );
    assert_eq!(app.cursor_idx, 0);

    // End jumps to end
    app.handle_key_input(
        key_event(
            crossterm::event::KeyCode::End,
            crossterm::event::KeyModifiers::empty(),
        ),
        &tx,
    );
    assert_eq!(app.cursor_idx, 4);

    // Shift+Enter inserts newline
    app.handle_key_input(
        key_event(
            crossterm::event::KeyCode::Enter,
            crossterm::event::KeyModifiers::SHIFT,
        ),
        &tx,
    );
    assert_eq!(app.input_buffer, "helo\n");
    assert_eq!(app.cursor_idx, 5);
}

#[test]
fn test_chat_slash_command_mutations() {
    let client = NexusClient::new("http://127.0.0.1:8080");
    let mut app = ChatApp::new(client, "test-model", None);

    // Test /temp
    assert!(app.handle_slash_command("/temp 0.25"));
    assert!((app.temperature - 0.25).abs() < 0.001);

    // Test /top_p
    assert!(app.handle_slash_command("/top_p 0.95"));
    assert!((app.top_p - 0.95).abs() < 0.001);

    // Test /max_tokens
    assert!(app.handle_slash_command("/max_tokens 4096"));
    assert_eq!(app.max_tokens, 4096);

    // Test /system
    assert!(app.handle_slash_command("/system You are an expert system."));
    assert_eq!(
        app.system_prompt.as_deref(),
        Some("You are an expert system.")
    );

    // Test /preset coder
    assert!(app.handle_slash_command("/preset coder"));
    assert_eq!(
        app.active_preset.as_ref().map(|p| p.name.as_str()),
        Some("coder")
    );
    assert_eq!(app.temperature, 0.2);

    // Test /clear
    app.messages.push(ChatEntry::user("Hi"));
    assert!(app.handle_slash_command("/clear"));
    assert!(app.messages.is_empty());
}

#[test]
fn test_markdown_rendering_and_boxed_code() {
    use nexus::ui::markdown::render_markdown;

    let markdown_text = "# Test Title\n\nHere is **bold** text and `inline_code`.\n\n```rust\nfn main() {\n    println!(\"Hello!\");\n}\n```";
    let lines = render_markdown(markdown_text);

    assert!(!lines.is_empty());

    // Check header render
    let full_rendered: String = lines
        .iter()
        .map(|l| {
            l.spans
                .iter()
                .map(|s| s.content.as_ref())
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n");

    assert!(
        full_rendered.contains("Test Title"),
        "Should render heading text"
    );
    assert!(full_rendered.contains("bold"), "Should render bold text");
    assert!(
        full_rendered.contains("inline_code"),
        "Should render inline code"
    );
    assert!(
        full_rendered.contains("┌─ rust"),
        "Should format fenced code block header"
    );
    assert!(
        full_rendered.contains("└─"),
        "Should format fenced code block footer"
    );
    assert!(
        full_rendered.contains("main"),
        "Should preserve code tokens"
    );
}

#[test]
fn test_generation_metrics_and_telemetry_badge() {
    let client = NexusClient::new("http://127.0.0.1:8080");
    let mut app = ChatApp::new(client, "test-model", None);
    app.set_target_hardware("Galaxy S23 Ultra", "Vulkan Adreno 740 GPU");

    // Simulate turn
    app.messages.push(ChatEntry::user("Hello"));

    app.is_streaming = true;
    app.stream_start_time = Some(Instant::now());

    app.handle_stream_token("Hello".to_string());
    app.handle_stream_token(" from GPU!".to_string());

    assert!(
        app.ttft_ms.is_some(),
        "Time to first token should be recorded"
    );
    assert_eq!(app.tokens_streamed, 2);

    app.finalize_stream();
    assert_eq!(app.messages.len(), 2);
    assert!(
        app.messages[1].metrics.is_some(),
        "Assistant should have metrics"
    );
    let metrics = app.messages[1]
        .metrics
        .as_ref()
        .expect("Assistant should have metrics");
    assert_eq!(metrics.tokens, 2);
    assert!(metrics.tokens_per_sec > 0.0);

    // Verify Headless TUI rendering includes telemetry badges
    let backend = TestBackend::new(140, 30);
    let mut terminal = Terminal::new(backend).expect("Failed to initialize headless TestBackend");

    terminal
        .draw(|f| app.render(f))
        .expect("Failed to render frame");

    let buffer = terminal.backend().buffer();
    let content = format!("{:?}", buffer);

    assert!(
        content.contains("Galaxy S23 Ultra"),
        "Header must include target device"
    );
    assert!(
        content.contains("Vulkan Adreno 740 GPU"),
        "Header must include hardware backend"
    );
    assert!(
        content.contains("⚡"),
        "Must render generation performance badge"
    );
}

#[test]
fn test_chat_stream_abort() {
    let client = NexusClient::new("http://127.0.0.1:8080");
    let mut app = ChatApp::new(client, "test-model", None);

    app.is_streaming = true;
    app.streaming_response = "Partial text".to_string();
    app.tokens_streamed = 2;
    app.stream_start_time = Some(Instant::now());

    let (abort_tx, mut abort_rx) = tokio::sync::oneshot::channel::<()>();
    app.abort_tx = Some(abort_tx);

    app.abort_generation();

    // Verify stream was aborted cleanly
    assert!(!app.is_streaming);
    assert!(
        abort_rx.try_recv().is_ok(),
        "Abort signal must be dispatched"
    );
    assert_eq!(app.messages.len(), 1);
    assert!(app.messages[0]
        .message
        .content
        .contains("Generation stopped by operator"));
    assert_eq!(
        app.status_message.as_deref(),
        Some("Generation stopped by operator")
    );
}

#[test]
fn test_session_logger_and_markdown_export() {
    use nexus::ui::session_logger::SessionLogger;

    let dir = tempdir().expect("Failed to create tempdir");
    let export_path = dir.path().join("chat_export.md");

    let messages = [
        ChatEntry::user("Explain distributed inference"),
        ChatEntry::assistant(
            "Distributed inference offloads neural network layers across connected nodes.",
        ),
    ];
    let export_msgs: Vec<_> = messages.iter().map(|e| e.message.clone()).collect();

    let result = SessionLogger::export_to_markdown(
        &export_msgs,
        "qwen2.5-3b",
        "http://192.168.1.150:8080",
        "Vulkan Adreno 740",
        &export_path,
    );

    assert!(result.is_ok());
    assert!(export_path.exists());

    let content = std::fs::read_to_string(&export_path).unwrap();
    assert!(content.contains("qwen2.5-3b"));
    assert!(content.contains("Vulkan Adreno 740"));
    assert!(content.contains("Explain distributed inference"));
    assert!(content.contains("Distributed inference offloads"));
}

#[test]
fn test_chat_entry_kind_filters_dialogue() {
    let notice = ChatEntry::notice("Connected to the database successfully.");
    let dialogue = ChatEntry::assistant("Connected to the database successfully.");
    assert_eq!(notice.kind, EntryKind::Notice);
    assert!(!notice.is_dialogue());
    assert!(dialogue.is_dialogue());
}

#[test]
fn test_wrapped_line_count_for_wide_code() {
    use ratatui::text::Line;
    let lines = vec![
        Line::from("short"),
        Line::from("abcdefghijklmnopqrstuvwxyz0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZ"),
    ];
    let count = wrapped_line_count(&lines, 20);
    assert!(
        count >= 4,
        "wide line must wrap into multiple visual rows, got {count}"
    );
}

#[test]
fn test_hot_swap_intent_preserves_zero_ngl() {
    use nexus::ui::hub::commands::effective_ngl;
    use nexus::ui::hub::HotSwapIntent;
    use std::path::PathBuf;

    let intent = HotSwapIntent {
        path: PathBuf::from("/models/m.gguf"),
        gpu_layers: Some(0),
        context_size: 2048,
        extra_args: Vec::new(),
        moe_cache_ceil_mb: None,
    };
    assert_eq!(intent.gpu_layers, Some(0));
    assert_eq!(effective_ngl(intent.gpu_layers, true, 99), 0);
}
