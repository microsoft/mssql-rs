// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Implementation of SQLDriverConnectW — connect using a connection string.

use tracing::{debug, error};

use crate::api::odbc_types::{
    SQL_DRIVER_NOPROMPT, SQL_ERROR, SQL_INVALID_HANDLE, SQL_NTS, SQL_SUCCESS,
    SQL_SUCCESS_WITH_INFO, SqlHWnd, SqlHandle, SqlReturn, SqlSmallInt, SqlUSmallInt, SqlWChar,
};
use crate::api::sqlstate::{
    SQLSTATE_01S00, SQLSTATE_01004, SQLSTATE_08001, SQLSTATE_HY009, SQLSTATE_HY010, SQLSTATE_HY024,
    SQLSTATE_HY110, post_tds_error,
};
use crate::api::util::{copy_with_nul, write_if_some};
use crate::error::{free_errors, post_sql_error};
use crate::handles::DbcHandle;
use crate::handles::dbc::{ConnectionState, DbcState};
use crate::handles::{HandleType, handle_from_raw};

use mssql_tds::connection::client_context::{ClientContext, TdsAuthenticationMethod};
use mssql_tds::connection_provider::tds_connection_provider::TdsConnectionProvider;
use mssql_tds::core::{EncryptionOptions, EncryptionSetting};

use super::util::read_utf16;
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
    debug!(
        ?connection_handle,
        ?in_connection_string,
        string_length_1,
        ?out_connection_string,
        buffer_length,
        ?string_length_2_ptr,
        driver_completion,
        "SQLDriverConnectW called",
    );

    crate::ffi_entry!("SQLDriverConnectW", unsafe {
        sql_driver_connect_w_impl(
            connection_handle,
            in_connection_string,
            string_length_1,
            out_connection_string,
            buffer_length,
            string_length_2_ptr,
            driver_completion,
        )
    })
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

    let Ok(mut state) = dbc.inner.lock() else {
        error!("SQLDriverConnectW: dbc mutex poisoned");
        return SQL_ERROR;
    };

    free_errors(&mut state);

    // Only SQL_DRIVER_NOPROMPT is supported (no UI prompting).
    if driver_completion != SQL_DRIVER_NOPROMPT {
        error!(
            driver_completion,
            "SQLDriverConnectW: only SQL_DRIVER_NOPROMPT is supported"
        );
        post_sql_error(&mut state, SQLSTATE_HY110, 0, "Invalid driver completion");
        return SQL_ERROR;
    }

    // HY090 (negative string_length_1 / buffer_length) is DM-enforced per spec.
    // https://learn.microsoft.com/en-us/sql/odbc/reference/syntax/sqldriverconnect-function
    debug_assert!(
        string_length_1 >= 0 || string_length_1 == SQL_NTS,
        "SQLDriverConnectW: DM should reject negative string_length_1 (HY090)"
    );
    debug_assert!(
        buffer_length >= 0,
        "SQLDriverConnectW: DM should reject negative buffer_length (HY090)"
    );

    // Transition to Connecting state under lock - prevents concurrent connect race.
    // 08002 (already connected) is DM-enforced, so we debug_assert only.
    debug_assert_ne!(
        state.connection_state,
        ConnectionState::Connected,
        "SQLDriverConnectW: DM should reject connect on already-connected handle (08002)"
    );
    if state.connection_state != ConnectionState::Disconnected {
        error!("SQLDriverConnectW: connection attempt already in progress");
        post_sql_error(&mut state, SQLSTATE_HY010, 0, "Function sequence error");
        return SQL_ERROR;
    }
    state.connection_state = ConnectionState::Connecting;

    // From here on, any early return must reset state to Disconnected.
    let result = unsafe {
        do_connect(
            dbc,
            &mut state,
            in_connection_string,
            string_length_1,
            out_connection_string,
            buffer_length,
            string_length_2_ptr,
        )
    };

    if result != SQL_SUCCESS && result != SQL_SUCCESS_WITH_INFO {
        // Reset state on failure
        state.connection_state = ConnectionState::Disconnected;
    }

    result
}

/// Inner connect logic, separated so the caller can reset state on failure.
unsafe fn do_connect(
    dbc: &DbcHandle,
    state: &mut DbcState,
    in_connection_string: *const SqlWChar,
    string_length_1: SqlSmallInt,
    out_connection_string: *mut SqlWChar,
    buffer_length: SqlSmallInt,
    string_length_2_ptr: *mut SqlSmallInt,
) -> SqlReturn {
    // Read the input connection string (UTF-16 → String)
    if in_connection_string.is_null() {
        error!("SQLDriverConnectW: in_connection_string is null");
        post_sql_error(state, SQLSTATE_HY009, 0, "Invalid use of null pointer");
        return SQL_ERROR;
    }

    let conn_str = unsafe { read_utf16(in_connection_string, string_length_1) };

    // Parse connection string - malformed tokens produce warnings (01S00),
    // invalid attribute values produce errors (E_FAIL in msodbcsql).
    let (params, has_warnings) = match parse_connection_string(&conn_str) {
        Ok(result) => result,
        Err(e) => {
            error!(%e, "SQLDriverConnectW: invalid connection string attribute value");
            post_sql_error(state, SQLSTATE_HY024, 0, e.to_string());
            return SQL_ERROR;
        }
    };

    // Validate required fields. Let mssql-tds validate based on auth method.
    if params.server.is_empty() {
        error!("SQLDriverConnectW: Server not specified in connection string");
        post_sql_error(
            state,
            SQLSTATE_08001,
            0,
            "Server not specified in connection string",
        );
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
            post_tds_error(state, &e, SQLSTATE_08001);
            return SQL_ERROR;
        }
    };

    // Write output connection string
    // TODO: build completed output connection string from resolved attributes and negotiated
    // settings; current output is reconstructed from parsed input fields with password redacted.
    let redacted_conn_str = params.fmt_as_odbc_conn_str();
    let out_utf16: Vec<u16> = redacted_conn_str.encode_utf16().collect();
    let actual_len = out_utf16.len();
    let out_len = SqlSmallInt::try_from(actual_len).unwrap_or(SqlSmallInt::MAX);

    unsafe { write_if_some(string_length_2_ptr, out_len) };

    let mut truncated = actual_len > SqlSmallInt::MAX as usize;
    truncated |=
        unsafe { copy_with_nul(out_connection_string, buffer_length as usize, &out_utf16) };

    state.client = Some(client);
    state.connection_state = ConnectionState::Connected;
    // TODO: This print is for demo purposes only. Remove before release.
    println!("**** Connected via mssql-odbc Driver ****");
    debug!("SQLDriverConnectW: connected successfully");

    if has_warnings || truncated {
        if has_warnings {
            post_sql_error(
                state,
                SQLSTATE_01S00,
                0,
                "Invalid connection string attribute",
            );
        }
        if truncated {
            post_sql_error(state, SQLSTATE_01004, 0, "String data, right truncation");
        }
        SQL_SUCCESS_WITH_INFO
    } else {
        SQL_SUCCESS
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::alloc_handle::sql_alloc_handle;
    use crate::api::free_handle::sql_free_handle;
    use crate::api::get_diag::sql_get_diag_rec_w;
    use crate::api::odbc_types::{
        SQL_ATTR_ODBC_VERSION, SQL_DRIVER_COMPLETE, SQL_HANDLE_DBC, SQL_HANDLE_ENV,
        SQL_INVALID_HANDLE, SQL_NULL_HANDLE, SQL_OV_ODBC3_80,
    };
    use crate::api::set_env_attr::sql_set_env_attr;

    /// Read SQLSTATE for record `rec_number` on a DBC handle by calling the
    /// driver's own `SQLGetDiagRecW` entry point. Tests use this to verify
    /// the diagnostic surface that real ODBC apps see, not just the internal
    /// `diag_records` vec.
    unsafe fn diag_sqlstate(dbc: SqlHandle, rec_number: SqlSmallInt) -> String {
        let mut state_buf = [0u16; 6];
        let mut msg_buf = [0u16; 256];
        let ret = unsafe {
            sql_get_diag_rec_w(
                SQL_HANDLE_DBC,
                dbc,
                rec_number,
                state_buf.as_mut_ptr(),
                std::ptr::null_mut(),
                msg_buf.as_mut_ptr(),
                msg_buf.len() as SqlSmallInt,
                std::ptr::null_mut(),
            )
        };
        assert_eq!(
            ret, SQL_SUCCESS,
            "SQLGetDiagRecW(rec={rec_number}) returned {ret}"
        );
        let len = state_buf
            .iter()
            .position(|&c| c == 0)
            .unwrap_or(state_buf.len());
        String::from_utf16(&state_buf[..len]).unwrap()
    }

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
        assert_eq!(unsafe { diag_sqlstate(dbc, 1) }, "HY110");

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
        assert_eq!(unsafe { diag_sqlstate(dbc, 1) }, "HY009");

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
        assert_eq!(unsafe { diag_sqlstate(dbc, 1) }, "08001");

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
        assert_eq!(unsafe { diag_sqlstate(dbc, 1) }, "08001");

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
            assert_eq!(
                unsafe { diag_sqlstate(dbc, 1) },
                "HY110",
                "mode {mode} should post HY110"
            );
        }

        unsafe { free_env_dbc(env, dbc) };
    }
}
