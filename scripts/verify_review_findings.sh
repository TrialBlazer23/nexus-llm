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
FAIL=0

echo
echo "########################################################################"
echo "# S0/S1.1 — control-plane server wiring (Phase 7)"
echo "########################################################################"
rule "control_plane_server module exists"
if [ -f src/control_plane_server.rs ]; then
    echo "PRESENT: src/control_plane_server.rs"
else
    echo "MISSING: src/control_plane_server.rs"
fi
rule "handle_load_model / handle_unload_model called from server"
hits=$(search 'handle_load_model|handle_unload_model' | grep -E 'src/control_plane_server\.rs' || true)
if [ -n "$hits" ]; then
    echo "$hits"
else
    echo "MISSING: server does not call handlers"
fi
rule "HTTP listener in src/ (hyper TcpListener)"
hits=$(search 'TcpListener::bind|hyper::server|control_plane_server' || true)
if [ -n "$hits" ]; then
    echo "$hits"
else
    echo "MISSING: no HTTP server in src/"
fi
rule "hub dispatches to control_endpoint (not api_endpoint for load)"
hits=$(search 'control_endpoint\(\)|dispatch_load_model' | grep -E 'src/ui/hub\.rs' || true)
if [ -n "$hits" ]; then
    echo "$hits"
else
    echo "MISSING: hub does not use control_endpoint / dispatch_load_model"
fi
# Still expected: client-only helpers unused at runtime until Phase 9/1.4
expect_no_callers "fetch_state (peer state verification; still Phase 9/1.4)" \
    'fetch_state' '^src/control_plane\.rs'
expect_no_callers "dispatch_unload_model (remote unload; still unwired from UI)" \
    'dispatch_unload_model' '^src/control_plane\.rs|^src/control_plane_server\.rs'

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
echo "# S0/S3.2 — config fields: Settings-displayed fields must be consumed"
echo "########################################################################"
# Still inert (TOML schema / not shown in Settings, or deferred):
for field in 'runtime_role' 'node\.capabilities' \
             'fallback_to_cpu' 'cpu_threads_batch' 'safety\.mlock'; do
    rule "$field (outside config.rs and settings_view.rs)"
    hits=$(search "$field" | grep -Ev '^src/config\.rs|^src/ui/settings_view\.rs')
    [ -z "$hits" ] && echo "INERT — no behavioral use (claim holds)" || echo "$hits"
done
# Phase 7 remainder wired these Settings fields into runtime consumers:
for field in 'max_ram_usage_percent' 'safety\.mmap' 'enable_rpc' 'prefer_adb_tunnel' \
             'resolved_display_name|display_name'; do
    rule "$field must have behavioral callers outside settings/config"
    hits=$(search "$field" | grep -Ev '^src/config\.rs|^src/ui/settings_view\.rs|^CAPABILITY_REVIEW|^AGENT_LEARNINGS|^scripts/')
    if [ -z "$hits" ]; then
        echo "MISSING — expected runtime consumers after Phase 7 remainder"
        FAIL=1
    else
        echo "$hits" | head -20
        echo "WIRED — behavioral use present"
    fi
done

echo
echo "########################################################################"
echo "# S2.7 — file logging installed for TUI via logging module"
echo "########################################################################"
rule "init_file_logging / logging module used from main"
hits=$(search 'init_file_logging|mod logging|nexus::logging' | grep -E '^src/(main|daemon|logging|lib)\.rs')
if [ -z "$hits" ]; then
    echo "MISSING — expected file logging wiring"
    FAIL=1
else
    echo "$hits"
    echo "WIRED — file logging present"
fi

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
exit "$FAIL"
