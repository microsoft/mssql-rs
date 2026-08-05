// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Implementation of SQLEndTran — commit or roll back a transaction.
//!
//! The driver models manual-commit mode the way msodbcsql does: it turns on
//! `IMPLICIT_TRANSACTIONS` so the server opens a transaction on the first
//! statement, and `SQLEndTran` then issues `COMMIT`/`ROLLBACK` guarded by
//! `@@TRANCOUNT` so ending a transaction that was never started is a no-op
//! rather than a `3903`/`3902` error.

use tracing::{debug, error};

use super::conn_exec::exec_on_connection;
use super::sqlstate::*;
use crate::api::odbc_types::{
    SQL_COMMIT, SQL_ERROR, SQL_HANDLE_DBC, SQL_HANDLE_ENV, SQL_INVALID_HANDLE, SQL_ROLLBACK,
    SQL_SUCCESS, SqlHandle, SqlReturn, SqlSmallInt,
};
use crate::error::free_errors;
use crate::handles::dbc::ConnectionState;
use crate::handles::{DbcHandle, EnvHandle, HandleType, handle_from_raw};

/// Commits or rolls back all open transactions on a connection.
///
/// # Safety
/// - `handle` must be a valid handle of type `handle_type` (`SQL_HANDLE_DBC` or
///   `SQL_HANDLE_ENV`) allocated by `SQLAllocHandle`.
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
            end_tran_on_dbc(dbc, completion_type)
        }
        SQL_HANDLE_ENV => {
            // ODBC allows ending transactions for every connection on an
            // environment. Each connection reports its own diagnostics; the
            // worst return code wins.
            let env = unsafe { handle_from_raw::<EnvHandle>(handle) };
            debug_assert_eq!(
                env.object_type,
                HandleType::Env,
                "SQLEndTran: handle is not an ENV"
            );
            let Ok(env_state) = env.inner.lock() else {
                error!("SQLEndTran: env mutex poisoned");
                return SQL_ERROR;
            };
            let connections = env_state.connections.clone();
            drop(env_state);
            let mut ret = SQL_SUCCESS;
            for raw in connections {
                let dbc = unsafe { handle_from_raw::<DbcHandle>(raw) };
                if end_tran_on_dbc(dbc, completion_type) == SQL_ERROR {
                    ret = SQL_ERROR;
                }
            }
            ret
        }
        _ => {
            error!(handle_type, "SQLEndTran: unsupported handle type");
            SQL_INVALID_HANDLE
        }
    }
}

fn end_tran_on_dbc(dbc: &DbcHandle, completion_type: SqlSmallInt) -> SqlReturn {
    let verb = match completion_type {
        SQL_COMMIT => "COMMIT",
        SQL_ROLLBACK => "ROLLBACK",
        other => {
            error!(other, "SQLEndTran: invalid completion type");
            if let Ok(mut state) = dbc.inner.lock() {
                free_errors(&mut state);
                post_diag(&mut state, ERR_INVALID_ATTRIBUTE_VALUE);
            }
            return SQL_ERROR;
        }
    };

    let (connected, autocommit) = {
        let Ok(mut state) = dbc.inner.lock() else {
            error!("SQLEndTran: dbc mutex poisoned");
            return SQL_ERROR;
        };
        free_errors(&mut state);
        (
            state.connection_state == ConnectionState::Connected,
            state.autocommit,
        )
    };

    if !connected {
        error!("SQLEndTran: connection is not open");
        if let Ok(mut state) = dbc.inner.lock() {
            post_diag(&mut state, ERR_CONNECTION_DOES_NOT_EXIST);
        }
        return SQL_ERROR;
    }

    // In autocommit mode every statement is its own transaction, so there is
    // nothing to end. msodbcsql returns success without a round trip.
    if autocommit {
        debug!("SQLEndTran: autocommit is on — nothing to do");
        return SQL_SUCCESS;
    }

    // `IMPLICIT_TRANSACTIONS` only opens a transaction once a statement runs, so
    // @@TRANCOUNT can legitimately be 0 here (e.g. commit immediately after
    // connect). Guard the verb instead of failing with 3902/3903.
    let sql = format!("IF @@TRANCOUNT > 0 {verb} TRANSACTION");
    match exec_on_connection(dbc, &sql, "SQLEndTran") {
        Ok(()) => SQL_SUCCESS,
        Err(rc) => rc,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::odbc_types::{SQL_HANDLE_STMT, SQL_NULL_HANDLE};
    use crate::test_support::TestHandles;

    #[test]
    fn null_handle_returns_invalid_handle() {
        let ret = unsafe { sql_end_tran(SQL_HANDLE_DBC, SQL_NULL_HANDLE, SQL_COMMIT) };
        assert_eq!(ret, SQL_INVALID_HANDLE);
    }

    #[test]
    fn wrong_handle_type_returns_invalid_handle() {
        let h = TestHandles::with_env_dbc();
        let ret = unsafe { sql_end_tran(SQL_HANDLE_STMT, h.dbc, SQL_COMMIT) };
        assert_eq!(ret, SQL_INVALID_HANDLE);
    }

    #[test]
    fn disconnected_connection_returns_error() {
        let h = TestHandles::with_env_dbc();
        let ret = unsafe { sql_end_tran(SQL_HANDLE_DBC, h.dbc, SQL_COMMIT) };
        assert_eq!(ret, SQL_ERROR);
        let dbc = unsafe { handle_from_raw::<DbcHandle>(h.dbc) };
        let state = dbc.inner.lock().unwrap();
        assert_eq!(state.diag_records[0].sql_state, SQLSTATE_08003);
    }

    #[test]
    fn invalid_completion_type_returns_error() {
        let h = TestHandles::with_env_dbc();
        h.mark_dbc_connected();
        let ret = unsafe { sql_end_tran(SQL_HANDLE_DBC, h.dbc, 42) };
        assert_eq!(ret, SQL_ERROR);
        let dbc = unsafe { handle_from_raw::<DbcHandle>(h.dbc) };
        let state = dbc.inner.lock().unwrap();
        assert_eq!(state.diag_records[0].sql_state, SQLSTATE_HY024);
    }

    #[test]
    fn autocommit_commit_is_a_noop() {
        let h = TestHandles::with_env_dbc();
        h.mark_dbc_connected();
        // Default autocommit is on, so no client round trip is attempted.
        let ret = unsafe { sql_end_tran(SQL_HANDLE_DBC, h.dbc, SQL_COMMIT) };
        assert_eq!(ret, SQL_SUCCESS);
    }
}
