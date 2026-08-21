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
//! succeeding; `SQL_ATTR_CURSOR_SCROLLABLE` is the same setting seen from the
//! other side and behaves identically. `SQL_ATTR_QUERY_TIMEOUT` is stored and
//! clamped to [`MAX_QUERY_TIMEOUT`], matching msodbcsql, which reports `01S02`
//! rather than rejecting an over-large request; enforcing the stored timeout
//! against a running statement is tracked separately. `SQL_ATTR_MAX_ROWS` is
//! stored and genuinely enforced by the fetch path. Other recognized statement
//! attributes (param / cursor / descriptor controls) are stored and
//! round-tripped without effect, because msodbcsql reports back whatever was
//! written; the handful whose reported value msodbcsql pins regardless of the
//! request (`SQL_ATTR_MAX_LENGTH`, `SQL_ATTR_KEYSET_SIZE`,
//! `SQL_ATTR_SIMULATE_CURSOR`) substitute and warn with `01S02`.
//! `SQL_ATTR_PARAMSET_SIZE` accepts the ODBC default of 1 but rejects larger
//! batches, since parameter arrays are not yet consumed and a silent success
//! would execute only the first row. Unrecognized attribute identifiers fail
//! with `HY092`.
//!
//! Each entry point follows the crate's mandatory layering: FFI panic boundary
//! → `unsafe` raw-handle shim → safe core (`README.md`; `num_result_cols.rs`).

use tracing::{debug, error};

use crate::api::attributes::{AttrOp, AttrScope, unimplemented_attr_diag};
use crate::api::odbc_types::{
    MAX_QUERY_TIMEOUT, MSODBCSQL_MAX_LENGTH, SQL_ATTR_APP_PARAM_DESC, SQL_ATTR_APP_ROW_DESC,
    SQL_ATTR_CONCURRENCY, SQL_ATTR_CURSOR_SCROLLABLE, SQL_ATTR_CURSOR_SENSITIVITY,
    SQL_ATTR_CURSOR_TYPE, SQL_ATTR_IMP_PARAM_DESC, SQL_ATTR_IMP_ROW_DESC, SQL_ATTR_KEYSET_SIZE,
    SQL_ATTR_MAX_LENGTH, SQL_ATTR_MAX_ROWS, SQL_ATTR_PARAMSET_SIZE, SQL_ATTR_QUERY_TIMEOUT,
    SQL_ATTR_ROW_ARRAY_SIZE, SQL_ATTR_ROW_BIND_OFFSET_PTR, SQL_ATTR_ROW_BIND_TYPE,
    SQL_ATTR_ROW_NUMBER, SQL_ATTR_ROW_STATUS_PTR, SQL_ATTR_ROWS_FETCHED_PTR,
    SQL_ATTR_SIMULATE_CURSOR, SQL_CONCUR_READ_ONLY, SQL_CURSOR_FORWARD_ONLY, SQL_ERROR,
    SQL_INSENSITIVE, SQL_INVALID_HANDLE, SQL_NONSCROLLABLE, SQL_SC_UNIQUE, SQL_SUCCESS,
    SQL_SUCCESS_WITH_INFO, SqlHandle, SqlInteger, SqlPointer, SqlReturn, SqlULen, SqlUSmallInt,
};
use crate::api::sqlstate::{
    ERR_FUNCTION_SEQUENCE, ERR_INVALID_ATTRIBUTE_VALUE, ERR_INVALID_CURSOR_STATE, SQLSTATE_01S02,
    SQLSTATE_HYC00, WARN_OPTION_VALUE_CHANGED, post_diag,
};
use crate::api::util::write_if_some;
use crate::error::{free_errors, post_sql_error};
use crate::handles::stmt::STMT_STATE_FETCH_IN_PROGRESS;
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
        SQL_ATTR_MAX_ROWS => {
            // Enforced, not merely stored: `fetch_rows_next` stops the cursor
            // once this many rows have been returned from the current result
            // set, matching msodbcsql.
            state.max_rows = value_ptr as SqlULen;
            debug!(
                max_rows = state.max_rows,
                "SQLSetStmtAttrW: SQL_ATTR_MAX_ROWS set"
            );
            SQL_SUCCESS
        }
        SQL_ATTR_MAX_LENGTH => {
            // msodbcsql substitutes MSODBCSQL_MAX_LENGTH for any non-zero
            // request and reports 01S02; the cap is then never applied, and a
            // longer value still comes back whole. Mirror the reported value
            // and the warning so an application that inspects either sees the
            // same thing from both drivers.
            let requested = value_ptr as SqlULen;
            let effective = if requested == 0 {
                0
            } else {
                MSODBCSQL_MAX_LENGTH
            };
            state.inert_attrs.set(SQL_ATTR_MAX_LENGTH, effective);
            if requested == effective {
                SQL_SUCCESS
            } else {
                post_diag(&mut state, WARN_OPTION_VALUE_CHANGED);
                SQL_SUCCESS_WITH_INFO
            }
        }
        SQL_ATTR_KEYSET_SIZE => {
            // Keyset-driven cursors are not implemented, so any non-zero keyset
            // is refused the same way msodbcsql refuses it: the stored value
            // stays 0 and the caller is told with 01S02.
            if value_ptr as SqlULen == 0 {
                SQL_SUCCESS
            } else {
                post_diag(&mut state, WARN_OPTION_VALUE_CHANGED);
                SQL_SUCCESS_WITH_INFO
            }
        }
        SQL_ATTR_SIMULATE_CURSOR => {
            // msodbcsql reports SQL_SC_UNIQUE and accepts only that value.
            if value_ptr as SqlULen == SQL_SC_UNIQUE {
                SQL_SUCCESS
            } else {
                post_diag(&mut state, WARN_OPTION_VALUE_CHANGED);
                SQL_SUCCESS_WITH_INFO
            }
        }
        SQL_ATTR_CURSOR_SCROLLABLE => {
            // Scrollability is the cursor type seen from the other side:
            // msodbcsql moves SQL_ATTR_CURSOR_TYPE off forward-only when a
            // caller asks for a scrollable cursor. This driver has no scrollable
            // cursor, so a request is substituted and reported exactly as
            // SQL_ATTR_CURSOR_TYPE already does.
            if value_ptr as SqlULen == SQL_NONSCROLLABLE {
                SQL_SUCCESS
            } else {
                post_sql_error(
                    &mut state,
                    SQLSTATE_01S02,
                    0,
                    "Scrollable cursors not supported; substituted SQL_NONSCROLLABLE",
                );
                SQL_SUCCESS_WITH_INFO
            }
        }
        SQL_ATTR_CURSOR_SENSITIVITY => {
            // SQL_UNSPECIFIED (0) means "whatever the driver does", so the get
            // path must answer with the driver's actual sensitivity rather than
            // the request. msodbcsql resolves it to SQL_INSENSITIVE silently —
            // no 01S02 — and stores any other value verbatim.
            let requested = value_ptr as SqlULen;
            let effective = if requested == 0 {
                SQL_INSENSITIVE
            } else {
                requested
            };
            state
                .inert_attrs
                .set(SQL_ATTR_CURSOR_SENSITIVITY, effective);
            SQL_SUCCESS
        }
        // Recognized attributes stored and round-tripped without effect: these
        // param / cursor / descriptor controls do not change the implemented
        // forward-only, read-only behavior, but msodbcsql reports back whatever
        // was written, so silently discarding the value would diverge on the
        // very next get.
        attribute if state.inert_attrs.set(attribute, value_ptr as SqlULen) => {
            debug!(
                attribute,
                "SQLSetStmtAttrW: attribute stored without effect"
            );
            SQL_SUCCESS
        }
        SQL_ATTR_APP_ROW_DESC | SQL_ATTR_APP_PARAM_DESC => {
            debug!(attribute, "SQLSetStmtAttrW: attribute accepted as no-op");
            SQL_SUCCESS
        }
        _ => {
            post_diag(
                &mut state,
                unimplemented_attr_diag(AttrScope::Stmt, AttrOp::Set, attribute),
            );
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
        SQL_ATTR_ROW_BIND_OFFSET_PTR => unsafe {
            write_if_some(value_ptr as *mut *mut SqlULen, state.row_bind_offset_ptr);
        },
        SQL_ATTR_QUERY_TIMEOUT => unsafe {
            write_if_some(value_ptr as *mut SqlULen, state.query_timeout as SqlULen);
        },
        SQL_ATTR_MAX_ROWS => unsafe {
            write_if_some(value_ptr as *mut SqlULen, state.max_rows);
        },
        SQL_ATTR_ROW_NUMBER => {
            // Get-only, and only answerable while the cursor sits on a row:
            // msodbcsql returns 24000 on a statement with no cursor, on an
            // executed-but-not-yet-fetched cursor, and again once the rowset is
            // exhausted or closed. Positioned, it answers 0 rather than an
            // ordinal, because a forward-only cursor keeps no rowset origin to
            // number rows against.
            if !state.row_positioned {
                post_diag(&mut state, ERR_INVALID_CURSOR_STATE);
                return SQL_ERROR;
            }
            unsafe {
                write_if_some(value_ptr as *mut SqlULen, 0);
            }
        }
        SQL_ATTR_CURSOR_SCROLLABLE => unsafe {
            // Derived from the cursor type rather than stored, so the pair can
            // never disagree.
            write_if_some(value_ptr as *mut SqlULen, SQL_NONSCROLLABLE);
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
        // The four implicit descriptors (ARD/APD/IRD/IPD). They live on
        // `StmtHandle` itself, not behind `inner` (set once in `new()`,
        // never reassigned — see that field's doc comment), so nothing
        // here needs the lock beyond the diagnostics reset above, which
        // every attribute on this call shares regardless of which one was
        // requested.
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
        // Attributes the set path stores without effect share the table that
        // holds their measured defaults, so a get before any set answers what
        // msodbcsql answers. Anything not in it is genuinely unhandled.
        _ => match state.inert_attrs.get(attribute) {
            Some(value) => unsafe {
                write_if_some(value_ptr as *mut SqlULen, value);
            },
            None => {
                post_diag(
                    &mut state,
                    unimplemented_attr_diag(AttrScope::Stmt, AttrOp::Get, attribute),
                );
                return SQL_ERROR;
            }
        },
    }

    SQL_SUCCESS
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::odbc_types::{
        SQL_ATTR_ASYNC_ENABLE, SQL_ATTR_ENABLE_AUTO_IPD, SQL_ATTR_FETCH_BOOKMARK_PTR,
        SQL_ATTR_METADATA_ID, SQL_ATTR_NOSCAN, SQL_ATTR_PARAM_BIND_OFFSET_PTR,
        SQL_ATTR_PARAM_BIND_TYPE, SQL_ATTR_PARAM_OPERATION_PTR, SQL_ATTR_PARAM_STATUS_PTR,
        SQL_ATTR_PARAMS_PROCESSED_PTR, SQL_ATTR_RETRIEVE_DATA, SQL_ATTR_ROW_BIND_OFFSET_PTR,
        SQL_ATTR_ROW_OPERATION_PTR, SQL_ATTR_USE_BOOKMARKS, SQL_BIND_BY_COLUMN, SQL_NULL_HANDLE,
        SQL_RD_ON, SQL_ROWSET_SIZE, SqlLen,
    };
    use crate::api::sqlstate::{SQLSTATE_24000, SQLSTATE_HY092};
    use crate::handles::handle_from_raw;
    use crate::handles::stmt::InertStmtAttrs;
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

    /// Reads the SQLSTATE of the newest diagnostic on a statement. Must be
    /// called before any `sql_get_stmt_attr_w` helper, which frees diagnostics
    /// on entry.
    fn stmt_sql_state(stmt: SqlHandle) -> [u8; 5] {
        let stmt = unsafe { handle_from_raw::<StmtHandle>(stmt) };
        let state = stmt.inner.lock().unwrap();
        state.diag_records[0].sql_state
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

    /// The rowset pointers are the only statement attributes whose value *is* a
    /// pointer, so the get path has to write a pointer-width slot rather than
    /// the `SqlULen` every other attribute uses. Round-tripped through the
    /// public entry points because an application that sets a rowset pointer
    /// and reads it back must get the same address, not a truncated one.
    #[test]
    fn rowset_pointers_round_trip_through_the_get_path() {
        let h = TestHandles::with_env_dbc_stmt();

        let mut rows_fetched: SqlULen = 0;
        let fetched_ptr = &mut rows_fetched as *mut SqlULen;
        let mut status: SqlUSmallInt = 0;
        let status_ptr = &mut status as *mut SqlUSmallInt;

        unsafe {
            assert_eq!(
                sql_set_stmt_attr_w(h.stmt, SQL_ATTR_ROWS_FETCHED_PTR, fetched_ptr.cast(), 0),
                SQL_SUCCESS
            );
            assert_eq!(
                sql_set_stmt_attr_w(h.stmt, SQL_ATTR_ROW_STATUS_PTR, status_ptr.cast(), 0),
                SQL_SUCCESS
            );

            let mut out_fetched: *mut SqlULen = std::ptr::null_mut();
            assert_eq!(
                sql_get_stmt_attr_w(
                    h.stmt,
                    SQL_ATTR_ROWS_FETCHED_PTR,
                    (&raw mut out_fetched).cast(),
                    0,
                    std::ptr::null_mut(),
                ),
                SQL_SUCCESS
            );
            assert_eq!(out_fetched, fetched_ptr);

            let mut out_status: *mut SqlUSmallInt = std::ptr::null_mut();
            assert_eq!(
                sql_get_stmt_attr_w(
                    h.stmt,
                    SQL_ATTR_ROW_STATUS_PTR,
                    (&raw mut out_status).cast(),
                    0,
                    std::ptr::null_mut(),
                ),
                SQL_SUCCESS
            );
            assert_eq!(out_status, status_ptr);
        }
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
        assert_eq!(stmt_sql_state(h.stmt), SQLSTATE_HY092);
    }

    /// An identifier msodbcsql implements but this driver does not must report
    /// `HYC00`, not `HY092`: a caller probing for the feature has to be able to
    /// tell "unavailable" from "not an attribute". `SQL_SOPT_SS_DEFER_PREPARE`
    /// (1232) is a statement attribute msodbcsql honors.
    #[test]
    fn set_attribute_known_to_msodbcsql_reports_not_implemented() {
        let h = TestHandles::with_env_dbc_stmt();
        let ret = unsafe { sql_set_stmt_attr_w(h.stmt, 1232, 0 as SqlPointer, 0) };
        assert_eq!(ret, SQL_ERROR);
        assert_eq!(stmt_sql_state(h.stmt), SQLSTATE_HYC00);
    }

    #[test]
    fn get_attribute_known_to_msodbcsql_reports_not_implemented() {
        let h = TestHandles::with_env_dbc_stmt();
        let mut out: SqlULen = 7;
        let ret = unsafe {
            sql_get_stmt_attr_w(
                h.stmt,
                1232,
                (&mut out as *mut SqlULen).cast(),
                0,
                std::ptr::null_mut(),
            )
        };
        assert_eq!(ret, SQL_ERROR);
        assert_eq!(stmt_sql_state(h.stmt), SQLSTATE_HYC00);
    }

    /// A connection attribute aimed at a statement stays `HY092`; the tables are
    /// scope-keyed precisely so this does not soften to `HYC00`.
    /// `SQL_ATTR_CURRENT_CATALOG` (109) is connection-only.
    #[test]
    fn connection_attribute_on_a_statement_stays_invalid() {
        let h = TestHandles::with_env_dbc_stmt();
        let ret = unsafe { sql_set_stmt_attr_w(h.stmt, 109, 0 as SqlPointer, 0) };
        assert_eq!(ret, SQL_ERROR);
        assert_eq!(stmt_sql_state(h.stmt), SQLSTATE_HY092);
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

    // ---- S4: remaining statement attributes -------------------------------
    //
    // Every expectation below was measured against msodbcsql 18 rather than
    // read out of the ODBC headers; see `docs/attributes_plan.md` §8.

    /// Sets an attribute and returns the return code.
    fn set_attr(stmt: SqlHandle, attribute: SqlInteger, value: SqlULen) -> SqlReturn {
        unsafe { sql_set_stmt_attr_w(stmt, attribute, value as SqlPointer, 0) }
    }

    /// Reads an attribute back, asserting the get itself succeeded.
    fn get_attr(stmt: SqlHandle, attribute: SqlInteger) -> SqlULen {
        let mut out: SqlULen = 0;
        let rc = unsafe {
            sql_get_stmt_attr_w(
                stmt,
                attribute,
                (&mut out as *mut SqlULen).cast(),
                0,
                std::ptr::null_mut(),
            )
        };
        assert_eq!(rc, SQL_SUCCESS, "get of attribute {attribute} failed");
        out
    }

    /// The msodbcsql defaults, restated independently of the table the driver
    /// reads them from so the two can be compared rather than agreeing by
    /// construction.
    const MEASURED_DEFAULTS: &[(SqlInteger, SqlULen)] = &[
        (SQL_ATTR_NOSCAN, 0),
        (SQL_ATTR_MAX_LENGTH, 0),
        (SQL_ATTR_ASYNC_ENABLE, 0),
        (SQL_ATTR_KEYSET_SIZE, 0),
        (SQL_ROWSET_SIZE, 1),
        (SQL_ATTR_SIMULATE_CURSOR, SQL_SC_UNIQUE),
        (SQL_ATTR_RETRIEVE_DATA, SQL_RD_ON),
        (SQL_ATTR_USE_BOOKMARKS, 0),
        (SQL_ATTR_ENABLE_AUTO_IPD, 0),
        (SQL_ATTR_FETCH_BOOKMARK_PTR, 0),
        (SQL_ATTR_PARAM_BIND_OFFSET_PTR, 0),
        (SQL_ATTR_PARAM_BIND_TYPE, 0),
        (SQL_ATTR_PARAM_OPERATION_PTR, 0),
        (SQL_ATTR_PARAM_STATUS_PTR, 0),
        (SQL_ATTR_PARAMS_PROCESSED_PTR, 0),
        (SQL_ATTR_ROW_BIND_OFFSET_PTR, 0),
        (SQL_ATTR_ROW_OPERATION_PTR, 0),
        (SQL_ATTR_METADATA_ID, 0),
        (SQL_ATTR_CURSOR_SENSITIVITY, SQL_INSENSITIVE),
    ];

    /// A get before any set must answer the measured default, because an
    /// application that reads an attribute to decide whether to change it would
    /// otherwise take a different branch on each driver.
    #[test]
    fn inert_attribute_defaults_match_msodbcsql() {
        let h = TestHandles::with_env_dbc_stmt();
        for &(attribute, expected) in MEASURED_DEFAULTS {
            assert_eq!(get_attr(h.stmt, attribute), expected, "attr {attribute}");
        }
    }

    /// Guards [`MEASURED_DEFAULTS`] against drift: adding an identifier to the
    /// store without measuring its msodbcsql default would otherwise ship an
    /// invented value that no test ever looks at.
    #[test]
    fn every_inert_identifier_has_a_measured_default() {
        for attribute in InertStmtAttrs::identifiers() {
            assert!(
                MEASURED_DEFAULTS.iter().any(|&(id, _)| id == attribute),
                "attribute {attribute} is stored but has no asserted default"
            );
        }
    }

    /// The set/get asymmetry this slice closes: these were previously accepted
    /// and discarded, so a read-back reported a stale default.
    #[test]
    fn inert_attributes_round_trip_the_written_value() {
        let h = TestHandles::with_env_dbc_stmt();
        for (attribute, value) in [
            (SQL_ATTR_NOSCAN, 1),
            (SQL_ATTR_ASYNC_ENABLE, 1),
            (SQL_ROWSET_SIZE, 10),
            (SQL_ATTR_RETRIEVE_DATA, 0),
            (SQL_ATTR_USE_BOOKMARKS, 2),
            (SQL_ATTR_ENABLE_AUTO_IPD, 1),
            (SQL_ATTR_FETCH_BOOKMARK_PTR, 0x1234),
            (SQL_ATTR_PARAM_BIND_OFFSET_PTR, 0x2345),
            (SQL_ATTR_PARAM_BIND_TYPE, 16),
            (SQL_ATTR_PARAM_OPERATION_PTR, 0x3456),
            (SQL_ATTR_PARAM_STATUS_PTR, 0x4567),
            (SQL_ATTR_PARAMS_PROCESSED_PTR, 0x5678),
            (SQL_ATTR_ROW_BIND_OFFSET_PTR, 0x6789),
            (SQL_ATTR_ROW_OPERATION_PTR, 0x789a),
            (SQL_ATTR_METADATA_ID, 1),
        ] {
            assert_eq!(
                set_attr(h.stmt, attribute, value),
                SQL_SUCCESS,
                "set {attribute}"
            );
            assert_eq!(get_attr(h.stmt, attribute), value, "readback {attribute}");
        }
    }

    /// `SQL_ATTR_PARAM_BIND_OFFSET_PTR` holds a *pointer to* the offset, so it
    /// is dereferenced at execute time rather than read at set time. An
    /// application can therefore step a binding across a buffer by writing one
    /// `SQLLEN` between executions.
    #[test]
    fn param_bind_offset_is_read_through_the_stored_pointer() {
        let mut attrs = InertStmtAttrs::default();
        assert_eq!(
            unsafe { attrs.param_bind_offset() },
            0,
            "unset is no offset"
        );

        let mut offset: SqlLen = 24;
        let slot = &raw mut offset;
        attrs.set(SQL_ATTR_PARAM_BIND_OFFSET_PTR, slot as SqlULen);
        assert_eq!(unsafe { attrs.param_bind_offset() }, 24);

        // Written after the set: the value is read at execute time, not
        // captured when the attribute was assigned.
        unsafe { slot.write(-8) };
        assert_eq!(unsafe { attrs.param_bind_offset() }, -8);
    }

    /// `SQL_ROWSET_SIZE` shares a name with `SQL_ATTR_ROW_ARRAY_SIZE` but not a
    /// slot: msodbcsql keeps them independent, so treating one as an alias of
    /// the other would silently resize an application's rowset.
    #[test]
    fn rowset_size_is_independent_of_row_array_size() {
        let h = TestHandles::with_env_dbc_stmt();
        assert_eq!(set_attr(h.stmt, SQL_ROWSET_SIZE, 7), SQL_SUCCESS);
        assert_eq!(get_attr(h.stmt, SQL_ROWSET_SIZE), 7);
        assert_eq!(get_attr(h.stmt, SQL_ATTR_ROW_ARRAY_SIZE), 1);

        assert_eq!(set_attr(h.stmt, SQL_ATTR_ROW_ARRAY_SIZE, 5), SQL_SUCCESS);
        assert_eq!(get_attr(h.stmt, SQL_ATTR_ROW_ARRAY_SIZE), 5);
        assert_eq!(get_attr(h.stmt, SQL_ROWSET_SIZE), 7);
    }

    /// A non-zero `SQL_ATTR_MAX_LENGTH` is substituted rather than honored, and
    /// the substitution is announced. The cap is cosmetic on msodbcsql too — a
    /// longer value still comes back whole — so only the reported state matters.
    #[test]
    fn max_length_substitutes_and_warns() {
        let h = TestHandles::with_env_dbc_stmt();
        assert_eq!(
            set_attr(h.stmt, SQL_ATTR_MAX_LENGTH, 10),
            SQL_SUCCESS_WITH_INFO
        );
        assert_eq!(stmt_sql_state(h.stmt), SQLSTATE_01S02);
        assert_eq!(get_attr(h.stmt, SQL_ATTR_MAX_LENGTH), MSODBCSQL_MAX_LENGTH);
    }

    #[test]
    fn max_length_zero_is_accepted_silently() {
        let h = TestHandles::with_env_dbc_stmt();
        assert_eq!(set_attr(h.stmt, SQL_ATTR_MAX_LENGTH, 0), SQL_SUCCESS);
        assert_eq!(get_attr(h.stmt, SQL_ATTR_MAX_LENGTH), 0);
    }

    /// Setting the already-substituted value is not itself a change, so it must
    /// not warn a second time.
    #[test]
    fn max_length_at_the_substituted_value_is_accepted_silently() {
        let h = TestHandles::with_env_dbc_stmt();
        let rc = set_attr(h.stmt, SQL_ATTR_MAX_LENGTH, MSODBCSQL_MAX_LENGTH);
        assert_eq!(rc, SQL_SUCCESS);
        assert_eq!(get_attr(h.stmt, SQL_ATTR_MAX_LENGTH), MSODBCSQL_MAX_LENGTH);
    }

    #[test]
    fn keyset_size_zero_accepted_nonzero_warns_and_stays_zero() {
        let h = TestHandles::with_env_dbc_stmt();
        assert_eq!(set_attr(h.stmt, SQL_ATTR_KEYSET_SIZE, 0), SQL_SUCCESS);

        let rc = set_attr(h.stmt, SQL_ATTR_KEYSET_SIZE, 100);
        assert_eq!(rc, SQL_SUCCESS_WITH_INFO);
        assert_eq!(stmt_sql_state(h.stmt), SQLSTATE_01S02);
        assert_eq!(get_attr(h.stmt, SQL_ATTR_KEYSET_SIZE), 0);
    }

    #[test]
    fn simulate_cursor_accepts_only_unique() {
        let h = TestHandles::with_env_dbc_stmt();
        assert_eq!(
            set_attr(h.stmt, SQL_ATTR_SIMULATE_CURSOR, SQL_SC_UNIQUE),
            SQL_SUCCESS
        );

        let rc = set_attr(h.stmt, SQL_ATTR_SIMULATE_CURSOR, 0);
        assert_eq!(rc, SQL_SUCCESS_WITH_INFO);
        assert_eq!(stmt_sql_state(h.stmt), SQLSTATE_01S02);
        assert_eq!(get_attr(h.stmt, SQL_ATTR_SIMULATE_CURSOR), SQL_SC_UNIQUE);
    }

    /// `SQL_UNSPECIFIED` means "whatever the driver does", so the get must
    /// resolve it to the driver's actual sensitivity instead of echoing the
    /// request. msodbcsql resolves it silently, with no `01S02`.
    #[test]
    fn cursor_sensitivity_unspecified_resolves_to_insensitive() {
        let h = TestHandles::with_env_dbc_stmt();
        assert_eq!(
            set_attr(h.stmt, SQL_ATTR_CURSOR_SENSITIVITY, 0),
            SQL_SUCCESS
        );
        assert_eq!(
            get_attr(h.stmt, SQL_ATTR_CURSOR_SENSITIVITY),
            SQL_INSENSITIVE
        );
    }

    #[test]
    fn cursor_sensitivity_explicit_values_round_trip() {
        let h = TestHandles::with_env_dbc_stmt();
        for value in [1, 2] {
            assert_eq!(
                set_attr(h.stmt, SQL_ATTR_CURSOR_SENSITIVITY, value),
                SQL_SUCCESS
            );
            assert_eq!(get_attr(h.stmt, SQL_ATTR_CURSOR_SENSITIVITY), value);
        }
    }

    #[test]
    fn cursor_scrollable_nonscrollable_accepted() {
        let h = TestHandles::with_env_dbc_stmt();
        let rc = set_attr(h.stmt, SQL_ATTR_CURSOR_SCROLLABLE, SQL_NONSCROLLABLE);
        assert_eq!(rc, SQL_SUCCESS);
        assert_eq!(
            get_attr(h.stmt, SQL_ATTR_CURSOR_SCROLLABLE),
            SQL_NONSCROLLABLE
        );
    }

    /// Scrollability is the cursor type seen from the other side, so a request
    /// for a scrollable cursor is refused exactly as `SQL_ATTR_CURSOR_TYPE`
    /// refuses a non-forward-only type: substituted, and announced.
    #[test]
    fn cursor_scrollable_request_is_substituted_like_cursor_type() {
        let h = TestHandles::with_env_dbc_stmt();
        assert_eq!(
            set_attr(h.stmt, SQL_ATTR_CURSOR_SCROLLABLE, 1),
            SQL_SUCCESS_WITH_INFO
        );
        assert_eq!(stmt_sql_state(h.stmt), SQLSTATE_01S02);
        assert_eq!(
            get_attr(h.stmt, SQL_ATTR_CURSOR_SCROLLABLE),
            SQL_NONSCROLLABLE
        );
        assert_eq!(
            get_attr(h.stmt, SQL_ATTR_CURSOR_TYPE),
            SQL_CURSOR_FORWARD_ONLY
        );
    }

    /// The pair can never disagree, because scrollability is derived from the
    /// cursor type rather than stored alongside it.
    #[test]
    fn cursor_scrollable_stays_consistent_with_cursor_type() {
        let h = TestHandles::with_env_dbc_stmt();
        assert_eq!(
            set_attr(h.stmt, SQL_ATTR_CURSOR_TYPE, 2),
            SQL_SUCCESS_WITH_INFO
        );
        assert_eq!(
            get_attr(h.stmt, SQL_ATTR_CURSOR_SCROLLABLE),
            SQL_NONSCROLLABLE
        );
    }

    #[test]
    fn max_rows_defaults_to_unlimited_and_round_trips() {
        let h = TestHandles::with_env_dbc_stmt();
        assert_eq!(get_attr(h.stmt, SQL_ATTR_MAX_ROWS), 0);
        assert_eq!(set_attr(h.stmt, SQL_ATTR_MAX_ROWS, 3), SQL_SUCCESS);
        assert_eq!(get_attr(h.stmt, SQL_ATTR_MAX_ROWS), 3);
        assert_eq!(set_attr(h.stmt, SQL_ATTR_MAX_ROWS, 0), SQL_SUCCESS);
        assert_eq!(get_attr(h.stmt, SQL_ATTR_MAX_ROWS), 0);
    }

    /// `SQL_ATTR_ROW_NUMBER` is get-only; the set falls through to the
    /// recognition table, which knows msodbcsql rejects it.
    #[test]
    fn row_number_cannot_be_set() {
        let h = TestHandles::with_env_dbc_stmt();
        assert_eq!(set_attr(h.stmt, SQL_ATTR_ROW_NUMBER, 1), SQL_ERROR);
        assert_eq!(stmt_sql_state(h.stmt), SQLSTATE_HY092);
    }

    /// Answerable only while the cursor sits on a row. A fresh statement has no
    /// cursor, so msodbcsql reports `24000` rather than a position.
    #[test]
    fn row_number_without_a_cursor_is_invalid_cursor_state() {
        let h = TestHandles::with_env_dbc_stmt();
        let mut out: SqlULen = 0;
        let rc = unsafe {
            sql_get_stmt_attr_w(
                h.stmt,
                SQL_ATTR_ROW_NUMBER,
                (&mut out as *mut SqlULen).cast(),
                0,
                std::ptr::null_mut(),
            )
        };
        assert_eq!(rc, SQL_ERROR);
        assert_eq!(stmt_sql_state(h.stmt), SQLSTATE_24000);
    }

    /// Positioned, msodbcsql answers 0 rather than an ordinal: a forward-only
    /// cursor keeps no rowset origin to number rows against.
    #[test]
    fn row_number_on_a_positioned_cursor_is_zero() {
        let h = TestHandles::with_env_dbc_stmt();
        {
            let stmt = unsafe { handle_from_raw::<StmtHandle>(h.stmt) };
            stmt.inner.lock().unwrap().row_positioned = true;
        }
        assert_eq!(get_attr(h.stmt, SQL_ATTR_ROW_NUMBER), 0);
    }

    /// The inert table is a linear scan over identifiers, so a duplicate would
    /// make the second entry unreachable and pin its attribute to the first
    /// one's value.
    #[test]
    fn inert_attribute_identifiers_are_unique() {
        let h = TestHandles::with_env_dbc_stmt();
        let stmt = unsafe { handle_from_raw::<StmtHandle>(h.stmt) };
        let state = stmt.inner.lock().unwrap();
        let ids: Vec<SqlInteger> = InertStmtAttrs::identifiers().collect();
        assert!(!ids.is_empty());
        for (i, attribute) in ids.iter().enumerate() {
            assert!(
                !ids[..i].contains(attribute),
                "duplicate inert attribute {attribute}"
            );
            assert!(state.inert_attrs.get(*attribute).is_some());
        }
    }
}
