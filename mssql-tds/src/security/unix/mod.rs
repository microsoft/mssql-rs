// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Unix GSSAPI (Generic Security Services API) implementation.
//!
//! This module provides Kerberos authentication support on Linux and macOS
//! using the GSSAPI from `libgssapi_krb5`.
//!
//! # Requirements
//!
//! - `libkrb5` and `libgssapi_krb5` installed
//! - Valid Kerberos ticket (run `kinit` before connecting)
//! - Properly configured `/etc/krb5.conf`
//!
//! # Note
//!
//! NTLM is **not** supported on Unix platforms. Only Kerberos authentication
//! is available through GSSAPI.

mod gssapi_context;
#[allow(dead_code)]
// GSSAPI spec-defined constants and FFI types; full set defined for completeness
mod gssapi_ffi;

pub use gssapi_context::GssapiContext;
pub(crate) use gssapi_context::convert_spn_to_gssapi_format;
pub use gssapi_ffi::{default_principal_name, has_valid_credentials, is_gssapi_available};

/// Checks if GSSAPI is available on this system.
pub fn is_available() -> bool {
    is_gssapi_available()
}

/// Checks if a valid Kerberos ticket exists.
///
/// # Returns
///
/// `true` if a valid TGT exists in the credential cache, `false` otherwise.
pub fn has_valid_ticket() -> bool {
    has_valid_credentials()
}
