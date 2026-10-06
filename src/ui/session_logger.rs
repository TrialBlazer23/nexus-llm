use crate::client::ChatMessage;
use serde::{Deserialize, Serialize};
use std::fs::{create_dir_all, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};
use tracing::info;

/// Record for a single conversation turn in JSONL history.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatTurnRecord {
    pub timestamp: u64,
    pub model: String,
    pub host: String,
    pub role: String,
    pub content: String,
    pub tokens_per_sec: Option<f64>,
    pub tokens_streamed: Option<usize>,
}

/// Helper for logging chat sessions and exporting markdown transcripts.
pub struct SessionLogger {
    history_file: Option<PathBuf>,
}

impl SessionLogger {
    /// Initialize a new session logger, writing to `~/.nexus/history/session-<timestamp>.jsonl`.
    pub fn new(model: &str, _host: &str) -> Self {
        let history_dir = Self::default_history_dir();
        let _ = create_dir_all(&history_dir);

        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let safe_model = model.replace(['/', '\\', ':', '*'], "_");
        let filename = format!("session-{}-{}.jsonl", now, safe_model);
        let history_file = history_dir.join(filename);

        info!("Session logger initialized at: {:?}", history_file);
        Self {
            history_file: Some(history_file),
        }
    }

    /// Return the default history directory `~/.nexus/history`.
    pub fn default_history_dir() -> PathBuf {
        if let Ok(home) = std::env::var("HOME") {
            PathBuf::from(home).join(".nexus").join("history")
        } else if let Ok(userprofile) = std::env::var("USERPROFILE") {
            PathBuf::from(userprofile).join(".nexus").join("history")
        } else {
            PathBuf::from(".nexus").join("history")
        }
    }

    /// Append a message turn to the session log.
    pub fn log_turn(
        &self,
        model: &str,
        host: &str,
        role: &str,
        content: &str,
        tps: Option<f64>,
        tokens: Option<usize>,
    ) {
        if let Some(path) = &self.history_file {
            let now = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0);

            let record = ChatTurnRecord {
                timestamp: now,
                model: model.to_string(),
                host: host.to_string(),
                role: role.to_string(),
                content: content.to_string(),
                tokens_per_sec: tps,
                tokens_streamed: tokens,
            };

            if let Ok(json) = serde_json::to_string(&record) {
                if let Ok(mut file) = OpenOptions::new().create(true).append(true).open(path) {
                    let _ = writeln!(file, "{}", json);
                }
            }
        }
    }

    /// Export the conversation to a formatted Markdown file.
    pub fn export_to_markdown<P: AsRef<Path>>(
        messages: &[ChatMessage],
        model: &str,
        host: &str,
        backend: &str,
        path: P,
    ) -> io::Result<PathBuf> {
        let target_path = path.as_ref().to_path_buf();
        if let Some(parent) = target_path.parent() {
            create_dir_all(parent)?;
        }

        let mut file = File::create(&target_path)?;
        writeln!(file, "# Nexus-LLM Conversation Export")?;
        writeln!(file, "")?;
        writeln!(file, "- **Model**: `{}`", model)?;
        writeln!(file, "- **Host Endpoint**: `{}`", host)?;
        writeln!(file, "- **Hardware Backend**: `{}`", backend)?;
        writeln!(file, "- **Exported At**: `{:?}`", SystemTime::now())?;
        writeln!(file, "")?;
        writeln!(file, "---")?;
        writeln!(file, "")?;

        for msg in messages {
            let role_display = match msg.role.as_str() {
                "user" => "### 👤 You",
                "assistant" => "### 🤖 Nexus",
                "system" => "### ⚙️ System",
                _ => "### Message",
            };
            writeln!(file, "{}", role_display)?;
            writeln!(file, "")?;
            writeln!(file, "{}", msg.content.trim())?;
            writeln!(file, "")?;
        }

        info!("Exported conversation to: {:?}", target_path);
        Ok(target_path)
    }
}

