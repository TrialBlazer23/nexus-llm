use std::process::Command;
use thiserror::Error;
use tracing::{debug, info, warn};

pub const DEFAULT_API_PORT: u16 = 8080;
pub const DEFAULT_RPC_PORT: u16 = 50052;

#[derive(Error, Debug)]
pub enum TunnelError {
    #[error("ADB binary not found in PATH: please install android-tools-adb")]
    AdbNotFound,

    #[error("No Android device detected over USB. Check USB cable and developer mode.")]
    NoDeviceConnected,

    #[error("ADB command failed: {0}")]
    CommandFailed(String),

    #[error("I/O error executing ADB: {0}")]
    Io(#[from] std::io::Error),
}

/// Preferred transport mechanism between Node A and Node B.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransportMode {
    Auto,
    Usb,
    Wifi,
}

impl std::str::FromStr for TransportMode {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "auto" => Ok(Self::Auto),
            "usb" | "adb" => Ok(Self::Usb),
            "wifi" | "network" => Ok(Self::Wifi),
            other => Err(format!(
                "Unknown transport mode '{}'. Valid: auto, usb, wifi",
                other
            )),
        }
    }
}

impl std::fmt::Display for TransportMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Auto => write!(f, "auto"),
            Self::Usb => write!(f, "usb"),
            Self::Wifi => write!(f, "wifi"),
        }
    }
}

/// Information describing a connected Android USB device.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdbDeviceInfo {
    pub serial: String,
    pub model: Option<String>,
    pub product: Option<String>,
    pub authorized: bool,
}

/// Current status of ADB forward and reverse tunnels.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TunnelStatus {
    pub is_active: bool,
    pub api_forwarded: bool,
    pub rpc_reversed: bool,
    pub api_port: u16,
    pub rpc_port: u16,
    pub device: Option<AdbDeviceInfo>,
}

pub struct AdbTunnelSupervisor;

impl AdbTunnelSupervisor {
    /// Check if `adb` binary is available on the host system.
    pub fn is_adb_available() -> bool {
        Command::new("adb")
            .arg("version")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    }

    /// List connected ADB devices.
    pub fn list_devices() -> Result<Vec<AdbDeviceInfo>, TunnelError> {
        let output = match Command::new("adb").arg("devices").arg("-l").output() {
            Ok(o) => o,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Err(TunnelError::AdbNotFound)
            }
            Err(e) => return Err(TunnelError::Io(e)),
        };

        if !output.status.success() {
            let err = String::from_utf8_lossy(&output.stderr);
            return Err(TunnelError::CommandFailed(err.trim().to_string()));
        }

        let stdout = String::from_utf8_lossy(&output.stdout);
        let mut devices = Vec::new();

        for line in stdout.lines() {
            let line = line.trim();
            if line.starts_with("List of devices") || line.is_empty() {
                continue;
            }

            let parts: Vec<&str> = line.split_whitespace().collect();
            if parts.len() >= 2 {
                let serial = parts[0].to_string();
                let state = parts[1];
                let authorized = state == "device";

                let mut model = None;
                let mut product = None;

                for part in &parts[2..] {
                    if let Some(m) = part.strip_prefix("model:") {
                        model = Some(m.to_string());
                    } else if let Some(p) = part.strip_prefix("product:") {
                        product = Some(p.to_string());
                    }
                }

                devices.push(AdbDeviceInfo {
                    serial,
                    model,
                    product,
                    authorized,
                });
            }
        }

        Ok(devices)
    }

    /// Setup ADB forward (Mac -> Phone API) and reverse (Phone -> Mac RPC).
    pub fn setup_tunnel(
        api_port: u16,
        rpc_port: u16,
        specific_serial: Option<&str>,
    ) -> Result<TunnelStatus, TunnelError> {
        let devices = Self::list_devices()?;
        let target_device = if let Some(serial) = specific_serial {
            devices.into_iter().find(|d| d.serial == serial)
        } else {
            devices.into_iter().find(|d| d.authorized)
        };

        let device = match target_device {
            Some(d) => d,
            None => return Err(TunnelError::NoDeviceConnected),
        };

        let serial_args: &[&str] = &["-s", &device.serial];

        // 1. Forward API port: adb -s <serial> forward tcp:<port> tcp:<port>
        let forward_arg = format!("tcp:{}", api_port);
        let mut fwd_cmd = Command::new("adb");
        fwd_cmd
            .args(serial_args)
            .args(["forward", &forward_arg, &forward_arg]);
        let fwd_out = fwd_cmd.output().map_err(TunnelError::Io)?;
        let api_forwarded = fwd_out.status.success();
        if !api_forwarded {
            warn!(
                "Failed to forward ADB port {}: {}",
                api_port,
                String::from_utf8_lossy(&fwd_out.stderr)
            );
        }

        // 2. Reverse RPC port: adb -s <serial> reverse tcp:<port> tcp:<port>
        let reverse_arg = format!("tcp:{}", rpc_port);
        let mut rev_cmd = Command::new("adb");
        rev_cmd
            .args(serial_args)
            .args(["reverse", &reverse_arg, &reverse_arg]);
        let rev_out = rev_cmd.output().map_err(TunnelError::Io)?;
        let rpc_reversed = rev_out.status.success();
        if !rpc_reversed {
            warn!(
                "Failed to reverse ADB port {}: {}",
                rpc_port,
                String::from_utf8_lossy(&rev_out.stderr)
            );
        }

        info!(
            "ADB Tunnel active for device {} (Model: {:?}): Forward {}:{}, Reverse {}:{}",
            device.serial, device.model, api_port, api_port, rpc_port, rpc_port
        );

        Ok(TunnelStatus {
            is_active: api_forwarded,
            api_forwarded,
            rpc_reversed,
            api_port,
            rpc_port,
            device: Some(device),
        })
    }

    /// Lightweight check: whether an ADB forward for `api_port` is already listed.
    /// Does not create tunnels; safe to call on a periodic UI tick.
    pub fn is_forward_active(api_port: u16) -> bool {
        if !Self::is_adb_available() {
            return false;
        }
        let output = match Command::new("adb").args(["forward", "--list"]).output() {
            Ok(o) if o.status.success() => o,
            _ => return false,
        };
        let needle = format!("tcp:{}", api_port);
        String::from_utf8_lossy(&output.stdout)
            .lines()
            .any(|line| line.contains(&needle))
    }

    /// Teardown ADB port forwarding and reverse tunnels.
    pub fn teardown_tunnel(api_port: u16, rpc_port: u16) -> Result<(), TunnelError> {
        let forward_arg = format!("tcp:{}", api_port);
        let _ = Command::new("adb")
            .args(["forward", "--remove", &forward_arg])
            .output();

        let reverse_arg = format!("tcp:{}", rpc_port);
        let _ = Command::new("adb")
            .args(["reverse", "--remove", &reverse_arg])
            .output();

        debug!("ADB tunnels removed for ports {}, {}", api_port, rpc_port);
        Ok(())
    }

    /// Resolve effective API endpoint based on TransportMode.
    /// If Auto: attempts USB tunnel first; if USB device connected, returns ("http://127.0.0.1:8080", is_usb=true).
    /// If no device or Wifi mode, returns None (allowing caller to use UDP discovery).
    pub fn resolve_transport_endpoint(
        mode: TransportMode,
        api_port: u16,
        rpc_port: u16,
    ) -> (Option<String>, bool) {
        match mode {
            TransportMode::Wifi => (None, false),
            TransportMode::Usb => match Self::setup_tunnel(api_port, rpc_port, None) {
                Ok(status) if status.is_active => {
                    (Some(format!("http://127.0.0.1:{}", api_port)), true)
                }
                _ => (None, false),
            },
            TransportMode::Auto => {
                if Self::is_adb_available() {
                    if let Ok(status) = Self::setup_tunnel(api_port, rpc_port, None) {
                        if status.is_active {
                            info!("Zero-latency USB cable detected via ADB: routing over 127.0.0.1:{}", api_port);
                            return (Some(format!("http://127.0.0.1:{}", api_port)), true);
                        }
                    }
                }
                (None, false)
            }
        }
    }
}
