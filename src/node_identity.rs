//! Ed25519 node identity persisted at `~/.nexus/node.key`.

use ed25519_dalek::{Signer, SigningKey, VerifyingKey};
use rand::rand_core::UnwrapErr;
use rand::rngs::SysRng;
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
        let signing_key = SigningKey::generate(&mut UnwrapErr(SysRng));
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

const HEX_CHARS: &[u8; 16] = b"0123456789abcdef";

pub fn hex_encode(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for &b in bytes {
        out.push(HEX_CHARS[(b >> 4) as usize] as char);
        out.push(HEX_CHARS[(b & 0x0f) as usize] as char);
    }
    out
}

pub fn hex_decode(hex: &str) -> Result<Vec<u8>, String> {
    let hex = hex.trim();
    if !hex.len().is_multiple_of(2) {
        return Err("hex length must be even".into());
    }
    let mut bytes = Vec::with_capacity(hex.len() / 2);
    let raw = hex.as_bytes();
    for chunk in raw.as_chunks::<2>().0 {
        let hi = hex_nibble(chunk[0])
            .ok_or_else(|| format!("invalid hex char: {}", chunk[0] as char))?;
        let lo = hex_nibble(chunk[1])
            .ok_or_else(|| format!("invalid hex char: {}", chunk[1] as char))?;
        bytes.push((hi << 4) | lo);
    }
    Ok(bytes)
}

#[inline]
fn hex_nibble(c: u8) -> Option<u8> {
    match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        b'A'..=b'F' => Some(c - b'A' + 10),
        _ => None,
    }
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

#[inline]
pub fn constant_time_eq_6(a: &[u8], b: &[u8]) -> bool {
    if a.len() != 6 || b.len() != 6 {
        return false;
    }
    let mut diff = 0u8;
    for i in 0..6 {
        diff |= a[i] ^ b[i];
    }
    diff == 0
}

pub fn verify_pairing_code(signing_key: &SigningKey, unix_secs: u64, code: &str) -> bool {
    let normalized = code.trim();
    if normalized.len() != 6 || !normalized.chars().all(|c| c.is_ascii_digit()) {
        return false;
    }
    let current = pairing_code(signing_key, unix_secs);
    let prev_window_secs = unix_secs.saturating_sub(PAIRING_CODE_WINDOW_SECS);
    let prev = pairing_code(signing_key, prev_window_secs);

    constant_time_eq_6(normalized.as_bytes(), current.as_bytes())
        || constant_time_eq_6(normalized.as_bytes(), prev.as_bytes())
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
    fn test_hex_roundtrip() {
        let data = b"hello world 1234567890!@#$";
        let hex = hex_encode(data);
        assert_eq!(hex_decode(&hex).unwrap(), data);
        assert!(hex_decode("invalid_hex").is_err());
        assert!(hex_decode("123").is_err());
    }

    #[test]
    fn test_constant_time_eq_6() {
        assert!(constant_time_eq_6(b"123456", b"123456"));
        assert!(!constant_time_eq_6(b"123456", b"123457"));
        assert!(!constant_time_eq_6(b"123456", b"abcdef"));
        assert!(!constant_time_eq_6(b"12345", b"123456"));
    }
}
