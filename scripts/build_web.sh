#!/usr/bin/env bash
set -euo pipefail

# scripts/build_web.sh — verify and package frontend assets for Nexus-LLM
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"

echo "==> Verifying web distribution assets in $REPO_ROOT/web/dist"
if [[ ! -f "$REPO_ROOT/web/dist/index.html" ]]; then
  echo "Error: $REPO_ROOT/web/dist/index.html is missing!" >&2
  exit 1
fi

SIZE=$(wc -c < "$REPO_ROOT/web/dist/index.html")
echo "==> Web UI bundle index.html size: $SIZE bytes"
echo "==> Frontend assets verified."
