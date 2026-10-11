# NPU prefill (Hexagon HTP0)

Nexus forwards Hexagon prefill only when the phone's own config accepts `HTP0`. Decode stays on the CPU. The default host build has no Hexagon backend.

1. `scripts/versions.env` pins BigMoeOnEdge `v0.28.0`.

2. `bash scripts/setup.sh` on Termux still builds the host CLI. That binary has no Hexagon backend. `--prefill-device HTP0` then stays on the CPU, which is the upstream fallback, and decode stays on the CPU either way.

3. Hexagon v73 or newer (Snapdragon 8 Gen 2 and later) can run prefill on `HTP0` after `NEXUS_BMOE_HEXAGON=1 bash scripts/lib/build_bmoe_hexagon.sh` on an aarch64 machine with Docker. The script refuses `x86_64`.

4. Install `bmoe-cli` and the `libggml-htp-v*.so` files on the phone. The container writes them under `build-hexagon` in the BigMoe checkout (`/workspace/build-hexagon`). Copy those files into one directory on the phone. Export `ADSP_LIBRARY_PATH` to that directory before `nexus` or `nexusd`. Nexus sets `GGML_HEXAGON_HOSTBUF=1` only for a spawn whose config accepted `HTP0`.

5. Set `prefill_device = "HTP0"` under `[inference.moe]` on that phone. Leave it empty on the Mac, the Windows laptop, and any 4 GB node.

6. Expert tensors the NPU matmul accepts are Q4_0, Q4_1, Q8_0, IQ4_NL, MXFP4, Q4_K, Q5_K, and Q6_K. Q3_K and below are not an NPU expert path. Output on the NPU is fp16 and is not bit-identical to CPU. The streamed expert path stays lossless in the sense upstream documents: the routed arena reads only routed experts, and `--no-prefill-routed` is not turned on.

7. Acceptance, run by the operator on the S23 Ultra and the Nothing Phone 3, with a GGUF larger than that phone's RAM: `BMOE_DONE` includes a non-zero `prefill_dev_tokens` when the flag is on, and a second run with `prefill_device` empty reports `prefill_dev_tokens` 0. Record tok/s for both. This VM does not run that acceptance.
