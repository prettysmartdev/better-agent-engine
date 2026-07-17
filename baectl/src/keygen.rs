//! Local admin-key generation for `baectl auth create key`.
//!
//! This is the ONE command that never touches the network. It pre-provisions a
//! shared admin credential for multi-replica deployments by producing the same
//! two artifacts the server understands:
//!
//! - `admin-key.pem` — the plaintext `bae_admin_<random>` token (live
//!   credential; keep it secret). Copied to wherever `baectl`/operators run, at
//!   `BAE_ADMIN_KEY_FILE`'s path.
//! - `admin-key-hash.pem` — a small JSON document holding the Argon2id PHC hash
//!   of that token plus its `prefix`/`name`. Dropped onto every replica's data
//!   volume at `BAE_ADMIN_KEY_HASH_FILE`'s path; each independently-running
//!   server ingests the identical hash at boot.
//!
//! The Argon2id parameters and key format below are pinned to match
//! `server/src/store/keys.rs` **exactly** (64 MiB / t=3 / p=1 / 32-byte output;
//! `bae_admin_` prefix; 24 bytes = 192 bits of CSPRNG entropy) — including that
//! module's debug/test-only drop to a cheap memory/iteration cost (see the
//! `cfg(debug_assertions)` constants below). Because Argon2id's PHC string
//! embeds its own salt and cost parameters, the server verifies this hash with
//! no shared code — both sides just implement standard PHC Argon2id, and a hash
//! minted by either cost verifies under the other. Shipped release binaries on
//! both sides always use the full floor.

use argon2::password_hash::rand_core::OsRng as SaltRng;
use argon2::password_hash::SaltString;
use argon2::{Algorithm, Argon2, Params, PasswordHasher, Version};
use rand::rngs::OsRng;
use rand::RngCore;

/// Plaintext prefix on every admin key — matches `keys::ADMIN_KEY_PREFIX`.
pub const ADMIN_KEY_PREFIX: &str = "bae_admin_";
/// Random bytes drawn per key: 24 bytes = 192 bits (≥ 128), matching the server.
const KEY_ENTROPY_BYTES: usize = 24;

// --- Argon2id parameters (must match server/src/store/keys.rs) ---
// Release builds — the only thing shipped — use the full production floor;
// debug/test builds drop to a cheap cost so unoptimized Argon2 doesn't make
// hashing sluggish. Verification reads the cost from the PHC string, so a hash
// minted under either profile still verifies under the other.
#[cfg(not(debug_assertions))]
const ARGON2_MEMORY_KIB: u32 = 64 * 1024; // 64 MiB (production floor)
#[cfg(not(debug_assertions))]
const ARGON2_ITERATIONS: u32 = 3;
#[cfg(debug_assertions)]
const ARGON2_MEMORY_KIB: u32 = 64; // debug/test only — cheap, never shipped
#[cfg(debug_assertions)]
const ARGON2_ITERATIONS: u32 = 1;
const ARGON2_PARALLELISM: u32 = 1;
const ARGON2_OUTPUT_LEN: usize = 32;

/// A freshly generated admin credential pair.
pub struct AdminKeyMaterial {
    /// The `bae_admin_<48 hex>` plaintext token. Shown/stored once.
    pub plaintext: String,
    /// Display prefix, e.g. `bae_admin_1a2b` — matches the documented hash-file
    /// example. Display-only for the server (not used in auth lookup).
    pub prefix: String,
    /// The Argon2id PHC hash of `plaintext`.
    pub key_hash: String,
}

/// Generate a new admin token and its Argon2id hash.
///
/// Returns an error only if the Argon2 parameters or hashing were rejected —
/// which cannot happen with the fixed constants above, but the fallible surface
/// is preserved rather than unwrapped so a future parameter change fails loudly.
pub fn generate() -> Result<AdminKeyMaterial, String> {
    let mut bytes = [0u8; KEY_ENTROPY_BYTES];
    // `OsRng` is a cryptographically secure, OS-backed RNG; `fill_bytes` cannot
    // partially fill or silently fall back.
    OsRng.fill_bytes(&mut bytes);
    let hex = to_hex(&bytes);
    let plaintext = format!("{ADMIN_KEY_PREFIX}{hex}");
    // `bae_admin_` (10 chars) + 4 hex, matching the documented `admin-key-hash.pem`
    // example (`"prefix": "bae_admin_1a2b"`).
    let prefix = format!("{ADMIN_KEY_PREFIX}{}", &hex[..4]);

    let key_hash = hash(&plaintext)?;
    Ok(AdminKeyMaterial {
        plaintext,
        prefix,
        key_hash,
    })
}

/// Hash a plaintext token with Argon2id, returning a PHC string.
fn hash(plaintext: &str) -> Result<String, String> {
    let params = Params::new(
        ARGON2_MEMORY_KIB,
        ARGON2_ITERATIONS,
        ARGON2_PARALLELISM,
        Some(ARGON2_OUTPUT_LEN),
    )
    .map_err(|e| format!("invalid Argon2 parameters: {e}"))?;
    let argon2 = Argon2::new(Algorithm::Argon2id, Version::V0x13, params);
    let salt = SaltString::generate(&mut SaltRng);
    let hash = argon2
        .hash_password(plaintext.as_bytes(), &salt)
        .map_err(|e| format!("key hashing failed: {e}"))?;
    Ok(hash.to_string())
}

/// Lowercase-hex encode, no external dependency (matches the server).
fn to_hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for &b in bytes {
        out.push(HEX[(b >> 4) as usize] as char);
        out.push(HEX[(b & 0x0f) as usize] as char);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use argon2::{PasswordHash, PasswordVerifier};

    #[test]
    fn generated_key_has_admin_prefix_and_entropy() {
        let m = generate().unwrap();
        assert!(m.plaintext.starts_with(ADMIN_KEY_PREFIX));
        // 24 bytes -> 48 hex chars of body after the prefix.
        assert_eq!(m.plaintext.len(), ADMIN_KEY_PREFIX.len() + 48);
        assert!(m.prefix.starts_with(ADMIN_KEY_PREFIX));
    }

    #[test]
    fn hash_round_trips_and_rejects_wrong_plaintext() {
        let m = generate().unwrap();
        let parsed = PasswordHash::new(&m.key_hash).unwrap();
        assert_eq!(parsed.algorithm.as_str(), "argon2id");
        // Verifies against its own plaintext...
        assert!(Argon2::default()
            .verify_password(m.plaintext.as_bytes(), &parsed)
            .is_ok());
        // ...and rejects a different one.
        assert!(Argon2::default()
            .verify_password(b"bae_admin_wrong", &parsed)
            .is_err());
    }
}
