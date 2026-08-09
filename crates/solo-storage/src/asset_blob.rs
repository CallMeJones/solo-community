// SPDX-License-Identifier: Apache-2.0

//! Encryption helpers for tenant-local retained asset blobs.
//!
//! The `assets.sha256` column remains the plaintext content hash for stable
//! identity, API contracts, and dedupe. The bytes on disk may be AEAD
//! ciphertext; encryption metadata lives in SQL alongside the asset row.

use chacha20poly1305::aead::{Aead, Payload};
use chacha20poly1305::{KeyInit, XChaCha20Poly1305, XNonce};
use sha2::{Digest, Sha256};
use solo_core::{Error, Result};

use crate::key_material::KeyMaterial;

pub const ASSET_BLOB_PLAINTEXT_ALG: &str = "none";
pub const ASSET_BLOB_ENCRYPTION_ALG: &str = "xchacha20poly1305-blake3-v1";
pub const ASSET_BLOB_NONCE_LEN: usize = 24;

const ASSET_BLOB_KEY_CONTEXT: &[u8] = b"solo.asset-blob.xchacha20poly1305.v1";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EncryptedAssetBlob {
    pub ciphertext: Vec<u8>,
    pub nonce: Vec<u8>,
}

pub fn encrypt_asset_blob(
    key: &KeyMaterial,
    plaintext: &[u8],
    plaintext_sha256: &str,
    plaintext_size_bytes: u64,
) -> Result<EncryptedAssetBlob> {
    let mut nonce = [0u8; ASSET_BLOB_NONCE_LEN];
    getrandom::getrandom(&mut nonce)
        .map_err(|e| Error::storage(format!("asset blob nonce: {e}")))?;
    let cipher = cipher_for(key);
    let ciphertext = cipher
        .encrypt(
            XNonce::from_slice(&nonce),
            Payload {
                msg: plaintext,
                aad: aad(plaintext_sha256, plaintext_size_bytes).as_bytes(),
            },
        )
        .map_err(|e| Error::storage(format!("encrypt asset blob: {e}")))?;
    Ok(EncryptedAssetBlob {
        ciphertext,
        nonce: nonce.to_vec(),
    })
}

pub fn decode_asset_blob(
    key: Option<&KeyMaterial>,
    encryption_alg: &str,
    encryption_nonce: Option<&[u8]>,
    plaintext_sha256: &str,
    plaintext_size_bytes: u64,
    stored_bytes: &[u8],
) -> Result<Vec<u8>> {
    let plaintext = match normalize_encryption_alg(encryption_alg) {
        ASSET_BLOB_PLAINTEXT_ALG => Ok(stored_bytes.to_vec()),
        ASSET_BLOB_ENCRYPTION_ALG => {
            let key = key.ok_or_else(|| {
                Error::storage("asset blob is encrypted but the library key is unavailable")
            })?;
            let nonce = encryption_nonce.ok_or_else(|| {
                Error::storage("asset blob is encrypted but encryption_nonce is missing")
            })?;
            if nonce.len() != ASSET_BLOB_NONCE_LEN {
                return Err(Error::storage(format!(
                    "asset blob encryption_nonce must be {ASSET_BLOB_NONCE_LEN} bytes, got {}",
                    nonce.len()
                )));
            }
            let cipher = cipher_for(key);
            cipher
                .decrypt(
                    XNonce::from_slice(nonce),
                    Payload {
                        msg: stored_bytes,
                        aad: aad(plaintext_sha256, plaintext_size_bytes).as_bytes(),
                    },
                )
                .map_err(|e| Error::storage(format!("decrypt asset blob: {e}")))
        }
        other => Err(Error::storage(format!(
            "unsupported asset blob encryption_alg {other:?}"
        ))),
    }?;
    validate_plaintext(&plaintext, plaintext_sha256, plaintext_size_bytes)?;
    Ok(plaintext)
}

pub fn expected_stored_size(
    encryption_alg: &str,
    plaintext_size_bytes: u64,
    encrypted_size_bytes: Option<u64>,
) -> Result<u64> {
    match normalize_encryption_alg(encryption_alg) {
        ASSET_BLOB_PLAINTEXT_ALG => Ok(plaintext_size_bytes),
        ASSET_BLOB_ENCRYPTION_ALG => encrypted_size_bytes
            .ok_or_else(|| Error::storage("encrypted asset blob is missing encrypted_size_bytes")),
        other => Err(Error::storage(format!(
            "unsupported asset blob encryption_alg {other:?}"
        ))),
    }
}

pub fn normalize_encryption_alg(value: &str) -> &str {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        ASSET_BLOB_PLAINTEXT_ALG
    } else {
        trimmed
    }
}

fn cipher_for(key: &KeyMaterial) -> XChaCha20Poly1305 {
    let subkey = key.derive_subkey(ASSET_BLOB_KEY_CONTEXT);
    XChaCha20Poly1305::new(subkey.as_ref().into())
}

fn aad(plaintext_sha256: &str, plaintext_size_bytes: u64) -> String {
    format!("solo.asset-blob.v1\0{plaintext_sha256}\0{plaintext_size_bytes}")
}

fn validate_plaintext(
    plaintext: &[u8],
    expected_sha256: &str,
    expected_size_bytes: u64,
) -> Result<()> {
    if plaintext.len() as u64 != expected_size_bytes {
        return Err(Error::storage(format!(
            "asset blob plaintext size mismatch: expected {expected_size_bytes}, got {}",
            plaintext.len()
        )));
    }
    let expected_sha256 = expected_sha256.trim().to_ascii_lowercase();
    let actual_sha256 = hex::encode(Sha256::digest(plaintext));
    if actual_sha256 != expected_sha256 {
        return Err(Error::storage(format!(
            "asset blob plaintext sha256 mismatch: expected {expected_sha256}, got {actual_sha256}"
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encrypted_asset_blob_round_trips_and_authenticates_metadata() {
        let key = KeyMaterial::from_bytes_for_tests([9u8; 32]);
        let plaintext = b"private asset bytes";
        let sha256 = hex::encode(Sha256::digest(plaintext));
        let encrypted =
            encrypt_asset_blob(&key, plaintext, &sha256, plaintext.len() as u64).unwrap();

        assert_ne!(encrypted.ciphertext, plaintext);
        let decoded = decode_asset_blob(
            Some(&key),
            ASSET_BLOB_ENCRYPTION_ALG,
            Some(&encrypted.nonce),
            &sha256,
            plaintext.len() as u64,
            &encrypted.ciphertext,
        )
        .unwrap();
        assert_eq!(decoded, plaintext);

        let err = decode_asset_blob(
            Some(&key),
            ASSET_BLOB_ENCRYPTION_ALG,
            Some(&encrypted.nonce),
            &sha256,
            plaintext.len() as u64 + 1,
            &encrypted.ciphertext,
        )
        .unwrap_err();
        assert!(err.to_string().contains("decrypt asset blob"));
    }
}
