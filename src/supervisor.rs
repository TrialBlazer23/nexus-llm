use crate::sysinfo::{AccelerationBackend, SystemProfile};
use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use thiserror::Error;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::watch;
use tracing::{debug, error, info, trace, warn};

#[derive(Error, Debug)]
pub enum SupervisorError {
    #[error("llama-server binary not found at: {0}")]
    BinaryNotFound(PathBuf),

    #[error("Model file not found at: {0}")]
    ModelNotFound(PathBuf),

    #[error("Android LMK Guard: Model size ({required_mb} MB) exceeds 75% memory ceiling ({max_allowed_mb} MB of {available_mb} MB available)")]
    MemoryCapExceeded {
        required_mb: u64,
        available_mb: u64,
        max_allowed_mb: u64,
    },

    #[error("Failed to spawn process: {0}")]
    SpawnFailed(String),

    #[error("Process exited prematurely with code: {0:?}")]
    ProcessExitedEarly(Option<i32>),

    #[error("Health check timed out after {0:?}")]
    HealthCheckTimeout(Duration),

    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SupervisorState {
    Starting,
    Ready,
    Stopped,
    Failed,
}

/// Configuration parameters for spawning the underlying llama-server instance.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LlamaServerConfig {
    pub binary_path: PathBuf,
    pub model_path: PathBuf,
    pub host: String,
    pub port: u16,
    pub gpu_layers: u32, // 99 for Vulkan offload, 0 for CPU
    pub threads: usize,
    pub context_size: usize,
    pub extra_args: Vec<String>,
}

impl LlamaServerConfig {
    pub fn new<P: AsRef<Path>, M: AsRef<Path>>(
        binary_path: P,
        model_path: M,
        host: impl Into<String>,
        port: u16,
    ) -> Self {
        Self {
            binary_path: binary_path.as_ref().to_path_buf(),
            model_path: model_path.as_ref().to_path_buf(),
            host: host.into(),
            port,
            gpu_layers: 99,
            threads: 6,
            context_size: 4096,
            extra_args: Vec::new(),
        }
    }

    /// Check if RPC distributed layer offloading is enabled in configuration.
    pub fn is_distributed(&self) -> bool {
        self.extra_args.iter().any(|arg| arg == "--rpc")
    }

    /// Construct command-line argument list for llama-server.
    pub fn build_args(&self, effective_gpu_layers: u32) -> Vec<String> {
        let model_alias = self
            .model_path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("model")
            .to_string();

        let mut args = vec![
            "--host".to_string(),
            self.host.clone(),
            "--port".to_string(),
            self.port.to_string(),
            "-m".to_string(),
            self.model_path.to_string_lossy().to_string(),
            "--alias".to_string(),
            model_alias,
            "-c".to_string(),
            self.context_size.to_string(),
            "-t".to_string(),
            self.threads.to_string(),
            "-ngl".to_string(),
            effective_gpu_layers.to_string(),
        ];
        args.extend(self.extra_args.clone());
        args
    }
}

/// Manages the llama-server child process with health checks, SIGTERM/SIGKILL lifecycle,
/// and Vulkan-to-CPU automatic fallback.
pub struct ProcessSupervisor {
    child: Option<Child>,
    config: LlamaServerConfig,
    active_backend: AccelerationBackend,
    client: reqwest::Client,
    state_tx: watch::Sender<SupervisorState>,
    stderr_history: Arc<Mutex<VecDeque<String>>>,
}

impl ProcessSupervisor {
    /// Return the active acceleration backend running on the child process.
    pub fn active_backend(&self) -> AccelerationBackend {
        self.active_backend
    }

    /// Return reference to configuration.
    pub fn config(&self) -> &LlamaServerConfig {
        &self.config
    }

    pub fn subscribe(&self) -> watch::Receiver<SupervisorState> {
        self.state_tx.subscribe()
    }

    pub fn state(&self) -> SupervisorState {
        *self.state_tx.borrow()
    }

    /// Return a copy of recently captured stderr lines from llama-server (up to 50 lines).
    pub fn last_stderr_lines(&self) -> Vec<String> {
        self.stderr_history
            .lock()
            .map(|hist| hist.iter().cloned().collect())
            .unwrap_or_default()
    }

    /// Non-blocking check of whether the underlying child process has exited.
    pub fn check_status(&mut self) -> Result<Option<std::process::ExitStatus>, std::io::Error> {
        if let Some(child) = &mut self.child {
            let res = child.try_wait()?;
            if res.is_some() {
                let _ = self.state_tx.send(SupervisorState::Failed);
            }
            Ok(res)
        } else {
            Ok(None)
        }
    }

    /// Spawn llama-server with Vulkan GPU offload if requested, automatically
    /// falling back to CPU mode if Vulkan runtime fails to initialize.
    pub async fn spawn_with_fallback(config: LlamaServerConfig) -> Result<Self, SupervisorError> {
        // Pre-flight check: Binary existence
        if !config.binary_path.exists() {
            // Also check if binary is in PATH
            if which::which(&config.binary_path).is_err() && !is_in_path(&config.binary_path) {
                return Err(SupervisorError::BinaryNotFound(config.binary_path));
            }
        }

        // Pre-flight check: Model existence
        if !config.model_path.exists() {
            return Err(SupervisorError::ModelNotFound(config.model_path));
        }

        // Pre-flight check: Android LMK 75% memory ceiling guard (for standalone mode)
        if !config.is_distributed() {
            let model_metadata = tokio::fs::metadata(&config.model_path).await?;
            let model_size_bytes = model_metadata.len();
            let sys_profile = SystemProfile::probe();

            if !sys_profile.can_safely_load(model_size_bytes, config.context_size) {
                let kv_bytes = SystemProfile::estimate_kv_cache_bytes(config.context_size);
                let total_required = model_size_bytes + kv_bytes;
                return Err(SupervisorError::MemoryCapExceeded {
                    required_mb: total_required / (1024 * 1024),
                    available_mb: sys_profile.available_ram_mb,
                    max_allowed_mb: sys_profile.max_allowed_memory_bytes() / (1024 * 1024),
                });
            }
        }

        let http_client = reqwest::Client::builder()
            .timeout(Duration::from_secs(2))
            .build()
            .map_err(|e| SupervisorError::SpawnFailed(e.to_string()))?;

        let stderr_history = Arc::new(Mutex::new(VecDeque::with_capacity(50)));

        // Attempt 1: Vulkan offload if requested
        if config.gpu_layers > 0 {
            info!(
                "Attempting to spawn llama-server with Vulkan offload (-ngl {})",
                config.gpu_layers
            );
            match Self::try_spawn(&config, config.gpu_layers).await {
                Ok((mut child, mut stdout_reader, mut stderr_reader)) => {
                    // Check if Vulkan initialization succeeds or fails
                    let vulkan_result = Self::monitor_vulkan_init(
                        &mut child,
                        &mut stdout_reader,
                        &mut stderr_reader,
                        stderr_history.clone(),
                        Duration::from_secs(4),
                    )
                    .await;

                    match vulkan_result {
                        Ok(()) => {
                            info!("Vulkan acceleration successfully initialized.");
                            Self::spawn_drain_tasks(stdout_reader, stderr_reader, stderr_history.clone());
                            let (state_tx, _) = watch::channel(SupervisorState::Starting);
                            let mut supervisor = Self {
                                child: Some(child),
                                config: config.clone(),
                                active_backend: AccelerationBackend::Vulkan,
                                client: http_client.clone(),
                                state_tx,
                                stderr_history: stderr_history.clone(),
                            };

                            // Wait for /health endpoint readiness
                            if supervisor.wait_until_ready(Duration::from_secs(15)).await {
                                let _ = supervisor.state_tx.send(SupervisorState::Ready);
                                return Ok(supervisor);
                            } else {
                                warn!("Health check timed out on Vulkan backend. Terminating...");
                                let _ = supervisor.state_tx.send(SupervisorState::Failed);
                                let _ = supervisor.stop().await;
                            }
                        }
                        Err(err_msg) => {
                            warn!(
                                "Vulkan initialization failed ({}); falling back to CPU mode...",
                                err_msg
                            );
                            // Kill failed child cleanly
                            let _ = child.kill().await;
                        }
                    }
                }
                Err(e) => {
                    warn!(
                        "Failed to spawn with Vulkan flags: {}; falling back to CPU mode...",
                        e
                    );
                }
            }
        }

        // Attempt 2: CPU fallback
        info!(
            "Spawning llama-server in CPU mode (-ngl 0, threads: {})",
            config.threads
        );
        let (child, stdout_reader, stderr_reader) = Self::try_spawn(&config, 0).await?;
        Self::spawn_drain_tasks(stdout_reader, stderr_reader, stderr_history.clone());

        let (state_tx, _) = watch::channel(SupervisorState::Starting);
        let mut supervisor = Self {
            child: Some(child),
            config: config.clone(),
            active_backend: if cfg!(target_arch = "aarch64") {
                AccelerationBackend::ArmCpuDotProd
            } else {
                AccelerationBackend::X86Baseline
            },
            client: http_client,
            state_tx,
            stderr_history,
        };

        if !supervisor.wait_until_ready(Duration::from_secs(20)).await {
            let _ = supervisor.state_tx.send(SupervisorState::Failed);
            let _ = supervisor.stop().await;
            return Err(SupervisorError::HealthCheckTimeout(Duration::from_secs(20)));
        }

        let _ = supervisor.state_tx.send(SupervisorState::Ready);
        info!("llama-server successfully running on CPU backend.");
        Ok(supervisor)
    }

    /// Spawn background tasks to drain child stdout/stderr so pipes never block or trigger SIGPIPE.
    fn spawn_drain_tasks(
        mut stdout_reader: BufReader<tokio::process::ChildStdout>,
        mut stderr_reader: BufReader<tokio::process::ChildStderr>,
        stderr_history: Arc<Mutex<VecDeque<String>>>,
    ) {
        tokio::spawn(async move {
            let mut line = String::new();
            while let Ok(n) = stdout_reader.read_line(&mut line).await {
                if n == 0 {
                    break;
                }
                trace!("[llama-server stdout] {}", line.trim_end());
                line.clear();
            }
        });

        tokio::spawn(async move {
            let mut line = String::new();
            while let Ok(n) = stderr_reader.read_line(&mut line).await {
                if n == 0 {
                    break;
                }
                let trimmed = line.trim_end().to_string();
                if !trimmed.is_empty() {
                    debug!("[llama-server stderr] {}", trimmed);
                    if let Ok(mut hist) = stderr_history.lock() {
                        if hist.len() >= 50 {
                            hist.pop_front();
                        }
                        hist.push_back(trimmed);
                    }
                }
                line.clear();
            }
        });
    }

    /// Helper to spawn child with piped stdout and stderr.
    async fn try_spawn(
        config: &LlamaServerConfig,
        gpu_layers: u32,
    ) -> Result<
        (
            Child,
            BufReader<tokio::process::ChildStdout>,
            BufReader<tokio::process::ChildStderr>,
        ),
        SupervisorError,
    > {
        let args = config.build_args(gpu_layers);
        debug!(
            "Spawning process: {:?} with args: {:?}",
            config.binary_path, args
        );

        let mut cmd = Command::new(&config.binary_path);
        cmd.args(&args)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);

        let mut child = cmd
            .spawn()
            .map_err(|e| SupervisorError::SpawnFailed(e.to_string()))?;

        let stdout = child.stdout.take().ok_or_else(|| {
            SupervisorError::SpawnFailed("Failed to capture child stdout".to_string())
        })?;
        let stderr = child.stderr.take().ok_or_else(|| {
            SupervisorError::SpawnFailed("Failed to capture child stderr".to_string())
        })?;

        Ok((child, BufReader::new(stdout), BufReader::new(stderr)))
    }

    /// Monitor startup log output for Vulkan error indicators or early exit.
    async fn monitor_vulkan_init(
        child: &mut Child,
        _stdout_reader: &mut BufReader<tokio::process::ChildStdout>,
        stderr_reader: &mut BufReader<tokio::process::ChildStderr>,
        stderr_history: Arc<Mutex<VecDeque<String>>>,
        timeout: Duration,
    ) -> Result<(), String> {
        let deadline = tokio::time::Instant::now() + timeout;

        loop {
            tokio::select! {
                // Check if child exited early
                status = child.wait() => {
                    return Err(format!("Process exited prematurely: {:?}", status));
                }
                // Read a line from stderr (where llama.cpp outputs device init logs)
                line_res = async {
                    let mut line = String::new();
                    let bytes = stderr_reader.read_line(&mut line).await;
                    (bytes, line)
                } => {
                    match line_res {
                        (Ok(0), _) => {
                            // EOF on stderr
                            break;
                        }
                        (Ok(_), line) => {
                            let trimmed = line.trim_end().to_string();
                            if !trimmed.is_empty() {
                                if let Ok(mut hist) = stderr_history.lock() {
                                    if hist.len() >= 50 {
                                        hist.pop_front();
                                    }
                                    hist.push_back(trimmed.clone());
                                }
                            }
                            let lower = line.to_lowercase();
                            debug!("[llama-server] {}", line.trim_end());

                            if lower.contains("vk_error")
                                || lower.contains("failed to initialize vulkan")
                                || lower.contains("ggml_vulkan: failed")
                                || lower.contains("no vulkan device")
                            {
                                return Err(format!("Vulkan error detected: {}", line.trim()));
                            }

                            // If server already started listening or loaded weights
                            if lower.contains("http server listening") || lower.contains("model loaded") {
                                return Ok(());
                            }
                        }
                        (Err(e), _) => {
                            return Err(format!("Error reading process output: {}", e));
                        }
                    }
                }
                _ = tokio::time::sleep_until(deadline) => {
                    // Timeout elapsed without fatal Vulkan error
                    break;
                }
            }
        }

        Ok(())
    }

    /// Poll `/health` endpoint until the server is ready or timeout occurs.
    async fn wait_until_ready(&mut self, timeout: Duration) -> bool {
        let start = tokio::time::Instant::now();

        while start.elapsed() < timeout {
            // Check if process terminated prematurely
            if let Some(child) = &mut self.child {
                if let Ok(Some(status)) = child.try_wait() {
                    error!("Child process exited with status: {:?}", status);
                    return false;
                }
            }

            if self.is_healthy().await {
                return true;
            }

            tokio::time::sleep(Duration::from_millis(500)).await;
        }

        false
    }

    /// Query child status and probe `http://{host}:{port}/health`.
    pub async fn is_healthy(&self) -> bool {
        let health_url = format!("http://{}:{}/health", self.config.host, self.config.port);

        match self.client.get(&health_url).send().await {
            Ok(resp) => {
                // HTTP 200 means healthy; HTTP 503 sometimes means "loading model"
                resp.status().is_success()
            }
            Err(_) => false,
        }
    }

    /// Wait asynchronously for child process to exit.
    pub async fn wait(&mut self) -> Result<std::process::ExitStatus, std::io::Error> {
        if let Some(child) = &mut self.child {
            let result = child.wait().await;
            if result.is_ok() {
                let _ = self.state_tx.send(SupervisorState::Failed);
            }
            result
        } else {
            Err(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "No child process running",
            ))
        }
    }

    /// Gracefully stop the underlying llama-server child process.
    pub async fn stop(&mut self) -> Result<(), SupervisorError> {
        if let Some(mut child) = self.child.take() {
            let _ = self.state_tx.send(SupervisorState::Stopped);
            if let Some(pid) = child.id() {
                info!("Sending SIGTERM to llama-server (PID: {})...", pid);
                unsafe {
                    libc::kill(pid as libc::pid_t, libc::SIGTERM);
                }

                // Wait up to 3 seconds for graceful shutdown
                let wait_timeout = tokio::time::sleep(Duration::from_secs(3));
                tokio::pin!(wait_timeout);

                tokio::select! {
                    res = child.wait() => {
                        debug!("Process exited gracefully: {:?}", res);
                        return Ok(());
                    }
                    _ = &mut wait_timeout => {
                        warn!("Process did not exit after SIGTERM, sending SIGKILL...");
                        let _ = child.kill().await;
                    }
                }
            }
        }
        Ok(())
    }
}

impl Drop for ProcessSupervisor {
    fn drop(&mut self) {
        if let Some(child) = self.child.take() {
            if let Some(pid) = child.id() {
                debug!("Dropping ProcessSupervisor: sending SIGKILL to PID {}", pid);
                unsafe {
                    libc::kill(pid as libc::pid_t, libc::SIGKILL);
                }
            }
        }
    }
}

/// Shared thread-safe handle to manage an active ProcessSupervisor instance.
#[derive(Clone, Default)]
pub struct SupervisorManager {
    inner: std::sync::Arc<tokio::sync::Mutex<Option<ProcessSupervisor>>>,
}

impl SupervisorManager {
    pub fn new() -> Self {
        Self {
            inner: std::sync::Arc::new(tokio::sync::Mutex::new(None)),
        }
    }

    /// True when a supervisor child process slot is occupied (may still be starting).
    pub async fn is_running(&self) -> bool {
        let lock = self.inner.lock().await;
        lock.is_some()
    }

    /// Non-async check for UI rendering (best-effort; treats a held lock as running).
    pub fn is_running_blocking(&self) -> bool {
        match self.inner.try_lock() {
            Ok(guard) => guard.is_some(),
            Err(_) => true,
        }
    }

    /// Check if a supervisor child process is currently running and healthy.
    pub async fn is_healthy(&self) -> bool {
        let lock = self.inner.lock().await;
        if let Some(sup) = lock.as_ref() {
            sup.is_healthy().await
        } else {
            false
        }
    }

    /// Return the active model path/name if currently running.
    pub async fn active_model(&self) -> Option<String> {
        let lock = self.inner.lock().await;
        lock.as_ref().map(|sup| {
            sup.config()
                .model_path
                .file_name()
                .map(|f| f.to_string_lossy().to_string())
                .unwrap_or_else(|| "active_model".to_string())
        })
    }

    /// Poll child process exit status. On exit, returns `(status, last_stderr_lines)`
    /// and clears the supervisor slot.
    pub async fn check_status(
        &self,
    ) -> Result<Option<(std::process::ExitStatus, Vec<String>)>, std::io::Error> {
        let mut lock = self.inner.lock().await;
        if let Some(sup) = lock.as_mut() {
            match sup.check_status()? {
                Some(status) => {
                    let lines = sup.last_stderr_lines();
                    *lock = None;
                    Ok(Some((status, lines)))
                }
                None => Ok(None),
            }
        } else {
            Ok(None)
        }
    }

    /// Last stderr lines from the active supervisor, if any.
    pub async fn last_stderr_lines(&self) -> Vec<String> {
        let lock = self.inner.lock().await;
        lock.as_ref()
            .map(|sup| sup.last_stderr_lines())
            .unwrap_or_default()
    }

    /// Spawn a new model supervisor, stopping any previously running instance.
    pub async fn spawn(&self, config: LlamaServerConfig) -> Result<(), SupervisorError> {
        let mut lock = self.inner.lock().await;
        if let Some(mut existing) = lock.take() {
            let _ = existing.stop().await;
        }
        let sup = ProcessSupervisor::spawn_with_fallback(config).await?;
        *lock = Some(sup);
        Ok(())
    }

    /// Stop the active model supervisor.
    pub async fn stop(&self) -> Result<(), SupervisorError> {
        let mut lock = self.inner.lock().await;
        if let Some(mut existing) = lock.take() {
            existing.stop().await?;
        }
        Ok(())
    }
}

/// Helper to check if binary is in PATH
fn is_in_path<P: AsRef<Path>>(binary: P) -> bool {
    let binary = binary.as_ref();
    if binary.is_absolute() {
        return binary.exists();
    }
    if let Ok(path_var) = std::env::var("PATH") {
        for dir in std::env::split_paths(&path_var) {
            if dir.join(binary).is_file() {
                return true;
            }
        }
    }
    false
}

// Module for path resolution without external `which` crate
mod which {
    use std::path::{Path, PathBuf};

    pub fn which<P: AsRef<Path>>(binary: P) -> Result<PathBuf, ()> {
        let binary = binary.as_ref();
        if binary.is_absolute() && binary.is_file() {
            return Ok(binary.to_path_buf());
        }
        if let Ok(path_var) = std::env::var("PATH") {
            for dir in std::env::split_paths(&path_var) {
                let candidate = dir.join(binary);
                if candidate.is_file() {
                    return Ok(candidate);
                }
            }
        }
        Err(())
    }
}
