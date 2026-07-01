// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! OpenSSL-backed crypto primitives for Always Encrypted.
//!
//! Used on every non-Windows target. OpenSSL is the same library the TLS layer
//! relies on on these platforms, and is an approved crypto provider under the
//! compliance policy. See [`super`] for the platform-selection rationale.

// Some primitives are only used by tests or by one consumer; keep the full
// surface so the cell cipher and CEK wrapping paths share one backend.
#![allow(dead_code)]

use openssl::encrypt::{Decrypter, Encrypter};
use openssl::hash::MessageDigest;
use openssl::pkey::{PKey, Private};
use openssl::rsa::{Padding, Rsa};
use openssl::sign::{Signer, Verifier};
use openssl::symm::Cipher;

use crate::core::TdsResult;
use crate::error::Error;

/// Maps an OpenSSL error into a column-encryption error with context.
fn crypto_err(context: &str, err: openssl::error::ErrorStack) -> Error {
    Error::ColumnEncryptionError(format!("{context}: {err}"))
}

/// Fills `buf` with cryptographically secure random bytes.
pub(crate) fn fill_random(buf: &mut [u8]) -> TdsResult<()> {
    openssl::rand::rand_bytes(buf).map_err(|e| crypto_err("random byte generation failed", e))
}

/// Computes `HMAC-SHA256(key, data)`.
pub(crate) fn hmac_sha256(key: &[u8], data: &[u8]) -> TdsResult<[u8; 32]> {
    let pkey = PKey::hmac(key).map_err(|e| crypto_err("HMAC key initialization failed", e))?;
    let mut signer = Signer::new(MessageDigest::sha256(), &pkey)
        .map_err(|e| crypto_err("HMAC initialization failed", e))?;
    signer
        .update(data)
        .map_err(|e| crypto_err("HMAC update failed", e))?;
    let tag = signer
        .sign_to_vec()
        .map_err(|e| crypto_err("HMAC finalization failed", e))?;
    tag.try_into().map_err(|v: Vec<u8>| {
        Error::ColumnEncryptionError(format!("HMAC produced {} bytes, expected 32", v.len()))
    })
}

/// Encrypts `plaintext` with AES-256-CBC and PKCS#7 padding.
pub(crate) fn aes_256_cbc_encrypt(
    key: &[u8; 32],
    iv: &[u8; 16],
    plaintext: &[u8],
) -> TdsResult<Vec<u8>> {
    openssl::symm::encrypt(Cipher::aes_256_cbc(), key, Some(iv), plaintext)
        .map_err(|e| crypto_err("AES-256-CBC encryption failed", e))
}

/// Decrypts AES-256-CBC ciphertext and strips PKCS#7 padding.
pub(crate) fn aes_256_cbc_decrypt(
    key: &[u8; 32],
    iv: &[u8; 16],
    ciphertext: &[u8],
) -> TdsResult<Vec<u8>> {
    openssl::symm::decrypt(Cipher::aes_256_cbc(), key, Some(iv), ciphertext)
        .map_err(|e| crypto_err("AES-256-CBC decryption failed", e))
}

/// An RSA private key (with its public part) backed by an OpenSSL `PKey`.
pub(crate) struct RsaKey {
    pkey: PKey<Private>,
}

impl RsaKey {
    /// Parses an RSA private key from PEM, accepting both PKCS#8
    /// (`-----BEGIN PRIVATE KEY-----`) and PKCS#1
    /// (`-----BEGIN RSA PRIVATE KEY-----`) encodings.
    pub(crate) fn from_pem(pem: &[u8]) -> TdsResult<Self> {
        // `private_key_from_pem` reads PKCS#8 ("BEGIN PRIVATE KEY"); fall back to
        // the traditional PKCS#1 RSA encoding ("BEGIN RSA PRIVATE KEY").
        if let Ok(pkey) = PKey::private_key_from_pem(pem) {
            return Ok(Self { pkey });
        }
        let rsa = Rsa::private_key_from_pem(pem).map_err(|e| {
            crypto_err(
                "failed to parse RSA private key from PEM (tried PKCS#8 and PKCS#1)",
                e,
            )
        })?;
        let pkey = PKey::from_rsa(rsa).map_err(|e| crypto_err("failed to wrap RSA key", e))?;
        Ok(Self { pkey })
    }

    /// Generates a fresh RSA key pair of the given modulus size in bits.
    pub(crate) fn generate(bits: u32) -> TdsResult<Self> {
        let rsa = Rsa::generate(bits).map_err(|e| crypto_err("RSA key generation failed", e))?;
        let pkey = PKey::from_rsa(rsa).map_err(|e| crypto_err("failed to wrap RSA key", e))?;
        Ok(Self { pkey })
    }

    /// Serializes the private key to a PKCS#8 PEM document.
    pub(crate) fn to_pkcs8_pem(&self) -> TdsResult<Vec<u8>> {
        self.pkey
            .private_key_to_pem_pkcs8()
            .map_err(|e| crypto_err("failed to encode RSA key to PKCS#8 PEM", e))
    }

    /// RSA-OAEP encryption using SHA-1 for both the OAEP hash and the MGF1 mask
    /// (the wrapping scheme Always Encrypted uses for column encryption keys).
    pub(crate) fn oaep_sha1_encrypt(&self, plaintext: &[u8]) -> TdsResult<Vec<u8>> {
        let mut enc =
            Encrypter::new(&self.pkey).map_err(|e| crypto_err("RSA-OAEP encrypter init", e))?;
        enc.set_rsa_padding(Padding::PKCS1_OAEP)
            .map_err(|e| crypto_err("set OAEP padding", e))?;
        enc.set_rsa_oaep_md(MessageDigest::sha1())
            .map_err(|e| crypto_err("set OAEP digest", e))?;
        enc.set_rsa_mgf1_md(MessageDigest::sha1())
            .map_err(|e| crypto_err("set MGF1 digest", e))?;
        let len = enc
            .encrypt_len(plaintext)
            .map_err(|e| crypto_err("RSA-OAEP length probe", e))?;
        let mut out = vec![0u8; len];
        let n = enc
            .encrypt(plaintext, &mut out)
            .map_err(|e| crypto_err("RSA-OAEP encryption failed", e))?;
        out.truncate(n);
        Ok(out)
    }

    /// RSA-OAEP decryption (SHA-1 OAEP hash and MGF1 mask).
    pub(crate) fn oaep_sha1_decrypt(&self, ciphertext: &[u8]) -> TdsResult<Vec<u8>> {
        let mut dec =
            Decrypter::new(&self.pkey).map_err(|e| crypto_err("RSA-OAEP decrypter init", e))?;
        dec.set_rsa_padding(Padding::PKCS1_OAEP)
            .map_err(|e| crypto_err("set OAEP padding", e))?;
        dec.set_rsa_oaep_md(MessageDigest::sha1())
            .map_err(|e| crypto_err("set OAEP digest", e))?;
        dec.set_rsa_mgf1_md(MessageDigest::sha1())
            .map_err(|e| crypto_err("set MGF1 digest", e))?;
        let len = dec
            .decrypt_len(ciphertext)
            .map_err(|e| crypto_err("RSA-OAEP length probe", e))?;
        let mut out = vec![0u8; len];
        let n = dec
            .decrypt(ciphertext, &mut out)
            .map_err(|e| crypto_err("RSA-OAEP decryption failed", e))?;
        out.truncate(n);
        Ok(out)
    }

    /// Signs `data` with RSASSA-PKCS1v1.5 over SHA-256.
    pub(crate) fn pkcs1_sha256_sign(&self, data: &[u8]) -> TdsResult<Vec<u8>> {
        let mut signer = Signer::new(MessageDigest::sha256(), &self.pkey)
            .map_err(|e| crypto_err("RSA signer init", e))?;
        signer
            .set_rsa_padding(Padding::PKCS1)
            .map_err(|e| crypto_err("set PKCS#1 padding", e))?;
        signer
            .update(data)
            .map_err(|e| crypto_err("RSA sign update", e))?;
        signer
            .sign_to_vec()
            .map_err(|e| crypto_err("RSA signing failed", e))
    }

    /// Verifies an RSASSA-PKCS1v1.5 / SHA-256 signature over `data`.
    pub(crate) fn pkcs1_sha256_verify(&self, data: &[u8], signature: &[u8]) -> TdsResult<bool> {
        let mut verifier = Verifier::new(MessageDigest::sha256(), &self.pkey)
            .map_err(|e| crypto_err("RSA verifier init", e))?;
        verifier
            .set_rsa_padding(Padding::PKCS1)
            .map_err(|e| crypto_err("set PKCS#1 padding", e))?;
        verifier
            .update(data)
            .map_err(|e| crypto_err("RSA verify update", e))?;
        verifier
            .verify(signature)
            .map_err(|e| crypto_err("RSA verification failed", e))
    }
}
