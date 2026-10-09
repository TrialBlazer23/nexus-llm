# shellcheck shell=bash
# Point ~/.nexus/config.toml binary paths at PREFIX/bin via `nexus setup __write_bins`.

nexus_write_config() {
  local prefix="$1"
  local enable_moe="${2:-0}"
  local nexus_bin="${NEXUS_INSTALLED_BIN:-}"
  local repo_root="${NEXUS_REPO_ROOT:-}"

  if [ -z "$nexus_bin" ] || [ ! -x "$nexus_bin" ]; then
    if [ -n "$repo_root" ] && [ -x "${repo_root}/target/release/nexus" ]; then
      nexus_bin="${repo_root}/target/release/nexus"
    elif [ -x "${prefix}/bin/nexus" ]; then
      nexus_bin="${prefix}/bin/nexus"
    else
      echo "error: nexus binary not found for config write" >&2
      return 1
    fi
  fi

  local args=(setup __write_bins --prefix "$prefix")
  if [ "$enable_moe" = "1" ] && [ -x "${prefix}/bin/bmoe-cli" ]; then
    args+=(--enable-moe)
  fi
  if [ -x "${prefix}/bin/llama-server" ]; then
    args+=(--llama-server "${prefix}/bin/llama-server")
  fi
  if [ -x "${prefix}/bin/rpc-server" ]; then
    args+=(--rpc-server "${prefix}/bin/rpc-server")
  fi
  if [ -x "${prefix}/bin/bmoe-cli" ]; then
    args+=(--bmoe-cli "${prefix}/bin/bmoe-cli")
  fi

  echo "==> Writing binary paths into config via nexus"
  "$nexus_bin" "${args[@]}"
}
