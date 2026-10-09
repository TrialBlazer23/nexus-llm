# shellcheck shell=bash
# Clone/build stock llama.cpp → llama-server + rpc-server into PREFIX/bin.

nexus_llama_cmake_flags() {
  local flags=()
  flags+=(-DCMAKE_BUILD_TYPE=Release)
  flags+=(-DLLAMA_BUILD_TOOLS=ON)
  flags+=(-DLLAMA_BUILD_SERVER=ON)
  flags+=(-DLLAMA_BUILD_EXAMPLES=OFF)
  flags+=(-DLLAMA_BUILD_TESTS=OFF)
  flags+=(-DGGML_RPC=ON)
  flags+=(-DGGML_NATIVE=OFF)

  if [ "${NEXUS_IS_PENRYN_SAFE}" = "1" ]; then
    # Penryn / Core 2 Duo: SSE4.1 only — never AVX/AVX2/FMA/SSE4.2/POPCNT.
    flags+=(-DGGML_AVX=OFF -DGGML_AVX2=OFF -DGGML_FMA=OFF -DGGML_F16C=OFF)
    flags+=(-DGGML_SSE42=OFF -DGGML_BMI2=OFF)
    flags+=(-DCMAKE_C_FLAGS='-march=x86-64 -msse4.1 -mno-sse4.2 -mno-popcnt -mno-avx -mno-avx2')
    flags+=(-DCMAKE_CXX_FLAGS='-march=x86-64 -msse4.1 -mno-sse4.2 -mno-popcnt -mno-avx -mno-avx2')
  fi

  if [ "${NEXUS_IS_TERMUX}" = "1" ] || { [ "$NEXUS_ARCH" = "aarch64" ] || [ "$NEXUS_ARCH" = "arm64" ]; }; then
    # Prefer DotProd/I8MM when the compiler accepts them (Snapdragon 8 Gen 2+).
    if [ "${NEXUS_IS_TERMUX}" = "1" ]; then
      flags+=(-DCMAKE_C_FLAGS='-march=armv8.2-a+dotprod+i8mm')
      flags+=(-DCMAKE_CXX_FLAGS='-march=armv8.2-a+dotprod+i8mm')
    fi
  fi

  # Vulkan is optional; enable when headers exist and operator did not disable.
  if [ "${NEXUS_DISABLE_VULKAN:-0}" != "1" ] \
    && { [ -f /usr/include/vulkan/vulkan.h ] || [ -f "${PREFIX:-}/include/vulkan/vulkan.h" ]; }; then
    flags+=(-DGGML_VULKAN=ON)
  fi

  printf '%s\n' "${flags[@]}"
}

nexus_clone_or_update() {
  local repo="$1" ref="$2" dest="$3"
  if [ -d "$dest/.git" ]; then
    echo "==> Updating $(basename "$dest") → ${ref}"
    git -C "$dest" fetch --tags --force --depth 1 origin "$ref" 2>/dev/null \
      || git -C "$dest" fetch --tags --force origin "$ref"
    git -C "$dest" checkout -f FETCH_HEAD 2>/dev/null || git -C "$dest" checkout -f "$ref"
  else
    echo "==> Cloning $(basename "$dest") (${ref})"
    mkdir -p "$(dirname "$dest")"
    git clone --depth 1 --branch "$ref" "$repo" "$dest" 2>/dev/null \
      || {
        git clone --depth 1 "$repo" "$dest"
        git -C "$dest" fetch --depth 1 origin "$ref"
        git -C "$dest" checkout -f FETCH_HEAD
      }
  fi
}

nexus_find_built_bin() {
  local root="$1" name="$2"
  find "$root" -type f -name "$name" -perm -111 2>/dev/null | head -n 1
}

nexus_build_llama() {
  local prefix="$1"
  local src_dir="${prefix}/src/llama.cpp"
  local build_dir="${src_dir}/build-nexus"
  local jobs="${NEXUS_JOBS:-$(getconf _NPROCESSORS_ONLN 2>/dev/null || echo 4)}"

  mkdir -p "${prefix}/bin"
  nexus_clone_or_update "$LLAMA_CPP_REPO" "$LLAMA_CPP_REF" "$src_dir"

  echo "==> Configuring llama.cpp (${LLAMA_CPP_REF})"
  # shellcheck disable=SC2046
  cmake -S "$src_dir" -B "$build_dir" $(nexus_llama_cmake_flags | tr '\n' ' ')

  echo "==> Building llama-server + ggml-rpc-server (jobs=${jobs})"
  cmake --build "$build_dir" -j "$jobs" --target llama-server ggml-rpc-server \
    || cmake --build "$build_dir" -j "$jobs"

  local llama_bin rpc_bin
  llama_bin="$(nexus_find_built_bin "$build_dir" llama-server)"
  rpc_bin="$(nexus_find_built_bin "$build_dir" ggml-rpc-server)"
  if [ -z "$rpc_bin" ]; then
    rpc_bin="$(nexus_find_built_bin "$build_dir" rpc-server)"
  fi

  if [ -z "$llama_bin" ]; then
    echo "error: llama-server binary not found under ${build_dir}" >&2
    return 1
  fi

  install -m 755 "$llama_bin" "${prefix}/bin/llama-server"
  if [ -n "$rpc_bin" ]; then
    install -m 755 "$rpc_bin" "${prefix}/bin/rpc-server"
  else
    echo "warn: rpc-server/ggml-rpc-server not built; RPC worker mode unavailable"
  fi

  echo "==> Installed ${prefix}/bin/llama-server"
  [ -x "${prefix}/bin/rpc-server" ] && echo "==> Installed ${prefix}/bin/rpc-server"

  if [ "${NEXUS_IS_PENRYN_SAFE}" = "1" ] && [ -x "$(command -v objdump || true)" ]; then
    if command -v objdump >/dev/null 2>&1; then
      if objdump -d "${prefix}/bin/llama-server" 2>/dev/null \
        | rg -q '\b(v?amax|v?fmadd|vpxor|vpcmp|aesenc|vpshufb|vinsert|vbroadcast|vzeroupper|vpand|vpor)\b|\bv[a-z]+\b.*ymm|vmovaps.*ymm|\b(avx|avx2|fma)\b'; then
        # Soft check — many false positives from CPUID-gated paths; warn only.
        echo "warn: objdump spotted possible wide-SIMD mnemonics in llama-server (may be CPUID-gated)"
      else
        echo "==> Penryn opcode smoke: no obvious AVX/AVX2/FMA mnemonics in llama-server"
      fi
    fi
  fi
}
