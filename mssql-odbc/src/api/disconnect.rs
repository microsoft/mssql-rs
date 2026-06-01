// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Implementation of SQLDisconnect — close a connection to a data source.

use std::panic;

use tracing::{debug, error, trace};

use crate::api::odbc_types::{SQL_ERROR, SQL_INVALID_HANDLE, SQL_SUCCESS, SqlHandle, SqlReturn};
use crate::handles::DbcHandle;
use crate::handles::StmtHandle;
use crate::handles::dbc::ConnectionState;
use crate::handles::{HandleType, free_handle, handle_from_raw};

/// Implementation of `SQLDisconnect`.
///
/// # Safety
/// - `connection_handle` must be a valid `DbcHandle` previously connected via `SQLDriverConnectW`.
pub(crate) unsafe fn sql_disconnect(connection_handle: SqlHandle) -> SqlReturn {
    debug!("SQLDisconnect called");

    let result = panic::catch_unwind(|| unsafe { sql_disconnect_impl(connection_handle) });

    let ret = result.unwrap_or_else(|_| {
        error!("SQLDisconnect: panic caught at FFI boundary");
        SQL_ERROR
    });

    trace!(?ret, "SQLDisconnect returning");
    ret
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

    let Ok(mut state) = dbc.inner.lock() else {
        error!("SQLDisconnect: dbc mutex poisoned");
        return SQL_ERROR;
    };

    if state.connection_state != ConnectionState::Connected {
        error!("SQLDisconnect: not connected");
        // TODO: post SQLSTATE 08003
        return SQL_ERROR;
    }

    // TODO: check for active local transaction → post SQLSTATE 25000
    // TODO: free user-allocated descriptors (once descriptor support is added)

    // Drop all child STMT handles.
    // Safety: the DBC lock is held, so no new statement operation can start (they
    // need the DBC to access the TDS client). The stmt lock drains any in-flight op.
    for &stmt_ptr in &state.statements {
        let stmt = unsafe { handle_from_raw::<StmtHandle>(stmt_ptr) };
        // Acquire and immediately release — waits for any in-flight operation to complete.
        let Ok(guard) = stmt.inner.lock() else {
            error!(?stmt_ptr, "SQLDisconnect: stmt mutex poisoned");
            return SQL_ERROR;
        };
        drop(guard);
        unsafe { free_handle::<StmtHandle>(stmt_ptr) };
    }
    state.statements.clear();

    // Drop the TDS client (closes the connection)
    state.client = None;
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
}
