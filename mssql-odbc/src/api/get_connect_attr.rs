// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Implementation of SQLGetConnectAttrW.
//!
//! Reports the attributes `SQLSetConnectAttrW` accepts, so a set/get round-trip
//! returns the configured value (matching msodbcsql, which answers
//! `SQL_ATTR_ACCESS_MODE`, `SQL_ATTR_PACKET_SIZE` and the two timeouts at
//! `sqlcmisc.cpp:3038-3391`). `SQL_ATTR_CURRENT_CATALOG` is the one character
//! attribute. Any other identifier returns `HY092` rather than claiming success
//! without writing.

use mssql_tds::connection::client_context::DEFAULT_CONNECT_TIMEOUT_SECS;
use tracing::{debug, error};

use super::current_catalog::get_current_catalog;
use super::sqlstate::*;
use crate::api::attributes::{AttrOp, AttrScope, unimplemented_attr_diag};
use crate::api::odbc_types::{
    SQL_ATTR_ACCESS_MODE, SQL_ATTR_AUTOCOMMIT, SQL_ATTR_CONNECTION_DEAD,
    SQL_ATTR_CONNECTION_TIMEOUT, SQL_ATTR_CURRENT_CATALOG, SQL_ATTR_LOGIN_TIMEOUT,
    SQL_ATTR_PACKET_SIZE, SQL_ATTR_TXN_ISOLATION, SQL_AUTOCOMMIT_OFF, SQL_AUTOCOMMIT_ON,
    SQL_CD_FALSE, SQL_CD_TRUE, SQL_COPT_SS_ENCRYPT, SQL_COPT_SS_INTEGRATED_SECURITY,
    SQL_COPT_SS_TRUST_SERVER_CERTIFICATE, SQL_COPT_SS_TXN_ISOLATION, SQL_EN_ON, SQL_ERROR,
    SQL_INVALID_HANDLE, SQL_SUCCESS, SqlHandle, SqlInteger, SqlPointer, SqlReturn,
};
use crate::api::util::write_if_some;
use crate::error::free_errors;
use crate::handles::dbc::ConnectionState;
use crate::handles::{DbcHandle, HandleType, handle_from_raw};

/// Login timeout reported when the application has not set
/// `SQL_ATTR_LOGIN_TIMEOUT`, which is what the connect path falls back to.
const DEFAULT_LOGIN_TIMEOUT_SECS: u32 = DEFAULT_CONNECT_TIMEOUT_SECS;

/// Retrieves a connection attribute.
///
/// # Safety
/// - `connection_handle` must be a valid `DbcHandle` from `SQLAllocHandle`.
/// - `value_ptr`, when non-null, must be writable for `buffer_length` bytes for
///   `SQL_ATTR_CURRENT_CATALOG`, or for one value of the requested fixed-width
///   attribute type otherwise.
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

/// # Safety
/// `connection_handle` must be null or point to a live `DbcHandle`.
/// `value_ptr`, when non-null, must be writable for `buffer_length` bytes for
/// `SQL_ATTR_CURRENT_CATALOG`, or for one value of the requested fixed-width
/// attribute type otherwise. `string_length_ptr`, when non-null, must be
/// writable for one `SqlInteger`.
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
    buffer_length: SqlInteger,
    string_length_ptr: *mut SqlInteger,
) -> SqlReturn {
    // The only character attribute: it does its own locking and writes a
    // length-delimited buffer rather than a fixed-width integer.
    //
    // SAFETY: the caller guarantees `value_ptr` is null or writable for
    // `buffer_length` bytes, and `string_length_ptr` is null or writable for one
    // `SQLINTEGER`.
    if attribute == SQL_ATTR_CURRENT_CATALOG {
        return unsafe { get_current_catalog(dbc, value_ptr, buffer_length, string_length_ptr) };
    }

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
        | SQL_ATTR_TXN_ISOLATION
        | SQL_COPT_SS_TXN_ISOLATION => {
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
                SQL_ATTR_TXN_ISOLATION | SQL_COPT_SS_TXN_ISOLATION => state.txn_isolation,
                _ => state.packet_size,
            };
            unsafe { write_if_some(value_ptr as *mut u32, value) };
            debug!(attribute, value, "SQLGetConnectAttrW: attribute returned");
            SQL_SUCCESS
        }
        // The `SQL_COPT_SS_*` forms of Encrypt / TrustServerCertificate /
        // Trusted_Connection. While connected this reports the resolved setting,
        // whether it came from the attribute or the keyword -- measured: with no
        // attribute set, `Encrypt=no` reads back 0.
        //
        // Before connect there is nothing to resolve, so an unset attribute falls
        // back to the driver defaults (encryption on, no trust, no integrated
        // auth). That pre-connect case is inferred from the defaults, not
        // measured, unlike the post-connect behaviour above.
        SQL_COPT_SS_ENCRYPT
        | SQL_COPT_SS_TRUST_SERVER_CERTIFICATE
        | SQL_COPT_SS_INTEGRATED_SECURITY => {
            if value_ptr.is_null() {
                error!(attribute, "SQLGetConnectAttrW: value pointer is null");
                post_diag(&mut state, ERR_INVALID_NULL_POINTER);
                return SQL_ERROR;
            }
            let settings = if state.connection_state == ConnectionState::Connected {
                state.effective_vendor_settings.as_ref()
            } else {
                None
            }
            .unwrap_or(&state.vendor_overrides);
            let value = match attribute {
                SQL_COPT_SS_ENCRYPT => settings.encrypt.unwrap_or(SQL_EN_ON as u32),
                SQL_COPT_SS_TRUST_SERVER_CERTIFICATE => {
                    settings.trust_server_certificate.unwrap_or(0)
                }
                _ => settings.integrated_security.unwrap_or(0),
            };
            unsafe { write_if_some(value_ptr as *mut u32, value) };
            debug!(
                attribute,
                value, "SQLGetConnectAttrW: vendor attribute returned"
            );
            SQL_SUCCESS
        }
        SQL_ATTR_CONNECTION_DEAD => {
            if value_ptr.is_null() {
                error!("SQLGetConnectAttrW: SQL_ATTR_CONNECTION_DEAD value pointer is null");
                post_diag(&mut state, ERR_INVALID_NULL_POINTER);
                return SQL_ERROR;
            }
            // Cached, never-probe liveness (D1/D2): report DEAD unless the DBC is
            // connected and its client has not been observed dead. Peek the
            // client under the DBC mutex — never `take` it (D8) and never touch
            // the socket. `SQL_CD_FALSE` means "not known dead", not "proven
            // healthy"; a connection that failed silently while idle is
            // recovered on the next operation. Disconnected/never-connected
            // reads DEAD so a pool discards it.
            //
            // A connected DBC whose client is momentarily absent is mid-claim:
            // another thread owns it for I/O (`claim_dbc_client` /
            // `claim_connection`). That is an in-flight healthy connection, not
            // a dead one, so it reads "not known dead" — reporting DEAD here
            // would let a concurrent pool liveness check discard a connection
            // that is merely busy.
            let known_dead = match state.connection_state {
                ConnectionState::Connected => state
                    .client
                    .as_ref()
                    .is_some_and(|client| client.is_connection_dead()),
                _ => true,
            };
            let value = if known_dead {
                SQL_CD_TRUE
            } else {
                SQL_CD_FALSE
            };
            unsafe { write_if_some(value_ptr as *mut u32, value) };
            debug!(value, "SQLGetConnectAttrW: connection-dead returned");
            SQL_SUCCESS
        }
        // Any other identifier is not one this driver reports: surface a
        // diagnostic rather than claiming success while leaving the caller's
        // buffer untouched. Which diagnostic depends on whether msodbcsql
        // recognizes the identifier *on the get path* — recognition is not
        // symmetric. `SQL_ATTR_ANSI_APP` lands here deliberately (the Driver
        // Manager sets it and ODBC defines no way to read it back), and so does
        // `SQL_ATTR_QUERY_TIMEOUT`, which msodbcsql accepts on a connection but
        // refuses to report back; both stay `HY092`. See `attributes.rs`.
        _ => {
            post_diag(
                &mut state,
                unimplemented_attr_diag(AttrScope::Dbc, AttrOp::Get, attribute),
            );
            SQL_ERROR
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::odbc_types::{
        DEFAULT_PACKET_SIZE, SQL_ATTR_ANSI_APP, SQL_ATTR_CONNECTION_DEAD, SQL_ATTR_QUERY_TIMEOUT,
        SQL_CD_FALSE, SQL_CD_TRUE, SQL_MODE_READ_WRITE, SQL_TXN_READ_COMMITTED,
        SQL_TXN_SS_SNAPSHOT,
    };
    use crate::api::set_connect_attr::sql_set_connect_attr_w;
    use crate::handles::dbc::VendorConnOverrides;
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
        // 99999 is not an attribute identifier in any namespace -> HY092, not
        // silent success, matching the set-side and msodbcsql.
        let mut out: u32 = 0;
        let get = unsafe {
            sql_get_connect_attr_w(
                h.dbc,
                99999,
                &mut out as *mut u32 as SqlPointer,
                0,
                std::ptr::null_mut(),
            )
        };
        assert_eq!(get, SQL_ERROR);
        let dbc = unsafe { handle_from_raw::<DbcHandle>(h.dbc) };
        let state = dbc.inner.lock().unwrap();
        assert_eq!(state.diag_records[0].sql_state, SQLSTATE_HY092);
    }

    /// `SQL_ATTR_QUERY_TIMEOUT` is settable on a connection but deliberately not
    /// readable from one — msodbcsql answers `HY092`, and mssql-python reads the
    /// timeout back from the statement handle, never the connection.
    #[test]
    fn query_timeout_is_write_only_on_a_connection() {
        let h = TestHandles::with_env_dbc();
        let set = unsafe {
            sql_set_connect_attr_w(h.dbc, SQL_ATTR_QUERY_TIMEOUT, 17usize as SqlPointer, 0)
        };
        assert_eq!(set, SQL_SUCCESS);

        let mut out: u32 = 0;
        let get = unsafe {
            sql_get_connect_attr_w(
                h.dbc,
                SQL_ATTR_QUERY_TIMEOUT,
                &mut out as *mut u32 as SqlPointer,
                0,
                std::ptr::null_mut(),
            )
        };
        assert_eq!(get, SQL_ERROR);
        let dbc = unsafe { handle_from_raw::<DbcHandle>(h.dbc) };
        let state = dbc.inner.lock().unwrap();
        assert_eq!(state.diag_records[0].sql_state, SQLSTATE_HY092);
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

    /// `SQL_COPT_SS_MARS_ENABLED` (1224) is readable from msodbcsql, so a
    /// caller probing for MARS must get `HYC00` here rather than `HY092`.
    /// Contrast `query_timeout_is_write_only_on_a_connection`: msodbcsql's
    /// recognized set is not the same for the set and get paths, so the lookup
    /// is keyed by operation as well as scope.
    #[test]
    fn attribute_known_to_msodbcsql_reports_not_implemented() {
        let h = TestHandles::with_env_dbc();
        let mut out: u32 = 0;
        let get = unsafe {
            sql_get_connect_attr_w(
                h.dbc,
                1224,
                &mut out as *mut u32 as SqlPointer,
                0,
                std::ptr::null_mut(),
            )
        };
        assert_eq!(get, SQL_ERROR);
        let dbc = unsafe { handle_from_raw::<DbcHandle>(h.dbc) };
        let state = dbc.inner.lock().unwrap();
        assert_eq!(state.diag_records[0].sql_state, SQLSTATE_HYC00);
    }

    #[test]
    fn connection_dead_reports_true_when_never_connected() {
        // D1: msodbcsql defaults the attribute to DEAD until a token read
        // succeeds; a freshly allocated DBC has no client, so a pool must
        // discard it.
        let h = TestHandles::with_env_dbc();
        let mut out: u32 = 12345;
        let get = unsafe {
            sql_get_connect_attr_w(
                h.dbc,
                SQL_ATTR_CONNECTION_DEAD,
                &mut out as *mut u32 as SqlPointer,
                0,
                std::ptr::null_mut(),
            )
        };
        assert_eq!(get, SQL_SUCCESS);
        assert_eq!(out, SQL_CD_TRUE);
    }

    #[test]
    fn connection_dead_reports_false_when_connected_with_client_claimed() {
        // `Connected` with the client momentarily taken means another thread
        // owns it for I/O (`claim_dbc_client`). That is a busy-but-healthy
        // connection, so it must read "not known dead" — reporting DEAD would
        // let a concurrent pool liveness check discard an in-flight connection.
        let h = TestHandles::with_env_dbc();
        h.mark_dbc_connected();
        let mut out: u32 = 0;
        let get = unsafe {
            sql_get_connect_attr_w(
                h.dbc,
                SQL_ATTR_CONNECTION_DEAD,
                &mut out as *mut u32 as SqlPointer,
                0,
                std::ptr::null_mut(),
            )
        };
        assert_eq!(get, SQL_SUCCESS);
        assert_eq!(out, SQL_CD_FALSE);
    }

    #[test]
    fn connection_dead_reports_false_when_connected_and_alive() {
        use crate::handles::DbcHandle;
        use crate::handles::handle_from_raw;
        use mssql_tds::test_client_support::tds_client_from_tokens;

        let h = TestHandles::with_env_dbc();
        h.mark_dbc_connected();
        let dbc = unsafe { handle_from_raw::<DbcHandle>(h.dbc) };
        // A replay-transport client has not been observed dead, so the cached
        // liveness read reports ALIVE.
        dbc.inner.lock().unwrap().client = Some(tds_client_from_tokens(vec![]));

        let mut out: u32 = 12345;
        let get = unsafe {
            sql_get_connect_attr_w(
                h.dbc,
                SQL_ATTR_CONNECTION_DEAD,
                &mut out as *mut u32 as SqlPointer,
                0,
                std::ptr::null_mut(),
            )
        };
        assert_eq!(get, SQL_SUCCESS);
        assert_eq!(out, SQL_CD_FALSE);

        // Clear the client so TestHandles drop does not try to disconnect it.
        dbc.inner.lock().unwrap().client = None;
    }

    #[test]
    fn connection_dead_reports_true_when_client_marked_dead() {
        use crate::handles::DbcHandle;
        use crate::handles::handle_from_raw;
        use mssql_tds::test_client_support::tds_client_from_tokens;

        let h = TestHandles::with_env_dbc();
        h.mark_dbc_connected();
        let dbc = unsafe { handle_from_raw::<DbcHandle>(h.dbc) };
        // A connected client whose session was left unusable (e.g. a reset whose
        // round trip failed poisons it) must read DEAD so the pool discards it,
        // even though the DBC is still marked Connected.
        {
            let mut state = dbc.inner.lock().unwrap();
            let mut client = tds_client_from_tokens(vec![]);
            client.mark_connection_dead();
            state.client = Some(client);
        }

        let mut out: u32 = 12345;
        let get = unsafe {
            sql_get_connect_attr_w(
                h.dbc,
                SQL_ATTR_CONNECTION_DEAD,
                &mut out as *mut u32 as SqlPointer,
                0,
                std::ptr::null_mut(),
            )
        };
        assert_eq!(get, SQL_SUCCESS);
        assert_eq!(out, SQL_CD_TRUE);

        dbc.inner.lock().unwrap().client = None;
    }

    #[test]
    fn connection_dead_null_pointer_is_rejected() {
        let h = TestHandles::with_env_dbc();
        let get = unsafe {
            sql_get_connect_attr_w(
                h.dbc,
                SQL_ATTR_CONNECTION_DEAD,
                std::ptr::null_mut(),
                0,
                std::ptr::null_mut(),
            )
        };
        assert_eq!(get, SQL_ERROR);
    }

    #[test]
    fn vendor_attributes_read_back_the_value_the_set_side_stored() {
        let h = TestHandles::with_env_dbc();
        // 7 exercises the folding the set side applies: any non-zero encrypt
        // value normalizes to on, so the get side must report the stored 1
        // rather than the 7 the caller passed.
        for (attribute, set_value, expected) in [
            (SQL_COPT_SS_ENCRYPT, 0usize, 0u32),
            (SQL_COPT_SS_ENCRYPT, 7, 1),
            (SQL_COPT_SS_TRUST_SERVER_CERTIFICATE, 1, 1),
            (SQL_COPT_SS_INTEGRATED_SECURITY, 1, 1),
        ] {
            let set =
                unsafe { sql_set_connect_attr_w(h.dbc, attribute, set_value as SqlPointer, 0) };
            assert_eq!(set, SQL_SUCCESS, "setting {attribute}");
            assert_eq!(
                get_u32(h.dbc, attribute),
                expected,
                "reading {attribute} back"
            );
        }
    }

    #[test]
    fn vendor_attributes_report_driver_defaults_before_connect() {
        // Nothing has resolved yet, so these come from the driver defaults:
        // encryption on, no trust, no integrated auth.
        let h = TestHandles::with_env_dbc();
        assert_eq!(get_u32(h.dbc, SQL_COPT_SS_ENCRYPT), SQL_EN_ON as u32);
        assert_eq!(get_u32(h.dbc, SQL_COPT_SS_TRUST_SERVER_CERTIFICATE), 0);
        assert_eq!(get_u32(h.dbc, SQL_COPT_SS_INTEGRATED_SECURITY), 0);
    }

    #[test]
    fn vendor_get_uses_effective_values_only_while_connected() {
        let h = TestHandles::with_env_dbc();
        let dbc = unsafe { handle_from_raw::<DbcHandle>(h.dbc) };
        {
            let mut state = dbc.inner.lock().unwrap();
            state.effective_vendor_settings = Some(VendorConnOverrides {
                encrypt: Some(0),
                trust_server_certificate: Some(1),
                integrated_security: Some(1),
            });
            state.connection_state = ConnectionState::Connected;
        }
        assert_eq!(get_u32(h.dbc, SQL_COPT_SS_ENCRYPT), 0);

        dbc.inner.lock().unwrap().connection_state = ConnectionState::Disconnected;
        assert_eq!(
            get_u32(h.dbc, SQL_COPT_SS_ENCRYPT),
            SQL_EN_ON as u32,
            "a disconnected reusable handle reports its explicit override or default"
        );
    }

    #[test]
    fn vendor_attribute_null_pointer_is_rejected() {
        let h = TestHandles::with_env_dbc();
        for attribute in [
            SQL_COPT_SS_ENCRYPT,
            SQL_COPT_SS_TRUST_SERVER_CERTIFICATE,
            SQL_COPT_SS_INTEGRATED_SECURITY,
        ] {
            let get = unsafe {
                sql_get_connect_attr_w(
                    h.dbc,
                    attribute,
                    std::ptr::null_mut(),
                    0,
                    std::ptr::null_mut(),
                )
            };
            assert_eq!(get, SQL_ERROR, "null value pointer for {attribute}");
        }
    }
}
