//! LAN Gossip Protocol for synchronizing Knowledge Base across mesh nodes.
//!
//! Provides lightweight manifest exchange, differential calculation, and
//! last-writer-wins conflict resolution over authenticated control-plane channels.

use crate::control_plane::{ControlPlaneError, CONTROL_PLANE_VERSION};
use crate::kb::{DocumentChunk, EpisodicMemory, KbError, KnowledgeStore, Persona};
use crate::node_identity::NodeIdentity;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use uuid::Uuid;

/// Lightweight chunk entry in a Knowledge Base manifest.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ChunkManifestItem {
    pub chunk_id: String,
    pub created_at: u64,
}

/// Lightweight persona entry in a Knowledge Base manifest.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PersonaManifestItem {
    pub id: String,
    pub version: u32,
    pub updated_at: u64,
}

/// Lightweight episodic memory entry in a Knowledge Base manifest.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct MemoryManifestItem {
    pub id: Uuid,
    pub timestamp: u64,
}

/// Manifest snapshot summarizing a node's Knowledge Base contents.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct KbSyncManifest {
    pub protocol_version: u16,
    pub node_id: Uuid,
    pub chunks: Vec<ChunkManifestItem>,
    pub personas: Vec<PersonaManifestItem>,
    pub memories: Vec<MemoryManifestItem>,
}

/// Differential calculated between a local and remote manifest.
///
/// Contains IDs of items that the remote peer possesses which are either
/// missing locally or newer than local versions.
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
pub struct KbSyncDiff {
    pub missing_chunk_ids: Vec<String>,
    pub missing_or_stale_persona_ids: Vec<String>,
    pub missing_or_stale_memory_ids: Vec<Uuid>,
}

impl KbSyncDiff {
    pub fn is_empty(&self) -> bool {
        self.missing_chunk_ids.is_empty()
            && self.missing_or_stale_persona_ids.is_empty()
            && self.missing_or_stale_memory_ids.is_empty()
    }
}

/// Request to pull specific items by ID from a remote node.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct KbSyncPullRequest {
    pub protocol_version: u16,
    pub requester_id: Uuid,
    #[serde(default)]
    pub chunk_ids: Vec<String>,
    #[serde(default)]
    pub persona_ids: Vec<String>,
    #[serde(default)]
    pub memory_ids: Vec<Uuid>,
}

/// Response containing full items requested by a pull request.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct KbSyncPullResponse {
    pub protocol_version: u16,
    pub chunks: Vec<DocumentChunk>,
    pub personas: Vec<Persona>,
    pub memories: Vec<EpisodicMemory>,
}

/// Request to push a batch of items to a remote node.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct KbSyncPushRequest {
    pub protocol_version: u16,
    pub requester_id: Uuid,
    #[serde(default)]
    pub chunks: Vec<DocumentChunk>,
    #[serde(default)]
    pub personas: Vec<Persona>,
    #[serde(default)]
    pub memories: Vec<EpisodicMemory>,
}

/// Response acknowledging a batch push.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct KbSyncPushResponse {
    pub protocol_version: u16,
    pub success: bool,
    pub accepted_chunks: usize,
    pub accepted_personas: usize,
    pub accepted_memories: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error_message: Option<String>,
}

/// Summary statistics of a completed synchronization exchange.
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
pub struct SyncReport {
    pub peer_node_id: Uuid,
    pub chunks_pulled: usize,
    pub personas_pulled: usize,
    pub memories_pulled: usize,
    pub chunks_pushed: usize,
    pub personas_pushed: usize,
    pub memories_pushed: usize,
}

/// Generate a lightweight manifest from a KnowledgeStore.
pub fn generate_manifest(store: &KnowledgeStore, node_id: Uuid) -> Result<KbSyncManifest, KbError> {
    let chunks = store
        .list_chunks()?
        .into_iter()
        .map(|c| ChunkManifestItem {
            chunk_id: c.chunk_id,
            created_at: c.created_at,
        })
        .collect();

    let personas = store
        .list_personas()?
        .into_iter()
        .map(|p| PersonaManifestItem {
            id: p.id,
            version: p.version,
            updated_at: p.updated_at,
        })
        .collect();

    let memories = store
        .list_memories(None)?
        .into_iter()
        .map(|m| MemoryManifestItem {
            id: m.id,
            timestamp: m.timestamp,
        })
        .collect();

    Ok(KbSyncManifest {
        protocol_version: CONTROL_PLANE_VERSION,
        node_id,
        chunks,
        personas,
        memories,
    })
}

/// Compute what items in `remote_manifest` should be pulled into local store.
pub fn compute_diff(
    local_manifest: &KbSyncManifest,
    remote_manifest: &KbSyncManifest,
) -> KbSyncDiff {
    let mut local_chunks: HashMap<&str, u64> = HashMap::new();
    for c in &local_manifest.chunks {
        local_chunks.insert(&c.chunk_id, c.created_at);
    }

    let mut local_personas: HashMap<&str, (u32, u64)> = HashMap::new();
    for p in &local_manifest.personas {
        local_personas.insert(&p.id, (p.version, p.updated_at));
    }

    let mut local_memories: HashMap<Uuid, u64> = HashMap::new();
    for m in &local_manifest.memories {
        local_memories.insert(m.id, m.timestamp);
    }

    let mut missing_chunk_ids = Vec::new();
    for remote_chunk in &remote_manifest.chunks {
        if !local_chunks.contains_key(remote_chunk.chunk_id.as_str()) {
            missing_chunk_ids.push(remote_chunk.chunk_id.clone());
        }
    }

    let mut missing_or_stale_persona_ids = Vec::new();
    for remote_persona in &remote_manifest.personas {
        match local_personas.get(remote_persona.id.as_str()) {
            None => missing_or_stale_persona_ids.push(remote_persona.id.clone()),
            Some(&(local_ver, local_ts)) => {
                if remote_persona.version > local_ver
                    || (remote_persona.version == local_ver && remote_persona.updated_at > local_ts)
                {
                    missing_or_stale_persona_ids.push(remote_persona.id.clone());
                }
            }
        }
    }

    let mut missing_or_stale_memory_ids = Vec::new();
    for remote_memory in &remote_manifest.memories {
        match local_memories.get(&remote_memory.id) {
            None => missing_or_stale_memory_ids.push(remote_memory.id),
            Some(&local_ts) => {
                if remote_memory.timestamp > local_ts {
                    missing_or_stale_memory_ids.push(remote_memory.id);
                }
            }
        }
    }

    KbSyncDiff {
        missing_chunk_ids,
        missing_or_stale_persona_ids,
        missing_or_stale_memory_ids,
    }
}

/// Package requested items from local store to satisfy an inbound pull request.
pub fn apply_pull(
    store: &KnowledgeStore,
    request: &KbSyncPullRequest,
) -> Result<KbSyncPullResponse, KbError> {
    let mut chunks = Vec::new();
    for cid in &request.chunk_ids {
        if let Some(c) = store.get_chunk(cid)? {
            chunks.push(c);
        }
    }

    let mut personas = Vec::new();
    for pid in &request.persona_ids {
        if let Some(p) = store.get_persona(pid)? {
            personas.push(p);
        }
    }

    let mut memories = Vec::new();
    for mid in &request.memory_ids {
        if let Some(m) = store.get_memory(*mid)? {
            memories.push(m);
        }
    }

    Ok(KbSyncPullResponse {
        protocol_version: CONTROL_PLANE_VERSION,
        chunks,
        personas,
        memories,
    })
}

/// Ingest an incoming batch push, upserting items into local store.
pub fn apply_push(
    store: &KnowledgeStore,
    request: &KbSyncPushRequest,
) -> Result<KbSyncPushResponse, KbError> {
    let mut accepted_chunks = 0;
    for chunk in &request.chunks {
        if store.insert_raw_chunk(chunk)? {
            accepted_chunks += 1;
        }
    }

    let mut accepted_personas = 0;
    for persona in &request.personas {
        if store.upsert_raw_persona(persona)? {
            accepted_personas += 1;
        }
    }

    let mut accepted_memories = 0;
    for memory in &request.memories {
        if store.upsert_raw_memory(memory)? {
            accepted_memories += 1;
        }
    }

    Ok(KbSyncPushResponse {
        protocol_version: CONTROL_PLANE_VERSION,
        success: true,
        accepted_chunks,
        accepted_personas,
        accepted_memories,
        error_message: None,
    })
}

/// Perform a complete bidirectional synchronization cycle with a remote peer node.
pub async fn sync_with_peer(
    client: &reqwest::Client,
    peer_control_url: &str,
    store: &KnowledgeStore,
    identity: &NodeIdentity,
    local_node_id: Uuid,
) -> Result<SyncReport, ControlPlaneError> {
    // 1. Fetch remote manifest
    let manifest_req = crate::control_plane::KbManifestRequest {
        protocol_version: CONTROL_PLANE_VERSION,
        requester_id: local_node_id,
    };
    let remote_manifest_resp = crate::control_plane::dispatch_kb_manifest_signed(
        client,
        peer_control_url,
        &manifest_req,
        identity,
    )
    .await?;

    let remote_manifest = remote_manifest_resp.manifest;
    let local_manifest = generate_manifest(store, local_node_id)
        .map_err(|e| ControlPlaneError::InvalidResponse(e.to_string()))?;

    // 2. Compute diff: what we need from remote
    let pull_diff = compute_diff(&local_manifest, &remote_manifest);
    let mut chunks_pulled = 0;
    let mut personas_pulled = 0;
    let mut memories_pulled = 0;

    if !pull_diff.is_empty() {
        let pull_req = KbSyncPullRequest {
            protocol_version: CONTROL_PLANE_VERSION,
            requester_id: local_node_id,
            chunk_ids: pull_diff.missing_chunk_ids,
            persona_ids: pull_diff.missing_or_stale_persona_ids,
            memory_ids: pull_diff.missing_or_stale_memory_ids,
        };
        let pulled = crate::control_plane::dispatch_kb_pull_signed(
            client,
            peer_control_url,
            &pull_req,
            identity,
        )
        .await?;

        let push_req = KbSyncPushRequest {
            protocol_version: CONTROL_PLANE_VERSION,
            requester_id: local_node_id,
            chunks: pulled.chunks,
            personas: pulled.personas,
            memories: pulled.memories,
        };
        let ingest_resp = apply_push(store, &push_req)
            .map_err(|e| ControlPlaneError::InvalidResponse(e.to_string()))?;

        chunks_pulled = ingest_resp.accepted_chunks;
        personas_pulled = ingest_resp.accepted_personas;
        memories_pulled = ingest_resp.accepted_memories;
    }

    // 3. Compute reverse diff: what remote needs from us
    let push_diff = compute_diff(&remote_manifest, &local_manifest);
    let mut chunks_pushed = 0;
    let mut personas_pushed = 0;
    let mut memories_pushed = 0;

    if !push_diff.is_empty() {
        let local_pull_req = KbSyncPullRequest {
            protocol_version: CONTROL_PLANE_VERSION,
            requester_id: local_node_id,
            chunk_ids: push_diff.missing_chunk_ids,
            persona_ids: push_diff.missing_or_stale_persona_ids,
            memory_ids: push_diff.missing_or_stale_memory_ids,
        };
        let items_to_push = apply_pull(store, &local_pull_req)
            .map_err(|e| ControlPlaneError::InvalidResponse(e.to_string()))?;

        let outbound_push = KbSyncPushRequest {
            protocol_version: CONTROL_PLANE_VERSION,
            requester_id: local_node_id,
            chunks: items_to_push.chunks,
            personas: items_to_push.personas,
            memories: items_to_push.memories,
        };
        let push_resp = crate::control_plane::dispatch_kb_push_signed(
            client,
            peer_control_url,
            &outbound_push,
            identity,
        )
        .await?;

        chunks_pushed = push_resp.accepted_chunks;
        personas_pushed = push_resp.accepted_personas;
        memories_pushed = push_resp.accepted_memories;
    }

    Ok(SyncReport {
        peer_node_id: remote_manifest.node_id,
        chunks_pulled,
        personas_pulled,
        memories_pulled,
        chunks_pushed,
        personas_pushed,
        memories_pushed,
    })
}
