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
    DbcHandle, EnvHandle, HandleType, OdbcVersion, StmtHandle, free_handle, handle_from_raw,
    handle_to_raw,
};

/// Implementation of [`SQLAllocHandle`](super::exports::SQLAllocHandle).
///
/// # Safety
/// See the exported function's doc for caller requirements.
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
            SQL_HANDLE_STMT => unsafe { alloc_stmt(input_handle, output_handle) },
            SQL_HANDLE_DESC => {
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
/// 3. Acquire the ENV lock and register the DBC in the connection list.
/// 4. Write the opaque pointer to `*output_handle`.
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
    debug_assert_eq!(
        env.header.object_type,
        HandleType::Env,
        "SQLAllocHandle(DBC): input_handle is not an ENV handle"
    );

    // ODBC requires SQL_ATTR_ODBC_VERSION be set on the env before allocating
    // a DBC. msodbcsql asserts this; we return SQL_ERROR per the no-panic guideline.
    // TODO: surface HY010 "Function sequence error" once SQLGetDiagRec lands.
    {
        let Ok(state) = env.inner.lock() else {
            error!("SQLAllocHandle(DBC): env mutex poisoned");
            return SQL_ERROR;
        };
        if state.odbc_version == OdbcVersion::Unset {
            error!("SQLAllocHandle(DBC): SQL_ATTR_ODBC_VERSION not set on env (HY010)");
            return SQL_ERROR;
        }
    }

    let dbc = Box::new(DbcHandle::new(input_handle));
    let raw = handle_to_raw(dbc);

    // The DM guarantees SQLFreeHandle(ENV) cannot be called while
    // SQLAllocHandle(DBC) is in progress on the same ENV, so the
    // parent ENV and its mutex are guaranteed alive here.
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

/// Allocates a statement handle under a parent connection.
///
/// Mirrors msodbcsql's `ExportImp::SQLAllocStmt` / `InitializeNewhStmt`:
/// 1. Validate that input_handle is a valid DBC handle.
/// 2. Heap-allocate a `StmtHandle` with a back-pointer to the parent DBC.
/// 3. Acquire the DBC lock and register the STMT in the statement list.
/// 4. Write the opaque pointer to `*output_handle`.
///
/// # Safety
/// `output_handle` must be a valid, aligned, writable pointer (validated by caller).
/// `input_handle` must be a live `DbcHandle` created by `alloc_dbc`.
unsafe fn alloc_stmt(input_handle: SqlHandle, output_handle: *mut SqlHandle) -> SqlReturn {
    if input_handle.is_null() {
        error!("SQLAllocHandle(STMT): input_handle (DBC) must not be null");
        return SQL_INVALID_HANDLE;
    }

    let dbc = unsafe { handle_from_raw::<DbcHandle>(input_handle) };
    debug_assert_eq!(
        dbc.header.object_type,
        HandleType::Dbc,
        "SQLAllocHandle(STMT): input_handle is not a DBC handle"
    );

    let stmt = Box::new(StmtHandle::new(input_handle));
    let raw = handle_to_raw(stmt);

    // The DM guarantees SQLFreeHandle(DBC) cannot be called while
    // SQLAllocHandle(STMT) is in progress on the same DBC, so the
    // parent DBC and its mutex are guaranteed alive here.
    let Ok(mut state) = dbc.inner.lock() else {
        error!("SQLAllocHandle(STMT): dbc mutex poisoned — freeing STMT");
        unsafe { free_handle::<StmtHandle>(raw) };
        return SQL_ERROR;
    };
    state.statements.push(raw);

    unsafe { output_handle.write(raw) };

    debug!(?raw, ?input_handle, "Allocated STMT handle");
    SQL_SUCCESS
}

#[cfg(test)]
mod tests {
    use std::ptr;

    use super::*;
    use crate::api::free_handle::sql_free_handle;
    use crate::api::odbc_types::{
        SQL_ATTR_ODBC_VERSION, SQL_OV_ODBC3_80,
    };
    use crate::api::set_env_attr::sql_set_env_attr;
    use crate::handles::{HandleType, free_handle};

    /// Helper: alloc env and set ODBC version so DBC allocation is permitted.
    fn alloc_env_v3_80() -> SqlHandle {
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
        let env_handle = alloc_env_v3_80();

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
    fn alloc_dbc_without_version_set_returns_error() {
        let mut env: SqlHandle = ptr::null_mut();
        let ret = unsafe { sql_alloc_handle(SQL_HANDLE_ENV, SQL_NULL_HANDLE, &mut env) };
        assert_eq!(ret, SQL_SUCCESS);

        let mut dbc: SqlHandle = ptr::null_mut();
        let ret = unsafe { sql_alloc_handle(SQL_HANDLE_DBC, env, &mut dbc) };
        assert_eq!(ret, SQL_ERROR);
        assert!(dbc.is_null());

        unsafe { sql_free_handle(SQL_HANDLE_ENV, env) };
    }

    #[test]
    fn alloc_dbc_default_state_is_disconnected() {
        use crate::handles::dbc::ConnectionState;

        let env_handle = alloc_env_v3_80();

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
        let env_handle = alloc_env_v3_80();

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
    fn alloc_stmt_returns_success_with_valid_dbc() {
        let (env, dbc) = alloc_env_dbc();

        let mut stmt: SqlHandle = ptr::null_mut();
        let ret = unsafe { sql_alloc_handle(SQL_HANDLE_STMT, dbc, &mut stmt) };
        assert_eq!(ret, SQL_SUCCESS);
        assert!(!stmt.is_null());

        let s = unsafe { &*(stmt as *const StmtHandle) };
        assert_eq!(s.header.object_type, HandleType::Stmt);
        assert_eq!(s.parent_dbc, dbc);

        unsafe { sql_free_handle(SQL_HANDLE_STMT, stmt) };
        unsafe { sql_free_handle(SQL_HANDLE_DBC, dbc) };
        unsafe { sql_free_handle(SQL_HANDLE_ENV, env) };
    }

    #[test]
    fn alloc_stmt_with_null_dbc_returns_invalid_handle() {
        let mut stmt: SqlHandle = ptr::null_mut();
        let ret = unsafe { sql_alloc_handle(SQL_HANDLE_STMT, SQL_NULL_HANDLE, &mut stmt) };
        assert_eq!(ret, SQL_INVALID_HANDLE);
        assert!(stmt.is_null());
    }

    #[test]
    fn alloc_multiple_stmts_on_same_dbc() {
        let (env, dbc) = alloc_env_dbc();

        let mut stmt1: SqlHandle = ptr::null_mut();
        let mut stmt2: SqlHandle = ptr::null_mut();
        assert_eq!(
            unsafe { sql_alloc_handle(SQL_HANDLE_STMT, dbc, &mut stmt1) },
            SQL_SUCCESS
        );
        assert_eq!(
            unsafe { sql_alloc_handle(SQL_HANDLE_STMT, dbc, &mut stmt2) },
            SQL_SUCCESS
        );
        assert_ne!(stmt1, stmt2);

        // Verify DBC tracks both.
        let dbc_ref = unsafe { &*(dbc as *const DbcHandle) };
        let state = dbc_ref.inner.lock().unwrap();
        assert_eq!(state.statements.len(), 2);
        drop(state);

        unsafe { sql_free_handle(SQL_HANDLE_STMT, stmt2) };
        unsafe { sql_free_handle(SQL_HANDLE_STMT, stmt1) };
        unsafe { sql_free_handle(SQL_HANDLE_DBC, dbc) };
        unsafe { sql_free_handle(SQL_HANDLE_ENV, env) };
    }

    #[test]
    fn alloc_stmt_registers_in_parent_dbc() {
        let (env, dbc) = alloc_env_dbc();

        let dbc_ref = unsafe { &*(dbc as *const DbcHandle) };
        assert!(dbc_ref.inner.lock().unwrap().statements.is_empty());

        let mut stmt: SqlHandle = ptr::null_mut();
        assert_eq!(
            unsafe { sql_alloc_handle(SQL_HANDLE_STMT, dbc, &mut stmt) },
            SQL_SUCCESS
        );

        let state = dbc_ref.inner.lock().unwrap();
        assert_eq!(state.statements.len(), 1);
        assert_eq!(state.statements[0], stmt);
        drop(state);

        unsafe { sql_free_handle(SQL_HANDLE_STMT, stmt) };
        unsafe { sql_free_handle(SQL_HANDLE_DBC, dbc) };
        unsafe { sql_free_handle(SQL_HANDLE_ENV, env) };
    }
}
