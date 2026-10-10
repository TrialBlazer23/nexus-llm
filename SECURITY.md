# Security Policy and Threat Model

> **Target System:** Nexus-LLM Distributed Inference Mesh  
> **Classification:** Public Technical Specification  
> **Last Updated:** October 2026  

---

## 1. Architectural Threat Model & Trust Boundaries

Nexus-LLM is an ultra-lightweight distributed LLM orchestration network designed to operate across a heterogeneous mesh of personal devices (Android phones, legacy workstations, desktop servers).

### Operational Assumptions
1. **Network Boundary:** Nexus-LLM assumes communication occurs across a **Trusted Local Area Network (LAN)** or over an **Encrypted Point-to-Point Overlay Network** (e.g. WireGuard, Tailscale, SSH port forwarding, or ADB USB tunnels).
2. **Cleartext Transport:** Transport payloads (chat prompt tokens, model generation outputs, document chunks, and model binary blobs) traverse network sockets via **HTTP/1.1 without Transport Layer Security (TLS)**.
3. **Public Network Warning:** Nexus-LLM **must not** be exposed directly to untrusted public networks (the Internet, public Wi-Fi) without a secure tunneling layer or a TLS-terminating reverse proxy.

---

## 2. Cryptographic Security Primitives

While payload confidentiality relies on network-level encapsulation, Nexus-LLM implements strict **application-layer cryptographic integrity and authentication**:

### 2.1 Ed25519 Control-Plane Request Signatures
Every Nexus node generates and maintains an Ed25519 signing keypair stored locally at `~/.nexus/identity.key` (with Unix file permissions enforced at `0600`).
All remote control-plane directives (`POST /nexus/control/v1/model/load`, `POST /nexus/control/v1/model/unload`, `POST /nexus/control/v1/blob/fetch`, and Knowledge Base mutations) are verified using a canonical message signature:

```text
nexus-control-v1
<HTTP_METHOD>
<REQUEST_PATH>
<SHA256_BODY_DIGEST>
<UNIX_TIMESTAMP>
<NONCE>
<SIGNER_NODE_UUID>
```

- **Replay Protection:** Nonces are recorded in an in-memory TTL cache (`NONCE_TTL = 600s`). Reused nonces are rejected with `401 Unauthorized`.
- **Clock Drift Window:** Timestamps outside a ±120 second tolerance window are rejected with `401 Unauthorized`.
- **Signature Verification:** Verified using `ed25519-dalek` with strict curve validation (`verify_strict`).

### 2.2 Out-of-Band Mutual Node Pairing
Nexus nodes do not execute privileged control directives from unknown peers. Nodes establish mutual trust via an interactive pairing code exchange (`nexus pair`):
- **HMAC-SHA256 Derivation:** 6-digit pairing codes are derived dynamically from the node's private signing key and a 5-minute time window.
- **Rotation Grace Window:** Verification accepts codes within a ±1 window grace period.
- **Constant-Time Verification:** Code comparisons are executed in constant time to prevent timing side-channel analysis.
- **IP-Based Progressive Rate Limiting:** Pairing endpoints enforce IP-based rate limiting with progressive backoff and lockouts to defeat automated brute-force attacks.

---

## 3. Security Guarantees Matrix

| Property | Guarantee | Mechanism |
| :--- | :--- | :--- |
| **Control Plane Authenticity** | **Guaranteed** | Ed25519 asymmetric signatures verified against paired peer public keys. |
| **Message Tampering Detection** | **Guaranteed** | SHA-256 body hash included in the signed canonical envelope. |
| **Replay Attack Resistance** | **Guaranteed** | 120s timestamp window + in-memory nonce cache. |
| **Brute-Force Resistance** | **Guaranteed** | IP-keyed exponential backoff and lockout on pairing endpoints. |
| **Untrusted Parser Safety** | **Guaranteed** | Fixed 64-byte binary discovery beacons with CRC-16; bounded GGUF metadata headers. |
| **Payload Confidentiality** | **NOT Provided** | Data in transit is unencrypted HTTP/1.1; operators must use VPN/tunnels. |

---

## 4. Operator Hardening Guidelines

For production or semi-public deployments, operators must observe the following hardening rules:

1. **Keep Pairing Enforced (Default):** Never enable `allow_unpaired_lan = true` outside isolated testing environments.
2. **Interface Binding:** When running on multi-homed devices, bind `network.api_host` to `127.0.0.1` rather than `0.0.0.0`:
   ```toml
   [network]
   api_host = "127.0.0.1"
   ```
3. **USB Mobile Tethering:** For Android mobile inference, use the built-in ADB USB tunnel (`nexus tunnel`) to keep model dispatch and streaming entirely off local Wi-Fi:
   ```bash
   nexus tunnel forward --device <SERIAL> --local-port 8080 --remote-port 8080
   ```
4. **Web Gateway Exposure:** When hosting the embedded Web Hub (`nexus web`), front the gateway port (`8090`) with a TLS-terminating reverse proxy (such as Caddy or Nginx) and configure a non-default terminal PIN.
5. **Private Key Protection:** On Unix systems, verify that `~/.nexus/identity.key` has strict permissions:
   ```bash
   chmod 600 ~/.nexus/identity.key
   ```
   Run `nexus doctor` to confirm that all security precondition checks pass.

---

## 5. Vulnerability Disclosure & Reporting

We take vulnerabilities and security defects seriously. If you discover a security issue or vulnerability in Nexus-LLM:

1. **Do not open a public GitHub issue.**
2. Report the vulnerability privately via **GitHub Security Advisories** on the repository, or email the security maintainers directly.
3. Include detailed reproduction steps, target platform information (OS, architecture), and proof-of-concept payloads.
4. Maintainers will acknowledge receipt within 48 hours and coordinate a prioritized patch within 14 days.
