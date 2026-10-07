use nexus::config::{expand_tilde, NexusConfig};
use nexus::discovery::DiscoveryService;
use nexus::supervisor::{LlamaServerConfig, ProcessSupervisor, SupervisorError};
use nexus::sysinfo::{AccelerationBackend, SystemProfile};
use std::io::Write;
use std::path::PathBuf;
use tempfile::NamedTempFile;
use uuid::Uuid;

#[test]
fn test_sysinfo_probing() {
    let profile = SystemProfile::probe();
    println!("Probed System Profile: {:?}", profile);

    assert!(profile.total_ram_mb > 0, "Total RAM should be non-zero");
    assert!(
        profile.available_ram_mb > 0,
        "Available RAM should be non-zero"
    );
    assert!(
        profile.recommended_threads >= 1,
        "Recommended threads should be >= 1"
    );

    // On Termux ARM64, backend should be Vulkan or ArmCpuDotProd
    let valid_backends = [
        AccelerationBackend::Vulkan,
        AccelerationBackend::ArmCpuDotProd,
        AccelerationBackend::X86Baseline,
        AccelerationBackend::GenericCpu,
    ];
    assert!(valid_backends.contains(&profile.detected_backend));
}

#[test]
fn test_sysinfo_meminfo_parsing() {
    let mock_meminfo = "\
MemTotal:       12582912 kB
MemFree:          524288 kB
MemAvailable:    8388608 kB
Buffers:           10240 kB
Cached:          3145728 kB
";
    let (total_mb, avail_mb) = SystemProfile::parse_meminfo(mock_meminfo);
    assert_eq!(total_mb, 12288); // 12582912 / 1024
    assert_eq!(avail_mb, 8192); // 8388608 / 1024
}

#[test]
fn test_sysinfo_backend_detection_cpuinfo() {
    let arm_dotprod_cpuinfo = "\
processor	: 0
BogoMIPS	: 38.40
Features	: fp asimd evtstrm aes pmull sha1 sha2 crc32 atomics asimddp i8mm
CPU implementer	: 0x41
";
    // ARM without Vulkan -> detects ArmCpuDotProd
    let backend_arm_cpu =
        SystemProfile::parse_cpuinfo_backend("aarch64", arm_dotprod_cpuinfo, false);
    assert_eq!(backend_arm_cpu, AccelerationBackend::ArmCpuDotProd);

    // ARM with Vulkan -> detects Vulkan
    let backend_arm_vulkan =
        SystemProfile::parse_cpuinfo_backend("aarch64", arm_dotprod_cpuinfo, true);
    assert_eq!(backend_arm_vulkan, AccelerationBackend::Vulkan);

    // x86_64 -> detects X86Baseline
    let backend_x86 = SystemProfile::parse_cpuinfo_backend("x86_64", "", false);
    assert_eq!(backend_x86, AccelerationBackend::X86Baseline);
}

#[test]
fn test_memory_guard_enforcement() {
    // Construct a profile with exactly 4,000 MB available RAM.
    // 75% memory ceiling = 3,000 MB = 3,145,728,000 bytes.
    let profile = SystemProfile {
        total_ram_mb: 8000,
        available_ram_mb: 4000,
        detected_backend: AccelerationBackend::ArmCpuDotProd,
        recommended_threads: 6,
    };

    assert_eq!(profile.max_allowed_memory_bytes(), 3_000 * 1024 * 1024);
    assert_eq!(
        profile.max_allowed_memory_bytes_pct(50),
        2_000 * 1024 * 1024
    );
    assert_eq!(
        profile.max_allowed_memory_bytes_pct(100),
        4_000 * 1024 * 1024
    );

    // 1. Safe small model (500 MB) with 1024 context tokens (~200 MB KV):
    // Total ~ 700 MB <= 3000 MB -> should pass.
    let small_model_bytes = 500 * 1024 * 1024;
    assert!(profile.can_safely_load(small_model_bytes, 1024));

    // 2. Dangerous model (2800 MB) with 4096 context tokens (~800 MB KV):
    // Total ~ 3600 MB > 3000 MB -> MUST be blocked by Android LMK Guard.
    let large_model_bytes = 2800 * 1024 * 1024;
    assert!(!profile.can_safely_load(large_model_bytes, 4096));

    // 3. Excessive model directly exceeding ceiling (3500 MB) with 0 context:
    let excessive_model_bytes = 3500 * 1024 * 1024;
    assert!(!profile.can_safely_load(excessive_model_bytes, 0));
}

#[test]
fn test_config_defaults_and_serde() {
    let config = NexusConfig::default();

    // Verify default constraints specified in DESIGN_SPEC.md
    assert_eq!(config.node.role, "host");
    assert!(config.hardware.acceleration.prefer_gpu);
    assert_eq!(config.hardware.acceleration.gpu_layers, 99);
    assert_eq!(config.hardware.acceleration.cpu_threads, 6);
    assert_eq!(config.hardware.safety.max_ram_usage_percent, 75);
    assert_eq!(config.network.api_port, 8080);
    assert_eq!(config.network.control_port, 9998);
    assert_eq!(config.network.discovery_port, 9999);

    // Round-trip TOML serialization
    let toml_str = toml::to_string(&config).expect("Failed to serialize config");
    let deserialized: NexusConfig =
        toml::from_str(&toml_str).expect("Failed to deserialize config");
    assert_eq!(config, deserialized);
}

#[test]
fn test_config_custom_toml_parsing() {
    let custom_toml = r#"
[node]
role = "client"
name = "macrowave"

[hardware.acceleration]
prefer_gpu = false
gpu_layers = 0
cpu_threads = 2

[hardware.safety]
max_ram_usage_percent = 70

[network]
api_port = 9090
"#;

    let parsed: NexusConfig = toml::from_str(custom_toml).expect("Failed to parse custom toml");
    assert_eq!(parsed.node.role, "client");
    assert_eq!(parsed.node.name, "macrowave");
    assert!(!parsed.hardware.acceleration.prefer_gpu);
    assert_eq!(parsed.hardware.acceleration.gpu_layers, 0);
    assert_eq!(parsed.hardware.acceleration.cpu_threads, 2);
    assert_eq!(parsed.hardware.safety.max_ram_usage_percent, 70);
    assert_eq!(parsed.network.api_port, 9090);
    // Unspecified fields should keep default
    assert_eq!(parsed.network.discovery_port, 9999);
}

#[test]
fn test_config_network_fallbacks_round_trip() {
    let custom_toml = r#"
[network]
static_peers = ["10.0.0.99", "http://127.0.0.1:8080"]
default_host = "http://10.0.0.1:8080"
"#;

    let parsed: NexusConfig = toml::from_str(custom_toml).expect("Failed to parse fallback config");
    assert_eq!(
        parsed.network.static_peers,
        vec!["10.0.0.99".to_string(), "http://127.0.0.1:8080".to_string()]
    );
    assert_eq!(
        parsed.network.default_host.as_deref(),
        Some("http://10.0.0.1:8080")
    );

    let serialized = toml::to_string(&parsed).expect("Failed to serialize fallback config");
    let round_trip: NexusConfig =
        toml::from_str(&serialized).expect("Failed to deserialize fallback config");
    assert_eq!(round_trip.network.static_peers, parsed.network.static_peers);
    assert_eq!(round_trip.network.default_host, parsed.network.default_host);
}

#[test]
fn test_config_persists_identity_on_first_load() {
    let file = NamedTempFile::new().expect("temporary config file");
    std::fs::write(file.path(), "[node]\nrole = \"client\"\n").expect("write legacy config");

    let loaded = NexusConfig::load_from_path(file.path()).expect("load config");
    assert_ne!(loaded.node.id, "auto");
    assert!(Uuid::parse_str(&loaded.node.id).is_ok());

    let reloaded = NexusConfig::load_from_path(file.path()).expect("reload config");
    assert_eq!(loaded.node.id, reloaded.node.id);
}

#[test]
fn test_config_discovery_security_defaults_are_backward_compatible() {
    let parsed: NexusConfig =
        toml::from_str("[network]\napi_port = 9090\n").expect("parse legacy network config");
    assert!(parsed.network.discovery.enabled);
    assert_eq!(parsed.network.discovery.protocol_version, 1);
    assert_eq!(parsed.network.security.protocol_version, 1);
    assert!(!parsed.network.security.require_pairing);
    assert!(parsed.network.discovery.mdns.enabled);
}

#[test]
fn test_config_rejects_invalid_loaded_role() {
    let file = NamedTempFile::new().expect("temporary config file");
    std::fs::write(file.path(), "[node]\nrole = \"invalid\"\n").expect("write invalid config");
    let result = NexusConfig::load_from_path(file.path());
    assert!(result.is_err());
}

#[test]
fn test_discovery_uses_configured_identity() {
    let expected = Uuid::new_v4();
    let mut config = NexusConfig::default();
    config.node.id = expected.to_string();
    let discovery = DiscoveryService::new(config, None);
    assert_eq!(discovery.node_uuid(), expected);
}

#[test]
fn test_node_identity_baseline_is_explicit_until_persistence() {
    let mut config = NexusConfig::default();
    config.node.id = "baseline-node-id".to_string();

    let serialized = toml::to_string(&config).expect("Failed to serialize node identity");
    let restored: NexusConfig =
        toml::from_str(&serialized).expect("Failed to deserialize node identity");

    assert_eq!(restored.node.id, "baseline-node-id");
    assert_eq!(restored.node.id, config.node.id);
}

#[test]
fn test_tilde_expansion() {
    let expanded = expand_tilde("~/models");
    assert!(!expanded.to_string_lossy().starts_with("~"));
}

#[test]
fn test_supervisor_command_args_builder() {
    let cfg = LlamaServerConfig {
        binary_path: PathBuf::from("llama-server"),
        model_path: PathBuf::from("/path/to/model.gguf"),
        host: "0.0.0.0".to_string(),
        port: 8080,
        gpu_layers: 99,
        threads: 6,
        context_size: 4096,
        extra_args: Vec::new(),
        use_mmap: true,
        use_mlock: false,
        cpu_threads_batch: 6,
        fallback_to_cpu: true,
        cache_type_k: None,
        cache_type_v: None,
        memory_budget_percent: 75,
    };

    // Test Vulkan offload args (-ngl 99)
    let vulkan_args = cfg.build_args(99);
    assert!(vulkan_args.contains(&"-ngl".to_string()));
    assert_eq!(
        vulkan_args
            .iter()
            .skip_while(|&x| x != "-ngl")
            .nth(1)
            .unwrap(),
        "99"
    );
    assert!(vulkan_args.contains(&"--port".to_string()));
    assert_eq!(
        vulkan_args
            .iter()
            .skip_while(|&x| x != "--port")
            .nth(1)
            .unwrap(),
        "8080"
    );
    assert!(vulkan_args.contains(&"--alias".to_string()));
    assert_eq!(
        vulkan_args
            .iter()
            .skip_while(|&x| x != "--alias")
            .nth(1)
            .unwrap(),
        "model"
    );
    assert!(!vulkan_args.iter().any(|a| a == "--no-mmap"));

    // Test CPU fallback args (-ngl 0)
    let cpu_args = cfg.build_args(0);
    assert_eq!(
        cpu_args.iter().skip_while(|&x| x != "-ngl").nth(1).unwrap(),
        "0"
    );

    let mut no_mmap = cfg.clone();
    no_mmap.use_mmap = false;
    let args = no_mmap.build_args(99);
    assert!(args.iter().any(|a| a == "--no-mmap"));

    // Phase 11: mlock, -tb, cache-type, health probe host
    assert!(cfg.build_args(99).iter().any(|a| a == "-tb"));
    let mut locked = cfg.clone();
    locked.use_mlock = true;
    locked.cache_type_k = Some("q8_0".into());
    locked.cache_type_v = Some("q8_0".into());
    let locked_args = locked.build_args(99);
    assert!(locked_args.iter().any(|a| a == "--mlock"));
    assert!(locked_args.iter().any(|a| a == "--cache-type-k"));
    assert_eq!(cfg.health_probe_host(), "127.0.0.1");
    assert!(!cfg.fallback_to_cpu || cfg.fallback_to_cpu); // field present
    let policy = nexus::supervisor::SupervisorPolicy::default();
    assert!(policy.backoff_delay(0).as_millis() >= 1000);
    assert!(policy.backoff_delay(2) > policy.backoff_delay(0));
}

#[tokio::test]
async fn test_supervisor_preflight_binary_not_found() {
    let cfg = LlamaServerConfig {
        binary_path: PathBuf::from("/non/existent/llama-server-binary-12345"),
        model_path: PathBuf::from("/non/existent/model.gguf"),
        host: "127.0.0.1".to_string(),
        port: 8080,
        gpu_layers: 0,
        threads: 2,
        context_size: 512,
        extra_args: Vec::new(),
        use_mmap: true,
        use_mlock: false,
        cpu_threads_batch: 6,
        fallback_to_cpu: true,
        cache_type_k: None,
        cache_type_v: None,
        memory_budget_percent: 75,
    };

    let res = ProcessSupervisor::spawn_with_fallback(cfg).await;
    match res {
        Err(SupervisorError::BinaryNotFound(p)) => {
            assert_eq!(p, PathBuf::from("/non/existent/llama-server-binary-12345"));
        }
        other => panic!("Expected BinaryNotFound error, got: {:?}", other.err()),
    }
}

#[tokio::test]
async fn test_supervisor_preflight_model_not_found() {
    // Use an existing binary (e.g. `ls` or `sh`) to pass the binary check
    let bin_path = if PathBuf::from("/data/data/com.termux/files/usr/bin/sh").exists() {
        PathBuf::from("/data/data/com.termux/files/usr/bin/sh")
    } else {
        PathBuf::from("/bin/sh")
    };

    let cfg = LlamaServerConfig {
        binary_path: bin_path,
        model_path: PathBuf::from("/non/existent/model-does-not-exist.gguf"),
        host: "127.0.0.1".to_string(),
        port: 8080,
        gpu_layers: 0,
        threads: 2,
        context_size: 512,
        extra_args: Vec::new(),
        use_mmap: true,
        use_mlock: false,
        cpu_threads_batch: 6,
        fallback_to_cpu: true,
        cache_type_k: None,
        cache_type_v: None,
        memory_budget_percent: 75,
    };

    let res = ProcessSupervisor::spawn_with_fallback(cfg).await;
    match res {
        Err(SupervisorError::ModelNotFound(p)) => {
            assert_eq!(p, PathBuf::from("/non/existent/model-does-not-exist.gguf"));
        }
        other => panic!("Expected ModelNotFound error, got: {:?}", other.err()),
    }
}

#[tokio::test]
async fn test_supervisor_memory_cap_rejection() {
    let bin_path = if PathBuf::from("/data/data/com.termux/files/usr/bin/sh").exists() {
        PathBuf::from("/data/data/com.termux/files/usr/bin/sh")
    } else {
        PathBuf::from("/bin/sh")
    };

    // Create a temporary mock model file
    let mut temp_model = NamedTempFile::new().expect("Failed to create temp file");
    // Write 1 KB dummy content
    temp_model
        .write_all(&[0u8; 1024])
        .expect("Failed to write to temp file");

    // Request an absurdly large context size that forces estimated KV memory
    // to exceed the 75% memory ceiling
    let profile = SystemProfile::probe();
    let avail_mb = profile.available_ram_mb;
    // Each 1000 tokens is ~200MB KV. So (avail_mb * 10) tokens is ~2x total memory!
    let insane_context = (avail_mb as usize) * 10 * 5;

    let cfg = LlamaServerConfig {
        binary_path: bin_path,
        model_path: temp_model.path().to_path_buf(),
        host: "127.0.0.1".to_string(),
        port: 8080,
        gpu_layers: 0,
        threads: 2,
        context_size: insane_context,
        extra_args: Vec::new(),
        use_mmap: true,
        use_mlock: false,
        cpu_threads_batch: 6,
        fallback_to_cpu: true,
        cache_type_k: None,
        cache_type_v: None,
        memory_budget_percent: 75,
    };

    let res = ProcessSupervisor::spawn_with_fallback(cfg).await;
    match res {
        Err(SupervisorError::MemoryCapExceeded { .. }) => {
            // Correctly blocked by Android LMK Guard
        }
        other => panic!("Expected MemoryCapExceeded error, got: {:?}", other.err()),
    }
}
