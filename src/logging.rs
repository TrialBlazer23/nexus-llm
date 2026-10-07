//! File-based tracing for TUI and daemon modes.
//!
//! Stdout must stay clean for the alternate screen, so logs go to
//! `~/.nexus/logs/nexus-<pid>.log` (override with `NEXUS_LOG_DIR`).

use std::fs::{self, OpenOptions};
use std::io;
use std::path::PathBuf;
use std::sync::Mutex;
use tracing_subscriber::EnvFilter;
use tracing_subscriber::FmtSubscriber;

/// Install a global tracing subscriber that appends to a per-process log file.
///
/// Filter order: `RUST_LOG` env → `default_level` argument (e.g. `"info"`).
/// Returns the log file path so callers can print it on exit.
pub fn init_file_logging(default_level: &str) -> io::Result<PathBuf> {
    let log_dir = log_dir();
    fs::create_dir_all(&log_dir)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(&log_dir, fs::Permissions::from_mode(0o700));
    }

    let path = log_dir.join(format!("nexus-{}.log", std::process::id()));
    let file = OpenOptions::new().create(true).append(true).open(&path)?;

    let filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(default_level));

    let subscriber = FmtSubscriber::builder()
        .with_env_filter(filter)
        .with_ansi(false)
        .with_writer(Mutex::new(file))
        .finish();

    tracing::subscriber::set_global_default(subscriber).map_err(|e| {
        io::Error::new(
            io::ErrorKind::AlreadyExists,
            format!("tracing subscriber already set: {e}"),
        )
    })?;

    Ok(path)
}

/// Directory used for Nexus diagnostic logs.
pub fn log_dir() -> PathBuf {
    if let Ok(custom) = std::env::var("NEXUS_LOG_DIR") {
        return PathBuf::from(custom);
    }
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
    PathBuf::from(home).join(".nexus").join("logs")
}
