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
use crate::api::odbc_types::{
    SQL_ATTR_ACCESS_MODE, SQL_ATTR_CONNECTION_TIMEOUT, SQL_ATTR_LOGIN_TIMEOUT,
    SQL_ATTR_PACKET_SIZE, SQL_COPT_SS_ACCESS_TOKEN, SQL_ERROR, SQL_INVALID_HANDLE, SQL_SUCCESS,
    SqlHandle, SqlInteger, SqlPointer, SqlReturn,
};
use crate::error::{free_errors, post_sql_error};
use crate::handles::dbc::ConnectionState;
use crate::handles::{DbcHandle, HandleType, handle_from_raw};

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
            let secs = value_ptr as usize as u32;
            state.login_timeout = Some(secs);
            debug!(secs, "SQLSetConnectAttrW: login timeout stored");
            SQL_SUCCESS
        }
        // Standard attributes the Driver Manager sets before connecting that we
        // accept (and currently ignore) so the connect handshake is not broken.
        // TODO: honor these (connection timeout, packet size, access mode) once wired.
        SQL_ATTR_ACCESS_MODE | SQL_ATTR_CONNECTION_TIMEOUT | SQL_ATTR_PACKET_SIZE => SQL_SUCCESS,
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
    use crate::api::odbc_types::SQL_IS_POINTER;
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
    fn accepted_standard_attribute_is_noop() {
        let h = TestHandles::with_env_dbc();
        // A standard connection attribute the DM sets pre-connect is accepted.
        let ret = unsafe {
            sql_set_connect_attr_w(h.dbc, SQL_ATTR_CONNECTION_TIMEOUT, std::ptr::null_mut(), 0)
        };
        assert_eq!(ret, SQL_SUCCESS);
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
}
