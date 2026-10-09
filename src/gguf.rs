use std::collections::HashMap;
use std::fs::File;
use std::io::{BufReader, Read, Seek, SeekFrom};
use std::path::Path;
use thiserror::Error;
use tracing::debug;

pub const GGUF_MAGIC: u32 = 0x46554747; // "GGUF" in little-endian

/// Soft caps to reject hostile/corrupt headers before allocating.
const MAX_KV_COUNT: u64 = 100_000;
const MAX_TENSOR_COUNT: u64 = 1_000_000;
const MAX_STRING_LEN: u64 = 16 * 1024 * 1024;
const MAX_ARRAY_LEN: u64 = 10_000_000;
const MAX_DIMS: u32 = 4;

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

    #[error("Invalid GGUF length or count: {0}")]
    InvalidLength(String),

    #[error("Unsupported ggml tensor type: {0}")]
    UnsupportedTensorType(u32),
}

/// KV cache element storage dtype used for exact cache sizing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum KvCacheDtype {
    #[default]
    F16,
    Q8_0,
    Q4_0,
}

impl KvCacheDtype {
    /// Approximate bytes per KV element (K or V scalar).
    pub fn bytes_per_elem(self) -> f64 {
        match self {
            Self::F16 => 2.0,
            Self::Q8_0 => 1.0,
            Self::Q4_0 => 0.5,
        }
    }

    pub fn as_llama_arg(self) -> &'static str {
        match self {
            Self::F16 => "f16",
            Self::Q8_0 => "q8_0",
            Self::Q4_0 => "q4_0",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s.to_ascii_lowercase().as_str() {
            "f16" | "fp16" => Some(Self::F16),
            "q8_0" | "q8" => Some(Self::Q8_0),
            "q4_0" | "q4" => Some(Self::Q4_0),
            _ => None,
        }
    }
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
    /// Array payload was seek-skipped (not materialized).
    SkippedArray {
        elem_type: u32,
        len: u64,
    },
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

/// One tensor info record from the GGUF tensor section (weights not loaded).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GgufTensorInfo {
    pub name: String,
    pub n_dims: u32,
    pub dims: [u64; 4],
    pub ggml_type: u32,
    pub offset: u64,
    pub nbytes: u64,
    /// Parsed from `blk.N.*` names; `None` for embeddings/output/etc.
    pub layer_index: Option<u32>,
}

impl GgufTensorInfo {
    pub fn is_layer_tensor(&self) -> bool {
        self.layer_index.is_some()
    }

    pub fn quant_label(&self) -> &'static str {
        ggml_type_name(self.ggml_type)
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
    /// Tensor info section (empty if parse stopped early / no tensors requested).
    pub tensors: Vec<GgufTensorInfo>,
    /// Dominant weight quantization label (modal by nbytes among weight tensors).
    pub quant_label: Option<String>,
}

impl GgufMetadata {
    /// Inspect a GGUF file from disk, reading headers, KV metadata, and tensor info.
    pub fn open<P: AsRef<Path>>(path: P) -> Result<Self, GgufError> {
        let file = File::open(&path)?;
        let file_size_bytes = file.metadata()?.len();
        let mut reader = BufReader::new(file);
        let mut meta = Self::read(&mut reader)?;
        meta.file_size_bytes = file_size_bytes;
        Ok(meta)
    }

    /// Read GGUF header, metadata dictionary, and tensor info section.
    pub fn read<R: Read + Seek>(reader: &mut R) -> Result<Self, GgufError> {
        let start = reader.stream_position().unwrap_or(0);
        let end = reader.seek(SeekFrom::End(0)).map_err(GgufError::Io)?;
        reader.seek(SeekFrom::Start(start)).map_err(GgufError::Io)?;
        let remaining_hint = end.saturating_sub(start);

        let mut magic_buf = [0u8; 4];
        read_exact_bounded(reader, &mut magic_buf)?;
        let magic = u32::from_le_bytes(magic_buf);
        if magic != GGUF_MAGIC {
            return Err(GgufError::InvalidMagic(magic));
        }

        let mut u32_buf = [0u8; 4];
        read_exact_bounded(reader, &mut u32_buf)?;
        let version = u32::from_le_bytes(u32_buf);
        if version != 2 && version != 3 {
            return Err(GgufError::UnsupportedVersion(version));
        }

        let mut u64_buf = [0u8; 8];
        read_exact_bounded(reader, &mut u64_buf)?;
        let tensor_count = u64::from_le_bytes(u64_buf);
        if tensor_count > MAX_TENSOR_COUNT {
            return Err(GgufError::InvalidLength(format!(
                "tensor_count {tensor_count} exceeds cap {MAX_TENSOR_COUNT}"
            )));
        }

        read_exact_bounded(reader, &mut u64_buf)?;
        let kv_count = u64::from_le_bytes(u64_buf);
        if kv_count > MAX_KV_COUNT {
            return Err(GgufError::InvalidLength(format!(
                "kv_count {kv_count} exceeds cap {MAX_KV_COUNT}"
            )));
        }

        debug!(
            "Parsing GGUF v{}: {} tensors, {} metadata KV pairs",
            version, tensor_count, kv_count
        );

        let mut metadata = HashMap::new();
        if kv_count > 0 {
            metadata.reserve(kv_count.min(4096) as usize);
        }

        for _ in 0..kv_count {
            let key = read_string_bounded(reader, remaining_hint)?;
            read_exact_bounded(reader, &mut u32_buf)?;
            let value_type = u32::from_le_bytes(u32_buf);
            let value = read_value_bounded(reader, value_type, remaining_hint, true)?;
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
            .get(&format!("{arch_prefix}.context_length"))
            .or_else(|| metadata.get("general.context_length"))
            .and_then(|v| v.as_usize());

        let block_count = metadata
            .get(&format!("{arch_prefix}.block_count"))
            .and_then(|v| v.as_usize());

        let head_count = metadata
            .get(&format!("{arch_prefix}.attention.head_count"))
            .and_then(|v| v.as_usize());

        let head_count_kv = metadata
            .get(&format!("{arch_prefix}.attention.head_count_kv"))
            .and_then(|v| v.as_usize())
            .or(head_count);

        let embedding_length = metadata
            .get(&format!("{arch_prefix}.embedding_length"))
            .and_then(|v| v.as_usize());

        let mut tensors = Vec::new();
        if tensor_count > 0 {
            tensors.reserve(tensor_count.min(65_536) as usize);
        }
        for _ in 0..tensor_count {
            let name = read_string_bounded(reader, remaining_hint)?;
            read_exact_bounded(reader, &mut u32_buf)?;
            let n_dims = u32::from_le_bytes(u32_buf);
            if n_dims == 0 || n_dims > MAX_DIMS {
                return Err(GgufError::InvalidLength(format!(
                    "tensor '{name}' has invalid n_dims={n_dims}"
                )));
            }
            let mut dims = [0u64; 4];
            for dim in dims.iter_mut().take(n_dims as usize) {
                read_exact_bounded(reader, &mut u64_buf)?;
                *dim = u64::from_le_bytes(u64_buf);
            }
            read_exact_bounded(reader, &mut u32_buf)?;
            let ggml_type = u32::from_le_bytes(u32_buf);
            read_exact_bounded(reader, &mut u64_buf)?;
            let offset = u64::from_le_bytes(u64_buf);
            let nbytes = tensor_nbytes(ggml_type, &dims[..n_dims as usize])?;
            let layer_index = parse_layer_index(&name);
            tensors.push(GgufTensorInfo {
                name,
                n_dims,
                dims,
                ggml_type,
                offset,
                nbytes,
                layer_index,
            });
        }

        let quant_label = dominant_quant_label(&tensors);

        Ok(Self {
            version,
            tensor_count,
            kv_count,
            metadata,
            architecture,
            model_name,
            context_length,
            block_count,
            head_count,
            head_count_kv,
            embedding_length,
            file_size_bytes: 0,
            tensors,
            quant_label,
        })
    }

    /// Total weight bytes from tensor info; falls back to `file_size_bytes` if empty.
    pub fn weights_bytes(&self) -> u64 {
        if self.tensors.is_empty() {
            return self.file_size_bytes;
        }
        self.tensors.iter().map(|t| t.nbytes).sum()
    }

    /// Bytes for non-layer tensors (embeddings, output, norms outside blk.N).
    pub fn host_fixed_weight_bytes(&self) -> u64 {
        self.tensors
            .iter()
            .filter(|t| !t.is_layer_tensor())
            .map(|t| t.nbytes)
            .sum()
    }

    /// Per-layer weight bytes indexed by layer; length == block_count when known.
    pub fn per_layer_weight_bytes(&self) -> Result<Vec<u64>, GgufError> {
        let n_layers = self.block_count.ok_or_else(|| {
            GgufError::InvalidLength("missing block_count for layer geometry".into())
        })?;
        if n_layers == 0 {
            return Err(GgufError::InvalidLength("block_count is zero".into()));
        }
        let mut layers = vec![0u64; n_layers];
        for t in &self.tensors {
            if let Some(idx) = t.layer_index {
                if (idx as usize) < n_layers {
                    layers[idx as usize] = layers[idx as usize].saturating_add(t.nbytes);
                }
            }
        }
        Ok(layers)
    }

    /// Exact KV cache bytes for FP16 (legacy helper).
    pub fn exact_kv_cache_bytes(&self, context_size: usize) -> u64 {
        self.exact_kv_cache_bytes_dtype(context_size, KvCacheDtype::F16)
    }

    /// Exact KV cache bytes for a given cache dtype.
    pub fn exact_kv_cache_bytes_dtype(&self, context_size: usize, dtype: KvCacheDtype) -> u64 {
        if let (Some(layers), Some(head_count), Some(head_count_kv), Some(embd)) = (
            self.block_count,
            self.head_count,
            self.head_count_kv,
            self.embedding_length,
        ) {
            if let Some(head_dim) = embd.checked_div(head_count) {
                let elems_per_token =
                    2.0 * (layers as f64) * (head_count_kv as f64) * (head_dim as f64);
                let bytes = elems_per_token * dtype.bytes_per_elem() * (context_size as f64);
                return bytes.round() as u64;
            }
        }
        // Last-resort heuristic only when dims are missing.
        (context_size as u64).saturating_mul(200 * 1024)
    }

    /// True when architectural dims needed for planning are present.
    pub fn has_geometry(&self) -> bool {
        self.block_count.is_some_and(|n| n > 0)
            && self.head_count.is_some_and(|n| n > 0)
            && self.embedding_length.is_some_and(|n| n > 0)
    }
}

fn dominant_quant_label(tensors: &[GgufTensorInfo]) -> Option<String> {
    if tensors.is_empty() {
        return None;
    }
    let mut by_type: HashMap<u32, u64> = HashMap::new();
    for t in tensors {
        *by_type.entry(t.ggml_type).or_insert(0) += t.nbytes;
    }
    by_type
        .into_iter()
        .max_by_key(|(_, nbytes)| *nbytes)
        .map(|(ty, _)| ggml_type_name(ty).to_string())
}

fn parse_layer_index(name: &str) -> Option<u32> {
    // Common patterns: "blk.12.attn_q.weight", "model.layers.3...."
    if let Some(rest) = name.strip_prefix("blk.") {
        let num: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
        return num.parse().ok();
    }
    if let Some(idx) = name.find("layers.") {
        let rest = &name[idx + "layers.".len()..];
        let num: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
        return num.parse().ok();
    }
    None
}

fn read_exact_bounded<R: Read>(reader: &mut R, buf: &mut [u8]) -> Result<(), GgufError> {
    match reader.read_exact(buf) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => Err(GgufError::UnexpectedEof),
        Err(e) => Err(GgufError::Io(e)),
    }
}

fn read_string_bounded<R: Read + Seek>(
    reader: &mut R,
    remaining_hint: u64,
) -> Result<String, GgufError> {
    let mut len_buf = [0u8; 8];
    read_exact_bounded(reader, &mut len_buf)?;
    let len = u64::from_le_bytes(len_buf);
    if len > MAX_STRING_LEN || (remaining_hint > 0 && len > remaining_hint) {
        return Err(GgufError::InvalidLength(format!(
            "string length {len} exceeds bound"
        )));
    }
    let mut str_buf = vec![0u8; len as usize];
    read_exact_bounded(reader, &mut str_buf)?;
    Ok(String::from_utf8(str_buf)?)
}

fn fixed_value_size(value_type: u32) -> Option<u64> {
    match value_type {
        0 | 1 | 7 => Some(1),
        2 | 3 => Some(2),
        4..=6 => Some(4),
        10..=12 => Some(8),
        _ => None,
    }
}

#[allow(clippy::only_used_in_recursion)] // remaining_hint reserved for nested array caps
fn skip_value<R: Read + Seek>(
    reader: &mut R,
    value_type: u32,
    remaining_hint: u64,
) -> Result<(), GgufError> {
    if let Some(sz) = fixed_value_size(value_type) {
        reader
            .seek(SeekFrom::Current(sz as i64))
            .map_err(GgufError::Io)?;
        return Ok(());
    }
    match value_type {
        8 => {
            let mut len_buf = [0u8; 8];
            read_exact_bounded(reader, &mut len_buf)?;
            let len = u64::from_le_bytes(len_buf);
            if len > MAX_STRING_LEN {
                return Err(GgufError::InvalidLength(format!(
                    "skip string length {len} exceeds cap"
                )));
            }
            reader
                .seek(SeekFrom::Current(len as i64))
                .map_err(GgufError::Io)?;
            Ok(())
        }
        9 => {
            let mut u32_buf = [0u8; 4];
            read_exact_bounded(reader, &mut u32_buf)?;
            let elem_type = u32::from_le_bytes(u32_buf);
            let mut u64_buf = [0u8; 8];
            read_exact_bounded(reader, &mut u64_buf)?;
            let array_len = u64::from_le_bytes(u64_buf);
            if array_len > MAX_ARRAY_LEN {
                return Err(GgufError::InvalidLength(format!(
                    "array_len {array_len} exceeds cap"
                )));
            }
            if let Some(elem_sz) = fixed_value_size(elem_type) {
                let total = elem_sz.saturating_mul(array_len);
                reader
                    .seek(SeekFrom::Current(total as i64))
                    .map_err(GgufError::Io)?;
            } else {
                for _ in 0..array_len {
                    skip_value(reader, elem_type, remaining_hint)?;
                }
            }
            Ok(())
        }
        other => Err(GgufError::UnsupportedValueType(other)),
    }
}

fn read_value_bounded<R: Read + Seek>(
    reader: &mut R,
    value_type: u32,
    remaining_hint: u64,
    materialize_arrays: bool,
) -> Result<GgufValue, GgufError> {
    match value_type {
        0 => {
            let mut buf = [0u8; 1];
            read_exact_bounded(reader, &mut buf)?;
            Ok(GgufValue::Uint8(buf[0]))
        }
        1 => {
            let mut buf = [0u8; 1];
            read_exact_bounded(reader, &mut buf)?;
            Ok(GgufValue::Int8(buf[0] as i8))
        }
        2 => {
            let mut buf = [0u8; 2];
            read_exact_bounded(reader, &mut buf)?;
            Ok(GgufValue::Uint16(u16::from_le_bytes(buf)))
        }
        3 => {
            let mut buf = [0u8; 2];
            read_exact_bounded(reader, &mut buf)?;
            Ok(GgufValue::Int16(i16::from_le_bytes(buf)))
        }
        4 => {
            let mut buf = [0u8; 4];
            read_exact_bounded(reader, &mut buf)?;
            Ok(GgufValue::Uint32(u32::from_le_bytes(buf)))
        }
        5 => {
            let mut buf = [0u8; 4];
            read_exact_bounded(reader, &mut buf)?;
            Ok(GgufValue::Int32(i32::from_le_bytes(buf)))
        }
        6 => {
            let mut buf = [0u8; 4];
            read_exact_bounded(reader, &mut buf)?;
            Ok(GgufValue::Float32(f32::from_le_bytes(buf)))
        }
        7 => {
            let mut buf = [0u8; 1];
            read_exact_bounded(reader, &mut buf)?;
            Ok(GgufValue::Bool(buf[0] != 0))
        }
        8 => {
            let s = read_string_bounded(reader, remaining_hint)?;
            Ok(GgufValue::String(s))
        }
        9 => {
            let mut u32_buf = [0u8; 4];
            read_exact_bounded(reader, &mut u32_buf)?;
            let elem_type = u32::from_le_bytes(u32_buf);
            let mut u64_buf = [0u8; 8];
            read_exact_bounded(reader, &mut u64_buf)?;
            let array_len = u64::from_le_bytes(u64_buf);
            if array_len > MAX_ARRAY_LEN {
                return Err(GgufError::InvalidLength(format!(
                    "array_len {array_len} exceeds cap"
                )));
            }
            // Skip large arrays (tokenizers) instead of materializing.
            const MATERIALIZE_CAP: u64 = 256;
            if !materialize_arrays || array_len > MATERIALIZE_CAP {
                if let Some(elem_sz) = fixed_value_size(elem_type) {
                    let total = elem_sz.saturating_mul(array_len);
                    reader
                        .seek(SeekFrom::Current(total as i64))
                        .map_err(GgufError::Io)?;
                } else {
                    for _ in 0..array_len {
                        skip_value(reader, elem_type, remaining_hint)?;
                    }
                }
                return Ok(GgufValue::SkippedArray {
                    elem_type,
                    len: array_len,
                });
            }
            let mut arr = Vec::with_capacity(array_len as usize);
            for _ in 0..array_len {
                arr.push(read_value_bounded(
                    reader,
                    elem_type,
                    remaining_hint,
                    false,
                )?);
            }
            Ok(GgufValue::Array(arr))
        }
        10 => {
            let mut buf = [0u8; 8];
            read_exact_bounded(reader, &mut buf)?;
            Ok(GgufValue::Uint64(u64::from_le_bytes(buf)))
        }
        11 => {
            let mut buf = [0u8; 8];
            read_exact_bounded(reader, &mut buf)?;
            Ok(GgufValue::Int64(i64::from_le_bytes(buf)))
        }
        12 => {
            let mut buf = [0u8; 8];
            read_exact_bounded(reader, &mut buf)?;
            Ok(GgufValue::Float64(f64::from_le_bytes(buf)))
        }
        other => Err(GgufError::UnsupportedValueType(other)),
    }
}

/// ggml type → human label (common GGUF types).
pub fn ggml_type_name(ty: u32) -> &'static str {
    match ty {
        0 => "F32",
        1 => "F16",
        2 => "Q4_0",
        3 => "Q4_1",
        6 => "Q5_0",
        7 => "Q5_1",
        8 => "Q8_0",
        9 => "Q8_1",
        10 => "Q2_K",
        11 => "Q3_K",
        12 => "Q4_K",
        13 => "Q5_K",
        14 => "Q6_K",
        15 => "Q8_K",
        16 => "IQ2_XXS",
        17 => "IQ2_XS",
        18 => "IQ3_XXS",
        19 => "IQ1_S",
        20 => "IQ4_NL",
        21 => "IQ3_S",
        22 => "IQ2_S",
        23 => "IQ4_XS",
        24 => "I8",
        25 => "I16",
        26 => "I32",
        27 => "I64",
        28 => "F64",
        29 => "IQ1_M",
        30 => "BF16",
        _ => "UNKNOWN",
    }
}

/// Bytes for a tensor given ggml type and dimension list (GGUF order: innermost first).
pub fn tensor_nbytes(ggml_type: u32, dims: &[u64]) -> Result<u64, GgufError> {
    let ne: u64 = dims.iter().try_fold(1u64, |acc, &d| {
        acc.checked_mul(d.max(1))
            .ok_or_else(|| GgufError::InvalidLength("tensor dim overflow".into()))
    })?;

    // Block size and type_size from llama.cpp ggml-common.h (QK_K = 256).
    let (block_size, type_size) = match ggml_type {
        0 => (1u64, 4u64), // F32
        1 => (1, 2),       // F16
        2 => (32, 18),     // Q4_0
        3 => (32, 20),     // Q4_1
        6 => (32, 22),     // Q5_0
        7 => (32, 24),     // Q5_1
        8 => (32, 34),     // Q8_0
        9 => (32, 36),     // Q8_1
        10 => (256, 84),   // Q2_K
        11 => (256, 110),  // Q3_K
        12 => (256, 144),  // Q4_K
        13 => (256, 176),  // Q5_K
        14 => (256, 210),  // Q6_K
        15 => (256, 292),  // Q8_K
        16 => (256, 66),   // IQ2_XXS
        17 => (256, 74),   // IQ2_XS
        18 => (256, 98),   // IQ3_XXS
        19 => (256, 50),   // IQ1_S
        20 => (32, 18),    // IQ4_NL
        21 => (256, 110),  // IQ3_S
        22 => (256, 82),   // IQ2_S
        23 => (256, 136),  // IQ4_XS
        24 => (1, 1),      // I8
        25 => (1, 2),      // I16
        26 => (1, 4),      // I32
        27 => (1, 8),      // I64
        28 => (1, 8),      // F64
        29 => (256, 56),   // IQ1_M
        30 => (1, 2),      // BF16
        other => return Err(GgufError::UnsupportedTensorType(other)),
    };

    if !ne.is_multiple_of(block_size) {
        // Pad up to next block for sizing (GGUF tensors are block-aligned).
        let blocks = ne.div_ceil(block_size);
        return Ok(blocks.saturating_mul(type_size));
    }
    Ok((ne / block_size).saturating_mul(type_size))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn write_string(buf: &mut Vec<u8>, s: &str) {
        buf.extend_from_slice(&(s.len() as u64).to_le_bytes());
        buf.extend_from_slice(s.as_bytes());
    }

    #[test]
    fn rejects_huge_kv_count() {
        let mut buf = Vec::new();
        buf.extend_from_slice(&GGUF_MAGIC.to_le_bytes());
        buf.extend_from_slice(&3u32.to_le_bytes());
        buf.extend_from_slice(&0u64.to_le_bytes());
        buf.extend_from_slice(&(MAX_KV_COUNT + 1).to_le_bytes());
        let err = GgufMetadata::read(&mut Cursor::new(buf)).unwrap_err();
        assert!(matches!(err, GgufError::InvalidLength(_)));
    }

    #[test]
    fn skips_large_string_array() {
        let mut buf = Vec::new();
        buf.extend_from_slice(&GGUF_MAGIC.to_le_bytes());
        buf.extend_from_slice(&3u32.to_le_bytes());
        buf.extend_from_slice(&0u64.to_le_bytes()); // tensors
        buf.extend_from_slice(&1u64.to_le_bytes()); // 1 KV
        write_string(&mut buf, "tokenizer.ggml.tokens");
        buf.extend_from_slice(&9u32.to_le_bytes()); // array
        buf.extend_from_slice(&8u32.to_le_bytes()); // string elems
        buf.extend_from_slice(&300u64.to_le_bytes()); // > MATERIALIZE_CAP
        for i in 0..300u64 {
            write_string(&mut buf, &format!("t{i}"));
        }
        let meta = GgufMetadata::read(&mut Cursor::new(buf)).expect("parse");
        match meta.metadata.get("tokenizer.ggml.tokens") {
            Some(GgufValue::SkippedArray { len: 300, .. }) => {}
            other => panic!("expected SkippedArray, got {other:?}"),
        }
    }

    #[test]
    fn parses_tensor_section_and_quant() {
        let mut buf = Vec::new();
        buf.extend_from_slice(&GGUF_MAGIC.to_le_bytes());
        buf.extend_from_slice(&3u32.to_le_bytes());
        buf.extend_from_slice(&2u64.to_le_bytes()); // 2 tensors
        buf.extend_from_slice(&1u64.to_le_bytes()); // 1 KV
        write_string(&mut buf, "llama.block_count");
        buf.extend_from_slice(&4u32.to_le_bytes());
        buf.extend_from_slice(&2u32.to_le_bytes());

        // Tensor 0: blk.0.attn_q.weight Q4_0 [32, 64]
        write_string(&mut buf, "blk.0.attn_q.weight");
        buf.extend_from_slice(&2u32.to_le_bytes());
        buf.extend_from_slice(&32u64.to_le_bytes());
        buf.extend_from_slice(&64u64.to_le_bytes());
        buf.extend_from_slice(&2u32.to_le_bytes()); // Q4_0
        buf.extend_from_slice(&0u64.to_le_bytes());

        // Tensor 1: token_embd.weight Q4_0 [32, 64]
        write_string(&mut buf, "token_embd.weight");
        buf.extend_from_slice(&2u32.to_le_bytes());
        buf.extend_from_slice(&32u64.to_le_bytes());
        buf.extend_from_slice(&64u64.to_le_bytes());
        buf.extend_from_slice(&2u32.to_le_bytes());
        buf.extend_from_slice(&100u64.to_le_bytes());

        let meta = GgufMetadata::read(&mut Cursor::new(buf)).expect("parse");
        assert_eq!(meta.tensors.len(), 2);
        assert_eq!(meta.tensors[0].layer_index, Some(0));
        assert!(meta.tensors[1].layer_index.is_none());
        assert_eq!(meta.quant_label.as_deref(), Some("Q4_0"));
        assert!(meta.weights_bytes() > 0);
        assert_eq!(meta.host_fixed_weight_bytes(), meta.tensors[1].nbytes);
    }

    #[test]
    fn kv_dtype_scales_cache() {
        let meta = GgufMetadata {
            version: 3,
            tensor_count: 0,
            kv_count: 0,
            metadata: HashMap::new(),
            architecture: Some("llama".into()),
            model_name: None,
            context_length: Some(4096),
            block_count: Some(28),
            head_count: Some(12),
            head_count_kv: Some(12),
            embedding_length: Some(1536),
            file_size_bytes: 0,
            tensors: vec![],
            quant_label: None,
        };
        let f16 = meta.exact_kv_cache_bytes_dtype(4096, KvCacheDtype::F16);
        let q8 = meta.exact_kv_cache_bytes_dtype(4096, KvCacheDtype::Q8_0);
        let q4 = meta.exact_kv_cache_bytes_dtype(4096, KvCacheDtype::Q4_0);
        assert!(q8 < f16);
        assert!(q4 < q8);
        // Must be far below 200KB/token * 4096 = 800 MB heuristic
        assert!(f16 < 800 * 1024 * 1024);
    }

    #[test]
    fn test_unaligned_kv_metadata_with_tensors() {
        let mut buf = Vec::new();
        buf.extend_from_slice(&GGUF_MAGIC.to_le_bytes());
        buf.extend_from_slice(&3u32.to_le_bytes());
        buf.extend_from_slice(&1u64.to_le_bytes()); // 1 tensor
        buf.extend_from_slice(&2u64.to_le_bytes()); // 2 KVs
        write_string(&mut buf, "general.architecture");
        buf.extend_from_slice(&8u32.to_le_bytes());
        write_string(&mut buf, "llama");
        write_string(&mut buf, "odd_length_key");
        buf.extend_from_slice(&4u32.to_le_bytes());
        buf.extend_from_slice(&42u32.to_le_bytes());

        // Stream position here is guaranteed not to be 32-byte aligned
        assert_ne!(buf.len() % 32, 0);

        write_string(&mut buf, "token_embd.weight");
        buf.extend_from_slice(&2u32.to_le_bytes());
        buf.extend_from_slice(&16u64.to_le_bytes());
        buf.extend_from_slice(&32u64.to_le_bytes());
        buf.extend_from_slice(&2u32.to_le_bytes()); // Q4_0
        buf.extend_from_slice(&0u64.to_le_bytes());

        let meta = GgufMetadata::read(&mut Cursor::new(buf)).expect("parse unaligned GGUF");
        assert_eq!(meta.architecture.as_deref(), Some("llama"));
        assert_eq!(meta.tensors.len(), 1);
        assert_eq!(meta.tensors[0].name, "token_embd.weight");
    }
}
