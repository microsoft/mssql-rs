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
    //
    // Pops (rather than iterating then clearing) so a poisoned STMT mutex
    // leaves only the not-yet-freed remainder in `state.statements`: a
    // retried `SQLDisconnect` — a reasonable response to the `SQL_ERROR` this
    // returns — picks up where this one stopped instead of double-freeing the
    // statements already dropped this pass (mssql-rs#401). Matches the
    // descriptor loop directly below, which already does this.
    while let Some(stmt_ptr) = state.statements.pop() {
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

    // Drop all explicitly-allocated DESC handles, after the statements: an
    // explicit descriptor carries no back-pointer to whichever statements had
    // it as their active ARD/APD, so dropping every STMT first (above) makes
    // any such association moot rather than something to walk and reset here.
    // Mirrors msodbcsql's `SQLDisconnect`, which frees the connection's
    // descriptor-allocation-node list the same way, after its own statement
    // loop (`sqlcconn.cpp:1313-1448`).
    //
    // Pops (rather than iterating then clearing) so a poisoned DESC mutex
    // leaves only the not-yet-freed remainder in `state.descriptors`: a
    // retried `SQLDisconnect` picks up where this one stopped instead of
    // re-freeing entries this pass already dropped.
    while let Some(desc_ptr) = state.descriptors.pop() {
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

    // Drop the TDS client (closes the connection) and clear connection-level cursor claim.
    state.client = None;
    state.active_stmt = None;
    state.effective_vendor_settings = None;
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

    /// Reproduces mssql-rs#401: a poisoned STMT mutex partway through the
    /// statement-free loop must leave every *not-yet-processed* statement
    /// untouched for a retry, not just stop and leave the whole original list
    /// in place. The pop-then-free shape (matching the descriptor loop
    /// directly below it) removes each pointer from `state.statements`
    /// *before* attempting to free it, so a retried `SQLDisconnect` never
    /// re-walks — and re-frees — a statement this pass already dropped.
    #[test]
    fn disconnect_retry_after_poisoned_stmt_mutex_does_not_double_free() {
        use crate::api::odbc_types::SQL_HANDLE_STMT;
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
        let dbc_ref = unsafe { handle_from_raw::<DbcHandle>(dbc) };
        dbc_ref.inner.lock().unwrap().connection_state = ConnectionState::Connected;

        let mut stmt1: SqlHandle = SQL_NULL_HANDLE;
        assert_eq!(
            unsafe { sql_alloc_handle(SQL_HANDLE_STMT, dbc, &mut stmt1) },
            SQL_SUCCESS
        );
        let mut stmt2: SqlHandle = SQL_NULL_HANDLE;
        assert_eq!(
            unsafe { sql_alloc_handle(SQL_HANDLE_STMT, dbc, &mut stmt2) },
            SQL_SUCCESS
        );

        // Poison stmt2's mutex — the loop pops in LIFO order, so stmt2 (the
        // more recently pushed) is attempted first and fails immediately,
        // before stmt1 is ever touched.
        let stmt2_ref = unsafe { handle_from_raw::<StmtHandle>(stmt2) };
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _guard = stmt2_ref.inner.lock().unwrap();
            panic!("poison the stmt lock");
        }));

        assert_eq!(unsafe { sql_disconnect(dbc) }, SQL_ERROR);
        {
            let state = dbc_ref.inner.lock().unwrap();
            // stmt2 was popped off (and so can no longer be double-freed by
            // a retry) even though its own free could not proceed — a
            // poisoned mutex can't be safely retried, so leaking it here is
            // the same accepted trade-off the descriptor loop already makes.
            // stmt1 was never reached and must still be tracked, untouched.
            assert_eq!(state.statements, vec![stmt1]);
            assert_eq!(state.connection_state, ConnectionState::Connected);
        }

        // Retry: connection_state is still Connected, so this is accepted
        // and frees stmt1 exactly once. Before this fix, a retry here would
        // re-walk the *original* two-element list collected before the first
        // pass failed, re-freeing stmt1 a second time.
        assert_eq!(unsafe { sql_disconnect(dbc) }, SQL_SUCCESS);
        assert!(dbc_ref.inner.lock().unwrap().statements.is_empty());

        // stmt2's box was deliberately never dropped (poisoned mutex —
        // leaked, not freed); free it directly so this test doesn't leak.
        unsafe { free_handle::<StmtHandle>(stmt2) };

        unsafe {
            sql_free_handle(SQL_HANDLE_DBC, dbc);
            sql_free_handle(SQL_HANDLE_ENV, env);
        }
    }
}
