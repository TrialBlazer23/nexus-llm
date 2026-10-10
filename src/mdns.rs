use crate::discovery::{BackendHealth, DiscoveryEvent, NodeRole, ServiceEndpoint};
use crate::sysinfo::PowerSource;
use mdns_sd::{ServiceDaemon, ServiceEvent, ServiceInfo};
use std::collections::{HashMap, HashSet};
use std::net::{IpAddr, ToSocketAddrs};
use tokio::sync::mpsc;
use uuid::Uuid;

pub const MDNS_BACKEND_NAME: &str = "mdns";

#[derive(Debug, thiserror::Error)]
pub enum MdnsError {
    #[error("mDNS daemon error: {0}")]
    Daemon(#[from] mdns_sd::Error),
    #[error("invalid mDNS identity: {0}")]
    InvalidIdentity(String),
    #[error("mDNS host could not be resolved: {0}")]
    Resolve(String),
}

pub struct MdnsBackend {
    daemon: ServiceDaemon,
}

impl MdnsBackend {
    pub fn new() -> Result<Self, MdnsError> {
        Ok(Self {
            daemon: ServiceDaemon::new()?,
        })
    }

    /// Registers the node as an mDNS-SD service with all positional advertisement records.
    #[allow(clippy::too_many_arguments)]
    pub fn register(
        &self,
        service_type: &str,
        instance_name: &str,
        host_name: &str,
        api_port: u16,
        rpc_port: u16,
        control_port: u16,
        node_id: Uuid,
        cluster_id: Option<Uuid>,
        role: NodeRole,
        capabilities: &[String],
        display_name: &str,
        address: IpAddr,
        battery_percent: Option<u8>,
        power_source: PowerSource,
    ) -> Result<(), MdnsError> {
        let capability_list = capabilities.join(",");
        let cluster = cluster_id.map(|id| id.to_string()).unwrap_or_default();
        let bat = match battery_percent {
            Some(percent) => percent.to_string(),
            None => String::new(),
        };
        let properties = HashMap::from([
            ("id".to_string(), node_id.to_string()),
            ("cluster".to_string(), cluster),
            ("proto".to_string(), "1".to_string()),
            ("role".to_string(), role.0.to_string()),
            ("caps".to_string(), capability_list),
            ("api".to_string(), api_port.to_string()),
            ("rpc".to_string(), rpc_port.to_string()),
            ("ctrl".to_string(), control_port.to_string()),
            ("name".to_string(), display_name.to_string()),
            ("bat".to_string(), bat),
            (
                "pwr".to_string(),
                power_source_txt(power_source).to_string(),
            ),
        ]);
        let info = ServiceInfo::new(
            service_type,
            instance_name,
            host_name,
            address.to_string(),
            api_port,
            properties,
        )?;
        self.daemon.register(info)?;
        Ok(())
    }

    pub fn browse(
        &self,
        service_type: &str,
    ) -> Result<(mpsc::Receiver<DiscoveryEvent>, tokio::task::JoinHandle<()>), MdnsError> {
        let events = self.daemon.browse(service_type)?;
        let (sender, receiver) = mpsc::channel(32);
        let task = tokio::spawn(async move {
            let mut seen_nodes = HashSet::new();
            let _ = sender
                .send(DiscoveryEvent::BackendHealth {
                    backend: MDNS_BACKEND_NAME,
                    health: BackendHealth::Started,
                })
                .await;
            loop {
                let event = match events.recv_async().await {
                    Ok(event) => event,
                    Err(_) => {
                        let _ = sender
                            .send(DiscoveryEvent::BackendHealth {
                                backend: MDNS_BACKEND_NAME,
                                health: BackendHealth::Failed,
                            })
                            .await;
                        break;
                    }
                };
                let converted = match event {
                    ServiceEvent::ServiceResolved(service) => {
                        match endpoint_from_resolved(&service) {
                            Ok(endpoint) if seen_nodes.insert(endpoint.node_id) => {
                                Some(DiscoveryEvent::ServiceFound(endpoint))
                            }
                            Ok(endpoint) => Some(DiscoveryEvent::ServiceUpdated(endpoint)),
                            Err(_) => None,
                        }
                    }
                    ServiceEvent::ServiceRemoved(_, fullname) => node_id_from_fullname(&fullname)
                        .ok()
                        .map(|node_id| DiscoveryEvent::ServiceRemoved { node_id }),
                    _ => None,
                };
                if let Some(event) = converted {
                    if sender.send(event).await.is_err() {
                        break;
                    }
                }
            }
        });
        Ok((receiver, task))
    }

    pub fn shutdown(self) -> Result<(), MdnsError> {
        self.daemon.shutdown()?;
        Ok(())
    }
}

fn endpoint_from_resolved(
    service: &mdns_sd::ResolvedService,
) -> Result<ServiceEndpoint, MdnsError> {
    let properties: HashMap<String, String> =
        service.txt_properties.clone().into_property_map_str();
    let mut endpoint = endpoint_from_properties(&properties, service.port)?;
    endpoint.addresses = (service.host.as_str(), service.port)
        .to_socket_addrs()
        .map_err(|e| MdnsError::Resolve(e.to_string()))?
        .collect();
    Ok(endpoint)
}

/// Read advertisement TXT without resolving a host or starting a daemon.
fn endpoint_from_properties(
    properties: &HashMap<String, String>,
    service_port: u16,
) -> Result<ServiceEndpoint, MdnsError> {
    let node_id = properties
        .get("id")
        .ok_or_else(|| MdnsError::InvalidIdentity("missing id TXT property".to_string()))
        .and_then(|id| {
            Uuid::parse_str(id).map_err(|e| MdnsError::InvalidIdentity(e.to_string()))
        })?;
    let role = properties
        .get("role")
        .and_then(|value| value.parse::<u8>().ok())
        .map(NodeRole)
        .unwrap_or(NodeRole::STANDALONE);
    let cluster_id = properties
        .get("cluster")
        .and_then(|id| Uuid::parse_str(id).ok());
    let api_port = properties
        .get("api")
        .and_then(|port| port.parse().ok())
        .unwrap_or(service_port);
    let rpc_port = properties
        .get("rpc")
        .and_then(|port| port.parse().ok())
        .unwrap_or(0);
    let control_port = properties
        .get("ctrl")
        .and_then(|port| port.parse().ok())
        .unwrap_or(9998);
    let capabilities = properties
        .get("caps")
        .map(|caps| {
            caps.split(',')
                .filter(|cap| !cap.is_empty())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();
    let display_name = properties
        .get("name")
        .map(|name| name.trim().to_string())
        .filter(|name| !name.is_empty())
        .unwrap_or_default();
    let (battery_percent, power_source) = power_from_txt(properties);

    Ok(ServiceEndpoint {
        node_id,
        cluster_id,
        protocol_version: properties
            .get("proto")
            .and_then(|v| v.parse().ok())
            .unwrap_or(0),
        role,
        capabilities,
        addresses: Vec::new(),
        api_port,
        rpc_port,
        control_port,
        display_name,
        battery_percent,
        power_source,
    })
}

/// `bat` is an integer 0–100. `pwr` is `ac`, `battery`, or `unknown`.
/// A missing or unusable key does not fail the parse.
fn power_from_txt(properties: &HashMap<String, String>) -> (Option<u8>, PowerSource) {
    let battery_percent = properties.get("bat").and_then(|value| parse_bat(value));
    let power_source = properties
        .get("pwr")
        .map(|value| parse_pwr(value))
        .unwrap_or(PowerSource::Unknown);
    (battery_percent, power_source)
}

fn parse_bat(value: &str) -> Option<u8> {
    let percent = value.trim().parse::<u8>().ok()?;
    if percent <= 100 {
        Some(percent)
    } else {
        None
    }
}

fn parse_pwr(value: &str) -> PowerSource {
    match value.trim() {
        "ac" => PowerSource::Ac,
        "battery" => PowerSource::Battery,
        _ => PowerSource::Unknown,
    }
}

fn power_source_txt(source: PowerSource) -> &'static str {
    match source {
        PowerSource::Ac => "ac",
        PowerSource::Battery => "battery",
        PowerSource::Unknown => "unknown",
    }
}

fn node_id_from_fullname(fullname: &str) -> Result<Uuid, MdnsError> {
    let instance = fullname.split('.').next().unwrap_or_default();
    Uuid::parse_str(instance).map_err(|e| MdnsError::InvalidIdentity(e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::endpoint_from_properties;
    use crate::sysinfo::PowerSource;
    use std::collections::HashMap;
    use uuid::Uuid;

    #[test]
    fn txt_bat_and_pwr_parse_and_missing_keys_leave_percent_none() {
        let with_power = HashMap::from([
            ("id".to_string(), Uuid::nil().to_string()),
            ("bat".to_string(), "40".to_string()),
            ("pwr".to_string(), "battery".to_string()),
        ]);
        let endpoint = endpoint_from_properties(&with_power, 8080).expect("bat and pwr parse");
        assert_eq!(endpoint.battery_percent, Some(40));
        assert_eq!(endpoint.power_source, PowerSource::Battery);

        let without = HashMap::from([("id".to_string(), Uuid::nil().to_string())]);
        let endpoint = endpoint_from_properties(&without, 8080).expect("missing keys are ignored");
        assert_eq!(endpoint.battery_percent, None);
        assert_eq!(endpoint.power_source, PowerSource::Unknown);
    }
}
