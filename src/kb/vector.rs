//! In-process vector similarity and hybrid search scoring.

use crate::kb::DocumentChunk;
use serde::{Deserialize, Serialize};

/// Search result holding a matched item and its similarity score [0.0..1.0].
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SearchResult<T> {
    pub item: T,
    pub score: f32,
}

/// Compute cosine similarity between two float vectors.
///
/// Returns a score in `[-1.0, 1.0]`. If lengths differ or either vector has zero magnitude, returns 0.0.
pub fn cosine_similarity(a: &[f32], b: &[f32]) -> f32 {
    if a.len() != b.len() || a.is_empty() {
        return 0.0;
    }

    let mut dot = 0.0f32;
    let mut norm_a = 0.0f32;
    let mut norm_b = 0.0f32;

    for i in 0..a.len() {
        dot += a[i] * b[i];
        norm_a += a[i] * a[i];
        norm_b += b[i] * b[i];
    }

    if norm_a <= 1e-9 || norm_b <= 1e-9 {
        return 0.0;
    }

    (dot / (norm_a.sqrt() * norm_b.sqrt())).clamp(-1.0, 1.0)
}

/// Normalize vector in-place to unit length (L2 norm = 1.0).
pub fn normalize_vector(v: &mut [f32]) {
    let norm_sq: f32 = v.iter().map(|x| x * x).sum();
    if norm_sq > 1e-9 {
        let inv_norm = 1.0 / norm_sq.sqrt();
        for x in v.iter_mut() {
            *x *= inv_norm;
        }
    }
}

/// Lexical keyword overlap score between query and document text.
///
/// Returns overlap ratio in `[0.0, 1.0]`.
pub fn lexical_overlap_score(query: &str, content: &str) -> f32 {
    let query_terms: Vec<String> = query
        .split(|c: char| !c.is_alphanumeric())
        .filter(|s| s.len() >= 2)
        .map(|s| s.to_lowercase())
        .collect();

    if query_terms.is_empty() {
        return 0.0;
    }

    let content_lower = content.to_lowercase();
    let mut matches = 0usize;

    for term in &query_terms {
        if content_lower.contains(term) {
            matches += 1;
        }
    }

    matches as f32 / query_terms.len() as f32
}

/// Rank document chunks against a query embedding and/or lexical query.
pub fn rank_chunks(
    query_embedding: Option<&[f32]>,
    query_text: &str,
    chunks: &[DocumentChunk],
    limit: usize,
    min_score: f32,
) -> Vec<SearchResult<DocumentChunk>> {
    let mut results: Vec<SearchResult<DocumentChunk>> = Vec::new();

    for chunk in chunks {
        let score = match (query_embedding, &chunk.embedding) {
            (Some(q_emb), Some(c_emb)) => {
                let vec_score = cosine_similarity(q_emb, c_emb);
                // If query text provided, combine vector score with small lexical boost
                let lex_score = lexical_overlap_score(query_text, &chunk.content);
                (vec_score * 0.85 + lex_score * 0.15).clamp(0.0, 1.0)
            }
            _ => {
                // Lexical fallback when embeddings are absent
                lexical_overlap_score(query_text, &chunk.content)
            }
        };

        if score >= min_score {
            results.push(SearchResult {
                item: chunk.clone(),
                score,
            });
        }
    }

    // Sort descending by score
    results.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap_or(std::cmp::Ordering::Equal));

    if results.len() > limit {
        results.truncate(limit);
    }

    results
}
