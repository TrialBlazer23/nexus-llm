use nexus::client::{ChatMessage, NexusClient};
use nexus::config::NexusConfig;
use nexus::discovery::DiscoveryService;
use nexus::ui::chat::ChatApp;
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
