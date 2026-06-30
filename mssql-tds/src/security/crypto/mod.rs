// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Platform-native cryptographic primitives for Always Encrypted.
//!
//! Always Encrypted needs AES-256-CBC, HMAC-SHA256, RSA-OAEP(SHA-1),
//! RSASSA-PKCS1v1.5(SHA-256), RSA key generation, and RSA private-key PEM
//! import. The crypto-compliance policy restricts the implementation to
//! approved providers (OpenSSL, Apple native, or Windows native crypto only),
//! so the backend is selected per platform:
//!
//! * **Windows** → Cryptography API: Next Generation (CNG) via `windows-sys`
//!   (`BCrypt*`/`NCrypt*` plus the CryptoAPI `Crypt*` PEM/DER decoders). This
//!   avoids `openssl-sys`, which cannot build on the MSVC toolchain without a
//!   system OpenSSL. The required `windows-sys` `Win32_Security_Cryptography`
//!   bindings are already pulled in by the Schannel-direct TLS engine, so no new
//!   dependency is introduced.
//! * **All other targets** → OpenSSL via the `openssl` crate, which is already
//!   the TLS backend on those platforms. OpenSSL is an approved provider on
//!   every platform.
//!
//! Mirrors the way the TLS layer splits between an in-tree Windows-native engine
//! and a portable backend (see `connection::transport::tls::default_engine`).
//!
//! Every backend produces byte-identical wire output so values encrypted by this
//! crate stay interoperable with SQL Server and the other Always Encrypted
//! drivers regardless of which platform produced them.

// Not every primitive is exercised by every consumer or on every platform; the
// surface is intentionally complete for the cell cipher and CEK wrapping paths.
#![allow(dead_code)]

#[cfg(windows)]
#[path = "cng.rs"]
mod imp;

#[cfg(not(windows))]
#[path = "openssl.rs"]
mod imp;

pub(crate) use imp::RsaKey;

use crate::core::TdsResult;

/// Number of bytes in an AES-256 key (and in each Always Encrypted derived key).
pub(crate) const AES_256_KEY_LEN: usize = 32;
/// Number of bytes in an AES block and in a CBC initialization vector.
pub(crate) const AES_BLOCK_LEN: usize = 16;
/// Number of bytes in an HMAC-SHA256 / SHA-256 digest.
pub(crate) const SHA256_LEN: usize = 32;

/// Fills `buf` with cryptographically secure random bytes.
pub(crate) fn fill_random(buf: &mut [u8]) -> TdsResult<()> {
    imp::fill_random(buf)
}

/// Computes `HMAC-SHA256(key, data)`.
pub(crate) fn hmac_sha256(key: &[u8], data: &[u8]) -> TdsResult<[u8; SHA256_LEN]> {
    imp::hmac_sha256(key, data)
}

/// Encrypts `plaintext` with AES-256-CBC and PKCS#7 padding.
pub(crate) fn aes_256_cbc_encrypt(
    key: &[u8; AES_256_KEY_LEN],
    iv: &[u8; AES_BLOCK_LEN],
    plaintext: &[u8],
) -> TdsResult<Vec<u8>> {
    imp::aes_256_cbc_encrypt(key, iv, plaintext)
}

/// Decrypts AES-256-CBC ciphertext and strips PKCS#7 padding.
pub(crate) fn aes_256_cbc_decrypt(
    key: &[u8; AES_256_KEY_LEN],
    iv: &[u8; AES_BLOCK_LEN],
    ciphertext: &[u8],
) -> TdsResult<Vec<u8>> {
    imp::aes_256_cbc_decrypt(key, iv, ciphertext)
}

/// Compares two byte slices in constant time.
///
/// Used to check authentication tags without leaking how many leading bytes
/// matched via timing. Returns `false` immediately for length mismatches (the
/// length is not secret).
pub(crate) fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn random_fills_buffer() {
        let mut a = [0u8; 32];
        let mut b = [0u8; 32];
        fill_random(&mut a).unwrap();
        fill_random(&mut b).unwrap();
        // Two independent draws should differ (probability of collision ~2^-256).
        assert_ne!(a, b);
        assert_ne!(a, [0u8; 32]);
    }

    #[test]
    fn hmac_sha256_matches_known_vector() {
        // RFC 4231 test case 2: key = "Jefe", data = "what do ya want for nothing?".
        let mac = hmac_sha256(b"Jefe", b"what do ya want for nothing?").unwrap();
        let hex: String = mac.iter().map(|b| format!("{b:02x}")).collect();
        assert_eq!(
            hex,
            "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843"
        );
    }

    #[test]
    fn aes_256_cbc_round_trips() {
        let key = [0x42u8; 32];
        let iv = [0x24u8; 16];
        let plaintext = b"Always Encrypted AES round trip";
        let ct = aes_256_cbc_encrypt(&key, &iv, plaintext).unwrap();
        // PKCS#7 padding always extends to a whole number of blocks.
        assert_eq!(ct.len() % 16, 0);
        assert_ne!(ct, plaintext);
        let pt = aes_256_cbc_decrypt(&key, &iv, &ct).unwrap();
        assert_eq!(pt, plaintext);
    }

    #[test]
    fn aes_256_cbc_known_vector() {
        // NIST SP 800-38A, F.2.5 CBC-AES256.Encrypt, first block.
        let key = [
            0x60, 0x3d, 0xeb, 0x10, 0x15, 0xca, 0x71, 0xbe, 0x2b, 0x73, 0xae, 0xf0, 0x85, 0x7d,
            0x77, 0x81, 0x1f, 0x35, 0x2c, 0x07, 0x3b, 0x61, 0x08, 0xd7, 0x2d, 0x98, 0x10, 0xa3,
            0x09, 0x14, 0xdf, 0xf4,
        ];
        let iv = [
            0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0a, 0x0b, 0x0c, 0x0d,
            0x0e, 0x0f,
        ];
        let plaintext = [
            0x6b, 0xc1, 0xbe, 0xe2, 0x2e, 0x40, 0x9f, 0x96, 0xe9, 0x3d, 0x7e, 0x11, 0x73, 0x93,
            0x17, 0x2a,
        ];
        let ct = aes_256_cbc_encrypt(&key, &iv, &plaintext).unwrap();
        // First 16 bytes are the documented ciphertext block; the rest is the
        // PKCS#7 padding block, which the NIST vector (no padding) omits.
        let hex: String = ct[..16].iter().map(|b| format!("{b:02x}")).collect();
        assert_eq!(hex, "f58c4c04d6e5f1ba779eabfb5f7bfbd6");
    }

    #[test]
    fn constant_time_eq_behaves_like_eq() {
        assert!(constant_time_eq(b"abcdef", b"abcdef"));
        assert!(!constant_time_eq(b"abcdef", b"abcdeg"));
        assert!(!constant_time_eq(b"abc", b"abcd"));
        assert!(constant_time_eq(b"", b""));
    }

    #[test]
    fn rsa_oaep_round_trip() {
        let key = RsaKey::generate(2048).unwrap();
        let secret = b"32-byte column encryption key!!!";
        let ct = key.oaep_sha1_encrypt(secret).unwrap();
        let pt = key.oaep_sha1_decrypt(&ct).unwrap();
        assert_eq!(pt, secret);
    }

    #[test]
    fn rsa_sign_verify_round_trip() {
        let key = RsaKey::generate(2048).unwrap();
        let msg = b"signed portion of an encrypted CEK blob";
        let sig = key.pkcs1_sha256_sign(msg).unwrap();
        assert!(key.pkcs1_sha256_verify(msg, &sig).unwrap());
        // A tampered message must fail verification.
        assert!(!key.pkcs1_sha256_verify(b"different message", &sig).unwrap());
    }

    #[test]
    fn rsa_pem_round_trip() {
        let key = RsaKey::generate(2048).unwrap();
        let pem = key.to_pkcs8_pem().unwrap();
        let reloaded = RsaKey::from_pem(&pem).unwrap();
        // A signature from the original verifies under the reloaded key (same key).
        let msg = b"pem round trip";
        let sig = key.pkcs1_sha256_sign(msg).unwrap();
        assert!(reloaded.pkcs1_sha256_verify(msg, &sig).unwrap());
    }
}
