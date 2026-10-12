# NPU prefill (Hexagon HTP0)

Each node decides Hexagon prefill from its own `[inference.moe]` config. The hub never sends a device token. Decode stays on the CPU. The default host build has no Hexagon backend.

```toml
[inference.moe]
npu_prefill = "auto"   # "on" | "off"
```

`auto` is the default when the key is missing. It passes `--prefill-device HTP0` only when this process is aarch64, Vulkan is present, and `/sys/devices/soc0/machine` or `getprop ro.soc.model` names a published Hexagon v73, v75, v79, or v81 part. Anything else, including a missing part, stays off. `on` requests HTP0 even when the part is unpublished. Upstream falls back to CPU if the skel cannot open. `off` never passes the flag.

An explicit `npu_prefill` wins over the legacy string. `prefill_device = "HTP0"` still means `on` when `npu_prefill` is unset. Any other legacy token means `auto`. Settings writes `npu_prefill` and clears `prefill_device`.

The part table lives in `src/npu_prefill.rs`. It is copied from LiteRT `supported_soc.csv` (fetched 2026-10-11). Behavior does not key on marketing names, `ro.product.model`, RAM, serials, or whether `libggml-htp-v*.so` files are on disk. The Hexagon build drops v73 through v81 together, so those files do not identify the phone.

Published examples:

| Part | HTP | `auto` on aarch64 Vulkan |
| --- | --- | --- |
| SM8550 (8 Gen 2) | v73 | HTP0 |
| SM8650 | v75 | HTP0 |
| SM8750 | v79 | HTP0 |
| SM8850 (8 Elite Gen 5) | v81 | HTP0 |
| SM8450 | v69 | off |
| SM8735 | v73, measured 2026-10-12 | HTP0 |

The 7.8–9.5× figures are prefill wall time on a 12 GB Hexagon v81 phone (Qwen3.6-35B-A3B Q4_0 streamed from flash): about 7.8× at 1418 tokens (63.8s CPU, 8.2s NPU) and 9.5× at 1921 tokens (106.5s, 11.2s). A 121-token prompt on that same part barely gained. Do not expect that ratio on v73. Decode tok/s is not multiplied. Placement may prefer a Vulkan MoE plan that advertised HTP0 when its predicted tok/s is at least 0.85× the leading MoE plan.

1. `scripts/versions.env` pins BigMoeOnEdge `v0.28.0`.

2. `bash scripts/setup.sh` on Termux still builds the host CLI. That binary has no Hexagon backend. `--prefill-device HTP0` then stays on the CPU, which is the upstream fallback, and decode stays on the CPU either way. Setup does not call `build_bmoe_hexagon.sh`.

3. Hexagon v73 or newer can run prefill on `HTP0`. The BigMoeOnEdge v0.28.0 arm64 release already contains `bmoe-cli` and `libggml-htp-v73.so` through `libggml-htp-v81.so`. Copy those files into one directory on the phone. The Docker build (`NEXUS_BMOE_HEXAGON=1 bash scripts/lib/build_bmoe_hexagon.sh`) is the other way to produce that same set. It refuses `x86_64`. `setup.sh` does not call it.

4. Export `ADSP_LIBRARY_PATH` to that directory before `nexus` or `nexusd`. Nexus sets `GGML_HEXAGON_HOSTBUF=1` only for a spawn that resolved HTP0. Skel files for several generations ship together, so their presence does not identify the phone. The `Hexagon Arch version vNN` stderr line does.

5. Leave `npu_prefill` at `auto` on a published part. Set `on` only to verify an unpublished part. Set `off` on the Mac, the Windows laptop, and any node that should stay on CPU. A known quant outside Q4_0, Q4_1, Q8_0, IQ4_NL, MXFP4, Q4_K, Q5_K, and Q6_K omits the flag even when the switch is `on`. Q3_K and below are not an NPU expert path. An unknown label does not block. Output on the NPU is fp16 and is not bit-identical to CPU. The streamed expert path stays lossless in the sense upstream documents: the routed arena reads only routed experts, and `--no-prefill-routed` is not turned on.

6. `nexus doctor` prints one of `npu prefill: off`, `npu prefill: on (HTP0)`, `npu prefill: auto (HTP0)`, or `npu prefill: auto (off)`. The line has no product name, quant, or part.

7. Operator check, not this VM. Use a GGUF larger than the phone's RAM and an allowed quant.

   `SM8735` is v73 in `src/npu_prefill.rs`. Measured on Termux, 2026-10-12, Nexus `c21e301`: `getprop ro.soc.model` was `SM8735` (`/sys/devices/soc0/machine` was unreadable) and Vulkan was present. `auto` doctored `auto (off)` and omitted `--prefill-device` (`prefill_dev_tokens=0`). `on` passed `--prefill-device HTP0`, opened `libggml-htp-v73.so`, and logged `Hexagon Arch version v73` after `FASTRPC_GET_DOMAINS` failed with `0x6c`. The same Q4_K_M MoE logged `prefill_dev_tokens=549`. `off` omitted the flag and logged `prefill_dev_tokens=0`. `auto` now resolves HTP0 on this part. An unknown part still stays off until the same pair of lines exists.

   On SM8550 with Vulkan, `auto` should doctor `auto (HTP0)`. Measured on Termux, 2026-10-12, Nexus `c21e301`: `getprop ro.soc.model` was `SM8550` (`/sys/devices/soc0/machine` was unreadable), doctor printed `auto (HTP0)`, and `Qwen3-Coder-30B-A3B-Instruct-Q4_K_M` streamed with `--prefill-device HTP0`. stderr reported `Hexagon Arch version v73` after `FASTRPC_GET_DOMAINS` failed with `0x6c` and static CDSP domains loaded. A 440-word prompt logged `prefill_dev_tokens=724`. Decode stayed on the CPU at about 1.9 tok/s. That run has no CPU-off comparison. Do not expect 7.8–9.5× on v73.
