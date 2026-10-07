//! Durable task state tracking for the model-to-model agent bus.
//!
//! Stores task records in JSON format (by default `~/.nexus/tasks.json`).
//! Employs atomic tempfile renames to prevent partial writes.

use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};
use thiserror::Error;
use uuid::Uuid;

const TASK_STORE_FILE_NAME: &str = "tasks.json";

#[derive(Error, Debug)]
pub enum TaskError {
    #[error("I/O error in task store: {0}")]
    Io(#[from] std::io::Error),
    #[error("Failed to serialize or deserialize task store: {0}")]
    Json(#[from] serde_json::Error),
    #[error("Task {0} not found")]
    NotFound(Uuid),
}

/// Execution status of an agent task.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TaskStatus {
    Pending,
    Running,
    Completed,
    Failed,
}

impl std::fmt::Display for TaskStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Pending => write!(f, "pending"),
            Self::Running => write!(f, "running"),
            Self::Completed => write!(f, "completed"),
            Self::Failed => write!(f, "failed"),
        }
    }
}

/// Record tracking an individual delegated agent task.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskRecord {
    pub task_id: Uuid,
    pub from_node: Uuid,
    pub to_route: String,
    pub prompt: String,
    pub status: TaskStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    pub created_at: u64,
    pub updated_at: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
pub struct TaskStoreData {
    pub version: u32,
    pub tasks: Vec<TaskRecord>,
}

/// Thread-safe durable task store.
#[derive(Debug, Clone)]
pub struct TaskStore {
    path: PathBuf,
    data: Arc<Mutex<TaskStoreData>>,
}

fn current_unix_timestamp() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

impl TaskStore {
    /// Default storage location for tasks: `$NEXUS_TASKS_FILE` or `~/.nexus/tasks.json`.
    pub fn default_path() -> PathBuf {
        if let Ok(p) = std::env::var("NEXUS_TASKS_FILE") {
            return PathBuf::from(p);
        }
        let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
        PathBuf::from(home)
            .join(".nexus")
            .join(TASK_STORE_FILE_NAME)
    }

    /// Load or initialize a task store at `path`.
    pub fn load_or_create<P: AsRef<Path>>(path: P) -> Result<Self, TaskError> {
        let path = path.as_ref().to_path_buf();
        let data = if path.exists() {
            let bytes = fs::read(&path)?;
            let parsed: TaskStoreData = serde_json::from_slice(&bytes)?;
            parsed
        } else {
            TaskStoreData {
                version: 1,
                tasks: Vec::new(),
            }
        };

        Ok(Self {
            path,
            data: Arc::new(Mutex::new(data)),
        })
    }

    /// Save current task data atomically to disk.
    fn persist(&self) -> Result<(), TaskError> {
        let data_guard = self.data.lock().expect("task store lock poisoned");
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent)?;
        }
        let bytes = serde_json::to_vec_pretty(&*data_guard)?;
        let tmp = self.path.with_extension("json.tmp");
        fs::write(&tmp, bytes)?;
        fs::rename(&tmp, &self.path)?;
        Ok(())
    }

    /// Enqueue a new task into the store.
    pub fn create_task(
        &self,
        task_id: Uuid,
        from_node: Uuid,
        to_route: impl Into<String>,
        prompt: impl Into<String>,
    ) -> Result<TaskRecord, TaskError> {
        let now = current_unix_timestamp();
        let record = TaskRecord {
            task_id,
            from_node,
            to_route: to_route.into(),
            prompt: prompt.into(),
            status: TaskStatus::Pending,
            output: None,
            error: None,
            created_at: now,
            updated_at: now,
        };

        {
            let mut data = self.data.lock().expect("task store lock");
            // If task already exists, update prompt / reset status
            if let Some(existing) = data.tasks.iter_mut().find(|t| t.task_id == task_id) {
                *existing = record.clone();
            } else {
                data.tasks.push(record.clone());
            }
        }

        self.persist()?;
        Ok(record)
    }

    /// Get a cloned task record by ID.
    pub fn get_task(&self, task_id: Uuid) -> Option<TaskRecord> {
        let data = self.data.lock().expect("task store lock");
        data.tasks.iter().find(|t| t.task_id == task_id).cloned()
    }

    /// Update status of a task.
    pub fn update_status(&self, task_id: Uuid, status: TaskStatus) -> Result<bool, TaskError> {
        let updated = {
            let mut data = self.data.lock().expect("task store lock");
            if let Some(task) = data.tasks.iter_mut().find(|t| t.task_id == task_id) {
                task.status = status;
                task.updated_at = current_unix_timestamp();
                true
            } else {
                false
            }
        };

        if updated {
            self.persist()?;
        }
        Ok(updated)
    }

    /// Mark task as completed with output.
    pub fn complete_task(
        &self,
        task_id: Uuid,
        output: impl Into<String>,
    ) -> Result<bool, TaskError> {
        let updated = {
            let mut data = self.data.lock().expect("task store lock");
            if let Some(task) = data.tasks.iter_mut().find(|t| t.task_id == task_id) {
                task.status = TaskStatus::Completed;
                task.output = Some(output.into());
                task.error = None;
                task.updated_at = current_unix_timestamp();
                true
            } else {
                false
            }
        };

        if updated {
            self.persist()?;
        }
        Ok(updated)
    }

    /// Mark task as failed with an error message.
    pub fn fail_task(&self, task_id: Uuid, error: impl Into<String>) -> Result<bool, TaskError> {
        let updated = {
            let mut data = self.data.lock().expect("task store lock");
            if let Some(task) = data.tasks.iter_mut().find(|t| t.task_id == task_id) {
                task.status = TaskStatus::Failed;
                task.error = Some(error.into());
                task.updated_at = current_unix_timestamp();
                true
            } else {
                false
            }
        };

        if updated {
            self.persist()?;
        }
        Ok(updated)
    }

    /// List all tasks in the store.
    pub fn list_tasks(&self) -> Vec<TaskRecord> {
        let data = self.data.lock().expect("task store lock");
        data.tasks.clone()
    }

    /// Count of tasks by status.
    pub fn count_by_status(&self, status: TaskStatus) -> usize {
        let data = self.data.lock().expect("task store lock");
        data.tasks.iter().filter(|t| t.status == status).count()
    }
}
