// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Column master key (CMK) store provider infrastructure for Always Encrypted.
//!
//! A column encryption key (CEK) is never sent to SQL Server in plaintext.
//! Instead, the server stores one or more *encrypted* CEK values in the
//! COLMETADATA CEK table, each wrapped by a column master key (CMK) that lives
//! in an external key store (a certificate store, Azure Key Vault, a file, ...).
//!
//! To use an encrypted column the driver must recover the plaintext CEK by
//! asking the appropriate [`ColumnEncryptionKeyStoreProvider`] to unwrap one of
//! the encrypted values. This module provides:
//!
//! * [`ColumnEncryptionKeyStoreProvider`] — the provider trait implemented by
//!   each key store.
//! * [`ColumnEncryptionKeyStoreProviderRegistry`] — a name-keyed registry of
//!   providers (provider names are matched case-insensitively, matching the
//!   behavior of the other Always Encrypted drivers).
//! * [`CekCache`] — a thread-safe cache of decrypted CEKs so a given CMK is only
//!   asked to unwrap a CEK once.
//! * [`decrypt_cek`] — resolves the plaintext CEK for a [`CekTableEntry`] using
//!   the registry and cache.
//!
//! These pieces are consumed by the result-set decryption and parameter
//! encryption phases.

// The registry/cache are consumed by the result-set and parameter encryption
// phases that build on this module.
#![allow(dead_code)]

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;

use crate::core::TdsResult;
use crate::datatypes::column_values::ColumnValues;
use crate::error::Error;
use crate::query::metadata::{CekTableEntry, CryptoMetadata};
use crate::security::cell_decryptor::CellDecryptor;
use crate::security::encryption::decrypt_cell;

/// Unwraps encrypted column encryption keys (CEKs) using a column master key
/// (CMK) held in an external key store.
///
/// Implementations are registered by name in a
/// [`ColumnEncryptionKeyStoreProviderRegistry`]; the name corresponds to the
/// `key_store_name` carried in the COLMETADATA CEK table.
#[async_trait]
pub(crate) trait ColumnEncryptionKeyStoreProvider: Send + Sync {
    /// Decrypts an encrypted column encryption key (CEK) using the column master
    /// key (CMK) identified by `master_key_path`.
    ///
    /// * `master_key_path` — provider-specific path identifying the CMK.
    /// * `encryption_algorithm` — the asymmetric algorithm used to wrap the CEK
    ///   (for example `RSA_OAEP`).
    /// * `encrypted_cek` — the wrapped CEK bytes.
    ///
    /// Returns the plaintext CEK (32 bytes for AEAD_AES_256_CBC_HMAC_SHA256).
    ///
    /// # Errors
    ///
    /// Returns [`Error::ColumnEncryptionError`] if the CEK cannot be unwrapped.
    async fn decrypt_column_encryption_key(
        &self,
        master_key_path: &str,
        encryption_algorithm: &str,
        encrypted_cek: &[u8],
    ) -> TdsResult<Vec<u8>>;
}

/// A name-keyed registry of [`ColumnEncryptionKeyStoreProvider`]s.
///
/// Provider names are matched case-insensitively to match SQL Server's
/// built-in provider names (`MSSQL_CERTIFICATE_STORE`, `AZURE_KEY_VAULT`, ...).
#[derive(Clone, Default)]
pub(crate) struct ColumnEncryptionKeyStoreProviderRegistry {
    providers: HashMap<String, Arc<dyn ColumnEncryptionKeyStoreProvider>>,
}

impl ColumnEncryptionKeyStoreProviderRegistry {
    /// Creates an empty registry.
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Registers `provider` under `name`, replacing any existing provider with
    /// the same (case-insensitive) name.
    pub(crate) fn register(
        &mut self,
        name: impl AsRef<str>,
        provider: Arc<dyn ColumnEncryptionKeyStoreProvider>,
    ) {
        self.providers
            .insert(name.as_ref().to_ascii_uppercase(), provider);
    }

    /// Looks up a provider by name (case-insensitive).
    pub(crate) fn get(&self, name: &str) -> Option<Arc<dyn ColumnEncryptionKeyStoreProvider>> {
        self.providers.get(&name.to_ascii_uppercase()).cloned()
    }

    /// Returns `true` if no providers are registered.
    pub(crate) fn is_empty(&self) -> bool {
        self.providers.is_empty()
    }
}

/// Identity of a wrapped CEK, used as the [`CekCache`] key.
///
/// A plaintext CEK is fully determined by the wrapping provider, the master key
/// path, and the encrypted CEK bytes, so caching on this triple is safe across
/// CEK rotations and reused across result sets.
#[derive(Clone, PartialEq, Eq, Hash)]
struct CekCacheKey {
    provider_name: String,
    master_key_path: String,
    encrypted_cek: Vec<u8>,
}

/// Thread-safe cache of decrypted column encryption keys.
///
/// Unwrapping a CEK can be expensive (a certificate-store or network round trip),
/// so plaintext CEKs are cached and shared across the columns and rows that use
/// them.
#[derive(Default)]
pub(crate) struct CekCache {
    entries: Mutex<HashMap<CekCacheKey, Arc<Vec<u8>>>>,
}

impl CekCache {
    /// Creates an empty cache.
    pub(crate) fn new() -> Self {
        Self::default()
    }

    fn get(&self, key: &CekCacheKey) -> Option<Arc<Vec<u8>>> {
        self.entries
            .lock()
            .expect("CEK cache mutex poisoned")
            .get(key)
            .cloned()
    }

    fn insert(&self, key: CekCacheKey, value: Arc<Vec<u8>>) {
        self.entries
            .lock()
            .expect("CEK cache mutex poisoned")
            .insert(key, value);
    }
}

/// Resolves the plaintext column encryption key for a CEK table entry.
///
/// Each [`CekTableEntry`] carries one or more encrypted CEK values (one per CMK
/// that wraps it, to support key rotation). This tries each value in turn: a
/// cache hit short-circuits; otherwise the provider named by the value is asked
/// to unwrap it and the result is cached. The first value that resolves wins.
///
/// # Errors
///
/// Returns [`Error::ColumnEncryptionError`] if the entry has no encrypted
/// values, if no value names a registered provider, or if every provider fails
/// to unwrap its value (the most recent failure is returned).
pub(crate) async fn decrypt_cek(
    registry: &ColumnEncryptionKeyStoreProviderRegistry,
    cache: &CekCache,
    entry: &CekTableEntry,
) -> TdsResult<Arc<Vec<u8>>> {
    if entry.encrypted_cek_values.is_empty() {
        return Err(Error::ColumnEncryptionError(
            "CEK table entry has no encrypted key values".to_string(),
        ));
    }

    let mut last_error: Option<Error> = None;

    for value in &entry.encrypted_cek_values {
        let cache_key = CekCacheKey {
            provider_name: value.key_store_name.to_ascii_uppercase(),
            master_key_path: value.key_path.clone(),
            encrypted_cek: value.encrypted_key.clone(),
        };

        if let Some(cached) = cache.get(&cache_key) {
            return Ok(cached);
        }

        let Some(provider) = registry.get(&value.key_store_name) else {
            last_error = Some(Error::ColumnEncryptionError(format!(
                "No column master key store provider is registered with name '{}'",
                value.key_store_name
            )));
            continue;
        };

        match provider
            .decrypt_column_encryption_key(
                &value.key_path,
                &value.algorithm_name,
                &value.encrypted_key,
            )
            .await
        {
            Ok(plaintext) => {
                let plaintext = Arc::new(plaintext);
                cache.insert(cache_key, plaintext.clone());
                return Ok(plaintext);
            }
            Err(error) => last_error = Some(error),
        }
    }

    Err(last_error.unwrap_or_else(|| {
        Error::ColumnEncryptionError(
            "Unable to decrypt column encryption key with any registered provider".to_string(),
        )
    }))
}

/// A [`CellDecryptor`] backed by the plaintext column encryption keys resolved
/// for one COLMETADATA CEK table.
///
/// The CEKs are resolved up front (see [`ResolvedCekDecryptor::resolve`]) so the
/// per-cell [`CellDecryptor::decrypt`] path is synchronous: it indexes the CEK
/// by the column's `cek_table_ordinal` and runs the AEAD decryption. Resolution
/// failures are captured per ordinal and only surfaced if a column that uses
/// that ordinal is actually read, so an unreadable key for an unused column does
/// not fail the whole result set.
#[derive(Debug)]
pub(crate) struct ResolvedCekDecryptor {
    /// Resolved plaintext CEKs indexed by CEK table ordinal. Each entry is the
    /// resolved key or the error message captured while resolving it.
    ceks: Vec<Result<Arc<Vec<u8>>, String>>,
}

impl ResolvedCekDecryptor {
    /// Resolves every entry in `cek_table` against `registry`/`cache`, producing
    /// a decryptor that can synchronously decrypt cells encrypted with any of
    /// those keys.
    ///
    /// Resolution never fails as a whole: a per-entry failure is recorded and
    /// only reported when a column using that ordinal is decrypted.
    pub(crate) async fn resolve(
        registry: &ColumnEncryptionKeyStoreProviderRegistry,
        cache: &CekCache,
        cek_table: &[CekTableEntry],
    ) -> Self {
        let mut ceks = Vec::with_capacity(cek_table.len());
        for entry in cek_table {
            ceks.push(
                decrypt_cek(registry, cache, entry)
                    .await
                    .map_err(|error| error.to_string()),
            );
        }
        Self { ceks }
    }
}

impl CellDecryptor for ResolvedCekDecryptor {
    fn decrypt(
        &self,
        crypto_metadata: &CryptoMetadata,
        cipher_blob: &[u8],
    ) -> TdsResult<ColumnValues> {
        let ordinal = crypto_metadata.cek_table_ordinal as usize;
        let cek = self.ceks.get(ordinal).ok_or_else(|| {
            Error::ColumnEncryptionError(format!(
                "Encrypted column references CEK table ordinal {ordinal}, but the CEK table has \
                 {} entries",
                self.ceks.len()
            ))
        })?;
        let cek = cek.as_ref().map_err(|message| {
            Error::ColumnEncryptionError(format!(
                "Column encryption key at ordinal {ordinal} could not be resolved: {message}"
            ))
        })?;
        decrypt_cell(crypto_metadata, cek, cipher_blob)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;
    use crate::query::metadata::EncryptedCekValue;

    /// A provider that returns a fixed plaintext key and counts its invocations.
    struct MockProvider {
        plaintext: Vec<u8>,
        calls: AtomicUsize,
    }

    impl MockProvider {
        fn new(plaintext: Vec<u8>) -> Self {
            Self {
                plaintext,
                calls: AtomicUsize::new(0),
            }
        }
    }

    #[async_trait]
    impl ColumnEncryptionKeyStoreProvider for MockProvider {
        async fn decrypt_column_encryption_key(
            &self,
            _master_key_path: &str,
            _encryption_algorithm: &str,
            _encrypted_cek: &[u8],
        ) -> TdsResult<Vec<u8>> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(self.plaintext.clone())
        }
    }

    /// A provider that always fails.
    struct FailingProvider;

    #[async_trait]
    impl ColumnEncryptionKeyStoreProvider for FailingProvider {
        async fn decrypt_column_encryption_key(
            &self,
            _master_key_path: &str,
            _encryption_algorithm: &str,
            _encrypted_cek: &[u8],
        ) -> TdsResult<Vec<u8>> {
            Err(Error::ColumnEncryptionError("provider failed".to_string()))
        }
    }

    fn cek_value(store: &str, path: &str, key: &[u8]) -> EncryptedCekValue {
        EncryptedCekValue {
            encrypted_key: key.to_vec(),
            key_store_name: store.to_string(),
            key_path: path.to_string(),
            algorithm_name: "RSA_OAEP".to_string(),
        }
    }

    fn entry(values: Vec<EncryptedCekValue>) -> CekTableEntry {
        CekTableEntry {
            database_id: 1,
            cek_id: 2,
            cek_version: 1,
            cek_md_version: [0u8; 8],
            encrypted_cek_values: values,
        }
    }

    #[test]
    fn registry_lookup_is_case_insensitive() {
        let mut registry = ColumnEncryptionKeyStoreProviderRegistry::new();
        registry.register(
            "MSSQL_CERTIFICATE_STORE",
            Arc::new(MockProvider::new(vec![1u8; 32])),
        );

        assert!(registry.get("mssql_certificate_store").is_some());
        assert!(registry.get("MSSQL_Certificate_Store").is_some());
        assert!(registry.get("UNKNOWN").is_none());
        assert!(!registry.is_empty());
    }

    #[tokio::test]
    async fn decrypt_cek_resolves_and_caches() {
        let provider = Arc::new(MockProvider::new(vec![7u8; 32]));
        let mut registry = ColumnEncryptionKeyStoreProviderRegistry::new();
        registry.register("PROVIDER", provider.clone());
        let cache = CekCache::new();

        let entry = entry(vec![cek_value("PROVIDER", "path", &[0xAB, 0xCD])]);

        let first = decrypt_cek(&registry, &cache, &entry).await.unwrap();
        assert_eq!(first.as_ref(), &vec![7u8; 32]);

        // Second resolution must come from the cache (provider not called again).
        let second = decrypt_cek(&registry, &cache, &entry).await.unwrap();
        assert_eq!(second.as_ref(), &vec![7u8; 32]);
        assert_eq!(provider.calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn decrypt_cek_falls_back_to_next_value() {
        let mut registry = ColumnEncryptionKeyStoreProviderRegistry::new();
        registry.register("FAILING", Arc::new(FailingProvider));
        registry.register("GOOD", Arc::new(MockProvider::new(vec![9u8; 32])));
        let cache = CekCache::new();

        // First value uses a provider that fails; second value succeeds.
        let entry = entry(vec![
            cek_value("FAILING", "p1", &[1]),
            cek_value("GOOD", "p2", &[2]),
        ]);

        let key = decrypt_cek(&registry, &cache, &entry).await.unwrap();
        assert_eq!(key.as_ref(), &vec![9u8; 32]);
    }

    #[tokio::test]
    async fn decrypt_cek_errors_when_no_provider_registered() {
        let registry = ColumnEncryptionKeyStoreProviderRegistry::new();
        let cache = CekCache::new();
        let entry = entry(vec![cek_value("MISSING", "p", &[1])]);

        let error = decrypt_cek(&registry, &cache, &entry).await.unwrap_err();
        assert!(matches!(error, Error::ColumnEncryptionError(_)));
    }

    #[tokio::test]
    async fn decrypt_cek_errors_when_no_values() {
        let registry = ColumnEncryptionKeyStoreProviderRegistry::new();
        let cache = CekCache::new();
        let entry = entry(vec![]);

        let error = decrypt_cek(&registry, &cache, &entry).await.unwrap_err();
        assert!(matches!(error, Error::ColumnEncryptionError(_)));
    }

    use crate::datatypes::column_values::ColumnValues;
    use crate::datatypes::sqldatatypes::{
        FixedLengthTypes, TdsDataType, TypeInfo, TypeInfoVariant,
    };
    use crate::query::metadata::CryptoMetadata;
    use crate::security::encryption::{AeadAes256CbcHmacSha256, ColumnEncryptionType};

    /// Builds `CryptoMetadata` describing an encrypted `int` column whose CEK
    /// lives at `ordinal` in the CEK table.
    fn int_crypto_metadata(ordinal: u16) -> CryptoMetadata {
        CryptoMetadata {
            cek_table_ordinal: ordinal,
            base_data_type: TdsDataType::Int4,
            base_type_info: TypeInfo {
                tds_type: TdsDataType::Int4,
                length: 4,
                type_info_variant: TypeInfoVariant::FixedLen(FixedLengthTypes::Int4),
            },
            cipher_algorithm_id: 0x01,
            cipher_algorithm_name: None,
            encryption_type: 1,
            normalization_rule_version: 1,
        }
    }

    #[tokio::test]
    async fn resolved_decryptor_decrypts_cell() {
        let cek = vec![3u8; 32];

        // Encrypt the normalized 8-byte little-endian form of an `int` value.
        let value: i64 = 0x1234_5678;
        let cipher = AeadAes256CbcHmacSha256::new(&cek)
            .unwrap()
            .encrypt(&value.to_le_bytes(), ColumnEncryptionType::Deterministic)
            .unwrap();

        let mut registry = ColumnEncryptionKeyStoreProviderRegistry::new();
        registry.register("PROVIDER", Arc::new(MockProvider::new(cek.clone())));
        let cache = CekCache::new();
        let cek_table = vec![entry(vec![cek_value("PROVIDER", "path", &[0xAB])])];

        let decryptor = ResolvedCekDecryptor::resolve(&registry, &cache, &cek_table).await;

        let crypto = int_crypto_metadata(0);
        let decrypted = decryptor.decrypt(&crypto, &cipher).unwrap();
        assert_eq!(decrypted, ColumnValues::Int(0x1234_5678));
    }

    #[tokio::test]
    async fn resolved_decryptor_reports_unresolved_cek_on_use() {
        let mut registry = ColumnEncryptionKeyStoreProviderRegistry::new();
        registry.register("FAILING", Arc::new(FailingProvider));
        let cache = CekCache::new();
        let cek_table = vec![entry(vec![cek_value("FAILING", "p", &[1])])];

        let decryptor = ResolvedCekDecryptor::resolve(&registry, &cache, &cek_table).await;

        // Resolution failed, but the error only surfaces when the ordinal is used.
        let error = decryptor
            .decrypt(&int_crypto_metadata(0), &[0u8; 16])
            .unwrap_err();
        assert!(matches!(error, Error::ColumnEncryptionError(_)));
    }

    #[tokio::test]
    async fn resolved_decryptor_errors_on_out_of_range_ordinal() {
        let registry = ColumnEncryptionKeyStoreProviderRegistry::new();
        let cache = CekCache::new();

        let decryptor = ResolvedCekDecryptor::resolve(&registry, &cache, &[]).await;

        let error = decryptor.decrypt(&int_crypto_metadata(5), &[]).unwrap_err();
        assert!(matches!(error, Error::ColumnEncryptionError(_)));
    }
}
