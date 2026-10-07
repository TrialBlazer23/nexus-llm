//! Embedded Knowledge Base and Retrieval-Augmented Generation (RAG) engine.
//!
//! Provides content-addressed document chunks, versioned instruction personas,
//! distilled episodic memory, in-process cosine vector search, and distributed
//! embeddings integration.

pub mod embedder;
pub mod janitor;
pub mod retriever;
pub mod store;
pub mod sync;
pub mod vector;

pub use embedder::{Embedder, FastPseudoEmbedder, RemoteEmbedder};
pub use janitor::JanitorAgent;
pub use retriever::KnowledgeRetriever;
pub use store::KnowledgeStore;
pub use sync::{
    apply_pull, apply_push, compute_diff, generate_manifest, sync_with_peer, ChunkManifestItem,
    KbSyncDiff, KbSyncManifest, KbSyncPullRequest, KbSyncPullResponse, KbSyncPushRequest,
    KbSyncPushResponse, MemoryManifestItem, PersonaManifestItem, SyncReport,
};
pub use vector::{cosine_similarity, SearchResult};

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use thiserror::Error;
use uuid::Uuid;

#[derive(Error, Debug)]
pub enum KbError {
    #[error("I/O error in knowledge base: {0}")]
    Io(#[from] std::io::Error),

    #[error("Database error in knowledge base: {0}")]
    Database(String),

    #[error("Serialization error in knowledge base: {0}")]
    Json(#[from] serde_json::Error),

    #[error("Embedding failure: {0}")]
    Embedding(String),

    #[error("Item not found: {0}")]
    NotFound(String),
}

/// Content-addressed document chunk stored in the knowledge base.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DocumentChunk {
    /// Content-addressed SHA-256 lowercase hex digest of `content`.
    pub chunk_id: String,
    /// Logical parent document identifier (e.g. URI, file path, manual title).
    pub document_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    pub content: String,
    #[serde(default)]
    pub metadata: HashMap<String, String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub embedding: Option<Vec<f32>>,
    pub created_at: u64,
}

/// Versioned instruction persona for guiding model behavior.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Persona {
    pub id: String,
    pub version: u32,
    pub name: String,
    pub system_prompt: String,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parameters: Option<serde_json::Value>,
    pub updated_at: u64,
}

/// Kind of distilled episodic memory.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum EpisodicKind {
    Fact,
    Summary,
    Preference,
    Entity,
}

impl std::fmt::Display for EpisodicKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Fact => write!(f, "fact"),
            Self::Summary => write!(f, "summary"),
            Self::Preference => write!(f, "preference"),
            Self::Entity => write!(f, "entity"),
        }
    }
}

/// Distilled episodic memory record (logs, facts, user preferences, entities).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct EpisodicMemory {
    pub id: Uuid,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    pub kind: EpisodicKind,
    pub title: String,
    pub summary: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub details: Option<String>,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub embedding: Option<Vec<f32>>,
    pub timestamp: u64,
}

/// Summary statistics for the Knowledge Base.
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
pub struct KbStats {
    pub total_chunks: usize,
    pub total_personas: usize,
    pub total_memories: usize,
    pub chunks_with_embeddings: usize,
}
