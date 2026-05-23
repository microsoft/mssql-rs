// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Implementation of SQLFreeHandle — the ODBC handle deallocation entry point.

use std::panic;

use tracing::{debug, error, trace};

use crate::api::odbc_types::{
    SQL_ERROR, SQL_HANDLE_DBC, SQL_HANDLE_DBC_INFO_TOKEN, SQL_HANDLE_DESC, SQL_HANDLE_ENV,
    SQL_HANDLE_STMT, SQL_INVALID_HANDLE, SQL_SUCCESS, SqlHandle, SqlReturn, SqlSmallInt,
};
use crate::handles::{DbcHandle, EnvHandle, HandleType, StmtHandle, free_handle, handle_from_raw};

/// Implementation of [`SQLFreeHandle`](super::exports::SQLFreeHandle).
///
/// # Safety
/// See the exported function's doc for caller requirements.
pub(crate) unsafe fn sql_free_handle(handle_type: SqlSmallInt, handle: SqlHandle) -> SqlReturn {
    debug!(handle_type, ?handle, "SQLFreeHandle called");

    let result = panic::catch_unwind(|| {
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
    });

    let ret = result.unwrap_or_else(|_| {
        error!("SQLFreeHandle: panic caught at FFI boundary");
        SQL_ERROR
    });

    trace!(handle_type, ?ret, "SQLFreeHandle returning");
    ret
}

/// Mirrors msodbcsql's `ExportImp::SQLFreeEnv`.
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

/// Mirrors msodbcsql's `ExportImp::SQLFreeConnect`.
///
/// No DBC mutex is acquired — the DM guarantees the DBC is disconnected
/// before calling `SQLFreeConnect`, and `SQLDisconnect` drops all child
/// handles. msodbcsql's `SQLFreeConnect` doesn't lock `csDbc` either.
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

    debug_assert!(
        dbc.inner
            .lock()
            .map(|s| s.statements.is_empty())
            .unwrap_or(true),
        "SQLFreeHandle(DBC): DM should have freed all STMTs before calling SQLFreeConnect"
    );

    // Unregister from parent ENV
    let env = unsafe { handle_from_raw::<EnvHandle>(dbc.parent_env) };
    let Ok(mut env_state) = env.inner.lock() else {
        error!(?handle, "SQLFreeHandle(DBC): env mutex poisoned");
        return SQL_ERROR;
    };
    if let Some(i) = env_state.connections.iter().position(|&p| p == handle) {
        env_state.connections.swap_remove(i);
    }

    unsafe { free_handle::<DbcHandle>(handle) };
    SQL_SUCCESS
}

/// Mirrors msodbcsql's `ExportImp::SQLFreeStmt(SQL_DROP)`.
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

    // Lock parent DBC and try to unregister.
    let dbc = unsafe { handle_from_raw::<DbcHandle>(stmt.parent_dbc) };
    let Ok(mut dbc_state) = dbc.inner.lock() else {
        error!(?handle, "SQLFreeHandle(STMT): dbc mutex poisoned");
        return SQL_ERROR;
    };
    let Some(i) = dbc_state.statements.iter().position(|&p| p == handle) else {
        // Already dropped by SQLDisconnect - early return.
        return SQL_SUCCESS;
    };
    dbc_state.statements.swap_remove(i);

    unsafe { free_handle::<StmtHandle>(handle) };
    SQL_SUCCESS
}

#[cfg(test)]
mod tests {
    use std::ptr;

    use super::*;
    use crate::api::alloc_handle::sql_alloc_handle;
    use crate::api::odbc_types::{SQL_HANDLE_ENV, SQL_NULL_HANDLE};

    #[test]
    fn free_env_returns_success() {
        let mut env: SqlHandle = ptr::null_mut();
        let ret = unsafe { sql_alloc_handle(SQL_HANDLE_ENV, SQL_NULL_HANDLE, &mut env) };
        assert_eq!(ret, SQL_SUCCESS);

        let ret = unsafe { sql_free_handle(SQL_HANDLE_ENV, env) };
        assert_eq!(ret, SQL_SUCCESS);
    }

    #[test]
    fn free_dbc_returns_success() {
        let mut env: SqlHandle = ptr::null_mut();
        let ret = unsafe { sql_alloc_handle(SQL_HANDLE_ENV, SQL_NULL_HANDLE, &mut env) };
        assert_eq!(ret, SQL_SUCCESS);

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
        let mut env: SqlHandle = ptr::null_mut();
        let ret = unsafe { sql_alloc_handle(SQL_HANDLE_ENV, SQL_NULL_HANDLE, &mut env) };
        assert_eq!(ret, SQL_SUCCESS);

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
        let mut env: SqlHandle = ptr::null_mut();
        let ret = unsafe { sql_alloc_handle(SQL_HANDLE_ENV, SQL_NULL_HANDLE, &mut env) };
        assert_eq!(ret, SQL_SUCCESS);
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
