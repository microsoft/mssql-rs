// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Diagnostic records posted on a handle.
//!
//! Each entry represents one error or warning the driver returned to the DM;
//! `SQLGetDiagRec` reads them back by index.

use std::ops::{Deref, DerefMut};
use std::sync::MutexGuard;

/// SQLSTATE stored as 5 ASCII bytes (no NUL).
pub(crate) type SqlState = [u8; 5];

/// Prefix every diagnostic message carries, matching msodbcsql's format:
/// `[Microsoft][ODBC Driver 18 for SQL Server]`. Errors raised inside the
/// SQL Server engine add a further `[SQL Server]` sub-source after this (see
/// `post_tds_error`). Consumers such as mssql-python and pyodbc rely on this
/// prefix being present to identify and reformat driver diagnostics.
pub(crate) const DRIVER_DIAG_PREFIX: &str = "[Microsoft][ODBC Driver 18 for SQL Server]";

/// Implemented by state structs that store ODBC diagnostic records.
pub(crate) trait HasDiagnostics {
    fn diag_records(&self) -> &[DiagRecord];
    fn diag_records_mut(&mut self) -> &mut Vec<DiagRecord>;
}

// If T implements HasDiagnostics, then MutexGuard<T> does too by delegation.
// This allows helper calls like free_errors(&mut state) where state is a MutexGuard.
impl<T: HasDiagnostics + ?Sized> HasDiagnostics for MutexGuard<'_, T> {
    fn diag_records(&self) -> &[DiagRecord] {
        self.deref().diag_records()
    }

    fn diag_records_mut(&mut self) -> &mut Vec<DiagRecord> {
        self.deref_mut().diag_records_mut()
    }
}

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

/// Post an ODBC diagnostic record onto a handle-local diagnostic list.
///
/// The message is prefixed with [`DRIVER_DIAG_PREFIX`] so every record the
/// driver surfaces via `SQLGetDiagRec` matches msodbcsql's format. Callers
/// pass the bare message (server paths first prepend their `[SQL Server]`
/// sub-source); they must not include the `[Microsoft]...` prefix themselves.
pub(crate) fn post_sql_error(
    state: &mut impl HasDiagnostics,
    sql_state: SqlState,
    native_error: i32,
    message: impl Into<String>,
) {
    let message = format!("{DRIVER_DIAG_PREFIX}{}", message.into());
    state
        .diag_records_mut()
        .push(DiagRecord::new(sql_state, native_error, message));
}

/// Clear all diagnostics from a handle-local list.
///
/// Call this at the beginning of handle-scoped API entry points, after the
/// handle has been validated and the handle mutex is acquired, before doing
/// new work that may post fresh diagnostics.
pub(crate) fn free_errors(state: &mut impl HasDiagnostics) {
    state.diag_records_mut().clear();
}
