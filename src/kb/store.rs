//! Persistent embedded structured store for the Knowledge Base backed by `redb`.

use crate::kb::{DocumentChunk, EpisodicKind, EpisodicMemory, KbError, KbStats, Persona};
use redb::{Database, ReadableDatabase, ReadableTable, TableDefinition};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};
use uuid::Uuid;

static OPEN_DATABASES: Mutex<Option<HashMap<PathBuf, Arc<Database>>>> = Mutex::new(None);

const TABLE_CHUNKS: TableDefinition<&str, &[u8]> = TableDefinition::new("chunks");
const TABLE_PERSONAS: TableDefinition<&str, &[u8]> = TableDefinition::new("personas");
const TABLE_MEMORIES: TableDefinition<&str, &[u8]> = TableDefinition::new("memories");

fn current_unix_timestamp() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

/// Thread-safe, single-file embedded Knowledge Base store.
#[derive(Clone)]
pub struct KnowledgeStore {
    path: PathBuf,
    db: Arc<Database>,
}

impl KnowledgeStore {
    /// Default file location: `$NEXUS_KB_PATH` or `~/.nexus/kb/knowledge.redb`.
    pub fn default_path() -> PathBuf {
        if let Ok(p) = std::env::var("NEXUS_KB_PATH") {
            return PathBuf::from(p);
        }
        let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
        PathBuf::from(home)
            .join(".nexus")
            .join("kb")
            .join("knowledge.redb")
    }

    /// Open or create the knowledge base at `path`.
    pub fn open<P: AsRef<Path>>(path: P) -> Result<Self, KbError> {
        let path = path.as_ref().to_path_buf();
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }

        // Return cached database if already open in this process
        {
            let mut cache_guard = OPEN_DATABASES.lock().expect("open databases lock");
            let cache = cache_guard.get_or_insert_with(HashMap::new);
            if let Some(existing_db) = cache.get(&path) {
                return Ok(Self {
                    path,
                    db: existing_db.clone(),
                });
            }
        }

        let db = Database::create(&path)
            .map_err(|e| KbError::Database(format!("Failed to open redb database: {e}")))?;

        // Ensure tables exist by running an initial write transaction
        {
            let write_txn = db
                .begin_write()
                .map_err(|e| KbError::Database(format!("Failed to begin init write txn: {e}")))?;
            let _ = write_txn
                .open_table(TABLE_CHUNKS)
                .map_err(|e| KbError::Database(format!("Failed to open chunks table: {e}")))?;
            let _ = write_txn
                .open_table(TABLE_PERSONAS)
                .map_err(|e| KbError::Database(format!("Failed to open personas table: {e}")))?;
            let _ = write_txn
                .open_table(TABLE_MEMORIES)
                .map_err(|e| KbError::Database(format!("Failed to open memories table: {e}")))?;
            write_txn
                .commit()
                .map_err(|e| KbError::Database(format!("Failed to commit init write txn: {e}")))?;
        }

        let db_arc = Arc::new(db);
        {
            let mut cache_guard = OPEN_DATABASES.lock().expect("open databases lock");
            let cache = cache_guard.get_or_insert_with(HashMap::new);
            cache.insert(path.clone(), db_arc.clone());
        }

        Ok(Self { path, db: db_arc })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Calculate content-addressed SHA-256 lowercase hex digest.
    pub fn hash_content(content: &str) -> String {
        let mut hasher = Sha256::new();
        hasher.update(content.as_bytes());
        format!("{:x}", hasher.finalize())
    }

    // --- Document Chunks ---

    /// Store a content-addressed document chunk.
    pub fn store_chunk(
        &self,
        document_id: &str,
        title: Option<&str>,
        content: &str,
        metadata: HashMap<String, String>,
        embedding: Option<Vec<f32>>,
    ) -> Result<DocumentChunk, KbError> {
        let chunk_id = Self::hash_content(content);
        let now = current_unix_timestamp();

        let chunk = DocumentChunk {
            chunk_id: chunk_id.clone(),
            document_id: document_id.to_string(),
            title: title.map(|t| t.to_string()),
            content: content.to_string(),
            metadata,
            embedding,
            created_at: now,
        };

        let bytes = serde_json::to_vec(&chunk)?;
        let write_txn = self
            .db
            .begin_write()
            .map_err(|e| KbError::Database(e.to_string()))?;
        {
            let mut table = write_txn
                .open_table(TABLE_CHUNKS)
                .map_err(|e| KbError::Database(e.to_string()))?;
            table
                .insert(chunk_id.as_str(), bytes.as_slice())
                .map_err(|e| KbError::Database(e.to_string()))?;
        }
        write_txn
            .commit()
            .map_err(|e| KbError::Database(e.to_string()))?;

        Ok(chunk)
    }

    /// Retrieve a document chunk by its SHA-256 chunk ID.
    pub fn get_chunk(&self, chunk_id: &str) -> Result<Option<DocumentChunk>, KbError> {
        let read_txn = self
            .db
            .begin_read()
            .map_err(|e| KbError::Database(e.to_string()))?;
        let table = read_txn
            .open_table(TABLE_CHUNKS)
            .map_err(|e| KbError::Database(e.to_string()))?;

        if let Some(val) = table
            .get(chunk_id)
            .map_err(|e| KbError::Database(e.to_string()))?
        {
            let chunk: DocumentChunk = serde_json::from_slice(val.value())?;
            Ok(Some(chunk))
        } else {
            Ok(None)
        }
    }

    /// Delete a document chunk by ID.
    pub fn delete_chunk(&self, chunk_id: &str) -> Result<bool, KbError> {
        let write_txn = self
            .db
            .begin_write()
            .map_err(|e| KbError::Database(e.to_string()))?;
        let mut table = write_txn
            .open_table(TABLE_CHUNKS)
            .map_err(|e| KbError::Database(e.to_string()))?;
        let removed = table
            .remove(chunk_id)
            .map_err(|e| KbError::Database(e.to_string()))?;
        let existed = removed.is_some();
        drop(removed);
        drop(table);
        write_txn
            .commit()
            .map_err(|e| KbError::Database(e.to_string()))?;
        Ok(existed)
    }

    /// List all document chunks in the store.
    pub fn list_chunks(&self) -> Result<Vec<DocumentChunk>, KbError> {
        let read_txn = self
            .db
            .begin_read()
            .map_err(|e| KbError::Database(e.to_string()))?;
        let table = read_txn
            .open_table(TABLE_CHUNKS)
            .map_err(|e| KbError::Database(e.to_string()))?;

        let mut chunks = Vec::new();
        let iter = table.iter().map_err(|e| KbError::Database(e.to_string()))?;

        for item in iter {
            let (_key, val) = item.map_err(|e| KbError::Database(e.to_string()))?;
            let chunk: DocumentChunk = serde_json::from_slice(val.value())?;
            chunks.push(chunk);
        }

        Ok(chunks)
    }

    // --- Personas ---

    /// Store or update an instruction persona, incrementing version automatically.
    pub fn store_persona(
        &self,
        id: &str,
        name: &str,
        system_prompt: &str,
        tags: Vec<String>,
        parameters: Option<serde_json::Value>,
    ) -> Result<Persona, KbError> {
        let existing = self.get_persona(id)?;
        let version = existing.map(|p| p.version + 1).unwrap_or(1);
        let now = current_unix_timestamp();

        let persona = Persona {
            id: id.to_string(),
            version,
            name: name.to_string(),
            system_prompt: system_prompt.to_string(),
            tags,
            parameters,
            updated_at: now,
        };

        let bytes = serde_json::to_vec(&persona)?;
        let write_txn = self
            .db
            .begin_write()
            .map_err(|e| KbError::Database(e.to_string()))?;
        {
            let mut table = write_txn
                .open_table(TABLE_PERSONAS)
                .map_err(|e| KbError::Database(e.to_string()))?;
            table
                .insert(id, bytes.as_slice())
                .map_err(|e| KbError::Database(e.to_string()))?;
        }
        write_txn
            .commit()
            .map_err(|e| KbError::Database(e.to_string()))?;

        Ok(persona)
    }

    /// Retrieve a persona by ID.
    pub fn get_persona(&self, id: &str) -> Result<Option<Persona>, KbError> {
        let read_txn = self
            .db
            .begin_read()
            .map_err(|e| KbError::Database(e.to_string()))?;
        let table = read_txn
            .open_table(TABLE_PERSONAS)
            .map_err(|e| KbError::Database(e.to_string()))?;

        if let Some(val) = table
            .get(id)
            .map_err(|e| KbError::Database(e.to_string()))?
        {
            let persona: Persona = serde_json::from_slice(val.value())?;
            Ok(Some(persona))
        } else {
            Ok(None)
        }
    }

    /// List all stored personas.
    pub fn list_personas(&self) -> Result<Vec<Persona>, KbError> {
        let read_txn = self
            .db
            .begin_read()
            .map_err(|e| KbError::Database(e.to_string()))?;
        let table = read_txn
            .open_table(TABLE_PERSONAS)
            .map_err(|e| KbError::Database(e.to_string()))?;

        let mut personas = Vec::new();
        let iter = table.iter().map_err(|e| KbError::Database(e.to_string()))?;

        for item in iter {
            let (_key, val) = item.map_err(|e| KbError::Database(e.to_string()))?;
            let persona: Persona = serde_json::from_slice(val.value())?;
            personas.push(persona);
        }

        Ok(personas)
    }

    // --- Episodic Memory ---

    /// Store a new episodic memory record.
    #[allow(clippy::too_many_arguments)]
    pub fn store_memory(
        &self,
        session_id: Option<&str>,
        kind: EpisodicKind,
        title: &str,
        summary: &str,
        details: Option<&str>,
        tags: Vec<String>,
        embedding: Option<Vec<f32>>,
    ) -> Result<EpisodicMemory, KbError> {
        let id = Uuid::new_v4();
        let now = current_unix_timestamp();

        let memory = EpisodicMemory {
            id,
            session_id: session_id.map(|s| s.to_string()),
            kind,
            title: title.to_string(),
            summary: summary.to_string(),
            details: details.map(|d| d.to_string()),
            tags,
            embedding,
            timestamp: now,
        };

        let key_str = id.to_string();
        let bytes = serde_json::to_vec(&memory)?;
        let write_txn = self
            .db
            .begin_write()
            .map_err(|e| KbError::Database(e.to_string()))?;
        {
            let mut table = write_txn
                .open_table(TABLE_MEMORIES)
                .map_err(|e| KbError::Database(e.to_string()))?;
            table
                .insert(key_str.as_str(), bytes.as_slice())
                .map_err(|e| KbError::Database(e.to_string()))?;
        }
        write_txn
            .commit()
            .map_err(|e| KbError::Database(e.to_string()))?;

        Ok(memory)
    }

    /// Retrieve an episodic memory by Uuid.
    pub fn get_memory(&self, id: Uuid) -> Result<Option<EpisodicMemory>, KbError> {
        let key_str = id.to_string();
        let read_txn = self
            .db
            .begin_read()
            .map_err(|e| KbError::Database(e.to_string()))?;
        let table = read_txn
            .open_table(TABLE_MEMORIES)
            .map_err(|e| KbError::Database(e.to_string()))?;

        if let Some(val) = table
            .get(key_str.as_str())
            .map_err(|e| KbError::Database(e.to_string()))?
        {
            let memory: EpisodicMemory = serde_json::from_slice(val.value())?;
            Ok(Some(memory))
        } else {
            Ok(None)
        }
    }

    /// List episodic memories, optionally filtered by kind.
    pub fn list_memories(
        &self,
        filter_kind: Option<EpisodicKind>,
    ) -> Result<Vec<EpisodicMemory>, KbError> {
        let read_txn = self
            .db
            .begin_read()
            .map_err(|e| KbError::Database(e.to_string()))?;
        let table = read_txn
            .open_table(TABLE_MEMORIES)
            .map_err(|e| KbError::Database(e.to_string()))?;

        let mut memories = Vec::new();
        let iter = table.iter().map_err(|e| KbError::Database(e.to_string()))?;

        for item in iter {
            let (_key, val) = item.map_err(|e| KbError::Database(e.to_string()))?;
            let memory: EpisodicMemory = serde_json::from_slice(val.value())?;
            if let Some(kind) = filter_kind {
                if memory.kind == kind {
                    memories.push(memory);
                }
            } else {
                memories.push(memory);
            }
        }

        Ok(memories)
    }

    // --- Statistics ---

    /// Compute statistics of the knowledge base.
    pub fn stats(&self) -> Result<KbStats, KbError> {
        let read_txn = self
            .db
            .begin_read()
            .map_err(|e| KbError::Database(e.to_string()))?;

        let chunks_table = read_txn
            .open_table(TABLE_CHUNKS)
            .map_err(|e| KbError::Database(e.to_string()))?;
        let personas_table = read_txn
            .open_table(TABLE_PERSONAS)
            .map_err(|e| KbError::Database(e.to_string()))?;
        let memories_table = read_txn
            .open_table(TABLE_MEMORIES)
            .map_err(|e| KbError::Database(e.to_string()))?;

        let total_personas = personas_table
            .iter()
            .map_err(|e| KbError::Database(e.to_string()))?
            .count();
        let total_memories = memories_table
            .iter()
            .map_err(|e| KbError::Database(e.to_string()))?
            .count();

        let mut total_chunks = 0;
        let mut chunks_with_embeddings = 0;

        let iter = chunks_table
            .iter()
            .map_err(|e| KbError::Database(e.to_string()))?;
        for item in iter {
            let (_key, val) = item.map_err(|e| KbError::Database(e.to_string()))?;
            total_chunks += 1;
            if let Ok(chunk) = serde_json::from_slice::<DocumentChunk>(val.value()) {
                if chunk.embedding.is_some() {
                    chunks_with_embeddings += 1;
                }
            }
        }

        Ok(KbStats {
            total_chunks,
            total_personas,
            total_memories,
            chunks_with_embeddings,
        })
    }

    // --- Gossip Sync Raw Operations ---

    /// Insert a raw document chunk received during sync.
    ///
    /// Content-addressing ensures immutability: returns `Ok(false)` if already present,
    /// or `Ok(true)` if newly inserted.
    pub fn insert_raw_chunk(&self, chunk: &DocumentChunk) -> Result<bool, KbError> {
        let read_txn = self
            .db
            .begin_read()
            .map_err(|e| KbError::Database(e.to_string()))?;
        let table = read_txn
            .open_table(TABLE_CHUNKS)
            .map_err(|e| KbError::Database(e.to_string()))?;

        if table
            .get(chunk.chunk_id.as_str())
            .map_err(|e| KbError::Database(e.to_string()))?
            .is_some()
        {
            return Ok(false);
        }
        drop(table);
        drop(read_txn);

        let bytes = serde_json::to_vec(chunk)?;
        let write_txn = self
            .db
            .begin_write()
            .map_err(|e| KbError::Database(e.to_string()))?;
        {
            let mut table = write_txn
                .open_table(TABLE_CHUNKS)
                .map_err(|e| KbError::Database(e.to_string()))?;
            table
                .insert(chunk.chunk_id.as_str(), bytes.as_slice())
                .map_err(|e| KbError::Database(e.to_string()))?;
        }
        write_txn
            .commit()
            .map_err(|e| KbError::Database(e.to_string()))?;

        Ok(true)
    }

    /// Upsert a versioned persona using last-writer-wins version resolution.
    ///
    /// Overwrites if incoming version is higher, or if version is equal and incoming
    /// `updated_at` is greater. Returns `Ok(true)` if written, `Ok(false)` if rejected.
    pub fn upsert_raw_persona(&self, persona: &Persona) -> Result<bool, KbError> {
        if let Some(existing) = self.get_persona(&persona.id)? {
            if persona.version < existing.version
                || (persona.version == existing.version
                    && persona.updated_at <= existing.updated_at)
            {
                return Ok(false);
            }
        }

        let bytes = serde_json::to_vec(persona)?;
        let write_txn = self
            .db
            .begin_write()
            .map_err(|e| KbError::Database(e.to_string()))?;
        {
            let mut table = write_txn
                .open_table(TABLE_PERSONAS)
                .map_err(|e| KbError::Database(e.to_string()))?;
            table
                .insert(persona.id.as_str(), bytes.as_slice())
                .map_err(|e| KbError::Database(e.to_string()))?;
        }
        write_txn
            .commit()
            .map_err(|e| KbError::Database(e.to_string()))?;

        Ok(true)
    }

    /// Upsert an episodic memory using timestamp-based resolution.
    ///
    /// Overwrites if incoming `timestamp` is greater than or equal to existing record.
    /// Returns `Ok(true)` if written, `Ok(false)` if rejected.
    pub fn upsert_raw_memory(&self, memory: &EpisodicMemory) -> Result<bool, KbError> {
        let key_str = memory.id.to_string();
        if let Some(existing) = self.get_memory(memory.id)? {
            if memory.timestamp <= existing.timestamp {
                return Ok(false);
            }
        }

        let bytes = serde_json::to_vec(memory)?;
        let write_txn = self
            .db
            .begin_write()
            .map_err(|e| KbError::Database(e.to_string()))?;
        {
            let mut table = write_txn
                .open_table(TABLE_MEMORIES)
                .map_err(|e| KbError::Database(e.to_string()))?;
            table
                .insert(key_str.as_str(), bytes.as_slice())
                .map_err(|e| KbError::Database(e.to_string()))?;
        }
        write_txn
            .commit()
            .map_err(|e| KbError::Database(e.to_string()))?;

        Ok(true)
    }
}
