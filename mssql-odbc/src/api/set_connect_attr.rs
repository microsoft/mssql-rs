// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Implementation of SQLSetConnectAttrW.
//!
//! Handles the msodbcsql-specific `SQL_COPT_SS_ACCESS_TOKEN` attribute (a
//! pre-acquired Entra access token) and `SQL_ATTR_LOGIN_TIMEOUT` (the login
//! deadline applied at connect time). Other standard attributes are accepted as
//! no-ops for now.

use tracing::{debug, error};

use super::sqlstate::*;
use super::txn::{set_autocommit, set_txn_isolation};
use crate::api::odbc_types::{
    SQL_ATTR_ACCESS_MODE, SQL_ATTR_ANSI_APP, SQL_ATTR_AUTOCOMMIT, SQL_ATTR_CONNECTION_TIMEOUT,
    SQL_ATTR_LOGIN_TIMEOUT, SQL_ATTR_PACKET_SIZE, SQL_ATTR_TXN_ISOLATION, SQL_COPT_SS_ACCESS_TOKEN,
    SQL_COPT_SS_TXN_ISOLATION, SQL_ERROR, SQL_INVALID_HANDLE, SQL_SUCCESS, SQL_SUCCESS_WITH_INFO,
    SqlHandle, SqlInteger, SqlPointer, SqlReturn,
};
use crate::error::{free_errors, post_sql_error};
use crate::handles::dbc::ConnectionState;
use crate::handles::{DbcHandle, HandleType, handle_from_raw};

/// Largest login timeout the driver accepts, in seconds.
///
/// Matches msodbcsql's `MAX_QUERY_TIMEOUT` (`0xfffe`, `tds/TdsParser.h:99`),
/// which it applies to `SQL_ATTR_LOGIN_TIMEOUT` in `sqlcmisc.cpp:1735`.
const MAX_LOGIN_TIMEOUT_SECS: u64 = 0xfffe;

/// Sets a connection attribute.
///
/// For `SQL_COPT_SS_ACCESS_TOKEN`, `string_length` is ignored: real ODBC callers
/// pass `SQL_IS_POINTER` and the token length comes from the `ACCESSTOKEN`
/// struct's own `dataSize` field (matching msodbcsql). Unrecognized attributes
/// return `HYC00` rather than silently succeeding.
///
/// # Safety
/// - `connection_handle` must be a valid `DbcHandle` from `SQLAllocHandle`.
/// - For `SQL_COPT_SS_ACCESS_TOKEN`, `value_ptr` must point to an ACCESSTOKEN
///   struct: a 4-byte little-endian length prefix followed by that many bytes
///   of the UTF-16-LE-encoded access token.
pub(crate) unsafe fn sql_set_connect_attr_w(
    connection_handle: SqlHandle,
    attribute: SqlInteger,
    value_ptr: SqlPointer,
    string_length: SqlInteger,
) -> SqlReturn {
    debug!(
        ?connection_handle,
        attribute,
        ?value_ptr,
        "SQLSetConnectAttrW called",
    );

    crate::ffi_entry!("SQLSetConnectAttrW", unsafe {
        sql_set_connect_attr_w_impl(connection_handle, attribute, value_ptr, string_length)
    })
}

unsafe fn sql_set_connect_attr_w_impl(
    connection_handle: SqlHandle,
    attribute: SqlInteger,
    value_ptr: SqlPointer,
    _string_length: SqlInteger,
) -> SqlReturn {
    if connection_handle.is_null() {
        error!("SQLSetConnectAttrW: connection_handle is null");
        return SQL_INVALID_HANDLE;
    }

    let dbc = unsafe { handle_from_raw::<DbcHandle>(connection_handle) };
    debug_assert_eq!(
        dbc.object_type,
        HandleType::Dbc,
        "SQLSetConnectAttrW: handle is not a DBC"
    );

    // The transaction attributes talk to the server, which must not happen while
    // the DBC mutex is held, so they manage their own locking.
    match attribute {
        SQL_ATTR_AUTOCOMMIT => return set_autocommit(dbc, value_ptr as usize as u64),
        // Both spellings drive the same session setting. The vendor attribute is
        // the only one that can carry SQL_TXN_SS_SNAPSHOT, because the Driver
        // Manager screens SQL_ATTR_TXN_ISOLATION down to the four standard bits.
        SQL_ATTR_TXN_ISOLATION | SQL_COPT_SS_TXN_ISOLATION => {
            return set_txn_isolation(dbc, value_ptr as usize as u64);
        }
        _ => {}
    }

    let Ok(mut state) = dbc.inner.lock() else {
        error!("SQLSetConnectAttrW: dbc mutex poisoned");
        return SQL_ERROR;
    };
    free_errors(&mut state);

    match attribute {
        SQL_COPT_SS_ACCESS_TOKEN => {
            // The access token is a pre-connect attribute; reject it once a
            // connection attempt has started. msodbcsql posts HY011 ("attribute
            // cannot be set now") for this case (sqlcmisc.cpp), not HY010.
            if state.connection_state != ConnectionState::Disconnected {
                error!("SQLSetConnectAttrW: SQL_COPT_SS_ACCESS_TOKEN set after connect");
                post_sql_error(
                    &mut state,
                    SQLSTATE_HY011,
                    0,
                    "SQL_COPT_SS_ACCESS_TOKEN must be set before connecting",
                );
                return SQL_ERROR;
            }
            if value_ptr.is_null() {
                error!("SQLSetConnectAttrW: SQL_COPT_SS_ACCESS_TOKEN value is null");
                post_sql_error(
                    &mut state,
                    SQLSTATE_HY009,
                    0,
                    "SQL_COPT_SS_ACCESS_TOKEN value pointer is null",
                );
                return SQL_ERROR;
            }
            match unsafe { decode_access_token(value_ptr) } {
                Some(token) => {
                    state.access_token = Some(token);
                    debug!("SQLSetConnectAttrW: access token stored");
                    SQL_SUCCESS
                }
                None => {
                    error!("SQLSetConnectAttrW: malformed SQL_COPT_SS_ACCESS_TOKEN structure");
                    post_sql_error(
                        &mut state,
                        SQLSTATE_HY024,
                        0,
                        "Malformed SQL_COPT_SS_ACCESS_TOKEN structure",
                    );
                    SQL_ERROR
                }
            }
        }
        SQL_ATTR_LOGIN_TIMEOUT => {
            // Integer attribute: the SQLUINTEGER value is passed by value in the
            // pointer slot (not a pointer to it). Store it so SQLDriverConnect
            // can apply it to the TDS login deadline. `0` means "wait
            // indefinitely" (mapped to no deadline at connect time).
            //
            // Accepted while connected, matching msodbcsql, which stores it
            // unconditionally (`sqlcmisc.cpp:1733-1748`) with none of the
            // `if (lpdbc->hConn)` guards its connect-time-only attributes carry.
            // The handle is reusable, so the value applies to the next connect.
            //
            // Read at pointer width and clamp before narrowing: a direct `as
            // u32` would wrap, turning a value like 2^32 into `0` and silently
            // granting an infinite deadline instead of a long one.
            let requested = value_ptr as usize as u64;
            let secs = requested.min(MAX_LOGIN_TIMEOUT_SECS);
            state.login_timeout = Some(secs as u32);
            debug!(secs, "SQLSetConnectAttrW: login timeout stored");
            if requested > MAX_LOGIN_TIMEOUT_SECS {
                post_diag(&mut state, WARN_LOGIN_TIMEOUT_CHANGED);
                SQL_SUCCESS_WITH_INFO
            } else {
                SQL_SUCCESS
            }
        }
        // Standard attributes the Driver Manager sets before connecting. Stored
        // rather than discarded so `SQLGetConnectAttrW` reports back what was
        // set; none of them changes behaviour on the wire yet.
        // TODO: honor these (connection timeout, packet size, access mode).
        SQL_ATTR_ACCESS_MODE => {
            state.access_mode = value_ptr as usize as u32;
            SQL_SUCCESS
        }
        SQL_ATTR_CONNECTION_TIMEOUT => {
            // Shares msodbcsql's clamp with SQL_ATTR_LOGIN_TIMEOUT
            // (`sqlcmisc.cpp:1733-1741`), but names this attribute in the
            // warning rather than reusing msodbcsql's "Login timeout changed".
            let requested = value_ptr as usize as u64;
            state.connection_timeout = requested.min(MAX_LOGIN_TIMEOUT_SECS) as u32;
            if requested > MAX_LOGIN_TIMEOUT_SECS {
                post_diag(&mut state, WARN_CONNECTION_TIMEOUT_CHANGED);
                SQL_SUCCESS_WITH_INFO
            } else {
                SQL_SUCCESS
            }
        }
        SQL_ATTR_PACKET_SIZE => {
            // Packet size is negotiated in the LOGIN7 handshake, so it can only
            // be chosen before connecting. msodbcsql rejects a late set with
            // HY011 (`sqlcmisc.cpp:1901-1906`).
            if state.connection_state != ConnectionState::Disconnected {
                error!("SQLSetConnectAttrW: SQL_ATTR_PACKET_SIZE set after connect");
                post_sql_error(
                    &mut state,
                    SQLSTATE_HY011,
                    0,
                    "SQL_ATTR_PACKET_SIZE must be set before connecting",
                );
                return SQL_ERROR;
            }
            state.packet_size = value_ptr as usize as u32;
            SQL_SUCCESS
        }
        // Set by the Driver Manager only, and not retrievable, so nothing to
        // store.
        SQL_ATTR_ANSI_APP => SQL_SUCCESS,
        // Any other attribute is genuinely unsupported: surface a clear error
        // (HYC00) instead of silently pretending it took effect.
        _ => {
            error!(
                attribute,
                "SQLSetConnectAttrW: unsupported connection attribute"
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

/// Decodes the msodbcsql `SQL_COPT_SS_ACCESS_TOKEN` structure into the raw JWT.
///
/// Layout: a 4-byte native-endian length `n` (an `unsigned int`), followed by
/// `n` bytes of the access token encoded as UTF-16-LE. Returns `None` if the
/// length is zero, odd, exceeds the size cap, or the bytes are not valid
/// UTF-16. The raw JWT is re-encoded to UTF-16-LE by mssql-tds for the wire.
///
/// # Safety
/// `value_ptr` must point to a valid ACCESSTOKEN struct whose declared length
/// does not exceed the allocation.
unsafe fn decode_access_token(value_ptr: SqlPointer) -> Option<String> {
    // Entra JWTs are only a few KB; reject an implausibly large declared length
    // so a malformed struct fails closed instead of a huge read/allocation.
    const MAX_ACCESS_TOKEN_BYTES: usize = 64 * 1024;
    let base = value_ptr as *const u8;
    // SAFETY: the caller guarantees `value_ptr` points to a readable ACCESSTOKEN
    // whose first 4 bytes are the `dataSize` field. Copying avoids assuming the
    // pointer is aligned for a `*const u32` read.
    let mut len_bytes = [0u8; 4];
    unsafe { std::ptr::copy_nonoverlapping(base, len_bytes.as_mut_ptr(), 4) };
    // `dataSize` is a native `unsigned int` written by the caller in host byte
    // order; the UTF-16 payload below is explicitly little-endian.
    let data_size = u32::from_ne_bytes(len_bytes) as usize;
    if data_size == 0 || !data_size.is_multiple_of(2) || data_size > MAX_ACCESS_TOKEN_BYTES {
        return None;
    }
    // SAFETY: `data_size` is bounded to <= MAX_ACCESS_TOKEN_BYTES and the caller
    // guarantees the payload is `dataSize` bytes after the 4-byte length prefix.
    let data = unsafe { std::slice::from_raw_parts(base.add(4), data_size) };
    let units: Vec<u16> = data
        .chunks_exact(2)
        .map(|c| u16::from_le_bytes([c[0], c[1]]))
        .collect();
    String::from_utf16(&units).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::odbc_types::{
        DEFAULT_PACKET_SIZE, SQL_AUTOCOMMIT_OFF, SQL_AUTOCOMMIT_ON, SQL_IS_POINTER,
        SQL_TXN_READ_COMMITTED, SQL_TXN_READ_UNCOMMITTED, SQL_TXN_REPEATABLE_READ,
        SQL_TXN_SERIALIZABLE, SQL_TXN_SS_SNAPSHOT,
    };
    use crate::error::HasDiagnostics;
    use crate::test_support::TestHandles;

    /// Build a `SQL_COPT_SS_ACCESS_TOKEN` struct the way msodbcsql apps do:
    /// a 4-byte little-endian length followed by UTF-16-LE token bytes.
    fn make_token_struct(jwt: &str) -> Vec<u8> {
        let token_bytes: Vec<u8> = jwt.encode_utf16().flat_map(|u| u.to_le_bytes()).collect();
        let mut buf = (token_bytes.len() as u32).to_le_bytes().to_vec();
        buf.extend_from_slice(&token_bytes);
        buf
    }

    #[test]
    fn decode_round_trips_jwt() {
        let jwt = "eyJhbGciOiJ.header.sig";
        let buf = make_token_struct(jwt);
        let decoded = unsafe { decode_access_token(buf.as_ptr() as SqlPointer) };
        assert_eq!(decoded.as_deref(), Some(jwt));
    }

    #[test]
    fn decode_rejects_odd_length() {
        // Declared length 3 is odd -> not valid UTF-16-LE.
        let buf: Vec<u8> = vec![3, 0, 0, 0, b'a', 0, b'b'];
        let decoded = unsafe { decode_access_token(buf.as_ptr() as SqlPointer) };
        assert_eq!(decoded, None);
    }

    #[test]
    fn decode_rejects_oversized_length() {
        // A declared length far above the cap is rejected before any read.
        let buf: Vec<u8> = 200_000u32.to_le_bytes().to_vec();
        let decoded = unsafe { decode_access_token(buf.as_ptr() as SqlPointer) };
        assert_eq!(decoded, None);
    }

    #[test]
    fn set_before_connect_stores_token() {
        let h = TestHandles::with_env_dbc();
        let jwt = "abc.def.ghi";
        let buf = make_token_struct(jwt);
        let ret = unsafe {
            sql_set_connect_attr_w(
                h.dbc,
                SQL_COPT_SS_ACCESS_TOKEN,
                buf.as_ptr() as SqlPointer,
                SQL_IS_POINTER,
            )
        };
        assert_eq!(ret, SQL_SUCCESS);
        let dbc = unsafe { handle_from_raw::<DbcHandle>(h.dbc) };
        let state = dbc.inner.lock().unwrap();
        assert_eq!(state.access_token.as_deref(), Some(jwt));
    }

    #[test]
    fn null_token_pointer_is_rejected() {
        let h = TestHandles::with_env_dbc();
        let ret = unsafe {
            sql_set_connect_attr_w(h.dbc, SQL_COPT_SS_ACCESS_TOKEN, std::ptr::null_mut(), 0)
        };
        assert_eq!(ret, SQL_ERROR);
    }

    #[test]
    fn unsupported_attribute_returns_error() {
        let h = TestHandles::with_env_dbc();
        // 1234 is an arbitrary unhandled attribute id -> HYC00, not silent success.
        let ret = unsafe { sql_set_connect_attr_w(h.dbc, 1234, std::ptr::null_mut(), 0) };
        assert_eq!(ret, SQL_ERROR);
    }

    #[test]
    fn connection_timeout_null_value_is_accepted_as_zero() {
        let h = TestHandles::with_env_dbc();
        // A standard connection attribute the DM sets pre-connect is accepted;
        // a null pointer slot carries the integer value 0.
        let ret = unsafe {
            sql_set_connect_attr_w(h.dbc, SQL_ATTR_CONNECTION_TIMEOUT, std::ptr::null_mut(), 0)
        };
        assert_eq!(ret, SQL_SUCCESS);
        let dbc = unsafe { handle_from_raw::<DbcHandle>(h.dbc) };
        assert_eq!(dbc.inner.lock().unwrap().connection_timeout, 0);
    }

    #[test]
    fn login_timeout_is_stored() {
        let h = TestHandles::with_env_dbc();
        // Integer attributes carry the value by value in the pointer slot.
        let ret = unsafe {
            sql_set_connect_attr_w(h.dbc, SQL_ATTR_LOGIN_TIMEOUT, 45usize as SqlPointer, 0)
        };
        assert_eq!(ret, SQL_SUCCESS);
        let dbc = unsafe { handle_from_raw::<DbcHandle>(h.dbc) };
        let state = dbc.inner.lock().unwrap();
        assert_eq!(state.login_timeout, Some(45));
    }

    #[test]
    fn login_timeout_zero_is_stored_as_infinite() {
        let h = TestHandles::with_env_dbc();
        // 0 is a valid value meaning "wait indefinitely"; it must be stored as
        // Some(0), not treated as unset.
        let ret = unsafe {
            sql_set_connect_attr_w(h.dbc, SQL_ATTR_LOGIN_TIMEOUT, std::ptr::null_mut(), 0)
        };
        assert_eq!(ret, SQL_SUCCESS);
        let dbc = unsafe { handle_from_raw::<DbcHandle>(h.dbc) };
        let state = dbc.inner.lock().unwrap();
        assert_eq!(state.login_timeout, Some(0));
    }

    #[test]
    fn login_timeout_at_maximum_is_not_clamped() {
        let h = TestHandles::with_env_dbc();
        let ret = unsafe {
            sql_set_connect_attr_w(
                h.dbc,
                SQL_ATTR_LOGIN_TIMEOUT,
                MAX_LOGIN_TIMEOUT_SECS as usize as SqlPointer,
                0,
            )
        };
        assert_eq!(ret, SQL_SUCCESS);
        let dbc = unsafe { handle_from_raw::<DbcHandle>(h.dbc) };
        let state = dbc.inner.lock().unwrap();
        assert_eq!(state.login_timeout, Some(MAX_LOGIN_TIMEOUT_SECS as u32));
    }

    #[test]
    fn login_timeout_above_maximum_is_clamped_with_warning() {
        let h = TestHandles::with_env_dbc();
        // msodbcsql clamps to MAX_QUERY_TIMEOUT and reports 01S02 rather than
        // failing or honoring the oversized value (`sqlcmisc.cpp:1735-1741`).
        let ret = unsafe {
            sql_set_connect_attr_w(
                h.dbc,
                SQL_ATTR_LOGIN_TIMEOUT,
                (MAX_LOGIN_TIMEOUT_SECS + 1) as usize as SqlPointer,
                0,
            )
        };
        assert_eq!(ret, SQL_SUCCESS_WITH_INFO);
        let dbc = unsafe { handle_from_raw::<DbcHandle>(h.dbc) };
        let state = dbc.inner.lock().unwrap();
        assert_eq!(state.login_timeout, Some(MAX_LOGIN_TIMEOUT_SECS as u32));
        assert_eq!(state.diag_records()[0].sql_state, SQLSTATE_01S02);
    }

    #[test]
    fn connection_timeout_above_maximum_warns_about_the_connection_timeout() {
        // Same clamp and same SQLSTATE as the login timeout, but the message
        // names the attribute the application actually set. msodbcsql reuses
        // "Login timeout changed" here (`sqlcmisc.cpp:1739`); this is a
        // deliberate divergence, so pin it.
        let h = TestHandles::with_env_dbc();
        let ret = unsafe {
            sql_set_connect_attr_w(
                h.dbc,
                SQL_ATTR_CONNECTION_TIMEOUT,
                (MAX_LOGIN_TIMEOUT_SECS + 1) as usize as SqlPointer,
                0,
            )
        };
        assert_eq!(ret, SQL_SUCCESS_WITH_INFO);
        let dbc = unsafe { handle_from_raw::<DbcHandle>(h.dbc) };
        let state = dbc.inner.lock().unwrap();
        assert_eq!(state.connection_timeout, MAX_LOGIN_TIMEOUT_SECS as u32);

        let record = &state.diag_records()[0];
        assert_eq!(record.sql_state, SQLSTATE_01S02);
        assert!(
            record
                .message
                .ends_with(WARN_CONNECTION_TIMEOUT_CHANGED.text),
            "got: {}",
            record.message
        );
        assert!(
            !record.message.contains(WARN_LOGIN_TIMEOUT_CHANGED.text),
            "must not reuse msodbcsql's login-timeout wording: {}",
            record.message
        );
    }

    #[cfg(target_pointer_width = "64")]
    #[test]
    fn login_timeout_beyond_u32_does_not_wrap_to_infinite() {
        let h = TestHandles::with_env_dbc();
        // A raw `as u32` would turn 2^32 into 0, which this driver reads as
        // "wait indefinitely" - the opposite of what the caller asked for.
        let ret = unsafe {
            sql_set_connect_attr_w(
                h.dbc,
                SQL_ATTR_LOGIN_TIMEOUT,
                0x1_0000_0000usize as SqlPointer,
                0,
            )
        };
        assert_eq!(ret, SQL_SUCCESS_WITH_INFO);
        let dbc = unsafe { handle_from_raw::<DbcHandle>(h.dbc) };
        let state = dbc.inner.lock().unwrap();
        assert_eq!(state.login_timeout, Some(MAX_LOGIN_TIMEOUT_SECS as u32));
    }

    #[test]
    fn login_timeout_after_connect_is_accepted() {
        // msodbcsql stores it unconditionally (`sqlcmisc.cpp:1733-1748`) with
        // none of the `if (lpdbc->hConn)` guards its connect-time-only
        // attributes carry. The value is not dead: SQLDisconnect leaves it in
        // place, so it applies to the next connect on this reusable handle.
        let h = TestHandles::with_env_dbc();
        let dbc = unsafe { handle_from_raw::<DbcHandle>(h.dbc) };
        dbc.inner.lock().unwrap().connection_state = ConnectionState::Connected;

        let ret = unsafe {
            sql_set_connect_attr_w(h.dbc, SQL_ATTR_LOGIN_TIMEOUT, 45usize as SqlPointer, 0)
        };
        assert_eq!(ret, SQL_SUCCESS);
        assert_eq!(dbc.inner.lock().unwrap().login_timeout, Some(45));
    }

    #[test]
    fn packet_size_after_connect_is_rejected() {
        // Packet size is fixed by the LOGIN7 handshake, so a late set could
        // never apply. msodbcsql posts HY011 for it (`sqlcmisc.cpp:1901-1906`).
        let h = TestHandles::with_env_dbc();
        let dbc = unsafe { handle_from_raw::<DbcHandle>(h.dbc) };
        dbc.inner.lock().unwrap().connection_state = ConnectionState::Connected;

        let ret = unsafe {
            sql_set_connect_attr_w(h.dbc, SQL_ATTR_PACKET_SIZE, 16384usize as SqlPointer, 0)
        };
        assert_eq!(ret, SQL_ERROR);
        let state = dbc.inner.lock().unwrap();
        assert_eq!(state.diag_records()[0].sql_state, SQLSTATE_HY011);
        assert_eq!(
            state.packet_size, DEFAULT_PACKET_SIZE,
            "a rejected set must not change the stored value"
        );
    }

    #[test]
    fn accepted_standard_attributes_are_stored() {
        let h = TestHandles::with_env_dbc();
        for (attribute, value) in [
            (SQL_ATTR_ACCESS_MODE, 1usize),
            (SQL_ATTR_CONNECTION_TIMEOUT, 30),
            (SQL_ATTR_PACKET_SIZE, 16384),
        ] {
            let ret = unsafe { sql_set_connect_attr_w(h.dbc, attribute, value as SqlPointer, 0) };
            assert_eq!(ret, SQL_SUCCESS, "setting {attribute}");
        }
        let dbc = unsafe { handle_from_raw::<DbcHandle>(h.dbc) };
        let state = dbc.inner.lock().unwrap();
        assert_eq!(state.access_mode, 1);
        assert_eq!(state.connection_timeout, 30);
        assert_eq!(state.packet_size, 16384);
    }

    #[test]
    fn autocommit_off_is_stored_before_connect() {
        let h = TestHandles::with_env_dbc();
        let ret = unsafe {
            sql_set_connect_attr_w(
                h.dbc,
                SQL_ATTR_AUTOCOMMIT,
                SQL_AUTOCOMMIT_OFF as usize as SqlPointer,
                0,
            )
        };
        assert_eq!(ret, SQL_SUCCESS);
        let dbc = unsafe { handle_from_raw::<DbcHandle>(h.dbc) };
        assert!(!dbc.inner.lock().unwrap().autocommit);
    }

    #[test]
    fn autocommit_default_is_on_and_resetting_it_is_a_no_op() {
        // ODBC's default is SQL_AUTOCOMMIT_ON; msodbcsql short-circuits a set to
        // the current mode (`sqlcmisc.cpp:1720`) instead of touching the server.
        let h = TestHandles::with_env_dbc();
        let dbc = unsafe { handle_from_raw::<DbcHandle>(h.dbc) };
        assert!(dbc.inner.lock().unwrap().autocommit);
        let ret = unsafe {
            sql_set_connect_attr_w(
                h.dbc,
                SQL_ATTR_AUTOCOMMIT,
                SQL_AUTOCOMMIT_ON as usize as SqlPointer,
                0,
            )
        };
        assert_eq!(ret, SQL_SUCCESS);
        assert!(dbc.inner.lock().unwrap().autocommit);
    }

    #[test]
    fn autocommit_rejects_values_outside_the_two_modes() {
        let h = TestHandles::with_env_dbc();
        let ret = unsafe { sql_set_connect_attr_w(h.dbc, SQL_ATTR_AUTOCOMMIT, 7 as SqlPointer, 0) };
        assert_eq!(ret, SQL_ERROR);
        let dbc = unsafe { handle_from_raw::<DbcHandle>(h.dbc) };
        let state = dbc.inner.lock().unwrap();
        assert_eq!(state.diag_records()[0].sql_state, SQLSTATE_HY024);
        assert!(state.autocommit, "a rejected set must not change the mode");
    }

    #[test]
    fn isolation_levels_are_stored_before_connect() {
        let h = TestHandles::with_env_dbc();
        let dbc = unsafe { handle_from_raw::<DbcHandle>(h.dbc) };
        for level in [
            SQL_TXN_READ_UNCOMMITTED,
            SQL_TXN_READ_COMMITTED,
            SQL_TXN_REPEATABLE_READ,
            SQL_TXN_SERIALIZABLE,
            SQL_TXN_SS_SNAPSHOT,
        ] {
            let ret = unsafe {
                sql_set_connect_attr_w(
                    h.dbc,
                    SQL_ATTR_TXN_ISOLATION,
                    level as usize as SqlPointer,
                    0,
                )
            };
            assert_eq!(ret, SQL_SUCCESS, "level {level:#x}");
            assert_eq!(dbc.inner.lock().unwrap().txn_isolation, level);
        }
    }

    #[test]
    fn vendor_isolation_attribute_is_accepted_and_reads_back() {
        // SQL_COPT_SS_TXN_ISOLATION is the only route to SNAPSHOT: the Driver
        // Manager screens SQL_ATTR_TXN_ISOLATION down to the four standard bits
        // before the driver is called.
        let h = TestHandles::with_env_dbc();
        let dbc = unsafe { handle_from_raw::<DbcHandle>(h.dbc) };
        let ret = unsafe {
            sql_set_connect_attr_w(
                h.dbc,
                SQL_COPT_SS_TXN_ISOLATION,
                SQL_TXN_SS_SNAPSHOT as usize as SqlPointer,
                0,
            )
        };
        assert_eq!(ret, SQL_SUCCESS);
        assert_eq!(dbc.inner.lock().unwrap().txn_isolation, SQL_TXN_SS_SNAPSHOT);
    }

    #[test]
    fn setting_the_current_isolation_level_again_is_a_no_op() {
        // Matches the same-value short-circuit autocommit uses
        // (`sqlcmisc.cpp:1720`): no cursor sweep and no round trip.
        let h = TestHandles::with_env_dbc();
        let dbc = unsafe { handle_from_raw::<DbcHandle>(h.dbc) };
        h.mark_dbc_connected();
        assert_eq!(
            dbc.inner.lock().unwrap().txn_isolation,
            SQL_TXN_READ_COMMITTED
        );
        // Connected with no TDS client: reaching the server would fail, so
        // SQL_SUCCESS proves the short-circuit fired.
        let ret = unsafe {
            sql_set_connect_attr_w(
                h.dbc,
                SQL_ATTR_TXN_ISOLATION,
                SQL_TXN_READ_COMMITTED as usize as SqlPointer,
                0,
            )
        };
        assert_eq!(ret, SQL_SUCCESS);
    }

    #[test]
    fn isolation_rejects_unsupported_level_with_hyc00() {
        // msodbcsql answers HYC00 rather than HY024 here (`sqlcmisc.cpp:1817`):
        // the value is a valid ODBC isolation bit the driver does not implement.
        let h = TestHandles::with_env_dbc();
        let ret =
            unsafe { sql_set_connect_attr_w(h.dbc, SQL_ATTR_TXN_ISOLATION, 0x10 as SqlPointer, 0) };
        assert_eq!(ret, SQL_ERROR);
        let dbc = unsafe { handle_from_raw::<DbcHandle>(h.dbc) };
        let state = dbc.inner.lock().unwrap();
        assert_eq!(state.diag_records()[0].sql_state, SQLSTATE_HYC00);
        assert_eq!(
            state.txn_isolation, SQL_TXN_READ_COMMITTED,
            "a rejected set must not change the stored level"
        );
    }

    #[test]
    fn isolation_is_rejected_while_a_transaction_is_open() {
        let h = TestHandles::with_env_dbc();
        let dbc = unsafe { handle_from_raw::<DbcHandle>(h.dbc) };
        dbc.inner.lock().unwrap().local_tran_started = true;

        let ret = unsafe {
            sql_set_connect_attr_w(
                h.dbc,
                SQL_ATTR_TXN_ISOLATION,
                SQL_TXN_SERIALIZABLE as usize as SqlPointer,
                0,
            )
        };
        assert_eq!(ret, SQL_ERROR);
        let mut state = dbc.inner.lock().unwrap();
        assert_eq!(state.diag_records()[0].sql_state, SQLSTATE_HY011);
        assert_eq!(state.txn_isolation, SQL_TXN_READ_COMMITTED);
        state.local_tran_started = false;
    }
}
