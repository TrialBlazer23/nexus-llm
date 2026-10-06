use nexus::client::{ChatMessage, NexusClient};
use nexus::config::NexusConfig;
use nexus::discovery::DiscoveryService;
use nexus::ui::chat::{ChatApp, TransportBadge};
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
    let mut app = ChatApp::new(client, "llama-3-8b", Some("You are a helpful assistant.".to_string()));

    // User prompt
    app.messages.push(ChatMessage::user("Hello"));
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
    assert_eq!(app.is_streaming, false);
    assert_eq!(app.messages.len(), 2);
    assert_eq!(app.messages[1].role, "assistant");
    assert_eq!(app.messages[1].content, "Hello world!");
}

#[test]
fn test_chat_tui_headless_render() {
    let client = NexusClient::new("http://127.0.0.1:8080");
    let mut app = ChatApp::new(client, "llama-3-8b", Some("System instruction".to_string()));
    app.messages.push(ChatMessage::user("Testing TUI layout"));
    app.messages.push(ChatMessage::assistant("Response from model"));

    let backend = TestBackend::new(100, 30);
    let mut terminal = Terminal::new(backend).expect("Failed to initialize headless TestBackend");

    terminal.draw(|f| app.render(f)).expect("Failed to render frame");

    let buffer = terminal.backend().buffer();
    let content = format!("{:?}", buffer);

    assert!(content.contains("Nexus-LLM Terminal"), "Buffer should contain header title");
    assert!(content.contains("Conversation History"), "Buffer should contain history block");
    assert!(content.contains("Prompt Input"), "Buffer should contain input block");
    assert!(content.contains("Testing TUI layout"), "Buffer should contain user prompt");
    assert!(content.contains("Response from model"), "Buffer should contain assistant reply");
}

#[test]
fn test_chat_tui_multi_turn_auto_scroll() {
    let client = NexusClient::new("http://127.0.0.1:8080");
    let mut app = ChatApp::new(client, "llama-3-8b", None);

    // Simulate 15 turns of conversation (well over 50 lines)
    for i in 1..=15 {
        app.messages.push(ChatMessage::user(format!("Question {}", i)));
        app.messages.push(ChatMessage::assistant(format!("Answer {}", i)));
    }
    app.messages.push(ChatMessage::user("Followup question 16"));
    app.messages.push(ChatMessage::assistant("Latest answer 16"));

    assert!(app.total_lines() > 50, "Total lines should exceed terminal height");
    assert!(app.auto_scroll, "Auto-scroll should be enabled by default");

    let backend = TestBackend::new(100, 20); // Small 20-row terminal
    let mut terminal = Terminal::new(backend).expect("Failed to initialize headless TestBackend");

    terminal.draw(|f| app.render(f)).expect("Failed to render frame");

    let buffer = terminal.backend().buffer();
    let content = format!("{:?}", buffer);

    // With auto-scroll active, the bottom messages MUST be visible in the buffer
    assert!(content.contains("Followup question 16"), "Auto-scroll must render latest user followup");
    assert!(content.contains("Latest answer 16"), "Auto-scroll must render latest assistant reply");
}

#[tokio::test]
async fn test_dashboard_tui_headless_render() {
    let config = NexusConfig::default();
    let discovery = Arc::new(DiscoveryService::new(config, None));
    let mut dashboard = DashboardApp::new(discovery);
    dashboard.refresh().await;

    let backend = TestBackend::new(120, 35);
    let mut terminal = Terminal::new(backend).expect("Failed to initialize headless TestBackend");

    terminal.draw(|f| dashboard.render(f)).expect("Failed to render dashboard frame");

    let buffer = terminal.backend().buffer();
    let content = format!("{:?}", buffer);

    assert!(content.contains("Cluster Monitor"), "Buffer should contain cluster monitor title");
    assert!(content.contains("Memory Utilization"), "Buffer should contain memory gauge");
    assert!(content.contains("Acceleration Tier"), "Buffer should contain engine capabilities");
    assert!(content.contains("Discovered Cluster Peers"), "Buffer should contain peers table");
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
        ChatMessage::status("Model 'qwen2.5-3b' loaded successfully and ready for inference."),
        ChatMessage::status("Connected to remote model 'qwen2.5-3b' running on Node-12345678. Ready for inference."),
        ChatMessage::status("⚠️ [Connection / Generation Error]: Transport Error"),
        ChatMessage::status("Model unloaded. Local inference engine is idle."),
        ChatMessage::user("What is the capital of France?"),
        ChatMessage::assistant("The capital of France is Paris."),
        ChatMessage::user("And its population?"),
        // User text that looks like a banner must survive (no prefix filtering)
        ChatMessage::user("Model 'x' is great for coding."),
    ];

    let cleaned = ChatApp::clean_conversation_messages(&raw_messages, Some("You are a helpful assistant."));

    assert_eq!(cleaned.len(), 5); // System instruction + 4 genuine dialogue turns
    assert_eq!(cleaned[0].role, "system");
    assert_eq!(cleaned[0].content, "You are a helpful assistant.");
    assert_eq!(cleaned[1].role, "user");
    assert_eq!(cleaned[1].content, "What is the capital of France?");
    assert_eq!(cleaned[2].role, "assistant");
    assert_eq!(cleaned[2].content, "The capital of France is Paris.");
    assert_eq!(cleaned[3].role, "user");
    assert_eq!(cleaned[3].content, "And its population?");
    assert_eq!(cleaned[4].role, "user");
    assert_eq!(cleaned[4].content, "Model 'x' is great for coding.");
    assert!(cleaned.iter().all(|m| !m.is_status()));
}

#[test]
fn test_transport_badge_localhost_is_local_not_usb() {
    let client = NexusClient::new("http://127.0.0.1:8080");
    let app = ChatApp::new(client, "llama-3-8b", None);
    assert_eq!(app.transport_badge, TransportBadge::Local);
    assert_eq!(app.transport_badge.label(), "[Local]");

    let wifi = NexusClient::new("http://192.168.1.50:8080");
    let app_wifi = ChatApp::new(wifi, "llama-3-8b", None);
    assert_eq!(app_wifi.transport_badge, TransportBadge::Wifi);

    let backend = TestBackend::new(100, 30);
    let mut terminal = Terminal::new(backend).unwrap();
    let mut app = ChatApp::new(NexusClient::new("http://127.0.0.1:8080"), "m", None);
    app.transport_badge = TransportBadge::Local;
    terminal.draw(|f| app.render(f)).unwrap();
    let content = format!("{:?}", terminal.backend().buffer());
    assert!(content.contains("[Local]"), "localhost supervised inference must show [Local]");
    assert!(!content.contains("[USB Cable]"), "must not mislabel local as USB Cable");

    app.transport_badge = TransportBadge::Usb;
    terminal.draw(|f| app.render(f)).unwrap();
    let content = format!("{:?}", terminal.backend().buffer());
    assert!(content.contains("[USB Cable]"), "explicit USB badge must render");
}

#[test]
fn test_abort_stream_clears_streaming_state() {
    let client = NexusClient::new("http://127.0.0.1:8080");
    let mut app = ChatApp::new(client, "llama-3-8b", None);
    app.is_streaming = true;
    app.stream_start_time = Some(Instant::now());
    app.streaming_response = "partial reply".to_string();

    assert!(app.abort_stream());
    assert!(!app.is_streaming);
    assert!(app.stream_start_time.is_none());
    assert_eq!(app.messages.last().unwrap().content, "partial reply");
    assert!(app
        .status_message
        .as_ref()
        .unwrap()
        .contains("aborted"));

    // Late tokens after abort must be ignored
    app.handle_stream_token("should-ignore".to_string());
    assert!(app.streaming_response.is_empty());
    assert!(!app.is_streaming);
}

#[test]
fn test_apply_preset_sets_hyperparams() {
    let client = NexusClient::new("http://127.0.0.1:8080");
    let mut app = ChatApp::new(client, "llama-3-8b", None);
    app.apply_preset("coder", "Be terse.".to_string(), 0.2, 4096);
    assert_eq!(app.persona_name.as_deref(), Some("coder"));
    assert_eq!(app.system_prompt.as_deref(), Some("Be terse."));
    assert_eq!(app.temperature, 0.2);
    assert_eq!(app.max_tokens, 4096);
}

#[test]
fn test_chat_cursor_navigation_and_editing() {
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use tokio::sync::mpsc;

    let client = NexusClient::new("http://127.0.0.1:8080");
    let mut app = ChatApp::new(client, "llama-3-8b", None);
    let (tx, _rx) = mpsc::channel(8);

    for c in ['h', 'e', 'l', 'l', 'o'] {
        app.handle_key_input(
            KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE),
            &tx,
        );
    }
    assert_eq!(app.input_buffer, "hello");
    assert_eq!(app.cursor_idx, 5);

    app.handle_key_input(KeyEvent::new(KeyCode::Left, KeyModifiers::NONE), &tx);
    app.handle_key_input(KeyEvent::new(KeyCode::Left, KeyModifiers::NONE), &tx);
    assert_eq!(app.cursor_idx, 3);

    app.handle_key_input(KeyEvent::new(KeyCode::Char('X'), KeyModifiers::NONE), &tx);
    assert_eq!(app.input_buffer, "helXlo");
    assert_eq!(app.cursor_idx, 4);

    app.handle_key_input(KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE), &tx);
    assert_eq!(app.input_buffer, "hello");
    assert_eq!(app.cursor_idx, 3);

    app.handle_key_input(KeyEvent::new(KeyCode::Delete, KeyModifiers::NONE), &tx);
    assert_eq!(app.input_buffer, "helo");
    assert_eq!(app.cursor_idx, 3);

    app.handle_key_input(KeyEvent::new(KeyCode::Home, KeyModifiers::NONE), &tx);
    assert_eq!(app.cursor_idx, 0);
    app.handle_key_input(KeyEvent::new(KeyCode::End, KeyModifiers::NONE), &tx);
    assert_eq!(app.cursor_idx, 4);
}

#[test]
fn test_chat_prompt_history_alt_arrows() {
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use tokio::sync::mpsc;

    let client = NexusClient::new("http://127.0.0.1:8080");
    let mut app = ChatApp::new(client, "llama-3-8b", None);
    let (tx, _rx) = mpsc::channel(8);

    app.prompt_history = vec!["first".into(), "second".into()];
    app.handle_key_input(KeyEvent::new(KeyCode::Up, KeyModifiers::ALT), &tx);
    assert_eq!(app.input_buffer, "second");
    app.handle_key_input(KeyEvent::new(KeyCode::Up, KeyModifiers::ALT), &tx);
    assert_eq!(app.input_buffer, "first");
    app.handle_key_input(KeyEvent::new(KeyCode::Down, KeyModifiers::ALT), &tx);
    assert_eq!(app.input_buffer, "second");
}

#[test]
fn test_markdown_rendering_and_boxed_code() {
    use nexus::ui::markdown::render_markdown;

    let markdown_text = "# Test Title\n\nHere is **bold** text and `inline_code`.\n\n```rust\nfn main() {\n    println!(\"Hello!\");\n}\n```";
    let lines = render_markdown(markdown_text);
    assert!(!lines.is_empty(), "markdown should produce lines");
    let flat: String = lines
        .iter()
        .flat_map(|l| l.spans.iter().map(|s| s.content.clone()))
        .collect::<Vec<_>>()
        .join("");
    assert!(flat.contains("Test Title"), "heading text should appear");
    assert!(flat.contains("main"), "code fence body should appear");
    assert!(
        flat.contains("─") || flat.contains("│"),
        "code block should use box drawing"
    );
}

