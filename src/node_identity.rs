//! Ed25519 node identity persisted at `~/.nexus/node.key`.

use ed25519_dalek::{Signer, SigningKey, VerifyingKey};
use rand::rngs::OsRng;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use thiserror::Error;
use uuid::Uuid;

/// Fixed namespace for deriving stable `node.id` from an Ed25519 public key.
pub const NODE_ID_NAMESPACE: Uuid = Uuid::from_u128(0x6e657875_73000000_00000000_00000001);

const KEY_FILE_NAME: &str = "node.key";
const SECRET_LEN: usize = 32;

#[derive(Error, Debug)]
pub enum IdentityError {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("Invalid node key file: {0}")]
    InvalidKey(String),
}

#[derive(Clone)]
pub struct NodeIdentity {
    signing_key: SigningKey,
}

impl NodeIdentity {
    pub fn generate() -> Self {
        let signing_key = SigningKey::generate(&mut OsRng);
        Self { signing_key }
    }

    pub fn from_signing_key(signing_key: SigningKey) -> Self {
        Self { signing_key }
    }

    pub fn verifying_key(&self) -> VerifyingKey {
        self.signing_key.verifying_key()
    }

    pub fn public_key_bytes(&self) -> [u8; 32] {
        self.verifying_key().to_bytes()
    }

    pub fn public_key_hex(&self) -> String {
        hex_encode(&self.public_key_bytes())
    }

    pub fn node_id_from_public_key(&self) -> Uuid {
        Uuid::new_v5(&NODE_ID_NAMESPACE, &self.public_key_bytes())
    }

    pub fn sign(&self, message: &[u8]) -> [u8; 64] {
        self.signing_key.sign(message).to_bytes()
    }

    pub fn current_pairing_code(&self, unix_secs: u64) -> String {
        pairing_code(&self.signing_key, unix_secs)
    }

    pub fn verify_pairing_code_at(&self, unix_secs: u64, code: &str) -> bool {
        verify_pairing_code(&self.signing_key, unix_secs, code)
    }

    pub fn default_key_path() -> PathBuf {
        let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
        PathBuf::from(home).join(".nexus").join(KEY_FILE_NAME)
    }

    pub fn load_or_create(path: Option<&Path>) -> Result<Self, IdentityError> {
        let path = path
            .map(Path::to_path_buf)
            .unwrap_or_else(Self::default_key_path);
        if path.exists() {
            return Self::load_from_path(&path);
        }
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let identity = Self::generate();
        identity.save_to_path(&path)?;
        Ok(identity)
    }

    pub fn load_from_path(path: &Path) -> Result<Self, IdentityError> {
        let bytes = fs::read(path)?;
        if bytes.len() != SECRET_LEN {
            return Err(IdentityError::InvalidKey(format!(
                "expected {} byte secret, got {}",
                SECRET_LEN,
                bytes.len()
            )));
        }
        let mut secret = [0u8; SECRET_LEN];
        secret.copy_from_slice(&bytes);
        let signing_key = SigningKey::from_bytes(&secret);
        Ok(Self { signing_key })
    }

    pub fn save_to_path(&self, path: &Path) -> Result<(), IdentityError> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            let mut file = OpenOptions::new()
                .create(true)
                .write(true)
                .truncate(true)
                .mode(0o600)
                .open(path)?;
            file.write_all(&self.signing_key.to_bytes())?;
            Ok(())
        }
        #[cfg(not(unix))]
        {
            fs::write(path, self.signing_key.to_bytes())?;
            Ok(())
        }
    }
}

pub fn verify_signature(public_key: &[u8; 32], message: &[u8], signature: &[u8; 64]) -> bool {
    let Ok(verifying) = VerifyingKey::from_bytes(public_key) else {
        return false;
    };
    let sig = ed25519_dalek::Signature::from_bytes(signature);
    verifying.verify_strict(message, &sig).is_ok()
}

pub fn parse_public_key_hex(hex: &str) -> Result<[u8; 32], IdentityError> {
    let bytes = hex_decode(hex.trim()).map_err(IdentityError::InvalidKey)?;
    if bytes.len() != 32 {
        return Err(IdentityError::InvalidKey(format!(
            "public key must be 32 bytes, got {}",
            bytes.len()
        )));
    }
    let mut out = [0u8; 32];
    out.copy_from_slice(&bytes);
    Ok(out)
}

pub fn hex_encode(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn hex_decode(hex: &str) -> Result<Vec<u8>, String> {
    if !hex.len().is_multiple_of(2) {
        return Err("hex length must be even".into());
    }
    (0..hex.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).map_err(|e| e.to_string()))
        .collect()
}

/// Six-digit pairing code rotating every `PAIRING_CODE_WINDOW_SECS`.
pub const PAIRING_CODE_WINDOW_SECS: u64 = 300;

pub fn pairing_code(signing_key: &SigningKey, unix_secs: u64) -> String {
    use hmac::{Hmac, Mac};
    use sha2::Sha256;
    type HmacSha256 = Hmac<Sha256>;
    let window = unix_secs / PAIRING_CODE_WINDOW_SECS;
    let mut mac =
        HmacSha256::new_from_slice(&signing_key.to_bytes()).expect("HMAC accepts 32-byte key");
    mac.update(b"nexus-pair-v1");
    mac.update(&window.to_be_bytes());
    let digest = mac.finalize().into_bytes();
    let value = u32::from_be_bytes([digest[0], digest[1], digest[2], digest[3]]) % 1_000_000;
    format!("{value:06}")
}

pub fn pairing_code_remaining_secs(unix_secs: u64) -> u64 {
    PAIRING_CODE_WINDOW_SECS - (unix_secs % PAIRING_CODE_WINDOW_SECS)
}

pub fn verify_pairing_code(signing_key: &SigningKey, unix_secs: u64, code: &str) -> bool {
    let normalized = code.trim();
    if normalized.len() != 6 || !normalized.chars().all(|c| c.is_ascii_digit()) {
        return false;
    }
    let current = pairing_code(signing_key, unix_secs);
    if normalized == current {
        return true;
    }
    // Allow previous window during rotation grace (±1 window).
    let prev_window_secs = unix_secs.saturating_sub(PAIRING_CODE_WINDOW_SECS);
    pairing_code(signing_key, prev_window_secs) == normalized
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn derived_node_id_is_stable() {
        let id1 = NodeIdentity::generate();
        let id2 = id1.clone();
        assert_eq!(id1.node_id_from_public_key(), id2.node_id_from_public_key());
    }

    #[test]
    fn pairing_code_is_six_digits_and_rotates() {
        let id = NodeIdentity::generate();
        let code = pairing_code(&id.signing_key, 1_700_000_000);
        assert_eq!(code.len(), 6);
        assert!(code.chars().all(|c| c.is_ascii_digit()));
        assert!(verify_pairing_code(&id.signing_key, 1_700_000_000, &code));
    }
}
