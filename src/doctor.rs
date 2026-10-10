//! `nexus doctor` — precondition probes for mesh / inference readiness.

use crate::config::NexusConfig;
use crate::logging;
use crate::sysinfo::SystemProfile;
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::Command;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CheckSeverity {
    Ok,
    Warn,
    Fail,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DoctorCheck {
    pub name: &'static str,
    pub severity: CheckSeverity,
    pub detail: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DoctorReport {
    pub checks: Vec<DoctorCheck>,
    /// Appliance section from [`doctor_appliance_lines`]. Printed verbatim.
    pub appliance_lines: Vec<String>,
}

impl DoctorReport {
    pub fn worst(&self) -> CheckSeverity {
        if self
            .checks
            .iter()
            .any(|c| c.severity == CheckSeverity::Fail)
        {
            CheckSeverity::Fail
        } else if self
            .checks
            .iter()
            .any(|c| c.severity == CheckSeverity::Warn)
        {
            CheckSeverity::Warn
        } else {
            CheckSeverity::Ok
        }
    }

    /// Exit code: 0 = all ok/warn only when `warn_is_ok`, else 1 on any Fail (and Warn if desired).
    pub fn exit_code(&self) -> i32 {
        match self.worst() {
            CheckSeverity::Fail => 1,
            CheckSeverity::Warn | CheckSeverity::Ok => 0,
        }
    }

    pub fn print(&self) {
        for check in &self.checks {
            let tag = match check.severity {
                CheckSeverity::Ok => "OK  ",
                CheckSeverity::Warn => "WARN",
                CheckSeverity::Fail => "FAIL",
            };
            println!("[{}] {}: {}", tag, check.name, check.detail);
        }
        if !self.appliance_lines.is_empty() {
            println!();
            for line in &self.appliance_lines {
                println!("{line}");
            }
        }
        println!();
        match self.worst() {
            CheckSeverity::Ok => println!("Doctor summary: all checks passed."),
            CheckSeverity::Warn => {
                println!("Doctor summary: warnings present (optional components missing).")
            }
            CheckSeverity::Fail => println!("Doctor summary: failures must be fixed."),
        }
    }
}

/// Run diagnostic probes against `config`. External binaries missing → WARN (not FAIL).
pub fn run_doctor(config: &NexusConfig) -> DoctorReport {
    let mut checks = vec![
        check_config(config),
        check_binary_on_path("llama-server", &config.node.llama_server_binary),
        check_binary_on_path("rpc-server", &config.node.rpc_server_binary),
        check_models_dir(&config.node.models_dir),
        check_system_profile(),
        check_port_bindable("discovery_port", config.network.discovery_port),
        check_port_bindable("control_port", config.network.control_port),
        check_port_bindable("api_port", config.network.api_port),
        check_gateway_port(config),
        check_adb(),
        check_log_dir(),
        check_display_name(config),
    ];
    checks.extend(check_security(config));
    if config.inference.moe.enabled {
        checks.push(check_binary_on_path(
            "bmoe-cli",
            &config.inference.moe.bmoe_binary,
        ));
        checks.push(check_moe_config(config));
    }

    DoctorReport {
        checks,
        appliance_lines: appliance_lines_for(config),
    }
}

/// Appliance section for `nexus doctor`.
///
/// `usb` pairs are `(serial, node label)`. `None` means the cable is up and Nexus did not answer.
/// Wi-Fi entries are [`PeerNode::label`](crate::discovery::PeerNode::label) values, not product names.
pub fn doctor_appliance_lines(
    orchestrator: Option<&str>,
    floor_tok_s: f32,
    usb: &[(&str, Option<&str>)],
    wifi_labels: &[&str],
    skipped_serials: &[&str],
) -> Vec<String> {
    let mut lines = Vec::with_capacity(2 + usb.len() + wifi_labels.len() + skipped_serials.len());
    match orchestrator.map(str::trim).filter(|file| !file.is_empty()) {
        Some(file) => lines.push(format!("appliance: {file}")),
        None => lines.push("appliance: no local model fits".to_string()),
    }
    lines.push(format!("interactive floor: {floor_tok_s:.1} tok/s"));
    for (serial, label) in usb {
        match label.map(str::trim).filter(|label| !label.is_empty()) {
            Some(label) => lines.push(format!("usb {serial}: {label}")),
            None => lines.push(format!("usb {serial}: Nexus not answering")),
        }
    }
    for label in wifi_labels {
        let label = label.trim();
        if !label.is_empty() {
            lines.push(format!("wifi: {label}"));
        }
    }
    for serial in skipped_serials {
        let serial = serial.trim();
        if !serial.is_empty() {
            lines.push(format!("adb skipped: {serial}"));
        }
    }
    lines
}

fn appliance_lines_for(config: &NexusConfig) -> Vec<String> {
    let orchestrator = crate::appliance::resolve_front_door_name(config);
    let (usb_rows, skipped) = observe_usb(config);
    let usb: Vec<(&str, Option<&str>)> = usb_rows
        .iter()
        .map(|(serial, label)| (serial.as_str(), label.as_deref()))
        .collect();
    let skipped_refs: Vec<&str> = skipped.iter().map(String::as_str).collect();
    // This process has no peer registry. Wi-Fi labels stay empty rather than inventing names.
    doctor_appliance_lines(
        orchestrator.as_deref(),
        config.inference.appliance.floor_tok_s(),
        &usb,
        &[],
        &skipped_refs,
    )
}

/// Authorized forwards, plus serials dropped by `max_devices`.
///
/// A label is the cached node id (`Node-<8>`) only when that serial's host control port accepts
/// a connection. The ADB `model:` token is not a label.
fn observe_usb(config: &NexusConfig) -> (Vec<(String, Option<String>)>, Vec<String>) {
    if !crate::tunnel::AdbTunnelSupervisor::is_adb_available() {
        return (Vec::new(), Vec::new());
    }
    let devices = match crate::tunnel::AdbTunnelSupervisor::list_devices() {
        Ok(devices) => devices,
        Err(_) => return (Vec::new(), Vec::new()),
    };
    let (forwards, skipped) = crate::adb_links::allocate_forwards(
        &devices,
        config.cluster.adb.host_port_base,
        config.cluster.adb.max_devices,
    );
    let cache =
        crate::adb_links::AdbLinkCache::load(&crate::adb_links::AdbLinkCache::default_path())
            .unwrap_or_default();
    let usb = forwards
        .into_iter()
        .map(|forward| {
            let label = cache
                .links
                .iter()
                .find(|link| link.serial == forward.serial)
                .filter(|_| control_port_open(forward.host_control))
                .map(|link| node_label(link.node_id));
            (forward.serial, label)
        })
        .collect();
    (usb, skipped)
}

fn node_label(id: uuid::Uuid) -> String {
    let text = id.to_string();
    let short = if text.len() >= 8 { &text[..8] } else { &text };
    format!("Node-{short}")
}

fn control_port_open(port: u16) -> bool {
    let addr = std::net::SocketAddr::from(([127, 0, 0, 1], port));
    std::net::TcpStream::connect_timeout(&addr, std::time::Duration::from_millis(200)).is_ok()
}

fn check_security(config: &NexusConfig) -> Vec<DoctorCheck> {
    let mut checks = Vec::new();

    if config.network.security.pairing_enforced() {
        checks.push(DoctorCheck {
            name: "security_pairing",
            severity: CheckSeverity::Ok,
            detail: format!(
                "enforced (require_pairing={}, {} paired peers)",
                config.network.security.require_pairing,
                config.network.security.paired_peers.len()
            ),
        });
    } else {
        checks.push(DoctorCheck {
            name: "security_pairing",
            severity: CheckSeverity::Warn,
            detail: "permissive / disabled (allow_unpaired_lan=true); unauthenticated LAN control calls accepted".to_string(),
        });
    }

    if !config.network.security.pairing_enforced() && config.network.api_host == "0.0.0.0" {
        checks.push(DoctorCheck {
            name: "security_binding",
            severity: CheckSeverity::Warn,
            detail: "api_host is 0.0.0.0 with pairing disabled (mesh control plane open to LAN)"
                .to_string(),
        });
    } else {
        checks.push(DoctorCheck {
            name: "security_binding",
            severity: CheckSeverity::Ok,
            detail: format!("api_host bound to {}", config.network.api_host),
        });
    }

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let key_path = crate::node_identity::NodeIdentity::default_key_path();
        if key_path.exists() {
            if let Ok(meta) = std::fs::metadata(&key_path) {
                let mode = meta.permissions().mode() & 0o777;
                if mode == 0o600 {
                    checks.push(DoctorCheck {
                        name: "identity_key_permissions",
                        severity: CheckSeverity::Ok,
                        detail: format!("key {:?} permissions 0600 (restricted)", key_path),
                    });
                } else {
                    checks.push(DoctorCheck {
                        name: "identity_key_permissions",
                        severity: CheckSeverity::Warn,
                        detail: format!(
                            "key {:?} has permissions {:04o} (expected 0600)",
                            key_path, mode
                        ),
                    });
                }
            }
        }
    }

    checks
}

fn check_moe_config(config: &NexusConfig) -> DoctorCheck {
    match config.inference.moe.validate() {
        Ok(()) => {
            let cap = if config.inference.moe.cache_ceil_mb == 0 {
                "no operator cap".to_string()
            } else {
                format!("operator cap {} MiB", config.inference.moe.cache_ceil_mb)
            };
            DoctorCheck {
                name: "moe-stream",
                severity: CheckSeverity::Ok,
                detail: format!(
                    "enabled (cache_mb={}, {}, min_cache_mb={}, prefer={:?}, adapt={}, quality={:?})",
                    config.inference.moe.cache_mb,
                    cap,
                    config.inference.moe.min_cache_mb,
                    config.inference.moe.prefer,
                    config.inference.moe.adapt,
                    config.inference.moe.quality_mode
                ),
            }
        }
        Err(e) => DoctorCheck {
            name: "moe-stream",
            severity: CheckSeverity::Fail,
            detail: e.to_string(),
        },
    }
}

fn check_config(config: &NexusConfig) -> DoctorCheck {
    match config.validate() {
        Ok(()) => DoctorCheck {
            name: "config",
            severity: CheckSeverity::Ok,
            detail: format!(
                "valid (node={}, role={}, control_port={})",
                config.resolved_display_name(),
                config.node.role,
                config.network.control_port
            ),
        },
        Err(e) => DoctorCheck {
            name: "config",
            severity: CheckSeverity::Fail,
            detail: e.to_string(),
        },
    }
}

fn check_binary_on_path(label: &'static str, binary: &str) -> DoctorCheck {
    match which_binary(binary) {
        Some(path) => DoctorCheck {
            name: label,
            severity: CheckSeverity::Ok,
            detail: format!("found at {}", path.display()),
        },
        None => DoctorCheck {
            name: label,
            severity: CheckSeverity::Warn,
            detail: format!(
                "'{}' not found on PATH (required for nexus host / worker)",
                binary
            ),
        },
    }
}

fn which_binary(binary: &str) -> Option<PathBuf> {
    let path = Path::new(binary);
    if path.is_absolute() || binary.contains('/') {
        return if path.exists() {
            Some(path.to_path_buf())
        } else {
            None
        };
    }
    let output = Command::new("which").arg(binary).output().ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if text.is_empty() {
        None
    } else {
        Some(PathBuf::from(text))
    }
}

fn check_models_dir(dir: &Path) -> DoctorCheck {
    if !dir.exists() {
        return DoctorCheck {
            name: "models_dir",
            severity: CheckSeverity::Warn,
            detail: format!("{} does not exist yet", dir.display()),
        };
    }
    match std::fs::read_dir(dir) {
        Ok(entries) => {
            let gguf = entries
                .filter_map(|e| e.ok())
                .filter(|e| {
                    e.path()
                        .extension()
                        .and_then(|ext| ext.to_str())
                        .map(|ext| ext.eq_ignore_ascii_case("gguf"))
                        .unwrap_or(false)
                })
                .count();
            DoctorCheck {
                name: "models_dir",
                severity: CheckSeverity::Ok,
                detail: format!("{} ({} .gguf file(s))", dir.display(), gguf),
            }
        }
        Err(e) => DoctorCheck {
            name: "models_dir",
            severity: CheckSeverity::Fail,
            detail: format!("unreadable {}: {}", dir.display(), e),
        },
    }
}

fn check_system_profile() -> DoctorCheck {
    let profile = SystemProfile::probe();
    DoctorCheck {
        name: "system_profile",
        severity: CheckSeverity::Ok,
        detail: format!(
            "{} MB available / {} MB total; backend={:?}; threads={}",
            profile.available_ram_mb,
            profile.total_ram_mb,
            profile.detected_backend,
            profile.recommended_threads
        ),
    }
}

fn check_port_bindable(label: &'static str, port: u16) -> DoctorCheck {
    match TcpListener::bind(("0.0.0.0", port)) {
        Ok(_listener) => DoctorCheck {
            name: label,
            severity: CheckSeverity::Ok,
            detail: format!("UDP/TCP port {} is bindable", port),
        },
        Err(e) => DoctorCheck {
            name: label,
            severity: CheckSeverity::Fail,
            detail: format!("cannot bind port {}: {}", port, e),
        },
    }
}

fn check_gateway_port(config: &NexusConfig) -> DoctorCheck {
    if !config.network.gateway_enabled {
        return DoctorCheck {
            name: "gateway_port",
            severity: CheckSeverity::Ok,
            detail: "disabled (network.gateway_enabled=false)".to_string(),
        };
    }
    check_port_bindable("gateway_port", config.network.gateway_port)
}

fn check_adb() -> DoctorCheck {
    match which_binary("adb") {
        Some(path) => DoctorCheck {
            name: "adb",
            severity: CheckSeverity::Ok,
            detail: format!("found at {}", path.display()),
        },
        None => DoctorCheck {
            name: "adb",
            severity: CheckSeverity::Warn,
            detail: "adb not on PATH (USB tunnel unavailable)".to_string(),
        },
    }
}

fn check_log_dir() -> DoctorCheck {
    let dir = logging::log_dir();
    match std::fs::create_dir_all(&dir) {
        Ok(()) => DoctorCheck {
            name: "log_dir",
            severity: CheckSeverity::Ok,
            detail: dir.display().to_string(),
        },
        Err(e) => DoctorCheck {
            name: "log_dir",
            severity: CheckSeverity::Fail,
            detail: format!("cannot create {}: {}", dir.display(), e),
        },
    }
}

fn check_display_name(config: &NexusConfig) -> DoctorCheck {
    DoctorCheck {
        name: "display_name",
        severity: CheckSeverity::Ok,
        detail: format!(
            "node.name={:?} → {}",
            config.node.name,
            config.resolved_display_name()
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn doctor_report_exit_code_fails_on_fail() {
        let report = DoctorReport {
            checks: vec![
                DoctorCheck {
                    name: "a",
                    severity: CheckSeverity::Ok,
                    detail: "ok".into(),
                },
                DoctorCheck {
                    name: "b",
                    severity: CheckSeverity::Fail,
                    detail: "bad".into(),
                },
            ],
            appliance_lines: Vec::new(),
        };
        assert_eq!(report.exit_code(), 1);
        assert_eq!(report.worst(), CheckSeverity::Fail);
    }

    #[test]
    fn doctor_warn_only_exits_zero() {
        let report = DoctorReport {
            checks: vec![DoctorCheck {
                name: "llama-server",
                severity: CheckSeverity::Warn,
                detail: "missing".into(),
            }],
            appliance_lines: Vec::new(),
        };
        assert_eq!(report.exit_code(), 0);
    }

    #[test]
    fn run_doctor_on_default_config_completes() {
        let config = NexusConfig::default();
        let report = run_doctor(&config);
        assert!(!report.checks.is_empty());
        assert!(report.checks.iter().any(|c| c.name == "config"));
        assert!(report.checks.iter().any(|c| c.name == "system_profile"));
    }

    #[test]
    fn doctor_lines_name_serials_not_products() {
        let lines = doctor_appliance_lines(
            None,
            3.0,
            &[("serial-a", None), ("serial-b", Some("node-bbbbbbbb"))],
            &["node-cccccccc"],
            &["serial-d"],
        );
        assert_eq!(lines[0], "appliance: no local model fits");
        assert_eq!(lines[1], "interactive floor: 3.0 tok/s");
        assert!(lines
            .iter()
            .any(|l| l == "usb serial-a: Nexus not answering"));
        assert!(lines.iter().any(|l| l == "usb serial-b: node-bbbbbbbb"));
        assert!(lines.iter().any(|l| l == "wifi: node-cccccccc"));
        assert!(lines.iter().any(|l| l == "adb skipped: serial-d"));
        let named = doctor_appliance_lines(Some("front.gguf"), 3.5, &[], &[], &[]);
        assert_eq!(named[0], "appliance: front.gguf");
        assert_eq!(named[1], "interactive floor: 3.5 tok/s");
    }
}
