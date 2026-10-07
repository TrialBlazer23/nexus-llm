//! Janitor Agent for background session distillation and episodic memory creation.
//!
//! Analyzes recent conversation turns and completed agent bus tasks, extracts
//! durable preferences and facts, generates vector embeddings, and persists
//! provenanced records to the KnowledgeStore.

use crate::client::ChatMessage;
use crate::kb::embedder::Embedder;
use crate::kb::{EpisodicKind, EpisodicMemory, KbError, KnowledgeStore};
use crate::task::TaskRecord;
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::Path;
use tracing::info;

/// Background agent that distills operational data into durable episodic memory.
pub struct JanitorAgent {
    store: KnowledgeStore,
    embedder: Embedder,
}

impl JanitorAgent {
    /// Create a new JanitorAgent instance with a KnowledgeStore and embedder.
    pub fn new(store: KnowledgeStore, embedder: Embedder) -> Self {
        Self { store, embedder }
    }

    /// Access the underlying KnowledgeStore.
    pub fn store(&self) -> &KnowledgeStore {
        &self.store
    }

    /// Distill a slice of conversation turns into episodic memories.
    pub fn distill_dialogue(
        &self,
        turns: &[ChatMessage],
        session_id: Option<&str>,
    ) -> Result<Vec<EpisodicMemory>, KbError> {
        let mut results = Vec::new();
        if turns.is_empty() {
            return Ok(results);
        }

        // 1. Scan for explicit user preferences or facts
        for turn in turns {
            let content = turn.content.trim();
            if content.is_empty() {
                continue;
            }

            // Check for user preferences
            if turn.role.eq_ignore_ascii_case("user") {
                if let Some((title, summary)) = extract_preference(content) {
                    let tags = vec![
                        "provenance:janitor".to_string(),
                        "source:session".to_string(),
                        "type:preference".to_string(),
                    ];
                    let embedding = Some(self.embedder.embed_sync(&summary));
                    let mem = self.store.store_memory(
                        session_id,
                        EpisodicKind::Preference,
                        &title,
                        &summary,
                        Some(content),
                        tags,
                        embedding,
                    )?;
                    results.push(mem);
                }
            }

            // Check for factual cluster / environment declarations
            if let Some((title, summary)) = extract_fact(content) {
                let tags = vec![
                    "provenance:janitor".to_string(),
                    "source:session".to_string(),
                    "type:fact".to_string(),
                ];
                let embedding = Some(self.embedder.embed_sync(&summary));
                let mem = self.store.store_memory(
                    session_id,
                    EpisodicKind::Fact,
                    &title,
                    &summary,
                    Some(content),
                    tags,
                    embedding,
                )?;
                results.push(mem);
            }
        }

        // 2. Synthesize an overall conversation summary if there are sufficient dialogue turns
        if turns.len() >= 2 {
            let summary_text = synthesize_conversation_summary(turns);
            let title = format!(
                "Session Summary: {}",
                session_id.unwrap_or("general_dialogue")
            );
            let tags = vec![
                "provenance:janitor".to_string(),
                "source:session".to_string(),
                "type:summary".to_string(),
            ];
            let embedding = Some(self.embedder.embed_sync(&summary_text));
            let mem = self.store.store_memory(
                session_id,
                EpisodicKind::Summary,
                &title,
                &summary_text,
                None,
                tags,
                embedding,
            )?;
            results.push(mem);
        }

        info!(
            "JanitorAgent distilled {} episodic memories from session {:?}",
            results.len(),
            session_id
        );
        Ok(results)
    }

    /// Distill a completed agent bus task into an episodic memory record.
    pub fn distill_task(&self, task: &TaskRecord) -> Result<Option<EpisodicMemory>, KbError> {
        let output = match &task.output {
            Some(out) if !out.trim().is_empty() => out.trim(),
            _ => return Ok(None),
        };

        let title = format!("Task [{}]: Route {}", task.task_id, task.to_route);
        let preview_prompt = if task.prompt.len() > 120 {
            format!("{}…", &task.prompt[..120])
        } else {
            task.prompt.clone()
        };
        let preview_output = if output.len() > 160 {
            format!("{}…", &output[..160])
        } else {
            output.to_string()
        };

        let summary = format!(
            "Prompt: \"{}\" -> Result: \"{}\"",
            preview_prompt, preview_output
        );

        let tags = vec![
            "provenance:janitor".to_string(),
            format!("task:{}", task.task_id),
            format!("route:{}", task.to_route),
            "type:task_summary".to_string(),
        ];

        let embedding = Some(self.embedder.embed_sync(&summary));
        let memory = self.store.store_memory(
            Some(&task.task_id.to_string()),
            EpisodicKind::Summary,
            &title,
            &summary,
            Some(output),
            tags,
            embedding,
        )?;

        info!(
            "JanitorAgent distilled task {} into episodic memory {}",
            task.task_id, memory.id
        );
        Ok(Some(memory))
    }

    /// Read and distill turns from a session JSONL file (as written by SessionLogger).
    pub fn distill_session_file<P: AsRef<Path>>(
        &self,
        path: P,
    ) -> Result<Vec<EpisodicMemory>, KbError> {
        let file = File::open(path.as_ref())?;
        let reader = BufReader::new(file);
        let mut turns = Vec::new();

        let session_name = path
            .as_ref()
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("session");

        for line in reader.lines() {
            let line = line?;
            if line.trim().is_empty() {
                continue;
            }
            if let Ok(record) =
                serde_json::from_str::<crate::ui::session_logger::ChatTurnRecord>(&line)
            {
                turns.push(ChatMessage {
                    role: record.role,
                    content: record.content,
                });
            }
        }

        self.distill_dialogue(&turns, Some(session_name))
    }
}

/// Helper to detect and extract user preferences from prompt text.
fn extract_preference(text: &str) -> Option<(String, String)> {
    let lower = text.to_lowercase();
    let keywords = [
        "i prefer",
        "always use",
        "never use",
        "my preference",
        "please respond in",
        "keep responses",
        "write only in",
        "my name is",
    ];

    for kw in &keywords {
        if let Some(pos) = lower.find(kw) {
            let slice = &text[pos..];
            let end = slice.find('.').unwrap_or(slice.len());
            let pref = slice[..end].trim();
            if !pref.is_empty() {
                let title = format!("Preference: {}", capitalize_first(pref));
                return Some((title, pref.to_string()));
            }
        }
    }
    None
}

/// Helper to detect and extract declared environment facts.
fn extract_fact(text: &str) -> Option<(String, String)> {
    let lower = text.to_lowercase();
    let keywords = [
        "has 16gb",
        "has 8gb",
        "has 4gb",
        "runs on port",
        "node id is",
        "cluster endpoint",
        "gpu acceleration is",
        "hardware profile:",
    ];

    for kw in &keywords {
        if let Some(pos) = lower.find(kw) {
            let start = text[..pos].rfind('\n').map(|p| p + 1).unwrap_or(0);
            let end = text[pos..]
                .find('\n')
                .map(|p| pos + p)
                .unwrap_or(text.len());
            let fact = text[start..end].trim();
            if !fact.is_empty() {
                let title = "Fact: Cluster Environment".to_string();
                return Some((title, fact.to_string()));
            }
        }
    }
    None
}

/// Synthesize a readable multi-turn summary.
fn synthesize_conversation_summary(turns: &[ChatMessage]) -> String {
    let mut topics = Vec::new();
    for turn in turns {
        let first_line = turn.content.lines().next().unwrap_or("").trim();
        if !first_line.is_empty() && first_line.len() <= 100 {
            topics.push(format!("{}: {}", turn.role, first_line));
        }
    }
    if topics.is_empty() {
        "Multi-turn conversation concluded.".to_string()
    } else {
        topics.join(" | ")
    }
}

fn capitalize_first(s: &str) -> String {
    let mut chars = s.chars();
    match chars.next() {
        None => String::new(),
        Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
    }
}
