#!/usr/bin/env bash
# Nexus-LLM unified bootstrap: deps → llama.cpp → bmoe-cli → nexus → config → doctor
#
# Usage (from repo root or any cwd):
#   bash scripts/setup.sh
#   bash scripts/setup.sh --skip-moe --jobs 4
#   bash scripts/setup.sh --prefix "$HOME/.nexus" --model 'https://...gguf'
#
# Termux: always invoke via `bash scripts/setup.sh` (see AGENT_LEARNINGS).

set -euo pipefail

SETUP_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${SETUP_DIR}/.." && pwd)"
# shellcheck disable=SC1091
. "${SETUP_DIR}/versions.env"
# shellcheck disable=SC1091
. "${SETUP_DIR}/lib/detect_platform.sh"
# shellcheck disable=SC1091
. "${SETUP_DIR}/lib/install_deps.sh"
# shellcheck disable=SC1091
. "${SETUP_DIR}/lib/build_llama.sh"
# shellcheck disable=SC1091
. "${SETUP_DIR}/lib/build_bmoe.sh"
# shellcheck disable=SC1091
. "${SETUP_DIR}/lib/install_nexus.sh"
# shellcheck disable=SC1091
. "${SETUP_DIR}/lib/write_config.sh"

SKIP_LLAMA=0
SKIP_MOE=0
SKIP_NEXUS=0
SKIP_DOCTOR=0
DRY_RUN=0
PREFIX="${NEXUS_PREFIX:-${HOME}/.nexus}"
MODEL_URL=""
NEXUS_JOBS="${NEXUS_JOBS:-}"

usage() {
  cat <<'EOF'
Nexus-LLM setup — install toolchain, backends, and wire ~/.nexus/config.toml

Options:
  --prefix DIR       Install prefix (default: ~/.nexus)
  --jobs N           Parallel build jobs
  --skip-llama       Do not build llama-server / rpc-server
  --skip-moe         Do not build / fetch bmoe-cli
  --skip-nexus       Do not cargo-build nexus (use existing target/release)
  --skip-doctor      Do not run nexus doctor at the end
  --model URL        After setup, download GGUF via `nexus download`
  --prefer-bmoe-source   Always build BigMoe from source (no GitHub prebuilt)
  --allow-bmoe-prebuilt  Allow x86_64 prebuilt bmoe-cli (may include AVX)
  --disable-vulkan   Force GGML_VULKAN=OFF for llama.cpp
  --dry-run          Detect platform and print plan only
  -h, --help         Show this help
EOF
}

while [ $# -gt 0 ]; do
  case "$1" in
    --prefix) PREFIX="$2"; shift 2 ;;
    --jobs) NEXUS_JOBS="$2"; shift 2 ;;
    --skip-llama) SKIP_LLAMA=1; shift ;;
    --skip-moe) SKIP_MOE=1; shift ;;
    --skip-nexus) SKIP_NEXUS=1; shift ;;
    --skip-doctor) SKIP_DOCTOR=1; shift ;;
    --model) MODEL_URL="$2"; shift 2 ;;
    --prefer-bmoe-source) NEXUS_PREFER_BMOE_SOURCE=1; shift ;;
    --allow-bmoe-prebuilt) NEXUS_ALLOW_BMOE_PREBUILT=1; shift ;;
    --disable-vulkan) NEXUS_DISABLE_VULKAN=1; shift ;;
    --dry-run) DRY_RUN=1; shift ;;
    -h|--help) usage; exit 0 ;;
    *) echo "unknown option: $1" >&2; usage >&2; exit 2 ;;
  esac
done

export NEXUS_JOBS NEXUS_PREFER_BMOE_SOURCE NEXUS_ALLOW_BMOE_PREBUILT NEXUS_DISABLE_VULKAN
export NEXUS_REPO_ROOT="$REPO_ROOT"
export NEXUS_PREFIX="$PREFIX"

nexus_detect_platform
echo "==> Nexus setup"
echo "    repo:    ${REPO_ROOT}"
echo "    prefix:  ${PREFIX}"
nexus_platform_summary
echo "    llama:   ${LLAMA_CPP_REPO} @ ${LLAMA_CPP_REF} (skip=${SKIP_LLAMA})"
echo "    bigmoe:  ${BIGMOE_REPO} @ ${BIGMOE_REF} (skip=${SKIP_MOE})"

if [ "$DRY_RUN" = "1" ]; then
  echo "==> dry-run complete (no changes)"
  exit 0
fi

mkdir -p "${PREFIX}/bin" "${PREFIX}/src" "${PREFIX}/models"

nexus_install_deps

# shellcheck disable=SC1091
[ -f "$HOME/.cargo/env" ] && . "$HOME/.cargo/env"

BUILT_MOE=0
if [ "$SKIP_LLAMA" != "1" ]; then
  nexus_build_llama "$PREFIX"
else
  echo "==> Skipping llama.cpp build"
fi

if [ "$SKIP_MOE" != "1" ]; then
  if nexus_build_bmoe "$PREFIX"; then
    BUILT_MOE=1
  else
    echo "warn: bmoe-cli build failed; continuing without MoE stream backend" >&2
    BUILT_MOE=0
  fi
else
  echo "==> Skipping BigMoe / bmoe-cli"
fi

if [ "$SKIP_NEXUS" != "1" ]; then
  nexus_build_and_install_nexus "$PREFIX" "$REPO_ROOT"
else
  echo "==> Skipping cargo build"
  if [ -x "${REPO_ROOT}/target/release/nexus" ]; then
    export NEXUS_INSTALLED_BIN="${REPO_ROOT}/target/release/nexus"
    install -m 755 "${REPO_ROOT}/target/release/nexus" "${PREFIX}/bin/nexus" 2>/dev/null || true
  elif [ -x "${PREFIX}/bin/nexus" ]; then
    export NEXUS_INSTALLED_BIN="${PREFIX}/bin/nexus"
  else
    echo "error: --skip-nexus but no nexus binary at target/release or ${PREFIX}/bin" >&2
    exit 1
  fi
fi

nexus_write_config "$PREFIX" "$BUILT_MOE"

if [ -n "$MODEL_URL" ]; then
  out="${PREFIX}/models/$(basename "${MODEL_URL%%\?*}")"
  echo "==> Downloading model → ${out}"
  "${NEXUS_INSTALLED_BIN}" download "$MODEL_URL" -o "$out"
fi

if [ "$SKIP_DOCTOR" != "1" ]; then
  echo "==> Running nexus doctor"
  if ! "${NEXUS_INSTALLED_BIN}" doctor; then
    echo "warn: doctor reported issues (see above)" >&2
  fi
fi

echo ""
echo "Setup complete."
echo "  Binaries: ${PREFIX}/bin"
echo "  Config:   \${NEXUS_CONFIG:-~/.nexus/config.toml}"
echo "  Run:      ${PREFIX}/bin/nexus"
echo "  Or:       export PATH=\"${PREFIX}/bin:\$PATH\" && nexus"
