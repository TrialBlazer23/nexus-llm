# shellcheck shell=bash
# Build or fetch BigMoeOnEdge bmoe-cli into PREFIX/bin.

nexus_try_bmoe_prebuilt() {
  local prefix="$1"
  local ver="${BIGMOE_REF#v}"
  local asset=""
  case "${NEXUS_OS}-${NEXUS_ARCH}" in
    linux-x86_64|linux-amd64) asset="bmoe-cli-v${ver}-linux-x86_64.tar.gz" ;;
    linux-aarch64|linux-arm64)
      if [ "${NEXUS_IS_TERMUX}" = "1" ]; then
        # Termux needs a Bionic-linked build; prefer source.
        return 1
      fi
      asset="bmoe-cli-v${ver}-linux-aarch64.tar.gz"
      ;;
    darwin-arm64|darwin-aarch64) asset="bmoe-cli-v${ver}-macos-arm64.tar.gz" ;;
    *) return 1 ;;
  esac

  if [ "${NEXUS_PREFER_BMOE_SOURCE:-0}" = "1" ]; then
    return 1
  fi
  # Penryn-safe hosts should build from source so CPU baselines match.
  if [ "${NEXUS_IS_PENRYN_SAFE}" = "1" ] && [ "${NEXUS_ALLOW_BMOE_PREBUILT:-0}" != "1" ]; then
    echo "==> Skipping bmoe prebuilt on x86_64 (Penryn-safe source build)"
    return 1
  fi

  local url="https://github.com/Helldez/BigMoeOnEdge/releases/download/${BIGMOE_REF}/${asset}"
  local tmp
  tmp="$(mktemp -d)"
  echo "==> Trying bmoe-cli prebuilt: ${asset}"
  if ! curl -fsSL "$url" -o "${tmp}/${asset}"; then
    rm -rf "$tmp"
    return 1
  fi
  tar -xzf "${tmp}/${asset}" -C "$tmp"
  local bin
  bin="$(find "$tmp" -type f -name 'bmoe-cli' -perm -111 | head -n 1)"
  if [ -z "$bin" ]; then
    rm -rf "$tmp"
    return 1
  fi
  install -m 755 "$bin" "${prefix}/bin/bmoe-cli"
  rm -rf "$tmp"
  echo "==> Installed prebuilt ${prefix}/bin/bmoe-cli"
  return 0
}

nexus_build_bmoe() {
  local prefix="$1"
  local src_dir="${prefix}/src/BigMoeOnEdge"
  local jobs="${NEXUS_JOBS:-$(getconf _NPROCESSORS_ONLN 2>/dev/null || echo 4)}"

  mkdir -p "${prefix}/bin"

  if nexus_try_bmoe_prebuilt "$prefix"; then
    return 0
  fi

  nexus_clone_or_update "$BIGMOE_REPO" "$BIGMOE_REF" "$src_dir"
  echo "==> Init BigMoe llama.cpp submodule (Helldez expert-ready fork)"
  git -C "$src_dir" submodule update --init --recursive

  if [ -x "$src_dir/scripts/build-host.sh" ]; then
    echo "==> Building bmoe-cli via scripts/build-host.sh"
    (
      cd "$src_dir"
      BUILD_DIR=build-nexus BUILD_TYPE=Release JOBS="$jobs" bash scripts/build-host.sh
    )
  else
    echo "==> Building bmoe-cli via cmake"
    cmake -S "$src_dir" -B "$src_dir/build-nexus" -DCMAKE_BUILD_TYPE=Release
    cmake --build "$src_dir/build-nexus" -j "$jobs"
  fi

  local bin
  bin="$(nexus_find_built_bin "$src_dir" bmoe-cli)"
  if [ -z "$bin" ]; then
    echo "error: bmoe-cli not found after BigMoe build" >&2
    return 1
  fi
  install -m 755 "$bin" "${prefix}/bin/bmoe-cli"
  echo "==> Installed ${prefix}/bin/bmoe-cli"
}
