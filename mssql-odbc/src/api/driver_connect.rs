// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Implementation of SQLDriverConnectW — connect using a connection string.

use tracing::{debug, error};

use crate::api::odbc_types::{
    SQL_DRIVER_NOPROMPT, SQL_ERROR, SQL_INVALID_HANDLE, SQL_NTS, SQL_SUCCESS,
    SQL_SUCCESS_WITH_INFO, SqlHWnd, SqlHandle, SqlReturn, SqlSmallInt, SqlUSmallInt, SqlWChar,
};
use crate::api::sqlstate::{
    ERR_FUNCTION_SEQUENCE, ERR_INVALID_CONNECTION_STRING_ATTRIBUTE, ERR_INVALID_NULL_POINTER,
    ERR_STRING_RIGHT_TRUNCATION, SQLSTATE_08001, SQLSTATE_HY024, SQLSTATE_HY110, SQLSTATE_HYC00,
    post_diag, post_tds_error, post_tds_info_messages,
};
use crate::api::util::{copy_with_nul, write_if_some};
use crate::error::{free_errors, post_sql_error};
use crate::handles::DbcHandle;
use crate::handles::dbc::{ConnectionState, DbcState};
use crate::handles::{HandleType, handle_from_raw};

use mssql_tds::connection::client_context::{ClientContext, IPAddressPreference};
use mssql_tds::connection_provider::tds_connection_provider::TdsConnectionProvider;
use mssql_tds::core::{EncryptionOptions, EncryptionSetting};
use mssql_tds::message::login_options::ApplicationIntent;
use std::path::PathBuf;

use super::util::read_utf16;
use crate::auth::configure_auth;
use crate::connection::odbc_authentication_transformer::transform_auth;
use crate::connection::odbc_authentication_validator::validate_auth;
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

    debug_assert!(
        string_length_1 == SQL_NTS || string_length_1 >= 0,
        "SQLDriverConnectW: string_length_1 must be SQL_NTS or non-negative (HY090)"
    );

    // Read the input connection string up-front so the inner helper works on `String`.
    // `do_connect` still needs to validate the null-pointer case (it posts a diagnostic),
    // so we capture that condition here and pass an `Option`.
    let conn_str = if in_connection_string.is_null() {
        None
    } else {
        Some(unsafe { read_utf16(in_connection_string, string_length_1) })
    };

    sql_driver_connect_w_safe(
        dbc,
        conn_str,
        out_connection_string,
        buffer_length,
        string_length_2_ptr,
        driver_completion,
    )
}

fn sql_driver_connect_w_safe(
    dbc: &DbcHandle,
    conn_str: Option<String>,
    out_connection_string: *mut SqlWChar,
    buffer_length: SqlSmallInt,
    string_length_2_ptr: *mut SqlSmallInt,
    driver_completion: SqlUSmallInt,
) -> SqlReturn {
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

    // HY090 (negative buffer_length) is DM-enforced per spec.
    // https://learn.microsoft.com/en-us/sql/odbc/reference/syntax/sqldriverconnect-function
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
        post_diag(&mut state, ERR_FUNCTION_SEQUENCE);
        return SQL_ERROR;
    }
    state.connection_state = ConnectionState::Connecting;

    // From here on, any early return must reset state to Disconnected.
    let result = do_connect(
        dbc,
        &mut state,
        conn_str,
        out_connection_string,
        buffer_length,
        string_length_2_ptr,
    );

    if result != SQL_SUCCESS && result != SQL_SUCCESS_WITH_INFO {
        // Reset state on failure
        state.connection_state = ConnectionState::Disconnected;
    }

    result
}

/// Inner connect logic, separated so the caller can reset state on failure.
fn do_connect(
    dbc: &DbcHandle,
    state: &mut DbcState,
    conn_str: Option<String>,
    out_connection_string: *mut SqlWChar,
    buffer_length: SqlSmallInt,
    string_length_2_ptr: *mut SqlSmallInt,
) -> SqlReturn {
    let Some(conn_str) = conn_str else {
        error!("SQLDriverConnectW: in_connection_string is null");
        post_diag(state, ERR_INVALID_NULL_POINTER);
        return SQL_ERROR;
    };

    // Parse connection string - malformed tokens produce warnings (01S00),
    // invalid attribute values produce errors.
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

    // Resolve authentication. Validate the ODBC keyword/credential combination,
    // then transform it into a concrete method with cleaned credentials. Any
    // access token was supplied before connect via SQL_COPT_SS_ACCESS_TOKEN.
    if let Err(e) = validate_auth(
        params.authentication.as_deref(),
        params.trusted_connection,
        &params.uid,
        &params.pwd,
        state.access_token.as_deref(),
    ) {
        error!(%e, "SQLDriverConnectW: authentication validation failed");
        post_sql_error(state, SQLSTATE_HY024, 0, e.to_string());
        return SQL_ERROR;
    }
    let resolved = transform_auth(
        params.authentication.as_deref(),
        params.trusted_connection,
        &params.uid,
        &params.pwd,
        state.access_token.as_deref(),
    );

    // Build ClientContext. T1 wired SQL password, integrated (SSPI/GSSAPI), and
    // pre-acquired access tokens; T2 adds Entra service principal (secret) and
    // managed identity via an Azure-SDK token factory. Methods that still need
    // token acquisition (AD password, interactive, device code, workload
    // identity, default credential, AD integrated) are rejected with HYC00
    // until a later tier.
    let mut context = ClientContext::default();
    context.database = params.database.clone();

    if let Err(method) = configure_auth(&mut context, resolved) {
        error!(
            ?method,
            "SQLDriverConnectW: authentication method not implemented"
        );
        post_sql_error(
            state,
            SQLSTATE_HYC00,
            0,
            format!("Authentication method {method:?} is not yet supported"),
        );
        return SQL_ERROR;
    }

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
        host_name_in_cert: params.host_name_in_certificate.clone(),
        server_certificate: params.server_certificate.as_deref().map(PathBuf::from),
    };

    // Transport, routing, and TLS-identity options. Values were validated during
    // parsing, so the enum mappings below fall through to their default variant.
    if let Some(server_spn) = &params.server_spn {
        context.server_spn = Some(server_spn.clone());
    }
    if let Some(intent) = &params.application_intent {
        context.application_intent = if intent.eq_ignore_ascii_case("readonly") {
            ApplicationIntent::ReadOnly
        } else {
            ApplicationIntent::ReadWrite
        };
    }
    if let Some(multi_subnet_failover) = params.multi_subnet_failover {
        context.multi_subnet_failover = multi_subnet_failover;
    }
    if let Some(count) = params.connect_retry_count {
        context.connect_retry_count = count;
    }
    if let Some(interval) = params.connect_retry_interval {
        context.connect_retry_interval = interval;
    }
    // ODBC expresses KeepAlive/KeepAliveInterval in seconds; mssql-tds stores
    // milliseconds. Saturate so an out-of-range value can't overflow.
    if let Some(secs) = params.keep_alive {
        context.keep_alive_in_ms = secs.saturating_mul(1000);
    }
    if let Some(secs) = params.keep_alive_interval {
        context.keep_alive_interval_in_ms = secs.saturating_mul(1000);
    }
    if let Some(pref) = &params.ip_address_preference {
        context.ipaddress_preference = if pref.eq_ignore_ascii_case("ipv6first") {
            IPAddressPreference::IPv6First
        } else if pref.eq_ignore_ascii_case("useplatformdefault") {
            IPAddressPreference::UsePlatformDefault
        } else {
            IPAddressPreference::IPv4First
        };
    }
    // Parsed leniently as u32; mssql-tds owns range clamping (valid 512–32768).
    if let Some(size) = params.packet_size {
        context.packet_size = u16::try_from(size).unwrap_or(u16::MAX);
    }

    // Connect via mssql-tds (lock is NOT held - the 'Connecting' state prevents races)
    let provider = TdsConnectionProvider::new();
    let client = dbc
        .runtime
        .block_on(provider.create_client(context, &params.server, None));

    let mut client = match client {
        Ok(c) => c,
        Err(e) => {
            error!(%e, "SQLDriverConnectW: connection failed");
            post_tds_error(state, &e, SQLSTATE_08001);
            return SQL_ERROR;
        }
    };
    let info_messages = client.take_info_messages();

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

    let has_server_info = post_tds_info_messages(state, &info_messages);

    state.client = Some(client);
    state.connection_state = ConnectionState::Connected;
    debug!("SQLDriverConnectW: connected successfully");

    if has_warnings || truncated || has_server_info {
        if has_warnings {
            post_diag(state, ERR_INVALID_CONNECTION_STRING_ATTRIBUTE);
        }
        if truncated {
            post_diag(state, ERR_STRING_RIGHT_TRUNCATION);
        }
        SQL_SUCCESS_WITH_INFO
    } else {
        SQL_SUCCESS
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::get_diag::sql_get_diag_rec_w;
    use crate::api::odbc_types::{
        SQL_DRIVER_COMPLETE, SQL_HANDLE_DBC, SQL_INVALID_HANDLE, SQL_NTS, SQL_NULL_HANDLE,
    };
    use crate::test_support::{TestHandles, cs};

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

    #[test]
    fn null_handle_returns_invalid_handle() {
        let conn_str: Vec<u16> = cs("Server=host;UID=u;<PW>=p")
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
        let h = TestHandles::with_env_dbc();
        let dbc = h.dbc;
        let conn_str: Vec<u16> = cs("Server=host;UID=u;<PW>=p")
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
    }

    #[test]
    fn null_connection_string_returns_error() {
        let h = TestHandles::with_env_dbc();
        let dbc = h.dbc;

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
    }

    #[test]
    fn interactive_method_not_implemented_returns_hyc00() {
        // ActiveDirectoryInteractive is recognized but not yet implemented (T3);
        // the gate must reject it with HYC00 before any network activity.
        // (T2 now implements ServicePrincipal and ManagedIdentity, so those no
        // longer hit this gate.)
        let h = TestHandles::with_env_dbc();
        let dbc = h.dbc;
        let conn_str: Vec<u16> = cs("Server=s;Authentication=ActiveDirectoryInteractive")
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
        assert_eq!(unsafe { diag_sqlstate(dbc, 1) }, "HYC00");
    }

    #[test]
    fn authentication_with_trusted_connection_conflicts() {
        // Authentication and Trusted_Connection are mutually exclusive; the
        // validator must reject the combination (HY024) before connecting.
        let h = TestHandles::with_env_dbc();
        let dbc = h.dbc;
        let conn_str: Vec<u16> = cs("Server=s;Authentication=SqlPassword;Trusted_Connection=yes")
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
        assert_eq!(unsafe { diag_sqlstate(dbc, 1) }, "HY024");
    }

    #[test]
    fn missing_server_returns_error() {
        let h = TestHandles::with_env_dbc();
        let dbc = h.dbc;
        let conn_str: Vec<u16> = cs("UID=u;<PW>=p")
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
    }

    #[test]
    fn explicit_string_length() {
        // Pass an explicit length instead of SQL_NTS — extra chars after length are ignored.
        let h = TestHandles::with_env_dbc();
        let dbc = h.dbc;
        // The first 11 chars are a Server-less connection string, so validation fails.
        // But we're testing that explicit length is respected (no null terminator needed).
        let conn_str: Vec<u16> = cs("UID=u;<PW>=pGARBAGE").encode_utf16().collect();

        let ret = unsafe {
            sql_driver_connect_w(
                dbc,
                std::ptr::null_mut(),
                conn_str.as_ptr(),
                11, // truncate before "GARBAGE"
                std::ptr::null_mut(),
                0,
                std::ptr::null_mut(),
                SQL_DRIVER_NOPROMPT,
            )
        };
        // Missing server → error, but proves explicit length was used
        assert_eq!(ret, SQL_ERROR);
        assert_eq!(unsafe { diag_sqlstate(dbc, 1) }, "08001");
    }

    #[test]
    fn all_driver_completion_modes_rejected_except_noprompt() {
        let h = TestHandles::with_env_dbc();
        let dbc = h.dbc;
        let conn_str: Vec<u16> = cs("Server=h;UID=u;<PW>=p")
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
    }
}
