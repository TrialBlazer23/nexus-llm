//! Knowledge retriever orchestrating semantic vector search and RAG prompt augmentation.

use crate::kb::embedder::Embedder;
use crate::kb::store::KnowledgeStore;
use crate::kb::vector::{cosine_similarity, lexical_overlap_score, rank_chunks, SearchResult};
use crate::kb::{DocumentChunk, EpisodicMemory, KbError};

/// High-level retriever combining the KnowledgeStore and Embedder.
#[derive(Clone)]
pub struct KnowledgeRetriever {
    store: KnowledgeStore,
    embedder: Embedder,
}

impl KnowledgeRetriever {
    pub fn new(store: KnowledgeStore, embedder: Embedder) -> Self {
        Self { store, embedder }
    }

    pub fn store(&self) -> &KnowledgeStore {
        &self.store
    }

    pub fn embedder(&self) -> &Embedder {
        &self.embedder
    }

    /// Retrieve top document chunks matching `query`.
    pub async fn retrieve_chunks(
        &self,
        query: &str,
        limit: usize,
        min_score: f32,
    ) -> Result<Vec<SearchResult<DocumentChunk>>, KbError> {
        let chunks = self.store.list_chunks()?;
        if chunks.is_empty() {
            return Ok(Vec::new());
        }

        let query_emb = self.embedder.embed(query).await.ok();

        let results = rank_chunks(query_emb.as_deref(), query, &chunks, limit, min_score);

        Ok(results)
    }

    /// Retrieve top episodic memories matching `query`.
    pub async fn retrieve_memories(
        &self,
        query: &str,
        limit: usize,
        min_score: f32,
    ) -> Result<Vec<SearchResult<EpisodicMemory>>, KbError> {
        let memories = self.store.list_memories(None)?;
        if memories.is_empty() {
            return Ok(Vec::new());
        }

        let query_emb = self.embedder.embed(query).await.ok();

        let mut results: Vec<SearchResult<EpisodicMemory>> = Vec::new();

        for mem in memories {
            let score = match (query_emb.as_deref(), &mem.embedding) {
                (Some(q), Some(m)) => {
                    let vec_score = cosine_similarity(q, m);
                    let lex_score = lexical_overlap_score(query, &mem.summary);
                    (vec_score * 0.85 + lex_score * 0.15).clamp(0.0, 1.0)
                }
                _ => lexical_overlap_score(query, &mem.summary),
            };

            if score >= min_score {
                results.push(SearchResult { item: mem, score });
            }
        }

        results.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        if results.len() > limit {
            results.truncate(limit);
        }

        Ok(results)
    }

    /// Format matching knowledge into a compact markdown context block for LLM prompts.
    pub async fn format_rag_context(
        &self,
        query: &str,
        limit: usize,
        min_score: f32,
    ) -> Result<Option<String>, KbError> {
        let chunks = self.retrieve_chunks(query, limit, min_score).await?;
        let memories = self.retrieve_memories(query, 2, min_score).await?;

        if chunks.is_empty() && memories.is_empty() {
            return Ok(None);
        }

        let mut out = String::from("### Knowledge Base Context\n");

        if !chunks.is_empty() {
            out.push_str("#### Relevant Documents:\n");
            for res in chunks {
                let doc_name = res.item.title.as_deref().unwrap_or(&res.item.document_id);
                out.push_str(&format!(
                    "- **[{}]** (relevance: {:.2})\n  {}\n",
                    doc_name,
                    res.score,
                    res.item.content.trim()
                ));
            }
        }

        if !memories.is_empty() {
            out.push_str("#### Episodic Memory:\n");
            for res in memories {
                out.push_str(&format!(
                    "- **[{}: {}]**: {}\n",
                    res.item.kind, res.item.title, res.item.summary
                ));
            }
        }

        Ok(Some(out))
    }

    /// Augment a user prompt with retrieved knowledge base context if available.
    pub async fn augment_prompt(
        &self,
        prompt: &str,
        limit: usize,
        min_score: f32,
    ) -> Result<String, KbError> {
        if let Some(context) = self.format_rag_context(prompt, limit, min_score).await? {
            Ok(format!("{context}\n---\nUser Request:\n{prompt}"))
        } else {
            Ok(prompt.to_string())
        }
    }
}
