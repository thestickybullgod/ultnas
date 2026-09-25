# Ultnas — Security Model & Vulnerability Disclosure

> Version: 0.1 | Last Updated: 2026-09-25

---

## Security Model

Ultnas provides **local, user-controlled** security guarantees. It is not a network security product and makes no claims about protection against attackers with physical access to your machine or with OS-level privileges.

### What Ultnas Protects Against

| Threat | Mechanism |
|---|---|
| Accidental content corruption | BLAKE3 hash verification on every read |
| Undetected tampering of archived records | ContentId mismatch → `IntegrityFailure` error |
| Unauthorized record sealing | Ed25519 key required; private key never stored by Ultnas |
| Policy drift at seal time | Policy file hash embedded in every seal |
| Concurrent write corruption | Advisory vault lock (`.ultnas-lock`) |
| Partial writes on crash | Atomic `write → fsync → rename` for all vault writes |

### What Ultnas Does NOT Protect Against

| Threat | Notes |
|---|---|
| Attacker with OS-level access | Can delete the vault, replace lock files, or steal keys |
| Encryption at rest | Ultnas stores content in plaintext. Use full-disk encryption (e.g. LUKS, FileVault) separately. |
| Network attackers | Ultnas is local-first; sync features (planned v0.3+) will document their own threat model |
| Compromised signing keys | Keep Ed25519 private keys in a hardware token or encrypted key store |

---

## Cryptographic Primitives

| Primitive | Use | Library |
|---|---|---|
| BLAKE3 | Content addressing, policy hashing, journal integrity | `blake3` crate |
| Ed25519 | Record seal signatures | `ed25519-dalek` crate |

No custom cryptographic code is written. Ultnas uses well-audited Rust crates for all cryptographic operations.

---

## Key Management

Ultnas does **not** manage your Ed25519 signing keys. You are responsible for:

1. Generating keys outside of Ultnas (e.g. with `openssl`, `age`, or a hardware token)
2. Providing the signing key at seal time
3. Storing the private key securely — in an encrypted keystore, hardware security module, or system keychain

The public key is stored in every `RecordSeal` and is the authoritative identity for that seal.

---

## Audit Log

Every mutation to the vault is written to `journal.log` — an append-only, newline-delimited JSON file. The journal cannot be selectively modified without leaving detectable gaps. Use `ultnas journal verify` (planned v0.2) to check journal continuity.

---

## Vulnerability Disclosure

**Please do not open a public GitHub Issue for security vulnerabilities.**

### Reporting Process

1. **Email** `sovereignarchivist@outlook.com` with subject: `[SECURITY] Ultnas — <brief description>`
2. Include: affected version, reproduction steps, impact assessment, and any proof-of-concept
3. You will receive an acknowledgment within **48 hours**
4. A fix will be targeted within **14 days** for critical issues, **30 days** for moderate issues
5. You will be credited in the release notes unless you prefer to remain anonymous

Alternatively, use **GitHub's private security advisory** feature:
`https://github.com/sovereignarchivist/ultnas/security/advisories/new`

### Severity Classification

| Severity | Examples |
|---|---|
| Critical | Remote code execution, cryptographic bypass, vault data destruction |
| High | Local privilege escalation, seal forgery, integrity check bypass |
| Moderate | Denial of service, information disclosure, policy bypass |
| Low | Minor information leakage, non-exploitable edge cases |

### Scope

In scope: `ultnas-core`, `ultnas-cli`, `ultnas-daemon`, the vault file format, the IPC protocol.

Out of scope: Third-party dependency vulnerabilities (report upstream), social engineering, and attacks requiring physical device access.
