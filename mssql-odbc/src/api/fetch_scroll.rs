// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Block-fetch support: bound-column materialization and `SQLFetchScroll`.

use tracing::{debug, error};

use super::cdata::{WriteError, WriteOutcome, write_c_value};
use super::odbc_types::{
    SQL_ERROR, SQL_FETCH_NEXT, SQL_INVALID_HANDLE, SQL_ROW_ERROR, SQL_ROW_SUCCESS,
    SQL_ROW_SUCCESS_WITH_INFO, SQL_SUCCESS, SQL_SUCCESS_WITH_INFO, SQL_UNKNOWN_TYPE, SqlHandle,
    SqlLen, SqlReturn, SqlSmallInt,
};
use super::sqlstate::{
    ERR_INVALID_C_DATA_TYPE, ERR_RESTRICTED_DATA_TYPE, ERR_STRING_RIGHT_TRUNCATION, SQLSTATE_HY106,
    post_diag,
};
use crate::error::{free_errors, post_sql_error};
use crate::handles::stmt::STMT_STATE_CURSOR_OPEN;
use crate::handles::{HandleType, StmtHandle, handle_from_raw};
use crate::params::convert::c_type_stride;

/// Implements `SQLFetchScroll`.
///
/// Only `SQL_FETCH_NEXT` is supported: the driver exposes forward-only,
/// firehose cursors, so any other orientation is a fetch-type-out-of-range
/// error (HY106) exactly as msodbcsql reports for a forward-only cursor.
///
/// # Safety
/// `statement_handle` must be a valid `StmtHandle` or null.
pub(crate) unsafe fn sql_fetch_scroll(
    statement_handle: SqlHandle,
    fetch_orientation: SqlSmallInt,
    fetch_offset: SqlLen,
) -> SqlReturn {
    debug!(
        ?statement_handle,
        fetch_orientation, fetch_offset, "SQLFetchScroll called"
    );
    crate::ffi_entry!("SQLFetchScroll", unsafe {
        sql_fetch_scroll_impl(statement_handle, fetch_orientation)
    })
}

unsafe fn sql_fetch_scroll_impl(
    statement_handle: SqlHandle,
    fetch_orientation: SqlSmallInt,
) -> SqlReturn {
    if statement_handle.is_null() {
        error!("SQLFetchScroll: statement_handle is null");
        return SQL_INVALID_HANDLE;
    }
    let stmt = unsafe { handle_from_raw::<StmtHandle>(statement_handle) };
    debug_assert_eq!(stmt.object_type, HandleType::Stmt);

    {
        let Ok(mut state) = stmt.inner.lock() else {
            error!("SQLFetchScroll: stmt mutex poisoned");
            return SQL_ERROR;
        };
        free_errors(&mut state);
        if let Some(pending) = state.pending_fetch_error.take() {
            error!("SQLFetchScroll: replaying error raised after the previous rowset");
            state.diag_records.push(pending);
            return SQL_ERROR;
        }
        if fetch_orientation != SQL_FETCH_NEXT {
            post_sql_error(
                &mut state,
                SQLSTATE_HY106,
                0,
                "Fetch type out of range; the cursor is forward-only",
            );
            return SQL_ERROR;
        }
        if !state.has_state(STMT_STATE_CURSOR_OPEN) {
            drop(state);
            return unsafe { super::fetch::sql_fetch(statement_handle) };
        }
    }

    super::fetch::fetch_rowset(statement_handle, stmt)
}

/// Copies the statement's current row into the application buffers registered
/// by `SQLBindCol`, at rowset slot `row_index` (column-wise binding).
///
/// Returns the ODBC row status for this row.
pub(crate) fn write_bound_columns(stmt: &StmtHandle, row_index: usize) -> u16 {
    let Ok(mut state) = stmt.inner.lock() else {
        error!("fetch: stmt mutex poisoned writing bound columns");
        return SQL_ROW_ERROR;
    };
    if state.bound_cols.is_empty() {
        return SQL_ROW_SUCCESS;
    }
    let Some(row) = state.current_row.clone() else {
        return SQL_ROW_ERROR;
    };

    let bindings = state.bound_cols.clone();
    let mut status = SQL_ROW_SUCCESS;

    for (idx, binding) in bindings.iter().enumerate() {
        let Some(bc) = binding else { continue };
        let Some(value) = row.get(idx) else { continue };

        // Column-wise binding: each column's buffer is an array of one element
        // per rowset slot. ODBC derives the element stride from the C type for
        // fixed-width targets and only falls back to `BufferLength` for
        // character/binary ones — applications legitimately pass the *total*
        // array size as `BufferLength` for fixed-width types, so using it as the
        // stride would write past the end of the buffer.
        let stride = c_type_stride(bc.target_type, SQL_UNKNOWN_TYPE, bc.buffer_length);
        let data_ptr = if bc.target_value_ptr.is_null() {
            std::ptr::null_mut()
        } else {
            unsafe {
                bc.target_value_ptr
                    .cast::<u8>()
                    .add(row_index * stride)
                    .cast()
            }
        };
        let ind_ptr = if bc.strlen_or_ind_ptr.is_null() {
            std::ptr::null_mut()
        } else {
            unsafe { bc.strlen_or_ind_ptr.add(row_index) }
        };

        match unsafe { write_c_value(value, bc.target_type, data_ptr, bc.buffer_length, ind_ptr) } {
            Ok(WriteOutcome::Complete) => {}
            Ok(WriteOutcome::Truncated) => {
                post_diag(&mut state, ERR_STRING_RIGHT_TRUNCATION);
                status = SQL_ROW_SUCCESS_WITH_INFO;
            }
            Err(WriteError::InvalidCType) => {
                post_diag(&mut state, ERR_INVALID_C_DATA_TYPE);
                return SQL_ROW_ERROR;
            }
            Err(WriteError::RestrictedConversion) => {
                post_diag(&mut state, ERR_RESTRICTED_DATA_TYPE);
                return SQL_ROW_ERROR;
            }
            Err(WriteError::OutOfRange) => {
                post_sql_error(
                    &mut state,
                    crate::api::sqlstate::SQLSTATE_22003,
                    0,
                    "Numeric value out of range",
                );
                return SQL_ROW_ERROR;
            }
        }
    }

    status
}

/// Folds a per-row status into the aggregate return code for the rowset.
pub(crate) fn fold_row_status(current: SqlReturn, row_status: u16) -> SqlReturn {
    match row_status {
        SQL_ROW_ERROR => SQL_ERROR,
        SQL_ROW_SUCCESS_WITH_INFO if current == SQL_SUCCESS => SQL_SUCCESS_WITH_INFO,
        _ => current,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::odbc_types::{SQL_C_SLONG, SQL_FETCH_PRIOR, SQL_NULL_HANDLE};
    use crate::test_support::TestHandles;
    use mssql_tds::datatypes::column_values::ColumnValues;

    #[test]
    fn fetch_scroll_null_handle() {
        let ret = unsafe { sql_fetch_scroll(SQL_NULL_HANDLE, SQL_FETCH_NEXT, 0) };
        assert_eq!(ret, SQL_INVALID_HANDLE);
    }

    #[test]
    fn fetch_scroll_rejects_non_next_orientation() {
        let h = TestHandles::with_env_dbc_stmt();
        let ret = unsafe { sql_fetch_scroll(h.stmt, SQL_FETCH_PRIOR, 0) };
        assert_eq!(ret, SQL_ERROR);

        let stmt = unsafe { handle_from_raw::<StmtHandle>(h.stmt) };
        let state = stmt.inner.lock().unwrap();
        assert_eq!(state.diag_records[0].sql_state, SQLSTATE_HY106);
    }

    #[test]
    fn write_bound_columns_fills_slot() {
        let h = TestHandles::with_env_dbc_stmt();
        let stmt = unsafe { handle_from_raw::<StmtHandle>(h.stmt) };
        let mut buf = [0i32; 4];
        let mut ind = [0 as SqlLen; 4];
        {
            let mut state = stmt.inner.lock().unwrap();
            state.bound_cols = vec![Some(crate::handles::stmt::BoundCol {
                target_type: SQL_C_SLONG,
                target_value_ptr: buf.as_mut_ptr().cast(),
                buffer_length: 4,
                strlen_or_ind_ptr: ind.as_mut_ptr(),
            })];
            state.current_row = Some(vec![ColumnValues::Int(99)]);
        }

        assert_eq!(write_bound_columns(stmt, 2), SQL_ROW_SUCCESS);
        assert_eq!(buf, [0, 0, 99, 0]);
        assert_eq!(ind[2], 4);
    }

    #[test]
    fn write_bound_columns_no_bindings_is_success() {
        let h = TestHandles::with_env_dbc_stmt();
        let stmt = unsafe { handle_from_raw::<StmtHandle>(h.stmt) };
        assert_eq!(write_bound_columns(stmt, 0), SQL_ROW_SUCCESS);
    }

    #[test]
    fn fold_row_status_promotes_info_and_error() {
        assert_eq!(fold_row_status(SQL_SUCCESS, SQL_ROW_SUCCESS), SQL_SUCCESS);
        assert_eq!(
            fold_row_status(SQL_SUCCESS, SQL_ROW_SUCCESS_WITH_INFO),
            SQL_SUCCESS_WITH_INFO
        );
        assert_eq!(fold_row_status(SQL_SUCCESS, SQL_ROW_ERROR), SQL_ERROR);
    }
}
