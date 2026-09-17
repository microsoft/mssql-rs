// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Implementation of SQLNumParams — how many parameters a statement has.

use tracing::{debug, error};

use crate::api::odbc_types::{
    SQL_ERROR, SQL_INVALID_HANDLE, SQL_SUCCESS, SqlHandle, SqlReturn, SqlSmallInt,
};
use crate::api::sqlstate::{ERR_FUNCTION_SEQUENCE, post_diag};
use crate::api::util::write_if_some;
use crate::error::free_errors;
use crate::handles::{HandleType, StmtHandle, handle_from_raw};

/// Returns the number of parameter markers in prepared or directly executed SQL.
///
/// The count is the one produced when the SQL was translated and rewritten
/// to `@P1..@Pn`, so `{call proc(?,?,?)}` reports 3 and `{? = call proc(?)}`
/// reports 2 — the return-status marker counts, matching msodbcsql.
///
/// A statement with neither prepared nor accepted direct SQL reports `HY010`.
/// Closing a cursor or resetting parameter bindings does not discard its SQL.
///
/// # Safety
/// - `statement_handle` must be a valid STMT handle from `SQLAllocHandle`.
/// - `parameter_count_ptr`, when non-null, must be writable for one
///   `SQLSMALLINT`.
pub(crate) unsafe fn sql_num_params(
    statement_handle: SqlHandle,
    parameter_count_ptr: *mut SqlSmallInt,
) -> SqlReturn {
    debug!(
        ?statement_handle,
        ?parameter_count_ptr,
        "SQLNumParams called"
    );

    crate::ffi_entry!("SQLNumParams", unsafe {
        sql_num_params_impl(statement_handle, parameter_count_ptr)
    })
}

/// # Safety
/// The handle and output buffer must satisfy [`sql_num_params`].
unsafe fn sql_num_params_impl(
    statement_handle: SqlHandle,
    parameter_count_ptr: *mut SqlSmallInt,
) -> SqlReturn {
    if statement_handle.is_null() {
        error!("SQLNumParams: statement_handle is null");
        return SQL_INVALID_HANDLE;
    }
    let stmt = unsafe { handle_from_raw::<StmtHandle>(statement_handle) };
    debug_assert_eq!(
        stmt.object_type,
        HandleType::Stmt,
        "SQLNumParams: handle is not a STMT"
    );

    sql_num_params_safe(stmt, parameter_count_ptr)
}

fn sql_num_params_safe(stmt: &StmtHandle, parameter_count_ptr: *mut SqlSmallInt) -> SqlReturn {
    let Ok(mut state) = stmt.inner.lock() else {
        error!("SQLNumParams: stmt mutex poisoned");
        return SQL_ERROR;
    };
    free_errors(&mut state);

    let Some(marker_count) = state
        .prepared
        .as_ref()
        .map(|plan| plan.marker_count)
        .or(state.direct_marker_count)
    else {
        error!("SQLNumParams: statement has no SQL");
        post_diag(&mut state, ERR_FUNCTION_SEQUENCE);
        return SQL_ERROR;
    };

    let count = SqlSmallInt::try_from(marker_count).unwrap_or(SqlSmallInt::MAX);
    unsafe { write_if_some(parameter_count_ptr, count) };
    SQL_SUCCESS
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::bind_param::{sql_bind_parameter, sql_free_stmt_reset_params};
    use crate::api::close_cursor::sql_free_stmt_close;
    use crate::api::exec_direct::sql_exec_direct_w;
    use crate::api::odbc_types::*;
    use crate::api::prepare::sql_prepare_w;
    use crate::api::sqlstate::SQLSTATE_HY010;
    use crate::test_support::TestHandles;

    #[test]
    fn null_handle_returns_invalid_handle() {
        let mut n: SqlSmallInt = -1;
        assert_eq!(
            unsafe { sql_num_params(SQL_NULL_HANDLE, &mut n) },
            SQL_INVALID_HANDLE
        );
    }

    /// ODBC: SQLNumParams on an unprepared statement is a sequence error.
    #[test]
    fn unprepared_statement_posts_hy010() {
        let h = TestHandles::with_env_dbc_stmt();
        let mut n: SqlSmallInt = -1;
        assert_eq!(unsafe { sql_num_params(h.stmt, &mut n) }, SQL_ERROR);
        assert_eq!(n, -1, "the out parameter must be left alone on error");
        let stmt = unsafe { handle_from_raw::<StmtHandle>(h.stmt) };
        assert_eq!(
            stmt.inner.lock().unwrap().diag_records[0].sql_state,
            SQLSTATE_HY010
        );
    }

    /// A null out pointer is legal — the call just validates state.
    #[test]
    fn null_out_pointer_is_tolerated() {
        let h = TestHandles::with_env_dbc_stmt();
        assert_eq!(
            unsafe { sql_num_params(h.stmt, std::ptr::null_mut()) },
            SQL_ERROR,
            "still a sequence error when unprepared"
        );
    }

    fn wide(sql: &str) -> Vec<u16> {
        sql.encode_utf16().chain(std::iter::once(0)).collect()
    }

    fn assert_count(h: &TestHandles, expected: SqlSmallInt) {
        let mut count = -1;
        assert_eq!(unsafe { sql_num_params(h.stmt, &mut count) }, SQL_SUCCESS);
        assert_eq!(count, expected);
    }

    fn bind_integer(h: &TestHandles, value: &mut i32) {
        assert_eq!(
            unsafe {
                sql_bind_parameter(
                    h.stmt,
                    1,
                    SQL_PARAM_INPUT,
                    SQL_C_SLONG,
                    SQL_INTEGER,
                    10,
                    0,
                    std::ptr::from_mut(value).cast(),
                    0,
                    std::ptr::null_mut(),
                )
            },
            SQL_SUCCESS
        );
    }

    #[test]
    fn prepared_markers_include_call_return_and_ignore_quoted_markers() {
        let h = TestHandles::with_env_dbc_stmt();
        h.mark_dbc_connected();
        for (sql, expected) in [
            ("SELECT '?', ? /* ? */", 1),
            ("{? = call dbo.proc(?, ?)}", 3),
            ("SELECT 1", 0),
        ] {
            assert_eq!(
                unsafe { sql_prepare_w(h.stmt, wide(sql).as_ptr(), SQL_NTS) },
                SQL_SUCCESS
            );
            assert_count(&h, expected);
            assert_eq!(
                unsafe { sql_num_params(h.stmt, std::ptr::null_mut()) },
                SQL_SUCCESS
            );
        }
    }

    #[test]
    fn direct_count_survives_connection_failure_close_and_parameter_reset() {
        let h = TestHandles::with_env_dbc_stmt();
        let mut value = 7;
        bind_integer(&h, &mut value);
        assert_eq!(
            unsafe { sql_exec_direct_w(h.stmt, wide("SELECT ? /* ? */").as_ptr(), SQL_NTS) },
            SQL_ERROR
        );
        assert_count(&h, 1);
        let stmt = unsafe { handle_from_raw::<StmtHandle>(h.stmt) };
        assert!(stmt.inner.lock().unwrap().diag_records.is_empty());
        assert!(stmt.inner.lock().unwrap().prepared.is_none());
        assert_eq!(unsafe { sql_free_stmt_close(h.stmt) }, SQL_SUCCESS);
        assert_count(&h, 1);
        assert_eq!(unsafe { sql_free_stmt_reset_params(h.stmt) }, SQL_SUCCESS);
        assert_count(&h, 1);
        assert_eq!(
            unsafe { sql_exec_direct_w(h.stmt, wide("SELECT 1").as_ptr(), SQL_NTS) },
            SQL_ERROR
        );
        assert_count(&h, 0);
    }

    #[test]
    fn prepare_and_direct_replace_each_others_counts() {
        let h = TestHandles::with_env_dbc_stmt();
        h.mark_dbc_connected();
        assert_eq!(
            unsafe { sql_prepare_w(h.stmt, wide("SELECT ?, ?").as_ptr(), SQL_NTS) },
            SQL_SUCCESS
        );
        assert_count(&h, 2);
        assert_eq!(
            unsafe { sql_exec_direct_w(h.stmt, wide("SELECT 1").as_ptr(), SQL_NTS) },
            SQL_ERROR
        );
        assert_count(&h, 0);
        assert_eq!(
            unsafe { sql_prepare_w(h.stmt, wide("SELECT ?").as_ptr(), SQL_NTS) },
            SQL_SUCCESS
        );
        assert_count(&h, 1);
        let stmt = unsafe { handle_from_raw::<StmtHandle>(h.stmt) };
        assert_eq!(stmt.inner.lock().unwrap().direct_marker_count, None);
    }

    #[test]
    fn rejected_sql_preserves_the_previous_count() {
        let h = TestHandles::with_env_dbc_stmt();
        h.mark_dbc_connected();
        assert_eq!(
            unsafe { sql_exec_direct_w(h.stmt, wide("SELECT 1").as_ptr(), SQL_NTS) },
            SQL_ERROR
        );
        for sql in ["SELECT {bogus 1}", "SELECT ?"] {
            assert_eq!(
                unsafe { sql_exec_direct_w(h.stmt, wide(sql).as_ptr(), SQL_NTS) },
                SQL_ERROR
            );
            assert_count(&h, 0);
        }
        assert_eq!(
            unsafe { sql_prepare_w(h.stmt, wide("SELECT {bogus 1}").as_ptr(), SQL_NTS) },
            SQL_ERROR
        );
        assert_count(&h, 0);
    }
}
