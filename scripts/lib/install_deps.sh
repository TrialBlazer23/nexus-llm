# shellcheck shell=bash
# Install host dependencies for Nexus + llama.cpp + BigMoe builds.

nexus_have_cmd() {
  command -v "$1" >/dev/null 2>&1
}

nexus_ensure_rust() {
  local min="${NEXUS_RUST_MIN:-1.85.0}"
  if nexus_have_cmd rustc && nexus_have_cmd cargo; then
    local ver
    ver="$(rustc --version | awk '{print $2}')"
    echo "==> Rust ${ver} found (need >= ${min})"
    return 0
  fi

  echo "==> Installing Rust via rustup (minimal profile)"
  if [ "${NEXUS_IS_TERMUX}" = "1" ]; then
    pkg install -y rust 2>/dev/null || true
    if nexus_have_cmd rustc; then
      return 0
    fi
  fi

  if ! nexus_have_cmd curl; then
    echo "error: curl required to install rustup" >&2
    return 1
  fi
  curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal --default-toolchain stable
  # shellcheck disable=SC1091
  [ -f "$HOME/.cargo/env" ] && . "$HOME/.cargo/env"
  rustup component add rustfmt clippy 2>/dev/null || true
  nexus_have_cmd rustc
}

nexus_install_deps() {
  if nexus_have_cmd git && nexus_have_cmd cmake && nexus_have_cmd clang && nexus_have_cmd rustc && nexus_have_cmd cargo; then
    echo "==> Build dependencies already satisfied (git, cmake, clang, rustc, cargo)"
    return 0
  fi
  echo "==> Installing build dependencies (pkg=${NEXUS_PKG_MGR})"
  case "$NEXUS_PKG_MGR" in
    pkg)
      pkg update -y || true
      pkg install -y clang cmake git ninja make rust libllvm 2>/dev/null \
        || pkg install -y clang cmake git make rust
      ;;
    apt)
      if nexus_have_cmd sudo && [ "$(id -u)" -ne 0 ]; then
        sudo apt-get update -y
        sudo DEBIAN_FRONTEND=noninteractive apt-get install -y \
          build-essential cmake git ninja-build pkg-config curl ca-certificates
      elif [ "$(id -u)" -eq 0 ]; then
        apt-get update -y
        DEBIAN_FRONTEND=noninteractive apt-get install -y \
          build-essential cmake git ninja-build pkg-config curl ca-certificates
      else
        echo "warn: cannot apt-install without root; assuming cmake/git/clang present"
      fi
      ;;
    dnf)
      if nexus_have_cmd sudo && [ "$(id -u)" -ne 0 ]; then
        sudo dnf install -y gcc gcc-c++ cmake git ninja-build pkgconf-pkg-config curl
      elif [ "$(id -u)" -eq 0 ]; then
        dnf install -y gcc gcc-c++ cmake git ninja-build pkgconf-pkg-config curl
      fi
      ;;
    pacman)
      if nexus_have_cmd sudo && [ "$(id -u)" -ne 0 ]; then
        sudo pacman -Sy --noconfirm base-devel cmake git ninja curl
      elif [ "$(id -u)" -eq 0 ]; then
        pacman -Sy --noconfirm base-devel cmake git ninja curl
      fi
      ;;
    brew)
      if ! nexus_have_cmd brew; then
        echo "error: Homebrew not found. Install from https://brew.sh" >&2
        return 1
      fi
      brew list cmake >/dev/null 2>&1 || brew install cmake
      brew list ninja >/dev/null 2>&1 || brew install ninja
      brew list git >/dev/null 2>&1 || brew install git
      ;;
    *)
      echo "warn: unknown package manager; require cmake, git, C/C++ toolchain on PATH"
      ;;
  esac

  for req in git cmake; do
    if ! nexus_have_cmd "$req"; then
      echo "error: missing required tool: $req" >&2
      return 1
    fi
  done

  nexus_ensure_rust
}
