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
//! succeeding. `SQL_ATTR_APP_ROW_DESC`/`SQL_ATTR_APP_PARAM_DESC` associate an
//! explicitly-allocated descriptor as the statement's active ARD/APD
//! (`associate_descriptor`); the remaining recognized param / descriptor
//! controls are accepted without effect. `SQL_ATTR_PARAMSET_SIZE` accepts the
//! ODBC default of 1 but rejects larger batches, since parameter arrays are not
//! yet consumed and a silent success would execute only the first row.
//! Unrecognized attribute identifiers fail with `HY092`.
//!
//! Each entry point follows the crate's mandatory layering: FFI panic boundary
//! → `unsafe` raw-handle shim → safe core (`README.md`; `num_result_cols.rs`).

use tracing::{debug, error};

use crate::api::odbc_types::{
    SQL_ATTR_APP_PARAM_DESC, SQL_ATTR_APP_ROW_DESC, SQL_ATTR_CONCURRENCY, SQL_ATTR_CURSOR_TYPE,
    SQL_ATTR_IMP_PARAM_DESC, SQL_ATTR_IMP_ROW_DESC, SQL_ATTR_PARAM_BIND_TYPE,
    SQL_ATTR_PARAM_STATUS_PTR, SQL_ATTR_PARAMS_PROCESSED_PTR, SQL_ATTR_PARAMSET_SIZE,
    SQL_ATTR_ROW_ARRAY_SIZE, SQL_ATTR_ROW_BIND_OFFSET_PTR, SQL_ATTR_ROW_BIND_TYPE,
    SQL_ATTR_ROW_STATUS_PTR, SQL_ATTR_ROWS_FETCHED_PTR, SQL_CONCUR_READ_ONLY,
    SQL_CURSOR_FORWARD_ONLY, SQL_ERROR, SQL_INVALID_HANDLE, SQL_SUCCESS, SQL_SUCCESS_WITH_INFO,
    SqlHandle, SqlInteger, SqlPointer, SqlReturn, SqlULen, SqlUSmallInt,
};
use crate::api::sqlstate::{
    DiagMsg, ERR_FUNCTION_SEQUENCE, ERR_INVALID_ATTRIBUTE_IDENTIFIER, ERR_INVALID_ATTRIBUTE_VALUE,
    ERR_INVALID_USE_OF_AUTO_DESC, SQLSTATE_01S02, SQLSTATE_HYC00, post_diag,
};
use crate::api::util::write_if_some;
use crate::error::{free_errors, post_sql_error};
use crate::handles::desc::DescHandle;
use crate::handles::stmt::STMT_STATE_FETCH_IN_PROGRESS;
use crate::handles::{HandleType, StmtHandle, handle_from_raw};

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
        // The rowset controls are read into a fetch's snapshot, so moving them
        // mid-fetch would point it at buffers of the wrong size or shape.
        SQL_ATTR_ROW_ARRAY_SIZE
        | SQL_ATTR_ROWS_FETCHED_PTR
        | SQL_ATTR_ROW_STATUS_PTR
        | SQL_ATTR_ROW_BIND_OFFSET_PTR
        | SQL_ATTR_ROW_BIND_TYPE
            if state.has_state(STMT_STATE_FETCH_IN_PROGRESS) =>
        {
            error!(
                attribute,
                "SQLSetStmtAttrW: a fetch is in progress on this statement"
            );
            post_diag(&mut state, ERR_FUNCTION_SEQUENCE);
            SQL_ERROR
        }
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
        SQL_ATTR_ROW_BIND_OFFSET_PTR => {
            state.row_bind_offset_ptr = value_ptr as *mut SqlULen;
            debug!("SQLSetStmtAttrW: SQL_ATTR_ROW_BIND_OFFSET_PTR set");
            SQL_SUCCESS
        }
        SQL_ATTR_APP_ROW_DESC => match validate_descriptor_association(stmt, stmt.ard, value_ptr) {
            Ok(new_active) => {
                state.active_ard = new_active;
                debug!(?new_active, "SQLSetStmtAttrW: SQL_ATTR_APP_ROW_DESC set");
                SQL_SUCCESS
            }
            Err(diag) => {
                error!(attribute, "SQLSetStmtAttrW: SQL_ATTR_APP_ROW_DESC rejected");
                post_diag(&mut state, diag);
                SQL_ERROR
            }
        },
        SQL_ATTR_APP_PARAM_DESC => match validate_descriptor_association(stmt, stmt.apd, value_ptr)
        {
            Ok(new_active) => {
                state.active_apd = new_active;
                debug!(?new_active, "SQLSetStmtAttrW: SQL_ATTR_APP_PARAM_DESC set");
                SQL_SUCCESS
            }
            Err(diag) => {
                error!(
                    attribute,
                    "SQLSetStmtAttrW: SQL_ATTR_APP_PARAM_DESC rejected"
                );
                post_diag(&mut state, diag);
                SQL_ERROR
            }
        },
        // Recognized attributes accepted without tracking: these param
        // controls have no effect on the implemented forward-only,
        // read-only behavior.
        SQL_ATTR_PARAM_BIND_TYPE | SQL_ATTR_PARAM_STATUS_PTR | SQL_ATTR_PARAMS_PROCESSED_PTR => {
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

/// Validates a new `SQL_ATTR_APP_ROW_DESC`/`SQL_ATTR_APP_PARAM_DESC` value and
/// returns the slot to store in `StmtState::active_ard`/`active_apd`:
/// `own_implicit` is the statement's own permanent implicit descriptor for
/// this role (`stmt.ard` or `stmt.apd`).
///
/// Mirrors msodbcsql's `SQLSetStmtAttr` ARD/APD handling
/// (`sqlcmisc.cpp:3599-3639`) and the ODBC reference's `SQL_ATTR_APP_ROW_DESC`/
/// `SQL_ATTR_APP_PARAM_DESC` entries:
/// - `value_ptr` null or equal to `own_implicit` (the handle originally
///   returned for this statement's ARD/APD) resets to implicit (`Ok(None)`).
/// - Otherwise `value_ptr` must be an explicitly-allocated descriptor
///   (`SQL_DESC_ALLOC_USER`) on the *same connection* as `stmt`, or the call
///   fails: `HY017` if it is some other implicit descriptor (another
///   statement's ARD/APD, or this statement's own IRD/IPD — implicitly
///   allocated descriptors can never be associated except as their own
///   statement's ARD/APD, which is the reset case above), `HY024` if it is
///   explicit but on a different connection.
fn validate_descriptor_association(
    stmt: &StmtHandle,
    own_implicit: SqlHandle,
    value_ptr: SqlPointer,
) -> Result<Option<SqlHandle>, DiagMsg> {
    let value = value_ptr as SqlHandle;
    if value.is_null() || value == own_implicit {
        return Ok(None);
    }

    // SAFETY: trusts the Driver Manager to pass a live descriptor handle, per
    // this crate's FFI-boundary convention (see module docs / README.md).
    let target = unsafe { handle_from_raw::<DescHandle>(value) };
    debug_assert_eq!(
        target.object_type,
        HandleType::Desc,
        "SQLSetStmtAttrW: SQL_ATTR_APP_ROW_DESC/APP_PARAM_DESC value is not a DESC handle"
    );

    if !target.is_explicit() {
        return Err(ERR_INVALID_USE_OF_AUTO_DESC);
    }
    if target.parent_dbc != stmt.parent_dbc {
        return Err(ERR_INVALID_ATTRIBUTE_VALUE);
    }
    Ok(Some(value))
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
        SQL_ATTR_ROW_BIND_OFFSET_PTR => unsafe {
            write_if_some(value_ptr as *mut *mut SqlULen, state.row_bind_offset_ptr);
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
        // ARD/APD report the active association (an explicit descriptor if
        // one was set via SQLSetStmtAttrW, else the implicit default), so
        // they need `state` — unlike IRD/IPD below, which are never
        // swappable and live only on `StmtHandle` itself (set once in
        // `new()`, never reassigned — see that field's doc comment).
        SQL_ATTR_APP_ROW_DESC => unsafe {
            write_if_some(
                value_ptr as *mut SqlHandle,
                state.active_ard.unwrap_or(stmt.ard),
            );
        },
        SQL_ATTR_APP_PARAM_DESC => unsafe {
            write_if_some(
                value_ptr as *mut SqlHandle,
                state.active_apd.unwrap_or(stmt.apd),
            );
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

    /// Previously accepted as a no-op, which silently misplaced every bound
    /// column once a nonzero offset was in play.
    #[test]
    fn set_row_bind_offset_ptr_stored() {
        let h = TestHandles::with_env_dbc_stmt();
        let mut offset: SqlULen = 64;
        let ptr: *mut SqlULen = &mut offset;
        let ret =
            unsafe { sql_set_stmt_attr_w(h.stmt, SQL_ATTR_ROW_BIND_OFFSET_PTR, ptr.cast(), 0) };
        assert_eq!(ret, SQL_SUCCESS);
        let stmt = unsafe { handle_from_raw::<StmtHandle>(h.stmt) };
        assert_eq!(stmt.inner.lock().unwrap().row_bind_offset_ptr, ptr);

        // An attribute that can be set has to be readable back: reading the
        // stored field alone would not have caught a missing getter arm.
        let mut read_back: *mut SqlULen = std::ptr::null_mut();
        let ret = unsafe {
            sql_get_stmt_attr_w(
                h.stmt,
                SQL_ATTR_ROW_BIND_OFFSET_PTR,
                (&mut read_back as *mut *mut SqlULen).cast(),
                0,
                std::ptr::null_mut(),
            )
        };
        assert_eq!(ret, SQL_SUCCESS);
        assert_eq!(read_back, ptr);
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

    /// Regression: querying one of the four implicit descriptor attributes
    /// must clear stale diagnostics from an earlier failed call on this
    /// statement, same as every other `SQLGetStmtAttrW` attribute — ODBC
    /// resets a handle's diagnostic records at the start of every call
    /// except `SQLGetDiagRec`/`SQLGetDiagField`. An earlier implementation
    /// answered these four attributes before the lock (and `free_errors`)
    /// were reached, so a stale diagnostic from a prior failure survived a
    /// subsequent `SQLGetStmtAttrW(SQL_ATTR_APP_PARAM_DESC)` call.
    #[test]
    fn get_descriptor_attribute_clears_stale_diagnostics() {
        let h = TestHandles::with_env_dbc_stmt();
        // Any unrecognized attribute posts a diagnostic on this statement.
        let ret = unsafe {
            sql_get_stmt_attr_w(
                h.stmt,
                0x7FFF,
                std::ptr::null_mut(),
                0,
                std::ptr::null_mut(),
            )
        };
        assert_eq!(ret, SQL_ERROR);
        let stmt = unsafe { handle_from_raw::<StmtHandle>(h.stmt) };
        assert!(!stmt.inner.lock().unwrap().diag_records.is_empty());

        let (rc, _) = read_desc(h.stmt, SQL_ATTR_APP_PARAM_DESC);
        assert_eq!(rc, SQL_SUCCESS);
        assert!(
            stmt.inner.lock().unwrap().diag_records.is_empty(),
            "stale diagnostic from the prior failure was not cleared"
        );
    }

    fn set_desc(stmt: SqlHandle, attribute: SqlInteger, value: SqlHandle) -> SqlReturn {
        unsafe { sql_set_stmt_attr_w(stmt, attribute, value as SqlPointer, 0) }
    }

    #[test]
    fn set_app_row_desc_associates_explicit_descriptor() {
        let mut h = TestHandles::with_env_dbc_stmt();
        let desc = h.alloc_explicit_desc();

        assert_eq!(set_desc(h.stmt, SQL_ATTR_APP_ROW_DESC, desc), SQL_SUCCESS);
        assert_eq!(
            read_desc(h.stmt, SQL_ATTR_APP_ROW_DESC),
            (SQL_SUCCESS, desc)
        );
        // APD is untouched.
        assert_eq!(read_desc(h.stmt, SQL_ATTR_APP_PARAM_DESC).1, h.apd());
    }

    #[test]
    fn set_app_param_desc_associates_explicit_descriptor() {
        let mut h = TestHandles::with_env_dbc_stmt();
        let desc = h.alloc_explicit_desc();

        assert_eq!(set_desc(h.stmt, SQL_ATTR_APP_PARAM_DESC, desc), SQL_SUCCESS);
        assert_eq!(
            read_desc(h.stmt, SQL_ATTR_APP_PARAM_DESC),
            (SQL_SUCCESS, desc)
        );
        // ARD is untouched.
        assert_eq!(read_desc(h.stmt, SQL_ATTR_APP_ROW_DESC).1, h.ard());
    }

    #[test]
    fn reassociation_replaces_previous_explicit_descriptor() {
        let mut h = TestHandles::with_env_dbc_stmt();
        let desc_a = h.alloc_explicit_desc();
        let desc_b = h.alloc_explicit_desc();

        assert_eq!(set_desc(h.stmt, SQL_ATTR_APP_ROW_DESC, desc_a), SQL_SUCCESS);
        assert_eq!(set_desc(h.stmt, SQL_ATTR_APP_ROW_DESC, desc_b), SQL_SUCCESS);
        assert_eq!(
            read_desc(h.stmt, SQL_ATTR_APP_ROW_DESC),
            (SQL_SUCCESS, desc_b)
        );
    }

    #[test]
    fn reset_to_implicit_via_null() {
        let mut h = TestHandles::with_env_dbc_stmt();
        let desc = h.alloc_explicit_desc();
        let implicit_ard = h.ard();

        assert_eq!(set_desc(h.stmt, SQL_ATTR_APP_ROW_DESC, desc), SQL_SUCCESS);
        assert_eq!(
            set_desc(h.stmt, SQL_ATTR_APP_ROW_DESC, SQL_NULL_HANDLE),
            SQL_SUCCESS
        );
        assert_eq!(
            read_desc(h.stmt, SQL_ATTR_APP_ROW_DESC),
            (SQL_SUCCESS, implicit_ard)
        );
    }

    #[test]
    fn reset_to_implicit_via_own_handle() {
        let mut h = TestHandles::with_env_dbc_stmt();
        let desc = h.alloc_explicit_desc();
        let implicit_apd = h.apd();

        assert_eq!(set_desc(h.stmt, SQL_ATTR_APP_PARAM_DESC, desc), SQL_SUCCESS);
        // ODBC spec: passing back the handle originally allocated for this
        // statement's APD is the other legal reset spelling, alongside null.
        assert_eq!(
            set_desc(h.stmt, SQL_ATTR_APP_PARAM_DESC, implicit_apd),
            SQL_SUCCESS
        );
        assert_eq!(
            read_desc(h.stmt, SQL_ATTR_APP_PARAM_DESC),
            (SQL_SUCCESS, implicit_apd)
        );
    }

    /// An implicit descriptor can only ever be reassigned as its own
    /// statement's ARD/APD (the reset case). Another statement's implicit
    /// ARD, or this statement's own IRD/IPD, must be rejected — HY017 per the
    /// ODBC reference ("was an implicitly allocated descriptor handle other
    /// than the handle originally allocated for the ARD or APD").
    #[test]
    fn set_app_row_desc_rejects_another_statements_implicit_descriptor() {
        use crate::api::sqlstate::SQLSTATE_HY017;

        let mut h = TestHandles::with_env_dbc_stmt();
        let other_stmt = h.alloc_extra_stmt();
        let other_ard = read_desc(other_stmt, SQL_ATTR_APP_ROW_DESC).1;

        assert_eq!(
            set_desc(h.stmt, SQL_ATTR_APP_ROW_DESC, other_ard),
            SQL_ERROR
        );
        let stmt = unsafe { handle_from_raw::<StmtHandle>(h.stmt) };
        assert_eq!(
            stmt.inner
                .lock()
                .unwrap()
                .diag_records
                .last()
                .unwrap()
                .sql_state,
            SQLSTATE_HY017
        );
        // Unchanged: still the implicit ARD.
        assert_eq!(read_desc(h.stmt, SQL_ATTR_APP_ROW_DESC).1, h.ard());
    }

    #[test]
    fn set_app_row_desc_rejects_own_ird_as_ard() {
        let h = TestHandles::with_env_dbc_stmt();
        let own_ird = h.ird();

        assert_eq!(set_desc(h.stmt, SQL_ATTR_APP_ROW_DESC, own_ird), SQL_ERROR);
        assert_eq!(read_desc(h.stmt, SQL_ATTR_APP_ROW_DESC).1, h.ard());
    }

    /// `SQLSetStmtAttrW(SQL_ATTR_APP_ROW_DESC/APP_PARAM_DESC)` rejects an
    /// explicit descriptor allocated on a different connection — HY024 per
    /// the ODBC reference.
    #[test]
    fn set_app_row_desc_rejects_cross_connection_descriptor() {
        use crate::api::sqlstate::SQLSTATE_HY024;

        let h = TestHandles::with_env_dbc_stmt();
        let other = h.alloc_other_connection();

        assert_eq!(
            set_desc(h.stmt, SQL_ATTR_APP_ROW_DESC, other.desc),
            SQL_ERROR
        );
        let stmt = unsafe { handle_from_raw::<StmtHandle>(h.stmt) };
        assert_eq!(
            stmt.inner
                .lock()
                .unwrap()
                .diag_records
                .last()
                .unwrap()
                .sql_state,
            SQLSTATE_HY024
        );
        assert_eq!(read_desc(h.stmt, SQL_ATTR_APP_ROW_DESC).1, h.ard());
    }

    /// One explicit descriptor may be associated with more than one
    /// statement at once (AB#47436 scope: "Preserve sound synchronization and
    /// lifetime rules when one explicit descriptor is associated with
    /// multiple statements").
    #[test]
    fn explicit_descriptor_can_be_shared_by_two_statements() {
        let mut h = TestHandles::with_env_dbc_stmt();
        let other_stmt = h.alloc_extra_stmt();
        let desc = h.alloc_explicit_desc();

        assert_eq!(set_desc(h.stmt, SQL_ATTR_APP_ROW_DESC, desc), SQL_SUCCESS);
        assert_eq!(
            set_desc(other_stmt, SQL_ATTR_APP_ROW_DESC, desc),
            SQL_SUCCESS
        );

        assert_eq!(
            read_desc(h.stmt, SQL_ATTR_APP_ROW_DESC),
            (SQL_SUCCESS, desc)
        );
        assert_eq!(
            read_desc(other_stmt, SQL_ATTR_APP_ROW_DESC),
            (SQL_SUCCESS, desc)
        );
    }

    /// `SQLFreeHandle(SQL_HANDLE_DESC)` on an explicit descriptor currently
    /// associated with one or more statements resets every one of them back
    /// to their own implicit descriptor, rather than leaving a dangling
    /// pointer — mirrors msodbcsql's `FreeDesc(pADesc, NULL, ...)`.
    #[test]
    fn freeing_associated_descriptor_resets_statements_to_implicit() {
        let mut h = TestHandles::with_env_dbc_stmt();
        let other_stmt = h.alloc_extra_stmt();
        let desc = h.alloc_explicit_desc();
        let implicit_ard = h.ard();
        let other_implicit_ard = read_desc(other_stmt, SQL_ATTR_APP_ROW_DESC).1;

        assert_eq!(set_desc(h.stmt, SQL_ATTR_APP_ROW_DESC, desc), SQL_SUCCESS);
        assert_eq!(
            set_desc(other_stmt, SQL_ATTR_APP_ROW_DESC, desc),
            SQL_SUCCESS
        );

        assert_eq!(h.free_explicit_desc(desc), SQL_SUCCESS);

        assert_eq!(
            read_desc(h.stmt, SQL_ATTR_APP_ROW_DESC),
            (SQL_SUCCESS, implicit_ard)
        );
        assert_eq!(
            read_desc(other_stmt, SQL_ATTR_APP_ROW_DESC),
            (SQL_SUCCESS, other_implicit_ard)
        );
    }

    /// Freeing a statement that currently has an explicit descriptor
    /// associated does not touch the descriptor itself: it is DBC-owned, not
    /// STMT-owned, so it stays valid and can be reused on another statement.
    #[test]
    fn association_survives_statement_free_and_can_be_reused() {
        let mut h = TestHandles::with_env_dbc_stmt();
        let desc = h.alloc_explicit_desc();
        let stmt_to_free = h.alloc_extra_stmt();
        assert_eq!(
            set_desc(stmt_to_free, SQL_ATTR_APP_ROW_DESC, desc),
            SQL_SUCCESS
        );

        assert_eq!(h.free_extra_stmt(stmt_to_free), SQL_SUCCESS);

        assert_eq!(set_desc(h.stmt, SQL_ATTR_APP_ROW_DESC, desc), SQL_SUCCESS);
        assert_eq!(
            read_desc(h.stmt, SQL_ATTR_APP_ROW_DESC),
            (SQL_SUCCESS, desc)
        );
    }
}
