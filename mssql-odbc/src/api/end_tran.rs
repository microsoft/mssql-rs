// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Implementation of `SQLEndTran` — commit or roll back a transaction.

use tracing::{debug, error};

use super::odbc_types::{
    SQL_COMMIT, SQL_ERROR, SQL_HANDLE_DBC, SQL_HANDLE_ENV, SQL_INVALID_HANDLE, SQL_ROLLBACK,
    SQL_SUCCESS, SqlHandle, SqlReturn, SqlSmallInt,
};
use super::sqlstate::{ERR_INVALID_TRANSACTION_OPERATION_CODE, post_diag};
use super::txn::end_transaction;
use crate::error::free_errors;
use crate::handles::{DbcHandle, EnvHandle, HandleType, handle_from_raw};

/// Implementation of `SQLEndTran`.
///
/// With `SQL_HANDLE_ENV` the request fans out over every connection owned by
/// that environment, the worst return code winning — msodbcsql `sqlctran.cpp:29-41`.
///
/// # Safety
/// `handle` must be a valid `EnvHandle` or `DbcHandle` matching `handle_type`,
/// or null.
pub(crate) unsafe fn sql_end_tran(
    handle_type: SqlSmallInt,
    handle: SqlHandle,
    completion_type: SqlSmallInt,
) -> SqlReturn {
    debug!(handle_type, ?handle, completion_type, "SQLEndTran called");
    crate::ffi_entry!("SQLEndTran", unsafe {
        sql_end_tran_impl(handle_type, handle, completion_type)
    })
}

unsafe fn sql_end_tran_impl(
    handle_type: SqlSmallInt,
    handle: SqlHandle,
    completion_type: SqlSmallInt,
) -> SqlReturn {
    if handle.is_null() {
        error!("SQLEndTran: handle is null");
        return SQL_INVALID_HANDLE;
    }

    match handle_type {
        SQL_HANDLE_DBC => {
            let dbc = unsafe { handle_from_raw::<DbcHandle>(handle) };
            debug_assert_eq!(
                dbc.object_type,
                HandleType::Dbc,
                "SQLEndTran: handle is not a DBC"
            );
            sql_end_tran_dbc_safe(dbc, completion_type)
        }
        SQL_HANDLE_ENV => {
            let env = unsafe { handle_from_raw::<EnvHandle>(handle) };
            debug_assert_eq!(
                env.object_type,
                HandleType::Env,
                "SQLEndTran: handle is not an ENV"
            );
            unsafe { sql_end_tran_env_safe(env, completion_type) }
        }
        _ => {
            error!(handle_type, "SQLEndTran: handle type is not ENV or DBC");
            SQL_INVALID_HANDLE
        }
    }
}

/// Validates `completion_type`, returning the "commit?" flag.
fn commit_flag(completion_type: SqlSmallInt) -> Option<bool> {
    match completion_type {
        SQL_COMMIT => Some(true),
        SQL_ROLLBACK => Some(false),
        _ => None,
    }
}

fn sql_end_tran_dbc_safe(dbc: &DbcHandle, completion_type: SqlSmallInt) -> SqlReturn {
    if let Ok(mut state) = dbc.inner.lock() {
        free_errors(&mut state);
    } else {
        error!("SQLEndTran: dbc mutex poisoned");
        return SQL_ERROR;
    }

    let Some(commit) = commit_flag(completion_type) else {
        error!(completion_type, "SQLEndTran: invalid completion type");
        if let Ok(mut state) = dbc.inner.lock() {
            post_diag(&mut state, ERR_INVALID_TRANSACTION_OPERATION_CODE);
        }
        return SQL_ERROR;
    };

    end_transaction(dbc, commit, "SQLEndTran")
}

/// Fans the request out over every connection on `env`.
///
/// # Safety
/// Every pointer in `EnvState::connections` must be a live `DbcHandle`.
unsafe fn sql_end_tran_env_safe(env: &EnvHandle, completion_type: SqlSmallInt) -> SqlReturn {
    let Ok(mut env_state) = env.inner.lock() else {
        error!("SQLEndTran: env mutex poisoned");
        return SQL_ERROR;
    };
    free_errors(&mut env_state);

    if commit_flag(completion_type).is_none() {
        error!(completion_type, "SQLEndTran: invalid completion type");
        post_diag(&mut env_state, ERR_INVALID_TRANSACTION_OPERATION_CODE);
        return SQL_ERROR;
    }

    let connections = env_state.connections.clone();
    drop(env_state);

    let mut worst = SQL_SUCCESS;
    for dbc_ptr in connections {
        // SAFETY: pointers in `connections` came from `handle_to_raw::<DbcHandle>`
        // and are owned by this ENV. A concurrent
        // `SQLFreeHandle(SQL_HANDLE_DBC)` could still free one between the clone
        // above and this call — the same handle-lifetime gap `SQLDisconnect`
        // documents (see the TODO in `disconnect.rs`), which refcounted handles
        // will close for the whole driver at once.
        let dbc = unsafe { handle_from_raw::<DbcHandle>(dbc_ptr) };
        let ret = sql_end_tran_dbc_safe(dbc, completion_type);
        // msodbcsql `PromoteRetcode`: the worst outcome wins.
        if ret != SQL_SUCCESS {
            worst = ret;
        }
    }
    worst
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::odbc_types::{SQL_HANDLE_STMT, SQL_NULL_HANDLE};
    use crate::error::HasDiagnostics;
    use crate::handles::handle_from_raw;
    use crate::test_support::TestHandles;

    fn dbc_state(handle: SqlHandle) -> [u8; 5] {
        let dbc = unsafe { handle_from_raw::<DbcHandle>(handle) };
        let state = dbc.inner.lock().expect("dbc mutex poisoned");
        state.diag_records()[0].sql_state
    }

    fn env_state(handle: SqlHandle) -> [u8; 5] {
        let env = unsafe { handle_from_raw::<EnvHandle>(handle) };
        let state = env.inner.lock().expect("env mutex poisoned");
        state.diag_records()[0].sql_state
    }

    #[test]
    fn end_tran_null_handle_returns_invalid_handle() {
        let ret = unsafe { sql_end_tran(SQL_HANDLE_DBC, SQL_NULL_HANDLE, SQL_COMMIT) };
        assert_eq!(ret, SQL_INVALID_HANDLE);
    }

    #[test]
    fn end_tran_bad_handle_type_returns_invalid_handle() {
        let h = TestHandles::with_env_dbc();
        let ret = unsafe { sql_end_tran(SQL_HANDLE_STMT, h.dbc, SQL_COMMIT) };
        assert_eq!(ret, SQL_INVALID_HANDLE);
    }

    #[test]
    fn end_tran_invalid_completion_type_posts_hy012() {
        let h = TestHandles::with_env_dbc();
        let ret = unsafe { sql_end_tran(SQL_HANDLE_DBC, h.dbc, 42) };
        assert_eq!(ret, SQL_ERROR);
        assert_eq!(&dbc_state(h.dbc), b"HY012");
    }

    #[test]
    fn end_tran_on_disconnected_dbc_posts_08003() {
        let h = TestHandles::with_env_dbc();
        let ret = unsafe { sql_end_tran(SQL_HANDLE_DBC, h.dbc, SQL_COMMIT) };
        assert_eq!(ret, SQL_ERROR);
        assert_eq!(&dbc_state(h.dbc), b"08003");
    }

    #[test]
    fn end_tran_without_open_transaction_succeeds() {
        let h = TestHandles::with_env_dbc();
        h.mark_dbc_connected();
        // No statement has executed, so `local_tran_started` is false and
        // msodbcsql's silent-success path applies — no client is ever touched.
        for completion in [SQL_COMMIT, SQL_ROLLBACK] {
            let ret = unsafe { sql_end_tran(SQL_HANDLE_DBC, h.dbc, completion) };
            assert_eq!(ret, SQL_SUCCESS, "completion {completion}");
        }
    }

    #[test]
    fn end_tran_on_env_fans_out_and_succeeds() {
        let h = TestHandles::with_env_dbc();
        h.mark_dbc_connected();
        let ret = unsafe { sql_end_tran(SQL_HANDLE_ENV, h.env, SQL_ROLLBACK) };
        assert_eq!(ret, SQL_SUCCESS);
    }

    #[test]
    fn end_tran_on_env_promotes_worst_return_code() {
        // The child DBC is left disconnected, so its leg fails with 08003 and
        // the fan-out must surface SQL_ERROR rather than the initial SUCCESS.
        let h = TestHandles::with_env_dbc();
        let ret = unsafe { sql_end_tran(SQL_HANDLE_ENV, h.env, SQL_COMMIT) };
        assert_eq!(ret, SQL_ERROR);
        assert_eq!(&dbc_state(h.dbc), b"08003");
    }

    #[test]
    fn end_tran_on_env_validates_completion_type() {
        let h = TestHandles::with_env_dbc();
        let ret = unsafe { sql_end_tran(SQL_HANDLE_ENV, h.env, -7) };
        assert_eq!(ret, SQL_ERROR);
        assert_eq!(&env_state(h.env), b"HY012");
    }

    #[test]
    fn end_tran_on_env_with_no_connections_succeeds() {
        let h = TestHandles::with_env();
        let ret = unsafe { sql_end_tran(SQL_HANDLE_ENV, h.env, SQL_COMMIT) };
        assert_eq!(ret, SQL_SUCCESS);
    }
}
