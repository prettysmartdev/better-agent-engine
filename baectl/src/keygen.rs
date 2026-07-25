//! Local admin-key generation for `baectl auth create key`.
//!
//! This is the ONE command that never touches the network. It pre-provisions a
//! shared admin credential for multi-replica deployments by producing the same
//! two artifacts the server understands:
//!
//! - `admin-key.pem` — the plaintext `bae_admin_<random>` token (live
//!   credential; keep it secret). Copied to wherever `baectl`/operators run, at
//!   `BAE_ADMIN_KEY_FILE`'s path.
//! - `admin-key-hash.pem` — a small JSON document holding the bare SHA-256 hex
//!   digest of that token plus its `prefix`/`name`. Dropped onto every
//!   replica's data volume at `BAE_ADMIN_KEY_HASH_FILE`'s path; each
//!   independently-running server ingests the identical hash at boot.
//!
//! The hashing and key format below are pinned to match
//! `server/src/store/keys.rs` **exactly** (unsalted SHA-256, lowercase-hex
//! encoded; `bae_admin_` prefix; 24 bytes = 192 bits of CSPRNG entropy). Unlike
//! a self-describing PHC hash, a bare digest carries no salt or cost
//! parameters — there's nothing to tune and no shared secret, so every replica
//! ingests it identically.

use rand::rngs::OsRng;
use rand::RngCore;
use sha2::{Digest, Sha256};

/// Plaintext prefix on every admin key — matches `keys::ADMIN_KEY_PREFIX`.
pub const ADMIN_KEY_PREFIX: &str = "bae_admin_";
/// Random bytes drawn per key: 24 bytes = 192 bits (≥ 128), matching the server.
const KEY_ENTROPY_BYTES: usize = 24;

/// A freshly generated admin credential pair.
pub struct AdminKeyMaterial {
    /// The `bae_admin_<48 hex>` plaintext token. Shown/stored once.
    pub plaintext: String,
    /// Display prefix, e.g. `bae_admin_1a2b` — matches the documented hash-file
    /// example. Display-only for the server (not used in auth lookup).
    pub prefix: String,
    /// The lowercase-hex SHA-256 digest of `plaintext`.
    pub key_hash: String,
}

/// Generate a new admin token and its SHA-256 hash.
///
/// Returns an error only if hashing were rejected — which cannot happen with
/// this fixed, unsalted SHA-256 computation, but the fallible surface is
/// preserved so a future change fails loudly rather than being unwrapped.
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

    let key_hash = hash(&plaintext);
    Ok(AdminKeyMaterial {
        plaintext,
        prefix,
        key_hash,
    })
}

/// Hash a plaintext token with SHA-256, returning a lowercase-hex digest.
fn hash(plaintext: &str) -> String {
    to_hex(&Sha256::digest(plaintext.as_bytes()))
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
    use baesrv::store::keys;

    fn is_lowercase_sha256_hex(value: &str) -> bool {
        value.len() == 64
            && value
                .bytes()
                .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
    }

    #[test]
    fn generated_admin_key_has_prefix_entropy_and_sha256_digest() {
        let material = generate().expect("SHA-256 key generation succeeds");
        assert!(material.plaintext.starts_with(ADMIN_KEY_PREFIX));
        assert_eq!(
            material.plaintext.len(),
            ADMIN_KEY_PREFIX.len() + KEY_ENTROPY_BYTES * 2
        );
        let entropy_bits = (material.plaintext.len() - ADMIN_KEY_PREFIX.len()) / 2 * 8;
        assert!(
            entropy_bits >= 128,
            "admin key entropy {entropy_bits} bits < 128"
        );
        assert_eq!(
            material.prefix,
            material.plaintext[..ADMIN_KEY_PREFIX.len() + 4]
        );
        assert!(is_lowercase_sha256_hex(&material.key_hash));
        assert_eq!(material.key_hash, hash(&material.plaintext));
    }

    #[test]
    fn baectl_hash_verifies_with_server_implementation() {
        let material = generate().expect("SHA-256 key generation succeeds");
        assert!(keys::verify_key(&material.plaintext, &material.key_hash).unwrap());
    }

    #[test]
    fn server_hash_matches_baectl_recomputation() {
        let server_key = keys::generate_admin_key();
        let server_hash = keys::hash_key(&server_key.plaintext);
        assert_eq!(server_hash, hash(&server_key.plaintext));
    }
}
