// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Implementation of `SQLSetStmtAttrW` / `SQLGetStmtAttrW`.
//!
//! The block-fetch rowset controls (`SQL_ATTR_ROW_ARRAY_SIZE`,
//! `SQL_ATTR_ROWS_FETCHED_PTR`, `SQL_ATTR_ROW_STATUS_PTR`,
//! `SQL_ATTR_ROW_BIND_TYPE`) are stored and later consumed by the columnar
//! fetch path. `SQL_ATTR_CURSOR_TYPE` and `SQL_ATTR_CONCURRENCY` accept only the
//! supported forward-only / read-only values; any other request is substituted
//! and reported with `01S02` (option value changed) rather than silently
//! succeeding. `SQL_ATTR_QUERY_TIMEOUT` is stored and clamped to
//! [`MAX_QUERY_TIMEOUT`], matching msodbcsql, which reports `01S02` rather than
//! rejecting an over-large request; enforcing the stored timeout against a
//! running statement is tracked separately. Other recognized statement
//! attributes (param / descriptor controls) are accepted without effect.
//! `SQL_ATTR_PARAMSET_SIZE` accepts the ODBC default of 1 but rejects larger
//! batches, since parameter arrays are not yet consumed and a silent success
//! would execute only the first row. Unrecognized attribute identifiers fail
//! with `HY092`.
//!
//! Each entry point follows the crate's mandatory layering: FFI panic boundary
//! → `unsafe` raw-handle shim → safe core (`README.md`; `num_result_cols.rs`).

use tracing::{debug, error};

use crate::api::odbc_types::{
    MAX_QUERY_TIMEOUT, SQL_ATTR_APP_PARAM_DESC, SQL_ATTR_APP_ROW_DESC, SQL_ATTR_CONCURRENCY,
    SQL_ATTR_CURSOR_TYPE, SQL_ATTR_IMP_PARAM_DESC, SQL_ATTR_IMP_ROW_DESC, SQL_ATTR_PARAM_BIND_TYPE,
    SQL_ATTR_PARAM_STATUS_PTR, SQL_ATTR_PARAMS_PROCESSED_PTR, SQL_ATTR_PARAMSET_SIZE,
    SQL_ATTR_QUERY_TIMEOUT, SQL_ATTR_ROW_ARRAY_SIZE, SQL_ATTR_ROW_BIND_OFFSET_PTR,
    SQL_ATTR_ROW_BIND_TYPE, SQL_ATTR_ROW_STATUS_PTR, SQL_ATTR_ROWS_FETCHED_PTR,
    SQL_CONCUR_READ_ONLY, SQL_CURSOR_FORWARD_ONLY, SQL_ERROR, SQL_INVALID_HANDLE, SQL_SUCCESS,
    SQL_SUCCESS_WITH_INFO, SqlHandle, SqlInteger, SqlPointer, SqlReturn, SqlULen, SqlUSmallInt,
};
use crate::api::sqlstate::{
    ERR_INVALID_ATTRIBUTE_IDENTIFIER, ERR_INVALID_ATTRIBUTE_VALUE, SQLSTATE_01S02, SQLSTATE_HYC00,
    WARN_OPTION_VALUE_CHANGED, post_diag,
};
use crate::api::util::write_if_some;
use crate::error::{free_errors, post_sql_error};
use crate::handles::{HandleType, StmtHandle, handle_from_raw};

/// Clamps a requested `SQL_ATTR_QUERY_TIMEOUT` to the largest value the driver
/// accepts, reporting whether the request had to be reduced.
///
/// msodbcsql caps at `MAX_QUERY_TIMEOUT` and posts `01S02` instead of rejecting
/// (`sqlcmisc.cpp:3988-3994`), so an over-large request still yields a usable
/// statement. Shared with the `SQLSetConnectAttrW` route, which applies the same
/// cap before fanning the value out.
pub(super) fn clamp_query_timeout(requested: SqlULen) -> (u32, bool) {
    let seconds = u32::try_from(requested).unwrap_or(u32::MAX);
    if seconds > MAX_QUERY_TIMEOUT {
        (MAX_QUERY_TIMEOUT, true)
    } else {
        (seconds, false)
    }
}

/// Sets a statement attribute.
///
/// # Safety
/// `statement_handle` must be a valid `StmtHandle` or null. For the pointer
/// attributes the caller-supplied `value_ptr` must remain valid for the
/// lifetime it is used by later fetches.
pub(crate) unsafe fn sql_set_stmt_attr_w(
    statement_handle: SqlHandle,
    attribute: SqlInteger,
    value_ptr: SqlPointer,
    string_length: SqlInteger,
) -> SqlReturn {
    debug!(
        ?statement_handle,
        attribute,
        ?value_ptr,
        string_length,
        "SQLSetStmtAttrW called",
    );
    crate::ffi_entry!("SQLSetStmtAttrW", unsafe {
        sql_set_stmt_attr_w_impl(statement_handle, attribute, value_ptr)
    })
}

unsafe fn sql_set_stmt_attr_w_impl(
    statement_handle: SqlHandle,
    attribute: SqlInteger,
    value_ptr: SqlPointer,
) -> SqlReturn {
    if statement_handle.is_null() {
        error!("SQLSetStmtAttrW: statement_handle is null");
        return SQL_INVALID_HANDLE;
    }

    let stmt = unsafe { handle_from_raw::<StmtHandle>(statement_handle) };
    debug_assert_eq!(
        stmt.object_type,
        HandleType::Stmt,
        "SQLSetStmtAttrW: handle is not a STMT"
    );
    sql_set_stmt_attr_w_safe(stmt, attribute, value_ptr)
}

fn sql_set_stmt_attr_w_safe(
    stmt: &StmtHandle,
    attribute: SqlInteger,
    value_ptr: SqlPointer,
) -> SqlReturn {
    let Ok(mut state) = stmt.inner.lock() else {
        error!("SQLSetStmtAttrW: stmt mutex poisoned");
        return SQL_ERROR;
    };
    free_errors(&mut state);

    match attribute {
        SQL_ATTR_ROW_ARRAY_SIZE => {
            // The value is a `SQLULEN` passed by value in the pointer slot. Zero
            // is an invalid rowset size (HY024) — reject rather than paper over.
            let n = value_ptr as SqlULen;
            if n == 0 {
                error!("SQLSetStmtAttrW: SQL_ATTR_ROW_ARRAY_SIZE of 0 is invalid");
                post_diag(&mut state, ERR_INVALID_ATTRIBUTE_VALUE);
                return SQL_ERROR;
            }
            state.row_array_size = n;
            debug!(
                row_array_size = n,
                "SQLSetStmtAttrW: SQL_ATTR_ROW_ARRAY_SIZE set"
            );
            SQL_SUCCESS
        }
        SQL_ATTR_ROWS_FETCHED_PTR => {
            state.rows_fetched_ptr = value_ptr as *mut SqlULen;
            SQL_SUCCESS
        }
        SQL_ATTR_ROW_STATUS_PTR => {
            state.row_status_ptr = value_ptr as *mut SqlUSmallInt;
            SQL_SUCCESS
        }
        SQL_ATTR_ROW_BIND_TYPE => {
            state.row_bind_type = value_ptr as SqlULen;
            SQL_SUCCESS
        }
        SQL_ATTR_PARAMSET_SIZE => {
            // Parameter arrays are not yet consumed (executemany batch insert is
            // tracked separately). Accept the ODBC default of 1; reject a larger
            // batch (HYC00) instead of silently executing only the first row,
            // and reject 0 as an invalid value (HY024).
            match value_ptr as SqlULen {
                1 => SQL_SUCCESS,
                0 => {
                    error!("SQLSetStmtAttrW: SQL_ATTR_PARAMSET_SIZE of 0 is invalid");
                    post_diag(&mut state, ERR_INVALID_ATTRIBUTE_VALUE);
                    SQL_ERROR
                }
                n => {
                    error!(
                        paramset_size = n,
                        "SQLSetStmtAttrW: SQL_ATTR_PARAMSET_SIZE > 1 not supported"
                    );
                    post_sql_error(
                        &mut state,
                        SQLSTATE_HYC00,
                        0,
                        "Parameter arrays (SQL_ATTR_PARAMSET_SIZE > 1) are not supported",
                    );
                    SQL_ERROR
                }
            }
        }
        SQL_ATTR_CURSOR_TYPE => {
            // The driver is forward-only. Accept SQL_CURSOR_FORWARD_ONLY as-is;
            // for any other cursor type substitute forward-only and warn with
            // 01S02, per the ODBC contract for unsupported cursor types (a
            // silent success would tell the caller a scrollable cursor took
            // effect when it did not). The substituted value is what
            // SQLGetStmtAttrW reports back.
            if value_ptr as SqlULen == SQL_CURSOR_FORWARD_ONLY {
                SQL_SUCCESS
            } else {
                debug!(
                    requested = value_ptr as SqlULen,
                    "SQLSetStmtAttrW: cursor type substituted with SQL_CURSOR_FORWARD_ONLY"
                );
                post_sql_error(
                    &mut state,
                    SQLSTATE_01S02,
                    0,
                    "Cursor type not supported; substituted SQL_CURSOR_FORWARD_ONLY",
                );
                SQL_SUCCESS_WITH_INFO
            }
        }
        SQL_ATTR_CONCURRENCY => {
            // The driver is read-only. Accept SQL_CONCUR_READ_ONLY as-is;
            // substitute read-only and warn with 01S02 for any writable
            // concurrency request.
            if value_ptr as SqlULen == SQL_CONCUR_READ_ONLY {
                SQL_SUCCESS
            } else {
                debug!(
                    requested = value_ptr as SqlULen,
                    "SQLSetStmtAttrW: concurrency substituted with SQL_CONCUR_READ_ONLY"
                );
                post_sql_error(
                    &mut state,
                    SQLSTATE_01S02,
                    0,
                    "Concurrency not supported; substituted SQL_CONCUR_READ_ONLY",
                );
                SQL_SUCCESS_WITH_INFO
            }
        }
        SQL_ATTR_QUERY_TIMEOUT => {
            let (seconds, clamped) = clamp_query_timeout(value_ptr as SqlULen);
            state.query_timeout = seconds;
            debug!(seconds, "SQLSetStmtAttrW: SQL_ATTR_QUERY_TIMEOUT set");
            if clamped {
                post_diag(&mut state, WARN_OPTION_VALUE_CHANGED);
                return SQL_SUCCESS_WITH_INFO;
            }
            SQL_SUCCESS
        }
        // Recognized attributes accepted without tracking: these param /
        // descriptor controls have no effect on the implemented forward-only,
        // read-only behavior.
        SQL_ATTR_PARAM_BIND_TYPE
        | SQL_ATTR_PARAM_STATUS_PTR
        | SQL_ATTR_PARAMS_PROCESSED_PTR
        | SQL_ATTR_ROW_BIND_OFFSET_PTR
        | SQL_ATTR_APP_ROW_DESC
        | SQL_ATTR_APP_PARAM_DESC => {
            debug!(attribute, "SQLSetStmtAttrW: attribute accepted as no-op");
            SQL_SUCCESS
        }
        _ => {
            error!(
                attribute,
                "SQLSetStmtAttrW: unrecognized attribute identifier"
            );
            post_diag(&mut state, ERR_INVALID_ATTRIBUTE_IDENTIFIER);
            SQL_ERROR
        }
    }
}

/// Retrieves a statement attribute.
///
/// # Safety
/// `statement_handle` must be a valid `StmtHandle` or null. `value_ptr`, when
/// non-null, must be writable for the size of the attribute (pointer-sized for
/// every attribute handled here).
pub(crate) unsafe fn sql_get_stmt_attr_w(
    statement_handle: SqlHandle,
    attribute: SqlInteger,
    value_ptr: SqlPointer,
    buffer_length: SqlInteger,
    string_length_ptr: *mut SqlInteger,
) -> SqlReturn {
    debug!(
        ?statement_handle,
        attribute,
        ?value_ptr,
        buffer_length,
        ?string_length_ptr,
        "SQLGetStmtAttrW called",
    );
    crate::ffi_entry!("SQLGetStmtAttrW", unsafe {
        sql_get_stmt_attr_w_impl(statement_handle, attribute, value_ptr)
    })
}

unsafe fn sql_get_stmt_attr_w_impl(
    statement_handle: SqlHandle,
    attribute: SqlInteger,
    value_ptr: SqlPointer,
) -> SqlReturn {
    if statement_handle.is_null() {
        error!("SQLGetStmtAttrW: statement_handle is null");
        return SQL_INVALID_HANDLE;
    }

    let stmt = unsafe { handle_from_raw::<StmtHandle>(statement_handle) };
    debug_assert_eq!(
        stmt.object_type,
        HandleType::Stmt,
        "SQLGetStmtAttrW: handle is not a STMT"
    );
    sql_get_stmt_attr_w_safe(stmt, attribute, value_ptr)
}

fn sql_get_stmt_attr_w_safe(
    stmt: &StmtHandle,
    attribute: SqlInteger,
    value_ptr: SqlPointer,
) -> SqlReturn {
    let Ok(mut state) = stmt.inner.lock() else {
        error!("SQLGetStmtAttrW: stmt mutex poisoned");
        return SQL_ERROR;
    };
    free_errors(&mut state);

    // Every attribute reported here is a pointer-sized integer or pointer.
    // `write_if_some` is a no-op when `value_ptr` is null.
    match attribute {
        SQL_ATTR_ROW_ARRAY_SIZE => unsafe {
            write_if_some(value_ptr as *mut SqlULen, state.row_array_size);
        },
        SQL_ATTR_ROWS_FETCHED_PTR => unsafe {
            write_if_some(value_ptr as *mut *mut SqlULen, state.rows_fetched_ptr);
        },
        SQL_ATTR_ROW_STATUS_PTR => unsafe {
            write_if_some(value_ptr as *mut *mut SqlUSmallInt, state.row_status_ptr);
        },
        SQL_ATTR_ROW_BIND_TYPE => unsafe {
            write_if_some(value_ptr as *mut SqlULen, state.row_bind_type);
        },
        SQL_ATTR_QUERY_TIMEOUT => unsafe {
            write_if_some(value_ptr as *mut SqlULen, state.query_timeout as SqlULen);
        },
        // Recognized attributes we don't store: report their effective ODBC
        // defaults for this forward-only, read-only, single-paramset driver.
        SQL_ATTR_CURSOR_TYPE => unsafe {
            write_if_some(value_ptr as *mut SqlULen, SQL_CURSOR_FORWARD_ONLY);
        },
        SQL_ATTR_CONCURRENCY => unsafe {
            write_if_some(value_ptr as *mut SqlULen, SQL_CONCUR_READ_ONLY);
        },
        SQL_ATTR_PARAMSET_SIZE => unsafe {
            write_if_some(value_ptr as *mut SqlULen, 1);
        },
        // The four implicit descriptors (ARD/APD/IRD/IPD). The Driver Manager
        // queries these while allocating a statement to obtain the driver's
        // descriptor handles; returning them avoids a null descriptor
        // dereference inside the DM's `SQLExecDirectW`.
        SQL_ATTR_APP_ROW_DESC => unsafe {
            write_if_some(value_ptr as *mut SqlHandle, stmt.ard);
        },
        SQL_ATTR_APP_PARAM_DESC => unsafe {
            write_if_some(value_ptr as *mut SqlHandle, stmt.apd);
        },
        SQL_ATTR_IMP_ROW_DESC => unsafe {
            write_if_some(value_ptr as *mut SqlHandle, stmt.ird);
        },
        SQL_ATTR_IMP_PARAM_DESC => unsafe {
            write_if_some(value_ptr as *mut SqlHandle, stmt.ipd);
        },
        _ => {
            error!(
                attribute,
                "SQLGetStmtAttrW: unrecognized attribute identifier"
            );
            post_diag(&mut state, ERR_INVALID_ATTRIBUTE_IDENTIFIER);
            return SQL_ERROR;
        }
    }

    SQL_SUCCESS
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::odbc_types::{SQL_BIND_BY_COLUMN, SQL_NULL_HANDLE};
    use crate::handles::handle_from_raw;
    use crate::test_support::TestHandles;

    #[test]
    fn set_stmt_attr_null_handle() {
        let ret = unsafe {
            sql_set_stmt_attr_w(
                SQL_NULL_HANDLE,
                SQL_ATTR_ROW_ARRAY_SIZE,
                10 as SqlPointer,
                0,
            )
        };
        assert_eq!(ret, SQL_INVALID_HANDLE);
    }

    /// Reads `SQL_ATTR_QUERY_TIMEOUT` back off a statement.
    fn stmt_query_timeout(stmt: SqlHandle) -> SqlULen {
        let mut out: SqlULen = 0;
        let rc = unsafe {
            sql_get_stmt_attr_w(
                stmt,
                SQL_ATTR_QUERY_TIMEOUT,
                (&mut out as *mut SqlULen).cast(),
                0,
                std::ptr::null_mut(),
            )
        };
        assert_eq!(rc, SQL_SUCCESS);
        out
    }

    #[test]
    fn query_timeout_defaults_to_no_limit() {
        let h = TestHandles::with_env_dbc_stmt();
        assert_eq!(stmt_query_timeout(h.stmt), 0);
    }

    #[test]
    fn query_timeout_round_trips() {
        let h = TestHandles::with_env_dbc_stmt();
        let ret =
            unsafe { sql_set_stmt_attr_w(h.stmt, SQL_ATTR_QUERY_TIMEOUT, 30 as SqlPointer, 0) };
        assert_eq!(ret, SQL_SUCCESS);
        assert_eq!(stmt_query_timeout(h.stmt), 30);
    }

    #[test]
    fn query_timeout_at_the_cap_is_accepted_silently() {
        let h = TestHandles::with_env_dbc_stmt();
        let ret = unsafe {
            sql_set_stmt_attr_w(
                h.stmt,
                SQL_ATTR_QUERY_TIMEOUT,
                MAX_QUERY_TIMEOUT as usize as SqlPointer,
                0,
            )
        };
        assert_eq!(ret, SQL_SUCCESS);
        assert_eq!(stmt_query_timeout(h.stmt), MAX_QUERY_TIMEOUT as SqlULen);
    }

    #[test]
    fn query_timeout_past_the_cap_is_clamped_with_a_warning() {
        let h = TestHandles::with_env_dbc_stmt();
        let ret = unsafe {
            sql_set_stmt_attr_w(h.stmt, SQL_ATTR_QUERY_TIMEOUT, 0x10000 as SqlPointer, 0)
        };
        // Clamped rather than rejected, so the statement stays usable.
        assert_eq!(ret, SQL_SUCCESS_WITH_INFO);
        {
            let stmt = unsafe { handle_from_raw::<StmtHandle>(h.stmt) };
            let state = stmt.inner.lock().unwrap();
            assert_eq!(state.diag_records[0].sql_state, SQLSTATE_01S02);
        }
        assert_eq!(stmt_query_timeout(h.stmt), MAX_QUERY_TIMEOUT as SqlULen);
    }

    #[test]
    fn clamp_reports_whether_it_reduced_the_request() {
        assert_eq!(clamp_query_timeout(0), (0, false));
        assert_eq!(clamp_query_timeout(30), (30, false));
        assert_eq!(
            clamp_query_timeout(MAX_QUERY_TIMEOUT as SqlULen),
            (MAX_QUERY_TIMEOUT, false)
        );
        assert_eq!(
            clamp_query_timeout(MAX_QUERY_TIMEOUT as SqlULen + 1),
            (MAX_QUERY_TIMEOUT, true)
        );
        // A value past `u32` must clamp, not wrap to a small number (or to 0,
        // which would silently mean "no timeout").
        assert_eq!(clamp_query_timeout(SqlULen::MAX), (MAX_QUERY_TIMEOUT, true));
    }

    #[test]
    fn set_row_array_size_stored_and_readback() {
        let h = TestHandles::with_env_dbc_stmt();
        let ret =
            unsafe { sql_set_stmt_attr_w(h.stmt, SQL_ATTR_ROW_ARRAY_SIZE, 128 as SqlPointer, 0) };
        assert_eq!(ret, SQL_SUCCESS);

        let stmt = unsafe { handle_from_raw::<StmtHandle>(h.stmt) };
        assert_eq!(stmt.inner.lock().unwrap().row_array_size, 128);

        let mut out: SqlULen = 0;
        let ret = unsafe {
            sql_get_stmt_attr_w(
                h.stmt,
                SQL_ATTR_ROW_ARRAY_SIZE,
                (&mut out as *mut SqlULen).cast(),
                0,
                std::ptr::null_mut(),
            )
        };
        assert_eq!(ret, SQL_SUCCESS);
        assert_eq!(out, 128);
    }

    #[test]
    fn set_row_array_size_zero_rejected() {
        let h = TestHandles::with_env_dbc_stmt();
        let ret =
            unsafe { sql_set_stmt_attr_w(h.stmt, SQL_ATTR_ROW_ARRAY_SIZE, 0 as SqlPointer, 0) };
        assert_eq!(ret, SQL_ERROR);
        // The previous (default) value must be left untouched.
        let stmt = unsafe { handle_from_raw::<StmtHandle>(h.stmt) };
        assert_eq!(stmt.inner.lock().unwrap().row_array_size, 1);
    }

    #[test]
    fn set_rows_fetched_ptr_stored() {
        let h = TestHandles::with_env_dbc_stmt();
        let mut rows_fetched: SqlULen = 0;
        let ptr = &mut rows_fetched as *mut SqlULen;
        let ret = unsafe { sql_set_stmt_attr_w(h.stmt, SQL_ATTR_ROWS_FETCHED_PTR, ptr.cast(), 0) };
        assert_eq!(ret, SQL_SUCCESS);
        let stmt = unsafe { handle_from_raw::<StmtHandle>(h.stmt) };
        assert_eq!(stmt.inner.lock().unwrap().rows_fetched_ptr, ptr);
    }

    #[test]
    fn set_row_status_ptr_stored() {
        let h = TestHandles::with_env_dbc_stmt();
        let mut status: SqlUSmallInt = 0;
        let ptr = &mut status as *mut SqlUSmallInt;
        let ret = unsafe { sql_set_stmt_attr_w(h.stmt, SQL_ATTR_ROW_STATUS_PTR, ptr.cast(), 0) };
        assert_eq!(ret, SQL_SUCCESS);
        let stmt = unsafe { handle_from_raw::<StmtHandle>(h.stmt) };
        assert_eq!(stmt.inner.lock().unwrap().row_status_ptr, ptr);
    }

    #[test]
    fn set_row_bind_type_stored_and_readback() {
        let h = TestHandles::with_env_dbc_stmt();
        let ret =
            unsafe { sql_set_stmt_attr_w(h.stmt, SQL_ATTR_ROW_BIND_TYPE, 40 as SqlPointer, 0) };
        assert_eq!(ret, SQL_SUCCESS);
        let mut out: SqlULen = 0;
        let ret = unsafe {
            sql_get_stmt_attr_w(
                h.stmt,
                SQL_ATTR_ROW_BIND_TYPE,
                (&mut out as *mut SqlULen).cast(),
                0,
                std::ptr::null_mut(),
            )
        };
        assert_eq!(ret, SQL_SUCCESS);
        assert_eq!(out, 40);
    }

    #[test]
    fn default_row_bind_type_is_column_wise() {
        let h = TestHandles::with_env_dbc_stmt();
        let mut out: SqlULen = 999;
        let ret = unsafe {
            sql_get_stmt_attr_w(
                h.stmt,
                SQL_ATTR_ROW_BIND_TYPE,
                (&mut out as *mut SqlULen).cast(),
                0,
                std::ptr::null_mut(),
            )
        };
        assert_eq!(ret, SQL_SUCCESS);
        assert_eq!(out, SQL_BIND_BY_COLUMN);
    }

    #[test]
    fn set_unknown_attribute_rejected() {
        let h = TestHandles::with_env_dbc_stmt();
        let ret = unsafe { sql_set_stmt_attr_w(h.stmt, 9999, 0 as SqlPointer, 0) };
        assert_eq!(ret, SQL_ERROR);
    }

    #[test]
    fn set_recognized_untracked_attribute_accepted() {
        let h = TestHandles::with_env_dbc_stmt();
        let ret = unsafe {
            sql_set_stmt_attr_w(
                h.stmt,
                SQL_ATTR_CONCURRENCY,
                SQL_CONCUR_READ_ONLY as SqlPointer,
                0,
            )
        };
        assert_eq!(ret, SQL_SUCCESS);
    }

    #[test]
    fn set_cursor_type_forward_only_accepted() {
        let h = TestHandles::with_env_dbc_stmt();
        let ret = unsafe {
            sql_set_stmt_attr_w(
                h.stmt,
                SQL_ATTR_CURSOR_TYPE,
                SQL_CURSOR_FORWARD_ONLY as SqlPointer,
                0,
            )
        };
        assert_eq!(ret, SQL_SUCCESS);
    }

    #[test]
    fn set_cursor_type_unsupported_substituted() {
        let h = TestHandles::with_env_dbc_stmt();
        // Any non-forward-only cursor (e.g. SQL_CURSOR_STATIC = 3) is
        // substituted with forward-only and reported via 01S02.
        let ret = unsafe { sql_set_stmt_attr_w(h.stmt, SQL_ATTR_CURSOR_TYPE, 3 as SqlPointer, 0) };
        assert_eq!(ret, SQL_SUCCESS_WITH_INFO);

        // The getter still reports the supported forward-only value.
        let mut out: SqlULen = 999;
        let ret = unsafe {
            sql_get_stmt_attr_w(
                h.stmt,
                SQL_ATTR_CURSOR_TYPE,
                (&mut out as *mut SqlULen).cast(),
                0,
                std::ptr::null_mut(),
            )
        };
        assert_eq!(ret, SQL_SUCCESS);
        assert_eq!(out, SQL_CURSOR_FORWARD_ONLY);
    }

    #[test]
    fn set_concurrency_unsupported_substituted() {
        let h = TestHandles::with_env_dbc_stmt();
        // Any writable concurrency (e.g. SQL_CONCUR_LOCK = 2) is substituted
        // with read-only and reported via 01S02.
        let ret = unsafe { sql_set_stmt_attr_w(h.stmt, SQL_ATTR_CONCURRENCY, 2 as SqlPointer, 0) };
        assert_eq!(ret, SQL_SUCCESS_WITH_INFO);

        let mut out: SqlULen = 999;
        let ret = unsafe {
            sql_get_stmt_attr_w(
                h.stmt,
                SQL_ATTR_CONCURRENCY,
                (&mut out as *mut SqlULen).cast(),
                0,
                std::ptr::null_mut(),
            )
        };
        assert_eq!(ret, SQL_SUCCESS);
        assert_eq!(out, SQL_CONCUR_READ_ONLY);
    }

    #[test]
    fn set_paramset_size_one_accepted() {
        let h = TestHandles::with_env_dbc_stmt();
        let ret =
            unsafe { sql_set_stmt_attr_w(h.stmt, SQL_ATTR_PARAMSET_SIZE, 1 as SqlPointer, 0) };
        assert_eq!(ret, SQL_SUCCESS);
    }

    #[test]
    fn set_paramset_size_greater_than_one_rejected() {
        let h = TestHandles::with_env_dbc_stmt();
        let ret =
            unsafe { sql_set_stmt_attr_w(h.stmt, SQL_ATTR_PARAMSET_SIZE, 100 as SqlPointer, 0) };
        assert_eq!(ret, SQL_ERROR);
    }

    #[test]
    fn set_paramset_size_zero_rejected() {
        let h = TestHandles::with_env_dbc_stmt();
        let ret =
            unsafe { sql_set_stmt_attr_w(h.stmt, SQL_ATTR_PARAMSET_SIZE, 0 as SqlPointer, 0) };
        assert_eq!(ret, SQL_ERROR);
    }

    #[test]
    fn get_stmt_attr_null_handle() {
        let mut out: SqlULen = 0;
        let ret = unsafe {
            sql_get_stmt_attr_w(
                SQL_NULL_HANDLE,
                SQL_ATTR_ROW_ARRAY_SIZE,
                (&mut out as *mut SqlULen).cast(),
                0,
                std::ptr::null_mut(),
            )
        };
        assert_eq!(ret, SQL_INVALID_HANDLE);
    }

    #[test]
    fn get_unknown_attribute_rejected() {
        let h = TestHandles::with_env_dbc_stmt();
        let mut out: SqlULen = 7;
        let ret = unsafe {
            sql_get_stmt_attr_w(
                h.stmt,
                9999,
                (&mut out as *mut SqlULen).cast(),
                0,
                std::ptr::null_mut(),
            )
        };
        assert_eq!(ret, SQL_ERROR);
        // Output must be left untouched on an invalid identifier.
        assert_eq!(out, 7);
    }

    #[test]
    fn get_concurrency_default_is_read_only() {
        let h = TestHandles::with_env_dbc_stmt();
        let mut out: SqlULen = 999;
        let ret = unsafe {
            sql_get_stmt_attr_w(
                h.stmt,
                SQL_ATTR_CONCURRENCY,
                (&mut out as *mut SqlULen).cast(),
                0,
                std::ptr::null_mut(),
            )
        };
        assert_eq!(ret, SQL_SUCCESS);
        assert_eq!(out, SQL_CONCUR_READ_ONLY);
    }

    #[test]
    fn get_cursor_type_default_is_forward_only() {
        let h = TestHandles::with_env_dbc_stmt();
        let mut out: SqlULen = 999;
        let ret = unsafe {
            sql_get_stmt_attr_w(
                h.stmt,
                SQL_ATTR_CURSOR_TYPE,
                (&mut out as *mut SqlULen).cast(),
                0,
                std::ptr::null_mut(),
            )
        };
        assert_eq!(ret, SQL_SUCCESS);
        assert_eq!(out, SQL_CURSOR_FORWARD_ONLY);
    }

    #[test]
    fn get_paramset_size_default_is_one() {
        let h = TestHandles::with_env_dbc_stmt();
        let mut out: SqlULen = 999;
        let ret = unsafe {
            sql_get_stmt_attr_w(
                h.stmt,
                SQL_ATTR_PARAMSET_SIZE,
                (&mut out as *mut SqlULen).cast(),
                0,
                std::ptr::null_mut(),
            )
        };
        assert_eq!(ret, SQL_SUCCESS);
        assert_eq!(out, 1);
    }

    #[test]
    fn get_stmt_attr_null_value_ptr_is_noop_success() {
        let h = TestHandles::with_env_dbc_stmt();
        let ret = unsafe {
            sql_get_stmt_attr_w(
                h.stmt,
                SQL_ATTR_ROW_ARRAY_SIZE,
                std::ptr::null_mut(),
                0,
                std::ptr::null_mut(),
            )
        };
        assert_eq!(ret, SQL_SUCCESS);
    }

    fn read_desc(stmt: SqlHandle, attribute: SqlInteger) -> (SqlReturn, SqlHandle) {
        let mut out: SqlHandle = SQL_NULL_HANDLE;
        let rc = unsafe {
            sql_get_stmt_attr_w(
                stmt,
                attribute,
                &mut out as *mut SqlHandle as SqlPointer,
                0,
                std::ptr::null_mut(),
            )
        };
        (rc, out)
    }

    #[test]
    fn get_returns_the_four_implicit_descriptors() {
        let h = TestHandles::with_env_dbc_stmt();
        let stmt_ref = unsafe { handle_from_raw::<StmtHandle>(h.stmt) };

        for (attr, expected) in [
            (SQL_ATTR_APP_ROW_DESC, stmt_ref.ard),
            (SQL_ATTR_APP_PARAM_DESC, stmt_ref.apd),
            (SQL_ATTR_IMP_ROW_DESC, stmt_ref.ird),
            (SQL_ATTR_IMP_PARAM_DESC, stmt_ref.ipd),
        ] {
            let (rc, out) = read_desc(h.stmt, attr);
            assert_eq!(rc, SQL_SUCCESS);
            assert!(!out.is_null());
            assert_eq!(out, expected);
        }
    }

    #[test]
    fn get_implicit_descriptors_are_distinct() {
        let h = TestHandles::with_env_dbc_stmt();
        let all = [
            read_desc(h.stmt, SQL_ATTR_APP_ROW_DESC).1,
            read_desc(h.stmt, SQL_ATTR_APP_PARAM_DESC).1,
            read_desc(h.stmt, SQL_ATTR_IMP_ROW_DESC).1,
            read_desc(h.stmt, SQL_ATTR_IMP_PARAM_DESC).1,
        ];
        for i in 0..all.len() {
            for j in (i + 1)..all.len() {
                assert_ne!(all[i], all[j], "descriptors {i} and {j} alias");
            }
        }
    }
}
