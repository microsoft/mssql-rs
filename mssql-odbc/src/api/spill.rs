// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Releasing a connection held by an open cursor by buffering its rows.
//!
//! ODBC without MARS allows a single active statement per connection, yet
//! msodbcsql happily serves a second statement while the first still has an
//! open cursor — as long as that cursor's result set already sits in the
//! driver's read buffer. Applications (mssql-python among them) rely on this:
//! they create a second cursor on the same connection while the first is only
//! partially consumed.
//!
//! This module reproduces that behaviour. When a statement finds the
//! connection claimed by another statement, it first asks the holder to spill:
//! the remaining rows of the holder's open result set are read off the wire
//! into memory, and if the batch ends there the connection is released. Rows
//! stay visible to the holder because `SQLFetch` drains the buffer before
//! touching the connection.
//!
//! Spilling is bounded. A result set larger than [`MAX_SPILL_ROWS`] keeps the
//! connection claimed and the caller still gets `HY000` — msodbcsql behaves
//! the same way once a result set outgrows its buffer.

use std::collections::VecDeque;

use mssql_tds::connection::tds_client::{ResultSet, StatementResult};
use tracing::{debug, error};

use crate::api::odbc_types::SqlHandle;
use crate::handles::stmt::STMT_STATE_CURSOR_OPEN;
use crate::handles::{DbcHandle, StmtHandle, handle_from_raw};

/// Upper bound on rows buffered to free a connection. Chosen to keep the
/// worst-case footprint bounded while covering the small result sets that
/// interleaved-cursor applications actually produce.
const MAX_SPILL_ROWS: usize = 20_000;

/// Attempts to release `dbc` from the statement identified by `busy_stmt` by
/// buffering the rest of its open result set.
///
/// Returns `true` only when the connection is now idle. Any partially read
/// rows are always handed to the holder, so a `false` result never loses data.
///
/// # Safety
/// `busy_stmt` is the `active_stmt` recorded on the connection, i.e. a live
/// statement handle allocated by this driver.
pub(crate) fn try_release_connection(dbc: &DbcHandle, busy_stmt: SqlHandle) -> bool {
    if busy_stmt.is_null() {
        return false;
    }
    let other: &StmtHandle = unsafe { handle_from_raw(busy_stmt) };

    // Only an open, row-producing cursor can be spilled. Anything else holding
    // the connection (a no-column result awaiting SQLMoreResults) must keep it.
    let (spillable, already_eof) = match other.inner.lock() {
        Ok(state) => (
            state.has_state(STMT_STATE_CURSOR_OPEN) && !state.column_metadata.is_empty(),
            state.buffered_eof,
        ),
        Err(_) => return false,
    };
    if !spillable {
        return false;
    }

    let mut client = {
        let Ok(mut dbc_state) = dbc.inner.lock() else {
            return false;
        };
        if dbc_state.active_stmt != Some(busy_stmt) {
            return false;
        }
        match dbc_state.client.take() {
            Some(client) => client,
            None => return false,
        }
    };

    let mut rows: VecDeque<Vec<mssql_tds::datatypes::column_values::ColumnValues>> =
        VecDeque::new();
    let mut reached_eof = already_eof;
    let mut failed = false;

    while !reached_eof && rows.len() < MAX_SPILL_ROWS {
        match dbc.runtime.block_on(client.next_row()) {
            Ok(Some(row)) => rows.push_back(row),
            Ok(None) => reached_eof = true,
            Err(e) => {
                error!(%e, "spill: failed reading ahead to release the connection");
                failed = true;
                break;
            }
        }
    }

    // The current result set is drained, but the batch's trailing DONE (and any
    // further result set) is still on the wire. Cross that boundary here so a
    // single-statement batch — the overwhelmingly common case — frees the
    // connection. If another result follows, hand it to the holder as a pending
    // result so its `SQLMoreResults` sees it without re-advancing.
    let mut pending = None;
    if reached_eof && !failed && client.has_open_batch() {
        match dbc.runtime.block_on(client.advance()) {
            Ok(StatementResult::End) => {}
            Ok(other) => pending = Some(other),
            Err(e) => {
                error!(%e, "spill: failed advancing past the current result set");
                failed = true;
            }
        }
    }

    // The batch is only finished when the current result set ended and no
    // further result set follows; otherwise SQLMoreResults still needs the wire.
    let released = reached_eof && !failed && pending.is_none() && !client.has_open_batch();

    if let Ok(mut state) = other.inner.lock() {
        state.buffered_rows.extend(rows);
        state.buffered_eof = reached_eof;
        state.pending_result = pending;
    }

    if let Ok(mut dbc_state) = dbc.inner.lock() {
        dbc_state.client = Some(client);
        if released && dbc_state.active_stmt == Some(busy_stmt) {
            dbc_state.active_stmt = None;
        }
    }

    debug!(released, "spill: read-ahead completed");
    released
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::odbc_types::SQL_NULL_HANDLE;
    use crate::handles::handle_from_raw;
    use crate::test_support::TestHandles;

    #[test]
    fn null_statement_is_not_spillable() {
        let handles = TestHandles::with_env_dbc();
        let dbc = unsafe { handle_from_raw::<DbcHandle>(handles.dbc) };
        assert!(!try_release_connection(dbc, SQL_NULL_HANDLE));
    }

    #[test]
    fn statement_without_open_cursor_is_not_spillable() {
        let handles = TestHandles::with_env_dbc_stmt();
        let dbc = unsafe { handle_from_raw::<DbcHandle>(handles.dbc) };
        assert!(!try_release_connection(dbc, handles.stmt));
    }
}
