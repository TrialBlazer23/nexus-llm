use nexus::client::NexusClient;
use nexus::config::NexusConfig;
use nexus::discovery::DiscoveryService;
use nexus::ui::hub::{HubApp, HubTab};
use nexus::ui::models_view::ModelsView;
use nexus::ui::settings_view::SettingsView;
use nexus::ui::tunnel_view::TunnelView;
use ratatui::backend::TestBackend;
use ratatui::Terminal;
use std::fs::File;
use std::io::Write;
use std::path::PathBuf;
use std::sync::Arc;
use tempfile::tempdir;

fn write_str(buf: &mut Vec<u8>, s: &str) {
    buf.extend_from_slice(&(s.len() as u64).to_le_bytes());
    buf.extend_from_slice(s.as_bytes());
}

fn build_synthetic_gguf(arch: &str, name: &str) -> Vec<u8> {
    let mut buf = Vec::new();
    // 1. Magic "GGUF" (0x46554747 in LE)
    buf.extend_from_slice(&0x46554747u32.to_le_bytes());
    // 2. Version 3
    buf.extend_from_slice(&3u32.to_le_bytes());
    // 3. Tensor count: 32
    buf.extend_from_slice(&32u64.to_le_bytes());
    // 4. Metadata KV count: 4
    buf.extend_from_slice(&4u64.to_le_bytes());

    // KV 1: "general.architecture" -> String
    write_str(&mut buf, "general.architecture");
    buf.extend_from_slice(&8u32.to_le_bytes()); // Type 8 = String
    write_str(&mut buf, arch);

    // KV 2: "general.name" -> String
    write_str(&mut buf, "general.name");
    buf.extend_from_slice(&8u32.to_le_bytes());
    write_str(&mut buf, name);

    // KV 3: "llama.context_length" -> UInt32 4096
    write_str(&mut buf, "llama.context_length");
    buf.extend_from_slice(&4u32.to_le_bytes());
    buf.extend_from_slice(&4096u32.to_le_bytes());

    // KV 4: "llama.block_count" -> UInt32 16
    write_str(&mut buf, "llama.block_count");
    buf.extend_from_slice(&4u32.to_le_bytes());
    buf.extend_from_slice(&16u32.to_le_bytes());

    buf
}

#[test]
fn test_hub_tab_cycling_and_titles() {
    let config = NexusConfig::default();
    let discovery = Arc::new(DiscoveryService::new(config.clone(), None));
    let client = NexusClient::new("http://127.0.0.1:8080");
    let mut hub = HubApp::new(config, client, discovery);

    // Initial state: Chat
    assert_eq!(hub.active_tab, HubTab::Chat);
    assert!(HubTab::Chat.title().contains("Chat"));

    // Cycle forward through all tabs
    hub.next_tab();
    assert_eq!(hub.active_tab, HubTab::Models);
    assert!(HubTab::Models.title().contains("Models"));

    hub.next_tab();
    assert_eq!(hub.active_tab, HubTab::Dashboard);
    assert!(HubTab::Dashboard.title().contains("Dashboard"));

    hub.next_tab();
    assert_eq!(hub.active_tab, HubTab::Tunnel);
    assert!(HubTab::Tunnel.title().contains("Tunnel"));

    hub.next_tab();
    assert_eq!(hub.active_tab, HubTab::Settings);
    assert!(HubTab::Settings.title().contains("Settings"));

    // Wraparound to Chat
    hub.next_tab();
    assert_eq!(hub.active_tab, HubTab::Chat);

    // Cycle backward
    hub.previous_tab();
    assert_eq!(hub.active_tab, HubTab::Settings);

    hub.previous_tab();
    assert_eq!(hub.active_tab, HubTab::Tunnel);

    hub.previous_tab();
    assert_eq!(hub.active_tab, HubTab::Dashboard);

    hub.previous_tab();
    assert_eq!(hub.active_tab, HubTab::Models);

    hub.previous_tab();
    assert_eq!(hub.active_tab, HubTab::Chat);
}

#[test]
fn test_models_view_navigation_and_selection() {
    let dir = tempdir().expect("Failed to create tempdir");
    let model1_path = dir.path().join("model-a.gguf");
    let model2_path = dir.path().join("model-b.gguf");

    // Write valid synthetic GGUF files
    File::create(&model1_path).unwrap().write_all(&build_synthetic_gguf("llama", "Model-A")).unwrap();
    File::create(&model2_path).unwrap().write_all(&build_synthetic_gguf("llama", "Model-B")).unwrap();

    let mut view = ModelsView::new(dir.path().to_path_buf());
    assert_eq!(view.models.len(), 2);
    assert_eq!(view.selected_index, 0);

    let sel = view.selected_model().expect("Should have selected model");
    assert_eq!(sel.filename, "model-a.gguf");
    assert_eq!(sel.architecture, "llama");

    // Move next
    view.next();
    assert_eq!(view.selected_index, 1);
    let sel2 = view.selected_model().expect("Should have selected model");
    assert_eq!(sel2.filename, "model-b.gguf");

    // Move next again (wraps around to 0)
    view.next();
    assert_eq!(view.selected_index, 0);

    // Move previous (wraps to 1)
    view.previous();
    assert_eq!(view.selected_index, 1);

    // Move previous again (to 0)
    view.previous();
    assert_eq!(view.selected_index, 0);
}

#[test]
fn test_settings_view_navigation_and_mutations() {
    let dir = tempdir().expect("Failed to create tempdir");
    let config_path = dir.path().join("test_config.toml");

    let mut config = NexusConfig::default();
    config.hardware.acceleration.cpu_threads = 4;
    config.hardware.acceleration.prefer_gpu = true;
    config.hardware.safety.max_ram_usage_percent = 75;

    let mut view = SettingsView::new(config);

    // Verify categories and initial selected field
    assert!(view.item_count() > 0);
    assert_eq!(view.selected_index, 0);
    assert_eq!(view.current_item().category, "Hardware & Acceleration");

    // Find and mutate prefer_gpu (boolean toggle)
    let gpu_idx = SettingsView::items().iter().position(|i| i.name == "Prefer Vulkan GPU Acceleration").expect("prefer_gpu field must exist");
    view.selected_index = gpu_idx;
    assert_eq!(view.config.hardware.acceleration.prefer_gpu, true);

    // Toggle boolean
    view.toggle_or_adjust(false, false);
    assert_eq!(view.config.hardware.acceleration.prefer_gpu, false);
    assert!(view.status_message.is_some());

    // Toggle boolean back
    view.toggle_or_adjust(false, false);
    assert_eq!(view.config.hardware.acceleration.prefer_gpu, true);

    // Find and adjust cpu_threads (numeric adjustments)
    let threads_idx = SettingsView::items().iter().position(|i| i.name.contains("CPU Compute Threads")).expect("cpu_threads field must exist");
    view.selected_index = threads_idx;
    assert_eq!(view.config.hardware.acceleration.cpu_threads, 4);

    // Adjust left (decrement)
    view.toggle_or_adjust(true, false);
    assert_eq!(view.config.hardware.acceleration.cpu_threads, 3);

    // Adjust right (increment)
    view.toggle_or_adjust(false, true);
    assert_eq!(view.config.hardware.acceleration.cpu_threads, 4);

    // Save settings to disk via save_to
    let save_res = view.save_to(&config_path);
    assert!(save_res.is_ok(), "Settings must save without error");
    assert!(config_path.exists(), "Config file must exist on disk after save");

    // Verify written TOML
    let loaded = NexusConfig::load_from_path(&config_path).expect("Must load saved config");
    assert_eq!(loaded.hardware.acceleration.prefer_gpu, true);
    assert_eq!(loaded.hardware.acceleration.cpu_threads, 4);
    assert_eq!(loaded.hardware.safety.max_ram_usage_percent, 75);
}

#[test]
fn test_tunnel_view_lifecycle() {
    let mut view = TunnelView::new(8080, 50052);
    assert_eq!(view.api_port, 8080);
    assert_eq!(view.rpc_port, 50052);
    assert!(view.status.is_none());

    // Refresh should not panic regardless of adb presence
    view.refresh();
}

#[tokio::test]
async fn test_hub_app_headless_render_all_tabs() {
    let dir = tempdir().expect("Failed to create tempdir");
    let dummy_model = dir.path().join("tiny-llama.gguf");
    File::create(&dummy_model).unwrap().write_all(&build_synthetic_gguf("llama", "TinyLlama")).unwrap();

    let mut config = NexusConfig::default();
    config.node.models_dir = dir.path().to_path_buf();

    let discovery = Arc::new(DiscoveryService::new(config.clone(), None));
    let client = NexusClient::new("http://127.0.0.1:8080");
    let mut hub = HubApp::new(config, client, discovery);

    let backend = TestBackend::new(120, 35);
    let mut terminal = Terminal::new(backend).expect("Failed to initialize TestBackend");

    // 1. Render Tab 0: Chat
    hub.set_tab(HubTab::Chat);
    terminal.draw(|f| hub.render(f)).expect("Failed to render Chat tab");
    let buffer = terminal.backend().buffer();
    let content = format!("{:?}", buffer);
    assert!(content.contains("[F1] 💬 Chat"), "Must render top tab bar with Chat tab");
    assert!(content.contains("Nexus-LLM Terminal"), "Must render chat sub-view");

    // 2. Render Tab 1: Models
    hub.set_tab(HubTab::Models);
    terminal.draw(|f| hub.render(f)).expect("Failed to render Models tab");
    let buffer = terminal.backend().buffer();
    let content = format!("{:?}", buffer);
    assert!(content.contains("[F2] 📦 Models"), "Must render Models tab active");
    assert!(content.contains("Local Models (1)"), "Must render models list pane");
    assert!(content.contains("Model Architecture & Metadata"), "Must render metadata inspector pane");
    assert!(content.contains("tiny-llama.gguf"), "Must list discovered model");

    // 3. Render Tab 2: Dashboard
    hub.set_tab(HubTab::Dashboard);
    terminal.draw(|f| hub.render(f)).expect("Failed to render Dashboard tab");
    let buffer = terminal.backend().buffer();
    let content = format!("{:?}", buffer);
    assert!(content.contains("[F3] 🖥️ Dashboard"), "Must render Dashboard tab active");
    assert!(content.contains("Cluster Monitor"), "Must render cluster monitor header");
    assert!(content.contains("Memory Utilization"), "Must render memory utilization gauge");

    // 4. Render Tab 3: Tunnel
    hub.set_tab(HubTab::Tunnel);
    terminal.draw(|f| hub.render(f)).expect("Failed to render Tunnel tab");
    let buffer = terminal.backend().buffer();
    let content = format!("{:?}", buffer);
    assert!(content.contains("[F4] 🔗 USB Tunnel"), "Must render Tunnel tab active");
    assert!(content.contains("Hardware Transport & USB Device Status"), "Must render tunnel view header");
    assert!(content.contains("ADB Runtime:"), "Must render ADB runtime status");

    // 5. Render Tab 4: Settings
    hub.set_tab(HubTab::Settings);
    terminal.draw(|f| hub.render(f)).expect("Failed to render Settings tab");
    let buffer = terminal.backend().buffer();
    let content = format!("{:?}", buffer);
    assert!(content.contains("[F5] ⚙️ Settings"), "Must render Settings tab active");
    assert!(content.contains("Configuration & Settings"), "Must render settings editor header");
    assert!(content.contains("Hardware & Acceleration"), "Must render Hardware category");
    assert!(content.contains("Memory & Android LMK Safeguards"), "Must render Memory Safety category");
}

#[tokio::test]
async fn test_hub_app_hot_swap_confirmation_modal() {
    let config = NexusConfig::default();
    let discovery = Arc::new(DiscoveryService::new(config.clone(), None));
    let client = NexusClient::new("http://127.0.0.1:8080");
    let mut hub = HubApp::new(config, client, discovery);

    // Simulate pending hot swap
    hub.pending_hot_swap_path = Some(PathBuf::from("/models/llama-3.2-3b.gguf"));

    let backend = TestBackend::new(120, 35);
    let mut terminal = Terminal::new(backend).expect("Failed to initialize TestBackend");

    terminal.draw(|f| hub.render(f)).expect("Failed to render frame with modal");
    let buffer = terminal.backend().buffer();
    let content = format!("{:?}", buffer);

    assert!(content.contains("Hot-Swap"), "Must render modal title");
    assert!(content.contains("llama-3.2-3b.gguf"), "Must display target model filename in modal");
    assert!(content.contains("[Y / N]"), "Must display confirmation prompt");
}
