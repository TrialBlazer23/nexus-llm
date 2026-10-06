use serde::{Deserialize, Serialize};
use std::fs;
use std::path::Path;
use tracing::{debug, warn};

/// Acceleration tiers supported across heterogeneous nodes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum AccelerationBackend {
    Vulkan,
    ArmCpuDotProd,
    X86Baseline,
    GenericCpu,
}

impl std::fmt::Display for AccelerationBackend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Vulkan => write!(f, "Vulkan (Adreno/GPU)"),
            Self::ArmCpuDotProd => write!(f, "ARM CPU (DotProd/I8MM)"),
            Self::X86Baseline => write!(f, "x86 Baseline (SSE4.1)"),
            Self::GenericCpu => write!(f, "Generic CPU"),
        }
    }
}

/// System profile capturing memory capacity, acceleration tier, and thread guidance.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SystemProfile {
    pub total_ram_mb: u64,
    pub available_ram_mb: u64,
    pub detected_backend: AccelerationBackend,
    pub recommended_threads: usize,
}

impl SystemProfile {
    /// Probe the current host environment: memory from /proc/meminfo,
    /// acceleration tier from Vulkan runtime or /proc/cpuinfo, and CPU thread guidance.
    pub fn probe() -> Self {
        let (total_ram_mb, available_ram_mb) = Self::read_meminfo();
        let has_vulkan = Self::probe_vulkan();
        let detected_backend = Self::detect_backend(has_vulkan);
        let recommended_threads = Self::calculate_recommended_threads(detected_backend);

        debug!(
            "System probed: Total RAM: {} MB, Available: {} MB, Backend: {:?}, Threads: {}",
            total_ram_mb, available_ram_mb, detected_backend, recommended_threads
        );

        Self {
            total_ram_mb,
            available_ram_mb,
            detected_backend,
            recommended_threads,
        }
    }

    /// Read `/proc/meminfo` and parse `MemTotal` and `MemAvailable` in MB.
    fn read_meminfo() -> (u64, u64) {
        if let Ok(content) = fs::read_to_string("/proc/meminfo") {
            Self::parse_meminfo(&content)
        } else {
            warn!("Failed to read /proc/meminfo, falling back to default estimates");
            // Sensible fallback: 4GB total, 2GB available
            (4096, 2048)
        }
    }

    /// Parse `/proc/meminfo` text to extract `(total_ram_mb, available_ram_mb)`.
    pub fn parse_meminfo(content: &str) -> (u64, u64) {
        let mut total_kb: u64 = 0;
        let mut avail_kb: u64 = 0;

        for line in content.lines() {
            let parts: Vec<&str> = line.split_whitespace().collect();
            if parts.len() >= 2 {
                if parts[0] == "MemTotal:" {
                    total_kb = parts[1].parse::<u64>().unwrap_or(0);
                } else if parts[0] == "MemAvailable:" {
                    avail_kb = parts[1].parse::<u64>().unwrap_or(0);
                }
            }
        }

        let total_mb = total_kb / 1024;
        let avail_mb = if avail_kb > 0 {
            avail_kb / 1024
        } else {
            // Some older kernels or virtual environments lack MemAvailable
            total_mb / 2
        };

        (total_mb, avail_mb)
    }

    /// Probe whether Vulkan runtime libraries or tools are available on the system.
    pub fn probe_vulkan() -> bool {
        // 1. Android / Termux dynamic library paths
        let standard_vulkan_paths = [
            "/system/lib64/libvulkan.so",
            "/data/data/com.termux/files/usr/lib/libvulkan.so",
            "/usr/lib/aarch64-linux-gnu/libvulkan.so.1",
            "/usr/lib/x86_64-linux-gnu/libvulkan.so.1",
            "/usr/lib/libvulkan.so.1",
        ];

        for path in standard_vulkan_paths {
            if Path::new(path).exists() {
                return true;
            }
        }

        // 2. Check if `vulkaninfo` is available in PATH
        if let Ok(path_var) = std::env::var("PATH") {
            for dir in std::env::split_paths(&path_var) {
                if dir.join("vulkaninfo").is_file() {
                    return true;
                }
            }
        }

        false
    }

    /// Detect acceleration backend based on architecture, Vulkan availability, and cpuinfo features.
    fn detect_backend(has_vulkan: bool) -> AccelerationBackend {
        let arch = std::env::consts::ARCH;
        let cpuinfo = fs::read_to_string("/proc/cpuinfo").unwrap_or_default();
        Self::parse_cpuinfo_backend(arch, &cpuinfo, has_vulkan)
    }

    /// Parse cpuinfo string and detect backend according to hardware target specifications.
    pub fn parse_cpuinfo_backend(arch: &str, cpuinfo: &str, has_vulkan: bool) -> AccelerationBackend {
        if arch == "aarch64" || arch == "arm" {
            if has_vulkan {
                return AccelerationBackend::Vulkan;
            }

            let lower = cpuinfo.to_lowercase();
            if lower.contains("asimddp") || lower.contains("dotprod") || lower.contains("i8mm") {
                return AccelerationBackend::ArmCpuDotProd;
            }
            return AccelerationBackend::GenericCpu;
        }

        if arch == "x86_64" {
            // Intel Core 2 Duo Penryn baseline or compatible x86_64
            return AccelerationBackend::X86Baseline;
        }

        AccelerationBackend::GenericCpu
    }

    /// Determine recommended CPU threads based on hardware profile.
    fn calculate_recommended_threads(backend: AccelerationBackend) -> usize {
        let detected_cores = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(2);

        match backend {
            AccelerationBackend::Vulkan => {
                // When offloaded to GPU, CPU thread requirement is minimal for token processing
                4.min(detected_cores)
            }
            AccelerationBackend::ArmCpuDotProd => {
                // On Snapdragon 8 Gen 2 (1x Prime Cortex-X3 + 4x Gold Cortex-A715/A710),
                // 6 threads yields optimal performance without burdening A510 efficiency cores.
                if detected_cores >= 8 {
                    6
                } else {
                    detected_cores.saturating_sub(1).max(1)
                }
            }
            AccelerationBackend::X86Baseline => {
                // Core 2 Duo P7550 has 2 cores / 2 threads
                2.min(detected_cores)
            }
            AccelerationBackend::GenericCpu => {
                detected_cores.saturating_sub(1).max(1)
            }
        }
    }

    /// Estimate KV cache memory consumption in bytes for a given context size.
    ///
    /// Heuristic: approximately 200 KB per token in 16-bit precision
    /// for typical 7B-8B parameter models.
    pub fn estimate_kv_cache_bytes(context_size: usize) -> u64 {
        const BYTES_PER_CONTEXT_TOKEN: u64 = 200 * 1024; // 200 KB
        (context_size as u64).saturating_mul(BYTES_PER_CONTEXT_TOKEN)
    }

    /// Maximum RAM bytes allowed under the Android LMK memory ceiling (default 75%).
    pub fn max_allowed_memory_bytes(&self) -> u64 {
        self.max_allowed_memory_bytes_pct(75)
    }

    /// Maximum RAM bytes allowed using an explicit usage percent (1–100).
    pub fn max_allowed_memory_bytes_pct(&self, percent: u8) -> u64 {
        let pct = u64::from(percent.clamp(1, 100));
        let available_bytes = self.available_ram_mb.saturating_mul(1024 * 1024);
        (available_bytes.saturating_mul(pct)) / 100
    }

    /// Android LMK Guard: verifies whether a model and its KV cache can safely load.
    ///
    /// Blocks model loads where:
    ///   (model_file_size_bytes + kv_cache_bytes) > percent * MemAvailable
    pub fn can_safely_load(&self, model_file_size_bytes: u64, context_size: usize) -> bool {
        self.can_safely_load_pct(model_file_size_bytes, context_size, 75)
    }

    pub fn can_safely_load_pct(
        &self,
        model_file_size_bytes: u64,
        context_size: usize,
        percent: u8,
    ) -> bool {
        let kv_cache_bytes = Self::estimate_kv_cache_bytes(context_size);
        let total_required = model_file_size_bytes.saturating_add(kv_cache_bytes);
        let max_allowed = self.max_allowed_memory_bytes_pct(percent);

        let safe = total_required <= max_allowed;
        if !safe {
            warn!(
                "Memory Safety Guard tripped! Required: {} MB (Model: {} MB + KV: {} MB), Max Allowed ({}% of Avail {} MB): {} MB",
                total_required / (1024 * 1024),
                model_file_size_bytes / (1024 * 1024),
                kv_cache_bytes / (1024 * 1024),
                percent.clamp(1, 100),
                self.available_ram_mb,
                max_allowed / (1024 * 1024)
            );
        }
        safe
    }

    /// Android LMK Guard with exact GGUF architectural dimensions.
    pub fn can_safely_load_gguf(&self, gguf: &crate::gguf::GgufMetadata, context_size: usize) -> bool {
        self.can_safely_load_gguf_pct(gguf, context_size, 75)
    }

    pub fn can_safely_load_gguf_pct(
        &self,
        gguf: &crate::gguf::GgufMetadata,
        context_size: usize,
        percent: u8,
    ) -> bool {
        let kv_cache_bytes = gguf.exact_kv_cache_bytes(context_size);
        let total_required = gguf.file_size_bytes.saturating_add(kv_cache_bytes);
        let max_allowed = self.max_allowed_memory_bytes_pct(percent);

        let safe = total_required <= max_allowed;
        if !safe {
            warn!(
                "Memory Safety Guard tripped on GGUF! Required: {} MB (Model: {} MB + Exact KV: {} MB), Max Allowed ({}% of Avail {} MB): {} MB",
                total_required / (1024 * 1024),
                gguf.file_size_bytes / (1024 * 1024),
                kv_cache_bytes / (1024 * 1024),
                percent.clamp(1, 100),
                self.available_ram_mb,
                max_allowed / (1024 * 1024)
            );
        }
        safe
    }
}
