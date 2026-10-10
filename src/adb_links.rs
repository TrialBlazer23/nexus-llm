//! Serial cache for USB cables this machine has already probed.
//!
//! The file is `~/.nexus/adb-links.json` (override with `NEXUS_ADB_LINKS`).
//! It stores device ports from a previous Nexus probe. It is not a phone catalog.

use crate::tunnel::AdbDeviceInfo;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use thiserror::Error;
use uuid::Uuid;

const CACHE_FILE_NAME: &str = "adb-links.json";

#[derive(Debug, Error)]
pub enum AdbLinkError {
    #[error("I/O error in adb link cache: {0}")]
    Io(#[from] std::io::Error),
    #[error("adb link cache JSON error: {0}")]
    Json(#[from] serde_json::Error),
}

/// Device ports remembered for one ADB serial.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AdbLinkRecord {
    pub serial: String,
    pub node_id: Uuid,
    pub device_api_port: u16,
    pub device_control_port: u16,
    pub device_rpc_port: u16,
}

/// Host-side forward ports for one authorized serial.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostForward {
    pub serial: String,
    pub host_api: u16,
    pub host_control: u16,
}

/// On-disk cache of probed USB links.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct AdbLinkCache {
    pub links: Vec<AdbLinkRecord>,
}

impl AdbLinkCache {
    /// `NEXUS_ADB_LINKS`, otherwise `~/.nexus/adb-links.json`.
    pub fn default_path() -> PathBuf {
        if let Ok(path) = std::env::var("NEXUS_ADB_LINKS") {
            if !path.is_empty() {
                return PathBuf::from(path);
            }
        }
        if let Ok(home) = std::env::var("HOME") {
            return PathBuf::from(home).join(".nexus").join(CACHE_FILE_NAME);
        }
        if let Ok(profile) = std::env::var("USERPROFILE") {
            return PathBuf::from(profile).join(".nexus").join(CACHE_FILE_NAME);
        }
        PathBuf::from(".nexus").join(CACHE_FILE_NAME)
    }

    /// Load the cache. A missing file is an empty cache, not an error.
    pub fn load(path: &Path) -> Result<Self, AdbLinkError> {
        match fs::read(path) {
            Ok(bytes) => Ok(serde_json::from_slice(&bytes)?),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(err) => Err(err.into()),
        }
    }

    /// Atomically replace `path` with this cache.
    pub fn save(&self, path: &Path) -> Result<(), AdbLinkError> {
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                fs::create_dir_all(parent)?;
            }
        }
        let bytes = serde_json::to_vec_pretty(self)?;
        let tmp = path.with_extension("json.tmp");
        fs::write(&tmp, &bytes)?;
        fs::rename(&tmp, path)?;
        Ok(())
    }

    /// Drop records whose serial is not in the current authorized list.
    pub fn retain_present<S: AsRef<str>>(&mut self, authorized: &[S]) {
        let present: HashSet<&str> = authorized.iter().map(AsRef::as_ref).collect();
        self.links
            .retain(|link| present.contains(link.serial.as_str()));
    }

    /// Stored `(device_api, device_control, device_rpc)` for `serial`.
    pub fn ports_for(&self, serial: &str) -> Option<(u16, u16, u16)> {
        self.links
            .iter()
            .find(|link| link.serial == serial)
            .map(|link| {
                (
                    link.device_api_port,
                    link.device_control_port,
                    link.device_rpc_port,
                )
            })
    }

    /// Insert or replace the record with the same serial.
    pub fn upsert(&mut self, record: AdbLinkRecord) {
        if let Some(existing) = self
            .links
            .iter_mut()
            .find(|link| link.serial == record.serial)
        {
            *existing = record;
        } else {
            self.links.push(record);
        }
    }

    /// Serials that repeat a UUID. `pairs` is already sorted by serial.
    /// The first serial for each UUID is kept; every later one is returned.
    pub fn duplicate_uuid_losers(pairs: &[(String, Uuid)]) -> Vec<String> {
        let mut seen = HashSet::new();
        let mut losers = Vec::new();
        for (serial, node_id) in pairs {
            if !seen.insert(*node_id) {
                losers.push(serial.clone());
            }
        }
        losers
    }
}

/// Parse `adb devices -l` stdout. State `device` is authorized; other states are kept.
pub fn parse_adb_devices_l(stdout: &str) -> Vec<AdbDeviceInfo> {
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
    devices
}

/// Authorized serials, sorted by serial bytes, capped at `max_devices`.
///
/// Index `i` uses `host_api = host_port_base + i * 2` and `host_control = host_api + 1`.
/// The second vector is the authorized serials that did not receive a host port, in sorted order.
pub fn allocate_forwards(
    devices: &[AdbDeviceInfo],
    host_port_base: u16,
    max_devices: u8,
) -> (Vec<HostForward>, Vec<String>) {
    let mut authorized: Vec<&AdbDeviceInfo> = devices.iter().filter(|d| d.authorized).collect();
    authorized.sort_by(|left, right| left.serial.as_bytes().cmp(right.serial.as_bytes()));

    let mut forwards = Vec::new();
    let mut skipped = Vec::new();
    for (index, device) in authorized.into_iter().enumerate() {
        if index >= usize::from(max_devices) {
            skipped.push(device.serial.clone());
            continue;
        }
        let Some(index_u32) = u32::try_from(index).ok() else {
            skipped.push(device.serial.clone());
            continue;
        };
        let Some(host_api) = u32::from(host_port_base).checked_add(index_u32.saturating_mul(2))
        else {
            skipped.push(device.serial.clone());
            continue;
        };
        let Some(host_control) = host_api.checked_add(1) else {
            skipped.push(device.serial.clone());
            continue;
        };
        if host_api > u32::from(u16::MAX) || host_control > u32::from(u16::MAX) {
            skipped.push(device.serial.clone());
            continue;
        }
        forwards.push(HostForward {
            serial: device.serial.clone(),
            host_api: host_api as u16,
            host_control: host_control as u16,
        });
    }
    (forwards, skipped)
}

/// `adb -s <serial> forward tcp:<host> tcp:<device>`.
pub fn forward_argv(serial: &str, host_port: u16, device_port: u16) -> Vec<String> {
    vec![
        "-s".to_string(),
        serial.to_string(),
        "forward".to_string(),
        format!("tcp:{host_port}"),
        format!("tcp:{device_port}"),
    ]
}

/// `adb -s <serial> forward --remove tcp:<host>`. Never omits `-s`.
pub fn remove_forward_argv(serial: &str, host_port: u16) -> Vec<String> {
    vec![
        "-s".to_string(),
        serial.to_string(),
        "forward".to_string(),
        "--remove".to_string(),
        format!("tcp:{host_port}"),
    ]
}

/// Device API and control ports from the cache, otherwise the local defaults.
pub fn resolve_device_forward_ports(
    cache: &AdbLinkCache,
    serial: &str,
    device_api: u16,
    device_control: u16,
) -> (u16, u16) {
    match cache.ports_for(serial) {
        Some((api, control, _rpc)) => (api, control),
        None => (device_api, device_control),
    }
}

/// Forward argument lists for each host forward. Cache ports win over the defaults.
pub fn plan_forward_argvs(
    forwards: &[HostForward],
    cache: &AdbLinkCache,
    device_api: u16,
    device_control: u16,
) -> Vec<Vec<String>> {
    let mut commands = Vec::with_capacity(forwards.len() * 2);
    for forward in forwards {
        let (api, control) =
            resolve_device_forward_ports(cache, &forward.serial, device_api, device_control);
        commands.push(forward_argv(&forward.serial, forward.host_api, api));
        commands.push(forward_argv(&forward.serial, forward.host_control, control));
    }
    commands
}

/// `NEXUS_ADB_DRY_RUN=1` builds forward commands and does not spawn `adb`.
pub fn adb_forward_dry_run(flag: Option<&str>) -> bool {
    flag == Some("1")
}

/// `adb forward --list` rows: `<serial> tcp:<host> tcp:<device>`.
pub fn parse_adb_forward_list(stdout: &str) -> Vec<(String, u16)> {
    let mut rows = Vec::new();
    for line in stdout.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with("List") {
            continue;
        }
        let mut parts = line.split_whitespace();
        let Some(serial) = parts.next() else {
            continue;
        };
        let Some(host) = parts.next() else {
            continue;
        };
        let Some(port_text) = host.strip_prefix("tcp:") else {
            continue;
        };
        let Ok(port) = port_text.parse::<u16>() else {
            continue;
        };
        rows.push((serial.to_string(), port));
    }
    rows
}

/// Doctor lines for cache rows that share a UUID. The first serial in sorted order is kept.
pub fn duplicate_link_warnings(cache: &AdbLinkCache) -> Vec<String> {
    let mut pairs: Vec<(String, Uuid)> = cache
        .links
        .iter()
        .map(|link| (link.serial.clone(), link.node_id))
        .collect();
    pairs.sort_by(|left, right| left.0.as_bytes().cmp(right.0.as_bytes()));
    let losers = AdbLinkCache::duplicate_uuid_losers(&pairs);
    let mut lines = Vec::new();
    for loser in losers {
        let Some(node_id) = cache
            .links
            .iter()
            .find(|link| link.serial == loser)
            .map(|link| link.node_id)
        else {
            continue;
        };
        let Some(kept) = pairs
            .iter()
            .find(|(_, id)| *id == node_id)
            .map(|(serial, _)| serial.clone())
        else {
            continue;
        };
        lines.push(format!(
            "usb duplicate uuid: kept {kept}, tore down {loser}"
        ));
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use uuid::Uuid;

    #[test]
    fn cache_round_trip_drops_absent_serial_and_keeps_first_uuid() {
        let path = std::env::temp_dir().join(format!(
            "nexus-adb-links-{}-{}.json",
            std::process::id(),
            Uuid::new_v4()
        ));
        let _ = fs::remove_file(&path);

        let missing = AdbLinkCache::load(&path).expect("missing file");
        assert!(missing.links.is_empty());

        let id_shared = Uuid::parse_str("11111111-1111-1111-1111-111111111111").unwrap();
        let id_other = Uuid::parse_str("22222222-2222-2222-2222-222222222222").unwrap();
        let mut cache = AdbLinkCache { links: Vec::new() };
        cache.upsert(AdbLinkRecord {
            serial: "serial-b".into(),
            node_id: id_other,
            device_api_port: 8080,
            device_control_port: 9998,
            device_rpc_port: 50052,
        });
        cache.upsert(AdbLinkRecord {
            serial: "serial-a".into(),
            node_id: id_shared,
            device_api_port: 1,
            device_control_port: 2,
            device_rpc_port: 3,
        });
        cache.upsert(AdbLinkRecord {
            serial: "serial-a".into(),
            node_id: id_shared,
            device_api_port: 8080,
            device_control_port: 9998,
            device_rpc_port: 50052,
        });
        cache.upsert(AdbLinkRecord {
            serial: "serial-c".into(),
            node_id: id_shared,
            device_api_port: 8070,
            device_control_port: 9970,
            device_rpc_port: 50050,
        });
        cache.save(&path).expect("save");

        let raw = fs::read_to_string(&path).expect("read back");
        assert!(raw.contains("\"serial\": \"serial-a\""));
        assert!(raw.contains("\"node_id\": \"11111111-1111-1111-1111-111111111111\""));
        assert!(raw.contains("\"device_api_port\": 8080"));
        assert!(raw.contains("\"device_control_port\": 9998"));
        assert!(raw.contains("\"device_rpc_port\": 50052"));

        let mut loaded = AdbLinkCache::load(&path).expect("load");
        assert_eq!(loaded.ports_for("serial-a"), Some((8080, 9998, 50052)));
        assert_eq!(loaded.ports_for("serial-b"), Some((8080, 9998, 50052)));
        assert_eq!(loaded.ports_for("serial-c"), Some((8070, 9970, 50050)));
        assert_eq!(loaded.links.len(), 3);

        loaded.retain_present(&["serial-a".to_string(), "serial-c".to_string()]);
        assert!(loaded.ports_for("serial-b").is_none());
        assert_eq!(loaded.ports_for("serial-a"), Some((8080, 9998, 50052)));
        assert_eq!(loaded.links.len(), 2);

        // Pairs are already sorted by serial. The first serial of each UUID stays.
        let pairs = vec![
            ("serial-a".to_string(), id_shared),
            ("serial-b".to_string(), id_other),
            ("serial-c".to_string(), id_shared),
            ("serial-d".to_string(), id_other),
        ];
        let losers = AdbLinkCache::duplicate_uuid_losers(&pairs);
        assert_eq!(losers, vec!["serial-c".to_string(), "serial-d".to_string()]);
        assert_eq!(losers.iter().position(|s| s == "serial-a"), None);
        assert_eq!(losers.iter().position(|s| s == "serial-b"), None);

        let _ = fs::remove_file(&path);
    }

    #[test]
    fn corrupt_cache_is_an_error_and_missing_file_is_empty() {
        let missing = std::env::temp_dir().join(format!(
            "nexus-adb-links-missing-{}-{}.json",
            std::process::id(),
            Uuid::new_v4()
        ));
        let _ = fs::remove_file(&missing);
        let loaded = AdbLinkCache::load(&missing).expect("missing");
        assert!(loaded.links.is_empty());
        assert!(!missing.exists());

        let bad = std::env::temp_dir().join(format!(
            "nexus-adb-links-bad-{}-{}.json",
            std::process::id(),
            Uuid::new_v4()
        ));
        fs::write(&bad, b"{").unwrap();
        let err = AdbLinkCache::load(&bad).unwrap_err();
        assert!(err.to_string().contains("JSON"), "{err}");
        let _ = fs::remove_file(&bad);
    }
}
