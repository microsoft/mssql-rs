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

/// Returns the number of parameter markers in a prepared statement.
///
/// The count is the one produced when the statement was prepared and rewritten
/// to `@P1..@Pn`, so `{call proc(?,?,?)}` reports 3 and `{? = call proc(?)}`
/// reports 2 — the return-status marker counts, matching msodbcsql.
///
/// ODBC requires the statement to be prepared; anything else is `HY010`
/// (function sequence error).
///
/// # Safety
/// - `statement_handle` must be a valid STMT handle from `SQLAllocHandle`.
/// - `parameter_count_ptr`, when non-null, must be writable for one
///   `SQLSMALLINT`.
pub(crate) unsafe fn sql_num_params(
    statement_handle: SqlHandle,
    parameter_count_ptr: *mut SqlSmallInt,
) -> SqlReturn {
    debug!(?statement_handle, "SQLNumParams called");

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

    let Ok(mut state) = stmt.inner.lock() else {
        error!("SQLNumParams: stmt mutex poisoned");
        return SQL_ERROR;
    };
    free_errors(&mut state);

    let Some(plan) = state.prepared.as_ref() else {
        error!("SQLNumParams: statement is not prepared");
        post_diag(&mut state, ERR_FUNCTION_SEQUENCE);
        return SQL_ERROR;
    };

    let count = SqlSmallInt::try_from(plan.marker_count).unwrap_or(SqlSmallInt::MAX);
    unsafe { write_if_some(parameter_count_ptr, count) };
    SQL_SUCCESS
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::odbc_types::SQL_NULL_HANDLE;
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
}
