# shellcheck shell=bash
# Detect host platform for Nexus bootstrap. Sourced by setup.sh.
# Sets: NEXUS_OS NEXUS_ARCH NEXUS_DISTRO NEXUS_IS_TERMUX NEXUS_IS_WSL
#        NEXUS_IS_PENRYN_SAFE NEXUS_PKG_MGR

nexus_detect_platform() {
  NEXUS_OS="$(uname -s 2>/dev/null | tr '[:upper:]' '[:lower:]')"
  NEXUS_ARCH="$(uname -m 2>/dev/null || echo unknown)"
  NEXUS_DISTRO="unknown"
  NEXUS_IS_TERMUX=0
  NEXUS_IS_WSL=0
  NEXUS_IS_PENRYN_SAFE=0
  NEXUS_PKG_MGR="none"

  if [ -n "${TERMUX_VERSION:-}" ] || [ -d "/data/data/com.termux" ] || [ -x "/data/data/com.termux/files/usr/bin/pkg" ] || { [ -n "${PREFIX:-}" ] && [ -x "${PREFIX}/bin/pkg" ]; }; then
    NEXUS_IS_TERMUX=1
    NEXUS_DISTRO="termux"
    NEXUS_PKG_MGR="pkg"
  elif [ -f /etc/os-release ]; then
    # shellcheck disable=SC1091
    . /etc/os-release
    NEXUS_DISTRO="${ID:-linux}"
  elif [ "$NEXUS_OS" = "darwin" ]; then
    NEXUS_DISTRO="macos"
  fi

  if grep -qiE 'microsoft|wsl' /proc/version 2>/dev/null; then
    NEXUS_IS_WSL=1
  fi

  case "$NEXUS_DISTRO" in
    debian|ubuntu|raspbian|linuxmint|pop) NEXUS_PKG_MGR="apt" ;;
    fedora|rhel|centos|rocky|almalinux) NEXUS_PKG_MGR="dnf" ;;
    arch|manjaro|endeavouros) NEXUS_PKG_MGR="pacman" ;;
    termux) NEXUS_PKG_MGR="pkg" ;;
    macos) NEXUS_PKG_MGR="brew" ;;
  esac

  # Always emit Penryn-safe x86_64 ggml flags for Linux/WSL desktop targets so the
  # same binary runs on Core 2 Duo P7550 and modern CPUs (SSE4.1 baseline).
  if [ "$NEXUS_ARCH" = "x86_64" ] || [ "$NEXUS_ARCH" = "amd64" ]; then
    NEXUS_IS_PENRYN_SAFE=1
  fi

  export NEXUS_OS NEXUS_ARCH NEXUS_DISTRO NEXUS_IS_TERMUX NEXUS_IS_WSL
  export NEXUS_IS_PENRYN_SAFE NEXUS_PKG_MGR
}

nexus_platform_summary() {
  echo "platform: os=${NEXUS_OS} arch=${NEXUS_ARCH} distro=${NEXUS_DISTRO} pkg=${NEXUS_PKG_MGR} termux=${NEXUS_IS_TERMUX} wsl=${NEXUS_IS_WSL} penryn_safe=${NEXUS_IS_PENRYN_SAFE}"
}
