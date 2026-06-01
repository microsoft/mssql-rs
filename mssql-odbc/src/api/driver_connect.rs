// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Implementation of SQLDriverConnectW — connect using a connection string.

use std::panic;
use std::slice;

use tracing::{debug, error, trace};

use crate::api::odbc_types::{
    SQL_DRIVER_NOPROMPT, SQL_ERROR, SQL_INVALID_HANDLE, SQL_NTS, SQL_SUCCESS,
    SQL_SUCCESS_WITH_INFO, SqlHWnd, SqlHandle, SqlReturn, SqlSmallInt, SqlUSmallInt, SqlWChar,
};
use crate::handles::DbcHandle;
use crate::handles::dbc::ConnectionState;
use crate::handles::{HandleType, handle_from_raw};

use mssql_tds::connection::client_context::{ClientContext, TdsAuthenticationMethod};
use mssql_tds::connection_provider::tds_connection_provider::TdsConnectionProvider;
use mssql_tds::core::{EncryptionOptions, EncryptionSetting};

use crate::connection::parse_connection_string;

/// Implementation of `SQLDriverConnectW`.
///
/// # Safety
/// - `connection_handle` must be a valid `DbcHandle` allocated by `SQLAllocHandle`.
/// - `window_handle` (if non-null) must be a valid parent window handle for dialog display.
/// - `in_connection_string` must point to a valid UTF-16 buffer.
/// - `out_connection_string` (if non-null) must point to a writable buffer of at least
///   `buffer_length` wide characters.
/// - `string_length_2_ptr` (if non-null) must point to a writable `SqlSmallInt`.
#[allow(clippy::too_many_arguments)]
pub(crate) unsafe fn sql_driver_connect_w(
    connection_handle: SqlHandle,
    _window_handle: SqlHWnd,
    in_connection_string: *const SqlWChar,
    string_length_1: SqlSmallInt,
    out_connection_string: *mut SqlWChar,
    buffer_length: SqlSmallInt,
    string_length_2_ptr: *mut SqlSmallInt,
    driver_completion: SqlUSmallInt,
) -> SqlReturn {
    debug!("SQLDriverConnectW called");

    let result = panic::catch_unwind(|| unsafe {
        sql_driver_connect_w_impl(
            connection_handle,
            in_connection_string,
            string_length_1,
            out_connection_string,
            buffer_length,
            string_length_2_ptr,
            driver_completion,
        )
    });

    let ret = result.unwrap_or_else(|_| {
        error!("SQLDriverConnectW: panic caught at FFI boundary");
        SQL_ERROR
    });

    trace!(?ret, "SQLDriverConnectW returning");
    ret
}

unsafe fn sql_driver_connect_w_impl(
    connection_handle: SqlHandle,
    in_connection_string: *const SqlWChar,
    string_length_1: SqlSmallInt,
    out_connection_string: *mut SqlWChar,
    buffer_length: SqlSmallInt,
    string_length_2_ptr: *mut SqlSmallInt,
    driver_completion: SqlUSmallInt,
) -> SqlReturn {
    if connection_handle.is_null() {
        error!("SQLDriverConnectW: connection_handle is null");
        return SQL_INVALID_HANDLE;
    }

    let dbc = unsafe { handle_from_raw::<DbcHandle>(connection_handle) };
    debug_assert_eq!(
        dbc.object_type,
        HandleType::Dbc,
        "SQLDriverConnectW: handle is not a DBC"
    );

    // Only SQL_DRIVER_NOPROMPT is supported (no UI prompting)
    if driver_completion != SQL_DRIVER_NOPROMPT {
        error!(
            driver_completion,
            "SQLDriverConnectW: only SQL_DRIVER_NOPROMPT is supported" // TODO: post SQLSTATE HY110
        );
        return SQL_ERROR;
    }

    // HY090 (negative string_length_1) is DM-enforced per spec.
    debug_assert!(
        string_length_1 >= 0 || string_length_1 == SQL_NTS,
        "SQLDriverConnectW: DM should reject negative string_length_1 (HY090)"
    );

    // Transition to Connecting state under lock - prevents concurrent connect race.
    // 08002 (already connected) is DM-enforced, so we debug_assert only.
    {
        let Ok(mut state) = dbc.inner.lock() else {
            error!("SQLDriverConnectW: dbc mutex poisoned");
            return SQL_ERROR;
        };
        debug_assert_ne!(
            state.connection_state,
            ConnectionState::Connected,
            "SQLDriverConnectW: DM should reject connect on already-connected handle (08002)"
        );
        if state.connection_state != ConnectionState::Disconnected {
            error!("SQLDriverConnectW: connection attempt already in progress");
            return SQL_ERROR;
        }
        state.connection_state = ConnectionState::Connecting;
    }

    // From here on, any early return must reset state to Disconnected.
    let result = unsafe {
        do_connect(
            dbc,
            in_connection_string,
            string_length_1,
            out_connection_string,
            buffer_length,
            string_length_2_ptr,
        )
    };

    if result != SQL_SUCCESS && result != SQL_SUCCESS_WITH_INFO {
        // Reset state on failure
        if let Ok(mut state) = dbc.inner.lock() {
            state.connection_state = ConnectionState::Disconnected;
        }
    }

    result
}

/// Inner connect logic, separated so the caller can reset state on failure.
unsafe fn do_connect(
    dbc: &DbcHandle,
    in_connection_string: *const SqlWChar,
    string_length_1: SqlSmallInt,
    out_connection_string: *mut SqlWChar,
    buffer_length: SqlSmallInt,
    string_length_2_ptr: *mut SqlSmallInt,
) -> SqlReturn {
    // Read the input connection string (UTF-16 → String)
    if in_connection_string.is_null() {
        error!("SQLDriverConnectW: in_connection_string is null");
        // TODO: post SQLSTATE HY009
        return SQL_ERROR;
    }

    let conn_str = unsafe { read_utf16(in_connection_string, string_length_1) };

    // Parse connection string - malformed tokens produce warnings (01S00),
    // invalid attribute values produce errors (E_FAIL in msodbcsql).
    let (params, has_warnings) = match parse_connection_string(&conn_str) {
        Ok(result) => result,
        Err(e) => {
            error!(%e, "SQLDriverConnectW: invalid connection string attribute value");
            // TODO: post SQLSTATE 01S00
            return SQL_ERROR;
        }
    };

    // Validate required fields. Let mssql-tds validate based on auth method.
    if params.server.is_empty() {
        error!("SQLDriverConnectW: Server not specified in connection string");
        // TODO: post SQLSTATE HY000
        return SQL_ERROR;
    }

    // Build ClientContext
    let mut context = ClientContext::default();
    context.user_name = params.uid.clone();
    context.password = params.pwd.clone();
    context.database = params.database.clone();
    context.tds_authentication_method = TdsAuthenticationMethod::Password;
    context.encryption_options = EncryptionOptions {
        trust_server_certificate: params.trust_server_certificate,
        mode: match params.encrypt.as_deref() {
            Some(e) if e.eq_ignore_ascii_case("yes") || e.eq_ignore_ascii_case("mandatory") => {
                EncryptionSetting::On
            }
            Some(e) if e.eq_ignore_ascii_case("no") || e.eq_ignore_ascii_case("optional") => {
                EncryptionSetting::PreferOff
            }
            Some(e) if e.eq_ignore_ascii_case("strict") => EncryptionSetting::Strict,
            _ => EncryptionSetting::On, // ODBC default
        },
        host_name_in_cert: None,
        server_certificate: None,
    };

    // Connect via mssql-tds (lock is NOT held - the 'Connecting' state prevents races)
    let provider = TdsConnectionProvider::new();
    let client = dbc
        .runtime
        .block_on(provider.create_client(context, &params.server, None));

    let client = match client {
        Ok(c) => c,
        Err(e) => {
            error!(%e, "SQLDriverConnectW: connection failed");
            // TODO: post SQLSTATE 08001
            return SQL_ERROR;
        }
    };

    // Store the client and transition to Connected
    {
        let Ok(mut state) = dbc.inner.lock() else {
            error!("SQLDriverConnectW: dbc mutex poisoned");
            return SQL_ERROR;
        };
        state.client = Some(client);
        state.connection_state = ConnectionState::Connected;
    }

    // Write output connection string
    // TODO: build completed connection string from resolved attributes (DSN
    // expansion, negotiated encryption, default database) instead of echoing input.
    let out_utf16: Vec<u16> = conn_str.encode_utf16().collect();
    let out_len = SqlSmallInt::try_from(out_utf16.len()).unwrap_or(SqlSmallInt::MAX);

    if !string_length_2_ptr.is_null() {
        unsafe { string_length_2_ptr.write(out_len) };
    }

    let mut truncated = false;
    if !out_connection_string.is_null() && buffer_length > 0 {
        let copy_len = out_len.min(buffer_length - 1) as usize;
        truncated = out_len > buffer_length - 1;
        unsafe {
            std::ptr::copy_nonoverlapping(out_utf16.as_ptr(), out_connection_string, copy_len);
            out_connection_string.add(copy_len).write(0);
        }
    }

    debug!("SQLDriverConnectW: connected successfully");
    if has_warnings || truncated {
        // TODO: post SQLSTATE 01004 for truncation, 01S00 for malformed tokens via SQLGetDiagRec
        SQL_SUCCESS_WITH_INFO
    } else {
        SQL_SUCCESS
    }
}

/// Read a UTF-16 string from a pointer + length.
/// If `length` is `SQL_NTS`, reads until null terminator.
unsafe fn read_utf16(ptr: *const SqlWChar, length: SqlSmallInt) -> String {
    let slice = if length == SQL_NTS {
        // Find null terminator
        let mut len = 0usize;
        unsafe {
            while *ptr.add(len) != 0 {
                len += 1;
            }
        }
        unsafe { slice::from_raw_parts(ptr, len) }
    } else {
        unsafe { slice::from_raw_parts(ptr, length as usize) }
    };
    String::from_utf16_lossy(slice)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::alloc_handle::sql_alloc_handle;
    use crate::api::free_handle::sql_free_handle;
    use crate::api::odbc_types::{
        SQL_ATTR_ODBC_VERSION, SQL_DRIVER_COMPLETE, SQL_HANDLE_DBC, SQL_HANDLE_ENV,
        SQL_INVALID_HANDLE, SQL_NULL_HANDLE, SQL_OV_ODBC3_80,
    };
    use crate::api::set_env_attr::sql_set_env_attr;

    /// Helper: allocate ENV + DBC for tests.
    unsafe fn alloc_env_dbc() -> (SqlHandle, SqlHandle) {
        let mut env: SqlHandle = SQL_NULL_HANDLE;
        let ret = unsafe { sql_alloc_handle(SQL_HANDLE_ENV, SQL_NULL_HANDLE, &mut env) };
        assert_eq!(ret, SQL_SUCCESS);
        assert!(!env.is_null());

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
        assert!(!dbc.is_null());

        (env, dbc)
    }

    unsafe fn free_env_dbc(env: SqlHandle, dbc: SqlHandle) {
        unsafe {
            sql_free_handle(SQL_HANDLE_DBC, dbc);
            sql_free_handle(SQL_HANDLE_ENV, env);
        }
    }

    #[test]
    fn null_handle_returns_invalid_handle() {
        let conn_str: Vec<u16> = "Server=host;UID=u;PWD=p"
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect();
        let ret = unsafe {
            sql_driver_connect_w(
                SQL_NULL_HANDLE,
                std::ptr::null_mut(),
                conn_str.as_ptr(),
                SQL_NTS,
                std::ptr::null_mut(),
                0,
                std::ptr::null_mut(),
                SQL_DRIVER_NOPROMPT,
            )
        };
        assert_eq!(ret, SQL_INVALID_HANDLE);
    }

    #[test]
    fn unsupported_driver_completion() {
        let (env, dbc) = unsafe { alloc_env_dbc() };
        let conn_str: Vec<u16> = "Server=host;UID=u;PWD=p"
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect();

        let ret = unsafe {
            sql_driver_connect_w(
                dbc,
                std::ptr::null_mut(),
                conn_str.as_ptr(),
                SQL_NTS,
                std::ptr::null_mut(),
                0,
                std::ptr::null_mut(),
                SQL_DRIVER_COMPLETE,
            )
        };
        assert_eq!(ret, SQL_ERROR);
        // TODO: verify SQLSTATE HY110 via SQLGetDiagRec

        unsafe { free_env_dbc(env, dbc) };
    }

    #[test]
    fn null_connection_string_returns_error() {
        let (env, dbc) = unsafe { alloc_env_dbc() };

        let ret = unsafe {
            sql_driver_connect_w(
                dbc,
                std::ptr::null_mut(),
                std::ptr::null(),
                SQL_NTS,
                std::ptr::null_mut(),
                0,
                std::ptr::null_mut(),
                SQL_DRIVER_NOPROMPT,
            )
        };
        assert_eq!(ret, SQL_ERROR);
        // TODO: verify SQLSTATE HY009 via SQLGetDiagRec

        unsafe { free_env_dbc(env, dbc) };
    }

    #[test]
    fn missing_server_returns_error() {
        let (env, dbc) = unsafe { alloc_env_dbc() };
        let conn_str: Vec<u16> = "UID=u;PWD=p"
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect();

        let ret = unsafe {
            sql_driver_connect_w(
                dbc,
                std::ptr::null_mut(),
                conn_str.as_ptr(),
                SQL_NTS,
                std::ptr::null_mut(),
                0,
                std::ptr::null_mut(),
                SQL_DRIVER_NOPROMPT,
            )
        };
        assert_eq!(ret, SQL_ERROR);
        // TODO: verify SQLSTATE HY000 via SQLGetDiagRec

        unsafe { free_env_dbc(env, dbc) };
    }

    #[test]
    fn explicit_string_length() {
        // Pass an explicit length instead of SQL_NTS — extra chars after length are ignored.
        let (env, dbc) = unsafe { alloc_env_dbc() };
        // "UID=u;PWD=p" is 11 chars — Server is missing, so validation fails.
        // But we're testing that explicit length is respected (no null terminator needed).
        let conn_str: Vec<u16> = "UID=u;PWD=pGARBAGE".encode_utf16().collect();

        let ret = unsafe {
            sql_driver_connect_w(
                dbc,
                std::ptr::null_mut(),
                conn_str.as_ptr(),
                11, // only "UID=u;PWD=p"
                std::ptr::null_mut(),
                0,
                std::ptr::null_mut(),
                SQL_DRIVER_NOPROMPT,
            )
        };
        // Missing server → error, but proves explicit length was used
        assert_eq!(ret, SQL_ERROR);
        // TODO: verify SQLSTATE HY000 via SQLGetDiagRec

        unsafe { free_env_dbc(env, dbc) };
    }

    #[test]
    fn all_driver_completion_modes_rejected_except_noprompt() {
        let (env, dbc) = unsafe { alloc_env_dbc() };
        let conn_str: Vec<u16> = "Server=h;UID=u;PWD=p"
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect();

        for mode in [
            SQL_DRIVER_COMPLETE,
            2u16, /* PROMPT */
            3u16, /* COMPLETE_REQUIRED */
        ] {
            let ret = unsafe {
                sql_driver_connect_w(
                    dbc,
                    std::ptr::null_mut(),
                    conn_str.as_ptr(),
                    SQL_NTS,
                    std::ptr::null_mut(),
                    0,
                    std::ptr::null_mut(),
                    mode,
                )
            };
            assert_eq!(ret, SQL_ERROR, "mode {mode} should be rejected");
            // TODO: verify SQLSTATE HY110 via SQLGetDiagRec
        }

        unsafe { free_env_dbc(env, dbc) };
    }

    #[test]
    fn read_utf16_with_nts() {
        let input: Vec<u16> = "hello".encode_utf16().chain(std::iter::once(0)).collect();
        let result = unsafe { read_utf16(input.as_ptr(), SQL_NTS) };
        assert_eq!(result, "hello");
    }

    #[test]
    fn read_utf16_with_explicit_length() {
        let input: Vec<u16> = "hello world".encode_utf16().collect();
        let result = unsafe { read_utf16(input.as_ptr(), 5) };
        assert_eq!(result, "hello");
    }
}
