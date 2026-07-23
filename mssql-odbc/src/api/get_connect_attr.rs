// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Implementation of SQLGetConnectAttrW.
//!
//! Reports `SQL_ATTR_LOGIN_TIMEOUT` from the stored connection state so a
//! set/get round-trip returns the configured value (matching msodbcsql). Other
//! attributes are accepted without writing, mirroring the set-side coverage.

use tracing::{debug, error};

use super::sqlstate::*;
use crate::api::odbc_types::{
    SQL_ATTR_LOGIN_TIMEOUT, SQL_ERROR, SQL_INVALID_HANDLE, SQL_SUCCESS, SqlHandle, SqlInteger,
    SqlPointer, SqlReturn,
};
use crate::error::{free_errors, post_sql_error};
use crate::handles::{DbcHandle, HandleType, handle_from_raw};

/// Login timeout reported when the application has not set
/// `SQL_ATTR_LOGIN_TIMEOUT`. Mirrors `ClientContext::connect_timeout`'s default,
/// which is what the connect path falls back to when no explicit login timeout
/// is present.
const DEFAULT_LOGIN_TIMEOUT_SECS: u32 = 15;

/// Retrieves a connection attribute.
///
/// # Safety
/// - `connection_handle` must be a valid `DbcHandle` from `SQLAllocHandle`.
/// - For `SQL_ATTR_LOGIN_TIMEOUT`, `value_ptr` must point to a writable
///   `SQLUINTEGER`.
pub(crate) unsafe fn sql_get_connect_attr_w(
    connection_handle: SqlHandle,
    attribute: SqlInteger,
    value_ptr: SqlPointer,
    buffer_length: SqlInteger,
    string_length_ptr: *mut SqlInteger,
) -> SqlReturn {
    debug!(
        ?connection_handle,
        attribute,
        ?value_ptr,
        "SQLGetConnectAttrW called",
    );

    crate::ffi_entry!("SQLGetConnectAttrW", unsafe {
        sql_get_connect_attr_w_impl(
            connection_handle,
            attribute,
            value_ptr,
            buffer_length,
            string_length_ptr,
        )
    })
}

unsafe fn sql_get_connect_attr_w_impl(
    connection_handle: SqlHandle,
    attribute: SqlInteger,
    value_ptr: SqlPointer,
    _buffer_length: SqlInteger,
    _string_length_ptr: *mut SqlInteger,
) -> SqlReturn {
    if connection_handle.is_null() {
        error!("SQLGetConnectAttrW: connection_handle is null");
        return SQL_INVALID_HANDLE;
    }

    let dbc = unsafe { handle_from_raw::<DbcHandle>(connection_handle) };
    debug_assert_eq!(
        dbc.object_type,
        HandleType::Dbc,
        "SQLGetConnectAttrW: handle is not a DBC"
    );

    let Ok(mut state) = dbc.inner.lock() else {
        error!("SQLGetConnectAttrW: dbc mutex poisoned");
        return SQL_ERROR;
    };
    free_errors(&mut state);

    match attribute {
        SQL_ATTR_LOGIN_TIMEOUT => {
            if value_ptr.is_null() {
                error!("SQLGetConnectAttrW: SQL_ATTR_LOGIN_TIMEOUT value pointer is null");
                post_sql_error(
                    &mut state,
                    SQLSTATE_HY009,
                    0,
                    "SQL_ATTR_LOGIN_TIMEOUT value pointer is null",
                );
                return SQL_ERROR;
            }
            // SQLUINTEGER attribute: write the current value into the caller's
            // buffer. `Some(0)` reflects an app-set "wait indefinitely"; an
            // unset attribute reports the driver default.
            let secs = state.login_timeout.unwrap_or(DEFAULT_LOGIN_TIMEOUT_SECS);
            unsafe { (value_ptr as *mut u32).write_unaligned(secs) };
            debug!(secs, "SQLGetConnectAttrW: login timeout returned");
            SQL_SUCCESS
        }
        // Attributes not backed by stored state report success without writing,
        // matching the historical stub behavior.
        _ => SQL_SUCCESS,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::set_connect_attr::sql_set_connect_attr_w;
    use crate::test_support::TestHandles;

    #[test]
    fn login_timeout_set_get_round_trips() {
        let h = TestHandles::with_env_dbc();
        let set = unsafe {
            sql_set_connect_attr_w(h.dbc, SQL_ATTR_LOGIN_TIMEOUT, 42usize as SqlPointer, 0)
        };
        assert_eq!(set, SQL_SUCCESS);

        let mut out: u32 = 0;
        let get = unsafe {
            sql_get_connect_attr_w(
                h.dbc,
                SQL_ATTR_LOGIN_TIMEOUT,
                &mut out as *mut u32 as SqlPointer,
                0,
                std::ptr::null_mut(),
            )
        };
        assert_eq!(get, SQL_SUCCESS);
        assert_eq!(out, 42);
    }

    #[test]
    fn login_timeout_get_reports_default_when_unset() {
        let h = TestHandles::with_env_dbc();
        let mut out: u32 = 999;
        let get = unsafe {
            sql_get_connect_attr_w(
                h.dbc,
                SQL_ATTR_LOGIN_TIMEOUT,
                &mut out as *mut u32 as SqlPointer,
                0,
                std::ptr::null_mut(),
            )
        };
        assert_eq!(get, SQL_SUCCESS);
        assert_eq!(out, DEFAULT_LOGIN_TIMEOUT_SECS);
    }

    #[test]
    fn login_timeout_get_null_pointer_is_rejected() {
        let h = TestHandles::with_env_dbc();
        let get = unsafe {
            sql_get_connect_attr_w(
                h.dbc,
                SQL_ATTR_LOGIN_TIMEOUT,
                std::ptr::null_mut(),
                0,
                std::ptr::null_mut(),
            )
        };
        assert_eq!(get, SQL_ERROR);
    }
}
