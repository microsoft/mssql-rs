// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Security module for integrated authentication support.
//!
//! This module provides abstractions and implementations for SSPI (Windows)
//! and GSSAPI (Linux/macOS) authentication with SQL Server.
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
#[cfg(feature = "column-encryption")]
pub(crate) mod describe_parameter_encryption;
#[cfg(feature = "column-encryption")]
pub(crate) mod encryption;
mod error;
#[cfg(feature = "column-encryption")]
pub(crate) mod keystore;
pub mod mock;
mod security_context;
mod spn;

// Re-export public types
pub use error::SecurityError;
pub use security_context::{IntegratedAuthConfig, SecurityContext, SecurityPackage, SspiAuthToken};
pub use spn::{canonicalize_hostname, is_loopback_address, make_spn, make_spn_canonicalized};

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
