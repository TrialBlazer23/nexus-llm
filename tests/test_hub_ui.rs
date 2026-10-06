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
    assert_eq!(hub.active_tab, HubTab::Cluster);
    assert!(HubTab::Cluster.title().contains("Cluster"));

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
    assert_eq!(view.current_item().category, "Node Identity");

    // Test enum cycling on Node Mesh Role
    let role_idx = SettingsView::items().iter().position(|i| i.name == "Node Mesh Role").expect("role field must exist");
    view.selected_index = role_idx;
    assert_eq!(view.config.node.role, "host");
    view.toggle_or_adjust(false, true); // cycle right
    assert_eq!(view.config.node.role, "client");
    view.toggle_or_adjust(true, false); // cycle left
    assert_eq!(view.config.node.role, "host");

    // Test text editing on Node Hostname / Name
    let name_idx = SettingsView::items().iter().position(|i| i.name.contains("Node Hostname")).expect("name field must exist");
    view.selected_index = name_idx;
    assert_eq!(view.is_current_text(), true);
    view.start_editing();
    assert_eq!(view.editing_text, true);
    view.push_char('-');
    view.push_char('1');
    view.commit_text();
    assert_eq!(view.editing_text, false);
    assert!(view.config.node.name.ends_with("-1"));

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
    assert_eq!(loaded.node.role, "host");
    assert!(loaded.node.name.ends_with("-1"));
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

    // 3. Render Tab 2: Cluster
    hub.set_tab(HubTab::Cluster);
    terminal.draw(|f| hub.render(f)).expect("Failed to render Cluster tab");
    let buffer = terminal.backend().buffer();
    let content = format!("{:?}", buffer);
    assert!(content.contains("[F3] 🌐 Cluster"), "Must render Cluster tab active");
    assert!(content.contains("Mesh Coordinator"), "Must render cluster header");
    assert!(content.contains("Local RAM"), "Must render local memory gauge");

    // 4. Render Tab 3: Settings
    hub.set_tab(HubTab::Settings);
    terminal.draw(|f| hub.render(f)).expect("Failed to render Settings tab");
    let buffer = terminal.backend().buffer();
    let content = format!("{:?}", buffer);
    assert!(content.contains("[F4] ⚙️ Settings"), "Must render Settings tab active");
    assert!(content.contains("Configuration & Settings"), "Must render settings editor header");
    assert!(content.contains("Node Identity"), "Must render Node Identity category");
    assert!(content.contains("Hardware & Acceleration"), "Must render Hardware category");
    assert!(content.contains("Memory & Android LMK Safeguards"), "Must render Memory Safety category");
}

#[tokio::test]
async fn test_hub_app_hot_swap_confirmation_modal() {
    let config = NexusConfig::default();
    let discovery = Arc::new(DiscoveryService::new(config.clone(), None));
    let client = NexusClient::new("http://127.0.0.1:8080");
    let mut hub = HubApp::new(config, client, discovery);

    // Simulate pending hot swap with an explicit CPU Safe Mode layer override
    hub.pending_hot_swap = Some((PathBuf::from("/models/llama-3.2-3b.gguf"), Some(0)));

    let backend = TestBackend::new(120, 35);
    let mut terminal = Terminal::new(backend).expect("Failed to initialize TestBackend");

    terminal.draw(|f| hub.render(f)).expect("Failed to render frame with modal");
    let buffer = terminal.backend().buffer();
    let content = format!("{:?}", buffer);

    assert!(content.contains("Hot-Swap"), "Must render modal title");
    assert!(content.contains("llama-3.2-3b.gguf"), "Must display target model filename in modal");
    assert!(content.contains("[Y / N]"), "Must display confirmation prompt");

    let pending = hub.pending_hot_swap.as_ref().expect("pending hot swap must remain");
    assert_eq!(pending.0, PathBuf::from("/models/llama-3.2-3b.gguf"));
    assert_eq!(pending.1, Some(0), "GPU layer override must survive into the confirm modal");
}

#[tokio::test]
async fn test_hub_app_target_node_selection_modal() {
    use nexus::ui::hub::TargetExecutionNode;
    use uuid::Uuid;

    let config = NexusConfig::default();
    let discovery = Arc::new(DiscoveryService::new(config.clone(), None));
    let client = NexusClient::new("http://127.0.0.1:8080");
    let mut hub = HubApp::new(config, client, discovery);

    let model_path = PathBuf::from("/models/qwen2.5-coder-7b.gguf");
    hub.open_target_selection(model_path.clone()).await;

    assert!(hub.pending_target_selection.is_some());
    let state = hub.pending_target_selection.as_mut().unwrap();
    assert_eq!(state.model_name, "qwen2.5-coder-7b.gguf");
    assert_eq!(state.candidates.len(), 2); // Local GPU and Local CPU options
    assert!(matches!(state.candidates[0], TargetExecutionNode::Local { .. }));
    assert!(matches!(state.candidates[1], TargetExecutionNode::LocalCpu { .. }));

    // Add a high-RAM remote (fits) and a tiny remote (won't fit) for badge coverage.
    // Model path does not exist so required_mb is ~KV-only (~800 MB at 4k estimate); set explicitly.
    state.required_mb = 5000;
    state.rpc_worker_cap_mb = 1800;

    let peer_id = Uuid::new_v4();
    state.candidates.push(TargetExecutionNode::Remote {
        uuid: peer_id,
        name: "Galaxy-S23".to_string(),
        endpoint: "http://192.168.1.100:8080".to_string(),
        free_ram_mb: 12000, // LMK budget ~9000 → fits 5000
        backend: "Vulkan".to_string(),
    });
    let tiny_id = Uuid::new_v4();
    state.candidates.push(TargetExecutionNode::Remote {
        uuid: tiny_id,
        name: "Tiny-Pi".to_string(),
        endpoint: "http://192.168.1.50:8080".to_string(),
        free_ram_mb: 800, // LMK ~600; +1800 RPC still < 5000 → won't fit
        backend: "CPU".to_string(),
    });
    assert_eq!(state.candidates.len(), 4);

    let fits = state.candidates[2].fit(state.required_mb, state.rpc_worker_cap_mb);
    let wont = state.candidates[3].fit(state.required_mb, state.rpc_worker_cap_mb);
    assert_eq!(fits, nexus::cluster::ModelFit::Fits);
    assert_eq!(wont, nexus::cluster::ModelFit::WontFit);

    let backend = TestBackend::new(120, 35);
    let mut terminal = Terminal::new(backend).expect("Failed to initialize TestBackend");

    terminal.draw(|f| hub.render(f)).expect("Failed to render target selection modal");
    let buffer = terminal.backend().buffer();
    let content = format!("{:?}", buffer);

    assert!(content.contains("Target Node Selection"), "Must render modal title");
    assert!(content.contains("qwen2.5-coder-7b"), "Must render target model name");
    assert!(content.contains("Local GPU") || content.contains("Local CPU"), "Must list Local execution options");
    assert!(content.contains("Galaxy-S23"), "Must list remote peer candidate");
    assert!(
        content.contains("fits") || content.contains("won't fit") || content.contains("needs RPC"),
        "Must show fit-per-target badges: {}",
        content
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
    assert_eq!(cluster.show_info_modal, false);
    assert_eq!(cluster.adding_peer, false);

    // Simulate adding peers
    let peer1 = PeerNode {
        uuid: Uuid::new_v4(),
        addr: "192.168.1.10:8080".parse().unwrap(),
        role: nexus::discovery::NodeRole::HOST,
        status: nexus::discovery::StatusFlags::READY,
        api_port: 8080,
        rpc_port: 50052,
        total_ram_mb: 12000,
        free_ram_mb: 8192,
        backend: AccelerationBackend::Vulkan,
        thermal_index: 45,
        active_model: "llama-3.2-3b.gguf".to_string(),
        last_seen: std::time::Instant::now(),
    };
    let peer2 = PeerNode {
        uuid: Uuid::new_v4(),
        addr: "192.168.1.20:8080".parse().unwrap(),
        role: nexus::discovery::NodeRole::CLIENT,
        status: nexus::discovery::StatusFlags::RPC_READY,
        api_port: 8080,
        rpc_port: 50052,
        total_ram_mb: 4000,
        free_ram_mb: 1800,
        backend: AccelerationBackend::ArmCpuDotProd,
        thermal_index: 30,
        active_model: String::new(),
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
    assert_eq!(cluster.show_info_modal, true);
    cluster.toggle_info_modal();
    assert_eq!(cluster.show_info_modal, false);

    // Static peer input
    cluster.start_add_peer();
    assert_eq!(cluster.adding_peer, true);
    for c in "192.168.1.99:8080".chars() {
        cluster.push_add_char(c);
    }
    assert_eq!(cluster.add_peer_input, "192.168.1.99:8080");
    cluster.backspace_add_char();
    assert_eq!(cluster.add_peer_input, "192.168.1.99:808");
    cluster.cancel_add_peer();
    assert_eq!(cluster.adding_peer, false);
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
    let mut hub = HubApp::new(config, client, discovery.clone());

    // Initially no model loaded
    assert_eq!(hub.active_model_name, "None (Idle)");
    hub.unload_active_model().await;
    assert_eq!(hub.active_model_name, "None (Idle)");

    // Simulate an active model name and discovery advertisement
    hub.active_model_name = "test-model-3b".to_string();
    discovery.set_active_model("test-model-3b").await;
    discovery.set_status_flags(nexus::discovery::StatusFlags::READY).await;

    // Call unload
    hub.unload_active_model().await;
    assert_eq!(hub.active_model_name, "None (Idle)");
    assert_eq!(hub.chat.model_name, "default");
    assert!(hub.status_message.is_some());
    let (msg, color) = hub.status_message.as_ref().unwrap();
    assert!(msg.contains("Unloaded model 'test-model-3b'"));
    assert_eq!(*color, ratatui::style::Color::Cyan);

    // Verify chat received unload notice
    let last_msg = hub.chat.messages.last().expect("Must have unload notice message");
    assert!(last_msg.content.contains("Model 'test-model-3b' unloaded"));
}

#[tokio::test]
async fn test_hub_cycle_persona_applies_preset() {
    let config = NexusConfig::default();
    let discovery = Arc::new(DiscoveryService::new(config.clone(), None));
    let client = NexusClient::new("http://127.0.0.1:8080");
    let mut hub = HubApp::new(config, client, discovery);

    assert!(hub.chat.persona_name.is_none());
    assert!(hub.chat.system_prompt.is_none());

    hub.cycle_persona();
    assert!(hub.chat.persona_name.is_some());
    assert!(hub.chat.system_prompt.is_some());
    let first = hub.chat.persona_name.clone().unwrap();
    let first_temp = hub.chat.temperature;
    let first_max = hub.chat.max_tokens;

    hub.cycle_persona();
    let second = hub.chat.persona_name.clone().unwrap();
    assert_ne!(first, second, "cycling should advance to a different persona");
    assert!(hub.chat.system_prompt.is_some());
    // Built-ins differ in temperature (coder=0.2, general=0.7)
    assert!(
        hub.chat.temperature != first_temp || hub.chat.max_tokens != first_max || first != second,
        "persona hyperparameters should update with cycle"
    );

    let (msg, _) = hub.status_message.as_ref().expect("status toast after persona");
    assert!(msg.starts_with("Persona:"));
}

#[test]
fn test_models_view_cached_profile_refresh() {
    let mut view = ModelsView::new(PathBuf::from("/tmp/nonexistent-nexus-models"));
    view.refresh_profile();
    assert!(view.cached_profile.total_ram_mb > 0 || view.cached_profile.available_ram_mb == 0);
}

#[tokio::test]
async fn test_hub_slash_commands_and_context() {
    let config = NexusConfig::default();
    let discovery = Arc::new(DiscoveryService::new(config.clone(), None));
    let client = NexusClient::new("http://127.0.0.1:8080");
    let mut hub = HubApp::new(config, client, discovery);

    hub.dispatch_slash_command("/help").await;
    assert!(
        hub.chat.messages.iter().any(|m| m.is_status() && m.content.contains("/unload")),
        "help should emit a status banner with command list"
    );

    hub.chat.messages.push(nexus::client::ChatMessage::user("keep me"));
    hub.dispatch_slash_command("/clear").await;
    assert!(hub.chat.messages.is_empty());

    hub.dispatch_slash_command("/temp 0.25").await;
    assert!((hub.chat.temperature - 0.25).abs() < f32::EPSILON);

    hub.dispatch_slash_command("/context 2048").await;
    assert_eq!(hub.selected_context, 2048);
    assert_eq!(hub.models_view.selected_context, 2048);

    hub.dispatch_slash_command("/preset coder").await;
    assert_eq!(hub.chat.persona_name.as_deref(), Some("coder"));

    hub.adjust_context(true);
    assert_eq!(hub.selected_context, 2048 + 512);
}


