// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Security module: integrated authentication plus Always Encrypted crypto.
//!
//! Integrated authentication: abstractions and implementations for SSPI
//! (Windows) and GSSAPI (Linux/macOS) authentication with SQL Server.
//!
//! Always Encrypted: the client-side crypto infrastructure — platform-native
//! crypto primitives (`crypto`), the AEAD cell cipher and parameter encryption
//! (`encryption`), `sp_describe_parameter_encryption` parsing
//! (`describe_parameter_encryption`), the cell-decryptor seam
//! (`cell_decryptor`), and the column master key store providers (`keystore`).
//! Results of `sp_describe_parameter_encryption` are cached per connection by
//! the query-metadata cache (`query_metadata_cache`).
//!
//! # Platform Support
//!
//! - **Windows**: SSPI with Negotiate, Kerberos, and NTLM packages
//! - **Linux/macOS**: GSSAPI with Kerberos only
//!
//! # Feature Flags
//!
//! - `sspi`: Enables Windows SSPI support
//! - `gssapi`: Enables Unix GSSAPI/Kerberos support

pub(crate) mod cell_decryptor;
pub(crate) mod crypto;
pub(crate) mod describe_parameter_encryption;
pub(crate) mod encryption;
mod error;
pub(crate) mod keystore;
pub mod mock;
pub(crate) mod query_metadata_cache;
mod security_context;
mod spn;

// Re-export public types
pub use error::SecurityError;
pub use security_context::{IntegratedAuthConfig, SecurityContext, SecurityPackage, SspiAuthToken};
pub use spn::{canonicalize_hostname, is_loopback_address, make_spn, make_spn_canonicalized};

// Always Encrypted key store provider API.
pub use keystore::{ColumnEncryptionKeyStoreProvider, RsaKeyStoreProvider};

// Platform-specific implementations
#[cfg(windows)]
pub mod windows;

#[cfg(unix)]
pub mod unix;

// Re-export platform implementations
#[cfg(windows)]
pub use windows::WindowsSspiContext;

#[cfg(unix)]
pub use unix::GssapiContext;

/// The `server_spn` setting ([`IntegratedAuthConfig::server_spn`], or the
/// client context's) that requests the SPN `spn`, made as [`make_spn`] makes
/// it. A configured SPN is used as given, so it must already be in the form
/// the platform takes: SSPI's `service/host:port`, or GSSAPI's host-based
/// `service@host`, into which an SPN the client makes itself is converted.
pub fn configured_spn(spn: &str) -> String {
    #[cfg(unix)]
    {
        unix::convert_spn_to_gssapi_format(spn)
    }
    #[cfg(not(unix))]
    {
        spn.to_string()
    }
}

/// Creates a platform-appropriate security context.
///
/// On Windows, creates a `WindowsSspiContext`.
/// On Unix, creates a `GssapiContext`.
///
/// # Arguments
///
/// * `config` - Configuration for integrated authentication
/// * `server` - The server hostname (FQDN preferred)
/// * `port` - The server port
///
/// # Errors
///
/// Returns `SecurityError` if the security context cannot be created.
#[cfg(windows)]
pub fn create_security_context(
    config: &IntegratedAuthConfig,
    server: &str,
    port: u16,
) -> Result<Box<dyn SecurityContext>, SecurityError> {
    Ok(Box::new(windows::WindowsSspiContext::new(
        config, server, port,
    )?))
}

/// Creates a platform-specific security context for integrated authentication.
#[cfg(unix)]
pub fn create_security_context(
    config: &IntegratedAuthConfig,
    server: &str,
    port: u16,
) -> Result<Box<dyn SecurityContext>, SecurityError> {
    Ok(Box::new(unix::GssapiContext::new(config, server, port)?))
}

#[cfg(not(any(windows, unix)))]
pub fn create_security_context(
    _config: &IntegratedAuthConfig,
    _server: &str,
    _port: u16,
) -> Result<Box<dyn SecurityContext>, SecurityError> {
    Err(SecurityError::NotSupported(
        "Integrated authentication is only supported on Windows and Unix platforms".to_string(),
    ))
}

#[cfg(test)]
mod tests {
    /// A configured SPN must request the SPN the client would make itself.
    #[test]
    fn a_configured_spn_is_in_the_form_the_platform_takes() {
        let spn = super::make_spn("db.contoso.com", None, 1433);
        assert_eq!(spn, "MSSQLSvc/db.contoso.com:1433");
        let expected = if cfg!(unix) {
            "MSSQLSvc@db.contoso.com"
        } else {
            "MSSQLSvc/db.contoso.com:1433"
        };
        assert_eq!(super::configured_spn(&spn), expected);

        // An IPv6 host keeps its colons; only the port is dropped.
        let spn = super::make_spn("::1", None, 1433);
        let expected = if cfg!(unix) {
            "MSSQLSvc@::1"
        } else {
            "MSSQLSvc/::1:1433"
        };
        assert_eq!(super::configured_spn(&spn), expected);
        let spn = super::make_spn("fe80::1", Some("INST"), 1433);
        if cfg!(unix) {
            assert_eq!(super::configured_spn(&spn), "MSSQLSvc@fe80::1");
        }
    }
}
