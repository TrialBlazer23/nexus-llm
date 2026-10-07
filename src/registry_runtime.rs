//! Background peer-registry maintenance and control-plane verification.

use crate::control_plane::{
    endpoint_from_state, fetch_state, fetch_state_signed, validate_state, ControlPlaneRequest,
    CONTROL_PLANE_VERSION,
};
use crate::discovery::{DiscoveryService, ServiceEndpoint};
use crate::node_identity::NodeIdentity;
use crate::peer_registry::PeerLifecycle;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tracing::{debug, warn};
use uuid::Uuid;

const REAPER_INTERVAL: Duration = Duration::from_secs(2);
const VERIFY_INTERVAL: Duration = Duration::from_secs(3);

pub fn spawn_registry_runtime(
    discovery: Arc<DiscoveryService>,
    identity: Arc<NodeIdentity>,
    requester_id: Uuid,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(3))
            .build()
            .expect("reqwest client");
        let mut reaper_tick = tokio::time::interval(REAPER_INTERVAL);
        let mut verify_tick = tokio::time::interval(VERIFY_INTERVAL);
        loop {
            tokio::select! {
                _ = reaper_tick.tick() => {
                    run_reaper(&discovery).await;
                }
                _ = verify_tick.tick() => {
                    run_verifier(&discovery, &client, &identity, requester_id).await;
                }
            }
        }
    })
}

async fn run_reaper(discovery: &DiscoveryService) {
    let now = Instant::now();
    let registry_arc = discovery.peer_registry();
    let expired = {
        let mut registry = registry_arc.write().await;
        let expired = registry.expire(now);
        let removed = registry.remove_terminal();
        expired.into_iter().chain(removed).collect::<Vec<_>>()
    };
    for node_id in expired {
        discovery.remove_peer(node_id).await;
        debug!("Registry reaper removed peer {}", node_id);
    }
}

async fn run_verifier(
    discovery: &DiscoveryService,
    client: &reqwest::Client,
    identity: &NodeIdentity,
    requester_id: Uuid,
) {
    let security = discovery.config().network.security;

    let registry_arc = discovery.peer_registry();
    let peers_to_verify: Vec<(Uuid, ServiceEndpoint)> = {
        let registry = registry_arc.read().await;
        registry
            .records()
            .filter(|record| {
                matches!(
                    record.lifecycle,
                    PeerLifecycle::Verifying | PeerLifecycle::Discovered
                ) || !record.verified
            })
            .map(|record| (record.node_id, record.endpoint.clone()))
            .collect()
    };

    for (node_id, endpoint) in peers_to_verify {
        if security.pairing_enforced() && !security.allowed_peer_ids.contains(&node_id) {
            discovery.peer_registry().write().await.reject(
                node_id,
                "unpaired peer",
                Instant::now(),
            );
            continue;
        }

        let control_base = control_base_url(&endpoint);
        let Some(base) = control_base else {
            continue;
        };

        let request = ControlPlaneRequest {
            protocol_version: CONTROL_PLANE_VERSION,
            requester_id,
        };

        let state = if security.pairing_enforced() {
            match fetch_state_signed(client, &base, &request, identity, requester_id).await {
                Ok(state) => state,
                Err(err) => {
                    warn!("Control-plane verify failed for {}: {}", node_id, err);
                    discovery.peer_registry().write().await.reject(
                        node_id,
                        format!("verify failed: {err}"),
                        Instant::now(),
                    );
                    continue;
                }
            }
        } else {
            match fetch_state(client, &base, &request).await {
                Ok(state) => state,
                Err(err) => {
                    warn!("Control-plane verify failed for {}: {}", node_id, err);
                    discovery.peer_registry().write().await.reject(
                        node_id,
                        format!("verify failed: {err}"),
                        Instant::now(),
                    );
                    continue;
                }
            }
        };

        let max_alloc = discovery.config().cluster.max_rpc_ram_mb.max(16_384);
        if let Err(err) = validate_state(&state, node_id, CONTROL_PLANE_VERSION, max_alloc) {
            discovery.peer_registry().write().await.reject(
                node_id,
                err.to_string(),
                Instant::now(),
            );
            continue;
        }

        let verified_endpoint = endpoint_from_state(&state, &endpoint);
        if let Err(err) = discovery.peer_registry().write().await.mark_verified(
            node_id,
            verified_endpoint,
            Instant::now(),
        ) {
            warn!("Registry mark_verified failed for {}: {}", node_id, err);
        } else {
            debug!("Peer {} verified via control plane", node_id);
        }
    }
}

fn control_base_url(endpoint: &ServiceEndpoint) -> Option<String> {
    let addr = endpoint.addresses.first()?;
    let port = if endpoint.control_port == 0 {
        9998
    } else {
        endpoint.control_port
    };
    Some(format!("http://{}:{}", addr.ip(), port))
}
