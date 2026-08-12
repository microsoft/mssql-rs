// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Implementation of SQLGetConnectAttrW.
//!
//! Reports the attributes `SQLSetConnectAttrW` accepts, so a set/get round-trip
//! returns the configured value (matching msodbcsql, which answers
//! `SQL_ATTR_ACCESS_MODE`, `SQL_ATTR_PACKET_SIZE` and the two timeouts at
//! `sqlcmisc.cpp:3038-3391`). Any other attribute is unsupported and returns
//! `HYC00` rather than claiming success without writing.

use mssql_tds::connection::client_context::DEFAULT_CONNECT_TIMEOUT_SECS;
use tracing::{debug, error};

use super::sqlstate::*;
use crate::api::odbc_types::{
    SQL_ATTR_ACCESS_MODE, SQL_ATTR_AUTOCOMMIT, SQL_ATTR_CONNECTION_TIMEOUT, SQL_ATTR_LOGIN_TIMEOUT,
    SQL_ATTR_PACKET_SIZE, SQL_ATTR_TXN_ISOLATION, SQL_AUTOCOMMIT_OFF, SQL_AUTOCOMMIT_ON, SQL_ERROR,
    SQL_INVALID_HANDLE, SQL_SUCCESS, SqlHandle, SqlInteger, SqlPointer, SqlReturn,
};
use crate::api::util::write_if_some;
use crate::error::{free_errors, post_sql_error};
use crate::handles::{DbcHandle, HandleType, handle_from_raw};

/// Login timeout reported when the application has not set
/// `SQL_ATTR_LOGIN_TIMEOUT`, which is what the connect path falls back to.
const DEFAULT_LOGIN_TIMEOUT_SECS: u32 = DEFAULT_CONNECT_TIMEOUT_SECS;

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
        buffer_length,
        ?string_length_ptr,
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
    buffer_length: SqlInteger,
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

    sql_get_connect_attr_w_safe(dbc, attribute, value_ptr, buffer_length, string_length_ptr)
}

fn sql_get_connect_attr_w_safe(
    dbc: &DbcHandle,
    attribute: SqlInteger,
    value_ptr: SqlPointer,
    _buffer_length: SqlInteger,
    _string_length_ptr: *mut SqlInteger,
) -> SqlReturn {
    let Ok(mut state) = dbc.inner.lock() else {
        error!("SQLGetConnectAttrW: dbc mutex poisoned");
        return SQL_ERROR;
    };
    free_errors(&mut state);

    match attribute {
        SQL_ATTR_LOGIN_TIMEOUT => {
            if value_ptr.is_null() {
                error!("SQLGetConnectAttrW: SQL_ATTR_LOGIN_TIMEOUT value pointer is null");
                post_diag(&mut state, ERR_INVALID_NULL_POINTER);
                return SQL_ERROR;
            }
            // SQLUINTEGER attribute: write the current value into the caller's
            // buffer. `Some(0)` reflects an app-set "wait indefinitely"; an
            // unset attribute reports the driver default.
            let secs = state.login_timeout.unwrap_or(DEFAULT_LOGIN_TIMEOUT_SECS);
            unsafe { write_if_some(value_ptr as *mut u32, secs) };
            debug!(secs, "SQLGetConnectAttrW: login timeout returned");
            SQL_SUCCESS
        }
        // The remaining attributes the set-side accepts. Returning HYC00 here
        // would make a set/get round-trip fail for a value the driver had just
        // reported as accepted.
        SQL_ATTR_ACCESS_MODE
        | SQL_ATTR_CONNECTION_TIMEOUT
        | SQL_ATTR_PACKET_SIZE
        | SQL_ATTR_AUTOCOMMIT
        | SQL_ATTR_TXN_ISOLATION => {
            if value_ptr.is_null() {
                error!(attribute, "SQLGetConnectAttrW: value pointer is null");
                post_diag(&mut state, ERR_INVALID_NULL_POINTER);
                return SQL_ERROR;
            }
            let value = match attribute {
                SQL_ATTR_ACCESS_MODE => state.access_mode,
                SQL_ATTR_CONNECTION_TIMEOUT => state.connection_timeout,
                // Read from the cached value rather than the server: msodbcsql
                // does the same for both (`sqlcmisc.cpp:3426`), so a get never
                // costs a round trip.
                SQL_ATTR_AUTOCOMMIT => {
                    if state.autocommit {
                        SQL_AUTOCOMMIT_ON
                    } else {
                        SQL_AUTOCOMMIT_OFF
                    }
                }
                SQL_ATTR_TXN_ISOLATION => state.txn_isolation,
                _ => state.packet_size,
            };
            unsafe { write_if_some(value_ptr as *mut u32, value) };
            debug!(attribute, value, "SQLGetConnectAttrW: attribute returned");
            SQL_SUCCESS
        }
        // Any other attribute is genuinely unsupported: surface HYC00 instead of
        // claiming success while leaving the caller's buffer untouched.
        // `SQL_ATTR_ANSI_APP` lands here deliberately — the Driver Manager sets
        // it and ODBC defines no way to read it back.
        _ => {
            error!(
                attribute,
                "SQLGetConnectAttrW: unsupported connection attribute"
            );
            post_sql_error(
                &mut state,
                SQLSTATE_HYC00,
                0,
                "Connection attribute not supported",
            );
            SQL_ERROR
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::odbc_types::{
        DEFAULT_PACKET_SIZE, SQL_ATTR_ANSI_APP, SQL_MODE_READ_WRITE, SQL_TXN_READ_COMMITTED,
        SQL_TXN_SS_SNAPSHOT,
    };
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

    #[test]
    fn unsupported_attribute_returns_error() {
        let h = TestHandles::with_env_dbc();
        // 1234 is an arbitrary unhandled attribute id -> HYC00, not silent
        // success, matching the set-side.
        let mut out: u32 = 0;
        let get = unsafe {
            sql_get_connect_attr_w(
                h.dbc,
                1234,
                &mut out as *mut u32 as SqlPointer,
                0,
                std::ptr::null_mut(),
            )
        };
        assert_eq!(get, SQL_ERROR);
    }

    /// Reads an attribute back, asserting the call succeeded.
    fn get_u32(dbc: crate::api::odbc_types::SqlHandle, attribute: SqlInteger) -> u32 {
        let mut out: u32 = 0;
        let rc = unsafe {
            sql_get_connect_attr_w(
                dbc,
                attribute,
                &mut out as *mut u32 as SqlPointer,
                0,
                std::ptr::null_mut(),
            )
        };
        assert_eq!(rc, SQL_SUCCESS, "attribute {attribute} should be readable");
        out
    }

    #[test]
    fn every_attribute_the_set_side_accepts_can_be_read_back() {
        // The set side reported success for these, so the get side must not
        // answer HYC00 for a value it has just accepted.
        let h = TestHandles::with_env_dbc();
        for (attribute, value) in [
            (SQL_ATTR_ACCESS_MODE, 1u32),
            (SQL_ATTR_CONNECTION_TIMEOUT, 30),
            (SQL_ATTR_PACKET_SIZE, 16384),
            (SQL_ATTR_AUTOCOMMIT, SQL_AUTOCOMMIT_OFF),
            (SQL_ATTR_TXN_ISOLATION, SQL_TXN_SS_SNAPSHOT),
        ] {
            let set = unsafe {
                sql_set_connect_attr_w(h.dbc, attribute, value as usize as SqlPointer, 0)
            };
            assert_eq!(set, SQL_SUCCESS, "setting {attribute}");
            assert_eq!(get_u32(h.dbc, attribute), value, "reading {attribute} back");
        }
    }

    #[test]
    fn accepted_attributes_report_defaults_when_unset() {
        let h = TestHandles::with_env_dbc();
        assert_eq!(get_u32(h.dbc, SQL_ATTR_ACCESS_MODE), SQL_MODE_READ_WRITE);
        assert_eq!(
            get_u32(h.dbc, SQL_ATTR_CONNECTION_TIMEOUT),
            0,
            "ODBC default is no timeout"
        );
        assert_eq!(get_u32(h.dbc, SQL_ATTR_PACKET_SIZE), DEFAULT_PACKET_SIZE);
        assert_eq!(
            get_u32(h.dbc, SQL_ATTR_AUTOCOMMIT),
            SQL_AUTOCOMMIT_ON,
            "ODBC and msodbcsql both default to autocommit"
        );
        assert_eq!(
            get_u32(h.dbc, SQL_ATTR_TXN_ISOLATION),
            SQL_TXN_READ_COMMITTED
        );
    }

    #[test]
    fn ansi_app_is_not_readable() {
        // Set by the Driver Manager; ODBC defines no way to retrieve it, and
        // msodbcsql's get switch has no arm for it either.
        let h = TestHandles::with_env_dbc();
        let mut out: u32 = 0;
        let get = unsafe {
            sql_get_connect_attr_w(
                h.dbc,
                SQL_ATTR_ANSI_APP,
                &mut out as *mut u32 as SqlPointer,
                0,
                std::ptr::null_mut(),
            )
        };
        assert_eq!(get, SQL_ERROR);
    }
}
