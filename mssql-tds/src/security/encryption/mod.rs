// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Cryptographic primitives for Always Encrypted (column encryption).
//!
//! This module implements the cell-encryption algorithm used by SQL Server's
//! Always Encrypted feature. It is gated behind the `column-encryption` cargo
//! feature because it depends on OpenSSL for the underlying AES and HMAC
//! primitives.

mod aead_aes_256_cbc_hmac_sha256;

#[allow(unused_imports)]
pub(crate) use aead_aes_256_cbc_hmac_sha256::{AeadAes256CbcHmacSha256, ColumnEncryptionType};
