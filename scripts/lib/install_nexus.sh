# shellcheck shell=bash
# Build Nexus release binaries and install into PREFIX/bin.

nexus_build_and_install_nexus() {
  local prefix="$1"
  local repo_root="$2"
  local jobs="${NEXUS_JOBS:-$(getconf _NPROCESSORS_ONLN 2>/dev/null || echo 4)}"

  # shellcheck disable=SC1091
  [ -f "$HOME/.cargo/env" ] && . "$HOME/.cargo/env"

  echo "==> Building nexus + nexusd (release, locked, jobs≈${jobs})"
  (
    cd "$repo_root"
    export CARGO_TERM_COLOR=always
    cargo build --release --locked --bins
  )

  mkdir -p "${prefix}/bin" "${prefix}/models"
  install -m 755 "${repo_root}/target/release/nexus" "${prefix}/bin/nexus"
  if [ -f "${repo_root}/target/release/nexusd" ]; then
    install -m 755 "${repo_root}/target/release/nexusd" "${prefix}/bin/nexusd"
  fi

  echo "==> Installed ${prefix}/bin/nexus"
  [ -x "${prefix}/bin/nexusd" ] && echo "==> Installed ${prefix}/bin/nexusd"

  # Convenience: also leave release artifacts in-tree (already there).
  export NEXUS_INSTALLED_BIN="${prefix}/bin/nexus"
}
