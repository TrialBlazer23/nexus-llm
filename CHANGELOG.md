# Changelog

All notable changes to Nexus-LLM will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.2.0] - 2026-10-09

### Added
- **Formal Threat Model & Security Policy**: Created [SECURITY.md](SECURITY.md) documenting LAN boundary assumptions, Ed25519 authentication guarantees, cleartext HTTP transport, pairing semantics, and vulnerability reporting procedures.
- **Dependency & License Auditing**: Added `deny.toml` configuration for `cargo-deny` security advisories, banned licenses, and duplicate dependency checks in CI.
- **Automated Dependency Updates**: Added `.github/dependabot.yml` tracking weekly Cargo crate updates and monthly GitHub Actions revisions.
- **Property-Based Parser Fuzzing**: Added `tests/test_gguf_properties.rs` with `proptest` suites asserting that `GgufMetadata::read` safely rejects arbitrary corrupted bytes, hostile lengths, extreme counts, and invalid UTF-8 without panicking or allocating unbounded memory.
- **Data-Driven Tier-1 Routing**: Extended `Router` and YAML presets (`presets/coder.yaml`, `presets/general.yaml`) with configurable `route_keywords` and `tags` to allow localized and tunable deterministic dispatch without code modifications.
- **Security Preflight Diagnostics**: Added `check_security()` probe to `nexus doctor` evaluating pairing configuration, private key file permissions (`0600`), and network listener interface bindings.
- **Exponential Backoff on Pairing**: Hardened `ControlPlaneContext` pair attempt rate limiter with progressive exponential delay (1s after 3 failures, 3s after 5 failures, 60s lockout after 10 failures) and constant-time 6-digit PIN comparisons (`constant_time_eq_6`).

### Changed
- **Lock Poisoning Resilience**: Eliminated all `.expect("... lock")` panic paths in `ControlPlaneContext` hyper async handlers by implementing safe poisoned-lock recovery helpers (`read_config()`, `write_config()`).
- **Dependency Drift Remediation**: Migrated from deprecated and archived `serde_yaml 0.9` to maintained drop-in replacement `serde_yml 0.0.12`.
- **String & Encoding Optimization**: Replaced O(N²) `format!` byte concatenation with a single-allocation byte-lookup table in `hex_encode`, and unified `hex_decode` across `node_identity.rs` and `trust_auth.rs`.
- **Discovery Beacon Deduplication**: Consolidated 3 separate beacon assembly sites in `discovery.rs` into a unified `create_current_beacon()` builder.
- **Thermal Heuristic Documentation**: Clarified thermal zone reading logic in `sysinfo.rs` as a platform heuristic with fallback traversal across all `/sys/class/thermal/thermal_zone*` interfaces.
- **Code & Comment Cleanup**: Replaced robotic comment annotations with standard documentation comments across control-plane handlers.
- **README Restructuring**: Relocated Quickstart to the top, removed marketing hyperbole, accurately documented LAN cleartext transport alongside signed security primitives, and presented MoE flash-streaming benchmarks objectively.

## [0.1.0] - 2026-03-28

### Added
- Initial release of Nexus-LLM distributed orchestrator.
- Zero-configuration UDP 9999 beacon, mDNS-SD, and ARP discovery.
- Sequential layer pipelining over llama.cpp `rpc-server`.
- BigMoeOnEdge (`bmoe-cli`) flash streaming integration for mobile MoE inference.
- Android LMK 75% memory guard and Penryn Core 2 Duo SSE4.1 opcode safety.
- Full-screen Ratatui TUI hub with Chat, Models, Cluster, Settings, Tunnel, Agents, and Logs tabs.
- Single-file zero-dependency embedded Web Hub and Gateway (:8090).
- Content-addressed redb knowledge base with vector embeddings.
- 24 integration test suites covering system components.
