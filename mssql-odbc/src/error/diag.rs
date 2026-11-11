// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Diagnostic records posted on a handle.
//!
//! Each entry represents one error or warning the driver returned to the DM;
//! `SQLGetDiagRec` reads them back by index.

use std::borrow::Cow;
use std::ops::{Deref, DerefMut};
use std::sync::MutexGuard;

use crate::api::odbc_types::{SQL_NO_ROW_NUMBER, SqlLen};

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
    fn diag_records_mut(&mut self) -> &mut DiagRecords;
}

// If T implements HasDiagnostics, then MutexGuard<T> does too by delegation.
// This allows helper calls like free_errors(&mut state) where state is a MutexGuard.
impl<T: HasDiagnostics + ?Sized> HasDiagnostics for MutexGuard<'_, T> {
    fn diag_records(&self) -> &[DiagRecord] {
        self.deref().diag_records()
    }

    fn diag_records_mut(&mut self) -> &mut DiagRecords {
        self.deref_mut().diag_records_mut()
    }
}

/// A handle's diagnostic records, in posting order until a record read sorts
/// them. Mutable access can add or reorder records, so it marks the list for
/// sorting again, and a run of record reads sorts it once.
#[derive(Debug, Default)]
pub(crate) struct DiagRecords {
    records: Vec<DiagRecord>,
    sorted: bool,
}

impl Deref for DiagRecords {
    type Target = Vec<DiagRecord>;

    fn deref(&self) -> &Vec<DiagRecord> {
        &self.records
    }
}

impl DerefMut for DiagRecords {
    fn deref_mut(&mut self) -> &mut Vec<DiagRecord> {
        self.sorted = false;
        &mut self.records
    }
}

#[derive(Debug, Clone)]
pub(crate) struct DiagRecord {
    pub(crate) sql_state: SqlState,
    pub(crate) native_error: i32,
    /// Internal row key used by msodbcsql's diagnostic comparator: the 1-based
    /// parameter set of a server message from a parameter array, otherwise
    /// `SQL_NO_ROW_NUMBER`. `SQL_DIAG_ROW_NUMBER` does not report it.
    pub(crate) row_number: SqlLen,
    /// msodbcsql's internal message-resource ID (or the matching server
    /// fallback tier). Equal keys retain posting order.
    pub(crate) priority: u16,
    /// Borrowed for driver-raised `DiagMsg` text, so posting a fixed
    /// diagnostic (e.g. the `01004` of a `SQLGetData` length probe) does
    /// not allocate.
    pub(crate) message: Cow<'static, str>,
}

impl DiagRecord {
    pub(crate) fn new(
        sql_state: SqlState,
        native_error: i32,
        message: impl Into<Cow<'static, str>>,
    ) -> Self {
        Self::with_sort_key(sql_state, native_error, SQL_NO_ROW_NUMBER, 0, message)
    }

    pub(crate) fn with_sort_key(
        sql_state: SqlState,
        native_error: i32,
        row_number: SqlLen,
        priority: u16,
        message: impl Into<Cow<'static, str>>,
    ) -> Self {
        Self {
            sql_state,
            native_error,
            row_number,
            priority,
            message: message.into(),
        }
    }
}

/// Sorts records into msodbcsql's stable `(row number, resource ID, posting
/// ordinal)` order, unless nothing changed since the last sort.
pub(crate) fn sort_diagnostics<T: HasDiagnostics + ?Sized>(state: &mut T) {
    let list = state.diag_records_mut();
    if !list.sorted {
        list.records
            .sort_by_key(|record| (record.row_number, record.priority));
        list.sorted = true;
    }
}

/// Post an ODBC diagnostic record onto a handle-local diagnostic list.
///
/// The message is prefixed with [`DRIVER_DIAG_PREFIX`] so every record the
/// driver surfaces via `SQLGetDiagRec` matches msodbcsql's format. Callers
/// pass the bare message and must not include the `[Microsoft]...` prefix
/// themselves. Server-raised text goes through `post_server_message` in
/// `sqlstate.rs` instead.
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

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Default)]
    struct State(DiagRecords);

    impl HasDiagnostics for State {
        fn diag_records(&self) -> &[DiagRecord] {
            &self.0
        }

        fn diag_records_mut(&mut self) -> &mut DiagRecords {
            &mut self.0
        }
    }

    fn post(state: &mut State, native_error: i32, priority: u16) {
        state.0.push(DiagRecord::with_sort_key(
            *b"01000",
            native_error,
            SQL_NO_ROW_NUMBER,
            priority,
            "message",
        ));
    }

    fn natives(state: &State) -> Vec<i32> {
        state.0.iter().map(|record| record.native_error).collect()
    }

    #[test]
    fn records_are_sorted_once_per_change() {
        let mut state = State::default();
        post(&mut state, 1, 40401);
        post(&mut state, 2, 0);
        sort_diagnostics(&mut state);
        assert_eq!(natives(&state), [2, 1]);
        assert!(state.0.sorted);
        assert_eq!(state.diag_records().len(), 2);
        assert!(state.0.sorted, "reading records keeps them sorted");

        post(&mut state, 3, 0);
        assert!(!state.0.sorted);
        sort_diagnostics(&mut state);
        assert_eq!(natives(&state), [2, 3, 1]);
    }
}
