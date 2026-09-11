// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Implementation of SQLAllocHandle — the ODBC handle allocation entry point.

use std::sync::Arc;

use tracing::{debug, error};

use crate::api::odbc_types::{
    SQL_DESC_ALLOC_AUTO, SQL_DESC_ALLOC_USER, SQL_ERROR, SQL_HANDLE_DBC, SQL_HANDLE_DESC,
    SQL_HANDLE_ENV, SQL_HANDLE_STMT, SQL_INVALID_HANDLE, SQL_NULL_HANDLE, SQL_SUCCESS, SqlHandle,
    SqlReturn, SqlSmallInt,
};
use crate::api::sqlstate::{ERR_MEMORY_ALLOCATION, post_diag};
use crate::error::free_errors;
use crate::handles::desc::DescKind;
use crate::handles::{
    DbcHandle, DescHandle, EnvHandle, Handle, HandleActivity, HandleType, OdbcVersion,
    RegistryError, StmtHandle, get_handle, handle_to_raw, retire_handle,
};

struct PendingHandle<T: Handle> {
    value: Arc<T>,
    raw: SqlHandle,
    published: bool,
}

impl<T: Handle> PendingHandle<T> {
    fn new(value: T) -> Result<Self, RegistryError> {
        let value = Arc::new(value);
        let raw = handle_to_raw(Arc::clone(&value))?;
        Ok(Self {
            value,
            raw,
            published: false,
        })
    }

    fn publish(mut self) -> SqlHandle {
        self.published = true;
        self.raw
    }
}

impl<T: Handle> Drop for PendingHandle<T> {
    fn drop(&mut self) {
        if !self.published
            && let Err(error) = retire_handle(&*self.value, self.raw)
        {
            error!(?error, "Rolling back unpublished ODBC handle failed");
        }
    }
}

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

    crate::ffi_entry!("SQLAllocHandle", {
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
            SQL_HANDLE_DESC => unsafe { alloc_desc(input_handle, output_handle) },
            _ => {
                error!(handle_type, "SQLAllocHandle: unknown handle type");
                SQL_INVALID_HANDLE
            }
        }
    })
}

/// Allocates an environment handle.
///
/// Mirrors msodbcsql's `SQLAllocEnv` behavior:
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

    let env = match EnvHandle::new() {
        Ok(e) => e,
        Err(_) => {
            return SQL_ERROR;
        }
    };
    let raw = match PendingHandle::new(env) {
        Ok(env) => env.publish(),
        Err(error) => {
            error!(?error, "SQLAllocHandle(ENV): registration failed");
            return SQL_ERROR;
        }
    };

    unsafe { output_handle.write(raw) };

    debug!(?raw, "Allocated ENV handle");
    SQL_SUCCESS
}

/// Allocates a connection handle under a parent environment.
///
/// Mirrors msodbcsql's `SQLAllocConnect` behavior:
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
    let env = get_handle!(EnvHandle, input_handle);
    debug_assert_eq!(
        env.object_type,
        HandleType::Env,
        "SQLAllocHandle(DBC): input_handle is not an ENV handle"
    );

    let Ok(mut env_state) = env.inner.lock() else {
        error!("SQLAllocHandle(DBC): env mutex poisoned");
        return SQL_ERROR;
    };
    free_errors(&mut env_state);

    // DM enforces SQL_ATTR_ODBC_VERSION is set before SQLAllocConnect (HY010).
    // We assert this in debug builds only.
    debug_assert!(
        env_state.odbc_version != OdbcVersion::Unset,
        "SQLAllocHandle(DBC): SQL_ATTR_ODBC_VERSION not set on env"
    );

    if env_state.connections.try_reserve(1).is_err() {
        post_diag(&mut env_state, ERR_MEMORY_ALLOCATION);
        return SQL_ERROR;
    }
    let dbc = match PendingHandle::new(DbcHandle::new(env.clone_arc())) {
        Ok(dbc) => dbc,
        Err(error) => {
            error!(?error, "SQLAllocHandle(DBC): registration failed");
            post_diag(&mut env_state, ERR_MEMORY_ALLOCATION);
            return SQL_ERROR;
        }
    };
    let raw = dbc.publish();
    env_state.connections.push(raw);

    unsafe { output_handle.write(raw) };

    debug!(?raw, ?input_handle, "Allocated DBC handle");
    SQL_SUCCESS
}

/// Allocates a statement handle under a parent connection.
///
/// Mirrors msodbcsql's `SQLAllocStmt` behavior:
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

    let dbc = get_handle!(DbcHandle, input_handle);
    debug_assert_eq!(
        dbc.object_type,
        HandleType::Dbc,
        "SQLAllocHandle(STMT): input_handle is not a DBC handle"
    );

    let Ok(mut dbc_state) = dbc.inner.lock() else {
        error!("SQLAllocHandle(STMT): dbc mutex poisoned");
        return SQL_ERROR;
    };
    free_errors(&mut dbc_state);

    if dbc_state.statements.try_reserve(1).is_err() {
        post_diag(&mut dbc_state, ERR_MEMORY_ALLOCATION);
        return SQL_ERROR;
    }
    let activity = HandleActivity::new(Some(Arc::clone(&dbc.activity)));
    let mut descriptors = [const { None }; 4];
    for (slot, kind) in descriptors.iter_mut().zip([
        DescKind::AppRow,
        DescKind::AppParam,
        DescKind::ImpRow,
        DescKind::ImpParam,
    ]) {
        let desc = DescHandle::new(
            kind,
            SQL_DESC_ALLOC_AUTO,
            input_handle,
            dbc.clone_arc(),
            HandleActivity::new(Some(Arc::clone(&activity))),
        );
        match PendingHandle::new(desc) {
            Ok(desc) => *slot = Some(desc),
            Err(error) => {
                error!(
                    ?error,
                    "SQLAllocHandle(STMT): implicit descriptor registration failed"
                );
                post_diag(&mut dbc_state, ERR_MEMORY_ALLOCATION);
                return SQL_ERROR;
            }
        }
    }
    let [Some(ard), Some(apd), Some(ird), Some(ipd)] = descriptors else {
        error!("SQLAllocHandle(STMT): implicit descriptors incomplete");
        return SQL_ERROR;
    };
    let stmt = StmtHandle::new(
        input_handle,
        dbc.clone_arc(),
        activity,
        [
            (ard.raw, Arc::clone(&ard.value)),
            (apd.raw, Arc::clone(&apd.value)),
            (ird.raw, Arc::clone(&ird.value)),
            (ipd.raw, Arc::clone(&ipd.value)),
        ],
        dbc_state.stmt_query_timeout,
    );
    let stmt = match PendingHandle::new(stmt) {
        Ok(stmt) => stmt,
        Err(error) => {
            error!(?error, "SQLAllocHandle(STMT): registration failed");
            post_diag(&mut dbc_state, ERR_MEMORY_ALLOCATION);
            return SQL_ERROR;
        }
    };
    for desc in [ard, apd, ird, ipd] {
        desc.publish();
    }
    let raw = stmt.publish();
    dbc_state.statements.push(raw);

    unsafe { output_handle.write(raw) };

    debug!(?raw, ?input_handle, "Allocated STMT handle");
    SQL_SUCCESS
}

/// Allocates an explicit descriptor handle under a parent connection.
///
/// Mirrors msodbcsql's `AllocDesc` (`sqlcdesc.cpp:5841-5908`):
/// 1. Validate that input_handle is a valid DBC handle.
/// 2. Heap-allocate a `DescHandle` tagged `DescKind::Ad` /
///    `SQL_DESC_ALLOC_USER` with a back-pointer to the parent DBC.
/// 3. Acquire the DBC lock and register the descriptor in its descriptor list.
/// 4. Write the opaque pointer to `*output_handle`.
///
/// Descriptors allocated this way are owned by the connection, not by any one
/// statement: `SQLSetStmtAttrW(SQL_ATTR_APP_ROW_DESC/APP_PARAM_DESC)`
/// associates one with any statement on the same connection, and it can
/// outlive that association or be shared by more than one statement at once.
///
/// The ODBC connection state-transition table gates `SQLAllocHandle` for
/// `SQL_HANDLE_DESC` the same as `SQL_HANDLE_STMT` (C4/C5/C6 only, `08003`
/// from C2), and marks that `08003` "(DM)" — generated by the Driver Manager,
/// not the driver. This function does not duplicate that check: msodbcsql's
/// own `AllocDesc` (cited above) has no connection-state check either, and
/// neither does this crate's own `alloc_stmt` for the structurally identical
/// `SQL_HANDLE_STMT` case. A disconnected DBC that reaches this function
/// anyway (bypassing the DM's gate) still can't leave `SQLFreeHandle(DBC)`'s
/// `descriptors.is_empty()` invariant broken: that invariant is just the
/// ordinary "free every child handle before the parent" contract, which
/// holds regardless of connection state.
///
/// # Safety
/// `output_handle` must be a valid, aligned, writable pointer (validated by caller).
/// `input_handle` must be a live `DbcHandle` created by `alloc_dbc`.
unsafe fn alloc_desc(input_handle: SqlHandle, output_handle: *mut SqlHandle) -> SqlReturn {
    if input_handle.is_null() {
        error!("SQLAllocHandle(DESC): input_handle (DBC) must not be null");
        return SQL_INVALID_HANDLE;
    }

    let dbc = get_handle!(DbcHandle, input_handle);
    debug_assert_eq!(
        dbc.object_type,
        HandleType::Dbc,
        "SQLAllocHandle(DESC): input_handle is not a DBC handle"
    );

    let Ok(mut dbc_state) = dbc.inner.lock() else {
        error!("SQLAllocHandle(DESC): dbc mutex poisoned");
        return SQL_ERROR;
    };
    free_errors(&mut dbc_state);

    if dbc_state.descriptors.try_reserve(1).is_err() {
        post_diag(&mut dbc_state, ERR_MEMORY_ALLOCATION);
        return SQL_ERROR;
    }
    let desc = DescHandle::new(
        DescKind::Ad,
        SQL_DESC_ALLOC_USER,
        input_handle,
        dbc.clone_arc(),
        HandleActivity::new(Some(Arc::clone(&dbc.activity))),
    );
    let raw = match PendingHandle::new(desc) {
        Ok(desc) => desc.publish(),
        Err(error) => {
            error!(?error, "SQLAllocHandle(DESC): registration failed");
            post_diag(&mut dbc_state, ERR_MEMORY_ALLOCATION);
            return SQL_ERROR;
        }
    };
    dbc_state.descriptors.push(raw);

    unsafe { output_handle.write(raw) };

    debug!(?raw, ?input_handle, "Allocated DESC handle");
    SQL_SUCCESS
}

#[cfg(test)]
mod tests {
    use std::ptr;

    use super::*;
    use crate::api::free_handle::sql_free_handle;
    use crate::api::odbc_types::{SQL_ATTR_ODBC_VERSION, SQL_OV_ODBC3_80};
    use crate::api::set_env_attr::sql_set_env_attr;
    use crate::handles::{HandleType, free_handle, handle_from_raw};

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
        let env = handle_from_raw::<EnvHandle>(handle).unwrap().into_arc();
        assert_eq!(env.object_type, HandleType::Env);

        // Cleanup
        free_handle::<EnvHandle>(handle).unwrap();
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

        let dbc = handle_from_raw::<DbcHandle>(dbc_handle).unwrap().into_arc();
        assert_eq!(dbc.object_type, HandleType::Dbc);
        let env = handle_from_raw::<EnvHandle>(env_handle).unwrap().into_arc();
        assert!(std::ptr::eq(dbc.parent_env(), &*env));

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

        let env_handle = alloc_env_v3_80();

        let mut dbc_handle: SqlHandle = ptr::null_mut();
        let ret = unsafe { sql_alloc_handle(SQL_HANDLE_DBC, env_handle, &mut dbc_handle) };
        assert_eq!(ret, SQL_SUCCESS);

        let dbc = handle_from_raw::<DbcHandle>(dbc_handle).unwrap().into_arc();
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

        let env = handle_from_raw::<EnvHandle>(handle).unwrap().into_arc();
        let state = env.inner.lock().unwrap();
        assert_eq!(state.odbc_version, OdbcVersion::Unset);
        assert!(state.output_nts);
        drop(state);

        free_handle::<EnvHandle>(handle).unwrap();
    }

    // --- Helper: alloc ENV + DBC for STMT tests ---
    fn alloc_env_dbc() -> (SqlHandle, SqlHandle) {
        let env = alloc_env_v3_80();
        let mut dbc: SqlHandle = ptr::null_mut();
        let ret = unsafe { sql_alloc_handle(SQL_HANDLE_DBC, env, &mut dbc) };
        assert_eq!(ret, SQL_SUCCESS);
        (env, dbc)
    }

    // --- Helper: alloc ENV + DBC, marked connected ---
    // Establishes a Connected state without a real TDS client (same
    // technique as `test_support::TestHandles::mark_dbc_connected`), for
    // tests that want a realistic connected-session baseline. Not required
    // for DESC allocation itself: `alloc_desc` (see its doc comment) doesn't
    // gate on connection state, matching msodbcsql's own `AllocDesc`.
    fn alloc_env_dbc_connected() -> (SqlHandle, SqlHandle) {
        use crate::handles::dbc::ConnectionState;

        let (env, dbc) = alloc_env_dbc();
        let dbc_ref = handle_from_raw::<DbcHandle>(dbc).unwrap().into_arc();
        dbc_ref.inner.lock().unwrap().connection_state = ConnectionState::Connected;
        (env, dbc)
    }

    #[test]
    fn alloc_stmt_returns_success_with_valid_dbc() {
        let (env, dbc) = alloc_env_dbc();

        let mut stmt: SqlHandle = ptr::null_mut();
        let ret = unsafe { sql_alloc_handle(SQL_HANDLE_STMT, dbc, &mut stmt) };
        assert_eq!(ret, SQL_SUCCESS);
        assert!(!stmt.is_null());

        let s = handle_from_raw::<StmtHandle>(stmt).unwrap().into_arc();
        assert_eq!(s.object_type, HandleType::Stmt);
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
        let dbc_ref = handle_from_raw::<DbcHandle>(dbc).unwrap().into_arc();
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

        let dbc_ref = handle_from_raw::<DbcHandle>(dbc).unwrap().into_arc();
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

    #[test]
    fn alloc_desc_returns_success_with_valid_dbc() {
        let (env, dbc) = alloc_env_dbc_connected();

        let mut desc: SqlHandle = ptr::null_mut();
        let ret = unsafe { sql_alloc_handle(SQL_HANDLE_DESC, dbc, &mut desc) };
        assert_eq!(ret, SQL_SUCCESS);
        assert!(!desc.is_null());

        let d = handle_from_raw::<DescHandle>(desc).unwrap().into_arc();
        assert_eq!(d.object_type, HandleType::Desc);
        assert!(d.is_explicit());
        assert_eq!(d.parent_dbc, dbc);

        unsafe { sql_free_handle(SQL_HANDLE_DESC, desc) };
        unsafe { sql_free_handle(SQL_HANDLE_DBC, dbc) };
        unsafe { sql_free_handle(SQL_HANDLE_ENV, env) };
    }

    #[test]
    fn alloc_desc_with_null_dbc_returns_invalid_handle() {
        let mut desc: SqlHandle = ptr::null_mut();
        let ret = unsafe { sql_alloc_handle(SQL_HANDLE_DESC, SQL_NULL_HANDLE, &mut desc) };
        assert_eq!(ret, SQL_INVALID_HANDLE);
        assert!(desc.is_null());
    }

    /// `SQLAllocHandle(SQL_HANDLE_DESC, ...)` on a disconnected DBC succeeds:
    /// the ODBC connection state-transition table's `08003` for this case is
    /// marked "(DM)" — generated by the Driver Manager, not the driver — and
    /// msodbcsql's own `AllocDesc` has no connection-state check, matching
    /// this crate's `alloc_stmt` for the structurally identical
    /// `SQL_HANDLE_STMT` case (see `alloc_desc`'s doc comment).
    #[test]
    fn alloc_desc_on_disconnected_dbc_succeeds() {
        let (env, dbc) = alloc_env_dbc();

        let mut desc: SqlHandle = ptr::null_mut();
        let ret = unsafe { sql_alloc_handle(SQL_HANDLE_DESC, dbc, &mut desc) };
        assert_eq!(ret, SQL_SUCCESS);
        assert!(!desc.is_null());

        let dbc_ref = handle_from_raw::<DbcHandle>(dbc).unwrap().into_arc();
        assert_eq!(dbc_ref.inner.lock().unwrap().descriptors.len(), 1);

        unsafe { sql_free_handle(SQL_HANDLE_DESC, desc) };
        unsafe { sql_free_handle(SQL_HANDLE_DBC, dbc) };
        unsafe { sql_free_handle(SQL_HANDLE_ENV, env) };
    }

    #[test]
    fn alloc_desc_registers_in_parent_dbc() {
        let (env, dbc) = alloc_env_dbc_connected();

        let dbc_ref = handle_from_raw::<DbcHandle>(dbc).unwrap().into_arc();
        assert!(dbc_ref.inner.lock().unwrap().descriptors.is_empty());

        let mut desc: SqlHandle = ptr::null_mut();
        assert_eq!(
            unsafe { sql_alloc_handle(SQL_HANDLE_DESC, dbc, &mut desc) },
            SQL_SUCCESS
        );

        let state = dbc_ref.inner.lock().unwrap();
        assert_eq!(state.descriptors.len(), 1);
        assert_eq!(state.descriptors[0], desc);
        drop(state);

        unsafe { sql_free_handle(SQL_HANDLE_DESC, desc) };
        unsafe { sql_free_handle(SQL_HANDLE_DBC, dbc) };
        unsafe { sql_free_handle(SQL_HANDLE_ENV, env) };
    }

    #[test]
    fn alloc_multiple_descs_on_same_dbc() {
        let (env, dbc) = alloc_env_dbc_connected();

        let mut desc1: SqlHandle = ptr::null_mut();
        let mut desc2: SqlHandle = ptr::null_mut();
        assert_eq!(
            unsafe { sql_alloc_handle(SQL_HANDLE_DESC, dbc, &mut desc1) },
            SQL_SUCCESS
        );
        assert_eq!(
            unsafe { sql_alloc_handle(SQL_HANDLE_DESC, dbc, &mut desc2) },
            SQL_SUCCESS
        );
        assert_ne!(desc1, desc2);

        let dbc_ref = handle_from_raw::<DbcHandle>(dbc).unwrap().into_arc();
        assert_eq!(dbc_ref.inner.lock().unwrap().descriptors.len(), 2);

        unsafe { sql_free_handle(SQL_HANDLE_DESC, desc2) };
        unsafe { sql_free_handle(SQL_HANDLE_DESC, desc1) };
        unsafe { sql_free_handle(SQL_HANDLE_DBC, dbc) };
        unsafe { sql_free_handle(SQL_HANDLE_ENV, env) };
    }
}
