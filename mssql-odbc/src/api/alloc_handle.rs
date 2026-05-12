// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Implementation of SQLAllocHandle — the ODBC handle allocation entry point.

use std::panic;

use tracing::{debug, error, trace};

use crate::api::odbc_types::{
    SQL_ERROR, SQL_HANDLE_DBC, SQL_HANDLE_DESC, SQL_HANDLE_ENV, SQL_HANDLE_STMT,
    SQL_INVALID_HANDLE, SQL_NULL_HANDLE, SQL_SUCCESS, SqlHandle, SqlReturn, SqlSmallInt,
};
use crate::handles::{
    DbcHandle, EnvHandle, HandleType, free_handle, handle_from_raw, handle_to_raw,
};

/// Allocates an environment, connection, statement, or descriptor handle.
///
/// Currently only `SQL_HANDLE_ENV` is implemented. Other handle types return `SQL_ERROR`.
///
/// # Safety
/// Called from C via the ODBC Driver Manager.
/// - `output_handle` must be a valid, aligned, writable pointer to `SqlHandle`.
/// - For `SQL_HANDLE_ENV`, `input_handle` must be `SQL_NULL_HANDLE`.
/// - For other types (future), `input_handle` must be a valid parent handle.
pub(crate) unsafe fn sql_alloc_handle(
    handle_type: SqlSmallInt,
    input_handle: SqlHandle,
    output_handle: *mut SqlHandle,
) -> SqlReturn {
    debug!(
        handle_type,
        ?input_handle,
        ?output_handle,
        "SQLAllocHandle called"
    );

    let result = panic::catch_unwind(|| {
        if output_handle.is_null() {
            error!("SQLAllocHandle: output_handle is null");
            return SQL_INVALID_HANDLE;
        }

        // Per ODBC spec, initialize output to null before attempting allocation.
        unsafe { output_handle.write(SQL_NULL_HANDLE) };

        match handle_type {
            SQL_HANDLE_ENV => unsafe { alloc_env(input_handle, output_handle) },
            SQL_HANDLE_DBC => unsafe { alloc_dbc(input_handle, output_handle) },
            SQL_HANDLE_STMT | SQL_HANDLE_DESC => {
                error!(
                    handle_type,
                    "SQLAllocHandle: handle type not yet implemented"
                );
                SQL_ERROR
            }
            _ => {
                error!(handle_type, "SQLAllocHandle: unknown handle type");
                SQL_INVALID_HANDLE
            }
        }
    });

    let ret = result.unwrap_or_else(|_| {
        error!("SQLAllocHandle: panic caught at FFI boundary");
        SQL_ERROR
    });

    trace!(handle_type, ?ret, "SQLAllocHandle returning");
    ret
}

/// Allocates an environment handle.
///
/// Mirrors msodbcsql's `ExportImp::SQLAllocEnv`:
/// 1. Validate that input_handle is SQL_NULL_HANDLE (per ODBC spec).
/// 2. Heap-allocate an `EnvHandle` with default state.
/// 3. Write the opaque pointer to `*output_handle`.
/// # Safety
/// `output_handle` must be a valid, aligned, writable pointer (validated by caller).
unsafe fn alloc_env(input_handle: SqlHandle, output_handle: *mut SqlHandle) -> SqlReturn {
    if !input_handle.is_null() {
        error!("SQLAllocHandle(ENV): input_handle must be SQL_NULL_HANDLE");
        return SQL_INVALID_HANDLE;
    }

    let env = Box::new(EnvHandle::new());
    let raw = handle_to_raw(env);

    unsafe { output_handle.write(raw) };

    debug!(?raw, "Allocated ENV handle");
    SQL_SUCCESS
}

/// Allocates a connection handle under a parent environment.
///
/// Mirrors msodbcsql's `ExportImp::SQLAllocConnect`:
/// 1. Validate that input_handle is a valid ENV handle.
/// 2. Heap-allocate a `DbcHandle` with a back-pointer to the parent ENV.
/// 3. Write the opaque pointer to `*output_handle`.
///
/// # Safety
/// `output_handle` must be a valid, aligned, writable pointer (validated by caller).
/// `input_handle` must be a live `EnvHandle` created by `alloc_env`.
unsafe fn alloc_dbc(input_handle: SqlHandle, output_handle: *mut SqlHandle) -> SqlReturn {
    if input_handle.is_null() {
        error!("SQLAllocHandle(DBC): input_handle (ENV) must not be null");
        return SQL_INVALID_HANDLE;
    }

    // Validate that the parent handle is actually an ENV.
    let env = unsafe { handle_from_raw::<EnvHandle>(input_handle) };
    if env.header.object_type != HandleType::Env {
        error!(
            ?input_handle,
            "SQLAllocHandle(DBC): input_handle is not an ENV handle"
        );
        return SQL_INVALID_HANDLE;
    }

    // TODO: Check that env ODBC version is set (not Unset). Return SQL_ERROR with
    // HY010 "Function sequence error" if not. Deferred to SQLSetEnvAttr PR.

    let dbc = Box::new(DbcHandle::new(input_handle));
    let raw = handle_to_raw(dbc);

    // Register the new DBC with the parent ENV.
    let Ok(mut state) = env.inner.lock() else {
        error!("SQLAllocHandle(DBC): env mutex poisoned — freeing DBC");
        unsafe { free_handle::<DbcHandle>(raw) };
        return SQL_ERROR;
    };
    state.connections.push(raw);

    unsafe { output_handle.write(raw) };

    debug!(?raw, ?input_handle, "Allocated DBC handle");
    SQL_SUCCESS
}

#[cfg(test)]
mod tests {
    use std::ptr;

    use super::*;
    use crate::api::free_handle::sql_free_handle;
    use crate::handles::{HandleType, free_handle};

    #[test]
    fn alloc_env_returns_success_and_valid_handle() {
        let mut handle: SqlHandle = ptr::null_mut();
        let ret = unsafe { sql_alloc_handle(SQL_HANDLE_ENV, SQL_NULL_HANDLE, &mut handle) };
        assert_eq!(ret, SQL_SUCCESS);
        assert!(!handle.is_null());

        // Verify the handle header is correctly set.
        let env = unsafe { &*(handle as *const EnvHandle) };
        assert_eq!(env.header.object_type, HandleType::Env);

        // Cleanup
        unsafe { free_handle::<EnvHandle>(handle) };
    }

    #[test]
    fn alloc_env_with_non_null_input_returns_invalid_handle() {
        let mut handle: SqlHandle = ptr::null_mut();
        let fake_parent = 0xDEAD_BEEF_usize as SqlHandle;
        let ret = unsafe { sql_alloc_handle(SQL_HANDLE_ENV, fake_parent, &mut handle) };
        assert_eq!(ret, SQL_INVALID_HANDLE);
        assert!(handle.is_null());
    }

    #[test]
    fn alloc_null_output_returns_invalid_handle() {
        let ret = unsafe { sql_alloc_handle(SQL_HANDLE_ENV, SQL_NULL_HANDLE, ptr::null_mut()) };
        assert_eq!(ret, SQL_INVALID_HANDLE);
    }

    #[test]
    fn alloc_invalid_handle_type_returns_invalid_handle() {
        let mut handle: SqlHandle = ptr::null_mut();
        let ret = unsafe { sql_alloc_handle(99, SQL_NULL_HANDLE, &mut handle) };
        assert_eq!(ret, SQL_INVALID_HANDLE);
        assert!(handle.is_null());
    }

    #[test]
    fn alloc_dbc_returns_success_with_valid_env() {
        let mut env_handle: SqlHandle = ptr::null_mut();
        let ret = unsafe { sql_alloc_handle(SQL_HANDLE_ENV, SQL_NULL_HANDLE, &mut env_handle) };
        assert_eq!(ret, SQL_SUCCESS);

        let mut dbc_handle: SqlHandle = ptr::null_mut();
        let ret = unsafe { sql_alloc_handle(SQL_HANDLE_DBC, env_handle, &mut dbc_handle) };
        assert_eq!(ret, SQL_SUCCESS);
        assert!(!dbc_handle.is_null());

        let dbc = unsafe { &*(dbc_handle as *const DbcHandle) };
        assert_eq!(dbc.header.object_type, HandleType::Dbc);
        assert_eq!(dbc.parent_env, env_handle);

        unsafe { sql_free_handle(SQL_HANDLE_DBC, dbc_handle) };
        unsafe { sql_free_handle(SQL_HANDLE_ENV, env_handle) };
    }

    #[test]
    fn alloc_dbc_with_null_env_returns_invalid_handle() {
        let mut dbc_handle: SqlHandle = ptr::null_mut();
        let ret = unsafe { sql_alloc_handle(SQL_HANDLE_DBC, SQL_NULL_HANDLE, &mut dbc_handle) };
        assert_eq!(ret, SQL_INVALID_HANDLE);
        assert!(dbc_handle.is_null());
    }

    #[test]
    fn alloc_dbc_default_state_is_disconnected() {
        use crate::handles::dbc::ConnectionState;

        let mut env_handle: SqlHandle = ptr::null_mut();
        let ret = unsafe { sql_alloc_handle(SQL_HANDLE_ENV, SQL_NULL_HANDLE, &mut env_handle) };
        assert_eq!(ret, SQL_SUCCESS);

        let mut dbc_handle: SqlHandle = ptr::null_mut();
        let ret = unsafe { sql_alloc_handle(SQL_HANDLE_DBC, env_handle, &mut dbc_handle) };
        assert_eq!(ret, SQL_SUCCESS);

        let dbc = unsafe { &*(dbc_handle as *const DbcHandle) };
        let state = dbc.inner.lock().unwrap();
        assert_eq!(state.connection_state, ConnectionState::Disconnected);
        drop(state);

        unsafe { sql_free_handle(SQL_HANDLE_DBC, dbc_handle) };
        unsafe { sql_free_handle(SQL_HANDLE_ENV, env_handle) };
    }

    #[test]
    fn alloc_multiple_dbcs_on_same_env() {
        let mut env_handle: SqlHandle = ptr::null_mut();
        let ret = unsafe { sql_alloc_handle(SQL_HANDLE_ENV, SQL_NULL_HANDLE, &mut env_handle) };
        assert_eq!(ret, SQL_SUCCESS);

        let mut dbc1: SqlHandle = ptr::null_mut();
        let mut dbc2: SqlHandle = ptr::null_mut();
        let ret = unsafe { sql_alloc_handle(SQL_HANDLE_DBC, env_handle, &mut dbc1) };
        assert_eq!(ret, SQL_SUCCESS);
        let ret = unsafe { sql_alloc_handle(SQL_HANDLE_DBC, env_handle, &mut dbc2) };
        assert_eq!(ret, SQL_SUCCESS);

        assert_ne!(dbc1, dbc2);

        unsafe { sql_free_handle(SQL_HANDLE_DBC, dbc2) };
        unsafe { sql_free_handle(SQL_HANDLE_DBC, dbc1) };
        unsafe { sql_free_handle(SQL_HANDLE_ENV, env_handle) };
    }

    #[test]
    fn alloc_env_default_state_is_correct() {
        use crate::handles::OdbcVersion;

        let mut handle: SqlHandle = ptr::null_mut();
        let ret = unsafe { sql_alloc_handle(SQL_HANDLE_ENV, SQL_NULL_HANDLE, &mut handle) };
        assert_eq!(ret, SQL_SUCCESS);

        let env = unsafe { &*(handle as *const EnvHandle) };
        let state = env.inner.lock().unwrap();
        assert_eq!(state.odbc_version, OdbcVersion::Unset);
        assert!(state.output_nts);
        drop(state);

        unsafe { free_handle::<EnvHandle>(handle) };
    }
}
