// SPDX-License-Identifier: Apache-2.0

//! `KeyMaterial`: holds the raw 32-byte SQLCipher key derived once at startup
//! from the user passphrase via Argon2id.
//!
//! Per ADR-0003 §P8-F:
//!   - Argon2id, m_cost = 64 MiB, t_cost = 3, p_cost = 4 (~500 ms one-time)
//!   - 16-byte salt, persisted in `solo.config.toml` (the salt is NOT secret)
//!   - 32-byte output key, formatted as `x'<hex>'` for SQLCipher's `PRAGMA key`
//!   - Wrapped in `Zeroizing<[u8; 32]>` so the key is wiped from memory on drop
//!
//! The Argon2 salt is distinct from SQLCipher's per-database salt (which lives
//! in the file header and feeds HMAC subkey derivation). Don't conflate them.

use argon2::{Algorithm, Argon2, Params, Version};
use solo_core::{Error, Result};
use zeroize::Zeroizing;

/// Argon2id `m_cost`, in KiB. 64 MiB.
pub const ARGON2_M_COST_KIB: u32 = 64 * 1024;
/// Argon2id `t_cost` (iterations).
pub const ARGON2_T_COST: u32 = 3;
/// Argon2id `p_cost` (parallelism).
pub const ARGON2_P_COST: u32 = 4;
/// SQLCipher raw key length (256 bits).
pub const KEY_LEN: usize = 32;
/// Argon2 salt length. 128 bits is the OWASP recommendation for password
/// hashing; SQLCipher's own per-database salt is separate.
pub const SALT_LEN: usize = 16;

/// Raw 32-byte SQLCipher key. Always wrapped in `Zeroizing` so the underlying
/// bytes are wiped when the value drops.
///
/// The struct is `Clone` (each clone produces its own zeroized buffer) but
/// deliberately does NOT impl `Copy` — that would defeat zeroization.
pub struct KeyMaterial {
    raw: Zeroizing<[u8; KEY_LEN]>,
}

impl KeyMaterial {
    /// Derive a 32-byte SQLCipher key from a UTF-8 passphrase + 16-byte salt.
    ///
    /// Argon2id with the parameters specified in ADR-0003 §P8-F. Takes ~500 ms
    /// on a modern laptop — call this once at daemon startup, never per-write.
    pub fn derive(passphrase: &str, salt: &[u8; SALT_LEN]) -> Result<Self> {
        let params = Params::new(
            ARGON2_M_COST_KIB,
            ARGON2_T_COST,
            ARGON2_P_COST,
            Some(KEY_LEN),
        )
        .map_err(|e| Error::storage(format!("argon2 params: {e}")))?;
        let argon2 = Argon2::new(Algorithm::Argon2id, Version::V0x13, params);

        let mut out: Zeroizing<[u8; KEY_LEN]> = Zeroizing::new([0u8; KEY_LEN]);
        argon2
            .hash_password_into(passphrase.as_bytes(), salt, out.as_mut())
            .map_err(|e| Error::storage(format!("argon2 hash: {e}")))?;
        Ok(Self { raw: out })
    }

    /// Generate a fresh cryptographically random 16-byte salt for first-run
    /// setup. Persist in `solo.config.toml` alongside the database.
    pub fn fresh_salt() -> Result<[u8; SALT_LEN]> {
        let mut salt = [0u8; SALT_LEN];
        getrandom::getrandom(&mut salt).map_err(|e| Error::storage(format!("getrandom: {e}")))?;
        Ok(salt)
    }

    /// Raw 32-byte key as 64-character lowercase hex, wrapped in
    /// `Zeroizing` so the underlying buffer is wiped on drop. Used to
    /// build the `PRAGMA key = "x'<hex>'"` statement on every fresh
    /// SQLCipher connection.
    ///
    /// Callers should hold the returned value just long enough to build
    /// the PRAGMA, then let it drop. The PRAGMA string itself isn't
    /// wrapped — once it's been formatted into a static prefix +
    /// dynamic hex + static suffix, the resulting `String` doesn't
    /// zeroize on its own. That's a known v0.2 hardening item; for now
    /// the smaller `Zeroizing<String>` from this method is the cleanest
    /// boundary.
    pub fn as_hex(&self) -> Zeroizing<String> {
        Zeroizing::new(hex::encode(self.raw.as_ref()))
    }

    /// Derive a domain-separated 32-byte subkey without exposing the raw
    /// SQLCipher key to callers. Used for tenant-local sidecar encryption
    /// such as retained asset blobs.
    pub fn derive_subkey(&self, context: &[u8]) -> Zeroizing<[u8; KEY_LEN]> {
        let hash = blake3::keyed_hash(&self.raw, context);
        Zeroizing::new(*hash.as_bytes())
    }

    /// Constant-time-ish equality. For tests + property checks; production
    /// code should never need to compare keys directly.
    #[cfg(test)]
    fn eq_for_test(&self, other: &Self) -> bool {
        self.raw.as_ref() == other.raw.as_ref()
    }

    /// Build a `KeyMaterial` from a fixed 32-byte buffer. Test-only —
    /// production code must use `derive`.
    #[cfg(any(test, feature = "test-support"))]
    pub fn from_bytes_for_tests(bytes: [u8; KEY_LEN]) -> Self {
        Self {
            raw: Zeroizing::new(bytes),
        }
    }
}

impl Clone for KeyMaterial {
    fn clone(&self) -> Self {
        Self {
            raw: Zeroizing::new(*self.raw),
        }
    }
}

impl std::fmt::Debug for KeyMaterial {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("KeyMaterial { raw: <redacted> }")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Lower-cost params for tests so the suite stays fast. Production callers
    /// must use `derive` (which uses the full params).
    fn derive_fast(passphrase: &str, salt: &[u8; SALT_LEN]) -> Result<KeyMaterial> {
        let params = Params::new(8, 1, 1, Some(KEY_LEN))
            .map_err(|e| Error::storage(format!("params: {e}")))?;
        let argon2 = Argon2::new(Algorithm::Argon2id, Version::V0x13, params);
        let mut out: Zeroizing<[u8; KEY_LEN]> = Zeroizing::new([0u8; KEY_LEN]);
        argon2
            .hash_password_into(passphrase.as_bytes(), salt, out.as_mut())
            .map_err(|e| Error::storage(format!("hash: {e}")))?;
        Ok(KeyMaterial { raw: out })
    }

    #[test]
    fn derive_is_deterministic_with_same_salt() {
        let salt = [0u8; SALT_LEN];
        let a = derive_fast("hunter2", &salt).unwrap();
        let b = derive_fast("hunter2", &salt).unwrap();
        assert!(a.eq_for_test(&b));
        assert_eq!(&*a.as_hex(), &*b.as_hex());
    }

    #[test]
    fn derive_differs_with_different_salt() {
        let s1 = [0u8; SALT_LEN];
        let mut s2 = [0u8; SALT_LEN];
        s2[0] = 1;
        let a = derive_fast("hunter2", &s1).unwrap();
        let b = derive_fast("hunter2", &s2).unwrap();
        assert!(!a.eq_for_test(&b));
    }

    #[test]
    fn derive_differs_with_different_passphrase() {
        let salt = [0u8; SALT_LEN];
        let a = derive_fast("hunter2", &salt).unwrap();
        let b = derive_fast("hunter3", &salt).unwrap();
        assert!(!a.eq_for_test(&b));
    }

    #[test]
    fn fresh_salt_is_random() {
        let s1 = KeyMaterial::fresh_salt().unwrap();
        let s2 = KeyMaterial::fresh_salt().unwrap();
        // Probability of collision is 2^-128 — if this ever fails, getrandom
        // is broken.
        assert_ne!(s1, s2);
    }

    #[test]
    fn as_hex_has_correct_length_and_charset() {
        let salt = [0u8; SALT_LEN];
        let k = derive_fast("hunter2", &salt).unwrap();
        let h = k.as_hex();
        assert_eq!(h.len(), KEY_LEN * 2);
        assert!(
            h.chars()
                .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
        );
    }

    #[test]
    fn derive_subkey_is_domain_separated() {
        let key = KeyMaterial::from_bytes_for_tests([7u8; KEY_LEN]);
        let a = key.derive_subkey(b"domain-a");
        let b = key.derive_subkey(b"domain-b");
        assert_ne!(&*a, &*b);
    }

    #[test]
    fn debug_redacts_key_material() {
        let salt = [0u8; SALT_LEN];
        let k = derive_fast("hunter2", &salt).unwrap();
        let dbg = format!("{k:?}");
        assert!(dbg.contains("redacted"));
        assert!(!dbg.contains(&k.as_hex()[..8]));
    }
}
