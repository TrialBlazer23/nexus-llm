#!/usr/bin/env bash
# Fail if a Nexus x86_64 binary contains unexpected AVX / AVX2 / FMA / SSE4.2
# instructions outside known runtime-dispatched dependency specializations.
#
# Honors AGENTS.md Directive 1 and .cargo/config.toml rustflags:
#   -C target-feature=-avx,-avx2,-fma,-sse4.2
#
# Why an allowlist?
#   Crates such as `ring`, `rand_chacha`, `memchr`, and `httparse` ship
#   `#[target_feature(enable = "avx"/"avx2")]` (or asm) kernels that are only
#   called after CPUID. Those opcodes appear in the binary but must not execute
#   on Penryn. rustc `-C target-feature=-avx,…` does not strip them. Continuous
#   CI still fails the job if forbidden mnemonics appear in any other symbol —
#   that is the signal that our rustflags/baseline broke.
#
# Usage: ./scripts/check_penryn_opcodes.sh [path-to-binary]
# Default binary: target/release/nexus

set -euo pipefail

BINARY="${1:-target/release/nexus}"

if [[ ! -f "$BINARY" ]]; then
  echo "error: binary not found: $BINARY" >&2
  echo "Build first: cargo build --locked --release --bin nexus" >&2
  exit 2
fi

if ! command -v objdump >/dev/null 2>&1; then
  echo "error: objdump is required (binutils)" >&2
  exit 2
fi

if [[ -f .cargo/config.toml ]]; then
  if ! grep -q 'target-feature=-avx,-avx2,-fma,-sse4.2' .cargo/config.toml; then
    echo "error: .cargo/config.toml missing Penryn rustflags (-avx,-avx2,-fma,-sse4.2)" >&2
    exit 1
  fi
fi

python3 - "$BINARY" <<'PY'
import re, sys

binary = sys.argv[1]
import subprocess
dis = subprocess.check_output(
    ["objdump", "-d", "--no-show-raw-insn", binary],
    text=True,
    errors="replace",
)

# Forbidden mnemonic at start of instruction text.
forbid = re.compile(
    r"^\s*[0-9a-fA-F]+:\s+"
    r"(?P<mn>v[a-z][a-z0-9]*|crc32|pcmpestri|pcmpestrm|pcmpistri|pcmpistrm)\b"
)
sym_re = re.compile(r"^[0-9a-fA-F]+ <(?P<sym>.+)>:")

# Runtime-dispatched SIMD/crypto specializations (CPUID-gated; not Penryn paths).
# Match demangled-ish Rust symbol fragments (Itanium/Rust mangling keeps crate paths).
allow = re.compile(
    r"("
    r"ring_core_"
    r"|_aesni_|aesni_|aes_nohw_"
    r"|rand_chacha|impl_avx|impl_sse|spec_avx"
    r"|memchr.*(avx|sse)"
    r"|httparse.*(avx|sse|simd)"
    r"|curve25519_dalek.*(avx|sse|vector)"
    r"|sha2.*(avx|sse)|sha512_compress_x86_64_avx"
    r"|sha256_compress_x86_64_avx"
    r")",
    re.IGNORECASE,
)

cur = "?"
bad = []  # (symbol, mnemonic, line)
allowed_counts = {}

for line in dis.splitlines():
    m = sym_re.match(line)
    if m:
        cur = m.group("sym")
        continue
    fm = forbid.search(line)
    if not fm:
        continue
    mn = fm.group("mn")
    if allow.search(cur):
        allowed_counts[mn] = allowed_counts.get(mn, 0) + 1
        continue
    bad.append((cur, mn, line.strip()))

if bad:
    print(f"Penryn safety check FAILED: unexpected forbidden opcodes in {binary}", file=sys.stderr)
    print(
        "Directive 1 forbids AVX / AVX2 / FMA / SSE4.2 outside known CPUID-gated dep kernels.",
        file=sys.stderr,
    )
    print("", file=sys.stderr)
    # Summarize unique symbol+mnemonic
    seen = {}
    for sym, mn, _ in bad:
        seen[(sym, mn)] = seen.get((sym, mn), 0) + 1
    print("Unexpected hits (symbol → mnemonic × count):", file=sys.stderr)
    for (sym, mn), c in sorted(seen.items(), key=lambda x: -x[1])[:60]:
        print(f"  {c:4}× {mn:16}  {sym[:140]}", file=sys.stderr)
    print("", file=sys.stderr)
    print("Sample lines:", file=sys.stderr)
    for _, _, ln in bad[:15]:
        print(f"  {ln}", file=sys.stderr)
    sys.exit(1)

print(f"Penryn safety check OK: {binary}")
print(
    "  No unexpected AVX/AVX2/FMA/SSE4.2 mnemonics "
    f"(allowlisted CPUID-gated dep hits: {sum(allowed_counts.values())})."
)
print("  Confirmed .cargo/config.toml Penryn rustflags present.")
PY
