// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Implementation of SQLFetch.
//!
//! ODBC defines `SQLFetch` as the `SQL_FETCH_NEXT` form of `SQLFetchScroll`,
//! including the part applications rely on most: it fills the buffers bound by
//! `SQLBindCol`. It therefore delegates rather than keeping a second row-reading
//! path, so the classic `SQLBindCol` + `SQLFetch` loop and the block
//! `SQLFetchScroll` loop cannot drift apart.
//!
//! That delegation also carries the rowset behaviour: `SQL_ATTR_ROW_ARRAY_SIZE`
//! applies to `SQLFetch` too, and `*rows_fetched_ptr` and the row status array
//! are written the same way.

use tracing::{debug, error};

use crate::api::fetch_scroll::sql_fetch_scroll_impl;
use crate::api::odbc_types::{SQL_FETCH_NEXT, SqlHandle, SqlReturn};

/// Implements SQLFetch for the current forward-only result set.
///
/// # Safety
/// `statement_handle` must be a valid `StmtHandle` or null.
/// Every active bound-column data buffer must be writable for the configured
/// rowset according to its C type and `BufferLength`; its indicator and
/// octet-length arrays must each be writable for `SQL_ATTR_ROW_ARRAY_SIZE`
/// `SqlLen` values. `SQL_ATTR_ROWS_FETCHED_PTR` must be writable for one
/// `SqlULen`, `SQL_ATTR_ROW_STATUS_PTR` for `SQL_ATTR_ROW_ARRAY_SIZE`
/// `SqlUSmallInt` values, and `SQL_ATTR_ROW_BIND_OFFSET_PTR` must be readable
/// for one `SqlULen`, whenever those attributes are non-null.
pub(crate) unsafe fn sql_fetch(statement_handle: SqlHandle) -> SqlReturn {
    debug!(?statement_handle, "SQLFetch called");
    crate::ffi_entry!("SQLFetch", unsafe {
        let rc = sql_fetch_scroll_impl(statement_handle, SQL_FETCH_NEXT, 0);
        if rc == crate::api::odbc_types::SQL_ERROR {
            error!("SQLFetch: fetch failed");
        }
        rc
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::odbc_types::{SQL_ERROR, SQL_INVALID_HANDLE, SQL_NO_DATA, SQL_NULL_HANDLE};
    use crate::api::sqlstate::SQLSTATE_HY000;
    use crate::handles::dbc::DbcHandle;
    use crate::handles::stmt::STMT_STATE_CURSOR_OPEN;
    use crate::handles::{StmtHandle, handle_from_raw};
    use crate::test_support::TestHandles;

    #[test]
    fn fetch_null_handle() {
        let ret = unsafe { sql_fetch(SQL_NULL_HANDLE) };
        assert_eq!(ret, SQL_INVALID_HANDLE);
    }

    #[test]
    fn fetch_without_open_cursor_returns_error() {
        let h = TestHandles::with_env_dbc_stmt();
        let ret = unsafe { sql_fetch(h.stmt) };
        assert_eq!(ret, SQL_ERROR);
    }

    #[test]
    fn fetch_busy_with_other_statement_returns_hy000() {
        let mut h = TestHandles::with_env_dbc_stmt();
        let other_stmt = h.alloc_extra_stmt();

        let stmt_handle = unsafe { handle_from_raw::<StmtHandle>(h.stmt) };
        {
            let mut stmt_state = stmt_handle.inner.lock().unwrap();
            stmt_state.set_state(STMT_STATE_CURSOR_OPEN);
        }

        let dbc_handle = unsafe { handle_from_raw::<DbcHandle>(h.dbc) };
        {
            let mut dbc_state = dbc_handle.inner.lock().unwrap();
            dbc_state.active_stmt = Some(other_stmt);
        }

        let ret = unsafe { sql_fetch(h.stmt) };
        assert_eq!(ret, SQL_ERROR);

        let stmt_state = stmt_handle.inner.lock().unwrap();
        assert_eq!(stmt_state.diag_records.len(), 1);
        assert_eq!(stmt_state.diag_records[0].sql_state, SQLSTATE_HY000);
        assert_eq!(
            stmt_state.diag_records[0].message,
            "[Microsoft][ODBC Driver 18 for SQL Server]Connection is busy with results for another command"
        );
        drop(stmt_state);

        let dbc_state = dbc_handle.inner.lock().unwrap();
        assert_eq!(dbc_state.active_stmt, Some(other_stmt));
    }

    /// CURSOR_OPEN is set but `active_stmt` is `None` — i.e. a previous fetch
    /// already drained the result set and cleared connection ownership, but
    /// the cursor hasn't been explicitly closed yet. Subsequent fetches must
    /// return `SQL_NO_DATA`, not an error.
    #[test]
    fn fetch_after_cursor_drained_returns_no_data() {
        let h = TestHandles::with_env_dbc_stmt();

        let stmt_handle = unsafe { handle_from_raw::<StmtHandle>(h.stmt) };
        {
            let mut stmt_state = stmt_handle.inner.lock().unwrap();
            stmt_state.set_state(STMT_STATE_CURSOR_OPEN);
        }
        // Leave dbc.active_stmt as None and dbc.client as None — this mirrors
        // the post-drain state that fetch_rows_next produces on Ok(None).

        let ret = unsafe { sql_fetch(h.stmt) };
        assert_eq!(ret, SQL_NO_DATA);

        // No diagnostic should be posted on the drained-cursor path.
        let stmt_state = stmt_handle.inner.lock().unwrap();
        assert!(stmt_state.diag_records.is_empty());
        assert!(stmt_state.has_state(STMT_STATE_CURSOR_OPEN));
    }

    /// Positioned on a no-row statement result (zero columns) with the
    /// connection busy on this statement: SQLFetch returns SQL_ERROR with
    /// SQLSTATE 24000, and the client is restored so the cursor can still be
    /// advanced with SQLMoreResults.
    #[test]
    fn fetch_norow_result_returns_24000() {
        use crate::api::sqlstate::SQLSTATE_24000;
        use mssql_tds::test_client_support::{done_no_more, tds_client_from_tokens};

        let h = TestHandles::with_env_dbc_stmt();
        let stmt_handle = unsafe { handle_from_raw::<StmtHandle>(h.stmt) };
        {
            let mut stmt_state = stmt_handle.inner.lock().unwrap();
            stmt_state.set_state(STMT_STATE_CURSOR_OPEN);
            // column_metadata left empty => no-row (0-column) result.
        }

        // A client must be present (the guard runs after it is claimed), but it
        // is never read because the guard returns first.
        let client = tds_client_from_tokens(vec![done_no_more()]);
        let dbc_handle = unsafe { handle_from_raw::<DbcHandle>(h.dbc) };
        {
            let mut dbc_state = dbc_handle.inner.lock().unwrap();
            dbc_state.client = Some(client);
            dbc_state.active_stmt = Some(h.stmt);
        }

        let ret = unsafe { sql_fetch(h.stmt) };
        assert_eq!(ret, SQL_ERROR);

        let stmt_state = stmt_handle.inner.lock().unwrap();
        assert_eq!(stmt_state.diag_records.len(), 1);
        assert_eq!(stmt_state.diag_records[0].sql_state, SQLSTATE_24000);
        drop(stmt_state);

        // The client is restored and the connection stays busy on this statement.
        let dbc_state = dbc_handle.inner.lock().unwrap();
        assert!(dbc_state.client.is_some());
        assert_eq!(dbc_state.active_stmt, Some(h.stmt));
    }
}
