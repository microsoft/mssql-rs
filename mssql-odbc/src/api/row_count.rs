// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Implementation of SQLRowCount.

use tracing::{debug, error};

use crate::api::odbc_types::{
    SQL_ERROR, SQL_INVALID_HANDLE, SQL_SUCCESS, SqlHandle, SqlLen, SqlReturn,
};
use crate::api::util::write_if_some;
use crate::error::free_errors;
use crate::handles::{HandleType, StmtHandle, handle_from_raw};

/// Returns the number of rows affected by the last INSERT, UPDATE, or DELETE.
///
/// The value is `-1` (`SQL_NO_ROWCOUNT_TOTAL`) when no count is available: a
/// forward-only SELECT, a DDL statement, `SET NOCOUNT ON`, or when no statement
/// has been executed yet.
///
/// # Safety
/// `statement_handle` must be a valid STMT handle or null.
/// `row_count_ptr`, when non-null, must be writable for one `SqlLen`.
pub(crate) unsafe fn sql_row_count(
    statement_handle: SqlHandle,
    row_count_ptr: *mut SqlLen,
) -> SqlReturn {
    debug!(?statement_handle, ?row_count_ptr, "SQLRowCount called");

    crate::ffi_entry!("SQLRowCount", unsafe {
        sql_row_count_impl(statement_handle, row_count_ptr)
    })
}

unsafe fn sql_row_count_impl(statement_handle: SqlHandle, row_count_ptr: *mut SqlLen) -> SqlReturn {
    // msodbcsql relies on the Driver Manager to reject a null handle before the
    // driver is called and does not null-check it. We validate anyway to keep
    // the FFI boundary defensive, returning SQL_INVALID_HANDLE.
    if statement_handle.is_null() {
        error!("SQLRowCount: statement_handle is null");
        return SQL_INVALID_HANDLE;
    }

    let stmt = unsafe { handle_from_raw::<StmtHandle>(statement_handle) };
    debug_assert_eq!(
        stmt.object_type,
        HandleType::Stmt,
        "SQLRowCount: handle is not a STMT"
    );
    sql_row_count_safe(stmt, row_count_ptr)
}

fn sql_row_count_safe(stmt: &StmtHandle, row_count_ptr: *mut SqlLen) -> SqlReturn {
    let Ok(mut stmt_state) = stmt.inner.lock() else {
        error!("SQLRowCount: stmt mutex poisoned");
        return SQL_ERROR;
    };

    free_errors(&mut stmt_state);

    // Unlike SQLNumResultCols, msodbcsql does NOT raise a function-sequence
    // error when called before an execute: it simply reports the current
    // row-count field, which defaults to -1. We mirror that and always succeed.
    // The keyset/static-cursor recompute branch (RS_COUNT/RS_SELECTION calling
    // GetRowInfo) is intentionally omitted — those cursor types are unsupported.
    let row_count = SqlLen::try_from(stmt_state.row_count).unwrap_or(SqlLen::MAX);
    unsafe { write_if_some(row_count_ptr, row_count) };

    SQL_SUCCESS
}

#[cfg(test)]
mod tests {
    use std::ptr;

    use super::*;
    use crate::handles::handle_from_raw;
    use crate::test_support::TestHandles;

    #[test]
    fn null_handle_returns_invalid_handle() {
        let mut count: SqlLen = 0;
        let rc = unsafe { sql_row_count(ptr::null_mut(), &mut count) };
        assert_eq!(rc, SQL_INVALID_HANDLE);
    }

    #[test]
    fn null_out_ptr_is_tolerated() {
        let h = TestHandles::with_env_dbc_stmt();
        let rc = unsafe { sql_row_count(h.stmt, ptr::null_mut()) };
        assert_eq!(rc, SQL_SUCCESS);
    }

    #[test]
    fn fresh_stmt_reports_no_rowcount() {
        let h = TestHandles::with_env_dbc_stmt();

        let mut count: SqlLen = 12345;
        let rc = unsafe { sql_row_count(h.stmt, &mut count) };
        assert_eq!(rc, SQL_SUCCESS);
        // No execute has run: default is -1 (SQL_NO_ROWCOUNT_TOTAL).
        assert_eq!(count, -1);
    }

    #[test]
    fn reports_stored_dml_count() {
        let h = TestHandles::with_env_dbc_stmt();

        let stmt_handle = unsafe { handle_from_raw::<StmtHandle>(h.stmt) };
        stmt_handle.inner.lock().unwrap().row_count = 7;

        let mut count: SqlLen = -1;
        let rc = unsafe { sql_row_count(h.stmt, &mut count) };
        assert_eq!(rc, SQL_SUCCESS);
        assert_eq!(count, 7);
    }

    #[test]
    fn poisoned_mutex_returns_error() {
        let h = TestHandles::with_env_dbc_stmt();
        let stmt_handle = unsafe { handle_from_raw::<StmtHandle>(h.stmt) };

        // Poison the stmt mutex by panicking while it is held.
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _guard = stmt_handle.inner.lock().unwrap();
            panic!("poison the stmt lock");
        }));

        let mut count: SqlLen = 0;
        let rc = unsafe { sql_row_count(h.stmt, &mut count) };
        assert_eq!(rc, SQL_ERROR);
    }
}
