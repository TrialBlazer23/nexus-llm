<!-- STREAMING_CHUNK:Documenting header and metadata -->
Nexus-LLM: System Architecture & Technical Specification
Document ID: SPEC-NEXUS-2026-REV4 Target Nodes: Node A (Samsung Galaxy S23 Ultra / Termux ARM64) & Node B (MacBook / Debian 13 Trixie x86-64) Lead Architect: Evan Hyland
<!-- STREAMING_CHUNK:Defining system overview and hardware target comparison table -->
1. System Overview & Architectural Topology
Nexus-LLM is an autonomous, cross-platform terminal orchestrator and local inference daemon designed for heterogeneous edge devices. It prioritizes GPU acceleration via Vulkan on modern ARM platforms, provides transparent CPU fallback, automates local network discovery, enforces memory bounds to protect against OS process terminations (Android LMK), and serves interactive terminal UIs to lightweight client machines.
Hardware Targets
Parameter
Node A: Primary Compute Host
Node B: Control & Workstation Node
Physical Device
Samsung Galaxy S23 Ultra (SM-S918U)
Apple MacBook (macrowave)
Processor
Qualcomm Snapdragon 8 Gen 2 (SM8550-AB)
Intel Core 2 Duo P7550 @ 2.26 GHz (2 Cores / 2 Threads)
Microarchitecture
Cortex-X3 + Cortex-A715/A710 + Cortex-A510
Intel Penryn (45nm, 3MB L2 Cache)
Operating System
Android 14+ via Termux (Bionic libc)
Debian GNU/Linux 13 (Trixie), Kernel 6.12.111-amd64
Desktop / Shell
Headless CLI / zsh in Termux
Xfce 4.20 / X11 / bash
Installed RAM
12 GB LPDDR5X (Unified Memory)
3.6 GiB Usable DDR3
Primary Acceleration
Adreno 740 GPU via Vulkan (GGML_VULKAN=1)
None (CPU only; model inference disabled by default)
CPU Fallback
ARMv8.2-A dotprod + i8mm (Cortex-X3/A715)
MMX, SSE, SSE2, SSE3, SSSE3, SSE4.1
Strict Prohibitions
Over-allocating beyond 75% available RAM
AVX, AVX2, FMA, F16C, POPCNT, SSE4.2 (causes SIGILL)
Designated Role
Autonomous Inference Host (nexusd)
Interactive TUI Client & Dashboard (nexus)

<!-- STREAMING_CHUNK:Defining GPU offload hierarchy and fallback rules -->
2. Compute Hierarchy & Acceleration Policy
Nexus-LLM enforces an explicit acceleration waterfall on the compute host:
[Model Load Request]
         │
         ▼
[Memory Safety Check: Model Size + KV Cache < 0.75 * MemAvailable]
         │
         ├───► FAIL: Reject with MemoryCapExceeded error
         │
         ▼ PASS
[Hardware Capability Probe]
         │
         ├───► Vulkan Runtime Available?
         │         │
         │         ├───► YES: Launch llama-server with -ngl 99 (Full GPU Offload)
         │         │          Verify device init in stdout/stderr.
         │         │          If Vulkan initialization fails at runtime:
         │         │          Fallback automatically to CPU Mode.
         │         │
         │         └───► NO (e.g. Minimal Termux environment):
         │                    Launch llama-server with -ngl 0 (CPU Mode)
         │                    Use optimized flags: -t 6 --threads-batch 6


Vulkan Integration Notes for Termux (Node A)
Unified LPDDR5X architecture on Snapdragon 8 Gen 2 allows zero-copy or high-bandwidth host-to-device tensor transfers.
When compiled with -DGGML_VULKAN=ON, llama.cpp dynamically binds to /system/lib64/libvulkan.so or the Termux packaged Vulkan loader.
Full offload (-ngl 99 or matching layer count) places weights directly into Adreno GPU buffers, drastically reducing CPU core thermal stress and battery drain.
<!-- STREAMING_CHUNK:Specifying network port allocation and binary beacon layout -->
3. Network Topology & Discovery Protocol
Port Allocation
Port 8080 (TCP) - Inference & Management API: Serves the OpenAI-compatible HTTP REST and Server-Sent Events (SSE) streaming API (/v1/chat/completions, /v1/models, /health).
Port 9999 (UDP) - Cluster Discovery & Heartbeat: Transmits autonomous binary telemetry packets across the subnet broadcast address (255.255.255.255).
Binary Beacon Packet Structure (64 Bytes Fixed)
Byte Offset
Field
Type
Description
0x00 - 0x03
Magic Header
uint32 (BE)
Fixed signature 0x4E585553 (ASCII "NXUS").
0x04
Version
uint8
Protocol version (0x01).
0x05
Node Role
uint8
Bitmask: 0x01 = Host, 0x02 = Client, 0x04 = Standalone.
0x06 - 0x07
Status Flags
uint16 (BE)
Bit 0: Ready, Bit 1: Inferring, Bit 2: Vulkan Active, Bit 3: Thermal Throttle.
0x08 - 0x17
Node UUID
uint8[16]
RFC 4122 unique node identifier.
0x18 - 0x19
API Port
uint16 (BE)
Active HTTP port (default 8080).
0x1A - 0x1B
Reserved
uint16 (BE)
Reserved for future transport extensions.
0x1C - 0x1F
Total RAM
uint32 (BE)
Physical memory in Megabytes.
0x20 - 0x23
Free RAM
uint32 (BE)
MemAvailable in Megabytes.
0x24
Acceleration Tier
uint8
0x01 = Vulkan (Adreno 740), 0x02 = ARM CPU DotProd/I8MM, 0x03 = Legacy x86 SSE4.1.
0x25
Thermal Index
uint8
0 (nominal, <40°C) to 100 (critical, >80°C).
0x26 - 0x3D
Active Model
char[24]
Null-padded ASCII model family name.
0x3E - 0x3F
Checksum
uint16 (BE)
CRC-16-CCITT computed over bytes 0x00 through 0x3D.

<!-- STREAMING_CHUNK:Configuring build flags and compiler constraints for ARM and Penryn -->
4. Hardware Compilation & Runtime Flags
Node A: Samsung Galaxy S23 Ultra (Termux ARM64) - Vulkan Enabled
pkg update && pkg install -y clang cmake ninja git libllvm

# Check Vulkan availability
which vulkaninfo || pkg install -y vulkan-tools vulkan-loader-generic

git clone https://github.com/ggml-org/llama.cpp
cd llama.cpp

# Build with Vulkan compute acceleration
cmake -B build-vulkan \
  -G Ninja \
  -DCMAKE_BUILD_TYPE=Release \
  -DGGML_VULKAN=ON \
  -DCMAKE_C_FLAGS="-O3 -march=armv8.2-a+dotprod+i8mm -mtune=cortex-x3" \
  -DCMAKE_CXXFLAGS="-O3 -march=armv8.2-a+dotprod+i8mm -mtune=cortex-x3"

ninja -C build-vulkan llama-server


For secondary Termux installations lacking Vulkan libraries, compile without -DGGML_VULKAN=ON to utilize the ARM NEON/DotProd CPU pipeline.
Node B: Apple MacBook (macrowave, Debian 13 Intel Core 2 Duo P7550) - Client Only
# Node B runs the Rust TUI client (nexus). Model inference is disabled.
sudo apt-get update && sudo apt-get install -y cargo rustc build-essential git


<!-- STREAMING_CHUNK:Defining configuration schema for nexus daemon -->
5. Configuration Specification
Global Config: ~/.nexus/config.toml
[node]
id = "auto"
name = "auto"
role = "host" # "host" on S23 Ultra, "client" on Mac
models_dir = "~/nexus-models"
presets_dir = "~/.nexus/presets"

[hardware.acceleration]
prefer_gpu = true           # Prioritize Vulkan on Adreno 740
gpu_layers = 99             # Offload all layers to Vulkan when available
fallback_to_cpu = true      # Seamlessly drop to ARM CPU if Vulkan init fails
cpu_threads = 6             # Optimal for Snapdragon 8 Gen 2 (1x X3 + 4x A715/A710)
cpu_threads_batch = 6

[hardware.safety]
max_ram_usage_percent = 75  # Guard against Android LMK SIGKILL (Signal 9)
mmap = true
mlock = false

[network]
api_host = "0.0.0.0"
api_port = 8080
discovery_port = 9999
broadcast_interval_ms = 2000
peer_timeout_ms = 6000
static_peers = []


