// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Implementation of SQLFreeHandle — the ODBC handle deallocation entry point.

use tracing::{debug, error};

use super::exec_common::{return_client_idle, try_claim_idle_client};
use crate::api::odbc_types::{
    SQL_ERROR, SQL_HANDLE_DBC, SQL_HANDLE_DBC_INFO_TOKEN, SQL_HANDLE_DESC, SQL_HANDLE_ENV,
    SQL_HANDLE_STMT, SQL_INVALID_HANDLE, SQL_SUCCESS, SqlHandle, SqlReturn, SqlSmallInt,
};
use crate::api::sqlstate::SQLSTATE_HY000;
use crate::error::{free_errors, post_sql_error};
use crate::handles::stmt::{PreparedHandle, STMT_STATE_CURSOR_OPEN};
use crate::handles::{DbcHandle, EnvHandle, HandleType, StmtHandle, free_handle, handle_from_raw};

/// Implementation of [`SQLFreeHandle`](super::exports::SQLFreeHandle).
///
/// # Safety
/// See the exported function's doc for caller requirements.
pub(crate) unsafe fn sql_free_handle(handle_type: SqlSmallInt, handle: SqlHandle) -> SqlReturn {
    debug!(handle_type, ?handle, "SQLFreeHandle called");

    crate::ffi_entry!("SQLFreeHandle", {
        if handle.is_null() {
            error!("SQLFreeHandle: handle is null");
            return SQL_INVALID_HANDLE;
        }

        match handle_type {
            SQL_HANDLE_ENV => unsafe { free_env(handle) },
            SQL_HANDLE_DBC => unsafe { free_dbc(handle) },
            SQL_HANDLE_STMT => unsafe { free_stmt(handle) },
            SQL_HANDLE_DESC | SQL_HANDLE_DBC_INFO_TOKEN => {
                error!(
                    handle_type,
                    "SQLFreeHandle: handle type not yet implemented"
                );
                SQL_ERROR
            }
            _ => {
                error!(handle_type, "SQLFreeHandle: unknown handle type");
                SQL_INVALID_HANDLE
            }
        }
    })
}

/// Mirrors msodbcsql's `SQLFreeEnv` behavior.
///
/// No mutex is acquired - per the ODBC spec, the DM guarantees the
/// connection count on this ENV is 0 before calling `SQLFreeEnv`. DM also
/// ensures no concurrent SQLFreeHandle calls on the same handle.
///
/// # Safety
/// `handle` must be a live `EnvHandle` created by `alloc_env`.
unsafe fn free_env(handle: SqlHandle) -> SqlReturn {
    let env = unsafe { handle_from_raw::<EnvHandle>(handle) };
    debug_assert_eq!(
        env.object_type,
        HandleType::Env,
        "SQLFreeHandle(ENV): handle is not an ENV"
    );

    if let Ok(mut state) = env.inner.lock() {
        free_errors(&mut state);
    }

    debug_assert!(
        env.inner
            .lock()
            .map(|s| s.connections.is_empty())
            .unwrap_or(true),
        "SQLFreeHandle(ENV): DM should have freed all DBCs before calling SQLFreeEnv"
    );

    unsafe { free_handle::<EnvHandle>(handle) };
    SQL_SUCCESS
}

/// Mirrors msodbcsql's `SQLFreeConnect` behavior.
///
/// No DBC mutex is acquired — the DM guarantees the DBC is disconnected
/// before calling `SQLFreeConnect`, and `SQLDisconnect` drops all child
/// handles. msodbcsql's `SQLFreeConnect` doesn't lock the connection mutex either.
///
/// # Safety
/// `handle` must be a live `DbcHandle` created by `alloc_dbc`.
unsafe fn free_dbc(handle: SqlHandle) -> SqlReturn {
    let dbc = unsafe { handle_from_raw::<DbcHandle>(handle) };
    debug_assert_eq!(
        dbc.object_type,
        HandleType::Dbc,
        "SQLFreeHandle(DBC): handle is not a DBC"
    );

    if let Ok(mut state) = dbc.inner.lock() {
        free_errors(&mut state);
    }

    debug_assert!(
        dbc.inner
            .lock()
            .map(|s| s.statements.is_empty())
            .unwrap_or(true),
        "SQLFreeHandle(DBC): DM should have freed all STMTs before calling SQLFreeConnect"
    );

    // Unregister from parent ENV
    let env = unsafe { handle_from_raw::<EnvHandle>(dbc.parent_env) };
    {
        // Lock scope for ENV mutex - so that we unlock before deallocating the DBC
        let Ok(mut env_state) = env.inner.lock() else {
            error!(?handle, "SQLFreeHandle(DBC): env mutex poisoned");
            if let Ok(mut dbc_state) = dbc.inner.lock() {
                post_sql_error(
                    &mut dbc_state,
                    SQLSTATE_HY000,
                    0,
                    "Internal error while freeing connection",
                );
            }
            return SQL_ERROR;
        };
        if let Some(i) = env_state.connections.iter().position(|&p| p == handle) {
            env_state.connections.swap_remove(i);
        }
    }

    unsafe { free_handle::<DbcHandle>(handle) };
    SQL_SUCCESS
}

/// Mirrors msodbcsql's `SQLFreeStmt(SQL_DROP)` behavior.
///
/// If the STMT is not found in the parent DBC's statement list, it was
/// already dropped by `SQLDisconnect` — returns `SQL_SUCCESS` without
/// calling `free_handle`.
///
/// # Safety
/// `handle` must be a live `StmtHandle` created by `alloc_stmt`.
unsafe fn free_stmt(handle: SqlHandle) -> SqlReturn {
    let stmt = unsafe { handle_from_raw::<StmtHandle>(handle) };
    debug_assert_eq!(
        stmt.object_type,
        HandleType::Stmt,
        "SQLFreeHandle(STMT): handle is not a STMT"
    );

    if let Ok(mut state) = stmt.inner.lock() {
        free_errors(&mut state);
    }

    // Lock parent DBC and try to unregister.
    let dbc = unsafe { handle_from_raw::<DbcHandle>(stmt.parent_dbc) };

    // Best-effort: release any server-side prepared handle(s) before the
    // statement is dropped, while the connection is still live and idle.
    best_effort_unprepare_on_free(handle, stmt, dbc);

    {
        // Lock scope for DBC mutex - so that we unlock before deallocating the STMT
        let Ok(mut dbc_state) = dbc.inner.lock() else {
            error!(?handle, "SQLFreeHandle(STMT): dbc mutex poisoned");
            if let Ok(mut stmt_state) = stmt.inner.lock() {
                post_sql_error(
                    &mut stmt_state,
                    SQLSTATE_HY000,
                    0,
                    "Internal error while freeing statement",
                );
            }
            return SQL_ERROR;
        };
        let Some(i) = dbc_state.statements.iter().position(|&p| p == handle) else {
            // Already dropped by SQLDisconnect - early return.
            return SQL_SUCCESS;
        };
        dbc_state.statements.swap_remove(i);
    }

    unsafe { free_handle::<StmtHandle>(handle) };
    SQL_SUCCESS
}

/// Releases a statement's cached and pending prepared handles with
/// `sp_unprepare` before the statement is freed, so server-side plans don't
/// leak for the lifetime of the connection (mirrors msodbcsql dropping the
/// prepared handle on statement drop).
///
/// If the statement still has an open cursor, its result set is drained first
/// (via `close_cursor::drain_and_release`) so the trailing `@handle` token is
/// captured and the connection goes idle. The unprepare itself is best-effort
/// and non-fatal: it acts only when the connection is live and idle, reusing
/// [`try_claim_idle_client`] / [`return_client_idle`]. If the connection is
/// disconnected or busy with another statement, the handles are left for the
/// server to reclaim when the connection closes. No lock is held across I/O.
fn best_effort_unprepare_on_free(handle: SqlHandle, stmt: &StmtHandle, dbc: &DbcHandle) {
    // If a cursor is still open, drain it first: `drain_and_release` reads the
    // trailing `@handle` token (capturing `prepared_handle`), returns the
    // client, and clears `active_stmt` — leaving the connection idle so the
    // unprepare below can claim it. Without this, a `prepare -> SQLExecute
    // (SELECT) -> SQLFreeHandle` sequence (no `SQLCloseCursor`) would skip the
    // unprepare and leak the handle.
    let cursor_open = stmt
        .inner
        .lock()
        .map(|s| s.has_state(STMT_STATE_CURSOR_OPEN))
        .unwrap_or(false);
    if cursor_open {
        super::close_cursor::drain_and_release(stmt, handle);
    }

    let handles: Vec<PreparedHandle> = match stmt.inner.lock() {
        Ok(mut stmt_state) => [
            stmt_state.prepared_handle.take(),
            stmt_state.pending_unprepare.take(),
        ]
        .into_iter()
        .flatten()
        .collect(),
        Err(_) => return,
    };
    if handles.is_empty() {
        return;
    }

    // Claim the client only if connected and idle; otherwise skip and let the
    // server reclaim the handles when the connection closes.
    let Some(mut client) = try_claim_idle_client(dbc, handle) else {
        return;
    };

    let session_epoch = client.connection_recovery_count();
    for prepared in handles {
        if prepared.session_epoch != session_epoch {
            // Handle from a superseded session — already gone server-side.
            continue;
        }
        if let Err(e) = dbc
            .runtime
            .block_on(client.execute_sp_unprepare(prepared.id, None, None))
        {
            error!(%e, handle = prepared.id, "SQLFreeHandle(STMT): sp_unprepare failed — handle leaked until disconnect");
        }
    }

    return_client_idle(dbc, handle, client);
}

#[cfg(test)]
mod tests {
    use std::ptr;

    use super::*;
    use crate::api::alloc_handle::sql_alloc_handle;
    use crate::api::odbc_types::{
        SQL_ATTR_ODBC_VERSION, SQL_HANDLE_ENV, SQL_NULL_HANDLE, SQL_OV_ODBC3_80,
    };
    use crate::api::set_env_attr::sql_set_env_attr;

    /// Allocate an ENV handle with ODBC 3.80 version set.
    fn alloc_env() -> SqlHandle {
        let mut env: SqlHandle = ptr::null_mut();
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
        env
    }

    #[test]
    fn free_env_returns_success() {
        let env = alloc_env();

        let ret = unsafe { sql_free_handle(SQL_HANDLE_ENV, env) };
        assert_eq!(ret, SQL_SUCCESS);
    }

    #[test]
    fn free_dbc_returns_success() {
        let env = alloc_env();

        let mut dbc: SqlHandle = ptr::null_mut();
        let ret = unsafe { sql_alloc_handle(SQL_HANDLE_DBC, env, &mut dbc) };
        assert_eq!(ret, SQL_SUCCESS);

        let ret = unsafe { sql_free_handle(SQL_HANDLE_DBC, dbc) };
        assert_eq!(ret, SQL_SUCCESS);

        let ret = unsafe { sql_free_handle(SQL_HANDLE_ENV, env) };
        assert_eq!(ret, SQL_SUCCESS);
    }

    #[test]
    fn free_null_handle_returns_invalid() {
        let ret = unsafe { sql_free_handle(SQL_HANDLE_ENV, ptr::null_mut()) };
        assert_eq!(ret, SQL_INVALID_HANDLE);
    }

    #[test]
    fn free_unknown_type_returns_invalid() {
        let mut env: SqlHandle = ptr::null_mut();
        let ret = unsafe { sql_alloc_handle(SQL_HANDLE_ENV, SQL_NULL_HANDLE, &mut env) };
        assert_eq!(ret, SQL_SUCCESS);

        let ret = unsafe { sql_free_handle(99, env) };
        assert_eq!(ret, SQL_INVALID_HANDLE);

        // env is still live — clean up
        unsafe { free_handle::<EnvHandle>(env) };
    }

    #[test]
    fn free_env_with_outstanding_dbc_fails_in_debug() {
        // The DM guarantees all DBCs are freed before calling SQLFreeEnv.
        // The driver trusts this and frees unconditionally (matching msodbcsql).
        // In debug builds, debug_assert! fires and catch_unwind returns SQL_ERROR.
        let env = alloc_env();

        let mut dbc: SqlHandle = ptr::null_mut();
        let ret = unsafe { sql_alloc_handle(SQL_HANDLE_DBC, env, &mut dbc) };
        assert_eq!(ret, SQL_SUCCESS);

        let ret = unsafe { sql_free_handle(SQL_HANDLE_ENV, env) };
        if cfg!(debug_assertions) {
            // debug_assert! panics, catch_unwind converts to SQL_ERROR.
            assert_eq!(ret, SQL_ERROR);
            // ENV was not freed due to panic — clean up both handles.
            unsafe { free_handle::<DbcHandle>(dbc) };
            unsafe { free_handle::<EnvHandle>(env) };
        } else {
            assert_eq!(ret, SQL_SUCCESS);
            // ENV freed, DBC orphaned — clean up directly.
            unsafe { free_handle::<DbcHandle>(dbc) };
        }
    }

    // --- Helper: alloc ENV + DBC for STMT tests ---
    fn alloc_env_dbc() -> (SqlHandle, SqlHandle) {
        let env = alloc_env();
        let mut dbc: SqlHandle = ptr::null_mut();
        let ret = unsafe { sql_alloc_handle(SQL_HANDLE_DBC, env, &mut dbc) };
        assert_eq!(ret, SQL_SUCCESS);
        (env, dbc)
    }

    #[test]
    fn free_stmt_returns_success() {
        let (env, dbc) = alloc_env_dbc();

        let mut stmt: SqlHandle = ptr::null_mut();
        let ret = unsafe { sql_alloc_handle(SQL_HANDLE_STMT, dbc, &mut stmt) };
        assert_eq!(ret, SQL_SUCCESS);

        let ret = unsafe { sql_free_handle(SQL_HANDLE_STMT, stmt) };
        assert_eq!(ret, SQL_SUCCESS);

        unsafe { sql_free_handle(SQL_HANDLE_DBC, dbc) };
        unsafe { sql_free_handle(SQL_HANDLE_ENV, env) };
    }

    #[test]
    fn free_stmt_unregisters_from_parent_dbc() {
        let (env, dbc) = alloc_env_dbc();

        let mut stmt: SqlHandle = ptr::null_mut();
        let ret = unsafe { sql_alloc_handle(SQL_HANDLE_STMT, dbc, &mut stmt) };
        assert_eq!(ret, SQL_SUCCESS);

        let dbc_ref = unsafe { &*(dbc as *const DbcHandle) };
        assert_eq!(dbc_ref.inner.lock().unwrap().statements.len(), 1);

        let ret = unsafe { sql_free_handle(SQL_HANDLE_STMT, stmt) };
        assert_eq!(ret, SQL_SUCCESS);
        assert!(dbc_ref.inner.lock().unwrap().statements.is_empty());

        unsafe { sql_free_handle(SQL_HANDLE_DBC, dbc) };
        unsafe { sql_free_handle(SQL_HANDLE_ENV, env) };
    }

    #[test]
    fn free_dbc_with_outstanding_stmt_fails_in_debug() {
        // The DM guarantees all STMTs are freed before calling SQLFreeConnect.
        // The driver trusts this and frees unconditionally (matching msodbcsql).
        // In debug builds, debug_assert! fires and catch_unwind returns SQL_ERROR.
        let (env, dbc) = alloc_env_dbc();

        let mut stmt: SqlHandle = ptr::null_mut();
        let ret = unsafe { sql_alloc_handle(SQL_HANDLE_STMT, dbc, &mut stmt) };
        assert_eq!(ret, SQL_SUCCESS);

        let ret = unsafe { sql_free_handle(SQL_HANDLE_DBC, dbc) };
        if cfg!(debug_assertions) {
            // debug_assert! panics, catch_unwind converts to SQL_ERROR.
            assert_eq!(ret, SQL_ERROR);
            // DBC was not freed due to panic — clean up all handles.
            unsafe { free_handle::<StmtHandle>(stmt) };
            unsafe { sql_free_handle(SQL_HANDLE_DBC, dbc) };
        } else {
            assert_eq!(ret, SQL_SUCCESS);
            // DBC freed, STMT orphaned — clean up directly.
            unsafe { free_handle::<StmtHandle>(stmt) };
        }

        unsafe { sql_free_handle(SQL_HANDLE_ENV, env) };
    }

    #[test]
    fn free_desc_returns_error_not_implemented() {
        let ret = unsafe { sql_free_handle(SQL_HANDLE_DESC, 0x1 as SqlHandle) };
        assert_eq!(ret, SQL_ERROR);
    }

    #[test]
    fn free_dbc_info_token_returns_error_not_implemented() {
        use crate::api::odbc_types::SQL_HANDLE_DBC_INFO_TOKEN;
        let ret = unsafe { sql_free_handle(SQL_HANDLE_DBC_INFO_TOKEN, 0x1 as SqlHandle) };
        assert_eq!(ret, SQL_ERROR);
    }
}
