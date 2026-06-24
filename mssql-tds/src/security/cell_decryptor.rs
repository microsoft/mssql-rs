// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! The [`CellDecryptor`] seam used by the result-set decode path to turn an
//! encrypted cell's cipher bytes back into a plaintext [`ColumnValues`].
//!
//! Implementations resolve the plaintext column encryption key (CEK) for a
//! column's [`CryptoMetadata`] and run the AEAD decryption plus
//! denormalization. The trait is intentionally synchronous and free of the
//! `column-encryption` feature gate so the always-compiled row parsers and
//! [`ParserContext`](crate::io::token_stream::ParserContext) can hold a
//! `dyn CellDecryptor` without conditional compilation. The concrete
//! implementation (`ResolvedCekDecryptor`, in [`crate::security::keystore`])
//! lives behind the `column-encryption` feature and is built once per result
//! set after the CEKs have been resolved, keeping per-cell decryption free of
//! `async`, allocation, and lock contention.

// The trait is consumed by the result-set decode path; without the
// `column-encryption` feature it has no implementor and is otherwise unused.
#![allow(dead_code)]

use crate::core::TdsResult;
use crate::datatypes::column_values::ColumnValues;
use crate::query::metadata::CryptoMetadata;

/// Decrypts a single encrypted cell using already-resolved column encryption
/// keys.
pub(crate) trait CellDecryptor: std::fmt::Debug + Send + Sync {
    /// Decrypts `cipher_blob` for the column described by `crypto_metadata`,
    /// returning the denormalized plaintext value.
    ///
    /// # Errors
    ///
    /// Returns [`Error::ColumnEncryptionError`](crate::error::Error::ColumnEncryptionError)
    /// if the column's CEK could not be resolved, or if decryption or
    /// denormalization fails.
    fn decrypt(
        &self,
        crypto_metadata: &CryptoMetadata,
        cipher_blob: &[u8],
    ) -> TdsResult<ColumnValues>;
}
