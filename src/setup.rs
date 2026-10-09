//! Bootstrap helpers for `nexus setup` and `scripts/setup.sh`.

use crate::config::{ConfigError, NexusConfig};
use std::env;
use std::path::{Path, PathBuf};
use std::process::Command;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum SetupError {
    #[error(transparent)]
    Config(#[from] ConfigError),

    #[error(
        "setup script not found (looked under NEXUS_ROOT, cwd, and executable-relative paths)"
    )]
    ScriptNotFound,

    #[error("failed to execute setup script: {0}")]
    Execute(#[from] std::io::Error),

    #[error("setup script exited with status {0}")]
    ExitStatus(i32),

    #[error("invalid setup arguments: {0}")]
    InvalidArgs(String),
}

/// Options produced by `nexus setup __write_bins ...`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BinPathUpdate {
    pub prefix: Option<PathBuf>,
    pub llama_server: Option<PathBuf>,
    pub rpc_server: Option<PathBuf>,
    pub bmoe_cli: Option<PathBuf>,
    pub enable_moe: bool,
    pub disable_moe: bool,
}

impl BinPathUpdate {
    pub fn parse(args: &[String]) -> Result<Self, SetupError> {
        let mut out = Self::default();
        let mut i = 0;
        while i < args.len() {
            match args[i].as_str() {
                "--prefix" => {
                    i += 1;
                    let v = args
                        .get(i)
                        .ok_or_else(|| SetupError::InvalidArgs("--prefix needs a value".into()))?;
                    out.prefix = Some(PathBuf::from(v));
                }
                "--llama-server" => {
                    i += 1;
                    let v = args.get(i).ok_or_else(|| {
                        SetupError::InvalidArgs("--llama-server needs a value".into())
                    })?;
                    out.llama_server = Some(PathBuf::from(v));
                }
                "--rpc-server" => {
                    i += 1;
                    let v = args.get(i).ok_or_else(|| {
                        SetupError::InvalidArgs("--rpc-server needs a value".into())
                    })?;
                    out.rpc_server = Some(PathBuf::from(v));
                }
                "--bmoe-cli" => {
                    i += 1;
                    let v = args.get(i).ok_or_else(|| {
                        SetupError::InvalidArgs("--bmoe-cli needs a value".into())
                    })?;
                    out.bmoe_cli = Some(PathBuf::from(v));
                }
                "--enable-moe" => out.enable_moe = true,
                "--disable-moe" => out.disable_moe = true,
                other => {
                    return Err(SetupError::InvalidArgs(format!("unknown flag: {other}")));
                }
            }
            i += 1;
        }
        if out.enable_moe && out.disable_moe {
            return Err(SetupError::InvalidArgs(
                "--enable-moe and --disable-moe are mutually exclusive".into(),
            ));
        }
        if out.prefix.is_none()
            && out.llama_server.is_none()
            && out.rpc_server.is_none()
            && out.bmoe_cli.is_none()
            && !out.enable_moe
            && !out.disable_moe
        {
            return Err(SetupError::InvalidArgs(
                "provide --prefix and/or explicit binary paths".into(),
            ));
        }
        Ok(out)
    }
}

/// Apply absolute binary paths from a setup prefix into `~/.nexus/config.toml`.
pub fn apply_bin_paths(update: &BinPathUpdate) -> Result<NexusConfig, SetupError> {
    let mut cfg = NexusConfig::load().unwrap_or_default();
    cfg.ensure_identity()?;

    let prefix = update.prefix.clone();
    let resolve = |explicit: &Option<PathBuf>, name: &str| -> Option<PathBuf> {
        if let Some(p) = explicit {
            return Some(p.clone());
        }
        prefix
            .as_ref()
            .map(|p| p.join("bin").join(name))
            .filter(|p| p.exists())
    };

    if let Some(p) = resolve(&update.llama_server, "llama-server") {
        cfg.node.llama_server_binary = p.display().to_string();
    }
    if let Some(p) = resolve(&update.rpc_server, "rpc-server") {
        cfg.node.rpc_server_binary = p.display().to_string();
    }
    if let Some(p) = resolve(&update.bmoe_cli, "bmoe-cli") {
        cfg.inference.moe.bmoe_binary = p.display().to_string();
    }
    if update.enable_moe {
        cfg.inference.moe.enabled = true;
    } else if update.disable_moe {
        cfg.inference.moe.enabled = false;
    }

    // Prefer models under the install prefix when still at the default location.
    if let Some(ref p) = prefix {
        let models = p.join("models");
        if models.is_dir() {
            let default_models = NexusConfig::default().node.models_dir;
            if cfg.node.models_dir == default_models || !cfg.node.models_dir.exists() {
                cfg.node.models_dir = models;
            }
        }
    }

    cfg.validate()?;
    cfg.save()?;
    Ok(cfg)
}

/// Locate `scripts/setup.sh` relative to env, cwd, or the running binary.
pub fn find_setup_script() -> Option<PathBuf> {
    let candidates = [
        env::var_os("NEXUS_ROOT").map(PathBuf::from),
        env::current_dir().ok(),
        env::current_exe()
            .ok()
            .and_then(|p| p.parent().map(|d| d.to_path_buf())),
        env::current_exe().ok().and_then(|p| {
            p.parent()
                .and_then(|d| d.parent())
                .and_then(|d| d.parent())
                .map(|d| d.to_path_buf())
        }),
    ];

    for base in candidates.into_iter().flatten() {
        let script = base.join("scripts").join("setup.sh");
        if script.is_file() {
            return Some(script);
        }
        // When cwd is scripts/ itself
        let alt = base.join("setup.sh");
        if alt.is_file() && base.file_name().and_then(|s| s.to_str()) == Some("scripts") {
            return Some(alt);
        }
    }
    None
}

/// Exec `scripts/setup.sh` with the given arguments (non-interactive).
pub fn run_setup_script(args: &[String]) -> Result<(), SetupError> {
    let script = find_setup_script().ok_or(SetupError::ScriptNotFound)?;
    let status = Command::new("bash").arg(&script).args(args).status()?;
    if status.success() {
        Ok(())
    } else {
        Err(SetupError::ExitStatus(status.code().unwrap_or(1)))
    }
}

/// True when `path` looks like an absolute install under a nexus prefix.
pub fn is_prefix_bin(path: &Path, name: &str) -> bool {
    path.file_name().and_then(|s| s.to_str()) == Some(name)
        && path
            .parent()
            .and_then(|p| p.file_name())
            .and_then(|s| s.to_str())
            == Some("bin")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    #[test]
    fn parse_bin_path_update() {
        let args = vec![
            "--prefix".into(),
            "/tmp/nexus-prefix".into(),
            "--enable-moe".into(),
            "--llama-server".into(),
            "/tmp/nexus-prefix/bin/llama-server".into(),
        ];
        let u = BinPathUpdate::parse(&args).unwrap();
        assert_eq!(u.prefix.unwrap(), PathBuf::from("/tmp/nexus-prefix"));
        assert!(u.enable_moe);
        assert_eq!(
            u.llama_server.unwrap(),
            PathBuf::from("/tmp/nexus-prefix/bin/llama-server")
        );
    }

    #[test]
    fn apply_bin_paths_writes_config() {
        let dir = tempdir().unwrap();
        let prefix = dir.path().join("prefix");
        fs::create_dir_all(prefix.join("bin")).unwrap();
        fs::create_dir_all(prefix.join("models")).unwrap();
        fs::write(prefix.join("bin/llama-server"), b"x").unwrap();
        fs::write(prefix.join("bin/rpc-server"), b"x").unwrap();
        fs::write(prefix.join("bin/bmoe-cli"), b"x").unwrap();

        let cfg_path = dir.path().join("config.toml");
        std::env::set_var("NEXUS_CONFIG", &cfg_path);
        std::env::set_var("HOME", dir.path());

        let update = BinPathUpdate {
            prefix: Some(prefix.clone()),
            llama_server: None,
            rpc_server: None,
            bmoe_cli: None,
            enable_moe: true,
            disable_moe: false,
        };
        let cfg = apply_bin_paths(&update).unwrap();
        assert!(cfg.node.llama_server_binary.ends_with("bin/llama-server"));
        assert!(cfg.node.rpc_server_binary.ends_with("bin/rpc-server"));
        assert!(cfg.inference.moe.bmoe_binary.ends_with("bin/bmoe-cli"));
        assert!(cfg.inference.moe.enabled);
        assert!(cfg_path.is_file());

        std::env::remove_var("NEXUS_CONFIG");
    }

    #[test]
    fn is_prefix_bin_helper() {
        assert!(is_prefix_bin(
            Path::new("/home/u/.nexus/bin/llama-server"),
            "llama-server"
        ));
        assert!(!is_prefix_bin(
            Path::new("/usr/bin/llama-server"),
            "rpc-server"
        ));
    }
}
