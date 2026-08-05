// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Implementation of SQLSetConnectAttrW.
//!
//! Currently handles the msodbcsql-specific `SQL_COPT_SS_ACCESS_TOKEN`
//! attribute, which supplies a pre-acquired Entra access token before
//! connecting. Other attributes are accepted as no-ops for now.

use tracing::{debug, error};

use super::conn_exec::exec_on_connection;
use super::sqlstate::*;
use super::util::read_utf16;
use crate::api::odbc_types::{
    SQL_ATTR_ACCESS_MODE, SQL_ATTR_ANSI_APP, SQL_ATTR_AUTOCOMMIT, SQL_ATTR_CONNECTION_TIMEOUT,
    SQL_ATTR_CURRENT_CATALOG, SQL_ATTR_LOGIN_TIMEOUT, SQL_ATTR_PACKET_SIZE,
    SQL_ATTR_RESET_CONNECTION, SQL_ATTR_TXN_ISOLATION, SQL_AUTOCOMMIT_OFF, SQL_AUTOCOMMIT_ON,
    SQL_COPT_SS_ACCESS_TOKEN, SQL_ERROR, SQL_INVALID_HANDLE, SQL_NTS, SQL_SUCCESS,
    SQL_TXN_READ_COMMITTED, SQL_TXN_READ_UNCOMMITTED, SQL_TXN_REPEATABLE_READ,
    SQL_TXN_SERIALIZABLE, SQL_TXN_SS_SNAPSHOT, SqlHandle, SqlInteger, SqlPointer, SqlReturn,
    SqlSmallInt, SqlWChar,
};
use crate::error::{free_errors, post_sql_error};
use crate::handles::dbc::ConnectionState;
use crate::handles::{DbcHandle, HandleType, handle_from_raw};

/// Maps an ODBC isolation level to its `SET TRANSACTION ISOLATION LEVEL` clause.
pub(crate) fn isolation_level_sql(level: u32) -> Option<&'static str> {
    match level {
        SQL_TXN_READ_UNCOMMITTED => Some("READ UNCOMMITTED"),
        SQL_TXN_READ_COMMITTED => Some("READ COMMITTED"),
        SQL_TXN_REPEATABLE_READ => Some("REPEATABLE READ"),
        SQL_TXN_SERIALIZABLE => Some("SERIALIZABLE"),
        SQL_TXN_SS_SNAPSHOT => Some("SNAPSHOT"),
        _ => None,
    }
}

/// Switches the session between autocommit and manual-commit mode.
///
/// Manual-commit mode is expressed as `SET IMPLICIT_TRANSACTIONS ON`, matching
/// msodbcsql: the server opens a transaction on the next statement and
/// `SQLEndTran` closes it. Returning to autocommit commits any transaction that
/// is still open, because ODBC requires the switch itself to be a commit point.
pub(crate) fn apply_autocommit(dbc: &DbcHandle, autocommit: bool) -> SqlReturn {
    let sql = if autocommit {
        "IF @@TRANCOUNT > 0 COMMIT TRANSACTION; SET IMPLICIT_TRANSACTIONS OFF"
    } else {
        "SET IMPLICIT_TRANSACTIONS ON"
    };
    match exec_on_connection(dbc, sql, "SQLSetConnectAttrW") {
        Ok(()) => SQL_SUCCESS,
        Err(rc) => rc,
    }
}

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
    string_length: SqlInteger,
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
        // Standard attributes the Driver Manager sets before connecting that we
        // accept (and currently ignore) so the connect handshake is not broken.
        // TODO: honor these (timeouts, packet size, access mode) once wired.
        SQL_ATTR_ACCESS_MODE
        | SQL_ATTR_LOGIN_TIMEOUT
        | SQL_ATTR_CONNECTION_TIMEOUT
        | SQL_ATTR_PACKET_SIZE
        | SQL_ATTR_ANSI_APP => SQL_SUCCESS,
        SQL_ATTR_AUTOCOMMIT => {
            let requested = value_ptr as usize as u32;
            let autocommit = match requested {
                SQL_AUTOCOMMIT_ON => true,
                SQL_AUTOCOMMIT_OFF => false,
                other => {
                    error!(
                        other,
                        "SQLSetConnectAttrW: invalid SQL_ATTR_AUTOCOMMIT value"
                    );
                    post_diag(&mut state, ERR_INVALID_ATTRIBUTE_VALUE);
                    return SQL_ERROR;
                }
            };
            if autocommit == state.autocommit {
                return SQL_SUCCESS;
            }
            state.autocommit = autocommit;
            if state.connection_state != ConnectionState::Connected {
                // Applied by SQLDriverConnect once the session exists.
                return SQL_SUCCESS;
            }
            drop(state);
            apply_autocommit(dbc, autocommit)
        }
        SQL_ATTR_TXN_ISOLATION => {
            let requested = value_ptr as usize as u32;
            let Some(level) = isolation_level_sql(requested) else {
                error!(
                    requested,
                    "SQLSetConnectAttrW: invalid SQL_ATTR_TXN_ISOLATION value"
                );
                post_diag(&mut state, ERR_INVALID_ATTRIBUTE_VALUE);
                return SQL_ERROR;
            };
            state.txn_isolation = requested;
            if state.connection_state != ConnectionState::Connected {
                return SQL_SUCCESS;
            }
            drop(state);
            match exec_on_connection(
                dbc,
                &format!("SET TRANSACTION ISOLATION LEVEL {level}"),
                "SQLSetConnectAttrW",
            ) {
                Ok(()) => SQL_SUCCESS,
                Err(rc) => rc,
            }
        }
        SQL_ATTR_CURRENT_CATALOG => {
            if value_ptr.is_null() {
                error!("SQLSetConnectAttrW: SQL_ATTR_CURRENT_CATALOG value is null");
                post_sql_error(
                    &mut state,
                    SQLSTATE_HY009,
                    0,
                    "SQL_ATTR_CURRENT_CATALOG value pointer is null",
                );
                return SQL_ERROR;
            }
            // `string_length` is in bytes (SQL_NTS when the caller passes a
            // NUL-terminated string); `read_utf16` counts SQLWCHARs.
            let chars = if string_length == SQL_NTS as SqlInteger {
                SQL_NTS
            } else {
                match SqlSmallInt::try_from(string_length / 2) {
                    Ok(chars) => chars,
                    Err(_) => {
                        post_diag(&mut state, ERR_INVALID_ATTRIBUTE_VALUE);
                        return SQL_ERROR;
                    }
                }
            };
            let catalog = unsafe { read_utf16(value_ptr.cast::<SqlWChar>(), chars) };
            if catalog.is_empty() {
                post_diag(&mut state, ERR_INVALID_ATTRIBUTE_VALUE);
                return SQL_ERROR;
            }
            if state.connection_state != ConnectionState::Connected {
                // Pre-connect: the catalog is carried by the connection string's
                // Database keyword, which SQLDriverConnect already honors.
                state.current_catalog = Some(catalog);
                return SQL_SUCCESS;
            }
            drop(state);
            let quoted = catalog.replace(']', "]]");
            match exec_on_connection(dbc, &format!("USE [{quoted}]"), "SQLSetConnectAttrW") {
                Ok(()) => {
                    if let Ok(mut state) = dbc.inner.lock() {
                        state.current_catalog = Some(catalog);
                    }
                    SQL_SUCCESS
                }
                Err(rc) => rc,
            }
        }
        SQL_ATTR_RESET_CONNECTION => {
            // Pooling reset: the DM sets this just before returning a connection
            // to the pool. There is no TDS reset primitive exposed here yet, so
            // roll back any in-flight work and report success.
            if state.connection_state != ConnectionState::Connected {
                return SQL_SUCCESS;
            }
            let autocommit = state.autocommit;
            drop(state);
            if autocommit {
                SQL_SUCCESS
            } else {
                match exec_on_connection(
                    dbc,
                    "IF @@TRANCOUNT > 0 ROLLBACK TRANSACTION",
                    "SQLSetConnectAttrW",
                ) {
                    Ok(()) => SQL_SUCCESS,
                    Err(rc) => rc,
                }
            }
        }
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
            sql_set_connect_attr_w(h.dbc, SQL_ATTR_LOGIN_TIMEOUT, std::ptr::null_mut(), 0)
        };
        assert_eq!(ret, SQL_SUCCESS);
    }
}
