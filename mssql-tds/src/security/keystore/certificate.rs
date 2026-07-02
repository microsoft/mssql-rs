// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! In-memory RSA column master key store provider for Always Encrypted.
//!
//! This provider unwraps column encryption keys (CEKs) using RSA key pairs (the
//! column master keys) supplied by the client and held in memory. Despite
//! registering under the wire identifier `MSSQL_CERTIFICATE_STORE`, it does not
//! read any operating-system certificate store: keys are provided directly via
//! [`RsaKeyStoreProvider::add_key_from_pem`] / `add_key` and matched by master
//! key path. It implements the same encrypted CEK wire format and algorithms as
//! the other Microsoft drivers' certificate / key-store providers (for example
//! JDBC's `MSSQL_JAVA_KEYSTORE` provider and the `MSSQL_CERTIFICATE_STORE`
//! provider), so it interoperates with column master keys created with
//! `KEY_STORE_PROVIDER_NAME = 'MSSQL_CERTIFICATE_STORE'` (or any provider that
//! uses the standard RSA_OAEP wrapping format).
//!
//! Encrypted CEK layout (little-endian lengths):
//!
//! ```text
//! version           : 1 byte  (0x01)
//! keyPathLength     : 2 bytes (UTF-16LE byte length of the master key path)
//! cipherTextLength  : 2 bytes (length of the RSA-OAEP ciphertext)
//! masterKeyPath     : keyPathLength bytes (UTF-16LE, lowercased)
//! cipherText        : cipherTextLength bytes (RSA-OAEP(SHA-1) wrapped CEK)
//! signature         : remaining bytes (RSASSA-PKCS1v1_5 / SHA-256 over the
//!                     preceding bytes, signed with the master key)
//! ```

use std::collections::HashMap;

use async_trait::async_trait;

use super::ColumnEncryptionKeyStoreProvider;
use crate::core::TdsResult;
use crate::error::Error;
use crate::security::crypto::RsaKey;

/// The only encrypted CEK version byte the format defines.
const SUPPORTED_VERSION: u8 = 0x01;

/// The only key wrapping algorithm Always Encrypted supports.
const RSA_OAEP_ALGORITHM: &str = "RSA_OAEP";

/// A column master key store provider backed by client-supplied, in-memory RSA
/// key pairs.
///
/// This does not read any OS certificate store. Keys are registered directly by
/// master key path (matched case-insensitively, as the path is lowercased
/// before signing); the path corresponds to the `key_path` carried in the
/// COLMETADATA CEK table. The provider registers under the wire identifier
/// `MSSQL_CERTIFICATE_STORE`.
pub struct RsaKeyStoreProvider {
    /// Master key path (lowercased) → RSA key pair that wraps CEKs for that CMK.
    keys: HashMap<String, RsaKey>,
}

impl RsaKeyStoreProvider {
    /// Creates a provider with no keys registered.
    pub fn new() -> Self {
        Self {
            keys: HashMap::new(),
        }
    }

    /// Registers the PEM-encoded RSA private key that backs the column master
    /// key identified by `master_key_path`. The key pair is used to verify the
    /// encrypted CEK signature and to RSA-OAEP unwrap the CEK.
    ///
    /// Both PKCS#1 (`-----BEGIN RSA PRIVATE KEY-----`) and PKCS#8
    /// (`-----BEGIN PRIVATE KEY-----`) PEM encodings are accepted.
    pub fn add_key_from_pem(
        &mut self,
        master_key_path: impl AsRef<str>,
        private_key_pem: &[u8],
    ) -> TdsResult<()> {
        // `RsaKey::from_pem` accepts either a PKCS#1 RSA key or a PKCS#8 key.
        let key = RsaKey::from_pem(private_key_pem)?;
        self.keys
            .insert(master_key_path.as_ref().to_ascii_lowercase(), key);
        Ok(())
    }

    /// Registers an already-parsed RSA key pair for `master_key_path`.
    ///
    /// Internal: callers outside the crate register keys via
    /// [`Self::add_key_from_pem`] or [`Self::generate_and_add_key`], since
    /// [`RsaKey`] is a crate-private wrapper over the platform crypto backend.
    pub(crate) fn add_key(&mut self, master_key_path: impl AsRef<str>, key: RsaKey) {
        self.keys
            .insert(master_key_path.as_ref().to_ascii_lowercase(), key);
    }

    /// Generates a fresh RSA-2048 key pair and registers it as the column master
    /// key for `master_key_path`.
    ///
    /// The key material lives only in memory for the lifetime of this provider;
    /// it is never written to a certificate store or to disk. This is primarily
    /// useful for provisioning a throwaway column master key in tests and
    /// tooling without persisting (or committing) static key material.
    pub fn generate_and_add_key(&mut self, master_key_path: impl AsRef<str>) -> TdsResult<()> {
        let key = RsaKey::generate(2048)?;
        self.add_key(master_key_path, key);
        Ok(())
    }

    /// Wraps a plaintext column encryption key (CEK) with the master key at
    /// `master_key_path`, producing the encrypted CEK blob in the format SQL
    /// Server stores (the value supplied to `CREATE COLUMN ENCRYPTION KEY ...
    /// WITH VALUES (... ENCRYPTED_VALUE = 0x...)`).
    ///
    /// This is the inverse of [`Self::decrypt_column_encryption_key`] and is
    /// primarily useful for provisioning column encryption keys and for tests.
    pub fn encrypt_column_encryption_key(
        &self,
        master_key_path: &str,
        encryption_algorithm: &str,
        plaintext_cek: &[u8],
    ) -> TdsResult<Vec<u8>> {
        if !encryption_algorithm.eq_ignore_ascii_case(RSA_OAEP_ALGORITHM) {
            return Err(Error::ColumnEncryptionError(format!(
                "Unsupported key encryption algorithm '{encryption_algorithm}'; \
                 Always Encrypted only supports '{RSA_OAEP_ALGORITHM}'"
            )));
        }
        let pkey = self.key_for(master_key_path)?;
        encrypt_rsa_oaep_cek(pkey, master_key_path, plaintext_cek)
    }

    fn key_for(&self, master_key_path: &str) -> TdsResult<&RsaKey> {
        self.keys
            .get(&master_key_path.to_ascii_lowercase())
            .ok_or_else(|| {
                Error::ColumnEncryptionError(format!(
                    "Certificate key store provider has no key registered for master key path '{master_key_path}'"
                ))
            })
    }
}

impl Default for RsaKeyStoreProvider {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl ColumnEncryptionKeyStoreProvider for RsaKeyStoreProvider {
    async fn decrypt_column_encryption_key(
        &self,
        master_key_path: &str,
        encryption_algorithm: &str,
        encrypted_cek: &[u8],
    ) -> TdsResult<Vec<u8>> {
        if !encryption_algorithm.eq_ignore_ascii_case(RSA_OAEP_ALGORITHM) {
            return Err(Error::ColumnEncryptionError(format!(
                "Unsupported key encryption algorithm '{encryption_algorithm}'; \
                 Always Encrypted only supports '{RSA_OAEP_ALGORITHM}'"
            )));
        }

        let pkey = self.key_for(master_key_path)?;
        decrypt_rsa_oaep_cek(pkey, master_key_path, encrypted_cek)
    }
}

/// Parses the encrypted CEK blob, verifies its signature, and RSA-OAEP unwraps
/// the wrapped column encryption key.
fn decrypt_rsa_oaep_cek(pkey: &RsaKey, master_key_path: &str, blob: &[u8]) -> TdsResult<Vec<u8>> {
    // version (1) + keyPathLength (2) + cipherTextLength (2)
    const HEADER_LEN: usize = 5;
    if blob.len() < HEADER_LEN {
        return Err(Error::ColumnEncryptionError(
            "Encrypted column encryption key is too short to contain a header".to_string(),
        ));
    }

    if blob[0] != SUPPORTED_VERSION {
        return Err(Error::ColumnEncryptionError(format!(
            "Unsupported encrypted column encryption key version byte {:#04x}",
            blob[0]
        )));
    }

    let key_path_len = u16::from_le_bytes([blob[1], blob[2]]) as usize;
    let cipher_text_len = u16::from_le_bytes([blob[3], blob[4]]) as usize;

    let cipher_text_start = HEADER_LEN
        .checked_add(key_path_len)
        .ok_or_else(|| length_error(master_key_path))?;
    let signature_start = cipher_text_start
        .checked_add(cipher_text_len)
        .ok_or_else(|| length_error(master_key_path))?;

    if blob.len() <= signature_start {
        return Err(Error::ColumnEncryptionError(format!(
            "Encrypted column encryption key wrapped by master key '{master_key_path}' is \
             truncated or missing its signature"
        )));
    }

    let cipher_text = &blob[cipher_text_start..signature_start];
    let signature = &blob[signature_start..];
    // The signature covers everything up to (but not including) the signature.
    let signed_portion = &blob[..signature_start];

    verify_signature(pkey, signed_portion, signature, master_key_path)?;
    rsa_oaep_decrypt(pkey, cipher_text)
}

/// Wraps a plaintext CEK into the SQL Server encrypted-CEK format: RSA-OAEP
/// (SHA-1) ciphertext framed by a version byte, the lowercased master key path,
/// and a SHA256withRSA signature. Inverse of [`decrypt_rsa_oaep_cek`].
fn encrypt_rsa_oaep_cek(
    pkey: &RsaKey,
    master_key_path: &str,
    plaintext_cek: &[u8],
) -> TdsResult<Vec<u8>> {
    let cipher_text = pkey.oaep_sha1_encrypt(plaintext_cek)?;

    let key_path_bytes: Vec<u8> = master_key_path
        .to_ascii_lowercase()
        .encode_utf16()
        .flat_map(|u| u.to_le_bytes())
        .collect();

    let mut signed_portion = Vec::with_capacity(5 + key_path_bytes.len() + cipher_text.len());
    signed_portion.push(SUPPORTED_VERSION);
    signed_portion.extend_from_slice(&(key_path_bytes.len() as u16).to_le_bytes());
    signed_portion.extend_from_slice(&(cipher_text.len() as u16).to_le_bytes());
    signed_portion.extend_from_slice(&key_path_bytes);
    signed_portion.extend_from_slice(&cipher_text);

    let signature = pkey.pkcs1_sha256_sign(&signed_portion)?;

    let mut blob = signed_portion;
    blob.extend_from_slice(&signature);
    Ok(blob)
}

/// Verifies the RSASSA-PKCS1v1_5 / SHA-256 signature over `signed_portion`.
fn verify_signature(
    pkey: &RsaKey,
    signed_portion: &[u8],
    signature: &[u8],
    master_key_path: &str,
) -> TdsResult<()> {
    if !pkey.pkcs1_sha256_verify(signed_portion, signature)? {
        return Err(Error::ColumnEncryptionError(format!(
            "Signature verification failed for the column encryption key wrapped by master key \
             '{master_key_path}'. The encrypted value may have been tampered with, or the wrong \
             master key is registered."
        )));
    }
    Ok(())
}

/// RSA-OAEP (SHA-1 / MGF1-SHA-1) decryption of the wrapped CEK.
fn rsa_oaep_decrypt(pkey: &RsaKey, cipher_text: &[u8]) -> TdsResult<Vec<u8>> {
    // Always Encrypted wraps CEKs with RSA-OAEP using SHA-1 for both the OAEP
    // digest and the MGF1 mask generation function.
    pkey.oaep_sha1_decrypt(cipher_text)
}

fn length_error(master_key_path: &str) -> Error {
    Error::ColumnEncryptionError(format!(
        "Encrypted column encryption key wrapped by master key '{master_key_path}' has \
         inconsistent length fields"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    const MASTER_KEY_PATH: &str = "CurrentUser/My/0123456789ABCDEF";
    const CEK: [u8; 32] = [
        0x10, 0x11, 0x12, 0x13, 0x14, 0x15, 0x16, 0x17, 0x18, 0x19, 0x1A, 0x1B, 0x1C, 0x1D, 0x1E,
        0x1F, 0x20, 0x21, 0x22, 0x23, 0x24, 0x25, 0x26, 0x27, 0x28, 0x29, 0x2A, 0x2B, 0x2C, 0x2D,
        0x2E, 0x2F,
    ];

    /// Generates a fresh RSA key serialized as PKCS#8 PEM. Returning PEM lets a
    /// test build several independent provider instances from the same key
    /// material without requiring the (deliberately non-`Clone`) [`RsaKey`].
    fn generate_key_pem() -> Vec<u8> {
        RsaKey::generate(2048).unwrap().to_pkcs8_pem().unwrap()
    }

    /// Wraps a CEK using a provider built from `pem` (the inverse of unwrap),
    /// producing a blob the provider must be able to unwrap again.
    fn wrap_cek(pem: &[u8], master_key_path: &str, cek: &[u8]) -> Vec<u8> {
        let mut provider = RsaKeyStoreProvider::new();
        provider.add_key_from_pem(master_key_path, pem).unwrap();
        provider
            .encrypt_column_encryption_key(master_key_path, "RSA_OAEP", cek)
            .unwrap()
    }

    #[tokio::test]
    async fn unwraps_a_faithfully_wrapped_cek() {
        let pem = generate_key_pem();
        let blob = wrap_cek(&pem, MASTER_KEY_PATH, &CEK);

        let mut provider = RsaKeyStoreProvider::new();
        provider.add_key_from_pem(MASTER_KEY_PATH, &pem).unwrap();

        let plaintext = provider
            .decrypt_column_encryption_key(MASTER_KEY_PATH, "RSA_OAEP", &blob)
            .await
            .unwrap();
        assert_eq!(plaintext, CEK);
    }

    #[tokio::test]
    async fn loads_pkcs8_pem_key() {
        // `generate_key_pem` emits PKCS#8 (`BEGIN PRIVATE KEY`); ensure it loads.
        let pem = generate_key_pem();
        let blob = wrap_cek(&pem, MASTER_KEY_PATH, &CEK);

        let mut provider = RsaKeyStoreProvider::new();
        provider.add_key_from_pem(MASTER_KEY_PATH, &pem).unwrap();

        let plaintext = provider
            .decrypt_column_encryption_key(MASTER_KEY_PATH, "RSA_OAEP", &blob)
            .await
            .unwrap();
        assert_eq!(plaintext, CEK);
    }

    #[tokio::test]
    async fn master_key_path_is_matched_case_insensitively() {
        let pem = generate_key_pem();
        let blob = wrap_cek(&pem, MASTER_KEY_PATH, &CEK);

        // Exercise the parsed-key registration path (`add_key`).
        let mut provider = RsaKeyStoreProvider::new();
        provider.add_key(MASTER_KEY_PATH, RsaKey::from_pem(&pem).unwrap());

        let plaintext = provider
            .decrypt_column_encryption_key(&MASTER_KEY_PATH.to_uppercase(), "rsa_oaep", &blob)
            .await
            .unwrap();
        assert_eq!(plaintext, CEK);
    }

    #[tokio::test]
    async fn rejects_unknown_algorithm() {
        let pem = generate_key_pem();
        let blob = wrap_cek(&pem, MASTER_KEY_PATH, &CEK);
        let mut provider = RsaKeyStoreProvider::new();
        provider.add_key_from_pem(MASTER_KEY_PATH, &pem).unwrap();

        let err = provider
            .decrypt_column_encryption_key(MASTER_KEY_PATH, "RSA_OAEP_256", &blob)
            .await
            .unwrap_err();
        assert!(matches!(err, Error::ColumnEncryptionError(_)));
    }

    #[tokio::test]
    async fn rejects_unknown_master_key_path() {
        let pem = generate_key_pem();
        let blob = wrap_cek(&pem, MASTER_KEY_PATH, &CEK);
        let mut provider = RsaKeyStoreProvider::new();
        provider.add_key_from_pem(MASTER_KEY_PATH, &pem).unwrap();

        let err = provider
            .decrypt_column_encryption_key("CurrentUser/My/other", "RSA_OAEP", &blob)
            .await
            .unwrap_err();
        assert!(matches!(err, Error::ColumnEncryptionError(_)));
    }

    #[tokio::test]
    async fn rejects_tampered_signature() {
        let pem = generate_key_pem();
        let mut blob = wrap_cek(&pem, MASTER_KEY_PATH, &CEK);
        // Flip a bit in the trailing signature.
        let last = blob.len() - 1;
        blob[last] ^= 0x01;

        let mut provider = RsaKeyStoreProvider::new();
        provider.add_key_from_pem(MASTER_KEY_PATH, &pem).unwrap();

        let err = provider
            .decrypt_column_encryption_key(MASTER_KEY_PATH, "RSA_OAEP", &blob)
            .await
            .unwrap_err();
        assert!(matches!(err, Error::ColumnEncryptionError(_)));
    }

    #[tokio::test]
    async fn rejects_unsupported_version() {
        let pem = generate_key_pem();
        let mut blob = wrap_cek(&pem, MASTER_KEY_PATH, &CEK);
        blob[0] = 0x02; // bump the version byte

        let mut provider = RsaKeyStoreProvider::new();
        provider.add_key_from_pem(MASTER_KEY_PATH, &pem).unwrap();

        let err = provider
            .decrypt_column_encryption_key(MASTER_KEY_PATH, "RSA_OAEP", &blob)
            .await
            .unwrap_err();
        assert!(matches!(err, Error::ColumnEncryptionError(_)));
    }

    #[tokio::test]
    async fn unwraps_via_registry_and_decrypt_cek() {
        use super::super::{CekCache, ColumnEncryptionKeyStoreProviderRegistry, decrypt_cek};
        use crate::query::metadata::{CekTableEntry, EncryptedCekValue};
        use std::sync::Arc;

        let pem = generate_key_pem();
        let blob = wrap_cek(&pem, MASTER_KEY_PATH, &CEK);

        let mut provider = RsaKeyStoreProvider::new();
        provider.add_key_from_pem(MASTER_KEY_PATH, &pem).unwrap();

        // Register under the standard certificate-store provider name and
        // resolve a CEK table entry end-to-end through `decrypt_cek`.
        let mut registry = ColumnEncryptionKeyStoreProviderRegistry::new();
        registry.register("MSSQL_CERTIFICATE_STORE", Arc::new(provider));
        let cache = CekCache::new();

        let entry = CekTableEntry {
            database_id: 1,
            cek_id: 1,
            cek_version: 1,
            cek_md_version: [0; 8],
            encrypted_cek_values: vec![EncryptedCekValue {
                encrypted_key: blob,
                key_store_name: "MSSQL_CERTIFICATE_STORE".to_string(),
                key_path: MASTER_KEY_PATH.to_string(),
                algorithm_name: "RSA_OAEP".to_string(),
            }],
        };

        let plaintext = decrypt_cek(&registry, &cache, &entry).await.unwrap();
        assert_eq!(plaintext.as_ref().as_slice(), &CEK);
    }
}
