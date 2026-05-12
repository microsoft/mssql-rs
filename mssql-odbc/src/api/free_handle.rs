// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Implementation of SQLFreeHandle — the ODBC handle deallocation entry point.

use std::panic;

use tracing::{debug, error, trace};

use crate::api::odbc_types::{
    SQL_ERROR, SQL_HANDLE_DBC, SQL_HANDLE_DBC_INFO_TOKEN, SQL_HANDLE_DESC, SQL_HANDLE_ENV,
    SQL_HANDLE_STMT, SQL_INVALID_HANDLE, SQL_SUCCESS, SqlHandle, SqlReturn, SqlSmallInt,
};
use crate::handles::{DbcHandle, EnvHandle, HandleType, free_handle, handle_from_raw};

/// Frees an environment, connection, statement, or descriptor handle.
///
/// Mirrors msodbcsql's `SQLFreeHandle` in `sqlcconn.cpp`: dispatches by
/// handle type to the appropriate free routine, which marks the handle
/// invalid before dropping.
///
/// # Safety
/// Called from C via the ODBC Driver Manager.
/// - `handle` must have been allocated by `SQLAllocHandle` and not already freed.
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
            SQL_HANDLE_STMT | SQL_HANDLE_DESC | SQL_HANDLE_DBC_INFO_TOKEN => {
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

/// Frees an environment handle.
///
/// Mirrors msodbcsql's `ExportImp::SQLFreeEnv`:
/// 1. Validate the handle type tag matches `Env`.
/// 2. Mark invalid and drop via `free_handle`.
///
/// # Safety
/// `handle` must be a live `EnvHandle` created by `alloc_env`.
unsafe fn free_env(handle: SqlHandle) -> SqlReturn {
    let env = unsafe { handle_from_raw::<EnvHandle>(handle) };
    if env.header.object_type != HandleType::Env {
        error!(?handle, "SQLFreeHandle(ENV): handle is not an ENV");
        return SQL_INVALID_HANDLE;
    }

    let Ok(state) = env.inner.lock() else {
        error!(?handle, "SQLFreeHandle(ENV): env mutex poisoned");
        return SQL_ERROR;
    };
    if !state.connections.is_empty() {
        error!(
            ?handle,
            count = state.connections.len(),
            "SQLFreeHandle(ENV): cannot free — outstanding DBC handles exist"
        );
        return SQL_ERROR;
    }
    // Release the lock before free_handle destroys the Mutex.
    drop(state);

    unsafe { free_handle::<EnvHandle>(handle) };
    SQL_SUCCESS
}

/// Frees a connection handle.
///
/// Mirrors msodbcsql's `ExportImp::SQLFreeConnect`:
/// 1. Validate the handle type tag matches `Dbc`.
/// 2. Verify the connection is disconnected (ODBC spec: freeing a connected
///    DBC returns SQL_ERROR with HY010).
/// 3. Mark invalid and drop via `free_handle`.
///
/// # Safety
/// `handle` must be a live `DbcHandle` created by `alloc_dbc`.
unsafe fn free_dbc(handle: SqlHandle) -> SqlReturn {
    let dbc = unsafe { handle_from_raw::<DbcHandle>(handle) };
    if dbc.header.object_type != HandleType::Dbc {
        error!(?handle, "SQLFreeHandle(DBC): handle is not a DBC");
        return SQL_INVALID_HANDLE;
    }

    // TODO: Check connection state — return SQL_ERROR with HY010 if still connected.
    // Deferred until SQLDriverConnect/SQLConnect is implemented.

    // Unregister from parent ENV.
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
    fn free_wrong_type_env_as_dbc_returns_invalid() {
        let mut env: SqlHandle = ptr::null_mut();
        let ret = unsafe { sql_alloc_handle(SQL_HANDLE_ENV, SQL_NULL_HANDLE, &mut env) };
        assert_eq!(ret, SQL_SUCCESS);

        // Try to free an ENV handle as if it were a DBC
        let ret = unsafe { sql_free_handle(SQL_HANDLE_DBC, env) };
        assert_eq!(ret, SQL_INVALID_HANDLE);

        unsafe { free_handle::<EnvHandle>(env) };
    }

    #[test]
    fn free_wrong_type_dbc_as_env_returns_invalid() {
        let mut env: SqlHandle = ptr::null_mut();
        let ret = unsafe { sql_alloc_handle(SQL_HANDLE_ENV, SQL_NULL_HANDLE, &mut env) };
        assert_eq!(ret, SQL_SUCCESS);

        let mut dbc: SqlHandle = ptr::null_mut();
        let ret = unsafe { sql_alloc_handle(SQL_HANDLE_DBC, env, &mut dbc) };
        assert_eq!(ret, SQL_SUCCESS);

        // Try to free a DBC handle as if it were an ENV
        let ret = unsafe { sql_free_handle(SQL_HANDLE_ENV, dbc) };
        assert_eq!(ret, SQL_INVALID_HANDLE);

        // Clean up via sql_free_handle (which also unregisters from env)
        let ret = unsafe { sql_free_handle(SQL_HANDLE_DBC, dbc) };
        assert_eq!(ret, SQL_SUCCESS);
        let ret = unsafe { sql_free_handle(SQL_HANDLE_ENV, env) };
        assert_eq!(ret, SQL_SUCCESS);
    }

    #[test]
    fn free_env_with_outstanding_dbc_returns_error() {
        let mut env: SqlHandle = ptr::null_mut();
        let ret = unsafe { sql_alloc_handle(SQL_HANDLE_ENV, SQL_NULL_HANDLE, &mut env) };
        assert_eq!(ret, SQL_SUCCESS);

        let mut dbc: SqlHandle = ptr::null_mut();
        let ret = unsafe { sql_alloc_handle(SQL_HANDLE_DBC, env, &mut dbc) };
        assert_eq!(ret, SQL_SUCCESS);

        // Try to free ENV while DBC is still alive — must fail
        let ret = unsafe { sql_free_handle(SQL_HANDLE_ENV, env) };
        assert_eq!(ret, SQL_ERROR);

        // Free DBC first, then ENV succeeds
        let ret = unsafe { sql_free_handle(SQL_HANDLE_DBC, dbc) };
        assert_eq!(ret, SQL_SUCCESS);
        let ret = unsafe { sql_free_handle(SQL_HANDLE_ENV, env) };
        assert_eq!(ret, SQL_SUCCESS);
    }

    #[test]
    fn free_stmt_returns_error_not_implemented() {
        let ret = unsafe { sql_free_handle(SQL_HANDLE_STMT, 0x1 as SqlHandle) };
        assert_eq!(ret, SQL_ERROR);
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
