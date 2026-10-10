//! Signed control-plane request authentication (replay protection).

use crate::config::SecurityConfig;
use crate::node_identity::{
    hex_decode, hex_encode, parse_public_key_hex, verify_signature, NodeIdentity,
};
use ed25519_dalek::SigningKey;
use http::HeaderMap;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use thiserror::Error;
use uuid::Uuid;

pub const CONTROL_AUTH_VERSION: &str = "nexus-control-v1";
pub const TIMESTAMP_TOLERANCE_SECS: i64 = 120;
pub const NONCE_TTL: Duration = Duration::from_secs(600);
pub const MAX_NONCE_ENTRIES: usize = 10_000;

pub const HDR_TIMESTAMP: &str = "x-nexus-timestamp";
pub const HDR_NONCE: &str = "x-nexus-nonce";
pub const HDR_SIGNER: &str = "x-nexus-signer-id";
pub const HDR_SIGNATURE: &str = "x-nexus-signature";

#[derive(Debug, Error, PartialEq, Eq)]
pub enum AuthError {
    #[error("missing auth header: {0}")]
    MissingHeader(String),
    #[error("invalid auth header: {0}")]
    InvalidHeader(String),
    #[error("timestamp outside allowed window")]
    StaleTimestamp,
    #[error("replay detected: nonce already used")]
    ReplayNonce,
    #[error("signature verification failed")]
    BadSignature,
    #[error("signer not authorized")]
    SignerNotAuthorized,
    #[error("pairing required")]
    PairingRequired,
}

#[derive(Default)]
pub struct NonceCache {
    entries: HashMap<String, Instant>,
}

impl NonceCache {
    pub fn insert(&mut self, nonce: &str) -> Result<(), AuthError> {
        self.evict_expired();
        if self.entries.contains_key(nonce) {
            return Err(AuthError::ReplayNonce);
        }
        if self.entries.len() >= MAX_NONCE_ENTRIES {
            self.evict_oldest();
        }
        self.entries
            .insert(nonce.to_string(), Instant::now() + NONCE_TTL);
        Ok(())
    }

    fn evict_expired(&mut self) {
        let now = Instant::now();
        self.entries.retain(|_, expiry| *expiry > now);
    }

    fn evict_oldest(&mut self) {
        if let Some(key) = self
            .entries
            .iter()
            .min_by_key(|(_, expiry)| *expiry)
            .map(|(k, _)| k.clone())
        {
            self.entries.remove(&key);
        }
    }
}

pub fn pairing_enforced(security: &SecurityConfig) -> bool {
    security.pairing_enforced()
}

pub fn is_loopback(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => v4.is_loopback(),
        IpAddr::V6(v6) => v6.is_loopback(),
    }
}

pub fn canonical_signing_message(
    method: &str,
    path: &str,
    body: &[u8],
    timestamp: i64,
    nonce: &str,
    signer_id: Uuid,
) -> String {
    let body_hash = hex_encode(&Sha256::digest(body));
    format!(
        "{CONTROL_AUTH_VERSION}\n{method}\n{path}\n{body_hash}\n{timestamp}\n{nonce}\n{signer_id}"
    )
}

pub fn unix_timestamp_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

pub fn fresh_nonce() -> String {
    hex_encode(&rand::random::<[u8; 16]>())
}

pub fn sign_request(
    identity: &NodeIdentity,
    signer_id: Uuid,
    method: &str,
    path: &str,
    body: &[u8],
    timestamp: i64,
    nonce: &str,
) -> String {
    let message = canonical_signing_message(method, path, body, timestamp, nonce, signer_id);
    hex_encode(&identity.sign(message.as_bytes()))
}

pub fn apply_auth_headers(
    builder: reqwest::RequestBuilder,
    identity: &NodeIdentity,
    signer_id: Uuid,
    method: &str,
    path: &str,
    body: &[u8],
) -> reqwest::RequestBuilder {
    let timestamp = unix_timestamp_now();
    let nonce = fresh_nonce();
    let signature = sign_request(identity, signer_id, method, path, body, timestamp, &nonce);
    builder
        .header(HDR_TIMESTAMP, timestamp.to_string())
        .header(HDR_NONCE, nonce)
        .header(HDR_SIGNER, signer_id.to_string())
        .header(HDR_SIGNATURE, signature)
}

#[derive(Debug, Clone)]
pub struct AuthHeaders {
    pub timestamp: i64,
    pub nonce: String,
    pub signer_id: Uuid,
    pub signature: [u8; 64],
}

pub fn parse_auth_headers(headers: &HeaderMap) -> Result<AuthHeaders, AuthError> {
    let timestamp = header_str(headers, HDR_TIMESTAMP)?
        .parse::<i64>()
        .map_err(|_| AuthError::InvalidHeader(HDR_TIMESTAMP.to_string()))?;
    let nonce = header_str(headers, HDR_NONCE)?.to_string();
    if nonce.len() < 16 {
        return Err(AuthError::InvalidHeader(HDR_NONCE.to_string()));
    }
    let signer_id = Uuid::parse_str(header_str(headers, HDR_SIGNER)?)
        .map_err(|_| AuthError::InvalidHeader(HDR_SIGNER.to_string()))?;
    let sig_hex = header_str(headers, HDR_SIGNATURE)?;
    let sig_bytes =
        hex_decode(sig_hex).map_err(|_| AuthError::InvalidHeader(HDR_SIGNATURE.to_string()))?;
    if sig_bytes.len() != 64 {
        return Err(AuthError::InvalidHeader(HDR_SIGNATURE.to_string()));
    }
    let mut signature = [0u8; 64];
    signature.copy_from_slice(&sig_bytes);
    Ok(AuthHeaders {
        timestamp,
        nonce,
        signer_id,
        signature,
    })
}

fn header_str<'a>(headers: &'a HeaderMap, name: &str) -> Result<&'a str, AuthError> {
    headers
        .get(name)
        .and_then(|v| v.to_str().ok())
        .ok_or_else(|| AuthError::MissingHeader(name.to_string()))
}

pub fn verify_timestamp(timestamp: i64, now: i64) -> Result<(), AuthError> {
    if (timestamp - now).abs() > TIMESTAMP_TOLERANCE_SECS {
        return Err(AuthError::StaleTimestamp);
    }
    Ok(())
}

pub fn lookup_signer_pubkey(
    security: &SecurityConfig,
    signer_id: Uuid,
    body_requester_pubkey_hex: Option<&str>,
) -> Result<[u8; 32], AuthError> {
    if let Some(hex) = security.public_key_for(signer_id) {
        return parse_public_key_hex(hex).map_err(|_| AuthError::BadSignature);
    }
    if let Some(hex) = body_requester_pubkey_hex {
        return parse_public_key_hex(hex).map_err(|_| AuthError::BadSignature);
    }
    Err(AuthError::SignerNotAuthorized)
}

/// Verifies canonical request signature, timestamp freshness, and nonce replay against security policy.
#[allow(clippy::too_many_arguments)]
pub fn verify_control_request(
    headers: &HeaderMap,
    method: &str,
    path: &str,
    body: &[u8],
    security: &SecurityConfig,
    nonce_cache: &Mutex<NonceCache>,
    body_requester_pubkey_hex: Option<&str>,
    allow_unsigned_localhost: bool,
    peer_ip: Option<IpAddr>,
) -> Result<AuthHeaders, AuthError> {
    if allow_unsigned_localhost && peer_ip.is_some_and(is_loopback) {
        return Ok(AuthHeaders {
            timestamp: unix_timestamp_now(),
            nonce: String::new(),
            signer_id: Uuid::nil(),
            signature: [0u8; 64],
        });
    }

    if !headers.contains_key(HDR_SIGNATURE) {
        if pairing_enforced(security) {
            return Err(AuthError::PairingRequired);
        }
        return Ok(AuthHeaders {
            timestamp: unix_timestamp_now(),
            nonce: String::new(),
            signer_id: Uuid::nil(),
            signature: [0u8; 64],
        });
    }

    let auth = parse_auth_headers(headers)?;
    verify_timestamp(auth.timestamp, unix_timestamp_now())?;
    {
        let mut cache = match nonce_cache.lock() {
            Ok(c) => c,
            Err(poisoned) => poisoned.into_inner(),
        };
        cache.insert(&auth.nonce)?;
    }
    let pubkey = lookup_signer_pubkey(security, auth.signer_id, body_requester_pubkey_hex)?;
    let message = canonical_signing_message(
        method,
        path,
        body,
        auth.timestamp,
        &auth.nonce,
        auth.signer_id,
    );
    if !verify_signature(&pubkey, message.as_bytes(), &auth.signature) {
        return Err(AuthError::BadSignature);
    }
    Ok(auth)
}

pub fn authorize_privileged_signer(
    security: &SecurityConfig,
    signer_id: Uuid,
) -> Result<(), AuthError> {
    if !pairing_enforced(security) {
        return Ok(());
    }
    if signer_id.is_nil() {
        return Err(AuthError::PairingRequired);
    }
    if security.allowed_peer_ids.contains(&signer_id) {
        Ok(())
    } else {
        Err(AuthError::SignerNotAuthorized)
    }
}

pub struct TrustBootstrap {
    pub config: Arc<std::sync::RwLock<crate::config::NexusConfig>>,
    pub config_path: std::path::PathBuf,
    pub identity: Arc<NodeIdentity>,
}

impl TrustBootstrap {
    pub fn load(
        mut config: crate::config::NexusConfig,
    ) -> Result<Self, crate::config::ConfigError> {
        let config_path = if let Ok(custom) = std::env::var("NEXUS_CONFIG") {
            std::path::PathBuf::from(custom)
        } else {
            crate::config::NexusConfig::default_config_path()
        };
        config.ensure_identity()?;
        config.load_node_identity()?;
        let identity = Arc::new(
            NodeIdentity::load_or_create(None)
                .map_err(|e| crate::config::ConfigError::Invalid(e.to_string()))?,
        );
        Ok(Self {
            config: Arc::new(std::sync::RwLock::new(config)),
            config_path,
            identity,
        })
    }
}

pub fn current_pairing_code(signing_key: &SigningKey) -> (String, u64) {
    let now = unix_timestamp_now() as u64;
    let code = crate::node_identity::pairing_code(signing_key, now);
    let remaining = crate::node_identity::pairing_code_remaining_secs(now);
    (code, remaining)
}
