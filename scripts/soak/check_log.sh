#!/usr/bin/env bash
# Match captured hub-log substrings for one fleet soak scenario.
# Does not start nexus or load a model.
set -euo pipefail

if [[ $# -ne 2 ]]; then
  echo "usage: check_log.sh <scenario> <logfile>" >&2
  exit 2
fi

scenario=$1
logfile=$2

if [[ ! -f "$logfile" || ! -r "$logfile" ]]; then
  echo "error: logfile not readable: $logfile" >&2
  exit 1
fi

require_all() {
  local pattern
  for pattern in "$@"; do
    if ! grep -F -q -- "$pattern" "$logfile"; then
      printf 'missing: %s\n' "$pattern"
      exit 1
    fi
  done
}

# First listed pattern is the `missing:` line when none of them match.
require_any() {
  local pattern
  for pattern in "$@"; do
    if grep -F -q -- "$pattern" "$logfile"; then
      return 0
    fi
  done
  printf 'missing: %s\n' "$1"
  exit 1
}

case "$scenario" in
  usb-wifi)
    require_all "USB" "Wi-Fi" "route="
    ;;
  cable-yank)
    require_all "USB" "Wi-Fi"
    ;;
  battery-floor)
    require_all "battery decline"
    ;;
  thermal)
    require_all "thermal skip"
    ;;
  dense-4gb)
    require_any "no local model fits" "Exceeds" "exceeds 75% memory ceiling"
    ;;
  moe-phone)
    require_all "moe-stream" "tok/s"
    ;;
  *)
    echo "unknown scenario: $scenario" >&2
    exit 2
    ;;
esac

printf 'ok %s\n' "$scenario"
