// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Implementation of SQLGetConnectAttrW.

use tracing::{debug, error};

use super::sqlstate::*;
use crate::api::odbc_types::{
    SQL_ATTR_ACCESS_MODE, SQL_ATTR_AUTOCOMMIT, SQL_ATTR_CONNECTION_DEAD,
    SQL_ATTR_CONNECTION_TIMEOUT, SQL_ATTR_LOGIN_TIMEOUT, SQL_ATTR_PACKET_SIZE,
    SQL_ATTR_TXN_ISOLATION, SQL_AUTOCOMMIT_OFF, SQL_AUTOCOMMIT_ON, SQL_CD_FALSE, SQL_CD_TRUE,
    SQL_ERROR, SQL_INVALID_HANDLE, SQL_SUCCESS, SqlHandle, SqlInteger, SqlPointer, SqlReturn,
};
use crate::api::util::write_if_some;
use crate::error::free_errors;
use crate::handles::dbc::ConnectionState;
use crate::handles::{DbcHandle, HandleType, handle_from_raw};

/// ODBC read-only access mode indicator; the driver never restricts writes.
const SQL_MODE_READ_WRITE: u32 = 0;

/// Retrieves the current setting of a connection attribute.
///
/// Only fixed-length (`SQLUINTEGER`) attributes are supported; string-valued
/// attributes report `HYC00`.
///
/// # Safety
/// - `connection_handle` must be a valid `DbcHandle` from `SQLAllocHandle`.
/// - `value_ptr`, when non-null, must be writable for the attribute's type
///   (4 bytes for every attribute this driver answers).
/// - `string_length_ptr`, when non-null, must be writable for one `SqlInteger`.
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
        buffer_length,
        "SQLGetConnectAttrW called",
    );

    crate::ffi_entry!("SQLGetConnectAttrW", unsafe {
        sql_get_connect_attr_w_impl(connection_handle, attribute, value_ptr, string_length_ptr)
    })
}

unsafe fn sql_get_connect_attr_w_impl(
    connection_handle: SqlHandle,
    attribute: SqlInteger,
    value_ptr: SqlPointer,
    string_length_ptr: *mut SqlInteger,
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

    let value: u32 = match attribute {
        SQL_ATTR_AUTOCOMMIT => {
            if state.autocommit {
                SQL_AUTOCOMMIT_ON
            } else {
                SQL_AUTOCOMMIT_OFF
            }
        }
        SQL_ATTR_TXN_ISOLATION => state.txn_isolation,
        SQL_ATTR_CONNECTION_DEAD => {
            if state.dead || state.connection_state != ConnectionState::Connected {
                SQL_CD_TRUE
            } else {
                SQL_CD_FALSE
            }
        }
        SQL_ATTR_ACCESS_MODE => SQL_MODE_READ_WRITE,
        // Timeouts and packet size are accepted but not enforced; report the
        // ODBC defaults rather than an error so generic callers keep working.
        SQL_ATTR_LOGIN_TIMEOUT | SQL_ATTR_CONNECTION_TIMEOUT => 0,
        SQL_ATTR_PACKET_SIZE => 0,
        _ => {
            error!(attribute, "SQLGetConnectAttrW: unsupported attribute");
            post_diag(&mut state, ERR_OPTIONAL_FEATURE_NOT_IMPLEMENTED);
            return SQL_ERROR;
        }
    };

    if value_ptr.is_null() {
        error!(attribute, "SQLGetConnectAttrW: value_ptr is null");
        post_diag(&mut state, ERR_INVALID_NULL_POINTER);
        return SQL_ERROR;
    }

    // SAFETY: every attribute answered above is a fixed-length SQLUINTEGER, and
    // the caller guarantees `value_ptr` is writable for that type.
    unsafe { std::ptr::write_unaligned(value_ptr as *mut u32, value) };
    unsafe { write_if_some(string_length_ptr, size_of::<u32>() as SqlInteger) };
    SQL_SUCCESS
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::odbc_types::SQL_NULL_HANDLE;
    use crate::test_support::TestHandles;

    fn get_attr(handle: SqlHandle, attribute: SqlInteger) -> (SqlReturn, u32) {
        let mut value: u32 = u32::MAX;
        let ret = unsafe {
            sql_get_connect_attr_w(
                handle,
                attribute,
                std::ptr::from_mut(&mut value) as SqlPointer,
                size_of::<u32>() as SqlInteger,
                std::ptr::null_mut(),
            )
        };
        (ret, value)
    }

    #[test]
    fn null_handle_returns_invalid_handle() {
        let (ret, _) = get_attr(SQL_NULL_HANDLE, SQL_ATTR_AUTOCOMMIT);
        assert_eq!(ret, SQL_INVALID_HANDLE);
    }

    #[test]
    fn autocommit_defaults_to_on() {
        let h = TestHandles::with_env_dbc();
        let (ret, value) = get_attr(h.dbc, SQL_ATTR_AUTOCOMMIT);
        assert_eq!(ret, SQL_SUCCESS);
        assert_eq!(value, SQL_AUTOCOMMIT_ON);
    }

    #[test]
    fn disconnected_connection_reports_dead() {
        let h = TestHandles::with_env_dbc();
        let (ret, value) = get_attr(h.dbc, SQL_ATTR_CONNECTION_DEAD);
        assert_eq!(ret, SQL_SUCCESS);
        assert_eq!(value, SQL_CD_TRUE);
    }

    #[test]
    fn connected_connection_reports_alive() {
        let h = TestHandles::with_env_dbc();
        h.mark_dbc_connected();
        let (ret, value) = get_attr(h.dbc, SQL_ATTR_CONNECTION_DEAD);
        assert_eq!(ret, SQL_SUCCESS);
        assert_eq!(value, SQL_CD_FALSE);
    }

    #[test]
    fn unsupported_attribute_returns_error() {
        let h = TestHandles::with_env_dbc();
        let (ret, _) = get_attr(h.dbc, 4242);
        assert_eq!(ret, SQL_ERROR);
    }

    #[test]
    fn null_value_pointer_is_rejected() {
        let h = TestHandles::with_env_dbc();
        let ret = unsafe {
            sql_get_connect_attr_w(
                h.dbc,
                SQL_ATTR_AUTOCOMMIT,
                std::ptr::null_mut(),
                0,
                std::ptr::null_mut(),
            )
        };
        assert_eq!(ret, SQL_ERROR);
    }
}
