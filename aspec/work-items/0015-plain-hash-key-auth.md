# Work Item: Task

Title: Plain-hash key authentication (SHA-256 for high-entropy tokens)
Issue: issuelink

## Summary:
- BAE's bearer keys — `bae_` client, `bae_ses_` session, `bae_admin_` admin — are **192-bit tokens drawn from the OS CSPRNG**. Because the secret is already high-entropy, the correct at-rest scheme is a single fast cryptographic hash, not a password KDF: the entire justification for a slow, memory-hard hash (making each brute-force guess of a *guessable* secret expensive) does not apply to a uniformly random 192-bit value, where an attacker holding the full hash table faces ~2^192 preimage work regardless of the hash function.

- This work item establishes **SHA-256** as the one hashing scheme for all three key roles, across `baesrv` and `baectl`. Each key's `key_hash` is the lowercase-hex SHA-256 digest of its plaintext token (64 hex chars). Verification recomputes the digest and compares it against the stored value in **constant time** (`subtle::ConstantTimeEq`), exactly as today. Key generation, entropy (24 bytes = 192 bits), the `key_prefix` display/selector column, the `role`-scoped lookups, and the constant-time posture are all unchanged — only the hash function changes.

- The old placeholder used Argon2id (64 MiB / t=3 / p=1). That cost ran on **every authenticated request**, including every JSON-RPC call in the streaming message loop, added tens-to-hundreds of ms and a 64 MiB allocation per verification, and forced a debug/test-only "cheap params" hack to keep the suite under its timeouts. It bought nothing against 192-bit tokens. It is removed entirely — no Argon2 dependency, no PHC strings, no per-build parameter split.

- **No migration, no dual-path verification.** BAE has no external users and no tagged release; there are no production key hashes to preserve. `verify_key` understands exactly one format (SHA-256 hex) and nothing else. Any local development keys created under the old scheme are simply re-created. The `keys` table schema is unchanged (`key_hash` is still `TEXT`), so no SQLite migration is added.

## User Stories

### User Story 1:
As a: Agent Developer

I want to:
send messages and drive sessions through the client harness without each authenticated request paying a heavy key-derivation cost on the server

So I can:
get low, predictable per-request latency in the hot JSON-RPC message loop, since a session key is re-verified on every `session.sendMessage`, `session.subscribe`, `registerDriver`, event replay, and close.

### User Story 2:
As a: Platform Operator

I want to:
pre-provision one shared admin credential across several independently-running replicas by copying a single hash file onto each, with no shared secret and no coordinated parameters

So I can:
stand up a multi-instance deployment where `baectl auth create key` produces a hash every replica ingests identically — the digest is a plain SHA-256 of the token, so the two independent implementations agree by construction, with nothing to tune or keep in sync.

## Implementation Details:

### Hashing scheme (`server/src/store/keys.rs`)
- `hash_key(plaintext) -> String`: return the lowercase-hex SHA-256 digest of the token bytes (64 chars). Infallible — no salt, no parameters, no fallible hasher construction. Reuse the existing `to_hex` helper.
- `verify_key(plaintext, stored) -> Result<bool, KeyError>`: recompute `sha256(plaintext)`, hex-encode, and constant-time compare against `stored` with `subtle::ConstantTimeEq`. Return `Err(KeyError::MalformedHash)` only when `stored` is not a well-formed digest (see `is_valid_key_hash`); a well-formed non-matching digest returns `Ok(false)`. Keep the comparison length-independent as today (`ct_eq` on the raw bytes).
- `is_valid_key_hash(stored) -> bool`: true iff `stored` is exactly 64 lowercase hex characters. This replaces the PHC/`argon2id`-algorithm structural check used at admin-hash-file boot validation.
- Remove all Argon2 machinery: the `argon2::*` imports, `hasher()`, every `ARGON2_*` constant, and the entire `#[cfg(debug_assertions)]` / `#[cfg(not(debug_assertions))]` parameter split (there is no longer any reason for tests to hash differently from release — a subtle prod/test divergence goes away with it). Trim `KeyError` to the variants that still occur: drop `Params` and the Argon2-specific `Hash`; keep `MalformedHash` (now "stored key hash is not a 64-char hex digest") and `Db`.
- Update the module-level doc comment to describe the SHA-256 scheme and, in a short **design note**, record *why no salt* (salts defend against precomputation/rainbow tables and cross-record hash-equality leaks — both moot for unique, high-entropy tokens) and *why we keep the candidate-select-then-constant-time-compare shape rather than an indexed `WHERE key_hash = ?` exact-match lookup* (the performance goal is already met by dropping Argon2; retaining the constant-time compare preserves the explicit posture in `aspec/architecture/security.md` without reasoning about SQL-comparison timing on the digest).

### Lookup / authentication paths (`server/src/store/keys.rs`)
- Leave the candidate-selection queries and the shared `authenticate()` verify-and-touch loop structurally as they are: `authenticate_client` narrows by `role='client' AND key_prefix=? AND deleted_at IS NULL`; `authenticate_session` by `role='session' AND name=? AND deleted_at IS NULL`; `authenticate_admin` over all active admin rows. Each candidate is constant-time verified with the new `verify_key`. The verify is now microseconds, so the multi-candidate loops (multiple session keys per session; multiple admin rows) cost nothing meaningful.
- `key_prefix` stays: it is the client-auth selector and the list-display value.

### `baectl` admin keygen (`baectl/src/keygen.rs`)
- Replace Argon2 hashing with `sha256(plaintext)` hex — the same function the server uses, so the two independent implementations agree by construction (the previous "both implement the standard PHC Argon2id format" argument collapses to "both SHA-256 the same bytes").
- `AdminKeyMaterial.key_hash` is now a 64-char hex digest. Remove the `ARGON2_*` constants and the debug/release split here too; drop the `argon2` dependency.
- Update the module doc comment (`admin-key-hash.pem` no longer carries a self-describing PHC salt/cost — it carries a bare SHA-256 hex digest; every replica ingests it identically with no shared secret).

### Admin-auth bootstrap (`server/src/admin_auth.rs`)
- `AdminKeyHashFile.key_hash` is a SHA-256 hex string; update its doc comment and the `MalformedHashFile` detail text (was "key_hash is not a valid Argon2id PHC string" → "key_hash is not a valid SHA-256 hex digest"). Validation continues to run once at boot via `keys::is_valid_key_hash`, now checking the hex shape. The self-generate and ingest paths are otherwise unchanged.

### Dependencies
- `server/Cargo.toml` and `baectl/Cargo.toml`: remove `argon2 = "0.5"`; add `sha2` (RustCrypto). Keep `subtle` (constant-time compare) and `rand` (CSPRNG). Update the crypto-dependency comment in `server/Cargo.toml`.

### Documentation
Update every operator/developer-facing reference to the hashing scheme to describe SHA-256 of a high-entropy token (constant-time verified, unsalted, no tunable parameters). At minimum:
- `aspec/architecture/security.md` (line ~10 — "stored only as Argon2id hashes … verified in constant time").
- `aspec/devops/operations.md` (lines ~22–23 — the storage claim and the entire Argon2id-parameters/PHC tuning paragraph, which no longer applies; replace with a one-line note that keys are high-entropy tokens stored as unsalted SHA-256 digests, nothing to tune).
- `aspec/uxui/setup.md` (~17), `aspec/uxui/cli.md` (~30 — "pre-provisioned Argon2id hash file" → "pre-provisioned key-hash file").
- `docs/guides/09-admin-authentication.md` (~23, ~193, ~202 example, ~228, ~249–252 — the independent-implementations paragraph simplifies; the JSON example `key_hash` becomes a 64-hex string).
- `docs/reference/02-admin-api.md` (~227, and the Key-security section ~551–557, including the algorithm table row).
- `docs/reference/03-baectl.md` (~335 example, ~345–347 — the hashing description and the PHC self-describing paragraph).
- `docs/reference/05-configuration.md` (~29 — `BAE_ADMIN_KEY_HASH_FILE` description).
- `server/src/store/migrations/0002_keys.sql` (line ~6 comment — "`key_hash` is an Argon2id PHC string" → "`key_hash` is a SHA-256 hex digest"). The comment only; the column definition does not change and no new migration is added.

## Edge Case Considerations:
- **No dual-path / no format detection.** `verify_key` accepts SHA-256 hex only; a stored value in any other shape is a malformed hash (treated as a non-match during authentication so one corrupt row can't lock out others, and a hard boot error for the ingested admin-hash file). Do not add Argon2-fallback verification "just in case."
- **Local dev keys under the old scheme stop verifying.** This is intended and acceptable pre-release: re-create any local client/admin keys after the change. Call this out in the PR description; nothing in the codebase needs to handle it.
- **Constant-time comparison is retained.** Even though the digest is deterministic, keep the `ConstantTimeEq` compare — do not replace it with a SQL equality lookup on `key_hash`. Timing oracles on key comparison are listed as a real attack surface in the original auth spec, and the compare is now negligibly cheap.
- **Entropy is unchanged and remains the whole basis of security.** Do not reduce `KEY_ENTROPY_BYTES`; the 192-bit token is what makes an unsalted single hash sufficient. The existing "entropy meets floor" tests stay.
- **`is_valid_key_hash` must reject uppercase / wrong-length / non-hex** so a hand-authored or truncated admin-hash file fails loudly at boot rather than silently rejecting every admin request later.
- **`baectl` and server must agree byte-for-byte** on what is hashed (the exact plaintext token string, no trailing newline). A parity test should assert a token hashed by `baectl`'s path verifies under the server's `verify_key` and vice versa.

## Test Considerations:
- **Unit (`keys.rs`)**: `hash_key` produces 64 lowercase hex chars; round-trip verify passes; wrong token fails; a near-miss token (same length, differs in one char) fails via the full-length constant-time path; `is_valid_key_hash` accepts a real digest and rejects uppercase / 63-or-65-char / non-hex / a leftover `$argon2id$…` string. Update the existing tests that assert `hash.starts_with("$argon2id$")` and the two-distinct-salts-same-plaintext test (with no salt, hashing the same token twice now yields the *same* digest — invert that test to assert determinism, and keep a separate assertion that two *different* tokens differ).
- **Unit (`baectl` keygen)**: generated admin key has the right prefix/entropy; its `key_hash` is a valid SHA-256 hex digest of the plaintext. Replace the current Argon2 `PasswordVerifier` round-trip.
- **Cross-implementation parity**: a token hashed by `baectl::keygen` verifies under `keys::verify_key`, and a server-minted key's hash verifies against a `baectl`-recomputed digest — guarding the "independent implementations agree" property that the pre-provisioning flow depends on.
- **Integration**: existing admin-auth and `baectl_cli` tests that construct or assert on Argon2 PHC hashes (`server/tests/admin_auth.rs`, `server/tests/baectl_cli.rs`, `server/tests/e2e_telemetry.rs`) must be updated to the hex-digest format. The end-to-end auth behavior (valid key → 200, deleted/wrong key → 401, session key on wrong session → 401, malformed admin-hash file → exit 2) is unchanged and must still pass.
- All tests continue to run offline. With the debug/release parameter split gone, remove any test that only existed to exercise the cheap-vs-full Argon2 cost.

## Codebase Integration:
- Follow established conventions, best practices, testing, and architecture patterns from the project's aspec.
- Key generation/hashing/comparison stays isolated in `server/src/store/keys.rs`; `baectl`'s independent hasher stays in `baectl/src/keygen.rs`. No auth logic moves.
- The `keys` table schema and the `key_hash TEXT` column are unchanged; **do not** add a SQLite migration. Only the migration's descriptive comment is edited.
- The admin-key self-generate / ingest / rotate lifecycle in `server/src/admin_auth.rs` is preserved verbatim except for the hash-format validation and doc text.
- Confirm `make test-server`, `make test-baectl` (or the equivalent target), and a release build (`make image`) pass — the release build is where the removed `cfg(debug_assertions)` split previously changed behavior, so it must be exercised.
