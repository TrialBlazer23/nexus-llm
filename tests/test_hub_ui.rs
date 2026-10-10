use nexus::client::NexusClient;
use nexus::config::NexusConfig;
use nexus::discovery::DiscoveryService;
use nexus::trust_auth::TrustBootstrap;
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

fn test_hub(config: NexusConfig, client: NexusClient, discovery: Arc<DiscoveryService>) -> HubApp {
    let trust = TrustBootstrap::load(config.clone()).expect("trust bootstrap");
    HubApp::new(
        config,
        client,
        discovery,
        trust.identity,
        trust.config,
        trust.config_path,
    )
}

fn build_synthetic_gguf(arch: &str, name: &str) -> Vec<u8> {
    let mut buf = Vec::new();
    // 1. Magic "GGUF" (0x46554747 in LE)
    buf.extend_from_slice(&0x46554747u32.to_le_bytes());
    // 2. Version 3
    buf.extend_from_slice(&3u32.to_le_bytes());
    // 3. Tensor count: 0 (KV-only fixture; tensor section covered in unit tests)
    buf.extend_from_slice(&0u64.to_le_bytes());
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
    let mut hub = test_hub(config, client, discovery);

    // Initial state: Chat
    assert_eq!(hub.active_tab, HubTab::Chat);
    assert!(HubTab::Chat.title().contains("Chat"));

    // Cycle forward through all tabs
    hub.next_tab();
    assert_eq!(hub.active_tab, HubTab::Models);
    assert!(HubTab::Models.title().contains("Models"));

    hub.next_tab();
    assert_eq!(hub.active_tab, HubTab::Cluster);
    assert!(HubTab::Cluster.title().contains("Cluster"));

    hub.next_tab();
    assert_eq!(hub.active_tab, HubTab::Settings);
    assert!(HubTab::Settings.title().contains("Settings"));

    hub.next_tab();
    assert_eq!(hub.active_tab, HubTab::Tunnel);
    assert!(HubTab::Tunnel.title().contains("Tunnel"));

    hub.next_tab();
    assert_eq!(hub.active_tab, HubTab::Agents);
    assert!(HubTab::Agents.title().contains("Agents"));

    hub.next_tab();
    assert_eq!(hub.active_tab, HubTab::Logs);
    assert!(HubTab::Logs.title().contains("Logs"));

    // Wraparound to Chat
    hub.next_tab();
    assert_eq!(hub.active_tab, HubTab::Chat);

    // Cycle backward
    hub.previous_tab();
    assert_eq!(hub.active_tab, HubTab::Logs);

    hub.previous_tab();
    assert_eq!(hub.active_tab, HubTab::Agents);

    hub.previous_tab();
    assert_eq!(hub.active_tab, HubTab::Tunnel);

    hub.previous_tab();
    assert_eq!(hub.active_tab, HubTab::Settings);

    hub.previous_tab();
    assert_eq!(hub.active_tab, HubTab::Cluster);

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
    File::create(&model1_path)
        .unwrap()
        .write_all(&build_synthetic_gguf("llama", "Model-A"))
        .unwrap();
    File::create(&model2_path)
        .unwrap()
        .write_all(&build_synthetic_gguf("llama", "Model-B"))
        .unwrap();

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
fn test_models_view_apply_remote_catalogs_in_memory() {
    use nexus::control_plane::{ModelCatalogEntry, ModelCatalogResponse};
    use uuid::Uuid;

    let dir = tempdir().expect("Failed to create tempdir");
    let model1_path = dir.path().join("local-model.gguf");
    File::create(&model1_path)
        .unwrap()
        .write_all(&build_synthetic_gguf("llama", "Local-Model"))
        .unwrap();

    let mut view = ModelsView::new(dir.path().to_path_buf());
    assert_eq!(view.models.len(), 1);
    assert_eq!(view.catalog.len(), 1);

    // Apply remote peer catalogs without rescanning disk
    let remotes = vec![(
        "MacBook-Host".to_string(),
        "http://192.168.1.50:52021".to_string(),
        ModelCatalogResponse {
            protocol_version: 1,
            node_id: Uuid::new_v4(),
            models: vec![ModelCatalogEntry {
                filename: "remote-mac-model.gguf".to_string(),
                size_mb: 2048,
                architecture: "llama".to_string(),
                context_length: 4096,
                digest: "remote123".to_string(),
                size_bytes: 2048 * 1024 * 1024,
            }],
        },
    )];

    view.apply_remote_catalogs(&remotes);
    assert_eq!(
        view.catalog.len(),
        2,
        "Should contain both local and remote models"
    );

    // Local model is still present and valid
    let local = view
        .catalog
        .iter()
        .find(|r| r.filename == "local-model.gguf");
    assert!(local.is_some());
    assert!(local.unwrap().local.is_some());

    // Remote model is present
    let remote = view
        .catalog
        .iter()
        .find(|r| r.filename == "remote-mac-model.gguf");
    assert!(remote.is_some());
    assert!(remote.unwrap().local.is_none());
    assert_eq!(remote.unwrap().holders, vec!["MacBook-Host"]);
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
    assert_eq!(view.current_item().category, "Node Identity");

    // Test enum cycling on Node Mesh Role
    let role_idx = SettingsView::items()
        .iter()
        .position(|i| i.name == "Node Mesh Role")
        .expect("role field must exist");
    view.selected_index = role_idx;
    assert_eq!(view.config.node.role, "host");
    view.toggle_or_adjust(false, true); // cycle right
    assert_eq!(view.config.node.role, "client");
    view.toggle_or_adjust(true, false); // cycle left
    assert_eq!(view.config.node.role, "host");

    // Test text editing on Node Hostname / Name
    let name_idx = SettingsView::items()
        .iter()
        .position(|i| i.name.contains("Node Hostname"))
        .expect("name field must exist");
    view.selected_index = name_idx;
    assert!(view.is_current_text());
    view.start_editing();
    assert!(view.editing_text);
    view.push_char('-');
    view.push_char('1');
    view.commit_text();
    assert!(!view.editing_text);
    assert!(view.config.node.name.ends_with("-1"));

    // Find and mutate prefer_gpu (boolean toggle)
    let gpu_idx = SettingsView::items()
        .iter()
        .position(|i| i.name == "Prefer Vulkan GPU Acceleration")
        .expect("prefer_gpu field must exist");
    view.selected_index = gpu_idx;
    assert!(view.config.hardware.acceleration.prefer_gpu);

    // Toggle boolean
    view.toggle_or_adjust(false, false);
    assert!(!view.config.hardware.acceleration.prefer_gpu);
    assert!(view.status_message.is_some());

    // Toggle boolean back
    view.toggle_or_adjust(false, false);
    assert!(view.config.hardware.acceleration.prefer_gpu);

    // Find and adjust cpu_threads (numeric adjustments)
    let threads_idx = SettingsView::items()
        .iter()
        .position(|i| i.name.contains("CPU Compute Threads"))
        .expect("cpu_threads field must exist");
    view.selected_index = threads_idx;
    assert_eq!(view.config.hardware.acceleration.cpu_threads, 4);

    // Adjust left (decrement)
    view.toggle_or_adjust(true, false);
    assert_eq!(view.config.hardware.acceleration.cpu_threads, 3);

    // Adjust right (increment)
    view.toggle_or_adjust(false, true);
    assert_eq!(view.config.hardware.acceleration.cpu_threads, 4);

    // Test text editing on Hugging Face Access Token
    let hf_idx = SettingsView::items()
        .iter()
        .position(|i| i.name.contains("Hugging Face Access Token"))
        .expect("hf_token field must exist");
    view.selected_index = hf_idx;
    assert!(view.is_current_text());
    view.start_editing();
    assert!(view.editing_text);
    view.text_buffer = "hf_0123456789abcdef".to_string();
    view.commit_text();
    assert!(!view.editing_text);
    assert_eq!(
        view.config.huggingface.token.as_deref(),
        Some("hf_0123456789abcdef")
    );
    assert_eq!(
        nexus::ui::settings_view::mask_hf_token("hf_0123456789abcdef"),
        "hf_•••••••• (19 chars)"
    );

    // Save settings to disk via save_to
    let save_res = view.save_to(&config_path);
    assert!(save_res.is_ok(), "Settings must save without error");
    assert!(
        config_path.exists(),
        "Config file must exist on disk after save"
    );

    // Verify written TOML
    let loaded = NexusConfig::load_from_path(&config_path).expect("Must load saved config");
    assert_eq!(loaded.node.role, "host");
    assert!(loaded.node.name.ends_with("-1"));
    assert!(loaded.hardware.acceleration.prefer_gpu);
    assert_eq!(loaded.hardware.acceleration.cpu_threads, 4);
    assert_eq!(loaded.hardware.safety.max_ram_usage_percent, 75);
    assert_eq!(
        loaded.huggingface.token.as_deref(),
        Some("hf_0123456789abcdef")
    );
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
    File::create(&dummy_model)
        .unwrap()
        .write_all(&build_synthetic_gguf("llama", "TinyLlama"))
        .unwrap();

    let mut config = NexusConfig::default();
    config.node.models_dir = dir.path().to_path_buf();

    let discovery = Arc::new(DiscoveryService::new(config.clone(), None));
    let client = NexusClient::new("http://127.0.0.1:8080");
    let mut hub = test_hub(config, client, discovery);

    let backend = TestBackend::new(140, 35);
    let mut terminal = Terminal::new(backend).expect("Failed to initialize TestBackend");

    // 1. Render Tab 0: Chat
    hub.set_tab(HubTab::Chat);
    terminal
        .draw(|f| hub.render(f))
        .expect("Failed to render Chat tab");
    let buffer = terminal.backend().buffer();
    let content = format!("{:?}", buffer);
    assert!(
        content.contains("[F1] 💬 Chat"),
        "Must render top tab bar with Chat tab"
    );
    assert!(
        content.contains("Nexus-LLM Terminal"),
        "Must render chat sub-view"
    );

    // 2. Render Tab 1: Models
    hub.set_tab(HubTab::Models);
    terminal
        .draw(|f| hub.render(f))
        .expect("Failed to render Models tab");
    let buffer = terminal.backend().buffer();
    let content = format!("{:?}", buffer);
    assert!(
        content.contains("[F2] 📦 Models"),
        "Must render Models tab active"
    );
    assert!(
        content.contains("Mesh Models (1)"),
        "Must render models list pane"
    );
    assert!(
        content.contains("Model Architecture & Metadata"),
        "Must render metadata inspector pane"
    );
    assert!(
        content.contains("tiny-llama.gguf"),
        "Must list discovered model"
    );

    // 3. Render Tab 2: Cluster
    hub.set_tab(HubTab::Cluster);
    terminal
        .draw(|f| hub.render(f))
        .expect("Failed to render Cluster tab");
    let buffer = terminal.backend().buffer();
    let content = format!("{:?}", buffer);
    assert!(
        content.contains("[F3] 🌐 Cluster"),
        "Must render Cluster tab active"
    );
    assert!(
        content.contains("Mesh Coordinator"),
        "Must render cluster header"
    );
    assert!(
        content.contains("Local RAM"),
        "Must render local memory gauge"
    );

    // 4. Render Tab 3: Settings
    hub.set_tab(HubTab::Settings);
    terminal
        .draw(|f| hub.render(f))
        .expect("Failed to render Settings tab");
    let buffer = terminal.backend().buffer();
    let content = format!("{:?}", buffer);
    assert!(
        content.contains("[F4] ⚙️ Settings"),
        "Must render Settings tab active"
    );
    assert!(
        content.contains("Configuration & Settings"),
        "Must render settings editor header"
    );

    // 5. Render Tab 4: Tunnel
    hub.set_tab(HubTab::Tunnel);
    terminal
        .draw(|f| hub.render(f))
        .expect("Failed to render Tunnel tab");
    let buffer = terminal.backend().buffer();
    let content = format!("{:?}", buffer);
    assert!(
        content.contains("[F5] 🚇 Tunnel"),
        "Must render Tunnel tab active"
    );

    // 6. Render Tab 5: Agents
    hub.set_tab(HubTab::Agents);
    terminal
        .draw(|f| hub.render(f))
        .expect("Failed to render Agents tab");
    let buffer = terminal.backend().buffer();
    let content = format!("{:?}", buffer);
    assert!(
        content.contains("[F6] 🤖 Agents"),
        "Must render Agents tab active"
    );

    // 7. Render Tab 6: Logs
    hub.set_tab(HubTab::Logs);
    terminal
        .draw(|f| hub.render(f))
        .expect("Failed to render Logs tab");
    let buffer = terminal.backend().buffer();
    let content = format!("{:?}", buffer);
    assert!(
        content.contains("[F7] 📜 Logs"),
        "Must render Logs tab active"
    );
    assert!(
        content.contains("Node Diagnostic Logs"),
        "Must render Logs header"
    );

    // Verify 2-line persistent dock
    assert!(
        content.contains("Context:"),
        "Must render persistent context gauge"
    );
    assert!(
        content.contains("[Ctrl+P] Palette"),
        "Must render Palette shortcut hint"
    );

    // 8. Render Command Palette modal overlay
    hub.show_command_palette = true;
    hub.palette_input = "chat".to_string();
    terminal
        .draw(|f| hub.render(f))
        .expect("Failed to render Command Palette modal");
    let buffer = terminal.backend().buffer();
    let content = format!("{:?}", buffer);
    assert!(
        content.contains("Command Palette"),
        "Must render Command Palette modal overlay"
    );
    assert!(
        content.contains("Go to Chat"),
        "Must match Go to Chat in filtered palette"
    );
}

#[tokio::test]
async fn test_hub_app_hot_swap_confirmation_modal() {
    let config = NexusConfig::default();
    let discovery = Arc::new(DiscoveryService::new(config.clone(), None));
    let client = NexusClient::new("http://127.0.0.1:8080");
    let mut hub = test_hub(config, client, discovery);

    // Simulate pending hot swap with full intent (preserves -ngl)
    hub.pending_hot_swap = Some(nexus::ui::hub::HotSwapIntent {
        path: PathBuf::from("/models/llama-3.2-3b.gguf"),
        gpu_layers: Some(0),
        context_size: 4096,
        extra_args: Vec::new(),
        moe_cache_ceil_mb: None,
    });

    let backend = TestBackend::new(120, 35);
    let mut terminal = Terminal::new(backend).expect("Failed to initialize TestBackend");

    terminal
        .draw(|f| hub.render(f))
        .expect("Failed to render frame with modal");
    let buffer = terminal.backend().buffer();
    let content = format!("{:?}", buffer);

    assert!(content.contains("Hot-Swap"), "Must render modal title");
    assert!(
        content.contains("llama-3.2-3b.gguf"),
        "Must display target model filename in modal"
    );
    assert!(
        content.contains("[Y / N]"),
        "Must display confirmation prompt"
    );
}

#[tokio::test]
async fn test_hub_app_target_node_selection_modal() {
    use nexus::ui::hub::TargetExecutionNode;
    use uuid::Uuid;

    let config = NexusConfig::default();
    let discovery = Arc::new(DiscoveryService::new(config.clone(), None));
    let client = NexusClient::new("http://127.0.0.1:8080");
    let mut hub = test_hub(config, client, discovery);

    let model_path = PathBuf::from("/models/qwen2.5-coder-7b.gguf");
    hub.open_target_selection(model_path.clone()).await;

    assert!(hub.pending_target_selection.is_some());
    let state = hub.pending_target_selection.as_mut().unwrap();
    assert_eq!(state.model_name, "qwen2.5-coder-7b.gguf");
    assert_eq!(state.candidates.len(), 2); // Local GPU and Local CPU options
    assert!(matches!(
        state.candidates[0],
        TargetExecutionNode::Local { .. }
    ));
    assert!(matches!(
        state.candidates[1],
        TargetExecutionNode::LocalCpu { .. }
    ));

    // Add a simulated remote peer candidate
    let peer_id = Uuid::new_v4();
    state.candidates.push(TargetExecutionNode::Remote {
        uuid: peer_id,
        name: "Galaxy-S23".to_string(),
        endpoint: "http://192.168.1.100:9998".to_string(),
        api_endpoint: "http://192.168.1.100:8080".to_string(),
        free_ram_mb: 8500,
        backend: "Vulkan".to_string(),
        predicted_label: "~28 tok/s".to_string(),
    });
    assert_eq!(state.candidates.len(), 3);

    let backend = TestBackend::new(120, 35);
    let mut terminal = Terminal::new(backend).expect("Failed to initialize TestBackend");

    terminal
        .draw(|f| hub.render(f))
        .expect("Failed to render target selection modal");
    let buffer = terminal.backend().buffer();
    let content = format!("{:?}", buffer);

    assert!(
        content.contains("Target Node Selection"),
        "Must render modal title"
    );
    assert!(
        content.contains("qwen2.5-coder-7b"),
        "Must render target model name"
    );
    assert!(
        content.contains("Local GPU") || content.contains("Local CPU"),
        "Must list Local execution options"
    );
    assert!(
        content.contains("Galaxy-S23"),
        "Must list remote peer candidate"
    );
}

#[tokio::test]
async fn test_cluster_view_interactions() {
    use nexus::discovery::PeerNode;
    use nexus::sysinfo::AccelerationBackend;
    use nexus::ui::cluster_view::ClusterView;
    use uuid::Uuid;

    let config = NexusConfig::default();
    let discovery = Arc::new(DiscoveryService::new(config, None));
    let mut cluster = ClusterView::new(discovery);

    // Initial state
    assert_eq!(cluster.peers.len(), 0);
    assert_eq!(cluster.selected_index, 0);
    assert!(!cluster.show_info_modal);
    assert!(!cluster.adding_peer);

    // Simulate adding peers
    let peer1 = PeerNode {
        uuid: Uuid::new_v4(),
        addr: "192.168.1.10:8080".parse().unwrap(),
        role: nexus::discovery::NodeRole::HOST,
        status: nexus::discovery::StatusFlags::READY,
        api_port: 8080,
        control_port: 9998,
        rpc_port: 50052,
        total_ram_mb: 12000,
        free_ram_mb: 8192,
        backend: AccelerationBackend::Vulkan,
        thermal_index: 45,
        active_model: "llama-3.2-3b.gguf".to_string(),
        display_name: String::new(),
        moe_stream: false,
        moe_cache_ceil_mb: 0,
        last_seen: std::time::Instant::now(),
    };
    let peer2 = PeerNode {
        uuid: Uuid::new_v4(),
        addr: "192.168.1.20:8080".parse().unwrap(),
        role: nexus::discovery::NodeRole::CLIENT,
        status: nexus::discovery::StatusFlags::RPC_READY,
        api_port: 8080,
        control_port: 9998,
        rpc_port: 50052,
        total_ram_mb: 4000,
        free_ram_mb: 1800,
        backend: AccelerationBackend::ArmCpuDotProd,
        thermal_index: 30,
        active_model: String::new(),
        display_name: String::new(),
        moe_stream: false,
        moe_cache_ceil_mb: 0,
        last_seen: std::time::Instant::now(),
    };

    cluster.peers.push(peer1);
    cluster.peers.push(peer2);

    assert_eq!(cluster.peers.len(), 2);
    assert_eq!(cluster.selected_index, 0);

    // Navigation
    cluster.next();
    assert_eq!(cluster.selected_index, 1);
    cluster.next();
    assert_eq!(cluster.selected_index, 0);
    cluster.previous();
    assert_eq!(cluster.selected_index, 1);

    // Modal inspection
    cluster.toggle_info_modal();
    assert!(cluster.show_info_modal);
    cluster.toggle_info_modal();
    assert!(!cluster.show_info_modal);

    // Static peer input
    cluster.start_add_peer();
    assert!(cluster.adding_peer);
    for c in "192.168.1.99:8080".chars() {
        cluster.push_add_char(c);
    }
    assert_eq!(cluster.add_peer_input, "192.168.1.99:8080");
    cluster.backspace_add_char();
    assert_eq!(cluster.add_peer_input, "192.168.1.99:808");
    cluster.cancel_add_peer();
    assert!(!cluster.adding_peer);
}

#[tokio::test]
async fn test_cluster_view_header_status_badges() {
    let config = NexusConfig::default();
    let discovery = Arc::new(DiscoveryService::new(config, None));
    let cluster = nexus::ui::cluster_view::ClusterView::new(discovery);

    let backend = TestBackend::new(120, 30);
    let mut terminal = Terminal::new(backend).unwrap();

    terminal.draw(|f| cluster.render(f, f.area())).unwrap();
    let content = format!("{:?}", terminal.backend().buffer());
    assert!(content.contains("Mesh Coordinator"));
    assert!(content.contains("UDP:"));
    assert!(content.contains("mDNS:"));
    assert!(content.contains("Broadcasting"));
}

#[tokio::test]
async fn test_hub_app_unload_model() {
    let config = NexusConfig::default();
    let discovery = Arc::new(DiscoveryService::new(config.clone(), None));
    let client = NexusClient::new("http://127.0.0.1:8080");
    let mut hub = test_hub(config, client, discovery.clone());

    // Initially no model loaded
    assert_eq!(hub.active_model_name, "None (Idle)");
    hub.unload_active_model().await;
    assert_eq!(hub.active_model_name, "None (Idle)");

    // Simulate an active model name and discovery advertisement
    hub.active_model_name = "test-model-3b".to_string();
    discovery.set_active_model("test-model-3b").await;
    discovery
        .set_status_flags(nexus::discovery::StatusFlags::READY)
        .await;

    // Call unload
    hub.unload_active_model().await;
    assert_eq!(hub.active_model_name, "None (Idle)");
    assert_eq!(hub.chat.model_name, "default");
    assert!(hub.status_message.is_some());
    let (msg, color) = hub.status_message.as_ref().unwrap();
    assert!(msg.contains("Unloaded model 'test-model-3b'"));
    assert_eq!(*color, ratatui::style::Color::Cyan);

    // Verify chat received unload notice
    let last_msg = hub
        .chat
        .messages
        .last()
        .expect("Must have unload notice message");
    assert!(last_msg
        .message
        .content
        .contains("Model 'test-model-3b' unloaded"));
}

#[tokio::test]
async fn test_command_palette_filtering_and_actions() {
    let config = NexusConfig::default();
    let discovery = Arc::new(DiscoveryService::new(config.clone(), None));
    let client = NexusClient::new("http://127.0.0.1:8080");
    let mut hub = test_hub(config, client, discovery);

    let items = hub.build_palette_items();
    assert!(
        items.len() >= 10,
        "Palette should index navigation, actions, and commands"
    );

    // Exact prefix match
    let filtered_logs = hub.filter_palette_items(&items, "logs");
    assert!(!filtered_logs.is_empty());
    assert_eq!(filtered_logs[0].label, "Go to Logs");

    // Action search
    let filtered_unload = hub.filter_palette_items(&items, "unload");
    assert!(!filtered_unload.is_empty());
    assert!(filtered_unload[0].label.contains("Unload"));

    // Slash command search
    let filtered_doc = hub.filter_palette_items(&items, "doc");
    assert!(!filtered_doc.is_empty());
    assert_eq!(filtered_doc[0].label, "/doctor");

    // Execute palette action: switch tab
    let (tx, _rx) = tokio::sync::mpsc::channel(10);
    hub.execute_palette_action(nexus::ui::hub::PaletteAction::SwitchTab(HubTab::Logs), &tx);
    assert_eq!(hub.active_tab, HubTab::Logs);

    // Execute palette action: help modal
    hub.execute_palette_action(nexus::ui::hub::PaletteAction::OpenHelp, &tx);
    assert!(hub.show_help);
}

#[tokio::test]
async fn test_persistent_dock_telemetry_and_gauge() {
    let config = NexusConfig::default();
    let discovery = Arc::new(DiscoveryService::new(config.clone(), None));
    let client = NexusClient::new("http://127.0.0.1:8080");
    let mut hub = test_hub(config, client, discovery);

    hub.active_model_name = "qwen2.5-coder-7b.gguf".to_string();
    hub.chat.tokens_per_sec = 28.5;
    hub.chat.is_streaming = true;

    let backend = TestBackend::new(120, 30);
    let mut terminal = Terminal::new(backend).unwrap();

    terminal.draw(|f| hub.render(f)).unwrap();
    let content = format!("{:?}", terminal.backend().buffer());

    // Verify Row 1: Telemetry
    assert!(
        content.contains("qwen2.5-coder-7b.gguf"),
        "Must display active model in persistent bar"
    );
    assert!(
        content.contains("28.5 t/s"),
        "Must display active live tokens/sec"
    );
    assert!(content.contains("Context:"), "Must display context label");
    assert!(
        content.contains("[STREAM]"),
        "Must display streaming status badge"
    );

    // Verify Row 2: Controls & Shortcuts
    assert!(
        content.contains("[F1-F7] Tabs"),
        "Must display tab range shortcut"
    );
    assert!(
        content.contains("[Ctrl+P] Palette"),
        "Must display Palette hotkey"
    );
    assert!(content.contains("[?] Help"), "Must display Help hotkey");
}

#[tokio::test]
async fn test_cluster_view_link_quality_visuals() {
    use nexus::cluster::LinkQuality;
    use nexus::discovery::{NodeRole, PeerNode};
    use std::time::Duration;
    use uuid::Uuid;

    let config = NexusConfig::default();
    let discovery = Arc::new(DiscoveryService::new(config, None));
    let mut cluster = nexus::ui::cluster_view::ClusterView::new(discovery);

    let peer_id = Uuid::new_v4();
    let peer = PeerNode {
        uuid: peer_id,
        addr: "192.168.1.42:8080".parse().unwrap(),
        role: NodeRole::HOST,
        status: nexus::discovery::StatusFlags::READY,
        api_port: 8080,
        control_port: 9998,
        rpc_port: 50052,
        total_ram_mb: 8192,
        free_ram_mb: 4096,
        backend: nexus::sysinfo::AccelerationBackend::Vulkan,
        thermal_index: 25,
        active_model: "phi-4-mini".to_string(),
        display_name: String::new(),
        moe_stream: false,
        moe_cache_ceil_mb: 0,
        last_seen: std::time::Instant::now(),
    };
    cluster.peers.push(peer);

    // Record synthetic measured link quality: 3.2ms RTT, ~50 MB/s
    let lq = LinkQuality::from_probe(
        Duration::from_millis(3),
        50 * 1024 * 1024,
        Duration::from_secs(1),
    );
    cluster.record_link_quality(peer_id, lq);

    let backend = TestBackend::new(140, 30);
    let mut terminal = Terminal::new(backend).unwrap();

    // Render table
    terminal.draw(|f| cluster.render(f, f.area())).unwrap();
    let content = format!("{:?}", terminal.backend().buffer());
    assert!(
        content.contains("Link Quality"),
        "Must render Link Quality column header"
    );
    assert!(
        content.contains("3.0ms"),
        "Must render measured RTT in table"
    );

    // Render inspect modal
    cluster.show_info_modal = true;
    terminal.draw(|f| cluster.render(f, f.area())).unwrap();
    let modal_content = format!("{:?}", terminal.backend().buffer());
    assert!(
        modal_content.contains("Link Telemetry:"),
        "Must display Link Telemetry row in inspector modal"
    );
    assert!(
        modal_content.contains("probed"),
        "Must show probe details in modal"
    );
}

#[test]
fn test_logs_view_streaming_and_filters() {
    use nexus::ui::logs_view::{LogFilterLevel, LogsView};
    use std::io::Write;
    use tempfile::NamedTempFile;

    let mut temp = NamedTempFile::new().unwrap();
    writeln!(temp, "2026-10-07T12:00:00Z INFO node: node initialized").unwrap();
    writeln!(temp, "2026-10-07T12:00:01Z WARN node: high peer latency").unwrap();
    writeln!(temp, "2026-10-07T12:00:02Z ERROR node: socket dropped").unwrap();
    temp.flush().unwrap();

    let mut logs = LogsView::for_path(temp.path().to_path_buf());
    assert_eq!(logs.lines.len(), 3);
    assert_eq!(logs.filtered_lines().len(), 3);

    // Filter cycling
    logs.cycle_filter();
    assert_eq!(logs.filter_level, LogFilterLevel::Info);
    assert_eq!(logs.filtered_lines().len(), 3);

    logs.cycle_filter();
    assert_eq!(logs.filter_level, LogFilterLevel::Warn);
    assert_eq!(logs.filtered_lines().len(), 2);

    logs.cycle_filter();
    assert_eq!(logs.filter_level, LogFilterLevel::Error);
    assert_eq!(logs.filtered_lines().len(), 1);

    // Live search
    logs.filter_level = LogFilterLevel::All;
    logs.search_query = "socket".to_string();
    assert_eq!(logs.filtered_lines().len(), 1);

    // Auto follow toggling
    assert!(logs.auto_tail);
    logs.toggle_tail();
    assert!(!logs.auto_tail);
    logs.toggle_tail();
    assert!(logs.auto_tail);

    // Clear buffer
    logs.clear();
    assert_eq!(logs.lines.len(), 0);
}

#[tokio::test]
async fn test_mobile_responsive_rendering() {
    let config = NexusConfig::default();
    let discovery = Arc::new(DiscoveryService::new(config.clone(), None));
    let client = NexusClient::new("http://127.0.0.1:8080");
    let mut hub = test_hub(config, client, discovery.clone());

    hub.active_model_name = "phi-4-mini-q4_k_m.gguf".to_string();
    hub.chat.tokens_per_sec = 18.2;

    // Simulate mobile screen dimensions: 75 columns x 22 rows
    let backend = TestBackend::new(75, 22);
    let mut terminal = Terminal::new(backend).unwrap();

    // 1. Chat tab mobile render
    terminal.draw(|f| hub.render(f)).unwrap();
    let content = format!("{:?}", terminal.backend().buffer());

    // Top tab bar should be condensed
    assert!(
        content.contains("1:Chat"),
        "Must render compact 1:Chat tab badge"
    );
    assert!(
        content.contains("2:Mod"),
        "Must render compact 2:Mod tab badge"
    );
    assert!(
        !content.contains("Nexus-LLM Unified Hub"),
        "Must omit long title on mobile"
    );

    // Bottom dock should be condensed
    assert!(
        content.contains("phi-4-mini"),
        "Must render model in compact dock"
    );
    assert!(
        content.contains("18.2 t/s"),
        "Must render speed in compact dock"
    );
    assert!(
        content.contains("[Tab] Next"),
        "Must render mobile-friendly shortcuts"
    );

    // 2. Cluster tab mobile render
    hub.set_tab(HubTab::Cluster);
    terminal.draw(|f| hub.render(f)).unwrap();
    let cluster_content = format!("{:?}", terminal.backend().buffer());

    assert!(
        cluster_content.contains("RAM:"),
        "Must render compact RAM title"
    );
    assert!(
        cluster_content.contains("[Engine]"),
        "Must render compact engine summary"
    );
    assert!(
        cluster_content.contains("Node / UUID"),
        "Must render 4-column compact header"
    );
    assert!(
        !cluster_content.contains("Link Quality"),
        "Must omit wide columns on mobile"
    );

    // 3. Models tab mobile render
    hub.set_tab(HubTab::Models);
    terminal.draw(|f| hub.render(f)).unwrap();
    let models_content = format!("{:?}", terminal.backend().buffer());
    assert!(models_content.contains("No models") || models_content.contains("Model"));

    // 4. Test explicit override: Wide mode forces desktop layout on small screen
    hub.settings_view.config.ui.layout_mode = "wide".to_string();
    hub.sync_layout_mode();
    assert_eq!(hub.layout_mode, nexus::ui::layout::LayoutMode::Wide);

    hub.set_tab(HubTab::Cluster);
    terminal.draw(|f| hub.render(f)).unwrap();
    let wide_content = format!("{:?}", terminal.backend().buffer());
    // In wide mode, it renders the desktop title and full telemetry dock
    assert!(
        wide_content.contains("Discovered Mesh Peers"),
        "Wide override must render wide mesh peers title"
    );
    assert!(
        wide_content.contains("[F1-F7] Tabs"),
        "Wide override must render desktop F1-F7 tabs shortcut"
    );
    assert!(
        wide_content.contains("Host:"),
        "Wide override must render Host endpoint in dock"
    );
}

#[tokio::test]
async fn test_hub_app_delete_model_confirmation_modal() {
    let config = NexusConfig::default();
    let discovery = Arc::new(DiscoveryService::new(config.clone(), None));
    let client = NexusClient::new("http://127.0.0.1:8080");
    let mut hub = test_hub(config, client, discovery);

    let entry = nexus::ui::models::ModelEntry {
        filename: "test-model.gguf".to_string(),
        path: PathBuf::from("/models/test-model.gguf"),
        size_mb: 4096,
        architecture: "llama".to_string(),
        context_length: 4096,
        exact_kv_mb: 512,
        lmk_compatible: true,
        gguf_version: 3,
        block_count: 32,
        head_count: 32,
        embedding_length: 4096,
        digest: "abc".to_string(),
        shard_count: 1,
        total_shards: None,
    };

    hub.pending_delete_model = Some((
        entry,
        vec![PathBuf::from("/models/test-model.gguf")],
        4 * 1024 * 1024 * 1024,
    ));

    let backend = TestBackend::new(120, 35);
    let mut terminal = Terminal::new(backend).expect("Failed to initialize TestBackend");

    terminal
        .draw(|f| hub.render(f))
        .expect("Failed to render frame with delete modal");
    let buffer = terminal.backend().buffer();
    let content = format!("{:?}", buffer);

    assert!(
        content.contains("Delete Model"),
        "Must render delete modal title"
    );
    assert!(
        content.contains("test-model.gguf"),
        "Must render model filename"
    );
    assert!(
        content.contains("Confirm Delete") && content.contains("Cancel"),
        "Must render action buttons"
    );
}

#[tokio::test]
async fn test_hub_app_download_cancellation_hint() {
    let config = NexusConfig::default();
    let discovery = Arc::new(DiscoveryService::new(config.clone(), None));
    let client = NexusClient::new("http://127.0.0.1:8080");
    let mut hub = test_hub(config, client, discovery);

    hub.download_progress = Some((
        "model.gguf".to_string(),
        Some(25.0),
        250_000_000,
        Some(1_000_000_000),
        15_000_000.0,
    ));

    let backend = TestBackend::new(120, 35);
    let mut terminal = Terminal::new(backend).expect("Failed to initialize TestBackend");

    terminal
        .draw(|f| hub.render(f))
        .expect("Failed to render frame with download progress");
    let buffer = terminal.backend().buffer();
    let content = format!("{:?}", buffer);

    assert!(
        content.contains("Transfer"),
        "Must render download transfer title"
    );
    assert!(
        content.contains("[Esc]") || content.contains("[C]"),
        "Must render cancellation shortcut hint"
    );
    assert!(
        content.contains("Cancel download"),
        "Must render cancel hint"
    );
}

#[tokio::test]
async fn test_hub_app_hf_quant_picker_modal() {
    let config = NexusConfig::default();
    let discovery = Arc::new(DiscoveryService::new(config.clone(), None));
    let client = NexusClient::new("http://127.0.0.1:8080");
    let mut hub = test_hub(config, client, discovery);

    let group1 = nexus::hf::HfGgufGroup {
        base_name: "test-model-Q4_K_M".to_string(),
        quant_label: "Q4_K_M".to_string(),
        total_size_bytes: 2_100_000_000,
        files: vec![nexus::hf::HfGgufFile {
            filename: "test-model-Q4_K_M.gguf".to_string(),
            size_bytes: 2_100_000_000,
            sha256: None,
            download_url: "https://huggingface.co/test/model/resolve/main/test-model-Q4_K_M.gguf"
                .to_string(),
        }],
        is_sharded: false,
        fit_status: nexus::hf::FitStatus::Fits,
    };

    let group2 = nexus::hf::HfGgufGroup {
        base_name: "test-model-Q8_0".to_string(),
        quant_label: "Q8_0".to_string(),
        total_size_bytes: 4_200_000_000,
        files: vec![nexus::hf::HfGgufFile {
            filename: "test-model-Q8_0.gguf".to_string(),
            size_bytes: 4_200_000_000,
            sha256: None,
            download_url: "https://huggingface.co/test/model/resolve/main/test-model-Q8_0.gguf"
                .to_string(),
        }],
        is_sharded: false,
        fit_status: nexus::hf::FitStatus::OffloadRequired,
    };

    hub.pending_hf_quant_picker = Some(nexus::ui::hub::HfQuantPickerState {
        repo_id: "test-org/test-model".to_string(),
        groups: vec![group1, group2],
        selected_idx: 0,
    });

    let backend = TestBackend::new(120, 35);
    let mut terminal = Terminal::new(backend).expect("Failed to initialize TestBackend");

    terminal
        .draw(|f| hub.render(f))
        .expect("Failed to render frame with quant picker");
    let buffer = terminal.backend().buffer();
    let content = format!("{:?}", buffer);

    assert!(
        content.contains("Hugging Face Quantization Picker"),
        "Must render quant picker modal title"
    );
    assert!(
        content.contains("test-org/test-model"),
        "Must render target repo ID"
    );
    assert!(content.contains("Q4_K_M"), "Must render quant label");
    assert!(
        content.contains("[OK] Fits"),
        "Must render memory fit badge"
    );
    assert!(
        content.contains("[RPC] Offload"),
        "Must render offload badge"
    );
    assert!(
        content.contains("Select Quant"),
        "Must render controls hint"
    );
}

#[tokio::test]
async fn test_hub_app_hf_auth_recovery_modal() {
    let config = NexusConfig::default();
    let discovery = Arc::new(DiscoveryService::new(config.clone(), None));
    let client = NexusClient::new("http://127.0.0.1:8080");
    let mut hub = test_hub(config, client, discovery);

    hub.pending_hf_auth_recovery = Some(nexus::ui::hub::HfAuthRecoveryState {
        repo_id: "meta-llama/Llama-3.2-3B".to_string(),
        retry_download_url: Some(
            "https://huggingface.co/meta-llama/Llama-3.2-3B/resolve/main/model.gguf".to_string(),
        ),
        retry_expected_sha: None,
        input_token: "hf_testtoken123".to_string(),
    });

    let backend = TestBackend::new(120, 35);
    let mut terminal = Terminal::new(backend).expect("Failed to initialize TestBackend");

    terminal
        .draw(|f| hub.render(f))
        .expect("Failed to render frame with auth recovery modal");
    let buffer = terminal.backend().buffer();
    let content = format!("{:?}", buffer);

    assert!(
        content.contains("Authentication Required"),
        "Must render auth recovery modal title"
    );
    assert!(
        content.contains("meta-llama/Llama-3.2-3B"),
        "Must render repo name"
    );
    assert!(
        content.contains("Save Token & Retry"),
        "Must render confirmation action"
    );
    assert!(content.contains("•••••••••••••••"), "Must mask token input");
}

#[tokio::test]
async fn test_hub_app_hf_explorer_mode_rendering() {
    let config = NexusConfig::default();
    let discovery = Arc::new(DiscoveryService::new(config.clone(), None));
    let client = NexusClient::new("http://127.0.0.1:8080");
    let mut hub = test_hub(config, client, discovery);

    hub.set_tab(nexus::ui::hub::HubTab::Models);
    hub.models_view.toggle_mode();
    assert_eq!(
        hub.models_view.mode,
        nexus::ui::models_view::ModelsTabMode::HfExplorer
    );

    hub.models_view
        .set_hf_models(vec![nexus::hf::HfModelSummary {
            id: "Qwen/Qwen2.5-Coder-7B-GGUF".to_string(),
            author: Some("Qwen".to_string()),
            downloads: 54_300,
            likes: 1_250,
            private: false,
            gated: None,
            pipeline_tag: Some("text-generation".to_string()),
            tags: vec!["code".to_string(), "gguf".to_string()],
        }]);

    let backend = TestBackend::new(120, 35);
    let mut terminal = Terminal::new(backend).expect("Failed to initialize TestBackend");

    terminal
        .draw(|f| hub.render(f))
        .expect("Failed to render frame in HF Explorer mode");
    let buffer = terminal.backend().buffer();
    let content = format!("{:?}", buffer);

    assert!(
        content.contains("HF Explorer"),
        "Must render HF Explorer tab title"
    );
    assert!(content.contains("Qwen2.5-Coder-7B"), "Must render model ID");
    assert!(
        content.contains("Hugging Face Model Details"),
        "Must render details pane"
    );
    assert!(
        content.contains("Press [Enter] to inspect GGUF quants"),
        "Must render quant inspection action"
    );

    // Search query active display
    hub.models_view.hf_is_searching = true;
    hub.models_view.hf_search_query = "deepseek".to_string();

    let backend2 = TestBackend::new(120, 35);
    let mut terminal2 = Terminal::new(backend2).expect("Failed to initialize TestBackend");
    terminal2
        .draw(|f| hub.render(f))
        .expect("Failed to render frame in HF Explorer search mode");
    let buffer2 = terminal2.backend().buffer();
    let content2 = format!("{:?}", buffer2);
    assert!(
        content2.contains("Search: deepseek_"),
        "Must render search input prompt"
    );
}
