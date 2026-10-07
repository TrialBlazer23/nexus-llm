use std::collections::HashMap;
use std::fs::File;
use std::io::{BufReader, Read, Seek, SeekFrom};
use std::path::Path;
use thiserror::Error;
use tracing::debug;

pub const GGUF_MAGIC: u32 = 0x46554747; // "GGUF" in little-endian (0x47, 0x47, 0x55, 0x46)

/// Absolute ceiling on metadata map/array capacity requests (bytes of length field claims).
/// Prevents hostile headers from requesting impossible heap before EOF checks run.
const MAX_ALLOC_ELEMENTS: usize = 1_048_576;

#[derive(Error, Debug)]
pub enum GgufError {
    #[error("I/O error reading GGUF file: {0}")]
    Io(#[from] std::io::Error),

    #[error("Invalid GGUF magic signature: 0x{0:08X} (expected 0x{GGUF_MAGIC:08X})")]
    InvalidMagic(u32),

    #[error("Unsupported GGUF version: {0} (supported: 2, 3)")]
    UnsupportedVersion(u32),

    #[error("Invalid UTF-8 in GGUF metadata string: {0}")]
    InvalidUtf8(#[from] std::string::FromUtf8Error),

    #[error("Unsupported GGUF value type ID: {0}")]
    UnsupportedValueType(u32),

    #[error("Unexpected EOF while parsing GGUF metadata")]
    UnexpectedEof,

    #[error("GGUF length claim exceeds remaining input or safe allocation limit ({0})")]
    InvalidLength(String),
}

/// Metadata value types stored in GGUF key-value headers.
#[derive(Debug, Clone, PartialEq)]
pub enum GgufValue {
    Uint8(u8),
    Int8(i8),
    Uint16(u16),
    Int16(i16),
    Uint32(u32),
    Int32(i32),
    Float32(f32),
    Bool(bool),
    String(String),
    Array(Vec<GgufValue>),
    Uint64(u64),
    Int64(i64),
    Float64(f64),
}

impl GgufValue {
    pub fn as_u64(&self) -> Option<u64> {
        match self {
            Self::Uint8(v) => Some(*v as u64),
            Self::Uint16(v) => Some(*v as u64),
            Self::Uint32(v) => Some(*v as u64),
            Self::Uint64(v) => Some(*v),
            Self::Int8(v) if *v >= 0 => Some(*v as u64),
            Self::Int16(v) if *v >= 0 => Some(*v as u64),
            Self::Int32(v) if *v >= 0 => Some(*v as u64),
            Self::Int64(v) if *v >= 0 => Some(*v as u64),
            _ => None,
        }
    }

    pub fn as_usize(&self) -> Option<usize> {
        self.as_u64().map(|v| v as usize)
    }

    pub fn as_str(&self) -> Option<&str> {
        match self {
            Self::String(s) => Some(s.as_str()),
            _ => None,
        }
    }
}

/// Parsed metadata extracted from a GGUF file header without loading weights into RAM.
#[derive(Debug, Clone)]
pub struct GgufMetadata {
    pub version: u32,
    pub tensor_count: u64,
    pub kv_count: u64,
    pub metadata: HashMap<String, GgufValue>,
    pub architecture: Option<String>,
    pub model_name: Option<String>,
    pub context_length: Option<usize>,
    pub block_count: Option<usize>,
    pub head_count: Option<usize>,
    pub head_count_kv: Option<usize>,
    pub embedding_length: Option<usize>,
    pub file_size_bytes: u64,
}

impl GgufMetadata {
    /// Inspect a GGUF file from disk, reading only headers and key-value metadata arrays.
    pub fn open<P: AsRef<Path>>(path: P) -> Result<Self, GgufError> {
        let file = File::open(&path)?;
        let file_size_bytes = file.metadata()?.len();
        let mut reader = BufReader::new(file);
        let mut meta = Self::read(&mut reader)?;
        meta.file_size_bytes = file_size_bytes;
        Ok(meta)
    }

    /// Read GGUF header and metadata dictionary from any reader supporting Read + Seek.
    pub fn read<R: Read + Seek>(reader: &mut R) -> Result<Self, GgufError> {
        let mut magic_buf = [0u8; 4];
        read_exact_eof(reader, &mut magic_buf)?;
        let magic = u32::from_le_bytes(magic_buf);
        if magic != GGUF_MAGIC {
            return Err(GgufError::InvalidMagic(magic));
        }

        let mut u32_buf = [0u8; 4];
        read_exact_eof(reader, &mut u32_buf)?;
        let version = u32::from_le_bytes(u32_buf);
        if version != 2 && version != 3 {
            return Err(GgufError::UnsupportedVersion(version));
        }

        let mut u64_buf = [0u8; 8];
        read_exact_eof(reader, &mut u64_buf)?;
        let tensor_count = u64::from_le_bytes(u64_buf);

        read_exact_eof(reader, &mut u64_buf)?;
        let kv_count_u64 = u64::from_le_bytes(u64_buf);

        debug!(
            "Parsing GGUF v{}: {} tensors, {} metadata KV pairs",
            version, tensor_count, kv_count_u64
        );

        // Each KV needs at least: empty key (8) + type (4) + 1-byte value = 13 bytes.
        const MIN_KV_BYTES: u64 = 13;
        let remaining = remaining_bytes(reader)?;
        let kv_count = checked_count(kv_count_u64, remaining, MIN_KV_BYTES, "kv_count")?;

        let mut metadata = HashMap::with_capacity(kv_count);

        for _ in 0..kv_count {
            let key = read_string(reader)?;

            read_exact_eof(reader, &mut u32_buf)?;
            let value_type = u32::from_le_bytes(u32_buf);

            let value = read_value(reader, value_type)?;
            metadata.insert(key, value);
        }

        let architecture = metadata
            .get("general.architecture")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());
        let model_name = metadata
            .get("general.name")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());

        let arch_prefix = architecture.as_deref().unwrap_or("llama");

        let context_length = metadata
            .get(&format!("{}.context_length", arch_prefix))
            .or_else(|| metadata.get("general.context_length"))
            .and_then(|v| v.as_usize());

        let block_count = metadata
            .get(&format!("{}.block_count", arch_prefix))
            .and_then(|v| v.as_usize());

        let head_count = metadata
            .get(&format!("{}.attention.head_count", arch_prefix))
            .and_then(|v| v.as_usize());

        let head_count_kv = metadata
            .get(&format!("{}.attention.head_count_kv", arch_prefix))
            .and_then(|v| v.as_usize())
            .or(head_count);

        let embedding_length = metadata
            .get(&format!("{}.embedding_length", arch_prefix))
            .and_then(|v| v.as_usize());

        Ok(Self {
            version,
            tensor_count,
            kv_count: kv_count_u64,
            metadata,
            architecture,
            model_name,
            context_length,
            block_count,
            head_count,
            head_count_kv,
            embedding_length,
            file_size_bytes: 0,
        })
    }

    /// Calculate exact KV cache memory requirement in bytes for the model at a given context size.
    ///
    /// Formula (FP16 precision):
    ///   head_dim = embedding_length / head_count
    ///   kv_bytes_per_token = 2 (K & V) * n_layers * n_kv_heads * head_dim * 2 bytes (FP16)
    ///   total_kv_bytes = kv_bytes_per_token * context_size
    pub fn exact_kv_cache_bytes(&self, context_size: usize) -> u64 {
        if let (Some(layers), Some(head_count), Some(head_count_kv), Some(embd)) = (
            self.block_count,
            self.head_count,
            self.head_count_kv,
            self.embedding_length,
        ) {
            if let Some(head_dim) = embd.checked_div(head_count) {
                // 2 (K + V) * layers * kv_heads * head_dim * 2 bytes (f16)
                let bytes_per_token =
                    2 * (layers as u64) * (head_count_kv as u64) * (head_dim as u64) * 2;
                return bytes_per_token.saturating_mul(context_size as u64);
            }
        }

        // Fallback heuristic: 200 KB per context token
        (context_size as u64).saturating_mul(200 * 1024)
    }
}

fn remaining_bytes<R: Seek>(reader: &mut R) -> Result<u64, GgufError> {
    let pos = reader.stream_position()?;
    let end = reader.seek(SeekFrom::End(0))?;
    reader.seek(SeekFrom::Start(pos))?;
    Ok(end.saturating_sub(pos))
}

fn checked_count(
    claimed: u64,
    remaining: u64,
    min_bytes_per: u64,
    label: &str,
) -> Result<usize, GgufError> {
    if claimed > MAX_ALLOC_ELEMENTS as u64 {
        return Err(GgufError::InvalidLength(format!(
            "{label}={claimed} exceeds max {MAX_ALLOC_ELEMENTS}"
        )));
    }
    let needed = claimed.saturating_mul(min_bytes_per);
    if needed > remaining {
        return Err(GgufError::InvalidLength(format!(
            "{label}={claimed} needs at least {needed} bytes, only {remaining} remain"
        )));
    }
    Ok(claimed as usize)
}

fn read_exact_eof<R: Read>(reader: &mut R, buf: &mut [u8]) -> Result<(), GgufError> {
    match reader.read_exact(buf) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => Err(GgufError::UnexpectedEof),
        Err(e) => Err(GgufError::Io(e)),
    }
}

fn read_string<R: Read + Seek>(reader: &mut R) -> Result<String, GgufError> {
    let mut len_buf = [0u8; 8];
    read_exact_eof(reader, &mut len_buf)?;
    let len_u64 = u64::from_le_bytes(len_buf);
    let remaining = remaining_bytes(reader)?;
    if len_u64 > remaining {
        return Err(GgufError::InvalidLength(format!(
            "string len={len_u64} exceeds remaining {remaining}"
        )));
    }
    if len_u64 > MAX_ALLOC_ELEMENTS as u64 {
        return Err(GgufError::InvalidLength(format!(
            "string len={len_u64} exceeds max {MAX_ALLOC_ELEMENTS}"
        )));
    }
    let len = len_u64 as usize;
    let mut str_buf = vec![0u8; len];
    read_exact_eof(reader, &mut str_buf)?;
    Ok(String::from_utf8(str_buf)?)
}

fn fixed_width_size(value_type: u32) -> Option<u64> {
    match value_type {
        0 | 1 | 7 => Some(1),
        2 | 3 => Some(2),
        4..=6 => Some(4),
        10..=12 => Some(8),
        _ => None,
    }
}

fn read_value<R: Read + Seek>(reader: &mut R, value_type: u32) -> Result<GgufValue, GgufError> {
    match value_type {
        0 => {
            let mut buf = [0u8; 1];
            read_exact_eof(reader, &mut buf)?;
            Ok(GgufValue::Uint8(buf[0]))
        }
        1 => {
            let mut buf = [0u8; 1];
            read_exact_eof(reader, &mut buf)?;
            Ok(GgufValue::Int8(buf[0] as i8))
        }
        2 => {
            let mut buf = [0u8; 2];
            read_exact_eof(reader, &mut buf)?;
            Ok(GgufValue::Uint16(u16::from_le_bytes(buf)))
        }
        3 => {
            let mut buf = [0u8; 2];
            read_exact_eof(reader, &mut buf)?;
            Ok(GgufValue::Int16(i16::from_le_bytes(buf)))
        }
        4 => {
            let mut buf = [0u8; 4];
            read_exact_eof(reader, &mut buf)?;
            Ok(GgufValue::Uint32(u32::from_le_bytes(buf)))
        }
        5 => {
            let mut buf = [0u8; 4];
            read_exact_eof(reader, &mut buf)?;
            Ok(GgufValue::Int32(i32::from_le_bytes(buf)))
        }
        6 => {
            let mut buf = [0u8; 4];
            read_exact_eof(reader, &mut buf)?;
            Ok(GgufValue::Float32(f32::from_le_bytes(buf)))
        }
        7 => {
            let mut buf = [0u8; 1];
            read_exact_eof(reader, &mut buf)?;
            Ok(GgufValue::Bool(buf[0] != 0))
        }
        8 => {
            let s = read_string(reader)?;
            Ok(GgufValue::String(s))
        }
        9 => {
            let mut u32_buf = [0u8; 4];
            read_exact_eof(reader, &mut u32_buf)?;
            let elem_type = u32::from_le_bytes(u32_buf);

            let mut u64_buf = [0u8; 8];
            read_exact_eof(reader, &mut u64_buf)?;
            let array_len_u64 = u64::from_le_bytes(u64_buf);

            let remaining = remaining_bytes(reader)?;
            let min_elem = fixed_width_size(elem_type).unwrap_or(1);
            let array_len = checked_count(array_len_u64, remaining, min_elem, "array_len")?;

            let mut arr = Vec::with_capacity(array_len);
            for _ in 0..array_len {
                arr.push(read_value(reader, elem_type)?);
            }
            Ok(GgufValue::Array(arr))
        }
        10 => {
            let mut buf = [0u8; 8];
            read_exact_eof(reader, &mut buf)?;
            Ok(GgufValue::Uint64(u64::from_le_bytes(buf)))
        }
        11 => {
            let mut buf = [0u8; 8];
            read_exact_eof(reader, &mut buf)?;
            Ok(GgufValue::Int64(i64::from_le_bytes(buf)))
        }
        12 => {
            let mut buf = [0u8; 8];
            read_exact_eof(reader, &mut buf)?;
            Ok(GgufValue::Float64(f64::from_le_bytes(buf)))
        }
        other => Err(GgufError::UnsupportedValueType(other)),
    }
}
