// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Cryptographic primitives for Always Encrypted (column encryption).
//!
//! This module implements the cell-encryption algorithm used by SQL Server's
//! Always Encrypted feature. It is gated behind the `column-encryption` cargo
//! feature, which pulls in the platform-selected crypto backend from
//! [`crate::security::crypto`] (Windows CNG, Apple Security.framework +
//! CommonCrypto, or OpenSSL) for the underlying AES and HMAC primitives.

mod aead_aes_256_cbc_hmac_sha256;
mod cell;

#[allow(unused_imports)]
pub(crate) use aead_aes_256_cbc_hmac_sha256::{AeadAes256CbcHmacSha256, ColumnEncryptionType};
#[allow(unused_imports)]
pub(crate) use cell::decrypt_cell;
#[allow(unused_imports)]
pub(crate) use cell::encrypt_cell_value;
pub(crate) use cell::encrypt_parameter;
