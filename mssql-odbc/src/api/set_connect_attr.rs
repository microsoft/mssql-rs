// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Implementation of SQLSetConnectAttrW.
//!
//! Handles the msodbcsql-specific `SQL_COPT_SS_ACCESS_TOKEN` attribute (a
//! pre-acquired Entra access token), `SQL_ATTR_LOGIN_TIMEOUT` (the login
//! deadline applied at connect time), `SQL_ATTR_CURRENT_CATALOG` (the current
//! database, which sends `USE` when connected), and `SQL_ATTR_QUERY_TIMEOUT`
//! (the statement option in its connection-wide form). Other standard
//! attributes are accepted as no-ops for now.

use tracing::{debug, error};

use super::current_catalog::set_current_catalog;
use super::set_stmt_attr::clamp_query_timeout;
use super::sqlstate::*;
use super::txn::{reset_connection, set_autocommit, set_txn_isolation};
use crate::api::attributes::{AttrOp, AttrScope, unimplemented_attr_diag};
use crate::api::odbc_types::{
    SQL_ATTR_ACCESS_MODE, SQL_ATTR_ANSI_APP, SQL_ATTR_AUTOCOMMIT, SQL_ATTR_CONNECTION_TIMEOUT,
    SQL_ATTR_CURRENT_CATALOG, SQL_ATTR_LOGIN_TIMEOUT, SQL_ATTR_PACKET_SIZE, SQL_ATTR_QUERY_TIMEOUT,
    SQL_ATTR_RESET_CONNECTION, SQL_ATTR_TXN_ISOLATION, SQL_COPT_SS_ACCESS_TOKEN,
    SQL_COPT_SS_ENCRYPT, SQL_COPT_SS_INTEGRATED_SECURITY, SQL_COPT_SS_RESET_CONNECTION,
    SQL_COPT_SS_TRUST_SERVER_CERTIFICATE, SQL_COPT_SS_TXN_ISOLATION, SQL_EN_OFF, SQL_EN_ON,
    SQL_EN_STRICT, SQL_ERROR, SQL_INVALID_HANDLE, SQL_SUCCESS, SQL_SUCCESS_WITH_INFO, SqlHandle,
    SqlInteger, SqlPointer, SqlReturn,
};
use crate::error::{free_errors, post_sql_error};
use crate::handles::dbc::ConnectionState;
use crate::handles::{DbcHandle, HandleType, StmtHandle, handle_from_raw};

/// Largest login timeout the driver accepts, in seconds.
///
/// Matches msodbcsql's `MAX_QUERY_TIMEOUT` (`0xfffe`, `tds/TdsParser.h:99`),
/// which it applies to `SQL_ATTR_LOGIN_TIMEOUT` in `sqlcmisc.cpp:1735`.
const MAX_LOGIN_TIMEOUT_SECS: u64 = 0xfffe;

/// Sets a connection attribute.
///
/// For `SQL_COPT_SS_ACCESS_TOKEN`, `string_length` is ignored: real ODBC callers
/// pass `SQL_IS_POINTER` and the token length comes from the `ACCESSTOKEN`
/// struct's own `dataSize` field (matching msodbcsql). Unrecognized attribute
/// identifiers return `HY092` rather than silently succeeding.
///
/// # Safety
/// - `connection_handle` must be a valid `DbcHandle` from `SQLAllocHandle`.
/// - For `SQL_COPT_SS_ACCESS_TOKEN`, `value_ptr` must point to an ACCESSTOKEN
///   struct: a 4-byte little-endian length prefix followed by that many bytes
///   of the UTF-16-LE-encoded access token.
/// - For `SQL_ATTR_CURRENT_CATALOG`, `value_ptr` must point to `string_length`
///   `SQLWCHAR`s, or to a NUL-terminated wide string when `string_length` is
///   `SQL_NTS`.
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
        // Pool-reuse reset: rolls back any live local transaction and arms the
        // RESETCONNECTION bit, so it must not run under the DBC mutex either.
        SQL_ATTR_RESET_CONNECTION | SQL_COPT_SS_RESET_CONNECTION => {
            return reset_connection(dbc, value_ptr as usize as u64);
        }
        // Sends `USE` when connected — likewise not under the mutex.
        SQL_ATTR_CURRENT_CATALOG => {
            return unsafe { set_current_catalog(dbc, value_ptr, string_length) };
        }
        // Fans out to every statement on the connection, so it takes statement
        // locks and must not hold the DBC mutex while doing so.
        SQL_ATTR_QUERY_TIMEOUT => return set_query_timeout(dbc, value_ptr as usize as u64),
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
        // The attribute forms of Encrypt / TrustServerCertificate /
        // Trusted_Connection. All three are pre-connect only (measured: HY011
        // afterwards, like the access token) and all three override the keyword
        // rather than yielding to it -- see `VendorConnOverrides`.
        SQL_COPT_SS_ENCRYPT
        | SQL_COPT_SS_TRUST_SERVER_CERTIFICATE
        | SQL_COPT_SS_INTEGRATED_SECURITY => {
            if state.connection_state != ConnectionState::Disconnected {
                error!(
                    attribute,
                    "SQLSetConnectAttrW: vendor attribute set after connect"
                );
                post_sql_error(
                    &mut state,
                    SQLSTATE_HY011,
                    0,
                    "Attribute must be set before connecting",
                );
                return SQL_ERROR;
            }
            let value = value_ptr as usize as u64;
            let overrides = &mut state.vendor_overrides;
            match attribute {
                SQL_COPT_SS_ENCRYPT => overrides.encrypt = Some(normalize_encrypt(value)),
                SQL_COPT_SS_TRUST_SERVER_CERTIFICATE => {
                    overrides.trust_server_certificate = Some(u32::from(value != 0));
                }
                _ => overrides.integrated_security = Some(u32::from(value != 0)),
            }
            debug!(
                attribute,
                value, "SQLSetConnectAttrW: vendor override stored"
            );
            SQL_SUCCESS
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
        // Any other identifier is one this driver does not act on. Which
        // diagnostic that earns depends on whether msodbcsql recognizes it:
        // `HYC00` when it does, so a caller can tell "unavailable here" from
        // "not an attribute", and `HY092` when it does not. See `attributes.rs`.
        _ => {
            post_diag(
                &mut state,
                unimplemented_attr_diag(AttrScope::Dbc, AttrOp::Set, attribute),
            );
            SQL_ERROR
        }
    }
}

/// Normalizes a `SQL_COPT_SS_ENCRYPT` value.
///
/// Measured against msodbcsql 18 by reading `encrypt_option` back from
/// `sys.dm_exec_connections`: `0` connects unencrypted, `1` encrypted, and `2`
/// selects TDS 8.0 strict mode -- which is distinguishable because strict stops
/// honoring `TrustServerCertificate`, so it fails a self-signed handshake that
/// `1` accepts.
///
/// Out-of-range values are **not** rejected. `3`, `7` and `-1` all connected
/// encrypted, and setting `7` then reading the attribute back returns `1`, so
/// they normalize to "on" rather than earning an `HY024`.
fn normalize_encrypt(value: u64) -> u32 {
    match value {
        SQL_EN_OFF => SQL_EN_OFF as u32,
        SQL_EN_STRICT => SQL_EN_STRICT as u32,
        _ => SQL_EN_ON as u32,
    }
}

/// Applies `SQL_ATTR_QUERY_TIMEOUT` on a connection handle.
///
/// ODBC lets the statement options be set on the connection, where they mean
/// "this is the default, and also apply it to what is already open". msodbcsql
/// implements exactly that (`sqlcmisc.cpp:2879-2935`): it records the connection
/// default and walks the statement list, and `SQLAllocHandle(SQL_HANDLE_STMT)`
/// then seeds new statements from the stored default (`sqlcfunc.cpp:173`).
///
/// Unlike the statement-level set, this is write-only: msodbcsql answers
/// `SQLGetConnectAttr(SQL_ATTR_QUERY_TIMEOUT)` with `HY092`, so there is
/// deliberately no matching arm in `SQLGetConnectAttrW`.
///
/// The fan-out is worst-wins, matching [`close_all_cursors`](super::txn): an
/// unusable statement does not stop the walk, so the healthy ones still get the
/// value, but the call reports `SQL_ERROR` because the attribute did not reach
/// every statement.
fn set_query_timeout(dbc: &DbcHandle, requested: u64) -> SqlReturn {
    let (seconds, clamped) = clamp_query_timeout(requested as usize);

    let statements = {
        let Ok(mut state) = dbc.inner.lock() else {
            error!("SQLSetConnectAttrW: dbc mutex poisoned");
            return SQL_ERROR;
        };
        free_errors(&mut state);
        state.stmt_query_timeout = seconds;
        // Clone so the statement locks below are taken with the DBC mutex
        // released, matching `close_all_cursors`.
        state.statements.clone()
    };

    let mut poisoned = false;
    for stmt_ptr in statements {
        // SAFETY: every pointer in `statements` came from
        // `handle_to_raw::<StmtHandle>` and is owned by this DBC.
        // A concurrent `SQLFreeHandle(SQL_HANDLE_STMT)` could still free it
        // between the clone above and this call — the same handle-lifetime gap
        // `close_all_cursors` and `SQLDisconnect` document (see the TODO in
        // `disconnect.rs`), which refcounted handles will close driver-wide.
        let stmt = unsafe { handle_from_raw::<StmtHandle>(stmt_ptr) };
        match stmt.inner.lock() {
            Ok(mut stmt_state) => stmt_state.query_timeout = seconds,
            // One unusable statement must not abort the fan-out — the others
            // still need the value — but it does mean the attribute was not
            // applied everywhere, so the call cannot report plain success.
            Err(_) => {
                error!(?stmt_ptr, "SQLSetConnectAttrW: stmt mutex poisoned");
                poisoned = true;
            }
        }
    }
    debug!(seconds, "SQLSetConnectAttrW: query timeout applied");

    // Worst-wins: a statement that never received the value outranks clamping.
    let outcome = if poisoned {
        (ERR_STATEMENT_UNUSABLE, SQL_ERROR)
    } else if clamped {
        (WARN_OPTION_VALUE_CHANGED, SQL_SUCCESS_WITH_INFO)
    } else {
        return SQL_SUCCESS;
    };

    let Ok(mut state) = dbc.inner.lock() else {
        error!("SQLSetConnectAttrW: dbc mutex poisoned");
        return SQL_ERROR;
    };
    post_diag(&mut state, outcome.0);
    outcome.1
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
    use crate::handles::dbc::VendorConnOverrides;
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
    fn access_token_after_connect_is_rejected() {
        // B6: an access token is a pre-connect credential. A reset is the same
        // physical login and never re-authenticates, so a rotated token cannot be
        // applied to a live session — it must drive a fresh SQLDriverConnect (new
        // physical login). Setting it after connect is rejected with HY011,
        // locking in that there is no live-session token-refresh path.
        let h = TestHandles::with_env_dbc();
        h.mark_dbc_connected();
        let buf = make_token_struct("rotated.jwt.value");
        let ret = unsafe {
            sql_set_connect_attr_w(
                h.dbc,
                SQL_COPT_SS_ACCESS_TOKEN,
                buf.as_ptr() as SqlPointer,
                SQL_IS_POINTER,
            )
        };
        assert_eq!(ret, SQL_ERROR);
        let dbc = unsafe { handle_from_raw::<DbcHandle>(h.dbc) };
        let state = dbc.inner.lock().unwrap();
        assert_eq!(state.diag_records()[0].sql_state, SQLSTATE_HY011);
        assert!(
            state.access_token.is_none(),
            "a post-connect token must not overwrite live-session credentials"
        );
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
        // 99999 is not an attribute identifier in any namespace, so it must fail
        // rather than succeed silently. msodbcsql answers HY092 ("invalid
        // attribute/option identifier") here, not HYC00. (1234 would be a poor
        // choice: it is a real undocumented SQL_COPT_SS_* id msodbcsql accepts.)
        let ret = unsafe { sql_set_connect_attr_w(h.dbc, 99999, std::ptr::null_mut(), 0) };
        assert_eq!(ret, SQL_ERROR);
        let dbc = unsafe { handle_from_raw::<DbcHandle>(h.dbc) };
        let state = dbc.inner.lock().unwrap();
        assert_eq!(state.diag_records[0].sql_state, SQLSTATE_HY092);
    }

    /// `SQL_COPT_SS_MARS_ENABLED` (1224) is a real msodbcsql connection
    /// attribute this driver does not implement, so it must report `HYC00`.
    /// `attrs_before` forwards identifiers unfiltered, and a caller probing for
    /// MARS has to be able to tell "not available" from "not an attribute".
    #[test]
    fn attribute_known_to_msodbcsql_reports_not_implemented() {
        let h = TestHandles::with_env_dbc();
        let ret = unsafe { sql_set_connect_attr_w(h.dbc, 1224, std::ptr::null_mut(), 0) };
        assert_eq!(ret, SQL_ERROR);
        let dbc = unsafe { handle_from_raw::<DbcHandle>(h.dbc) };
        let state = dbc.inner.lock().unwrap();
        assert_eq!(state.diag_records[0].sql_state, SQLSTATE_HYC00);
    }

    /// A statement attribute aimed at a connection is `HYC00`, not `HY092`:
    /// msodbcsql accepts the ODBC 2.x statement options here and fans them out
    /// to every statement (`SQL_ATTR_MAX_ROWS` = 1). This driver implements the
    /// fan-out only for `SQL_ATTR_QUERY_TIMEOUT`.
    #[test]
    fn statement_option_on_a_connection_reports_not_implemented() {
        let h = TestHandles::with_env_dbc();
        let ret = unsafe { sql_set_connect_attr_w(h.dbc, 1, 10 as SqlPointer, 0) };
        assert_eq!(ret, SQL_ERROR);
        let dbc = unsafe { handle_from_raw::<DbcHandle>(h.dbc) };
        let state = dbc.inner.lock().unwrap();
        assert_eq!(state.diag_records[0].sql_state, SQLSTATE_HYC00);
    }

    /// A statement-only identifier msodbcsql rejects on a connection stays
    /// `HY092`. `SQL_ATTR_ROW_ARRAY_SIZE` (27) is outside the fan-out band.
    #[test]
    fn statement_only_attribute_on_a_connection_stays_invalid() {
        let h = TestHandles::with_env_dbc();
        let ret = unsafe { sql_set_connect_attr_w(h.dbc, 27, 10 as SqlPointer, 0) };
        assert_eq!(ret, SQL_ERROR);
        let dbc = unsafe { handle_from_raw::<DbcHandle>(h.dbc) };
        let state = dbc.inner.lock().unwrap();
        assert_eq!(state.diag_records[0].sql_state, SQLSTATE_HY092);
    }

    /// `SQL_ATTR_QUERY_TIMEOUT` set on a connection is a bulk statement-option
    /// set: it applies to statements that already exist and becomes the default
    /// for ones allocated later.
    #[test]
    fn query_timeout_on_a_connection_fans_out_and_is_inherited() {
        let mut h = TestHandles::with_env_dbc();
        let existing = h.alloc_extra_stmt();

        let ret = unsafe {
            sql_set_connect_attr_w(h.dbc, SQL_ATTR_QUERY_TIMEOUT, 17usize as SqlPointer, 0)
        };
        assert_eq!(ret, SQL_SUCCESS);

        // Already open when the attribute was set.
        let stmt = unsafe { handle_from_raw::<StmtHandle>(existing) };
        assert_eq!(stmt.inner.lock().unwrap().query_timeout, 17);

        // Allocated afterwards: seeded from the connection default.
        let later = h.alloc_extra_stmt();
        let stmt = unsafe { handle_from_raw::<StmtHandle>(later) };
        assert_eq!(stmt.inner.lock().unwrap().query_timeout, 17);
    }

    #[test]
    fn query_timeout_on_a_connection_clamps_with_a_warning() {
        let mut h = TestHandles::with_env_dbc();
        let stmt_handle = h.alloc_extra_stmt();
        let ret = unsafe {
            sql_set_connect_attr_w(h.dbc, SQL_ATTR_QUERY_TIMEOUT, 0x10000usize as SqlPointer, 0)
        };
        assert_eq!(ret, SQL_SUCCESS_WITH_INFO);

        let dbc = unsafe { handle_from_raw::<DbcHandle>(h.dbc) };
        assert_eq!(
            dbc.inner.lock().unwrap().diag_records[0].sql_state,
            *b"01S02"
        );

        // The clamped value, not the requested one, is what statements receive.
        let stmt = unsafe { handle_from_raw::<StmtHandle>(stmt_handle) };
        assert_eq!(stmt.inner.lock().unwrap().query_timeout, 0xfffe);
    }

    #[test]
    fn query_timeout_on_a_connection_with_no_statements_is_still_stored() {
        let h = TestHandles::with_env_dbc();
        let ret = unsafe {
            sql_set_connect_attr_w(h.dbc, SQL_ATTR_QUERY_TIMEOUT, 42usize as SqlPointer, 0)
        };
        assert_eq!(ret, SQL_SUCCESS);
        let dbc = unsafe { handle_from_raw::<DbcHandle>(h.dbc) };
        assert_eq!(dbc.inner.lock().unwrap().stmt_query_timeout, 42);
    }

    #[test]
    fn query_timeout_fan_out_reports_a_poisoned_statement() {
        // Worst-wins, matching `close_all_cursors`: an unusable statement must
        // not abort the fan-out — the connection default still lands and the
        // healthy statements still get the value — but the attribute did not
        // reach every statement, so the call cannot claim success.
        let mut h = TestHandles::with_env_dbc();
        let bad = h.alloc_extra_stmt();
        let good = h.alloc_extra_stmt();

        let bad_stmt = unsafe { handle_from_raw::<StmtHandle>(bad) };
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _guard = bad_stmt.inner.lock().unwrap();
            panic!("poison the stmt lock");
        }));

        let ret = unsafe {
            sql_set_connect_attr_w(h.dbc, SQL_ATTR_QUERY_TIMEOUT, 19usize as SqlPointer, 0)
        };
        assert_eq!(ret, SQL_ERROR);

        let dbc = unsafe { handle_from_raw::<DbcHandle>(h.dbc) };
        {
            let state = dbc.inner.lock().unwrap();
            assert_eq!(state.diag_records[0].sql_state, SQLSTATE_HY000);
        }
        assert_eq!(dbc.inner.lock().unwrap().stmt_query_timeout, 19);
        let good_stmt = unsafe { handle_from_raw::<StmtHandle>(good) };
        assert_eq!(good_stmt.inner.lock().unwrap().query_timeout, 19);
    }

    #[test]
    fn query_timeout_fails_cleanly_on_a_poisoned_connection() {
        let h = TestHandles::with_env_dbc();
        let dbc = unsafe { handle_from_raw::<DbcHandle>(h.dbc) };
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _guard = dbc.inner.lock().unwrap();
            panic!("poison the dbc lock");
        }));
        let ret = unsafe {
            sql_set_connect_attr_w(h.dbc, SQL_ATTR_QUERY_TIMEOUT, 7usize as SqlPointer, 0)
        };
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

    #[test]
    fn vendor_attributes_store_normalized_values() {
        let h = TestHandles::with_env_dbc();
        for (attribute, raw, expected) in [
            (SQL_COPT_SS_ENCRYPT, 0usize, 0u32),
            (SQL_COPT_SS_ENCRYPT, 1, 1),
            (SQL_COPT_SS_ENCRYPT, 2, 2),
            // Out of range folds to "on" rather than erroring: msodbcsql
            // connects encrypted for 3, 7 and -1, and a get after setting 7
            // reads back 1.
            (SQL_COPT_SS_ENCRYPT, 7, 1),
            (SQL_COPT_SS_TRUST_SERVER_CERTIFICATE, 0, 0),
            (SQL_COPT_SS_TRUST_SERVER_CERTIFICATE, 7, 1),
            (SQL_COPT_SS_INTEGRATED_SECURITY, 0, 0),
            (SQL_COPT_SS_INTEGRATED_SECURITY, 1, 1),
        ] {
            let ret = unsafe { sql_set_connect_attr_w(h.dbc, attribute, raw as SqlPointer, 0) };
            assert_eq!(ret, SQL_SUCCESS, "attribute {attribute} value {raw}");

            let dbc = unsafe { handle_from_raw::<DbcHandle>(h.dbc) };
            let overrides = dbc.inner.lock().unwrap().vendor_overrides.clone();
            let stored = match attribute {
                SQL_COPT_SS_ENCRYPT => overrides.encrypt,
                SQL_COPT_SS_TRUST_SERVER_CERTIFICATE => overrides.trust_server_certificate,
                _ => overrides.integrated_security,
            };
            assert_eq!(stored, Some(expected), "attribute {attribute} value {raw}");
        }
    }

    #[test]
    fn vendor_attributes_after_connect_are_rejected() {
        // These select the transport and the credential, both fixed by the
        // handshake, so a late set could never apply. Measured against
        // msodbcsql: post-connect sets in the vendor band return HY011.
        for attribute in [
            SQL_COPT_SS_ENCRYPT,
            SQL_COPT_SS_TRUST_SERVER_CERTIFICATE,
            SQL_COPT_SS_INTEGRATED_SECURITY,
        ] {
            let h = TestHandles::with_env_dbc();
            let dbc = unsafe { handle_from_raw::<DbcHandle>(h.dbc) };
            dbc.inner.lock().unwrap().connection_state = ConnectionState::Connected;

            let ret = unsafe { sql_set_connect_attr_w(h.dbc, attribute, 1usize as SqlPointer, 0) };
            assert_eq!(ret, SQL_ERROR, "attribute {attribute}");
            let state = dbc.inner.lock().unwrap();
            assert_eq!(state.diag_records()[0].sql_state, SQLSTATE_HY011);
            assert_eq!(
                state.vendor_overrides,
                VendorConnOverrides::default(),
                "a rejected set must not change the stored value"
            );
        }
    }

    #[test]
    fn encrypt_normalization_covers_the_whole_range() {
        assert_eq!(normalize_encrypt(0), 0);
        assert_eq!(normalize_encrypt(1), 1);
        assert_eq!(normalize_encrypt(2), 2);
        for out_of_range in [3u64, 7, u64::MAX] {
            assert_eq!(
                normalize_encrypt(out_of_range),
                1,
                "out-of-range {out_of_range} must fold to on, not error"
            );
        }
    }
}
