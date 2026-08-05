// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Data-at-execution (DAE) parameter streaming — `SQLParamData` / `SQLPutData`.
//!
//! When an application binds a parameter whose length/indicator buffer holds
//! `SQL_DATA_AT_EXEC` or `SQL_LEN_DATA_AT_EXEC(n)`, the value is not in the
//! bound buffer. `SQLExecute`/`SQLExecDirect` return `SQL_NEED_DATA`, and the
//! application then drives this loop:
//!
//! ```text
//! while SQLParamData(&token) == SQL_NEED_DATA {
//!     SQLPutData(chunk, len);   // one or more times
//! }
//! ```
//!
//! The token handed back is the `ParameterValuePtr` supplied at bind time,
//! which the application uses to identify which parameter is being asked for.
//! Once every hungry parameter has been fed, the final `SQLParamData` performs
//! the execution that was deferred at staging time.

use tracing::{debug, error};

use super::execute::{Execution, run_execution};
use super::sqlstate::*;
use crate::api::exec_common::build_named_params_with_dae;
use crate::api::odbc_types::{
    SQL_ERROR, SQL_INVALID_HANDLE, SQL_NEED_DATA, SQL_NTS, SQL_NULL_DATA, SqlHandle, SqlLen,
    SqlPointer, SqlReturn,
};
use crate::error::{free_errors, post_sql_error};
use crate::handles::{StmtHandle, handle_from_raw};

/// Implements `SQLParamData`.
///
/// # Safety
/// - `statement_handle` must be a valid `StmtHandle` or null.
/// - `value_ptr_ptr`, if non-null, must point to one writable `SqlPointer`.
pub(crate) unsafe fn sql_param_data(
    statement_handle: SqlHandle,
    value_ptr_ptr: *mut SqlPointer,
) -> SqlReturn {
    crate::ffi_entry!("SQLParamData", unsafe {
        sql_param_data_impl(statement_handle, value_ptr_ptr)
    })
}

unsafe fn sql_param_data_impl(
    statement_handle: SqlHandle,
    value_ptr_ptr: *mut SqlPointer,
) -> SqlReturn {
    if statement_handle.is_null() {
        error!("SQLParamData: statement_handle is null");
        return SQL_INVALID_HANDLE;
    }
    let stmt = unsafe { handle_from_raw::<StmtHandle>(statement_handle) };

    // Either hand out the next hungry parameter, or build the deferred
    // execution. Both need the STMT lock; the execution itself must not.
    let staged = {
        let Ok(mut state) = stmt.inner.lock() else {
            error!("SQLParamData: stmt mutex poisoned");
            return SQL_ERROR;
        };
        free_errors(&mut state);

        let Some(dae) = state.dae.as_mut() else {
            error!("SQLParamData: no data-at-execution sequence in progress");
            post_sql_error(
                &mut state,
                SQLSTATE_HY010,
                0,
                "Function sequence error: SQLParamData called outside a data-at-execution sequence",
            );
            return SQL_ERROR;
        };

        if dae.next < dae.order.len() {
            let index = dae.order[dae.next];
            dae.next += 1;
            dae.current = Some(index);
            let token = state
                .bound_params
                .get(index)
                .and_then(|p| p.as_ref())
                .map(|p| p.parameter_value_ptr)
                .unwrap_or(std::ptr::null_mut());
            if !value_ptr_ptr.is_null() {
                unsafe { *value_ptr_ptr = token };
            }
            debug!(index, "SQLParamData: requesting data for parameter");
            return SQL_NEED_DATA;
        }

        // Every DAE parameter has been fed — materialize the execution.
        let Some(dae) = state.dae.take() else {
            return SQL_ERROR;
        };
        let named_params = match unsafe {
            build_named_params_with_dae(&mut state, dae.marker_count, dae.op, Some(&dae.data))
        } {
            Ok(params) => params,
            Err(rc) => return rc,
        };
        Execution {
            rewritten_sql: dae.rewritten_sql,
            named_params,
            handle: dae.handle,
            drop_handle: dae.drop_handle,
        }
    };

    run_execution(statement_handle, stmt, staged, "SQLParamData")
}

/// Implements `SQLPutData`.
///
/// # Safety
/// - `statement_handle` must be a valid `StmtHandle` or null.
/// - `data_ptr` must be readable for `str_len_or_ind` bytes when that length is
///   positive.
pub(crate) unsafe fn sql_put_data(
    statement_handle: SqlHandle,
    data_ptr: SqlPointer,
    str_len_or_ind: SqlLen,
) -> SqlReturn {
    crate::ffi_entry!("SQLPutData", unsafe {
        sql_put_data_impl(statement_handle, data_ptr, str_len_or_ind)
    })
}

unsafe fn sql_put_data_impl(
    statement_handle: SqlHandle,
    data_ptr: SqlPointer,
    str_len_or_ind: SqlLen,
) -> SqlReturn {
    if statement_handle.is_null() {
        error!("SQLPutData: statement_handle is null");
        return SQL_INVALID_HANDLE;
    }
    let stmt = unsafe { handle_from_raw::<StmtHandle>(statement_handle) };

    let Ok(mut state) = stmt.inner.lock() else {
        error!("SQLPutData: stmt mutex poisoned");
        return SQL_ERROR;
    };
    free_errors(&mut state);

    let Some(dae) = state.dae.as_mut() else {
        error!("SQLPutData: no data-at-execution sequence in progress");
        post_sql_error(
            &mut state,
            SQLSTATE_HY010,
            0,
            "Function sequence error: SQLPutData called outside a data-at-execution sequence",
        );
        return SQL_ERROR;
    };

    let Some(index) = dae.current else {
        error!("SQLPutData: called before SQLParamData named a parameter");
        post_sql_error(
            &mut state,
            SQLSTATE_HY010,
            0,
            "Function sequence error: SQLPutData called before SQLParamData",
        );
        return SQL_ERROR;
    };

    if str_len_or_ind == SQL_NULL_DATA {
        dae.data[index] = None;
        return crate::api::odbc_types::SQL_SUCCESS;
    }

    // A null pointer with a non-NULL indicator is how mssql-python signals a
    // `None` value mid-stream; treat it as a zero-length contribution.
    if data_ptr.is_null() {
        dae.data[index].get_or_insert_with(Vec::new);
        return crate::api::odbc_types::SQL_SUCCESS;
    }

    let len = if str_len_or_ind == SQL_NTS as SqlLen {
        // Null-terminated: the caller did not tell us the width, so treat the
        // buffer as a byte string.
        let mut n = 0usize;
        while unsafe { *(data_ptr as *const u8).add(n) } != 0 {
            n += 1;
        }
        n
    } else if str_len_or_ind < 0 {
        error!(str_len_or_ind, "SQLPutData: invalid length");
        post_sql_error(
            &mut state,
            SQLSTATE_HY090,
            0,
            "Invalid string or buffer length",
        );
        return SQL_ERROR;
    } else {
        str_len_or_ind as usize
    };

    let chunk = unsafe { std::slice::from_raw_parts(data_ptr as *const u8, len) };
    dae.data[index]
        .get_or_insert_with(Vec::new)
        .extend_from_slice(chunk);
    crate::api::odbc_types::SQL_SUCCESS
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::odbc_types::{SQL_NULL_HANDLE, SQL_SUCCESS};
    use crate::handles::stmt::DaeState;
    use crate::test_support::TestHandles;

    fn arm_dae(stmt_raw: SqlHandle, markers: usize, order: Vec<usize>) {
        let stmt = unsafe { handle_from_raw::<StmtHandle>(stmt_raw) };
        let mut state = stmt.inner.lock().unwrap();
        state.dae = Some(DaeState {
            rewritten_sql: "INSERT INTO t VALUES (@P1)".to_string(),
            marker_count: markers,
            handle: None,
            drop_handle: None,
            order,
            next: 0,
            current: None,
            data: vec![None; markers],
            op: "SQLExecute",
        });
    }

    #[test]
    fn null_handle_returns_invalid_handle() {
        let mut token: SqlPointer = std::ptr::null_mut();
        assert_eq!(
            unsafe { sql_param_data(SQL_NULL_HANDLE, &mut token) },
            SQL_INVALID_HANDLE
        );
        assert_eq!(
            unsafe { sql_put_data(SQL_NULL_HANDLE, std::ptr::null_mut(), 0) },
            SQL_INVALID_HANDLE
        );
    }

    #[test]
    fn param_data_without_sequence_posts_hy010() {
        let h = TestHandles::with_env_dbc_stmt();
        let mut token: SqlPointer = std::ptr::null_mut();
        assert_eq!(unsafe { sql_param_data(h.stmt, &mut token) }, SQL_ERROR);
        let stmt = unsafe { handle_from_raw::<StmtHandle>(h.stmt) };
        let state = stmt.inner.lock().unwrap();
        assert_eq!(state.diag_records[0].sql_state, SQLSTATE_HY010);
    }

    #[test]
    fn put_data_before_param_data_posts_hy010() {
        let h = TestHandles::with_env_dbc_stmt();
        arm_dae(h.stmt, 1, vec![0]);
        let mut buf = *b"abc";
        let ret = unsafe { sql_put_data(h.stmt, buf.as_mut_ptr() as SqlPointer, 3) };
        assert_eq!(ret, SQL_ERROR);
        let stmt = unsafe { handle_from_raw::<StmtHandle>(h.stmt) };
        let state = stmt.inner.lock().unwrap();
        assert_eq!(state.diag_records[0].sql_state, SQLSTATE_HY010);
    }

    #[test]
    fn param_data_hands_out_each_parameter_then_executes() {
        let h = TestHandles::with_env_dbc_stmt();
        arm_dae(h.stmt, 1, vec![0]);
        let mut token: SqlPointer = std::ptr::null_mut();
        assert_eq!(unsafe { sql_param_data(h.stmt, &mut token) }, SQL_NEED_DATA);
        // Second call has no more hungry parameters, so it attempts execution,
        // which fails because the DBC is not connected.
        let ret = unsafe { sql_param_data(h.stmt, &mut token) };
        assert_eq!(ret, SQL_ERROR);
    }

    #[test]
    fn put_data_accumulates_chunks() {
        let h = TestHandles::with_env_dbc_stmt();
        arm_dae(h.stmt, 1, vec![0]);
        let mut token: SqlPointer = std::ptr::null_mut();
        assert_eq!(unsafe { sql_param_data(h.stmt, &mut token) }, SQL_NEED_DATA);

        let mut a = *b"abc";
        let mut b = *b"de";
        assert_eq!(
            unsafe { sql_put_data(h.stmt, a.as_mut_ptr() as SqlPointer, 3) },
            SQL_SUCCESS
        );
        assert_eq!(
            unsafe { sql_put_data(h.stmt, b.as_mut_ptr() as SqlPointer, 2) },
            SQL_SUCCESS
        );

        let stmt = unsafe { handle_from_raw::<StmtHandle>(h.stmt) };
        let state = stmt.inner.lock().unwrap();
        let dae = state.dae.as_ref().unwrap();
        assert_eq!(dae.data[0].as_deref(), Some(&b"abcde"[..]));
    }

    #[test]
    fn put_data_null_indicator_marks_null() {
        let h = TestHandles::with_env_dbc_stmt();
        arm_dae(h.stmt, 1, vec![0]);
        let mut token: SqlPointer = std::ptr::null_mut();
        assert_eq!(unsafe { sql_param_data(h.stmt, &mut token) }, SQL_NEED_DATA);
        let mut a = *b"abc";
        assert_eq!(
            unsafe { sql_put_data(h.stmt, a.as_mut_ptr() as SqlPointer, 3) },
            SQL_SUCCESS
        );
        assert_eq!(
            unsafe { sql_put_data(h.stmt, std::ptr::null_mut(), SQL_NULL_DATA) },
            SQL_SUCCESS
        );
        let stmt = unsafe { handle_from_raw::<StmtHandle>(h.stmt) };
        let state = stmt.inner.lock().unwrap();
        assert!(state.dae.as_ref().unwrap().data[0].is_none());
    }

    #[test]
    fn put_data_negative_length_posts_hy090() {
        let h = TestHandles::with_env_dbc_stmt();
        arm_dae(h.stmt, 1, vec![0]);
        let mut token: SqlPointer = std::ptr::null_mut();
        assert_eq!(unsafe { sql_param_data(h.stmt, &mut token) }, SQL_NEED_DATA);
        let mut a = *b"abc";
        let ret = unsafe { sql_put_data(h.stmt, a.as_mut_ptr() as SqlPointer, -7) };
        assert_eq!(ret, SQL_ERROR);
        let stmt = unsafe { handle_from_raw::<StmtHandle>(h.stmt) };
        let state = stmt.inner.lock().unwrap();
        assert_eq!(state.diag_records[0].sql_state, SQLSTATE_HY090);
    }
}
