#!/usr/bin/env bash
# Print fleet soak steps, or check one captured hub log.
# Does not start nexus, load a model, or call device tools.
set -euo pipefail

script_dir=$(cd "$(dirname "$0")" && pwd)

usage() {
  echo "usage: run.sh --dry-run | run.sh --check <scenario> <logfile>" >&2
  exit 2
}

if [[ $# -eq 0 ]]; then
  usage
fi

case "$1" in
  --dry-run)
    if [[ $# -ne 1 ]]; then
      usage
    fi
    cat <<'EOF'
Fleet soak dry-run. Prints operator steps only. Does not start nexus or load a model.

scenarios:
  usb-wifi
  cable-yank
  battery-floor
  thermal
  dense-4gb
  moe-phone

usb-wifi
  Have one USB peer and one Wi-Fi peer up at the same time. Send a prompt from the hub. Keep hub lines that contain link=usb, link=wifi, and route=.

cable-yank
  start a generation on the USB peer, unplug, confirm the stream ends or errors, then send a new prompt and confirm the route line is Wi-Fi when that peer is still the winner. Unplug flips the peer to Wi-Fi when the beacon is fresh, and an in-flight stream is not migrated. Keep a hub line with link=usb from the start, and a later line with link=wifi for that same session.

battery-floor
  Drain a peer under the battery floor with battery action decline. Send a new prompt. Keep hub lines that contain battery and decline. That peer must not take the new load.

thermal
  Raise a peer above the thermal cutoff. Send a new heavy prompt. Keep a hub line that contains thermal. The next new load skips that peer. An in-flight local reply is not killed.

dense-4gb
  on the 4 GB node, ask for a dense GGUF whose weights alone exceed 75% of MemAvailable, and keep the log line that reports the reject. The checker accepts either substring: no local model fits, or Exceeds.

moe-phone
  on a 12–16 GB phone, load a streamable MoE larger than RAM through the hub escalation path and keep a `tok/s` line. The log must also contain moe-stream.

Check a captured hub log with:
  bash scripts/soak/run.sh --check <scenario> <logfile>
Passing logs print: ok <scenario>
The first missing substring prints: missing: <pattern>
EOF
    ;;
  --check)
    if [[ $# -ne 3 ]]; then
      usage
    fi
    exec bash "$script_dir/check_log.sh" "$2" "$3"
    ;;
  *)
    usage
    ;;
esac
