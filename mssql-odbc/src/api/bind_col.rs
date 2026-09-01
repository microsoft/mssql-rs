// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Implementation of SQLBindCol: the application side of the columnar fetch
//! path.
//!
//! Binding only records where a column's value should land; nothing is written
//! until `SQLFetchScroll` fills the rowset. That separation is why validation
//! here is deliberately shallow — ODBC allows binding before the statement is
//! executed, so there is no column metadata yet to check the ordinal or the
//! source type against. Anything metadata-dependent is reported per row by the
//! fetch instead.

use tracing::{debug, error};

use crate::api::odbc_types::{
    SQL_ERROR, SQL_INVALID_HANDLE, SQL_SUCCESS, SqlHandle, SqlLen, SqlPointer, SqlReturn,
    SqlSmallInt, SqlUSmallInt,
};
use crate::api::sqlstate::{
    ERR_FUNCTION_SEQUENCE, ERR_INVALID_C_DATA_TYPE, ERR_INVALID_DESCRIPTOR_INDEX,
    ERR_INVALID_STRING_OR_BUFFER_LENGTH, post_diag,
};
use crate::api::type_rules::{canonical_c_type, is_valid_c_type};
use crate::error::free_errors;
use crate::handles::stmt::{ColumnBinding, STMT_STATE_FETCH_IN_PROGRESS};
use crate::handles::{HandleType, StmtHandle, handle_from_raw};

/// Implements SQLBindCol.
///
/// # Safety
/// `statement_handle` must be a valid `StmtHandle` or null. The buffers must
/// stay valid until the column is unbound or the statement is freed.
pub(crate) unsafe fn sql_bind_col(
    statement_handle: SqlHandle,
    column_number: SqlUSmallInt,
    target_type: SqlSmallInt,
    target_value_ptr: SqlPointer,
    buffer_length: SqlLen,
    strlen_or_ind_ptr: *mut SqlLen,
) -> SqlReturn {
    debug!(
        ?statement_handle,
        column_number, target_type, buffer_length, "SQLBindCol called"
    );
    crate::ffi_entry!("SQLBindCol", unsafe {
        sql_bind_col_impl(
            statement_handle,
            column_number,
            target_type,
            target_value_ptr,
            buffer_length,
            strlen_or_ind_ptr,
        )
    })
}

unsafe fn sql_bind_col_impl(
    statement_handle: SqlHandle,
    column_number: SqlUSmallInt,
    target_type: SqlSmallInt,
    target_value_ptr: SqlPointer,
    buffer_length: SqlLen,
    strlen_or_ind_ptr: *mut SqlLen,
) -> SqlReturn {
    if statement_handle.is_null() {
        error!("SQLBindCol: statement_handle is null");
        return SQL_INVALID_HANDLE;
    }
    let stmt = unsafe { handle_from_raw::<StmtHandle>(statement_handle) };
    debug_assert_eq!(stmt.object_type, HandleType::Stmt);
    sql_bind_col_safe(
        stmt,
        column_number,
        target_type,
        target_value_ptr,
        buffer_length,
        strlen_or_ind_ptr,
    )
}

fn sql_bind_col_safe(
    stmt: &StmtHandle,
    column_number: SqlUSmallInt,
    target_type: SqlSmallInt,
    target_value_ptr: SqlPointer,
    buffer_length: SqlLen,
    strlen_or_ind_ptr: *mut SqlLen,
) -> SqlReturn {
    let Ok(mut stmt_state) = stmt.inner.lock() else {
        error!("SQLBindCol: stmt mutex poisoned");
        return SQL_ERROR;
    };
    free_errors(&mut stmt_state);

    // A fetch in flight is writing through the buffers it snapshotted, so
    // replacing them now could free one mid-write.
    if stmt_state.has_state(STMT_STATE_FETCH_IN_PROGRESS) {
        error!("SQLBindCol: a fetch is in progress on this statement");
        post_diag(&mut stmt_state, ERR_FUNCTION_SEQUENCE);
        return SQL_ERROR;
    }

    // Column 0 is the bookmark column. Bookmarks need SQL_ATTR_USE_BOOKMARKS,
    // which a forward-only cursor does not offer, so the ordinal is simply out
    // of range here.
    if column_number == 0 {
        error!("SQLBindCol: column 0 is the bookmark column, which is not supported");
        post_diag(&mut stmt_state, ERR_INVALID_DESCRIPTOR_INDEX);
        return SQL_ERROR;
    }

    // A null TargetValuePtr unbinds the column whatever the indicator says.
    // msodbcsql never inspects the indicator here (`sqlcdesc.cpp` UnbindParam)
    // and has no indicator-only binding, so keeping one bound would both consume
    // the column from the row cursor and run validation msodbcsql never reaches.
    if target_value_ptr.is_null() {
        stmt_state.clear_binding(column_number);
        debug!(column_number, "SQLBindCol: column unbound");
        return SQL_SUCCESS;
    }

    if buffer_length < 0 {
        error!(buffer_length, "SQLBindCol: negative buffer length");
        post_diag(&mut stmt_state, ERR_INVALID_STRING_OR_BUFFER_LENGTH);
        return SQL_ERROR;
    }

    // Same gate as SQLBindParameter: fold the deprecated 2.x date/time
    // spellings first so one form per type reaches storage and delivery. This
    // only decides whether the identifier names a real ODBC type; whether the
    // fetch can actually deliver it is a per-row question.
    let target_type = canonical_c_type(target_type);
    if !is_valid_c_type(target_type) {
        error!(target_type, "SQLBindCol: invalid target C type");
        post_diag(&mut stmt_state, ERR_INVALID_C_DATA_TYPE);
        return SQL_ERROR;
    }

    stmt_state.set_binding(ColumnBinding {
        column_number,
        target_type,
        target_value_ptr,
        buffer_length,
        strlen_or_ind_ptr,
    });
    debug!(column_number, target_type, "SQLBindCol: column bound");
    SQL_SUCCESS
}

/// Implements `SQLFreeStmt(SQL_UNBIND)`: drops every column binding.
///
/// mssql-python calls this before every fetch, so the columnar path depends on
/// it even when the application never unbinds a column itself.
///
/// # Safety
/// `statement_handle` must be a valid `StmtHandle` or null.
pub(crate) unsafe fn sql_free_stmt_unbind(statement_handle: SqlHandle) -> SqlReturn {
    debug!(?statement_handle, "SQLFreeStmt(SQL_UNBIND) called");
    crate::ffi_entry!("SQLFreeStmt(SQL_UNBIND)", unsafe {
        if statement_handle.is_null() {
            error!("SQLFreeStmt(SQL_UNBIND): statement_handle is null");
            return SQL_INVALID_HANDLE;
        }
        let stmt = handle_from_raw::<StmtHandle>(statement_handle);
        debug_assert_eq!(stmt.object_type, HandleType::Stmt);
        sql_free_stmt_unbind_safe(stmt)
    })
}

fn sql_free_stmt_unbind_safe(stmt: &StmtHandle) -> SqlReturn {
    let Ok(mut stmt_state) = stmt.inner.lock() else {
        error!("SQLFreeStmt(SQL_UNBIND): stmt mutex poisoned");
        return SQL_ERROR;
    };
    free_errors(&mut stmt_state);
    if stmt_state.has_state(STMT_STATE_FETCH_IN_PROGRESS) {
        error!("SQLFreeStmt(SQL_UNBIND): a fetch is in progress on this statement");
        post_diag(&mut stmt_state, ERR_FUNCTION_SEQUENCE);
        return SQL_ERROR;
    }
    stmt_state.clear_bindings();
    debug!("SQLFreeStmt(SQL_UNBIND): all column bindings released");
    SQL_SUCCESS
}

#[cfg(test)]
mod tests {
    use std::ptr;

    use super::*;
    use crate::api::odbc_types::{
        SQL_C_CHAR, SQL_C_DATE, SQL_C_DEFAULT, SQL_C_INTERVAL_YEAR, SQL_C_NUMERIC, SQL_C_SLONG,
        SQL_C_TIME, SQL_C_TIMESTAMP, SQL_C_TYPE_DATE, SQL_C_TYPE_TIME, SQL_C_TYPE_TIMESTAMP,
    };
    use crate::handles::stmt::STMT_STATE_FETCH_IN_PROGRESS;
    use crate::test_support::TestHandles;

    fn bindings_len(h: &TestHandles) -> usize {
        let stmt = unsafe { handle_from_raw::<StmtHandle>(h.stmt) };
        let s = stmt.inner.lock().unwrap();
        s.bindings.len()
    }

    fn last_state(h: &TestHandles) -> [u8; 5] {
        let stmt = unsafe { handle_from_raw::<StmtHandle>(h.stmt) };
        let s = stmt.inner.lock().unwrap();
        s.diag_records.last().unwrap().sql_state
    }

    #[test]
    fn null_handle_returns_invalid_handle() {
        let mut buf = 0i32;
        let rc = unsafe {
            sql_bind_col(
                ptr::null_mut(),
                1,
                SQL_C_SLONG,
                &mut buf as *mut i32 as SqlPointer,
                0,
                ptr::null_mut(),
            )
        };
        assert_eq!(rc, SQL_INVALID_HANDLE);
    }

    #[test]
    fn binding_a_column_records_it() {
        let h = TestHandles::with_env_dbc_stmt();
        let mut buf = [0i32; 4];
        let mut ind = [0 as SqlLen; 4];
        let rc = unsafe {
            sql_bind_col(
                h.stmt,
                2,
                SQL_C_SLONG,
                buf.as_mut_ptr() as SqlPointer,
                0,
                ind.as_mut_ptr(),
            )
        };
        assert_eq!(rc, SQL_SUCCESS);
        let stmt = unsafe { handle_from_raw::<StmtHandle>(h.stmt) };
        let s = stmt.inner.lock().unwrap();
        assert_eq!(s.bindings.len(), 1);
        assert_eq!(s.bindings[0].column_number, 2);
        assert_eq!(s.bindings[0].target_type, SQL_C_SLONG);
    }

    /// Binding is legal before the statement is executed, so there is no
    /// metadata to validate the ordinal against and no cursor requirement.
    #[test]
    fn binding_before_execute_is_allowed() {
        let h = TestHandles::with_env_dbc_stmt();
        let mut buf = [0i32; 1];
        let rc = unsafe {
            sql_bind_col(
                h.stmt,
                99,
                SQL_C_SLONG,
                buf.as_mut_ptr() as SqlPointer,
                0,
                ptr::null_mut(),
            )
        };
        assert_eq!(
            rc, SQL_SUCCESS,
            "an out-of-range ordinal is a fetch-time concern"
        );
        assert_eq!(bindings_len(&h), 1);
    }

    /// Both pointers null unbinds the column.
    #[test]
    fn binding_with_both_pointers_null_unbinds() {
        let h = TestHandles::with_env_dbc_stmt();
        let mut buf = [0i32; 1];
        unsafe {
            sql_bind_col(
                h.stmt,
                1,
                SQL_C_SLONG,
                buf.as_mut_ptr() as SqlPointer,
                0,
                ptr::null_mut(),
            )
        };
        assert_eq!(bindings_len(&h), 1);

        let rc =
            unsafe { sql_bind_col(h.stmt, 1, SQL_C_SLONG, ptr::null_mut(), 0, ptr::null_mut()) };
        assert_eq!(rc, SQL_SUCCESS);
        assert_eq!(bindings_len(&h), 0);
    }

    /// A null data pointer unbinds whatever the indicator says. Keeping such a
    /// binding alive would consume the column from the row cursor and subject it
    /// to validation msodbcsql never reaches, since it unbinds first.
    #[test]
    fn a_null_data_pointer_unbinds_whatever_the_indicator_says() {
        let h = TestHandles::with_env_dbc_stmt();
        let mut buf = [0i32; 1];
        let mut ind = [0 as SqlLen; 1];
        let rc = unsafe {
            sql_bind_col(
                h.stmt,
                1,
                SQL_C_SLONG,
                buf.as_mut_ptr() as SqlPointer,
                4,
                ind.as_mut_ptr(),
            )
        };
        assert_eq!(rc, SQL_SUCCESS);
        assert_eq!(bindings_len(&h), 1);

        let rc =
            unsafe { sql_bind_col(h.stmt, 1, SQL_C_SLONG, ptr::null_mut(), 0, ind.as_mut_ptr()) };
        assert_eq!(rc, SQL_SUCCESS);
        assert_eq!(
            bindings_len(&h),
            0,
            "the live indicator must not keep it bound"
        );
    }

    /// Unbinding happens before any argument validation, so a combination that
    /// would otherwise be rejected still unbinds rather than erroring.
    #[test]
    fn unbinding_skips_the_argument_validation() {
        let h = TestHandles::with_env_dbc_stmt();
        let mut ind = [0 as SqlLen; 1];
        let rc = unsafe {
            sql_bind_col(
                h.stmt,
                1,
                SQL_C_DEFAULT,
                ptr::null_mut(),
                -1,
                ind.as_mut_ptr(),
            )
        };
        assert_eq!(rc, SQL_SUCCESS);
    }

    /// Column 0 is the bookmark column, which a forward-only cursor does not
    /// offer.
    #[test]
    fn binding_the_bookmark_column_is_rejected() {
        let h = TestHandles::with_env_dbc_stmt();
        let mut buf = [0i32; 1];
        let rc = unsafe {
            sql_bind_col(
                h.stmt,
                0,
                SQL_C_SLONG,
                buf.as_mut_ptr() as SqlPointer,
                0,
                ptr::null_mut(),
            )
        };
        assert_eq!(rc, SQL_ERROR);
        assert_eq!(last_state(&h), *b"07009");
    }

    #[test]
    fn a_negative_buffer_length_is_rejected() {
        let h = TestHandles::with_env_dbc_stmt();
        let mut buf = [0u8; 8];
        let rc = unsafe {
            sql_bind_col(
                h.stmt,
                1,
                SQL_C_CHAR,
                buf.as_mut_ptr() as SqlPointer,
                -1,
                ptr::null_mut(),
            )
        };
        assert_eq!(rc, SQL_ERROR);
        assert_eq!(last_state(&h), *b"HY090");
    }

    /// SQL_C_DEFAULT cannot be resolved until execution supplies the IRD, so
    /// the binding retains the placeholder for each fetch to resolve.
    #[test]
    fn default_target_is_deferred_until_fetch() {
        let h = TestHandles::with_env_dbc_stmt();
        let mut buf = [0u8; 8];
        let rc = unsafe {
            sql_bind_col(
                h.stmt,
                1,
                SQL_C_DEFAULT,
                buf.as_mut_ptr() as SqlPointer,
                buf.len() as SqlLen,
                ptr::null_mut(),
            )
        };
        assert_eq!(rc, SQL_SUCCESS);

        let stmt = unsafe { handle_from_raw::<StmtHandle>(h.stmt) };
        let s = stmt.inner.lock().unwrap();
        assert_eq!(s.bindings[0].target_type, SQL_C_DEFAULT);
    }

    /// An unknown target type is rejected at bind time rather than surfacing as
    /// a per-row failure on every row of the first fetch.
    #[test]
    fn an_unsupported_target_type_is_rejected() {
        let h = TestHandles::with_env_dbc_stmt();
        let mut buf = [0u8; 8];
        let rc = unsafe {
            sql_bind_col(
                h.stmt,
                1,
                12345,
                buf.as_mut_ptr() as SqlPointer,
                8,
                ptr::null_mut(),
            )
        };
        assert_eq!(rc, SQL_ERROR);
        assert_eq!(last_state(&h), *b"HY003");
        assert_eq!(bindings_len(&h), 0);
    }

    /// SQL_UNBIND drops every binding; mssql-python calls it before each fetch.
    #[test]
    fn free_stmt_unbind_clears_every_binding() {
        let h = TestHandles::with_env_dbc_stmt();
        let mut buf = [0i32; 4];
        for col in 1..=3 {
            unsafe {
                sql_bind_col(
                    h.stmt,
                    col,
                    SQL_C_SLONG,
                    buf.as_mut_ptr() as SqlPointer,
                    0,
                    ptr::null_mut(),
                )
            };
        }
        assert_eq!(bindings_len(&h), 3);

        let rc = unsafe { sql_free_stmt_unbind(h.stmt) };
        assert_eq!(rc, SQL_SUCCESS);
        assert_eq!(bindings_len(&h), 0);
        // Unbinding again is a no-op rather than an error.
        assert_eq!(unsafe { sql_free_stmt_unbind(h.stmt) }, SQL_SUCCESS);
    }

    /// A fetch writes through the buffers it snapshotted after releasing the
    /// statement lock, so rebinding mid-fetch could free one under it. Both
    /// mutating entry points refuse rather than race.
    /// The deprecated 2.x date/time spellings are still legal for a 3.x
    /// application, and SQLBindParameter already accepts them; the two paths
    /// share `type_rules` so they cannot drift apart on this.
    #[test]
    fn deprecated_2x_date_types_are_accepted_and_canonicalized() {
        for (passed, canonical) in [
            (SQL_C_DATE, SQL_C_TYPE_DATE),
            (SQL_C_TIME, SQL_C_TYPE_TIME),
            (SQL_C_TIMESTAMP, SQL_C_TYPE_TIMESTAMP),
        ] {
            let h = TestHandles::with_env_dbc_stmt();
            let mut buf = [0u8; 32];
            let rc = unsafe {
                sql_bind_col(
                    h.stmt,
                    1,
                    passed,
                    buf.as_mut_ptr() as SqlPointer,
                    buf.len() as SqlLen,
                    ptr::null_mut(),
                )
            };
            assert_eq!(rc, SQL_SUCCESS, "binding {passed} must be accepted");

            // Storing the canonical form keeps element_stride and deliver_bound
            // on one spelling per type.
            let stmt = unsafe { handle_from_raw::<StmtHandle>(h.stmt) };
            let s = stmt.inner.lock().unwrap();
            assert_eq!(s.bindings[0].target_type, canonical);
        }
    }

    /// A C type that names a real ODBC type but that the fetch cannot deliver
    /// belongs to the per-row path (07006 / HYC00), not to this HY003 gate.
    #[test]
    fn valid_but_undeliverable_c_types_pass_the_bind_gate() {
        for c_type in [SQL_C_NUMERIC, SQL_C_INTERVAL_YEAR] {
            let h = TestHandles::with_env_dbc_stmt();
            let mut buf = [0u8; 32];
            let rc = unsafe {
                sql_bind_col(
                    h.stmt,
                    1,
                    c_type,
                    buf.as_mut_ptr() as SqlPointer,
                    buf.len() as SqlLen,
                    ptr::null_mut(),
                )
            };
            assert_eq!(rc, SQL_SUCCESS, "c_type {c_type} must pass the bind gate");
        }
    }

    #[test]
    fn binding_is_refused_while_a_fetch_is_in_progress() {
        let h = TestHandles::with_env_dbc_stmt();
        {
            let stmt = unsafe { handle_from_raw::<StmtHandle>(h.stmt) };
            let mut s = stmt.inner.lock().unwrap();
            s.set_state(STMT_STATE_FETCH_IN_PROGRESS);
        }
        let mut buf = [0i32; 1];
        let rc = unsafe {
            sql_bind_col(
                h.stmt,
                1,
                SQL_C_SLONG,
                buf.as_mut_ptr() as SqlPointer,
                0,
                ptr::null_mut(),
            )
        };
        assert_eq!(rc, SQL_ERROR);
        assert_eq!(last_state(&h), *b"HY010");

        assert_eq!(unsafe { sql_free_stmt_unbind(h.stmt) }, SQL_ERROR);
        assert_eq!(last_state(&h), *b"HY010");
    }

    #[test]
    fn free_stmt_unbind_rejects_a_null_handle() {
        assert_eq!(
            unsafe { sql_free_stmt_unbind(ptr::null_mut()) },
            SQL_INVALID_HANDLE
        );
    }
}
