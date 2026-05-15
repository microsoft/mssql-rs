// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

pub mod bulk_copy_errors;

use std::path::PathBuf;

pub use bulk_copy_errors::{BulkCopyAttentionTimeoutError, BulkCopyError, BulkCopyTimeoutError};

use crate::security::SecurityError;
use thiserror::Error;
use tokio::time::error::Elapsed;

/// A single SQL Server error, analogous to SqlClient's `SqlError`.
///
/// SQL Server can return multiple errors for a single batch execution.
/// This struct represents one error from the stream. The full collection
/// is available via `Error::SqlServerError { errors }`.
#[derive(Debug, Clone)]
pub struct SqlErrorInfo {
    /// Error message text returned by the server.
    pub message: String,
    /// Error state, used by the server to indicate specific error conditions.
    pub state: u8,
    /// Severity class of the error (maps to TDS `Class` field).
    pub class: i32,
    /// Server-defined error number.
    pub number: u32,
    /// Name of the server that generated the error.
    pub server_name: Option<String>,
    /// Name of the stored procedure that generated the error.
    pub proc_name: Option<String>,
    /// Line number in the batch or procedure where the error occurred.
    pub line_number: Option<i32>,
}

impl std::fmt::Display for SqlErrorInfo {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "Sql Error: {}: Class {}: State {}: {} on {} in {} at line {}",
            self.number,
            self.class,
            self.state,
            self.message,
            self.server_name.as_deref().unwrap_or("Unknown"),
            self.proc_name.as_deref().unwrap_or("Unknown"),
            self.line_number.unwrap_or_default()
        )
    }
}

impl From<&crate::token::tokens::ErrorToken> for SqlErrorInfo {
    fn from(token: &crate::token::tokens::ErrorToken) -> Self {
        Self {
            message: token.message.clone(),
            state: token.state,
            class: token.severity as i32,
            number: token.number,
            server_name: Some(token.server_name.clone()),
            proc_name: Some(token.proc_name.clone()),
            line_number: Some(token.line_number as i32),
        }
    }
}

/// The source of a timeout: either a Tokio `Elapsed` or a descriptive string.
#[derive(Debug, Error)]
pub enum TimeoutErrorType {
    /// Wrapper around a Tokio `Elapsed` error.
    #[error("Elapsed: {0}")]
    Elapsed(Elapsed),

    /// Freeform timeout description.
    #[error("{0}")]
    String(String),
}

/// All errors produced by the TDS client.
#[derive(Debug, Error)]
pub enum Error {
    /// Underlying I/O failure.
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    /// Server requested a connection redirect.
    #[error("Server redirected the connection: {host}:{port} times")]
    Redirection {
        /// Target hostname.
        host: String,
        /// Target port.
        port: u16,
    },

    /// Failed to establish a connection.
    #[error("Connection Error: {0}")]
    ConnectionError(String),

    /// TDS protocol violation or unexpected server response.
    #[error("Protocol Error: {0}")]
    ProtocolError(String),

    /// TLS/SSL library error.
    #[error("TLS Error: {0}")]
    TlsError(#[from] native_tls::Error),

    /// TLS handshake failed with host/SAN details.
    #[error(
        "TLS handshake failed while connecting to '{expected_host}': {source}. Certificate SANs: {cert_sans}"
    )]
    TlsHandshakeError {
        /// Inner TLS error.
        source: native_tls::Error,
        /// Hostname the client tried to connect to.
        expected_host: String,
        /// Subject Alternative Names found on the certificate.
        cert_sans: String,
    },

    /// Operation exceeded its deadline.
    #[error("Timeout Error: {0}")]
    TimeoutError(TimeoutErrorType),

    /// Operation was cancelled via a `CancelHandle`.
    #[error("Operation Cancelled Error: {0}")]
    OperationCancelledError(String),

    /// One or more errors returned by SQL Server.
    #[error("{}", SqlServerError::format_errors(errors))]
    SqlServerError {
        /// Ordered list of server-reported errors.
        errors: Vec<SqlErrorInfo>,
    },

    /// Caller misused the API (e.g., invalid parameter combination).
    #[error("Usage Error: {0}")]
    UsageError(String),

    /// Internal logic error that should not occur.
    #[error("Unexpected Implementation Error: {0}")]
    ImplementationError(String),

    /// Feature recognized but not yet implemented.
    #[error("Unimplemented Feature: {feature} - {context}")]
    UnimplementedFeature {
        /// Name of the unimplemented feature.
        feature: String,
        /// Additional context.
        context: String,
    },

    /// Value could not be converted to the target SQL type.
    #[error("Type Conversion Error: {0}")]
    TypeConversionError(String),

    /// Connection was closed by server or transport.
    #[error("Connection closed: {0}")]
    ConnectionClosed(String),

    /// Code page / LCID has no mapped encoding.
    #[error(
        "Unsupported Encoding: LCID {lcid} (0x{lcid:04X}). Consider using NVARCHAR instead of VARCHAR/TEXT for better compatibility."
    )]
    UnsupportedEncoding {
        /// Windows locale ID that has no available encoding.
        lcid: u32,
    },

    /// Certificate file does not exist on disk.
    #[error(
        "Certificate file not found: {path}. Verify the ServerCertificate path is correct and the file exists."
    )]
    CertificateNotFound {
        /// File path that was looked up.
        path: PathBuf,
    },

    /// Certificate file is present but cannot be parsed.
    #[error(
        "Invalid certificate format in file: {path}. Ensure the file contains a valid DER or PEM encoded X.509 certificate."
    )]
    InvalidCertificateFormat {
        /// File path with the invalid certificate.
        path: PathBuf,
    },

    /// Server certificate has passed its validity period.
    #[error(
        "Server certificate has expired. The server's certificate is no longer valid. Contact your administrator."
    )]
    CertificateExpired,

    /// Server presented a certificate that does not match expectations.
    #[error(
        "Server certificate validation failed: Certificate mismatch. The server presented a different certificate than expected. Verify you are connecting to the correct server."
    )]
    CertificateMismatch,

    /// I/O error while reading a certificate file.
    #[error(
        "Failed to read certificate file: {path}. Error: {error}. Check file permissions and ensure the file is not locked by another process."
    )]
    CertificateFileIoError {
        /// File path that could not be read.
        path: PathBuf,
        /// Underlying I/O error message.
        error: String,
    },

    /// TLS handshake completed but server did not provide a certificate.
    #[error("No server certificate available during TLS handshake.")]
    NoServerCertificate,

    /// Error from a bulk copy operation.
    #[error("Bulk Copy Error: {0}")]
    BulkCopyError(#[from] BulkCopyError),

    /// Authentication / security subsystem error.
    #[error("Security error: {0}")]
    Security(#[from] SecurityError),

    /// All reconnection attempts exhausted.
    #[error("Session recovery failed after {attempts} attempt(s): {message}")]
    SessionRecoveryFailed {
        /// Number of reconnection attempts made.
        attempts: u32,
        /// Description of the final failure.
        message: String,
    },

    /// Session state prevents reconnection (transactions, unrecoverable state).
    #[error("Session not recoverable: {0}")]
    SessionNotRecoverable(String),

    /// Reconnected server properties don't match original.
    #[error("Reconnection validation failed: {0}")]
    ReconnectionValidationFailed(String),
}

/// Helper for `SqlServerError` display formatting.
struct SqlServerError;

impl SqlServerError {
    fn format_errors(errors: &[SqlErrorInfo]) -> String {
        match errors.len() {
            0 => "Sql Error: (no error details)".to_string(),
            1 => errors[0].to_string(),
            _ => errors
                .iter()
                .map(|e| e.to_string())
                .collect::<Vec<_>>()
                .join("\n"),
        }
    }
}

/// SQL Server error numbers that indicate a transient condition during
/// connection open. These are server-reported errors where retrying the
/// connection may succeed.
///
/// This list is the intersection of the transient error sets used by
/// SqlClient (`s_defaultTransientErrors` in `SqlConfigurableRetryFactory.cs`)
/// and JDBC (`TransientError` enum in `SQLServerError.java`).
///
/// Transport-level transient errors (JDBC codes 64, 10053, 10054) are already
/// covered by [`Error::Io`] and [`Error::ConnectionClosed`] variants.
const TRANSIENT_SQL_ERROR_NUMBERS: &[u32] = &[
    233,   // Connection closed by remote host / no process on other end of pipe
    4060,  // Cannot open database requested by the login
    4221,  // Login to read-secondary failed (HADR transition timeout)
    10928, // Resource limit reached (Azure SQL)
    10929, // Resource minimum guarantee exceeded (Azure SQL)
    40143, // Service error processing request
    40197, // Service error during upgrades/failover (may embed 40020/40143/40166/40540)
    40501, // Service busy — retry after 10 seconds
    40540, // Service error processing request
    40613, // Database not currently available
    42108, // SQL pool paused (Synapse)
    42109, // SQL pool warming up (Synapse)
    49918, // Not enough resources to process request
    49919, // Too many create/update operations in progress
    49920, // Too many operations in progress for subscription
];

impl Error {
    /// Create a `SqlServerError` from a single `SqlErrorInfo`.
    pub fn from_sql_error(error: SqlErrorInfo) -> Self {
        Error::SqlServerError {
            errors: vec![error],
        }
    }

    /// Create a `SqlServerError` from multiple `SqlErrorInfo`s.
    pub fn from_sql_errors(errors: Vec<SqlErrorInfo>) -> Self {
        Error::SqlServerError { errors }
    }

    /// Whether this error is transient for connection open retry purposes.
    ///
    /// Transient errors warrant retrying the entire connection sequence.
    /// Permanent errors (auth failures, TLS config, protocol violations)
    /// should not be retried.
    ///
    /// For transport-level failures (`Io`, `ConnectionError`, `TimeoutError`,
    /// `ConnectionClosed`), all instances are considered transient.
    ///
    /// For server-reported errors (`SqlServerError`), only specific error
    /// numbers known to be transient trigger a retry. See
    /// [`TRANSIENT_SQL_ERROR_NUMBERS`] for the full list.
    pub(crate) fn is_transient_connect_error(&self) -> bool {
        match self {
            Error::Io(_)
            | Error::ConnectionError(_)
            | Error::TimeoutError(_)
            | Error::ConnectionClosed(_) => true,

            Error::SqlServerError { errors } => errors
                .iter()
                .any(|e| TRANSIENT_SQL_ERROR_NUMBERS.contains(&e.number)),

            _ => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io;

    #[test]
    fn test_timeout_error_type_string() {
        let error = TimeoutErrorType::String("Test timeout".to_string());
        assert_eq!(error.to_string(), "Test timeout");
    }

    #[test]
    fn test_timeout_error_type_elapsed() {
        // Create an Elapsed error by timing out a sleep
        let rt = tokio::runtime::Runtime::new().unwrap();
        let elapsed = rt.block_on(async {
            tokio::time::timeout(std::time::Duration::from_millis(1), async {
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            })
            .await
            .unwrap_err()
        });
        let error = TimeoutErrorType::Elapsed(elapsed);
        assert!(error.to_string().contains("Elapsed"));
    }

    #[test]
    fn test_io_error_conversion() {
        let io_error = io::Error::new(io::ErrorKind::ConnectionRefused, "Connection refused");
        let error = Error::from(io_error);
        match error {
            Error::Io(e) => assert_eq!(e.kind(), io::ErrorKind::ConnectionRefused),
            _ => panic!("Expected IO error"),
        }
    }

    #[test]
    fn test_redirection_error() {
        let error = Error::Redirection {
            host: "example.com".to_string(),
            port: 1433,
        };
        assert!(error.to_string().contains("example.com"));
        assert!(error.to_string().contains("1433"));
    }

    #[test]
    fn test_protocol_error() {
        let error = Error::ProtocolError("Invalid packet".to_string());
        assert_eq!(error.to_string(), "Protocol Error: Invalid packet");
    }

    #[test]
    fn test_timeout_error() {
        let timeout_type = TimeoutErrorType::String("Query timeout".to_string());
        let error = Error::TimeoutError(timeout_type);
        assert!(error.to_string().contains("Query timeout"));
    }

    #[test]
    fn test_operation_cancelled_error() {
        let error = Error::OperationCancelledError("User cancelled".to_string());
        assert!(error.to_string().contains("User cancelled"));
    }

    #[test]
    fn test_sql_server_error_full() {
        let error = Error::from_sql_error(SqlErrorInfo {
            message: "Login failed".to_string(),
            state: 1,
            class: 14,
            number: 18456,
            server_name: Some("SQLSERVER01".to_string()),
            proc_name: Some("sp_login".to_string()),
            line_number: Some(42),
        });
        let err_str = error.to_string();
        assert!(err_str.contains("18456"));
        assert!(err_str.contains("Login failed"));
        assert!(err_str.contains("SQLSERVER01"));
        assert!(err_str.contains("sp_login"));
        assert!(err_str.contains("42"));
    }

    #[test]
    fn test_sql_server_error_with_none_values() {
        let error = Error::from_sql_error(SqlErrorInfo {
            message: "Error occurred".to_string(),
            state: 2,
            class: 16,
            number: 50000,
            server_name: None,
            proc_name: None,
            line_number: None,
        });
        let err_str = error.to_string();
        assert!(err_str.contains("50000"));
        assert!(err_str.contains("Error occurred"));
        assert!(err_str.contains("Unknown"));
    }

    #[test]
    fn test_sql_server_error_multiple() {
        let error = Error::from_sql_errors(vec![
            SqlErrorInfo {
                message: "First error".to_string(),
                state: 1,
                class: 16,
                number: 50000,
                server_name: Some("SRV".to_string()),
                proc_name: None,
                line_number: Some(1),
            },
            SqlErrorInfo {
                message: "Second error".to_string(),
                state: 1,
                class: 16,
                number: 50001,
                server_name: Some("SRV".to_string()),
                proc_name: None,
                line_number: Some(2),
            },
        ]);
        let err_str = error.to_string();
        assert!(err_str.contains("First error"));
        assert!(err_str.contains("Second error"));
        assert!(err_str.contains("50000"));
        assert!(err_str.contains("50001"));
    }

    #[test]
    fn test_usage_error() {
        let error = Error::UsageError("Invalid connection string".to_string());
        assert_eq!(error.to_string(), "Usage Error: Invalid connection string");
    }

    #[test]
    fn test_implementation_error() {
        let error = Error::ImplementationError("Not implemented yet".to_string());
        assert_eq!(
            error.to_string(),
            "Unexpected Implementation Error: Not implemented yet"
        );
    }

    #[test]
    fn test_unimplemented_feature() {
        let error = Error::UnimplementedFeature {
            feature: "Always Encrypted".to_string(),
            context: "Column encryption not supported".to_string(),
        };
        let err_str = error.to_string();
        assert!(err_str.contains("Always Encrypted"));
        assert!(err_str.contains("Column encryption not supported"));
    }

    #[test]
    fn test_type_conversion_error() {
        let error = Error::TypeConversionError("Cannot convert VARCHAR to INT".to_string());
        assert_eq!(
            error.to_string(),
            "Type Conversion Error: Cannot convert VARCHAR to INT"
        );
    }

    #[test]
    fn test_error_debug_format() {
        let error = Error::ProtocolError("Test".to_string());
        let debug_str = format!("{error:?}");
        assert!(debug_str.contains("ProtocolError"));
    }

    // ── Transient error classification tests ──

    #[test]
    fn io_error_is_transient() {
        let err = Error::Io(io::Error::new(io::ErrorKind::ConnectionRefused, "refused"));
        assert!(err.is_transient_connect_error());
    }

    #[test]
    fn connection_error_is_transient() {
        let err = Error::ConnectionError("failed to connect".to_string());
        assert!(err.is_transient_connect_error());
    }

    #[test]
    fn timeout_error_is_transient() {
        let err = Error::TimeoutError(TimeoutErrorType::String("timed out".to_string()));
        assert!(err.is_transient_connect_error());
    }

    #[test]
    fn connection_closed_is_transient() {
        let err = Error::ConnectionClosed("reset by peer".to_string());
        assert!(err.is_transient_connect_error());
    }

    #[test]
    fn permanent_sql_server_error_is_not_transient() {
        // 18456 = Login failed — permanent, should not retry
        let err = Error::from_sql_error(SqlErrorInfo {
            message: "Login failed".to_string(),
            state: 1,
            class: 14,
            number: 18456,
            server_name: None,
            proc_name: None,
            line_number: None,
        });
        assert!(!err.is_transient_connect_error());
    }

    #[test]
    fn transient_sql_server_errors_are_retried() {
        for &code in &[
            233, 4060, 4221, 10928, 10929, 40143, 40197, 40501, 40540, 40613, 42108, 42109, 49918,
            49919, 49920,
        ] {
            let err = Error::from_sql_error(SqlErrorInfo {
                message: "transient".to_string(),
                state: 1,
                class: 16,
                number: code,
                server_name: None,
                proc_name: None,
                line_number: None,
            });
            assert!(
                err.is_transient_connect_error(),
                "SQL error {code} should be transient"
            );
        }
    }

    #[test]
    fn mixed_sql_errors_transient_if_any_match() {
        // If any error in the batch is transient, the whole error is transient
        let err = Error::from_sql_errors(vec![
            SqlErrorInfo {
                message: "Login failed".to_string(),
                state: 1,
                class: 14,
                number: 18456,
                server_name: None,
                proc_name: None,
                line_number: None,
            },
            SqlErrorInfo {
                message: "Service busy".to_string(),
                state: 1,
                class: 16,
                number: 40501,
                server_name: None,
                proc_name: None,
                line_number: None,
            },
        ]);
        assert!(err.is_transient_connect_error());
    }

    #[test]
    fn protocol_error_is_not_transient() {
        let err = Error::ProtocolError("bad packet".to_string());
        assert!(!err.is_transient_connect_error());
    }

    #[test]
    fn operation_cancelled_is_not_transient() {
        let err = Error::OperationCancelledError("cancelled".to_string());
        assert!(!err.is_transient_connect_error());
    }

    #[test]
    fn usage_error_is_not_transient() {
        let err = Error::UsageError("bad param".to_string());
        assert!(!err.is_transient_connect_error());
    }

    #[test]
    fn security_error_is_not_transient() {
        let err = Error::Security(SecurityError::NotSupported("SSPI".to_string()));
        assert!(!err.is_transient_connect_error());
    }
}
