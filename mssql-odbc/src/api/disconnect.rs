// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Implementation of SQLDisconnect — close a connection to a data source.

use tracing::{debug, error};

use crate::api::odbc_types::{SQL_ERROR, SQL_INVALID_HANDLE, SQL_SUCCESS, SqlHandle, SqlReturn};
use crate::api::sqlstate::{
    ERR_CONNECTION_DOES_NOT_EXIST, ERR_INVALID_TRANSACTION_STATE, post_diag,
};
use crate::api::txn::rollback_before_disconnect;
use crate::error::free_errors;
use crate::handles::DbcHandle;
use crate::handles::StmtHandle;
use crate::handles::dbc::ConnectionState;
use crate::handles::desc::DescHandle;
use crate::handles::{HandleType, free_handle, handle_from_raw};

/// Implementation of `SQLDisconnect`.
///
/// # Safety
/// - `connection_handle` must be a valid `DbcHandle` previously connected via `SQLDriverConnectW`.
pub(crate) unsafe fn sql_disconnect(connection_handle: SqlHandle) -> SqlReturn {
    debug!(?connection_handle, "SQLDisconnect called");
    crate::ffi_entry!("SQLDisconnect", unsafe {
        sql_disconnect_impl(connection_handle)
    })
}

unsafe fn sql_disconnect_impl(connection_handle: SqlHandle) -> SqlReturn {
    if connection_handle.is_null() {
        error!("SQLDisconnect: connection_handle is null");
        return SQL_INVALID_HANDLE;
    }
    let dbc = unsafe { handle_from_raw::<DbcHandle>(connection_handle) };
    debug_assert_eq!(
        dbc.object_type,
        HandleType::Dbc,
        "SQLDisconnect: handle is not a DBC"
    );
    sql_disconnect_safe(dbc)
}

fn sql_disconnect_safe(dbc: &DbcHandle) -> SqlReturn {
    // Validate under a short lock; the rollback that may follow needs network
    // I/O and must not run while the mutex is held.
    {
        let Ok(mut state) = dbc.inner.lock() else {
            error!("SQLDisconnect: dbc mutex poisoned");
            return SQL_ERROR;
        };
        free_errors(&mut state);

        if state.connection_state != ConnectionState::Connected {
            error!("SQLDisconnect: not connected");
            post_diag(&mut state, ERR_CONNECTION_DOES_NOT_EXIST);
            return SQL_ERROR;
        }

        // msodbcsql refuses to disconnect while a transaction holds user work
        // (`sqlcconn.cpp:1169-1239`) rather than guessing commit or rollback;
        // the application must call SQLEndTran first.
        if state.local_tran_started {
            error!("SQLDisconnect: a transaction is still open");
            post_diag(&mut state, ERR_INVALID_TRANSACTION_STATE);
            return SQL_ERROR;
        }
    }

    // Manual-commit mode leaves a driver-begun transaction open between
    // statements. It holds no user work, so roll it back explicitly instead of
    // relying on the server's cleanup when the socket closes.
    rollback_before_disconnect(dbc);

    let Ok(mut state) = dbc.inner.lock() else {
        error!("SQLDisconnect: dbc mutex poisoned");
        return SQL_ERROR;
    };

    // Drop all child STMT handles.
    // Note: the DBC lock prevents any *new* SQLExecDirectW from taking the client (it needs
    // the DBC lock). However, a call that already took the client and is mid-execute() holds
    // no locks during I/O, so it can race here and access a STMT handle we are about to free.
    // TODO: fix with refcounted handle lifetimes so STMT handles cannot be freed while in use.
    for &stmt_ptr in &state.statements {
        // SAFETY: `stmt_ptr` came from `handle_to_raw::<StmtHandle>` and is still
        // live (the DBC owns it). Acquire the STMT lock to serialize with any
        // op still holding it, then drop the box.
        let stmt = unsafe { handle_from_raw::<StmtHandle>(stmt_ptr) };
        let Ok(guard) = stmt.inner.lock() else {
            error!(?stmt_ptr, "SQLDisconnect: stmt mutex poisoned");
            return SQL_ERROR;
        };
        drop(guard);
        unsafe { free_handle::<StmtHandle>(stmt_ptr) };
    }
    state.statements.clear();

    // Drop all explicitly-allocated DESC handles, after the statements: an
    // explicit descriptor carries no back-pointer to whichever statements had
    // it as their active ARD/APD, so dropping every STMT first (above) makes
    // any such association moot rather than something to walk and reset here.
    // Mirrors msodbcsql's `SQLDisconnect`, which frees the connection's
    // descriptor-allocation-node list the same way, after its own statement
    // loop (`sqlcconn.cpp:1313-1448`).
    for &desc_ptr in &state.descriptors {
        // SAFETY: `desc_ptr` came from `handle_to_raw::<DescHandle>` and is
        // still live (the DBC owns it). Acquire the DESC lock to serialize
        // with any op still holding it, then drop the box.
        let desc = unsafe { handle_from_raw::<DescHandle>(desc_ptr) };
        let Ok(guard) = desc.inner.lock() else {
            error!(?desc_ptr, "SQLDisconnect: desc mutex poisoned");
            return SQL_ERROR;
        };
        drop(guard);
        unsafe { free_handle::<DescHandle>(desc_ptr) };
    }
    state.descriptors.clear();

    // Drop the TDS client (closes the connection) and clear connection-level cursor claim.
    state.client = None;
    state.active_stmt = None;
    state.connection_state = ConnectionState::Disconnected;

    debug!("SQLDisconnect: disconnected successfully");
    SQL_SUCCESS
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::alloc_handle::sql_alloc_handle;
    use crate::api::free_handle::sql_free_handle;
    use crate::api::odbc_types::{
        SQL_ATTR_ODBC_VERSION, SQL_HANDLE_DBC, SQL_HANDLE_ENV, SQL_NULL_HANDLE, SQL_OV_ODBC3_80,
    };
    use crate::api::set_env_attr::sql_set_env_attr;

    #[test]
    fn disconnect_when_not_connected() {
        let mut env: SqlHandle = SQL_NULL_HANDLE;
        let ret = unsafe { sql_alloc_handle(SQL_HANDLE_ENV, SQL_NULL_HANDLE, &mut env) };
        assert_eq!(ret, SQL_SUCCESS);

        let ret = unsafe {
            sql_set_env_attr(
                env,
                SQL_ATTR_ODBC_VERSION,
                SQL_OV_ODBC3_80 as usize as *mut std::ffi::c_void,
                0,
            )
        };
        assert_eq!(ret, SQL_SUCCESS);

        let mut dbc: SqlHandle = SQL_NULL_HANDLE;
        let ret = unsafe { sql_alloc_handle(SQL_HANDLE_DBC, env, &mut dbc) };
        assert_eq!(ret, SQL_SUCCESS);

        // Disconnect without connecting — should error
        let ret = unsafe { sql_disconnect(dbc) };
        assert_eq!(ret, SQL_ERROR);
        // TODO: verify SQLSTATE 08003 via SQLGetDiagRec

        unsafe {
            sql_free_handle(SQL_HANDLE_DBC, dbc);
            sql_free_handle(SQL_HANDLE_ENV, env);
        }
    }

    #[test]
    fn null_handle_returns_invalid_handle() {
        let ret = unsafe { sql_disconnect(SQL_NULL_HANDLE) };
        assert_eq!(ret, SQL_INVALID_HANDLE);
        // TODO: verify SQLSTATE HY009 via SQLGetDiagRec
    }

    /// `SQLDisconnect` must free any outstanding explicitly-allocated
    /// descriptors (ODBC reference, "Freeing a Connection Handle": "Notice
    /// that SQLDisconnect automatically drops any statements and descriptors
    /// open on the connection"), leaving `DbcState::descriptors` empty so a
    /// later `SQLFreeHandle(SQL_HANDLE_DBC)` doesn't trip its
    /// "DM should have freed all explicit DESCs" debug_assert.
    #[test]
    fn disconnect_frees_outstanding_explicit_descriptors() {
        use crate::api::odbc_types::SQL_HANDLE_DESC;
        use crate::handles::dbc::ConnectionState;

        let mut env: SqlHandle = SQL_NULL_HANDLE;
        assert_eq!(
            unsafe { sql_alloc_handle(SQL_HANDLE_ENV, SQL_NULL_HANDLE, &mut env) },
            SQL_SUCCESS
        );
        assert_eq!(
            unsafe {
                sql_set_env_attr(
                    env,
                    SQL_ATTR_ODBC_VERSION,
                    SQL_OV_ODBC3_80 as usize as *mut std::ffi::c_void,
                    0,
                )
            },
            SQL_SUCCESS
        );
        let mut dbc: SqlHandle = SQL_NULL_HANDLE;
        assert_eq!(
            unsafe { sql_alloc_handle(SQL_HANDLE_DBC, env, &mut dbc) },
            SQL_SUCCESS
        );

        // Simulate a connected DBC without a real TDS client — sufficient for
        // this test, since `rollback_before_disconnect` no-ops when there is
        // no client to roll back.
        let dbc_ref = unsafe { handle_from_raw::<DbcHandle>(dbc) };
        dbc_ref.inner.lock().unwrap().connection_state = ConnectionState::Connected;

        let mut desc: SqlHandle = SQL_NULL_HANDLE;
        assert_eq!(
            unsafe { sql_alloc_handle(SQL_HANDLE_DESC, dbc, &mut desc) },
            SQL_SUCCESS
        );
        assert_eq!(dbc_ref.inner.lock().unwrap().descriptors.len(), 1);

        assert_eq!(unsafe { sql_disconnect(dbc) }, SQL_SUCCESS);
        assert!(dbc_ref.inner.lock().unwrap().descriptors.is_empty());
        // Deliberately not asserting on `desc`'s memory here: `free_handle`
        // stamps `object_type = Invalid` before dropping the box, but reading
        // it back through the now-dangling `desc` pointer is a use-after-free
        // read — technically UB regardless of platform. It happened to read
        // back correctly on Windows/Linux (the freed block wasn't reused
        // before the read) but not on macOS, whose allocator reused it
        // sooner. The connection no longer tracking the descriptor (checked
        // above) is the real, safely-observable contract.

        unsafe {
            sql_free_handle(SQL_HANDLE_DBC, dbc);
            sql_free_handle(SQL_HANDLE_ENV, env);
        }
    }
}
