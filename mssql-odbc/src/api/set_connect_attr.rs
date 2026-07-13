// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Implementation of SQLSetConnectAttrW.
//!
//! Currently handles the msodbcsql-specific `SQL_COPT_SS_ACCESS_TOKEN`
//! attribute, which supplies a pre-acquired Entra access token before
//! connecting. Other attributes are accepted as no-ops for now.

use tracing::{debug, error};

use super::sqlstate::*;
use crate::api::odbc_types::{
    SQL_COPT_SS_ACCESS_TOKEN, SQL_ERROR, SQL_INVALID_HANDLE, SQL_SUCCESS, SqlHandle, SqlInteger,
    SqlPointer, SqlReturn,
};
use crate::error::{free_errors, post_sql_error};
use crate::handles::dbc::ConnectionState;
use crate::handles::{DbcHandle, HandleType, handle_from_raw};

/// Sets a connection attribute.
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
        // Other connection attributes are accepted as no-ops for now.
        _ => SQL_SUCCESS,
    }
}

/// Decodes the msodbcsql `SQL_COPT_SS_ACCESS_TOKEN` structure into the raw JWT.
///
/// Layout: a 4-byte little-endian length `n`, followed by `n` bytes of the
/// access token encoded as UTF-16-LE. Returns `None` if the length is zero,
/// odd, or the bytes are not valid UTF-16. The raw JWT is re-encoded to
/// UTF-16-LE by mssql-tds for the TDS wire format.
///
/// # Safety
/// `value_ptr` must point to a valid ACCESSTOKEN struct whose declared length
/// does not exceed the allocation.
unsafe fn decode_access_token(value_ptr: SqlPointer) -> Option<String> {
    // Entra JWTs are only a few KB; reject an implausibly large declared length
    // so a malformed struct fails closed instead of a huge read/allocation.
    const MAX_ACCESS_TOKEN_BYTES: usize = 64 * 1024;
    let base = value_ptr as *const u8;
    let mut len_bytes = [0u8; 4];
    unsafe { std::ptr::copy_nonoverlapping(base, len_bytes.as_mut_ptr(), 4) };
    let data_size = u32::from_le_bytes(len_bytes) as usize;
    if data_size == 0 || !data_size.is_multiple_of(2) || data_size > MAX_ACCESS_TOKEN_BYTES {
        return None;
    }
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
                buf.len() as SqlInteger,
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
    fn unknown_attribute_is_accepted_as_noop() {
        let h = TestHandles::with_env_dbc();
        // 1234 is an arbitrary unhandled attribute id.
        let ret = unsafe { sql_set_connect_attr_w(h.dbc, 1234, std::ptr::null_mut(), 0) };
        assert_eq!(ret, SQL_SUCCESS);
    }
}
