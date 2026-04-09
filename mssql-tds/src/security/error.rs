// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Security-related error types for integrated authentication.

use std::fmt;

/// Error type for security operations during integrated authentication.
#[derive(Debug, Clone)]
pub enum SecurityError {
    /// Failed to load the security library (secur32.dll or libgssapi_krb5).
    LoadLibraryFailed(String),

    /// Failed to acquire user credentials.
    AcquireCredentialsFailed {
        /// Platform-specific error code
        code: u32,
        /// Human-readable error message
        message: String,
    },

    /// Failed to initialize security context during authentication.
    InitContextFailed {
        /// Platform-specific error code
        code: u32,
        /// Human-readable error message
        message: String,
    },

    /// The security token received from the server is invalid.
    InvalidToken,

    /// The target SPN (Service Principal Name) is unknown or invalid.
    TargetUnknown(String),

    /// The SPN format is invalid.
    InvalidSpnFormat(String),

    /// Authentication timed out.
    Timeout,

    /// User credentials have expired.
    CredentialsExpired,

    /// No credentials are available for authentication.
    /// On Windows: No logged-in user or cached credentials.
    /// On Linux: No valid Kerberos ticket (run `kinit` first).
    NoCredentials,

    /// Authentication was denied by the server.
    AuthenticationDenied(String),

    /// GSSAPI-specific error (Linux/macOS only).
    #[cfg(unix)]
    GssapiError {
        /// Major status code
        major: u32,
        /// Minor status code
        minor: u32,
        /// Human-readable error message
        message: String,
    },

    /// SSPI-specific error (Windows only).
    #[cfg(windows)]
    SspiError {
        /// SECURITY_STATUS code
        status: i32,
        /// Human-readable error message
        message: String,
    },

    /// Integrated authentication is not supported on this platform/configuration.
    NotSupported(String),

    /// An internal error occurred.
    InternalError(String),
}

impl fmt::Display for SecurityError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SecurityError::LoadLibraryFailed(lib) => {
                write!(f, "Failed to load security library: {}", lib)
            }
            SecurityError::AcquireCredentialsFailed { code, message } => {
                write!(
                    f,
                    "Failed to acquire credentials (code=0x{:08X}): {}",
                    code, message
                )
            }
            SecurityError::InitContextFailed { code, message } => {
                write!(
                    f,
                    "Failed to initialize security context (code=0x{:08X}): {}",
                    code, message
                )
            }
            SecurityError::InvalidToken => {
                write!(f, "Invalid security token received from server")
            }
            SecurityError::TargetUnknown(spn) => {
                write!(f, "Target SPN unknown or not found: {}", spn)
            }
            SecurityError::InvalidSpnFormat(spn) => {
                write!(f, "Invalid SPN format: {}", spn)
            }
            SecurityError::Timeout => {
                write!(f, "Authentication timed out")
            }
            SecurityError::CredentialsExpired => {
                write!(f, "User credentials have expired")
            }
            SecurityError::NoCredentials => {
                write!(
                    f,
                    "No credentials available for authentication. \
                     On Linux, ensure you have a valid Kerberos ticket (run 'kinit')."
                )
            }
            SecurityError::AuthenticationDenied(reason) => {
                write!(f, "Authentication denied: {}", reason)
            }
            #[cfg(unix)]
            SecurityError::GssapiError {
                major,
                minor,
                message,
            } => {
                write!(
                    f,
                    "GSSAPI error (major=0x{:08X}, minor=0x{:08X}): {}",
                    major, minor, message
                )
            }
            #[cfg(windows)]
            SecurityError::SspiError { status, message } => {
                write!(f, "SSPI error (status=0x{:08X}): {}", status, message)
            }
            SecurityError::NotSupported(reason) => {
                write!(f, "Integrated authentication not supported: {}", reason)
            }
            SecurityError::InternalError(msg) => {
                write!(f, "Internal security error: {}", msg)
            }
        }
    }
}

impl std::error::Error for SecurityError {}

// Common SSPI status codes for reference
#[cfg(windows)]
#[allow(dead_code)] // SSPI spec-defined constants; full set defined for completeness
pub mod sspi_status {
    /// The function completed successfully.
    pub const SEC_E_OK: i32 = 0x00000000_u32 as i32;

    /// The function completed successfully, but you must call this function again.
    pub const SEC_I_CONTINUE_NEEDED: i32 = 0x00090312_u32 as i32;

    /// The function completed, but CompleteToken must be called.
    pub const SEC_I_COMPLETE_NEEDED: i32 = 0x00090313_u32 as i32;

    /// The function completed, but both CompleteToken and this function must be called.
    pub const SEC_I_COMPLETE_AND_CONTINUE: i32 = 0x00090314_u32 as i32;

    /// No credentials are available in the security package.
    pub const SEC_E_NO_CREDENTIALS: i32 = 0x8009030E_u32 as i32;

    /// The target principal name is incorrect.
    pub const SEC_E_TARGET_UNKNOWN: i32 = 0x80090303_u32 as i32;

    /// The logon was denied.
    pub const SEC_E_LOGON_DENIED: i32 = 0x8009030C_u32 as i32;

    /// The token supplied to the function is invalid.
    pub const SEC_E_INVALID_TOKEN: i32 = 0x80090308_u32 as i32;

    /// The credentials supplied were not complete.
    pub const SEC_E_INCOMPLETE_CREDENTIALS: i32 = 0x80090320_u32 as i32;

    /// An internal error occurred.
    pub const SEC_E_INTERNAL_ERROR: i32 = 0x80090304_u32 as i32;
}

// Common GSSAPI status codes for reference
#[cfg(unix)]
#[allow(dead_code)] // GSSAPI spec-defined constants; full set defined for completeness
pub mod gssapi_status {
    /// The routine completed successfully.
    pub const GSS_S_COMPLETE: u32 = 0;

    /// Continuation call to routine required.
    pub const GSS_S_CONTINUE_NEEDED: u32 = 1 << 0; // 0x00000001

    /// The token was a duplicate.
    pub const GSS_S_DUPLICATE_TOKEN: u32 = 1 << 1;

    /// The token's validity period has expired.
    pub const GSS_S_OLD_TOKEN: u32 = 1 << 2;

    /// A later token has already been processed.
    pub const GSS_S_UNSEQ_TOKEN: u32 = 1 << 3;

    /// An expected per-message token was not received.
    pub const GSS_S_GAP_TOKEN: u32 = 1 << 4;

    // Calling errors (bits 24-31)
    /// A required input parameter could not be read.
    pub const GSS_S_CALL_INACCESSIBLE_READ: u32 = 1 << 24;

    /// A required output parameter could not be written.
    pub const GSS_S_CALL_INACCESSIBLE_WRITE: u32 = 2 << 24;

    /// A parameter was malformed.
    pub const GSS_S_CALL_BAD_STRUCTURE: u32 = 3 << 24;

    // Routine errors (bits 16-23)
    /// An unsupported mechanism was requested.
    pub const GSS_S_BAD_MECH: u32 = 1 << 16;

    /// An invalid name was supplied.
    pub const GSS_S_BAD_NAME: u32 = 2 << 16;

    /// A supplied name was of an unsupported type.
    pub const GSS_S_BAD_NAMETYPE: u32 = 3 << 16;

    /// Incorrect channel bindings were supplied.
    pub const GSS_S_BAD_BINDINGS: u32 = 4 << 16;

    /// An invalid status code was supplied.
    pub const GSS_S_BAD_STATUS: u32 = 5 << 16;

    /// A token had an invalid MIC.
    pub const GSS_S_BAD_SIG: u32 = 6 << 16;

    /// No credentials were supplied.
    pub const GSS_S_NO_CRED: u32 = 7 << 16;

    /// No context has been established.
    pub const GSS_S_NO_CONTEXT: u32 = 8 << 16;

    /// A token was invalid.
    pub const GSS_S_DEFECTIVE_TOKEN: u32 = 9 << 16;

    /// A credential was invalid.
    pub const GSS_S_DEFECTIVE_CREDENTIAL: u32 = 10 << 16;

    /// The referenced credentials have expired.
    pub const GSS_S_CREDENTIALS_EXPIRED: u32 = 11 << 16;

    /// The context has expired.
    pub const GSS_S_CONTEXT_EXPIRED: u32 = 12 << 16;

    /// Miscellaneous failure.
    pub const GSS_S_FAILURE: u32 = 13 << 16;

    /// The quality-of-protection requested could not be provided.
    pub const GSS_S_BAD_QOP: u32 = 14 << 16;

    /// The operation is forbidden by local security policy.
    pub const GSS_S_UNAUTHORIZED: u32 = 15 << 16;

    /// The operation or option is unavailable.
    pub const GSS_S_UNAVAILABLE: u32 = 16 << 16;

    /// The requested credential element already exists.
    pub const GSS_S_DUPLICATE_ELEMENT: u32 = 17 << 16;

    /// The provided name was not a mechanism name.
    pub const GSS_S_NAME_NOT_MN: u32 = 18 << 16;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_error_display_load_library_failed() {
        let err = SecurityError::LoadLibraryFailed("secur32.dll".to_string());
        assert!(err.to_string().contains("secur32.dll"));
    }

    #[test]
    fn test_error_display_acquire_credentials_failed() {
        let err = SecurityError::AcquireCredentialsFailed {
            code: 0x8009030E,
            message: "No creds".to_string(),
        };
        let msg = err.to_string();
        assert!(msg.contains("8009030E"));
        assert!(msg.contains("No creds"));
    }

    #[test]
    fn test_error_display_no_credentials() {
        let err = SecurityError::NoCredentials;
        let msg = err.to_string();
        assert!(msg.contains("No credentials"));
        assert!(msg.contains("kinit"));
    }

    #[test]
    fn test_error_display_invalid_spn() {
        let err = SecurityError::InvalidSpnFormat("bad-spn".to_string());
        assert!(err.to_string().contains("bad-spn"));
    }

    #[test]
    fn test_error_display_init_context_failed() {
        let err = SecurityError::InitContextFailed {
            code: 0x80090303,
            message: "Target unknown".to_string(),
        };
        let msg = err.to_string();
        assert!(msg.contains("80090303"));
        assert!(msg.contains("Target unknown"));
    }

    #[test]
    fn test_error_display_invalid_token() {
        let err = SecurityError::InvalidToken;
        assert!(err.to_string().contains("Invalid security token"));
    }

    #[test]
    fn test_error_display_target_unknown() {
        let err = SecurityError::TargetUnknown("MSSQLSvc/host:1433".to_string());
        assert!(err.to_string().contains("MSSQLSvc/host:1433"));
    }

    #[test]
    fn test_error_display_timeout() {
        let err = SecurityError::Timeout;
        assert!(err.to_string().contains("timed out"));
    }

    #[test]
    fn test_error_display_credentials_expired() {
        let err = SecurityError::CredentialsExpired;
        assert!(err.to_string().contains("expired"));
    }

    #[test]
    fn test_error_display_authentication_denied() {
        let err = SecurityError::AuthenticationDenied("bad password".to_string());
        assert!(err.to_string().contains("bad password"));
    }

    #[cfg(unix)]
    #[test]
    fn test_error_display_gssapi_error() {
        let err = SecurityError::GssapiError {
            major: 0x000D0000,
            minor: 0x96C73A05,
            message: "Failure".to_string(),
        };
        let msg = err.to_string();
        assert!(msg.contains("GSSAPI"));
        assert!(msg.contains("Failure"));
    }

    #[test]
    fn test_error_display_not_supported() {
        let err = SecurityError::NotSupported("no GSSAPI".to_string());
        assert!(err.to_string().contains("no GSSAPI"));
    }

    #[test]
    fn test_error_display_internal_error() {
        let err = SecurityError::InternalError("unexpected state".to_string());
        assert!(err.to_string().contains("unexpected state"));
    }

    #[test]
    fn test_security_error_is_std_error() {
        let err = SecurityError::Timeout;
        let _: &dyn std::error::Error = &err;
    }
}
