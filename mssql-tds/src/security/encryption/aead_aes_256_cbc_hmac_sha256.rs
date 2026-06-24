// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Implementation of the `AEAD_AES_256_CBC_HMAC_SHA256` cell-encryption
//! algorithm used by Always Encrypted.
//!
//! The algorithm derives three keys (encryption, MAC, IV) from a 256-bit column
//! encryption key (the "root key") and produces a self-describing cell blob:
//!
//! ```text
//! cell_blob = version (1 byte)
//!           || authentication_tag (32 bytes, HMAC-SHA256)
//!           || iv (16 bytes)
//!           || ciphertext (AES-256-CBC, PKCS7 padded)
//! ```
//!
//! Key derivation, the cell layout, and the authentication-tag computation match
//! the reference implementations in Microsoft.Data.SqlClient and the JDBC driver
//! so that values encrypted by this crate are interoperable with SQL Server and
//! other Always Encrypted clients.

// The cipher is a self-contained primitive; its callers (parameter encryption and
// result-set decryption) land in later Always Encrypted phases.
#![allow(dead_code)]

use cbc::cipher::block_padding::Pkcs7;
use cbc::cipher::{BlockDecryptMut, BlockEncryptMut, KeyIvInit};
use hmac::{Hmac, Mac};
use rand_core::{OsRng, RngCore};
use sha2::Sha256;

use crate::core::TdsResult;
use crate::error::Error;

/// HMAC-SHA256 used for key derivation and the cell authentication tag.
type HmacSha256 = Hmac<Sha256>;
/// AES-256 in CBC mode with PKCS7 padding (encryption side).
type Aes256CbcEnc = cbc::Encryptor<aes::Aes256>;
/// AES-256 in CBC mode with PKCS7 padding (decryption side).
type Aes256CbcDec = cbc::Decryptor<aes::Aes256>;

/// Size in bytes of the column encryption key and each derived key (256-bit).
const KEY_SIZE_IN_BYTES: usize = 32;
/// AES block size in bytes.
const BLOCK_SIZE_IN_BYTES: usize = 16;
/// Size in bytes of the HMAC-SHA256 authentication tag.
const TAG_SIZE_IN_BYTES: usize = 32;
/// Algorithm version byte stored at the start of every cell blob.
const ALGORITHM_VERSION: u8 = 0x01;
/// Trailing byte fed into the authentication-tag HMAC (the encoded version size).
const VERSION_SIZE_BYTE: u8 = 0x01;

/// Minimum valid cell-blob length: version + tag + iv + one cipher block.
const MIN_CIPHERTEXT_LEN: usize = 1 + TAG_SIZE_IN_BYTES + BLOCK_SIZE_IN_BYTES + BLOCK_SIZE_IN_BYTES;

/// Salt used to derive the encryption key from the root key.
const ENCRYPTION_KEY_SALT: &str = "Microsoft SQL Server cell encryption key with encryption algorithm:AEAD_AES_256_CBC_HMAC_SHA256 and key length:256";
/// Salt used to derive the MAC key from the root key.
const MAC_KEY_SALT: &str = "Microsoft SQL Server cell MAC key with encryption algorithm:AEAD_AES_256_CBC_HMAC_SHA256 and key length:256";
/// Salt used to derive the IV key from the root key.
const IV_KEY_SALT: &str = "Microsoft SQL Server cell IV key with encryption algorithm:AEAD_AES_256_CBC_HMAC_SHA256 and key length:256";

/// Whether a column uses deterministic or randomized encryption.
///
/// Deterministic encryption derives the IV from the plaintext so identical
/// plaintexts produce identical ciphertexts (enabling equality lookups).
/// Randomized encryption uses a cryptographically random IV.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ColumnEncryptionType {
    /// Deterministic encryption (synthetic IV derived from the plaintext).
    Deterministic,
    /// Randomized encryption (random IV).
    Randomized,
}

/// An `AEAD_AES_256_CBC_HMAC_SHA256` cipher initialized from a column
/// encryption key (the 32-byte plaintext root key).
pub(crate) struct AeadAes256CbcHmacSha256 {
    encryption_key: [u8; KEY_SIZE_IN_BYTES],
    mac_key: [u8; KEY_SIZE_IN_BYTES],
    iv_key: [u8; KEY_SIZE_IN_BYTES],
}

impl AeadAes256CbcHmacSha256 {
    /// Derives the encryption, MAC, and IV keys from a 256-bit root key.
    ///
    /// # Errors
    ///
    /// Returns [`Error::ColumnEncryptionError`] if `root_key` is not exactly
    /// 32 bytes or if key derivation fails.
    pub(crate) fn new(root_key: &[u8]) -> TdsResult<Self> {
        if root_key.len() != KEY_SIZE_IN_BYTES {
            return Err(Error::ColumnEncryptionError(format!(
                "Invalid column encryption key size for AEAD_AES_256_CBC_HMAC_SHA256: \
                 expected {KEY_SIZE_IN_BYTES} bytes, got {}",
                root_key.len()
            )));
        }

        Ok(Self {
            encryption_key: derive_key(root_key, ENCRYPTION_KEY_SALT),
            mac_key: derive_key(root_key, MAC_KEY_SALT),
            iv_key: derive_key(root_key, IV_KEY_SALT),
        })
    }

    /// Encrypts `plaintext`, producing a self-describing cell blob.
    ///
    /// # Errors
    ///
    /// Returns [`Error::ColumnEncryptionError`] if any cryptographic operation
    /// fails.
    pub(crate) fn encrypt(
        &self,
        plaintext: &[u8],
        encryption_type: ColumnEncryptionType,
    ) -> TdsResult<Vec<u8>> {
        let iv = match encryption_type {
            ColumnEncryptionType::Deterministic => {
                // Synthetic IV: HMAC-SHA256(iv_key, plaintext) truncated to one block.
                let full = hmac_sha256(&self.iv_key, &[plaintext]);
                full[..BLOCK_SIZE_IN_BYTES].to_vec()
            }
            ColumnEncryptionType::Randomized => {
                let mut iv = vec![0u8; BLOCK_SIZE_IN_BYTES];
                OsRng.fill_bytes(&mut iv);
                iv
            }
        };

        let ciphertext = Aes256CbcEnc::new_from_slices(&self.encryption_key, &iv)
            .map_err(|e| crypto_err(format!("invalid AES key/IV length: {e}")))?
            .encrypt_padded_vec_mut::<Pkcs7>(plaintext);

        let tag = self.authentication_tag(&iv, &ciphertext);

        let mut blob = Vec::with_capacity(1 + TAG_SIZE_IN_BYTES + iv.len() + ciphertext.len());
        blob.push(ALGORITHM_VERSION);
        blob.extend_from_slice(&tag);
        blob.extend_from_slice(&iv);
        blob.extend_from_slice(&ciphertext);
        Ok(blob)
    }

    /// Decrypts a cell blob produced by [`encrypt`](Self::encrypt) (or by any
    /// compatible Always Encrypted client) back to the original plaintext.
    ///
    /// The authentication tag is verified before decryption; a mismatch
    /// indicates tampering or use of the wrong key.
    ///
    /// # Errors
    ///
    /// Returns [`Error::ColumnEncryptionError`] if the blob is malformed, the
    /// version byte is unsupported, the authentication tag does not match, or
    /// decryption fails.
    pub(crate) fn decrypt(&self, blob: &[u8]) -> TdsResult<Vec<u8>> {
        if blob.len() < MIN_CIPHERTEXT_LEN {
            return Err(Error::ColumnEncryptionError(format!(
                "Ciphertext too short: {} bytes (minimum {MIN_CIPHERTEXT_LEN})",
                blob.len()
            )));
        }

        if blob[0] != ALGORITHM_VERSION {
            return Err(Error::ColumnEncryptionError(format!(
                "Unsupported cipher algorithm version: {:#04x} (expected {ALGORITHM_VERSION:#04x})",
                blob[0]
            )));
        }

        let tag = &blob[1..1 + TAG_SIZE_IN_BYTES];
        let iv = &blob[1 + TAG_SIZE_IN_BYTES..1 + TAG_SIZE_IN_BYTES + BLOCK_SIZE_IN_BYTES];
        let ciphertext = &blob[1 + TAG_SIZE_IN_BYTES + BLOCK_SIZE_IN_BYTES..];

        // Constant-time tag verification (HMAC `verify_slice`) to avoid leaking
        // tag bytes via timing.
        if !self.verify_authentication_tag(iv, ciphertext, tag) {
            return Err(Error::ColumnEncryptionError(
                "Authentication tag mismatch; the ciphertext may have been tampered with or the \
                 wrong column encryption key was used"
                    .to_string(),
            ));
        }

        Aes256CbcDec::new_from_slices(&self.encryption_key, iv)
            .map_err(|e| crypto_err(format!("invalid AES key/IV length: {e}")))?
            .decrypt_padded_vec_mut::<Pkcs7>(ciphertext)
            .map_err(|e| crypto_err(format!("AES-CBC decryption failed: {e}")))
    }

    /// Computes the authentication tag over `version || iv || ciphertext || version_size`.
    fn authentication_tag(&self, iv: &[u8], ciphertext: &[u8]) -> Vec<u8> {
        hmac_sha256(
            &self.mac_key,
            &[&[ALGORITHM_VERSION], iv, ciphertext, &[VERSION_SIZE_BYTE]],
        )
    }

    /// Recomputes the authentication tag and compares it against `tag` in
    /// constant time, returning whether they match.
    fn verify_authentication_tag(&self, iv: &[u8], ciphertext: &[u8], tag: &[u8]) -> bool {
        let mut mac =
            HmacSha256::new_from_slice(&self.mac_key).expect("HMAC accepts keys of any length");
        mac.update(&[ALGORITHM_VERSION]);
        mac.update(iv);
        mac.update(ciphertext);
        mac.update(&[VERSION_SIZE_BYTE]);
        mac.verify_slice(tag).is_ok()
    }
}

/// Derives a 256-bit key as `HMAC-SHA256(root_key, utf16le(salt))`.
fn derive_key(root_key: &[u8], salt: &str) -> [u8; KEY_SIZE_IN_BYTES] {
    let derived = hmac_sha256(root_key, &[&utf16le(salt)]);
    let mut key = [0u8; KEY_SIZE_IN_BYTES];
    key.copy_from_slice(&derived);
    key
}

/// Computes `HMAC-SHA256(key, segments[0] || segments[1] || ...)`.
fn hmac_sha256(key: &[u8], segments: &[&[u8]]) -> Vec<u8> {
    let mut mac = HmacSha256::new_from_slice(key).expect("HMAC accepts keys of any length");
    for segment in segments {
        mac.update(segment);
    }
    mac.finalize().into_bytes().to_vec()
}

/// Encodes a string as little-endian UTF-16 bytes (matching .NET `Encoding.Unicode`).
fn utf16le(s: &str) -> Vec<u8> {
    s.encode_utf16().flat_map(u16::to_le_bytes).collect()
}

/// Maps a low-level cryptographic failure into a [`ColumnEncryptionError`](Error::ColumnEncryptionError).
fn crypto_err(msg: impl std::fmt::Display) -> Error {
    Error::ColumnEncryptionError(format!("AEAD cipher error: {msg}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fixed 32-byte root key for deterministic test vectors.
    const ROOT_KEY: [u8; 32] = [
        0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0a, 0x0b, 0x0c, 0x0d, 0x0e, 0x0f,
        0x10, 0x11, 0x12, 0x13, 0x14, 0x15, 0x16, 0x17, 0x18, 0x19, 0x1a, 0x1b, 0x1c, 0x1d, 0x1e,
        0x1f, 0x20,
    ];

    fn hex(bytes: &[u8]) -> String {
        bytes.iter().map(|b| format!("{b:02x}")).collect()
    }

    fn cipher() -> AeadAes256CbcHmacSha256 {
        AeadAes256CbcHmacSha256::new(&ROOT_KEY).unwrap()
    }

    #[test]
    fn new_rejects_wrong_key_size() {
        assert!(AeadAes256CbcHmacSha256::new(&[0u8; 16]).is_err());
        assert!(AeadAes256CbcHmacSha256::new(&[0u8; 31]).is_err());
        assert!(AeadAes256CbcHmacSha256::new(&[0u8; 33]).is_err());
        assert!(AeadAes256CbcHmacSha256::new(&[0u8; 32]).is_ok());
    }

    #[test]
    fn round_trip_deterministic() {
        let c = cipher();
        let plaintext = b"Always Encrypted round trip";
        let blob = c
            .encrypt(plaintext, ColumnEncryptionType::Deterministic)
            .unwrap();
        assert_eq!(c.decrypt(&blob).unwrap(), plaintext);
    }

    #[test]
    fn round_trip_randomized() {
        let c = cipher();
        let plaintext = b"Always Encrypted round trip";
        let blob = c
            .encrypt(plaintext, ColumnEncryptionType::Randomized)
            .unwrap();
        assert_eq!(c.decrypt(&blob).unwrap(), plaintext);
    }

    #[test]
    fn round_trip_empty_plaintext() {
        let c = cipher();
        for enc_type in [
            ColumnEncryptionType::Deterministic,
            ColumnEncryptionType::Randomized,
        ] {
            let blob = c.encrypt(b"", enc_type).unwrap();
            assert_eq!(c.decrypt(&blob).unwrap(), b"");
        }
    }

    #[test]
    fn deterministic_is_stable() {
        let c = cipher();
        let plaintext = b"deterministic";
        let a = c
            .encrypt(plaintext, ColumnEncryptionType::Deterministic)
            .unwrap();
        let b = c
            .encrypt(plaintext, ColumnEncryptionType::Deterministic)
            .unwrap();
        assert_eq!(a, b, "deterministic encryption must be reproducible");
    }

    #[test]
    fn randomized_differs_each_call() {
        let c = cipher();
        let plaintext = b"randomized";
        let a = c
            .encrypt(plaintext, ColumnEncryptionType::Randomized)
            .unwrap();
        let b = c
            .encrypt(plaintext, ColumnEncryptionType::Randomized)
            .unwrap();
        assert_ne!(a, b, "randomized encryption must use a fresh IV each call");
        // Both still decrypt to the same plaintext.
        assert_eq!(c.decrypt(&a).unwrap(), plaintext);
        assert_eq!(c.decrypt(&b).unwrap(), plaintext);
    }

    #[test]
    fn decrypt_rejects_tampered_ciphertext() {
        let c = cipher();
        let mut blob = c
            .encrypt(b"tamper me", ColumnEncryptionType::Randomized)
            .unwrap();
        let last = blob.len() - 1;
        blob[last] ^= 0xff;
        assert!(c.decrypt(&blob).is_err());
    }

    #[test]
    fn decrypt_rejects_tampered_tag() {
        let c = cipher();
        let mut blob = c
            .encrypt(b"tamper the tag", ColumnEncryptionType::Randomized)
            .unwrap();
        blob[1] ^= 0xff; // first byte of the authentication tag
        assert!(c.decrypt(&blob).is_err());
    }

    #[test]
    fn decrypt_rejects_wrong_key() {
        let c = cipher();
        let blob = c
            .encrypt(b"secret", ColumnEncryptionType::Randomized)
            .unwrap();
        let other = AeadAes256CbcHmacSha256::new(&[0xaa; 32]).unwrap();
        assert!(other.decrypt(&blob).is_err());
    }

    #[test]
    fn decrypt_rejects_short_blob() {
        let c = cipher();
        assert!(c.decrypt(&[ALGORITHM_VERSION; 10]).is_err());
    }

    #[test]
    fn decrypt_rejects_bad_version() {
        let c = cipher();
        let mut blob = c
            .encrypt(b"version check", ColumnEncryptionType::Randomized)
            .unwrap();
        blob[0] = 0x02;
        assert!(c.decrypt(&blob).is_err());
    }

    /// Pins the derived keys for a fixed root key. These values are determined
    /// solely by the key-derivation salts and the root key, so a change here
    /// signals an accidental break in wire compatibility with SQL Server.
    #[test]
    fn derived_keys_are_pinned() {
        let c = cipher();
        assert_eq!(
            hex(&c.encryption_key),
            "a95fcb709ee9984771c647c765f5351c3bc77fbf91d0e13c699289d143a4d4d7"
        );
        assert_eq!(
            hex(&c.mac_key),
            "5e63796429de42ebd1a886f948ff46d898a153262c54ee4ac52c338062c05bda"
        );
        assert_eq!(
            hex(&c.iv_key),
            "cc7f08850c91bd6515d404140272116039cfdac80c19cb10def7f25d40661548"
        );
    }

    /// Pins a full deterministic cell blob for fixed inputs. Deterministic
    /// encryption is fully reproducible, so this guards the on-the-wire format.
    #[test]
    fn deterministic_blob_is_pinned() {
        let c = cipher();
        let blob = c
            .encrypt(b"pinned", ColumnEncryptionType::Deterministic)
            .unwrap();
        assert_eq!(
            hex(&blob),
            "019dac660c4165242f51d710f8596605a1617825f2089614c621012369280c2364f3fd11c3adb0c685e19a7c6046bc330d64fe00eb7a744ec7ac22966e4946a040"
        );
    }
}
