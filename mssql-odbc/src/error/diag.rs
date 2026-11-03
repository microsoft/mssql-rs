// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Diagnostic records posted on a handle.
//!
//! Each entry represents one error or warning the driver returned to the DM;
//! `SQLGetDiagRec` reads them back by index.

use std::borrow::Cow;
use std::ops::{Deref, DerefMut};
use std::sync::MutexGuard;

/// SQLSTATE stored as 5 ASCII bytes (no NUL).
pub(crate) type SqlState = [u8; 5];

/// [`DRIVER_DIAG_PREFIX`] as a literal, so `concat!` can build each
/// `DiagMsg`'s prefixed text at compile time.
macro_rules! driver_diag_prefix {
    () => {
        "[Microsoft][ODBC Driver 18 for SQL Server]"
    };
}
pub(crate) use driver_diag_prefix;

/// Prefix every diagnostic message carries, matching msodbcsql's format:
/// `[Microsoft][ODBC Driver 18 for SQL Server]`. Errors raised inside the
/// SQL Server engine add a further `[SQL Server]` sub-source after this (see
/// `post_tds_error`). Consumers such as mssql-python and pyodbc rely on this
/// prefix being present to identify and reformat driver diagnostics.
pub(crate) const DRIVER_DIAG_PREFIX: &str = driver_diag_prefix!();

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
    /// Borrowed for driver-raised `DiagMsg` text, so posting a fixed
    /// diagnostic (e.g. the `01004` of every `SQLGetData` length probe) does
    /// not allocate.
    pub(crate) message: Cow<'static, str>,
}

impl DiagRecord {
    pub(crate) fn new(
        sql_state: SqlState,
        native_error: i32,
        message: impl Into<Cow<'static, str>>,
    ) -> Self {
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
    message: impl AsRef<str>,
) {
    post_prefixed(state, sql_state, native_error, &[message.as_ref()]);
}

/// Pushes one record whose text is [`DRIVER_DIAG_PREFIX`] followed by `parts`,
/// built in a single exactly-sized allocation.
pub(crate) fn post_prefixed(
    state: &mut impl HasDiagnostics,
    sql_state: SqlState,
    native_error: i32,
    parts: &[&str],
) {
    let len = DRIVER_DIAG_PREFIX.len() + parts.iter().map(|p| p.len()).sum::<usize>();
    let mut message = String::with_capacity(len);
    message.push_str(DRIVER_DIAG_PREFIX);
    for part in parts {
        message.push_str(part);
    }
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
