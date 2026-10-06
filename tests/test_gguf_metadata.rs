use nexus::client::ChatMessage;
use nexus::downloader::ModelDownloader;
use nexus::gguf::{GgufError, GgufMetadata, GGUF_MAGIC};
use nexus::preset::{ChatTemplate, Preset};
use std::io::{Cursor, Write};
use tempfile::NamedTempFile;

/// Helper function to build a synthetic in-memory GGUF v3 file
fn build_synthetic_gguf() -> Vec<u8> {
    let mut buf = Vec::new();

    // 1. Magic "GGUF" (0x46554747 in LE)
    buf.extend_from_slice(&GGUF_MAGIC.to_le_bytes());

    // 2. Version 3 (u32)
    buf.extend_from_slice(&3u32.to_le_bytes());

    // 3. Tensor count: 128 (u64)
    buf.extend_from_slice(&128u64.to_le_bytes());

    // 4. Metadata KV count: 6 (u64)
    buf.extend_from_slice(&6u64.to_le_bytes());

    // KV 1: "general.architecture" -> String "llama"
    write_gguf_string(&mut buf, "general.architecture");
    buf.extend_from_slice(&8u32.to_le_bytes()); // Type 8 = String
    write_gguf_string(&mut buf, "llama");

    // KV 2: "general.name" -> String "Llama-3-8B-Instruct"
    write_gguf_string(&mut buf, "general.name");
    buf.extend_from_slice(&8u32.to_le_bytes());
    write_gguf_string(&mut buf, "Llama-3-8B-Instruct");

    // KV 3: "llama.context_length" -> UInt32 8192
    write_gguf_string(&mut buf, "llama.context_length");
    buf.extend_from_slice(&4u32.to_le_bytes()); // Type 4 = UInt32
    buf.extend_from_slice(&8192u32.to_le_bytes());

    // KV 4: "llama.block_count" -> UInt32 32 (32 layers)
    write_gguf_string(&mut buf, "llama.block_count");
    buf.extend_from_slice(&4u32.to_le_bytes());
    buf.extend_from_slice(&32u32.to_le_bytes());

    // KV 5: "llama.attention.head_count" -> UInt32 32
    write_gguf_string(&mut buf, "llama.attention.head_count");
    buf.extend_from_slice(&4u32.to_le_bytes());
    buf.extend_from_slice(&32u32.to_le_bytes());

    // KV 6: "llama.embedding_length" -> UInt32 4096
    write_gguf_string(&mut buf, "llama.embedding_length");
    buf.extend_from_slice(&4u32.to_le_bytes());
    buf.extend_from_slice(&4096u32.to_le_bytes());

    buf
}

fn write_gguf_string(buf: &mut Vec<u8>, s: &str) {
    buf.extend_from_slice(&(s.len() as u64).to_le_bytes());
    buf.extend_from_slice(s.as_bytes());
}

#[test]
fn test_gguf_synthetic_v3_header_parsing() {
    let bytes = build_synthetic_gguf();
    let mut cursor = Cursor::new(bytes);

    let meta = GgufMetadata::read(&mut cursor).expect("Failed to parse synthetic GGUF");

    assert_eq!(meta.version, 3);
    assert_eq!(meta.tensor_count, 128);
    assert_eq!(meta.kv_count, 6);
    assert_eq!(meta.architecture.as_deref(), Some("llama"));
    assert_eq!(meta.model_name.as_deref(), Some("Llama-3-8B-Instruct"));
    assert_eq!(meta.context_length, Some(8192));
    assert_eq!(meta.block_count, Some(32));
    assert_eq!(meta.head_count, Some(32));
    assert_eq!(meta.embedding_length, Some(4096));
}

#[test]
fn test_exact_kv_cache_calculation() {
    let bytes = build_synthetic_gguf();
    let mut cursor = Cursor::new(bytes);
    let meta = GgufMetadata::read(&mut cursor).expect("Failed to parse synthetic GGUF");

    // Layers = 32, Head count = 32, Embd = 4096 -> Head dim = 128.
    // Bytes per token = 2 (K+V) * 32 layers * 32 heads * 128 dim * 2 bytes = 524,288 bytes/token.
    // Context = 4096 tokens: 4096 * 524288 = 2,147,483,648 bytes (exactly 2048 MB = 2 GB).
    let kv_bytes = meta.exact_kv_cache_bytes(4096);
    assert_eq!(kv_bytes, 2_147_483_648);
    assert_eq!(kv_bytes / (1024 * 1024), 2048);
}

#[test]
fn test_gguf_corruption_rejection() {
    let mut bytes = build_synthetic_gguf();

    // 1. Corrupt magic signature
    bytes[0] = 0xFF;
    let mut cursor = Cursor::new(bytes.clone());
    match GgufMetadata::read(&mut cursor) {
        Err(GgufError::InvalidMagic(_)) => (),
        other => panic!("Expected InvalidMagic error, got {:?}", other),
    }

    // 2. Corrupt version
    bytes[0] = 0x47; // Restore magic
    bytes[4] = 99;   // Version 99
    let mut cursor = Cursor::new(bytes);
    match GgufMetadata::read(&mut cursor) {
        Err(GgufError::UnsupportedVersion(99)) => (),
        other => panic!("Expected UnsupportedVersion error, got {:?}", other),
    }

    // 3. Truncated header
    let short_bytes = vec![0x47, 0x47];
    let mut cursor = Cursor::new(short_bytes);
    assert!(GgufMetadata::read(&mut cursor).is_err());
}

#[test]
fn test_preset_loading_from_file() {
    let coder = Preset::load_from_file("presets/coder.yaml").expect("Failed to load coder.yaml");
    assert_eq!(coder.name, "coder");
    assert_eq!(coder.template, ChatTemplate::ChatML);
    assert_eq!(coder.temperature, 0.2);
    assert_eq!(coder.top_p, 0.95);
    assert!(coder.system_prompt.contains("systems programmer"));

    let general = Preset::load_from_file("presets/general.yaml").expect("Failed to load general.yaml");
    assert_eq!(general.name, "general");
    assert_eq!(general.template, ChatTemplate::Llama3);
    assert_eq!(general.temperature, 0.7);
    assert!(general.system_prompt.contains("Nexus"));
}

#[test]
fn test_preset_list_names_includes_builtins() {
    use std::path::Path;
    let names = Preset::list_names(Path::new("/tmp/nonexistent-nexus-presets-dir"));
    assert!(names.contains(&"coder".to_string()));
    assert!(names.contains(&"general".to_string()));

    let from_repo = Preset::list_names(Path::new("presets"));
    assert!(from_repo.contains(&"coder".to_string()));
    assert!(from_repo.contains(&"general".to_string()));
}

#[test]
fn test_preset_chatml_formatting() {
    let preset = Preset::coder();
    let messages = vec![
        ChatMessage::user("How do I avoid AVX on Core 2 Duo?"),
        ChatMessage::assistant("Target SSE4.1 and disable AVX in rustflags."),
        ChatMessage::user("Can you show the Cargo config?"),
    ];

    let formatted = preset.format_prompt(&messages);

    assert!(formatted.contains("<|im_start|>system\nYou are an expert systems programmer"));
    assert!(formatted.contains("<|im_start|>user\nHow do I avoid AVX on Core 2 Duo?<|im_end|>"));
    assert!(formatted.contains("<|im_start|>assistant\nTarget SSE4.1 and disable AVX in rustflags.<|im_end|>"));
    assert!(formatted.ends_with("<|im_start|>assistant\n"));
}

#[test]
fn test_preset_llama3_formatting() {
    let preset = Preset::general();
    let messages = vec![
        ChatMessage::user("What is Nexus-LLM?"),
    ];

    let formatted = preset.format_prompt(&messages);

    assert!(formatted.contains("<|start_header_id|>system<|end_header_id|>\n\nYou are Nexus"));
    assert!(formatted.contains("<|start_header_id|>user<|end_header_id|>\n\nWhat is Nexus-LLM?<|eot_id|>"));
    assert!(formatted.ends_with("<|start_header_id|>assistant<|end_header_id|>\n\n"));
}

#[test]
fn test_downloader_sha256_calculation() {
    let mut temp = NamedTempFile::new().expect("Failed to create tempfile");
    let content = b"Nexus-LLM SHA-256 validation test content";
    temp.write_all(content).expect("Failed to write test content");

    let hash = ModelDownloader::calculate_sha256(temp.path()).expect("Failed to compute SHA-256");

    // Pre-computed sha256sum of "Nexus-LLM SHA-256 validation test content":
    // 04d1c3dd6ba33b3a728b7fe17c7d42cf38a0c2dd3866dc64197e4e138a4b67e0
    use sha2::{Digest, Sha256};
    let mut expected_hasher = Sha256::new();
    expected_hasher.update(content);
    let expected = format!("{:x}", expected_hasher.finalize());

    assert_eq!(hash, expected);
}
