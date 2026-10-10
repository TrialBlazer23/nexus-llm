//! Background peer-registry maintenance and control-plane verification.

use crate::adb_links::{forward_argv, parse_adb_forward_list, AdbLinkCache};
use crate::control_plane::{
    fetch_state, fetch_state_signed, validate_state, ControlPlaneRequest, CONTROL_PLANE_VERSION,
};
use crate::discovery::{usb_link_healthy, DiscoveryService, ServiceEndpoint, UsbProbeHit};
use crate::node_identity::NodeIdentity;
use crate::peer_registry::{ObservationSource, PeerLifecycle, RegistryError};
use crate::tunnel::{
    install_control_reverse, read_forward_list, run_adb_command, AdbTunnelSupervisor, TunnelError,
};
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
                    probe_usb_peers(&discovery, &identity, requester_id, false).await;
                }
                _ = verify_tick.tick() => {
                    run_verifier(&discovery, &client, &identity, requester_id).await;
                }
            }
        }
    })
}

/// Probe authorized serials and merge the ones Nexus answers.
///
/// `force` is the Tunnel tab. The background tick is skipped when the last probe
/// was under 5 seconds ago. A missing `adb` returns before any peer is changed.
pub async fn probe_usb_peers(
    discovery: &DiscoveryService,
    identity: &NodeIdentity,
    requester_id: Uuid,
    force: bool,
) -> Vec<crate::adb_links::HostForward> {
    let Some(_guard) = discovery.usb_probe_permit(force).await else {
        return Vec::new();
    };
    if !AdbTunnelSupervisor::is_adb_available() {
        return Vec::new();
    }
    let devices = match AdbTunnelSupervisor::list_devices() {
        Ok(devices) => devices,
        Err(TunnelError::AdbNotFound) => return Vec::new(),
        Err(err) => {
            warn!("ADB device list failed: {err}");
            return Vec::new();
        }
    };
    let cfg = discovery.config();
    let (forwards, skipped) = crate::adb_links::allocate_forwards(
        &devices,
        cfg.cluster.adb.host_port_base,
        cfg.cluster.adb.max_devices,
    );
    if !skipped.is_empty() {
        warn!(
            "ADB max_devices cap skipped serials: {}",
            skipped.join(", ")
        );
    }
    let present_serials: Vec<String> = devices.iter().map(|device| device.serial.clone()).collect();
    let mut cache = AdbLinkCache::load(&AdbLinkCache::default_path()).unwrap_or_else(|err| {
        warn!("ADB link cache unavailable, probing default ports: {err}");
        AdbLinkCache::default()
    });
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(2))
        .build()
        .unwrap_or_else(|_| reqwest::Client::new());
    let pairing = cfg.network.security.pairing_enforced();
    let local_rpc = cfg.cluster.rpc_port;
    let mut hits = Vec::new();
    for forward in &forwards {
        let attempts = crate::discovery::device_port_attempts(
            cache.ports_for(&forward.serial),
            cfg.network.api_port,
            cfg.network.control_port,
            local_rpc,
        );
        let mut adb_gone = false;
        for (device_api, device_control, device_rpc) in attempts {
            let api_args = forward_argv(&forward.serial, forward.host_api, device_api);
            let control_args = forward_argv(&forward.serial, forward.host_control, device_control);
            if !adb_step(&api_args, &forward.serial, &mut adb_gone) {
                if adb_gone {
                    break;
                }
                continue;
            }
            if !adb_step(&control_args, &forward.serial, &mut adb_gone) {
                if adb_gone {
                    break;
                }
                continue;
            }
            if let Err(err) = install_control_reverse(&forward.serial, device_rpc, local_rpc) {
                warn!(
                    "ADB RPC reverse for {} did not start: {err}",
                    forward.serial
                );
                if matches!(err, TunnelError::AdbNotFound) {
                    adb_gone = true;
                    break;
                }
            }
            let base = format!("http://127.0.0.1:{}", forward.host_control);
            let request = ControlPlaneRequest {
                protocol_version: CONTROL_PLANE_VERSION,
                requester_id,
            };
            let fetched = if pairing {
                fetch_state_signed(&client, &base, &request, identity, requester_id).await
            } else {
                fetch_state(&client, &base, &request).await
            };
            match fetched {
                Ok(state) => {
                    hits.push(UsbProbeHit {
                        serial: forward.serial.clone(),
                        host_api: forward.host_api,
                        host_control: forward.host_control,
                        device_api,
                        device_control,
                        device_rpc,
                        state,
                    });
                    break;
                }
                Err(err) => {
                    warn!(
                        "USB control probe failed for {} on {}: {err}",
                        forward.serial, base
                    );
                }
            }
        }
        if adb_gone {
            return Vec::new();
        }
    }

    let listed = match read_forward_list() {
        Ok(text) => parse_adb_forward_list(&text),
        Err(TunnelError::AdbNotFound) => return Vec::new(),
        Err(err) => {
            warn!("ADB forward --list failed: {err}");
            Vec::new()
        }
    };
    let fold = discovery
        .apply_folded_usb_probes(&mut cache, &hits, &present_serials, &listed)
        .await;
    if let Err(err) = cache.save(&AdbLinkCache::default_path()) {
        warn!("ADB link cache was not saved: {err}");
    }
    for argv in &fold.teardown {
        if !argv.iter().any(|arg| arg == "-s") {
            warn!(
                "refusing adb forward --remove without -s: {}",
                argv.join(" ")
            );
            continue;
        }
        if let Err(err) = run_adb_command(argv) {
            warn!("ADB forward --remove failed: {err}");
            if matches!(err, TunnelError::AdbNotFound) {
                break;
            }
        }
    }
    publish_usb_winners(discovery, &fold.winner_serials).await;
    forwards
        .into_iter()
        .filter(|forward| {
            fold.winner_serials
                .iter()
                .any(|serial| serial == &forward.serial)
        })
        .collect()
}

fn adb_step(args: &[String], serial: &str, adb_gone: &mut bool) -> bool {
    match run_adb_command(args) {
        Err(TunnelError::AdbNotFound) => {
            *adb_gone = true;
            false
        }
        Err(err) => {
            warn!("ADB forward could not start for {serial}: {err}");
            false
        }
        Ok(ok) => ok,
    }
}

async fn publish_usb_winners(discovery: &DiscoveryService, winner_serials: &[String]) {
    let endpoints: Vec<(Uuid, ServiceEndpoint)> = {
        let peers = discovery.peers();
        let peers = peers.read().await;
        winner_serials
            .iter()
            .filter_map(|serial| {
                peers
                    .values()
                    .find(|peer| peer.usb_serial.as_deref() == Some(serial.as_str()))
            })
            .map(|peer| (peer.uuid, peer.service_endpoint()))
            .collect()
    };
    let now = Instant::now();
    let registry = discovery.peer_registry();
    let mut registry = registry.write().await;
    for (node_id, endpoint) in endpoints {
        match registry.observe(endpoint.clone(), ObservationSource::ControlPlane, now) {
            Ok(()) => {
                if let Err(err) = registry.mark_verified(node_id, endpoint, now) {
                    if matches!(err, RegistryError::EndpointConflict(_)) {
                        registry.keep_fresh(node_id, now);
                    } else {
                        warn!("USB mark_verified failed for {node_id}: {err}");
                    }
                }
            }
            Err(RegistryError::EndpointConflict(_)) => {
                registry.keep_fresh(node_id, now);
            }
            Err(err) => warn!("USB registry observe failed for {node_id}: {err}"),
        }
    }
}

async fn run_reaper(discovery: &DiscoveryService) {
    keep_usb_peers_fresh(discovery).await;
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

async fn keep_usb_peers_fresh(discovery: &DiscoveryService) {
    let now = Instant::now();
    let ids: Vec<Uuid> = {
        let peers = discovery.peers();
        let peers = peers.read().await;
        peers
            .iter()
            .filter_map(|(id, peer)| usb_link_healthy(peer).then_some(*id))
            .collect()
    };
    if ids.is_empty() {
        return;
    }
    let registry = discovery.peer_registry();
    let mut registry = registry.write().await;
    for id in ids {
        registry.keep_fresh(id, now);
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

        let control_base = {
            let peers = discovery.peers();
            let peers = peers.read().await;
            peers
                .get(&node_id)
                .map(|peer| peer.control_endpoint())
                .or_else(|| control_base_url(&endpoint))
        };
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

        if let Err(err) = discovery.apply_verified_state(&state, &endpoint).await {
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
