use crate::adb_links::{
    adb_forward_dry_run, plan_forward_argvs, remove_forward_argv, resolve_device_forward_ports,
    AdbLinkCache,
};
use std::process::Command;
use thiserror::Error;
use tracing::{debug, info, warn};

pub use crate::adb_links::{allocate_forwards, parse_adb_devices_l, HostForward};

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

    #[error("invalid ADB host port config: {0}")]
    InvalidConfig(String),

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
        Ok(parse_adb_devices_l(&stdout))
    }

    /// Setup one USB link.
    ///
    /// `specific_serial == None` uses only `allocate_forwards(...)[0]` (sorted serial,
    /// host ports from config). No authorized device returns `NoDeviceConnected`.
    /// Device API/control ports come from the serial cache, otherwise `api_port` and
    /// the configured control port. RPC reverse stays on this one serial.
    /// An on-disk config that fails `NexusConfig::validate` is refused before `adb` runs.
    pub fn setup_tunnel(
        api_port: u16,
        rpc_port: u16,
        specific_serial: Option<&str>,
    ) -> Result<TunnelStatus, TunnelError> {
        let (host_port_base, max_devices, control_port) = adb_host_settings()?;
        let devices = Self::list_devices()?;
        let forward = select_one_forward(&devices, host_port_base, max_devices, specific_serial)?;
        let device = devices.into_iter().find(|d| d.serial == forward.serial);

        let cache = load_link_cache();
        let (device_api, device_control) =
            resolve_device_forward_ports(&cache, &forward.serial, api_port, control_port);
        let device_rpc = cache
            .ports_for(&forward.serial)
            .map(|(_api, _control, rpc)| rpc)
            .unwrap_or(rpc_port);

        let api_args =
            crate::adb_links::forward_argv(&forward.serial, forward.host_api, device_api);
        let control_args =
            crate::adb_links::forward_argv(&forward.serial, forward.host_control, device_control);
        let reverse_args = reverse_argv(&forward.serial, device_rpc, rpc_port);

        let api_forwarded = run_adb(&api_args)?;
        let _ = run_adb(&control_args)?;
        let rpc_reversed = run_adb(&reverse_args)?;

        info!(
            "ADB Tunnel active for device {} (Model: {:?}): Forward {}:{}, Control {}:{}, Reverse {}:{}",
            forward.serial,
            device.as_ref().and_then(|d| d.model.as_deref()),
            forward.host_api,
            device_api,
            forward.host_control,
            device_control,
            device_rpc,
            rpc_port
        );

        Ok(TunnelStatus {
            is_active: api_forwarded,
            api_forwarded,
            rpc_reversed,
            api_port: forward.host_api,
            rpc_port,
            device,
        })
    }

    /// Forward every allocated serial.
    ///
    /// When `NEXUS_ADB_DRY_RUN=1`, the forward argument list is built and `adb` is not spawned.
    /// Device ports come from the serial cache, otherwise `device_api` and `device_control`.
    pub fn setup_all_tunnels(
        devices: &[AdbDeviceInfo],
        host_port_base: u16,
        max_devices: u8,
        device_api: u16,
        device_control: u16,
    ) -> Vec<HostForward> {
        let (forwards, skipped) = allocate_forwards(devices, host_port_base, max_devices);
        if !skipped.is_empty() {
            warn!(
                "ADB max_devices cap skipped serials: {}",
                skipped.join(", ")
            );
        }
        let cache = load_link_cache();
        let commands = plan_forward_argvs(&forwards, &cache, device_api, device_control);
        if adb_forward_dry_run(std::env::var("NEXUS_ADB_DRY_RUN").ok().as_deref()) {
            debug!(
                "NEXUS_ADB_DRY_RUN=1; built {} adb forward commands without spawning",
                commands.len()
            );
            return forwards;
        }
        for args in &commands {
            if let Err(err) = run_adb(args) {
                warn!("ADB forward could not start: {err}");
            }
        }
        forwards
    }

    /// Host forwards that `adb forward --list` currently shows.
    ///
    /// `None` means `adb` is missing, so the caller must not clear USB state.
    /// An empty vec means `adb` ran and no allocated forward is listed.
    pub fn active_authorized_forwards(
        host_port_base: u16,
        max_devices: u8,
    ) -> Option<Vec<HostForward>> {
        if !Self::is_adb_available() {
            return None;
        }
        let devices = Self::list_devices().ok()?;
        let (forwards, _) = allocate_forwards(&devices, host_port_base, max_devices);
        let listed = listed_forward_ports();
        Some(
            forwards
                .into_iter()
                .filter(|forward| listed.contains(&forward.host_api))
                .collect(),
        )
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

    /// Teardown forwards for each currently allocated serial.
    ///
    /// `adb forward --remove` is always `adb -s <serial> forward --remove tcp:<host_port>`
    /// for both host ports. An unscoped `forward --remove` would drop every phone.
    pub fn teardown_tunnel(api_port: u16, rpc_port: u16) -> Result<(), TunnelError> {
        let (host_port_base, max_devices, _) = adb_host_settings()?;
        let devices = match Self::list_devices() {
            Ok(devices) => devices,
            Err(err) => {
                warn!("ADB teardown skipped; device list failed: {err}");
                return Ok(());
            }
        };
        let (forwards, _) = allocate_forwards(&devices, host_port_base, max_devices);
        let cache = load_link_cache();
        for forward in &forwards {
            let _ = Self::teardown_serial(&forward.serial, forward.host_api, forward.host_control);
            let device_rpc = cache
                .ports_for(&forward.serial)
                .map(|(_api, _control, rpc)| rpc)
                .unwrap_or(rpc_port);
            let reverse = remove_reverse_argv(&forward.serial, device_rpc);
            if let Err(err) = run_adb(&reverse) {
                warn!(
                    "ADB reverse --remove could not start for {}: {err}",
                    forward.serial
                );
            }
        }
        debug!(
            "ADB tunnels removed for {} serial(s); api hint {api_port}, rpc {rpc_port}",
            forwards.len()
        );
        Ok(())
    }

    /// Remove both host forwards for one serial. Always passes `-s`.
    pub fn teardown_serial(
        serial: &str,
        host_api: u16,
        host_control: u16,
    ) -> Result<(), TunnelError> {
        for port in [host_api, host_control] {
            let args = remove_forward_argv(serial, port);
            if let Err(err) = run_adb(&args) {
                warn!("ADB forward --remove tcp:{port} for {serial} could not start: {err}");
            }
        }
        Ok(())
    }

    /// Resolve the API endpoint for `mode`.
    /// USB and Auto return `http://127.0.0.1:<host_api>` when the single forward is active.
    /// Wifi, or no authorized device, returns None so the caller can use discovery.
    pub fn resolve_transport_endpoint(
        mode: TransportMode,
        api_port: u16,
        rpc_port: u16,
    ) -> (Option<String>, bool) {
        match mode {
            TransportMode::Wifi => (None, false),
            TransportMode::Usb => match Self::setup_tunnel(api_port, rpc_port, None) {
                Ok(status) if status.is_active => {
                    (Some(format!("http://127.0.0.1:{}", status.api_port)), true)
                }
                _ => (None, false),
            },
            TransportMode::Auto => {
                if Self::is_adb_available() {
                    if let Ok(status) = Self::setup_tunnel(api_port, rpc_port, None) {
                        if status.is_active {
                            info!(
                                "Zero-latency USB cable detected via ADB: routing over 127.0.0.1:{}",
                                status.api_port
                            );
                            return (Some(format!("http://127.0.0.1:{}", status.api_port)), true);
                        }
                    }
                }
                (None, false)
            }
        }
    }
}

/// First allocated forward, or the named serial when it received a host port.
fn select_one_forward(
    devices: &[AdbDeviceInfo],
    host_port_base: u16,
    max_devices: u8,
    specific_serial: Option<&str>,
) -> Result<HostForward, TunnelError> {
    let (forwards, _) = allocate_forwards(devices, host_port_base, max_devices);
    let chosen = match specific_serial {
        Some(serial) => forwards
            .into_iter()
            .find(|forward| forward.serial == serial),
        None => forwards.into_iter().next(),
    };
    chosen.ok_or(TunnelError::NoDeviceConnected)
}

/// Host-port block from config without calling `NexusConfig::load`.
///
/// A missing file uses `AdbConfig` defaults (base 18080, 8 devices). A file that parses
/// must pass `NexusConfig::validate`, including the reserved host-port rule.
fn adb_host_settings() -> Result<(u16, u8, u16), TunnelError> {
    let path = std::env::var("NEXUS_CONFIG")
        .ok()
        .filter(|value| !value.is_empty())
        .map(std::path::PathBuf::from)
        .unwrap_or_else(crate::config::NexusConfig::default_config_path);
    adb_host_settings_at(&path)
}

fn adb_host_settings_at(path: &std::path::Path) -> Result<(u16, u8, u16), TunnelError> {
    let content = match std::fs::read_to_string(path) {
        Ok(content) => content,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            return Ok(default_adb_host_settings());
        }
        Err(err) => return Err(TunnelError::Io(err)),
    };
    let cfg: crate::config::NexusConfig = toml::from_str(&content)
        .map_err(|err| TunnelError::InvalidConfig(format!("could not parse {path:?}: {err}")))?;
    cfg.validate()
        .map_err(|err| TunnelError::InvalidConfig(err.to_string()))?;
    Ok((
        cfg.cluster.adb.host_port_base,
        cfg.cluster.adb.max_devices,
        cfg.network.control_port,
    ))
}

fn default_adb_host_settings() -> (u16, u8, u16) {
    let cfg = crate::config::NexusConfig::default();
    (
        cfg.cluster.adb.host_port_base,
        cfg.cluster.adb.max_devices,
        cfg.network.control_port,
    )
}

fn listed_forward_ports() -> std::collections::HashSet<u16> {
    let output = match Command::new("adb").args(["forward", "--list"]).output() {
        Ok(output) if output.status.success() => output,
        _ => return std::collections::HashSet::new(),
    };
    let text = String::from_utf8_lossy(&output.stdout);
    let mut ports = std::collections::HashSet::new();
    for line in text.lines() {
        for part in line.split_whitespace() {
            if let Some(port) = part.strip_prefix("tcp:") {
                if let Ok(port) = port.parse::<u16>() {
                    ports.insert(port);
                }
            }
        }
    }
    ports
}

fn load_link_cache() -> AdbLinkCache {
    AdbLinkCache::load(&AdbLinkCache::default_path()).unwrap_or_else(|err| {
        warn!("ADB link cache unavailable, using local device ports: {err}");
        AdbLinkCache::default()
    })
}

/// Spawn `adb`. A non-zero exit is `Ok(false)` plus a warning. A missing binary is `AdbNotFound`.
pub fn run_adb_command(args: &[String]) -> Result<bool, TunnelError> {
    run_adb(args)
}

/// `adb forward --list` stdout. `AdbNotFound` means the caller must not clear Wi-Fi peers.
pub fn read_forward_list() -> Result<String, TunnelError> {
    let output = match Command::new("adb").args(["forward", "--list"]).output() {
        Ok(output) => output,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            return Err(TunnelError::AdbNotFound);
        }
        Err(err) => return Err(TunnelError::Io(err)),
    };
    if !output.status.success() {
        let err = String::from_utf8_lossy(&output.stderr);
        return Err(TunnelError::CommandFailed(err.trim().to_string()));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Spawn `adb`. A non-zero exit is `Ok(false)` plus a warning. A missing binary is `AdbNotFound`.
fn run_adb(args: &[String]) -> Result<bool, TunnelError> {
    let output = match Command::new("adb").args(args).output() {
        Ok(output) => output,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            return Err(TunnelError::AdbNotFound);
        }
        Err(err) => return Err(TunnelError::Io(err)),
    };
    if output.status.success() {
        return Ok(true);
    }
    let err = String::from_utf8_lossy(&output.stderr);
    warn!(
        "ADB command failed (adb {}): {}",
        args.join(" "),
        err.trim()
    );
    Ok(false)
}

/// `adb -s <serial> reverse tcp:<device_port> tcp:<local_port>`.
pub fn control_reverse_argv(serial: &str, device_port: u16, local_port: u16) -> Vec<String> {
    reverse_argv(serial, device_port, local_port)
}

/// Reverse `device_port` on the phone to the host control port. Does not pick the port.
pub fn install_control_reverse(
    serial: &str,
    device_port: u16,
    local_port: u16,
) -> Result<(), TunnelError> {
    let args = reverse_argv(serial, device_port, local_port);
    run_adb(&args)?;
    Ok(())
}

fn reverse_argv(serial: &str, device_port: u16, local_port: u16) -> Vec<String> {
    vec![
        "-s".to_string(),
        serial.to_string(),
        "reverse".to_string(),
        format!("tcp:{device_port}"),
        format!("tcp:{local_port}"),
    ]
}

fn remove_reverse_argv(serial: &str, device_port: u16) -> Vec<String> {
    vec![
        "-s".to_string(),
        serial.to_string(),
        "reverse".to_string(),
        "--remove".to_string(),
        format!("tcp:{device_port}"),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_keeps_unauthorized_and_allocate_skips_them() {
        let stdout = "\
List of devices attached
serial-b device usb:1-1 product:foo model:Bar
serial-a device usb:1-2 product:foo model:Baz
serial-c unauthorized usb:1-3
";
        let devices = parse_adb_devices_l(stdout);
        assert_eq!(devices.len(), 3);
        let (forwards, skipped) = allocate_forwards(&devices, 18080, 8);
        assert!(skipped.is_empty());
        assert_eq!(forwards[0].serial, "serial-a");
        assert_eq!(forwards[0].host_api, 18080);
        assert_eq!(forwards[0].host_control, 18081);
        assert_eq!(forwards[1].serial, "serial-b");
        assert_eq!(forwards[1].host_api, 18082);
        assert_eq!(forwards[1].host_control, 18083);
        assert!(!forwards.iter().any(|f| f.serial == "serial-c"));
    }

    #[test]
    fn max_devices_reports_skipped_serials() {
        let stdout = "\
serial-a device
serial-b device
serial-c device
";
        let devices = parse_adb_devices_l(stdout);
        let (forwards, skipped) = allocate_forwards(&devices, 18080, 2);
        assert_eq!(forwards.len(), 2);
        assert_eq!(skipped, vec!["serial-c".to_string()]);
    }

    #[test]
    fn parse_reads_model_product_and_only_device_is_authorized() {
        let devices = parse_adb_devices_l(
            "serial-b device usb:1-1 product:foo model:Bar\nserial-c offline\n",
        );
        assert!(devices[0].authorized);
        assert_eq!(devices[0].product.as_deref(), Some("foo"));
        assert_eq!(devices[0].model.as_deref(), Some("Bar"));
        assert!(!devices[1].authorized);
        assert!(devices[1].model.is_none());
        assert!(devices[1].product.is_none());
    }

    #[test]
    fn skipped_serials_follow_byte_sort() {
        let devices = parse_adb_devices_l("serial-c device\nserial-a device\nserial-b device\n");
        let (forwards, skipped) = allocate_forwards(&devices, 18080, 1);
        assert_eq!(forwards[0].serial, "serial-a");
        assert_eq!(forwards[0].host_api, 18080);
        assert_eq!(forwards[0].host_control, 18081);
        assert_eq!(
            skipped,
            vec!["serial-b".to_string(), "serial-c".to_string()]
        );
    }

    #[test]
    fn allocate_skips_ports_that_do_not_fit_in_u16() {
        let devices = parse_adb_devices_l("serial-a device\nserial-b device\n");
        let (forwards, skipped) = allocate_forwards(&devices, u16::MAX, 2);
        assert!(forwards.is_empty());
        assert_eq!(
            skipped,
            vec!["serial-a".to_string(), "serial-b".to_string()]
        );
    }

    #[test]
    fn none_serial_uses_only_the_first_allocated_forward() {
        let devices =
            parse_adb_devices_l("serial-b device\nserial-a device\nserial-c unauthorized\n");
        let forward = select_one_forward(&devices, 18080, 8, None).unwrap();
        assert_eq!(forward.serial, "serial-a");
        assert_eq!(forward.host_api, 18080);
        assert_eq!(forward.host_control, 18081);
        let err = select_one_forward(&[], 18080, 8, None).unwrap_err();
        assert!(matches!(err, TunnelError::NoDeviceConnected));
        let unauthorized = select_one_forward(&devices, 18080, 8, Some("serial-c")).unwrap_err();
        assert!(matches!(unauthorized, TunnelError::NoDeviceConnected));
        let named = select_one_forward(&devices, 18080, 8, Some("serial-b")).unwrap();
        assert_eq!(named.serial, "serial-b");
        assert_eq!(named.host_api, 18082);
        assert_eq!(named.host_control, 18083);
    }

    #[test]
    fn forward_args_prefer_cache_and_remove_names_the_serial() {
        use crate::adb_links::AdbLinkRecord;
        use uuid::Uuid;

        let devices = parse_adb_devices_l("serial-b device\nserial-a device\n");
        let (forwards, skipped) = allocate_forwards(&devices, 18080, 8);
        assert!(skipped.is_empty());
        let mut cache = AdbLinkCache::default();
        cache.upsert(AdbLinkRecord {
            serial: "serial-b".into(),
            node_id: Uuid::nil(),
            device_api_port: 7000,
            device_control_port: 7001,
            device_rpc_port: 50052,
        });
        let cmds = plan_forward_argvs(&forwards, &cache, 8080, 9998);
        assert_eq!(
            cmds,
            vec![
                args(&["-s", "serial-a", "forward", "tcp:18080", "tcp:8080"]),
                args(&["-s", "serial-a", "forward", "tcp:18081", "tcp:9998"]),
                args(&["-s", "serial-b", "forward", "tcp:18082", "tcp:7000"]),
                args(&["-s", "serial-b", "forward", "tcp:18083", "tcp:7001"]),
            ]
        );
        let (api, control) = resolve_device_forward_ports(&cache, "serial-a", 8080, 9998);
        assert_eq!((api, control), (8080, 9998));
        let remove = remove_forward_argv("serial-a", 18080);
        assert_eq!(
            remove,
            args(&["-s", "serial-a", "forward", "--remove", "tcp:18080"])
        );
        let serial_at = remove.iter().position(|part| part == "-s").unwrap();
        let forward_at = remove.iter().position(|part| part == "forward").unwrap();
        assert!(serial_at < forward_at);
    }

    #[test]
    fn adb_host_settings_rejects_gateway_collision_and_defaults_when_missing() {
        let missing = std::env::temp_dir().join(format!(
            "nexus-adb-settings-missing-{}.toml",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&missing);
        let (base, max_devices, _) = adb_host_settings_at(&missing).unwrap();
        assert_eq!(base, 18080);
        assert_eq!(max_devices, 8);
        assert!(!missing.exists());

        let collision = std::env::temp_dir().join(format!(
            "nexus-adb-settings-collision-{}.toml",
            std::process::id()
        ));
        let mut cfg = crate::config::NexusConfig::default();
        cfg.cluster.adb.host_port_base = 8090;
        cfg.save_to_path(&collision).unwrap();
        let err = adb_host_settings_at(&collision).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("8090"), "{msg}");
        assert!(
            msg.contains("collides") || msg.contains("reserved"),
            "{msg}"
        );
        let _ = std::fs::remove_file(&collision);
    }

    #[test]
    fn dry_run_flag_is_only_the_digit_one() {
        assert!(adb_forward_dry_run(Some("1")));
        assert!(!adb_forward_dry_run(Some("true")));
        assert!(!adb_forward_dry_run(Some("")));
        assert!(!adb_forward_dry_run(None));
    }

    fn args(parts: &[&str]) -> Vec<String> {
        parts.iter().map(|part| (*part).to_string()).collect()
    }
}
