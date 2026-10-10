//! Property-based adversarial testing for the GGUF header parser.
//!
//! Ensures GgufFile::read never panics, loops infinitely, or allocates unbounded
//! memory when consuming malformed or adversarial byte sequences from untrusted sources.

use nexus::gguf::{GgufError, GgufMetadata, GGUF_MAGIC};
use proptest::prelude::*;
use std::io::Cursor;

proptest! {
    /// 1. Arbitrary random byte streams must cleanly return Ok or Err, never panic.
    #[test]
    fn test_gguf_never_panics_on_arbitrary_bytes(
        bytes in prop::collection::vec(any::<u8>(), 0..2048)
    ) {
        let mut cursor = Cursor::new(bytes);
        let _ = GgufMetadata::read(&mut cursor);
    }

    /// 2. Valid magic header with arbitrary versions and random counts must never panic.
    #[test]
    fn test_gguf_valid_magic_hostile_metadata_never_panics(
        version in 0u32..=10u32,
        tensor_count in any::<u64>(),
        kv_count in any::<u64>(),
        payload in prop::collection::vec(any::<u8>(), 0..1024)
    ) {
        let mut buf = Vec::with_capacity(4 + 4 + 8 + 8 + payload.len());
        buf.extend_from_slice(&GGUF_MAGIC.to_le_bytes());
        buf.extend_from_slice(&version.to_le_bytes());
        buf.extend_from_slice(&tensor_count.to_le_bytes());
        buf.extend_from_slice(&kv_count.to_le_bytes());
        buf.extend_from_slice(&payload);

        let mut cursor = Cursor::new(buf);
        let _ = GgufMetadata::read(&mut cursor);
    }

    /// 3. Extreme length headers (e.g. u64::MAX) must be rejected boundedly without OOM.
    #[test]
    fn test_gguf_huge_counts_rejected_without_oom(
        huge_kv in 100_001u64..=u64::MAX,
        huge_tensors in 1_000_001u64..=u64::MAX,
    ) {
        let mut buf = Vec::new();
        buf.extend_from_slice(&GGUF_MAGIC.to_le_bytes());
        buf.extend_from_slice(&3u32.to_le_bytes()); // Version 3
        buf.extend_from_slice(&huge_tensors.to_le_bytes());
        buf.extend_from_slice(&huge_kv.to_le_bytes());

        let mut cursor = Cursor::new(buf);
        let result = GgufMetadata::read(&mut cursor);
        assert!(matches!(result, Err(GgufError::InvalidLength(_))));
    }

    /// 4. Hostile UTF-8 sequences in strings must return InvalidUtf8 or UnexpectedEof, never panic.
    #[test]
    fn test_gguf_invalid_utf8_strings_rejected(
        invalid_bytes in prop::collection::vec(0x80u8..=0xFFu8, 1..64)
    ) {
        let mut buf = Vec::new();
        buf.extend_from_slice(&GGUF_MAGIC.to_le_bytes());
        buf.extend_from_slice(&3u32.to_le_bytes());
        buf.extend_from_slice(&0u64.to_le_bytes()); // 0 tensors
        buf.extend_from_slice(&1u64.to_le_bytes()); // 1 KV entry
        buf.extend_from_slice(&(invalid_bytes.len() as u64).to_le_bytes());
        buf.extend_from_slice(&invalid_bytes);

        let mut cursor = Cursor::new(buf);
        let result = GgufMetadata::read(&mut cursor);
        assert!(result.is_err());
    }
}
