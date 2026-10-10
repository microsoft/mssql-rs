// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Unix GSSAPI context implementation.
//!
//! This module implements the `SecurityContext` trait using GSSAPI for
//! Kerberos authentication on Linux and macOS.
//!
//! # TDS Login Flow with GSSAPI/Kerberos
//!
//! The integrated authentication flow with SQL Server follows this sequence:
//!
//! ```text
//! ┌─────────┐                              ┌─────────┐
//! │ Client  │                              │ Server  │
//! └────┬────┘                              └────┬────┘
//!      │                                        │
//!      │  1. GssapiContext::new()               │
//!      │     - Import SPN, prepare context      │
//!      │                                        │
//!      │  2. generate_token(None)               │
//!      │     - Create initial Kerberos token    │
//!      │                                        │
//!      │  ───── LOGIN7 + SSPI token ──────────► │
//!      │                                        │
//!      │  ◄──── SSPI token (if more needed) ─── │
//!      │                                        │
//!      │  3. generate_token(Some(server_token)) │
//!      │     - Process server challenge         │
//!      │     - Check is_complete()              │
//!      │                                        │
//!      │  ───── SSPI response ────────────────► │
//!      │        (repeat until complete)         │
//!      │                                        │
//!      │  ◄──── LOGINACK ─────────────────────  │
//!      │                                        │
//!      │  4. Drop                               │
//!      │     - Release GSSAPI resources         │
//! ```
//!
//! ## Prerequisites
//!
//! - Valid Kerberos TGT must exist (run `kinit user@REALM`)
//! - SQL Server must have an SPN registered in Active Directory
//! - Client must be able to reach the KDC (Key Distribution Center)

use super::gssapi_ffi::{
    self, GSS_C_DELEG_FLAG, GSS_C_MUTUAL_FLAG, GSS_C_NO_CHANNEL_BINDINGS, GSS_C_NO_CONTEXT,
    GSS_C_NO_CREDENTIAL, GSS_C_NO_OID, GSS_S_COMPLETE, GSS_S_CONTINUE_NEEDED, GssBufferDesc,
    GssCtxIdT, GssNameT, GssOid, GssOmUint32, get_gss_nt_service_name, get_gssapi_error,
    gss_delete_sec_context, gss_import_name, gss_init_sec_context, gss_release_buffer,
    gss_release_name, spnego_mechanism,
};
use crate::security::{
    IntegratedAuthConfig, SecurityContext, SecurityError, SspiAuthToken,
    spn::make_spn_canonicalized,
};
use std::ptr;

/// Wrapper for GSSAPI context handle that implements Send + Sync.
///
/// GSSAPI library is designed to be thread-safe, so wrapping these
/// opaque handles is safe.
#[derive(Debug)]
struct GssCtxHandle(GssCtxIdT);

// SAFETY: GSSAPI is thread-safe and the handle is only used through FFI calls
unsafe impl Send for GssCtxHandle {}
unsafe impl Sync for GssCtxHandle {}

impl Default for GssCtxHandle {
    fn default() -> Self {
        Self(GSS_C_NO_CONTEXT)
    }
}

/// Wrapper for GSSAPI name handle that implements Send + Sync.
#[derive(Debug)]
struct GssNameHandle(GssNameT);

// SAFETY: GSSAPI is thread-safe and the handle is only used through FFI calls
unsafe impl Send for GssNameHandle {}
unsafe impl Sync for GssNameHandle {}

impl Default for GssNameHandle {
    fn default() -> Self {
        Self(ptr::null_mut())
    }
}

/// GSSAPI security context for Kerberos authentication.
///
/// This struct manages the GSSAPI authentication state. It requires a valid
/// Kerberos ticket (TGT) to be present in the credential cache (run `kinit`).
///
/// # Login Flow Usage
///
/// 1. Created via `new()` during connection setup, before sending LOGIN7
/// 2. `generate_token(None)` called to get initial token for LOGIN7 packet
/// 3. `generate_token(Some(token))` called for each server SSPI challenge
/// 4. Dropped after authentication completes or connection closes
pub struct GssapiContext {
    /// The SPN being used for authentication
    spn: String,

    /// GSSAPI context handle
    ctx_handle: GssCtxHandle,

    /// GSSAPI target name (imported SPN)
    target_name: GssNameHandle,

    /// Whether authentication is complete
    is_complete: bool,

    /// Channel bindings for extended protection
    #[allow(dead_code)]
    channel_bindings: Option<Vec<u8>>,

    /// Mechanism and request flags passed to gss_init_sec_context.
    purpose: ContextPurpose,
}

/// What a [GssapiContext] authenticates to, which fixes its mechanism and
/// request flags.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ContextPurpose {
    /// TDS login: default (Kerberos) mechanism with mutual authentication and
    /// credential delegation.
    TdsLogin,
    /// HTTP Negotiate (RFC 4559): SPNEGO with mutual authentication and no
    /// delegation, so the TGT is never forwarded to the web server.
    HttpNegotiate,
}

impl ContextPurpose {
    fn mechanism(self) -> GssOid {
        match self {
            Self::TdsLogin => GSS_C_NO_OID,
            Self::HttpNegotiate => spnego_mechanism(),
        }
    }

    fn req_flags(self) -> GssOmUint32 {
        match self {
            Self::TdsLogin => GSS_C_MUTUAL_FLAG | GSS_C_DELEG_FLAG,
            Self::HttpNegotiate => GSS_C_MUTUAL_FLAG,
        }
    }
}

impl GssapiContext {
    /// Creates a new GSSAPI security context.
    ///
    /// # Login Flow
    ///
    /// Called during connection setup, **before** the LOGIN7 packet is constructed.
    /// The handler factory creates this context when `TdsAuthenticationMethod::SSPI`
    /// is specified in the client configuration.
    ///
    /// # Arguments
    ///
    /// * `config` - Configuration for integrated authentication
    /// * `server` - The server hostname (should be FQDN for Kerberos)
    /// * `port` - The server port
    ///
    /// # Returns
    ///
    /// A new GSSAPI context, or an error if initialization fails.
    ///
    /// # Errors
    ///
    /// - `SecurityError::LoadLibraryFailed` if GSSAPI library is not available
    /// - `SecurityError::NoCredentials` if no Kerberos ticket is available
    /// - `SecurityError::InvalidSpn` if the SPN cannot be imported
    pub fn new(
        config: &IntegratedAuthConfig,
        server: &str,
        port: u16,
    ) -> Result<Self, SecurityError> {
        // Check GSSAPI availability first
        Self::check_availability()?;

        // Determine SPN and whether it was user-provided
        // For auto-generated SPNs, canonicalize the hostname via DNS lookup
        // to ensure it matches the SPN registered in Active Directory
        let (spn, is_user_provided) = match &config.server_spn {
            Some(user_spn) => (user_spn.clone(), true),
            None => (make_spn_canonicalized(server, None, port), false),
        };

        // Import the SPN as a GSSAPI name
        // Only convert auto-generated SPNs; user-provided SPNs are used as-is
        let target_name = import_name(&spn, is_user_provided)?;

        Ok(Self {
            spn,
            ctx_handle: GssCtxHandle::default(),
            target_name: GssNameHandle(target_name),
            is_complete: false,
            channel_bindings: config.channel_bindings.clone(),
            purpose: ContextPurpose::TdsLogin,
        })
    }

    /// Creates a context for HTTP Negotiate authentication to HTTP@host.
    ///
    /// Used for the ADFS WS-Trust request in Entra integrated authentication,
    /// where msodbcsql uses curl's CURLAUTH_GSSNEGOTIATE
    /// (AzureADAuth.cpp Request): SPNEGO, mutual authentication, and no
    /// delegation, since msodbcsql never sets CURLOPT_GSSAPI_DELEGATION.
    pub fn for_http_negotiate(host: &str) -> Result<Self, SecurityError> {
        Self::check_availability()?;
        let spn = format!("HTTP@{host}");
        let target_name = import_name(&spn, true)?;
        Ok(Self {
            spn,
            ctx_handle: GssCtxHandle::default(),
            target_name: GssNameHandle(target_name),
            is_complete: false,
            channel_bindings: None,
            purpose: ContextPurpose::HttpNegotiate,
        })
    }

    /// Checks if GSSAPI is available on this system.
    ///
    /// # Login Flow
    ///
    /// Called early during connection setup to fail fast if GSSAPI libraries
    /// are not installed. This prevents confusing errors later in the flow.
    pub fn check_availability() -> Result<(), SecurityError> {
        if gssapi_ffi::is_gssapi_available() {
            Ok(())
        } else {
            Err(SecurityError::LoadLibraryFailed(
                "libgssapi_krb5.so: GSSAPI library not available".to_string(),
            ))
        }
    }
}

impl SecurityContext for GssapiContext {
    fn package_name(&self) -> &str {
        match self.purpose {
            ContextPurpose::TdsLogin => "Kerberos",
            ContextPurpose::HttpNegotiate => "Negotiate",
        }
    }

    /// Generates or continues GSSAPI authentication.
    ///
    /// # Login Flow
    ///
    /// - **First call** (`server_token = None`): Called when constructing the LOGIN7
    ///   packet. The returned token is embedded in the SSPI data section of LOGIN7.
    ///
    /// - **Subsequent calls** (`server_token = Some(...)`): Called when the server
    ///   responds with an SSPI token (parsed by `sspi_parser`). The client must
    ///   respond with another token until `is_complete()` returns true.
    ///
    /// # Returns
    ///
    /// An `SspiAuthToken` containing:
    /// - `data`: The GSSAPI token bytes to send to the server
    /// - `is_complete`: Whether authentication is finished
    fn generate_token(
        &mut self,
        server_token: Option<&[u8]>,
    ) -> Result<SspiAuthToken, SecurityError> {
        let mut minor_status: GssOmUint32 = 0;
        let mut output_token = GssBufferDesc::default();
        let mut ret_flags: GssOmUint32 = 0;
        let mut time_rec: GssOmUint32 = 0;

        // Prepare input token buffer
        let mut input_token_desc;
        let input_token = match server_token {
            Some(data) if !data.is_empty() => {
                input_token_desc = GssBufferDesc::from_slice(data);
                &mut input_token_desc as *mut GssBufferDesc
            }
            _ => ptr::null_mut(),
        };

        let req_flags = self.purpose.req_flags();

        // Call gss_init_sec_context
        let major_status = unsafe {
            gss_init_sec_context(
                &mut minor_status,
                GSS_C_NO_CREDENTIAL, // Use default credentials (from kinit)
                &mut self.ctx_handle.0,
                self.target_name.0,
                self.purpose.mechanism(),
                req_flags,
                0,                         // No time limit
                GSS_C_NO_CHANNEL_BINDINGS, // Channel bindings not supported on non-Windows
                input_token,
                ptr::null_mut(), // actual_mech_type
                &mut output_token,
                &mut ret_flags,
                &mut time_rec,
            )
        };

        // Check result
        let is_complete = major_status == GSS_S_COMPLETE;
        let continue_needed = major_status == GSS_S_CONTINUE_NEEDED;

        if !is_complete && !continue_needed {
            // Authentication failed - get error message
            let error_msg = get_gssapi_error(major_status, minor_status);

            // Check for specific error conditions
            if major_status & 0xFFFF0000 == gssapi_ffi::GSS_S_NO_CRED {
                // Use GssapiError which has more details
                return Err(SecurityError::GssapiError {
                    major: major_status,
                    minor: minor_status,
                    message: format!(
                        "No Kerberos credentials available. Run 'kinit' to obtain a ticket. {}",
                        error_msg
                    ),
                });
            }

            return Err(SecurityError::InitContextFailed {
                message: error_msg,
                code: major_status,
            });
        }

        // Copy output token to Vec<u8>
        let token_data = unsafe { output_token.to_vec() };

        // Release the output token buffer
        if !output_token.value.is_null() {
            unsafe {
                gss_release_buffer(&mut minor_status, &mut output_token);
            }
        }

        // Update completion state
        if is_complete {
            self.is_complete = true;
        }

        Ok(SspiAuthToken {
            data: token_data,
            is_complete,
        })
    }

    fn is_complete(&self) -> bool {
        self.is_complete
    }

    fn spn(&self) -> &str {
        &self.spn
    }
}

/// # Login Flow
///
/// Called when the connection is closed or authentication fails. Releases
/// GSSAPI resources including the security context handle and imported name.
impl Drop for GssapiContext {
    fn drop(&mut self) {
        // Only release resources if GSSAPI library is available.
        // If it was unloaded or never loaded, we can't call the cleanup functions.
        if !gssapi_ffi::is_gssapi_available() {
            return;
        }

        let mut minor_status: GssOmUint32 = 0;

        // Delete security context if it was created
        if self.ctx_handle.0 != GSS_C_NO_CONTEXT {
            unsafe {
                gss_delete_sec_context(&mut minor_status, &mut self.ctx_handle.0, ptr::null_mut());
            }
        }

        // Release target name
        if !self.target_name.0.is_null() {
            unsafe {
                gss_release_name(&mut minor_status, &mut self.target_name.0);
            }
        }
    }
}

/// Imports an SPN string into a GSSAPI name handle.
///
/// GSSAPI expects service names in `service@host` format when using
/// GSS_C_NT_HOSTBASED_SERVICE. For auto-generated SPNs, this function converts
/// the Windows-style SPN format `MSSQLSvc/host:port` to `MSSQLSvc@host`.
/// User-provided SPNs are passed through unchanged to allow full control.
///
/// # Arguments
///
/// * `spn` - The SPN string to import
/// * `user_provided` - If true, the SPN is used as-is without conversion
fn import_name(spn: &str, user_provided: bool) -> Result<GssNameT, SecurityError> {
    let mut minor_status: GssOmUint32 = 0;
    let mut target_name: GssNameT = ptr::null_mut();

    // For user-provided SPNs, use as-is; for auto-generated, convert to GSSAPI format
    let gssapi_name = if user_provided {
        spn.to_string()
    } else {
        let converted = convert_spn_to_gssapi_format(spn);
        if converted != spn {
            tracing::debug!(
                "Converted auto-generated SPN '{}' to GSSAPI format '{}'",
                spn,
                converted
            );
        }
        converted
    };
    let mut name_buffer = GssBufferDesc::from_str(&gssapi_name);

    // Use GSS_C_NT_HOSTBASED_SERVICE for "service@host" format
    let name_type = get_gss_nt_service_name();

    let major_status = unsafe {
        gss_import_name(
            &mut minor_status,
            &mut name_buffer,
            name_type,
            &mut target_name,
        )
    };

    if major_status != GSS_S_COMPLETE {
        let error_msg = get_gssapi_error(major_status, minor_status);
        return Err(SecurityError::InvalidSpnFormat(format!(
            "Failed to import SPN '{}': {}",
            spn, error_msg
        )));
    }

    Ok(target_name)
}

/// Converts a Windows-style SPN to GSSAPI host-based service format.
///
/// Windows SPNs use the format `MSSQLSvc/host:port` or `MSSQLSvc/host:instance`,
/// but GSSAPI's GSS_C_NT_HOSTBASED_SERVICE expects `service@host`.
///
/// # Examples
/// - `MSSQLSvc/sql.example.local:1433` → `MSSQLSvc@sql.example.local`
/// - `MSSQLSvc/server:INSTANCE1` → `MSSQLSvc@server`
/// - `MSSQLSvc@host` (already in GSSAPI format) → `MSSQLSvc@host`
pub(crate) fn convert_spn_to_gssapi_format(spn: &str) -> String {
    // If already in GSSAPI format (contains @), return as-is
    if spn.contains('@') && !spn.contains('/') {
        return spn.to_string();
    }

    // Parse Windows SPN format: service/host:port or service/host:instance
    if let Some(slash_pos) = spn.find('/') {
        let service = &spn[..slash_pos];
        let rest = &spn[slash_pos + 1..];

        // Extract host: everything before the last colon, which starts the
        // port or instance (an IPv6 host has colons of its own), or the whole
        // string if there is no colon
        let host = if let Some(colon_pos) = rest.rfind(':') {
            &rest[..colon_pos]
        } else {
            rest
        };

        format!("{}@{}", service, host)
    } else {
        // Unexpected format, return as-is
        spn.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_gssapi_context_creation() {
        let config = IntegratedAuthConfig::new();
        let result = GssapiContext::new(&config, "server.contoso.com", 1433);

        // This may fail if GSSAPI is not available (no krb5 installed),
        // but should at least not panic
        if let Ok(ctx) = result {
            assert_eq!(ctx.spn(), "MSSQLSvc/server.contoso.com:1433");
            assert!(!ctx.is_complete());
        }
    }

    #[test]
    fn test_gssapi_context_with_explicit_spn() {
        let config = IntegratedAuthConfig::with_spn("MSSQLSvc/custom:5000".to_string());
        if let Ok(ctx) = GssapiContext::new(&config, "server", 1433) {
            assert_eq!(ctx.spn(), "MSSQLSvc/custom:5000");
        }
    }

    #[test]
    fn test_gssapi_package_name() {
        let config = IntegratedAuthConfig::new();
        if let Ok(ctx) = GssapiContext::new(&config, "server", 1433) {
            assert_eq!(ctx.package_name(), "Kerberos");
        }
    }

    #[test]
    fn test_gssapi_check_availability() {
        // This should succeed if GSSAPI library is installed
        let result = GssapiContext::check_availability();
        // Don't assert - just check it doesn't panic
        let _ = result;
    }

    #[test]
    fn test_gssapi_generate_token_no_credentials() {
        // This test verifies behavior when no Kerberos ticket is available
        let config = IntegratedAuthConfig::new();
        if let Ok(mut ctx) = GssapiContext::new(&config, "server.contoso.com", 1433) {
            let result = ctx.generate_token(None);
            // Should fail with NoCredentials error if no kinit was run
            // or succeed if there's a valid ticket
            match result {
                Ok(token) => {
                    // Got a token - kinit must have been run
                    assert!(!token.data.is_empty());
                }
                Err(SecurityError::NoCredentials) => {
                    // Expected when no kinit
                }
                Err(e) => {
                    // Other errors might occur depending on system config
                    println!("Got error: {:?}", e);
                }
            }
        }
    }

    #[test]
    fn http_negotiate_uses_spnego_without_delegation() {
        // Delegation toward a web server would forward the user's TGT to it.
        let flags = ContextPurpose::HttpNegotiate.req_flags();
        assert_eq!(flags & GSS_C_DELEG_FLAG, 0);
        assert_ne!(flags & GSS_C_MUTUAL_FLAG, 0);
        let oid = unsafe { &*ContextPurpose::HttpNegotiate.mechanism() };
        let bytes = unsafe { std::slice::from_raw_parts(oid.elements as *const u8, 6) };
        assert_eq!(oid.length, 6);
        assert_eq!(bytes, [0x2b, 0x06, 0x01, 0x05, 0x05, 0x02]);
        // The TDS login path keeps its existing mechanism and flags.
        assert!(ContextPurpose::TdsLogin.mechanism().is_null());
        assert_eq!(
            ContextPurpose::TdsLogin.req_flags(),
            GSS_C_MUTUAL_FLAG | GSS_C_DELEG_FLAG
        );
    }

    #[test]
    fn http_negotiate_targets_the_http_service() {
        if let Ok(ctx) = GssapiContext::for_http_negotiate("adfs.contoso.com") {
            assert_eq!(ctx.spn(), "HTTP@adfs.contoso.com");
            assert_eq!(ctx.package_name(), "Negotiate");
        }
    }

    #[test]
    fn test_convert_spn_to_gssapi_format() {
        // Standard port-based SPN
        assert_eq!(
            convert_spn_to_gssapi_format("MSSQLSvc/sql.example.local:1433"),
            "MSSQLSvc@sql.example.local"
        );

        // Instance-based SPN
        assert_eq!(
            convert_spn_to_gssapi_format("MSSQLSvc/server:INSTANCE1"),
            "MSSQLSvc@server"
        );

        // Already in GSSAPI format
        assert_eq!(
            convert_spn_to_gssapi_format("MSSQLSvc@host"),
            "MSSQLSvc@host"
        );

        // SPN without port (unusual but possible)
        assert_eq!(
            convert_spn_to_gssapi_format("MSSQLSvc/server"),
            "MSSQLSvc@server"
        );
    }
}
