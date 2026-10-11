#!/usr/bin/env bash
# Opt-in Hexagon NPU build of BigMoeOnEdge (bmoe-cli + libggml-htp skels).
# scripts/setup.sh and nexus_build_bmoe do not call this. Host setup stays CPU-only.
#
#   NEXUS_BMOE_HEXAGON=1 bash scripts/lib/build_bmoe_hexagon.sh
#
# Refuses x86_64 before any clone or container. Does not pass -march; the
# Snapdragon container script owns the Android flags.

set -euo pipefail

if [ "${NEXUS_BMOE_HEXAGON:-}" != "1" ]; then
  echo "hexagon build skipped"
  exit 0
fi

case "$(uname -m)" in
  x86_64|amd64)
    echo "refusing Hexagon build on x86_64" >&2
    exit 2
    ;;
esac

# Printed as-is when docker is missing. <src> is the placeholder from the
# BigMoe container invocation, not a path on this machine.
docker_line='docker run --rm -v <src>:/workspace ghcr.io/snapdragon-toolchain/arm64-android:v0.7 bash /workspace/scripts/build-hexagon-android.sh'
if ! command -v docker >/dev/null 2>&1; then
  echo "$docker_line" >&2
  exit 3
fi

lib_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck disable=SC1091
. "${lib_dir}/../versions.env"
# shellcheck disable=SC1091
. "${lib_dir}/build_llama.sh"

src_dir="${NEXUS_PREFIX:-${HOME}/.nexus}/src/BigMoeOnEdge"
nexus_clone_or_update "$BIGMOE_REPO" "$BIGMOE_REF" "$src_dir"
echo "==> Init BigMoe llama.cpp submodule (Helldez expert-ready fork)"
git -C "$src_dir" submodule update --init --recursive

# Mount that checkout at /workspace. No -march flags.
docker run --rm -v "${src_dir}:/workspace" ghcr.io/snapdragon-toolchain/arm64-android:v0.7 bash /workspace/scripts/build-hexagon-android.sh
