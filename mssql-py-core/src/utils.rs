// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

// Utility functions for the PyO3 bindings

use mssql_tds::error::{BulkCopyError, Error as TdsError};
use pyo3::prelude::*;

/// Convert error types from mssql-tds to Python exceptions
/// This will be used when implementing actual Core TDS connection logic
#[allow(dead_code)]
pub fn convert_error(error: anyhow::Error) -> PyErr {
    pyo3::exceptions::PyRuntimeError::new_err(error.to_string())
}

/// Convert TDS errors to appropriate Python exceptions
pub fn convert_tds_error(error: TdsError) -> PyErr {
    match error {
        TdsError::UsageError(msg) => {
            // UsageError includes client-side validation failures like:
            // - NULL to non-nullable column conversion
            // - Invalid string-to-int parsing
            // - Out of range values
            pyo3::exceptions::PyValueError::new_err(msg)
        }
        TdsError::TypeConversionError(msg) => {
            // Type conversion errors (e.g., decimal parsing, type mismatches)
            pyo3::exceptions::PyValueError::new_err(msg)
        }
        TdsError::ProtocolError(msg) => {
            pyo3::exceptions::PyRuntimeError::new_err(format!("Protocol Error: {}", msg))
        }
        TdsError::BulkCopyError(bc_err) => {
            // Handle bulk copy specific errors with appropriate Python exceptions
            match bc_err {
                BulkCopyError::Timeout(timeout_err) => {
                    // Bulk copy operation timed out - raise TimeoutError
                    pyo3::exceptions::PyTimeoutError::new_err(format!(
                        "Bulk copy operation timed out after {} seconds ({} rows copied){}",
                        timeout_err.timeout_seconds,
                        timeout_err.rows_copied,
                        timeout_err
                            .context
                            .as_ref()
                            .map(|c| format!(". {}", c))
                            .unwrap_or_default()
                    ))
                }
                BulkCopyError::AttentionTimeout(attn_err) => {
                    // Attention ACK not received - connection is broken
                    let msg = if attn_err.connection_broken {
                        "Attention acknowledgment not received within 5 seconds. Connection has been broken."
                    } else {
                        "Attention acknowledgment not received within 5 seconds."
                    };
                    pyo3::exceptions::PyTimeoutError::new_err(msg)
                }
                BulkCopyError::ConnectionBroken(msg) => pyo3::exceptions::PyRuntimeError::new_err(
                    format!("Connection is broken: {}", msg),
                ),
            }
        }
        _ => pyo3::exceptions::PyRuntimeError::new_err(error.to_string()),
    }
}
