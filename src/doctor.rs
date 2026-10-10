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

    DoctorReport { checks }
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
}
