use crate::discovery::{BackendHealth, DiscoveryEvent, NodeRole, ServiceEndpoint};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::time::{Duration, Instant};
use thiserror::Error;
use uuid::Uuid;

pub const REGISTRY_METADATA_LIMIT: usize = 4096;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ObservationSource {
    Udp,
    Mdns,
    Static,
    ControlPlane,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PeerLifecycle {
    Discovered,
    Verifying,
    Healthy,
    Stale,
    Removed,
    Rejected,
}

#[derive(Debug, Clone)]
pub struct PeerRecord {
    pub node_id: Uuid,
    pub endpoint: ServiceEndpoint,
    pub lifecycle: PeerLifecycle,
    pub verified: bool,
    pub sources: Vec<ObservationSource>,
    pub last_seen: Instant,
    pub last_verified: Option<Instant>,
    pub rejection_reason: Option<String>,
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum RegistryError {
    #[error("peer registry capacity {limit} reached")]
    CapacityExceeded { limit: usize },
    #[error("peer {0} has conflicting endpoint data")]
    EndpointConflict(Uuid),
    #[error("peer metadata exceeds {limit} bytes")]
    MetadataTooLarge { limit: usize },
    #[error("peer {0} is not eligible: {1}")]
    Ineligible(Uuid, String),
}

#[derive(Debug)]
pub struct PeerRegistry {
    peers: HashMap<Uuid, PeerRecord>,
    max_peers: usize,
    stale_after: Duration,
    metadata_limit: usize,
}

impl PeerRegistry {
    pub fn new(max_peers: usize, stale_after: Duration) -> Result<Self, RegistryError> {
        if max_peers == 0 {
            return Err(RegistryError::CapacityExceeded { limit: 0 });
        }
        Ok(Self {
            peers: HashMap::new(),
            max_peers,
            stale_after,
            metadata_limit: REGISTRY_METADATA_LIMIT,
        })
    }

    pub fn with_metadata_limit(mut self, metadata_limit: usize) -> Self {
        self.metadata_limit = metadata_limit;
        self
    }

    pub fn apply_event(
        &mut self,
        event: DiscoveryEvent,
        source: ObservationSource,
        now: Instant,
    ) -> Result<(), RegistryError> {
        match event {
            DiscoveryEvent::ServiceFound(endpoint) | DiscoveryEvent::ServiceUpdated(endpoint) => {
                self.observe(endpoint, source, now)
            }
            DiscoveryEvent::ServiceRemoved { node_id } => {
                if let Some(peer) = self.peers.get_mut(&node_id) {
                    peer.lifecycle = PeerLifecycle::Removed;
                    peer.last_seen = now;
                }
                Ok(())
            }
            DiscoveryEvent::BackendHealth { .. } => Ok(()),
        }
    }

    pub fn observe(
        &mut self,
        endpoint: ServiceEndpoint,
        source: ObservationSource,
        now: Instant,
    ) -> Result<(), RegistryError> {
        let metadata_size = endpoint.capabilities.iter().map(|c| c.len()).sum::<usize>();
        if metadata_size > self.metadata_limit {
            return Err(RegistryError::MetadataTooLarge {
                limit: self.metadata_limit,
            });
        }

        if let Some(peer) = self.peers.get_mut(&endpoint.node_id) {
            if peer.verified
                && source != ObservationSource::ControlPlane
                && !compatible_endpoint(&peer.endpoint, &endpoint)
            {
                return Err(RegistryError::EndpointConflict(endpoint.node_id));
            }
            merge_endpoint(&mut peer.endpoint, endpoint);
            peer.lifecycle = if peer.verified {
                PeerLifecycle::Healthy
            } else {
                PeerLifecycle::Verifying
            };
            peer.last_seen = now;
            if !peer.sources.contains(&source) {
                peer.sources.push(source);
            }
            return Ok(());
        }

        if self.peers.len() >= self.max_peers {
            return Err(RegistryError::CapacityExceeded {
                limit: self.max_peers,
            });
        }
        self.peers.insert(
            endpoint.node_id,
            PeerRecord {
                node_id: endpoint.node_id,
                endpoint,
                lifecycle: PeerLifecycle::Verifying,
                verified: false,
                sources: vec![source],
                last_seen: now,
                last_verified: None,
                rejection_reason: None,
            },
        );
        Ok(())
    }

    pub fn mark_verified(
        &mut self,
        node_id: Uuid,
        endpoint: ServiceEndpoint,
        now: Instant,
    ) -> Result<(), RegistryError> {
        let peer = self
            .peers
            .get_mut(&node_id)
            .ok_or_else(|| RegistryError::Ineligible(node_id, "peer was not observed".into()))?;
        if endpoint.node_id != node_id {
            return Err(RegistryError::EndpointConflict(node_id));
        }
        merge_endpoint(&mut peer.endpoint, endpoint);
        peer.verified = true;
        peer.lifecycle = PeerLifecycle::Healthy;
        peer.last_seen = now;
        peer.last_verified = Some(now);
        if !peer.sources.contains(&ObservationSource::ControlPlane) {
            peer.sources.push(ObservationSource::ControlPlane);
        }
        Ok(())
    }

    pub fn reject(&mut self, node_id: Uuid, reason: impl Into<String>, now: Instant) {
        if let Some(peer) = self.peers.get_mut(&node_id) {
            peer.lifecycle = PeerLifecycle::Rejected;
            peer.rejection_reason = Some(reason.into());
            peer.last_seen = now;
        }
    }

    pub fn expire(&mut self, now: Instant) -> Vec<Uuid> {
        let mut expired = Vec::new();
        for peer in self.peers.values_mut() {
            if !matches!(
                peer.lifecycle,
                PeerLifecycle::Removed | PeerLifecycle::Rejected
            ) && now.saturating_duration_since(peer.last_seen) > self.stale_after
            {
                peer.lifecycle = PeerLifecycle::Stale;
                expired.push(peer.node_id);
            }
        }
        expired
    }

    pub fn remove_terminal(&mut self) -> Vec<Uuid> {
        let removed = self
            .peers
            .iter()
            .filter_map(|(id, peer)| {
                matches!(
                    peer.lifecycle,
                    PeerLifecycle::Removed | PeerLifecycle::Rejected
                )
                .then_some(*id)
            })
            .collect::<Vec<_>>();
        for id in &removed {
            self.peers.remove(id);
        }
        removed
    }

    pub fn get(&self, node_id: Uuid) -> Option<&PeerRecord> {
        self.peers.get(&node_id)
    }

    pub fn records(&self) -> impl Iterator<Item = &PeerRecord> {
        self.peers.values()
    }

    pub fn eligible_rpc_peers(&self, protocol_version: u16) -> Vec<&PeerRecord> {
        self.peers
            .values()
            .filter(|peer| {
                peer.verified
                    && peer.lifecycle == PeerLifecycle::Healthy
                    && peer.endpoint.protocol_version == protocol_version
                    && peer.endpoint.rpc_port != 0
            })
            .collect()
    }
}

fn compatible_endpoint(current: &ServiceEndpoint, observed: &ServiceEndpoint) -> bool {
    current.api_port == observed.api_port
        && current.rpc_port == observed.rpc_port
        && current.addresses.iter().any(|addr| {
            observed
                .addresses
                .iter()
                .any(|candidate| candidate.ip() == addr.ip())
        })
}

fn merge_endpoint(current: &mut ServiceEndpoint, observed: ServiceEndpoint) {
    if !observed.addresses.is_empty() {
        current.addresses = observed.addresses;
    }
    current.cluster_id = observed.cluster_id.or(current.cluster_id);
    current.protocol_version = observed.protocol_version;
    current.role = observed.role;
    current.capabilities = observed.capabilities;
    current.api_port = observed.api_port;
    current.rpc_port = observed.rpc_port;
}

impl From<&PeerRecord> for SocketAddr {
    fn from(peer: &PeerRecord) -> Self {
        peer.endpoint
            .addresses
            .first()
            .copied()
            .unwrap_or_else(|| SocketAddr::from(([0, 0, 0, 0], peer.endpoint.api_port)))
    }
}

#[allow(dead_code)]
fn _role_is_valid(role: NodeRole) -> bool {
    role.0 != 0
}

#[allow(dead_code)]
fn _backend_health_is_terminal(health: BackendHealth) -> bool {
    matches!(health, BackendHealth::Failed | BackendHealth::Stopped)
}
