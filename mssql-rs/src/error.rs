// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.
// Portions derived from saurabh500/mssql-tiberius-bridge (MIT).

//! Error types for mssql-rs.
//!
//! All fallible operations in this crate return [`Result<T>`], which uses
//! the unified [`Error`] enum. Errors from the underlying `mssql-tds` driver
//! are wrapped in [`Error::Tds`].

use std::fmt;

/// Unified error type for all mssql-rs operations.
///
/// Wraps errors from the underlying `mssql-tds` driver alongside
/// facade-specific errors for column access and type conversion.
#[derive(Debug)]
pub enum Error {
    /// An error from the underlying mssql-tds TDS protocol driver.
    ///
    /// This includes connection failures, authentication errors,
    /// SQL syntax errors, and protocol-level issues.
    Tds(mssql_tds::error::Error),

    /// A column was requested by name but does not exist in the result set.
    ///
    /// Returned by [`Row::try_get()`](crate::Row::try_get) when the column
    /// name doesn't match any column in the row.
    ColumnNotFound(String),

    /// A column was requested by index but the index exceeds the column count.
    ///
    /// Returned by [`Row::try_get()`](crate::Row::try_get) when the numeric
    /// index is out of bounds.
    ColumnIndexOutOfBounds {
        /// The requested column index.
        index: usize,
        /// The actual number of columns in the row.
        count: usize,
    },

    /// A type conversion failed when extracting a column value.
    Conversion(String),

    /// A row cannot be encoded for a compatibility bulk upload.
    BulkInput(String),

    /// A connection pool error occurred.
    Pool(String),

    /// A prepared statement belongs to another client or a reset session.
    ///
    /// Prepare a new statement on the current client before using it.
    InvalidPreparedStatement,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Tds(e) => write!(f, "TDS error: {e}"),
            Error::ColumnNotFound(name) => write!(f, "Column not found: {name}"),
            Error::ColumnIndexOutOfBounds { index, count } => {
                write!(f, "Column index {index} out of bounds (count: {count})")
            }
            Error::Conversion(msg) => write!(f, "Conversion error: {msg}"),
            Error::BulkInput(msg) => write!(f, "BULK UPLOAD input failure: {msg}"),
            Error::Pool(msg) => write!(f, "Pool error: {msg}"),
            Error::InvalidPreparedStatement => {
                write!(
                    f,
                    "Prepared statement belongs to a different or reset client session"
                )
            }
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Error::Tds(e) => Some(e),
            _ => None,
        }
    }
}

impl From<mssql_tds::error::Error> for Error {
    fn from(e: mssql_tds::error::Error) -> Self {
        Error::Tds(e)
    }
}

/// Result type alias using [`Error`] for all mssql-rs operations.
pub type Result<T> = std::result::Result<T, Error>;

#[cfg(test)]
mod tests {
    use super::*;
    use std::error::Error as _;

    #[test]
    fn tds_errors_retain_the_original_source() {
        let original = mssql_tds::error::Error::UsageError("invalid parameter".into());
        let original_message = original.to_string();
        let error = Error::from(original);
        assert_eq!(error.to_string(), format!("TDS error: {original_message}"));
        let source = error.source().expect("TDS error should retain its source");
        assert_eq!(source.to_string(), original_message);
        assert!(matches!(
            source.downcast_ref::<mssql_tds::error::Error>(),
            Some(mssql_tds::error::Error::UsageError(message)) if message == "invalid parameter"
        ));
    }

    #[test]
    fn facade_errors_preserve_context_without_a_source() {
        let cases = [
            (
                Error::ColumnNotFound("missing".into()),
                "Column not found: missing",
            ),
            (
                Error::ColumnIndexOutOfBounds { index: 3, count: 2 },
                "Column index 3 out of bounds (count: 2)",
            ),
            (
                Error::Conversion("invalid date".into()),
                "Conversion error: invalid date",
            ),
            (
                Error::BulkInput("wrong column count".into()),
                "BULK UPLOAD input failure: wrong column count",
            ),
            (Error::Pool("closed".into()), "Pool error: closed"),
            (
                Error::InvalidPreparedStatement,
                "Prepared statement belongs to a different or reset client session",
            ),
        ];
        for (error, expected) in cases {
            assert_eq!(error.to_string(), expected);
            assert!(error.source().is_none());
        }
    }
}
