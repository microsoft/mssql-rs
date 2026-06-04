// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Diagnostic records posted on a handle.
//!
//! Equivalent to msodbcsql's `ERRORDATA` linked list hanging off
//! `tagOBJBASE::errinfo`. Each entry represents one error or warning the
//! driver returned to the DM; `SQLGetDiagRec` reads them back by index.

use std::ops::{Deref, DerefMut};
use std::sync::MutexGuard;

/// SQLSTATE stored as 5 ASCII bytes (no NUL).
pub(crate) type SqlState = [u8; 5];

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
pub(crate) fn post_sql_error(
    state: &mut impl HasDiagnostics,
    sql_state: SqlState,
    native_error: i32,
    message: impl Into<String>,
) {
    state
        .diag_records_mut()
        .push(DiagRecord::new(sql_state, native_error, message));
}

/// FreeErrors equivalent: clear all diagnostics from a handle-local list.
///
/// Call this at the beginning of handle-scoped API entry points, after the
/// handle has been validated and the handle mutex is acquired, before doing
/// new work that may post fresh diagnostics.
pub(crate) fn free_errors(state: &mut impl HasDiagnostics) {
    state.diag_records_mut().clear();
}
