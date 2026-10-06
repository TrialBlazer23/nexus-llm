#!/usr/bin/env bash
# Reproduces the call-site evidence behind CAPABILITY_REVIEW.md.
# Each check prints the call sites found in src/ for a symbol the review claims
# is unreachable from a user action. Empty output means the claim holds.
set -uo pipefail
cd "$(dirname "$0")/.."

rule() { printf '\n==== %s ====\n' "$1"; }

# Prefer ripgrep, fall back to grep -r.
if command -v rg >/dev/null 2>&1; then
    search() { rg -n "$1" src/ 2>/dev/null; }
else
    search() { grep -rn --include='*.rs' "$1" src/ 2>/dev/null; }
fi

# $1 = label, $2 = pattern, $3 = regex of files that are allowed to match
# (definition sites). Call sites outside those files are what we are hunting.
expect_no_callers() {
    local label="$1" pattern="$2" defs="$3" hits
    rule "$label"
    hits=$(search "$pattern" | { [ -n "$defs" ] && grep -Ev "$defs" || cat; })
    if [ -z "$hits" ]; then
        echo "NO CALL SITES IN src/ (claim holds)"
    else
        echo "$hits"
    fi
}

echo "Nexus-LLM — CAPABILITY_REVIEW.md evidence"
echo "commit: $(git rev-parse --short HEAD 2>/dev/null || echo unknown)"

echo
echo "########################################################################"
echo "# S0/S1.1 — control-plane handlers have no server binding them"
echo "########################################################################"
expect_no_callers "handle_load_model / handle_unload_model" \
    'handle_load_model|handle_unload_model' '^src/control_plane\.rs'
expect_no_callers "fetch_state (peer state verification)" \
    'fetch_state' '^src/control_plane\.rs'
expect_no_callers "dispatch_unload_model (remote unload)" \
    'dispatch_unload_model' '^src/control_plane\.rs'
rule "any HTTP listener in src/ (axum/hyper/TcpListener::bind)"
search 'TcpListener::bind|axum::|hyper::server|Server::bind' || \
    echo "NO HTTP SERVER IN src/ (claim holds)"

echo
echo "########################################################################"
echo "# S1.4 — PeerRegistry lifecycle is inert at runtime"
echo "########################################################################"
expect_no_callers "expire / remove_terminal / mark_verified / eligible_rpc_peers" \
    '\.expire\(|remove_terminal|mark_verified|eligible_rpc_peers' \
    '^src/peer_registry\.rs'

echo
echo "########################################################################"
echo "# S0 — other implemented-but-unreachable modules"
echo "########################################################################"
expect_no_callers "Preset::format_prompt / ChatTemplate" \
    'format_prompt' '^src/preset\.rs'
expect_no_callers "NexusClient::complete_chat" \
    'complete_chat' '^src/client\.rs'
expect_no_callers "TunnelView (no hub tab hosts it)" \
    'TunnelView' '^src/ui/tunnel_view\.rs|^src/ui/mod\.rs'
expect_no_callers "ProcessSupervisor::subscribe / SupervisorState" \
    '\.subscribe\(\)|SupervisorState' '^src/supervisor\.rs'
expect_no_callers "GgufError::UnexpectedEof is never constructed" \
    'UnexpectedEof' '^src/gguf\.rs:2[0-9]:'
rule "ModelDownloader reachable only from the CLI, not the TUI"
search 'ModelDownloader'

echo
echo "########################################################################"
echo "# S0/S3.2 — config fields that are read only by the settings UI"
echo "########################################################################"
for field in 'node\.name' 'runtime_role' 'node\.capabilities' \
             'fallback_to_cpu' 'cpu_threads_batch' 'safety\.mmap' \
             'safety\.mlock' 'max_ram_usage_percent' 'enable_rpc' \
             'prefer_adb_tunnel'; do
    rule "$field (outside config.rs and settings_view.rs)"
    hits=$(search "$field" | grep -Ev '^src/config\.rs|^src/ui/settings_view\.rs')
    [ -z "$hits" ] && echo "INERT — no behavioral use (claim holds)" || echo "$hits"
done

echo
echo "########################################################################"
echo "# S2.7 — no tracing subscriber in TUI mode"
echo "########################################################################"
expect_no_callers "subscriber installation outside the daemon" \
    'tracing_subscriber|set_global_default' '^src/daemon\.rs'

echo
echo "########################################################################"
echo "# S3.3 — hardcoded device constants outside cluster.rs"
echo "########################################################################"
rule "1800 MB worker cap and 10300 MB badge threshold"
search '1800|10300' | grep -Ev '^src/cluster\.rs'
rule "context size hardcoded to 4096 in the hub"
search 'context_size: 4096|estimate_kv_cache_bytes\(4096\)' 

echo
echo "########################################################################"
echo "# S2.4 — messages mutated without the parallel metrics vector"
echo "########################################################################"
rule "messages.push / messages.clear vs message_metrics in hub.rs"
search 'messages\.push|messages\.clear|message_metrics'

echo
echo "########################################################################"
echo "# S3.4 — unbounded allocations sized from untrusted file input"
echo "########################################################################"
rule "with_capacity / vec! sized from GGUF header fields"
search 'with_capacity\(kv_count|with_capacity\(array_len|vec!\[0u8; len\]'

echo
printf '\nDone.\n'
