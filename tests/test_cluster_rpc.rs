use nexus::cluster::{ClusterCoordinator, ClusterError, NodeBudget};
use nexus::config::NexusConfig;
use nexus::tunnel::TransportMode;
use uuid::Uuid;

#[test]
fn test_cluster_budget_calculation() {
    // Dynamic budget: 75% of available RAM
    // Case 1: Standalone host with 16000 MB available -> 75% is 12000 MB
    let budget_standalone = ClusterCoordinator::calculate_budget(16000, None);
    assert_eq!(budget_standalone.host_max_mb, 12000);
    assert_eq!(budget_standalone.remote_max_mb, 0);
    assert_eq!(budget_standalone.total_cluster_mb, 12000);

    // Case 2: Host with lower RAM (8000 MB available) -> 75% is 6000 MB
    let budget_low_ram = ClusterCoordinator::calculate_budget(8000, None);
    assert_eq!(budget_low_ram.host_max_mb, 6000);
    assert_eq!(budget_low_ram.total_cluster_mb, 6000);

    // Case 3: Host with explicit cap (e.g., 8500 MB)
    let budget_capped = ClusterCoordinator::calculate_dynamic_budget(16000, Some(8500), Some(1800));
    assert_eq!(budget_capped.host_max_mb, 8500);
    assert_eq!(budget_capped.remote_max_mb, 1800);
    assert_eq!(budget_capped.total_cluster_mb, 10300);

    // Case 4: Host + Remote worker with custom allocatable budget
    let budget_cluster = ClusterCoordinator::calculate_budget(16000, Some(4000));
    assert_eq!(budget_cluster.host_max_mb, 12000);
    assert_eq!(budget_cluster.remote_max_mb, 4000);
    assert_eq!(budget_cluster.total_cluster_mb, 16000);

    // Case 5: NodeBudget profile calculation
    let host_node = NodeBudget::new(Uuid::new_v4(), "host-node", 9000);
    let worker_node = NodeBudget::new(Uuid::new_v4(), "worker-node", 3500);
    let budget_from_nodes =
        ClusterCoordinator::calculate_from_nodes(&host_node, Some(&worker_node));
    assert_eq!(budget_from_nodes.host_max_mb, 9000);
    assert_eq!(budget_from_nodes.remote_max_mb, 3500);
    assert_eq!(budget_from_nodes.total_cluster_mb, 12500);
}

#[test]
fn test_cluster_layer_split_standalone_fit() {
    let budget = ClusterCoordinator::calculate_dynamic_budget(12000, Some(8500), Some(1800));
    let model_size = 4000 * 1024 * 1024; // 4000 MB
    let kv_cache = 500 * 1024 * 1024; // 500 MB
    let total_layers = 32;

    // Fits in 8500 MB host budget -> 100% on Host
    let split = ClusterCoordinator::plan_layer_split(
        model_size,
        kv_cache,
        total_layers,
        &budget,
        Some("192.168.1.100:50052"),
    )
    .expect("Planning should succeed");

    assert_eq!(split.total_layers, 32);
    assert_eq!(split.host_layers, 32);
    assert_eq!(split.remote_layers, 0);
    assert_eq!(split.remote_endpoint, None);
    assert!(!split.is_distributed());
    assert!(split.build_llama_args().is_empty());
}

#[test]
fn test_cluster_layer_split_overflow_offload() {
    let budget = ClusterCoordinator::calculate_dynamic_budget(12000, Some(8500), Some(1800)); // Host: 8500 MB, Remote: 1800 MB, Total: 10300 MB
    let model_size = 8500 * 1024 * 1024; // 8500 MB
    let kv_cache = 1000 * 1024 * 1024; // 1000 MB (Total required: 9500 MB)
    let total_layers = 32;

    // 9500 MB > 8500 MB host cap, but < 10300 MB cluster total
    let split = ClusterCoordinator::plan_layer_split(
        model_size,
        kv_cache,
        total_layers,
        &budget,
        Some("192.168.1.100:50052"),
    )
    .expect("Planning should succeed");

    assert_eq!(split.total_layers, 32);
    assert!(split.is_distributed());
    assert!(split.remote_layers > 0);
    assert!(split.host_layers > 0);
    assert_eq!(split.host_layers + split.remote_layers, 32);
    assert_eq!(
        split.remote_endpoint.as_deref(),
        Some("192.168.1.100:50052")
    );

    let args = split.build_llama_args();
    assert!(args.contains(&"--rpc".to_string()));
    assert!(args.contains(&"192.168.1.100:50052".to_string()));
    assert!(args.contains(&"--split-mode".to_string()));
    assert!(args.contains(&"layer".to_string()));
    assert!(args.contains(&"--tensor-split".to_string()));
}

#[test]
fn test_cluster_memory_cap_exceeded_rejection() {
    let budget = ClusterCoordinator::calculate_dynamic_budget(12000, Some(8500), Some(1800)); // Max 10300 MB
    let model_size = 11000 * 1024 * 1024; // 11000 MB
    let kv_cache = 1000 * 1024 * 1024; // 1000 MB (Total 12000 MB)

    let err = ClusterCoordinator::plan_layer_split(
        model_size,
        kv_cache,
        32,
        &budget,
        Some("192.168.1.100:50052"),
    )
    .unwrap_err();

    match err {
        ClusterError::ClusterMemoryCapExceeded {
            required_mb,
            cluster_max_mb,
            ..
        } => {
            assert_eq!(required_mb, 12000);
            assert_eq!(cluster_max_mb, 10300);
        }
        other => panic!("Expected ClusterMemoryCapExceeded, got {:?}", other),
    }
}

#[test]
fn test_cluster_missing_rpc_peer_rejection() {
    let budget = ClusterCoordinator::calculate_dynamic_budget(12000, Some(8500), None); // Standalone only (0 remote)
    let model_size = 8500 * 1024 * 1024;
    let kv_cache = 1000 * 1024 * 1024; // Total 9500 MB (exceeds 8500 MB host)

    let err =
        ClusterCoordinator::plan_layer_split(model_size, kv_cache, 32, &budget, None).unwrap_err();

    match err {
        ClusterError::NoRpcWorkerAvailable {
            overflow_mb,
            host_max_mb,
        } => {
            assert_eq!(overflow_mb, 1000);
            assert_eq!(host_max_mb, 8500);
        }
        other => panic!("Expected NoRpcWorkerAvailable, got {:?}", other),
    }
}

#[test]
fn test_transport_mode_parsing() {
    assert_eq!(
        "auto".parse::<TransportMode>().unwrap(),
        TransportMode::Auto
    );
    assert_eq!("usb".parse::<TransportMode>().unwrap(), TransportMode::Usb);
    assert_eq!("adb".parse::<TransportMode>().unwrap(), TransportMode::Usb);
    assert_eq!(
        "wifi".parse::<TransportMode>().unwrap(),
        TransportMode::Wifi
    );
    assert_eq!(
        "network".parse::<TransportMode>().unwrap(),
        TransportMode::Wifi
    );
    assert!("bluetooth".parse::<TransportMode>().is_err());
}

#[test]
fn test_wifi_transport_preserves_discovery_fallback() {
    let (endpoint, uses_usb) = nexus::tunnel::AdbTunnelSupervisor::resolve_transport_endpoint(
        TransportMode::Wifi,
        8080,
        50052,
    );

    assert_eq!(endpoint, None);
    assert!(!uses_usb);
}

#[test]
fn test_config_cluster_defaults() {
    let config = NexusConfig::default();
    assert!(config.cluster.enable_rpc);
    assert_eq!(config.cluster.rpc_port, 50052);
    assert_eq!(config.cluster.max_rpc_ram_mb, 1800);
    assert!(config.cluster.auto_offload);
    assert!(config.cluster.prefer_adb_tunnel);
}
