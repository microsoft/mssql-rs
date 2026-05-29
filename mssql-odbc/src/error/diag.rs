// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Diagnostic records posted on a handle.
//!
//! Equivalent to msodbcsql's `ERRORDATA` linked list hanging off
//! `tagOBJBASE::errinfo`. Each entry represents one error or warning the
//! driver returned to the DM; `SQLGetDiagRec` reads them back by index.

/// SQLSTATE stored as 5 ASCII bytes (no NUL).
pub(crate) type SqlState = [u8; 5];

#[derive(Debug, Clone)]
pub(crate) struct DiagRecord {
    pub(crate) sql_state: SqlState,
    pub(crate) native_error: i32,
    pub(crate) message: String,
}

impl DiagRecord {
    pub(crate) fn new(sql_state: SqlState, native_error: i32, message: impl Into<String>) -> Self {
        Self {
            sql_state,
            native_error,
            message: message.into(),
        }
    }
}
