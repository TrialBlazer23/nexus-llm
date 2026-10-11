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
| SM8735 | unpublished | off until measured |

The 7.8–9.5× figures are prefill wall time on a 12 GB Hexagon v81 phone (Qwen3.6-35B-A3B Q4_0 streamed from flash): about 7.8× at 1418 tokens (63.8s CPU, 8.2s NPU) and 9.5× at 1921 tokens (106.5s, 11.2s). A 121-token prompt on that same part barely gained. Do not expect that ratio on v73. Decode tok/s is not multiplied. Placement may prefer a Vulkan MoE plan that advertised HTP0 when its predicted tok/s is at least 0.85× the leading MoE plan.

1. `scripts/versions.env` pins BigMoeOnEdge `v0.28.0`.

2. `bash scripts/setup.sh` on Termux still builds the host CLI. That binary has no Hexagon backend. `--prefill-device HTP0` then stays on the CPU, which is the upstream fallback, and decode stays on the CPU either way. Setup does not call `build_bmoe_hexagon.sh`.

3. Hexagon v73 or newer can run prefill on `HTP0` after `NEXUS_BMOE_HEXAGON=1 bash scripts/lib/build_bmoe_hexagon.sh` on an aarch64 machine with Docker. The script refuses `x86_64`.

4. Install `bmoe-cli` and the `libggml-htp-v*.so` files on the phone. The container writes them under `build-hexagon` in the BigMoe checkout (`/workspace/build-hexagon`). Copy those files into one directory on the phone. Export `ADSP_LIBRARY_PATH` to that directory before `nexus` or `nexusd`. Nexus sets `GGML_HEXAGON_HOSTBUF=1` only for a spawn that resolved HTP0.

5. Leave `npu_prefill` at `auto` on a published part. Set `on` only to verify an unpublished part. Set `off` on the Mac, the Windows laptop, and any node that should stay on CPU. A known quant outside Q4_0, Q4_1, Q8_0, IQ4_NL, MXFP4, Q4_K, Q5_K, and Q6_K omits the flag even when the switch is `on`. Q3_K and below are not an NPU expert path. An unknown label does not block. Output on the NPU is fp16 and is not bit-identical to CPU. The streamed expert path stays lossless in the sense upstream documents: the routed arena reads only routed experts, and `--no-prefill-routed` is not turned on.

6. `nexus doctor` prints one of `npu prefill: off`, `npu prefill: on (HTP0)`, `npu prefill: auto (HTP0)`, or `npu prefill: auto (off)`. The line has no product name, quant, or part.

7. Operator check, not this VM. Use a GGUF larger than the phone's RAM and an allowed quant.

   On the 16 GB phone (SM8735): `auto` must doctor `auto (off)` and must not pass the flag. Set `on`. `BMOE_DONE.prefill_dev_tokens` must be greater than 0. Record the `Hexagon Arch version vNN` line. Set `off` and the same prompt must report `prefill_dev_tokens` 0. Only then add `SM8735` → that generation in `src/npu_prefill.rs`.

   On SM8550 with Vulkan, `auto` should already doctor `auto (HTP0)`. Record the actual prefill ratio. Do not expect 7.8–9.5× on v73.
