// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Implementation of SQLDriverConnectW — connect using a connection string.

use tracing::{debug, error};

use crate::api::odbc_types::{
    SQL_DRIVER_NOPROMPT, SQL_EN_OFF, SQL_EN_ON, SQL_EN_STRICT, SQL_ERROR, SQL_INVALID_HANDLE,
    SQL_NTS, SQL_SUCCESS, SQL_SUCCESS_WITH_INFO, SqlHWnd, SqlHandle, SqlReturn, SqlSmallInt,
    SqlUSmallInt, SqlWChar,
};
use crate::api::sqlstate::{
    ERR_FUNCTION_SEQUENCE, ERR_INVALID_CONNECTION_STRING_ATTRIBUTE, ERR_INVALID_NULL_POINTER,
    SQLSTATE_08001, SQLSTATE_HY024, SQLSTATE_HY110, SQLSTATE_HYC00, WARN_STRING_TRUNCATION,
    post_diag, post_tds_error, post_tds_info_messages,
};
use crate::api::txn::apply_post_connect_txn_settings;
use crate::api::util::{copy_with_nul, write_if_some};
use crate::error::{free_errors, post_sql_error};
use crate::handles::DbcHandle;
use crate::handles::dbc::{ConnectionState, DbcState, VendorConnOverrides};
use crate::handles::{HandleType, handle_from_raw};

use mssql_tds::connection::client_context::{ClientContext, IPAddressPreference};
use mssql_tds::connection_provider::tds_connection_provider::TdsConnectionProvider;
use mssql_tds::core::{EncryptionOptions, EncryptionSetting};
use mssql_tds::message::login_options::ApplicationIntent;
use std::path::PathBuf;

use super::util::read_utf16;
use crate::auth::{UnsupportedAuth, configure_auth};
use crate::connection::odbc_authentication_transformer::transform_auth;
use crate::connection::odbc_authentication_validator::validate_auth;
use crate::connection::{ConnectionParams, parse_connection_string};

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

pub(crate) fn sql_driver_connect_w_safe(
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
        return result;
    }

    // Autocommit and isolation level set before connecting had no session to
    // apply to; push them now. Needs the DBC lock and network I/O, so it runs
    // only once the connect path has released the lock. A failure downgrades the
    // connect to SQL_SUCCESS_WITH_INFO rather than failing it: the session is
    // usable, but the application must be able to see that its requested
    // settings did not take effect.
    drop(state);
    if apply_post_connect_txn_settings(dbc) == SQL_SUCCESS_WITH_INFO {
        return SQL_SUCCESS_WITH_INFO;
    }

    result
}

/// Lets the `SQL_COPT_SS_*` attribute forms win over the connection-string
/// keywords they duplicate.
///
/// Only a caller-set attribute (`Some`) displaces anything, so a connection
/// string on its own still behaves exactly as it did before.
fn apply_vendor_overrides(params: &mut ConnectionParams, overrides: &VendorConnOverrides) {
    if let Some(encrypt) = overrides.encrypt {
        params.encrypt = Some(
            match u64::from(encrypt) {
                SQL_EN_OFF => "no",
                SQL_EN_STRICT => "strict",
                _ => "yes",
            }
            .to_string(),
        );
    }
    if let Some(trust) = overrides.trust_server_certificate {
        params.trust_server_certificate = trust != 0;
    }
    if let Some(integrated) = overrides.integrated_security {
        params.trusted_connection = Some(integrated != 0);
    }
}

/// Resolves the `Encrypt=` keyword vocabulary onto an mssql-tds setting.
///
/// `ENCRYPT_VALUES` accepts five spellings; `mandatory`/`optional` are the
/// aliases of `yes`/`no`. An unspecified keyword means encryption on, which is
/// the ODBC Driver 18 default.
fn encryption_setting(encrypt: Option<&str>) -> EncryptionSetting {
    match encrypt {
        Some(e) if e.eq_ignore_ascii_case("no") || e.eq_ignore_ascii_case("optional") => {
            EncryptionSetting::PreferOff
        }
        Some(e) if e.eq_ignore_ascii_case("strict") => EncryptionSetting::Strict,
        _ => EncryptionSetting::On,
    }
}

/// The settings a post-connect get should report, taken from the connection as
/// it actually negotiated.
///
/// Measured against msodbcsql: a get returns the effective value regardless of
/// which path produced it. `Encrypt=no` reads back `0` when the server permits
/// plaintext and `1` when the server forces TLS, so neither the caller's raw
/// input nor a fixed default would match.
fn effective_vendor_settings(
    params: &ConnectionParams,
    connection_is_encrypted: bool,
) -> VendorConnOverrides {
    let mode = encryption_setting(params.encrypt.as_deref());
    let encrypt = match (mode, connection_is_encrypted) {
        (EncryptionSetting::Strict, _) => SQL_EN_STRICT,
        (_, true) => SQL_EN_ON,
        (_, false) => SQL_EN_OFF,
    };
    let trust = connection_is_encrypted && params.trust_server_certificate;
    VendorConnOverrides {
        encrypt: Some(encrypt as u32),
        trust_server_certificate: Some(u32::from(trust)),
        integrated_security: Some(u32::from(params.trusted_connection.unwrap_or(false))),
    }
}

fn initial_database(database_keyword: &str, current_catalog: Option<&str>) -> String {
    if database_keyword.is_empty() {
        current_catalog.unwrap_or_default().to_string()
    } else {
        database_keyword.to_string()
    }
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

    // Pre-connect vendor attributes override the matching keyword, which is the
    // reverse of the `Database=` / `SQL_ATTR_CURRENT_CATALOG` ranking applied
    // below. Both directions were measured, not assumed.
    let mut params = params;
    apply_vendor_overrides(&mut params, &state.vendor_overrides);
    let params = params;

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
    // pre-acquired access tokens; T2 added Entra service principal (secret) and
    // managed identity; T3 adds interactive sign-in (Windows only, matching
    // msodbcsql) — all via a token factory. Methods that still need token
    // acquisition (AD password, device code, workload identity, default
    // credential, AD integrated) are rejected with HYC00 until a later tier.
    // Off Windows an interactive request is reported as AD integrated, the same
    // method msodbcsql falls through to there.
    let mut context = ClientContext::default();
    // The connection string wins over a pre-connect
    // `SQLSetConnectAttr(SQL_ATTR_CURRENT_CATALOG)`: msodbcsql overwrites the
    // attribute's `conninfo.DataBase` while parsing the keywords, so a caller
    // supplying both logs in to the `Database=` one.
    context.database = initial_database(&params.database, state.current_catalog.as_deref());

    // Apply an app-set SQL_ATTR_LOGIN_TIMEOUT before configuring auth so an
    // explicit login timeout takes precedence over any method-specific default
    // (e.g. the larger default interactive sign-in installs).
    if let Some(secs) = state.login_timeout {
        context.login_timeout = Some(secs);
    }

    if let Err(unsupported) = configure_auth(&mut context, resolved, &params.server) {
        let UnsupportedAuth {
            requested,
            resolved,
        } = &unsupported;
        error!(
            ?requested,
            ?resolved,
            "SQLDriverConnectW: authentication method not implemented"
        );
        // Name the keyword the application actually supplied. Where the
        // platform maps it to another method, say so rather than reporting a
        // method the connection string never mentioned.
        let message = if requested == resolved {
            format!("Authentication method {requested:?} is not yet supported")
        } else {
            format!(
                "Authentication method {requested:?} resolves to {resolved:?} on this platform, \
                 which is not yet supported"
            )
        };
        post_sql_error(state, SQLSTATE_HYC00, 0, message);
        return SQL_ERROR;
    }

    context.encryption_options = EncryptionOptions {
        trust_server_certificate: params.trust_server_certificate,
        mode: encryption_setting(params.encrypt.as_deref()),
        host_name_in_cert: None,
        server_certificate: None,
    };

    apply_connection_params(&mut context, &params);

    // Connect via mssql-tds. The caller's DBC lock is still held across this
    // I/O, so other entry points block here rather than observing 'Connecting'.
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

    // Publish resolved values only after the connection succeeds. Explicit
    // attribute overrides remain separate so a reusable DBC does not feed a
    // previous connection string back into its next connection attempt.
    state.effective_vendor_settings =
        Some(effective_vendor_settings(&params, client.is_encrypted()));
    state.client = Some(client);
    state.connection_state = ConnectionState::Connected;
    debug!("SQLDriverConnectW: connected successfully");

    if has_warnings || truncated || has_server_info {
        if has_warnings {
            post_diag(state, ERR_INVALID_CONNECTION_STRING_ATTRIBUTE);
        }
        if truncated {
            post_diag(state, WARN_STRING_TRUNCATION);
        }
        SQL_SUCCESS_WITH_INFO
    } else {
        SQL_SUCCESS
    }
}

/// TDS packet-size range accepted by `mssql-tds` (`DefaultClientContextValidator`).
/// Unlike `ConnectRetryCount` / `ConnectRetryInterval` (which the parser rejects
/// out-of-range to match msodbcsql), `PacketSize` is clamped to this range.
const MIN_PACKET_SIZE: u32 = 512;
const MAX_PACKET_SIZE: u32 = 32768;

/// Maps parsed [`ConnectionParams`] onto a [`ClientContext`]. `ConnectRetryCount`
/// and `ConnectRetryInterval` are already range-validated during parsing;
/// `PacketSize` is clamped here to the range `mssql-tds` accepts. Enum strings are
/// mapped to their variant with a default fallback — validated during parsing,
/// except `IpAddressPreference`, whose unknown values fall back to `IPv4First`
/// (matching msodbcsql). Kept separate from `do_connect` so the mapping is
/// unit-testable without a live server.
fn apply_connection_params(context: &mut ClientContext, params: &ConnectionParams) {
    context.encryption_options.host_name_in_cert = params.host_name_in_certificate.clone();
    context.encryption_options.server_certificate =
        params.server_certificate.as_deref().map(PathBuf::from);

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
    // milliseconds. Saturate so a large value can't overflow.
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
    if let Some(size) = params.packet_size {
        context.packet_size =
            u16::try_from(size.clamp(MIN_PACKET_SIZE, MAX_PACKET_SIZE)).unwrap_or(u16::MAX);
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

    #[test]
    fn initial_database_uses_keyword_then_attribute_then_login_default() {
        assert_eq!(
            initial_database("keyword_db", Some("attribute_db")),
            "keyword_db"
        );
        assert_eq!(initial_database("", Some("attribute_db")), "attribute_db");
        assert_eq!(initial_database("", Some("")), "");
        assert_eq!(initial_database("", None), "");
    }

    /// The value a get reports must match the encryption the connection
    /// actually uses. These are two separate mappings over the same keyword
    /// vocabulary, so pin them together rather than trusting them to stay in
    /// step.
    #[test]
    fn reported_encrypt_matches_the_negotiated_setting() {
        for (keyword, setting, connection_is_encrypted, code) in [
            (Some("yes"), EncryptionSetting::On, true, 1u32),
            (Some("mandatory"), EncryptionSetting::On, true, 1),
            (Some("no"), EncryptionSetting::PreferOff, false, 0),
            (Some("no"), EncryptionSetting::PreferOff, true, 1),
            (Some("optional"), EncryptionSetting::PreferOff, false, 0),
            (Some("strict"), EncryptionSetting::Strict, true, 2),
            (Some("STRICT"), EncryptionSetting::Strict, true, 2),
            (None, EncryptionSetting::On, true, 1),
        ] {
            assert_eq!(encryption_setting(keyword), setting, "keyword {keyword:?}");

            let (mut params, _) = parse_connection_string(&cs("Server=h;UID=u;<PW>=p")).unwrap();
            params.encrypt = keyword.map(str::to_string);
            assert_eq!(
                effective_vendor_settings(&params, connection_is_encrypted).encrypt,
                Some(code),
                "keyword {keyword:?}"
            );
        }
    }

    #[test]
    fn vendor_overrides_displace_the_matching_keyword() {
        let base = cs("Server=h;UID=u;<PW>=p;Encrypt=no;TrustServerCertificate=yes");
        let (params, _) = parse_connection_string(&base).unwrap();

        // Nothing set: the connection string is left exactly as parsed.
        let mut untouched = params.clone();
        apply_vendor_overrides(&mut untouched, &VendorConnOverrides::default());
        assert_eq!(untouched.encrypt, params.encrypt);
        assert_eq!(
            untouched.trust_server_certificate,
            params.trust_server_certificate
        );
        assert_eq!(untouched.trusted_connection, params.trusted_connection);

        // Each attribute wins over the keyword it duplicates.
        let mut overridden = params.clone();
        apply_vendor_overrides(
            &mut overridden,
            &VendorConnOverrides {
                encrypt: Some(2),
                trust_server_certificate: Some(0),
                integrated_security: Some(1),
            },
        );
        assert_eq!(overridden.encrypt.as_deref(), Some("strict"));
        assert!(!overridden.trust_server_certificate);
        assert_eq!(overridden.trusted_connection, Some(true));
    }

    #[test]
    fn effective_settings_report_the_resolved_connection() {
        // A get reads back what the connection resolved to, from whichever path
        // set it -- measured: `Encrypt=no` with no attribute reads back 0.
        let (params, _) =
            parse_connection_string(&cs("Server=h;Trusted_Connection=yes;Encrypt=no")).unwrap();
        assert_eq!(
            effective_vendor_settings(&params, false),
            VendorConnOverrides {
                encrypt: Some(0),
                trust_server_certificate: Some(0),
                integrated_security: Some(1),
            }
        );
    }

    /// The trust flag reports the effective certificate policy, not the
    /// keyword. Encryption off means there is no certificate in play, and
    /// msodbcsql reports 0 for it even when `TrustServerCertificate=Yes` was
    /// asked for -- found by running the e2e parity variation against the
    /// vendor driver.
    #[test]
    fn trust_is_only_reported_when_the_connection_is_encrypted() {
        for (keywords, connection_is_encrypted, expected) in [
            ("Encrypt=yes;TrustServerCertificate=yes", true, 1),
            ("Encrypt=yes;TrustServerCertificate=no", true, 0),
            ("Encrypt=no;TrustServerCertificate=yes", false, 0),
            ("Encrypt=no;TrustServerCertificate=no", false, 0),
            ("Encrypt=no;TrustServerCertificate=yes", true, 1),
            ("Encrypt=no;TrustServerCertificate=no", true, 0),
        ] {
            let (params, _) =
                parse_connection_string(&cs(&format!("Server=h;UID=u;<PW>=p;{keywords}"))).unwrap();
            assert_eq!(
                effective_vendor_settings(&params, connection_is_encrypted)
                    .trust_server_certificate,
                Some(expected),
                "{keywords}, encrypted={connection_is_encrypted}"
            );
        }
    }

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
    fn device_code_method_not_implemented_returns_hyc00() {
        // ActiveDirectoryDeviceCodeFlow is recognized but not yet implemented;
        // the gate must reject it with HYC00 before any network activity. (T2
        // implements ServicePrincipal and ManagedIdentity and T3 implements
        // Interactive, so those no longer hit this gate.)
        let h = TestHandles::with_env_dbc();
        let dbc = h.dbc;
        let conn_str: Vec<u16> = cs("Server=s;Authentication=ActiveDirectoryDeviceCodeFlow")
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
    fn failed_connects_do_not_promote_keywords_to_reusable_attribute_overrides() {
        let h = TestHandles::with_env_dbc();
        for encrypt in ["no", "yes"] {
            let conn_str: Vec<u16> = cs(&format!("UID=u;<PW>=p;Encrypt={encrypt}"))
                .encode_utf16()
                .chain(std::iter::once(0))
                .collect();
            let ret = unsafe {
                sql_driver_connect_w(
                    h.dbc,
                    std::ptr::null_mut(),
                    conn_str.as_ptr(),
                    SQL_NTS,
                    std::ptr::null_mut(),
                    0,
                    std::ptr::null_mut(),
                    SQL_DRIVER_NOPROMPT,
                )
            };
            assert_eq!(ret, SQL_ERROR, "attempt with Encrypt={encrypt}");

            let dbc = unsafe { handle_from_raw::<DbcHandle>(h.dbc) };
            let state = dbc.inner.lock().unwrap();
            assert_eq!(state.connection_state, ConnectionState::Disconnected);
            assert_eq!(state.vendor_overrides, VendorConnOverrides::default());
            assert_eq!(state.effective_vendor_settings, None);
        }
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

    #[test]
    fn apply_params_maps_tls_identity_fields() {
        let mut ctx = ClientContext::default();
        let params = ConnectionParams {
            host_name_in_certificate: Some("cn.contoso.com".to_string()),
            server_certificate: Some("/etc/ssl/server.pem".to_string()),
            ..Default::default()
        };
        apply_connection_params(&mut ctx, &params);
        assert_eq!(
            ctx.encryption_options.host_name_in_cert.as_deref(),
            Some("cn.contoso.com")
        );
        assert_eq!(
            ctx.encryption_options.server_certificate,
            Some(PathBuf::from("/etc/ssl/server.pem"))
        );
    }

    #[test]
    fn apply_params_passes_through_connect_retry_values() {
        // The parser range-validates these, so the mapping stores them verbatim.
        let mut ctx = ClientContext::default();
        apply_connection_params(
            &mut ctx,
            &ConnectionParams {
                connect_retry_count: Some(255),
                connect_retry_interval: Some(60),
                ..Default::default()
            },
        );
        assert_eq!(ctx.connect_retry_count, 255);
        assert_eq!(ctx.connect_retry_interval, 60);
    }

    #[test]
    fn apply_params_falls_back_unknown_ip_preference_to_ipv4first() {
        let mut ctx = ClientContext::default();
        apply_connection_params(
            &mut ctx,
            &ConnectionParams {
                ip_address_preference: Some("IPv7".to_string()),
                ..Default::default()
            },
        );
        assert!(matches!(
            ctx.ipaddress_preference,
            IPAddressPreference::IPv4First
        ));
    }

    #[test]
    fn apply_params_clamps_packet_size_to_tds_range() {
        let mut ctx = ClientContext::default();
        apply_connection_params(
            &mut ctx,
            &ConnectionParams {
                packet_size: Some(100),
                ..Default::default()
            },
        );
        assert_eq!(ctx.packet_size, 512);
        apply_connection_params(
            &mut ctx,
            &ConnectionParams {
                packet_size: Some(70_000),
                ..Default::default()
            },
        );
        assert_eq!(ctx.packet_size, 32768);
    }

    #[test]
    fn apply_params_maps_keepalive_seconds_to_millis() {
        let mut ctx = ClientContext::default();
        apply_connection_params(
            &mut ctx,
            &ConnectionParams {
                keep_alive: Some(30),
                keep_alive_interval: Some(5),
                ..Default::default()
            },
        );
        assert_eq!(ctx.keep_alive_in_ms, 30_000);
        assert_eq!(ctx.keep_alive_interval_in_ms, 5_000);
    }

    #[test]
    fn apply_params_maps_validated_enums() {
        let mut ctx = ClientContext::default();
        apply_connection_params(
            &mut ctx,
            &ConnectionParams {
                application_intent: Some("ReadOnly".to_string()),
                ip_address_preference: Some("IPv6First".to_string()),
                multi_subnet_failover: Some(true),
                server_spn: Some("MSSQLSvc/host:1433".to_string()),
                ..Default::default()
            },
        );
        assert!(matches!(
            ctx.application_intent,
            ApplicationIntent::ReadOnly
        ));
        assert!(matches!(
            ctx.ipaddress_preference,
            IPAddressPreference::IPv6First
        ));
        assert!(ctx.multi_subnet_failover);
        assert_eq!(ctx.server_spn.as_deref(), Some("MSSQLSvc/host:1433"));
    }

    #[test]
    fn apply_params_leaves_unset_fields_at_defaults() {
        let mut ctx = ClientContext::default();
        let before_packet = ctx.packet_size;
        let before_retry = ctx.connect_retry_count;
        apply_connection_params(&mut ctx, &ConnectionParams::default());
        assert_eq!(ctx.packet_size, before_packet);
        assert_eq!(ctx.connect_retry_count, before_retry);
        assert_eq!(ctx.encryption_options.host_name_in_cert, None);
        assert_eq!(ctx.encryption_options.server_certificate, None);
    }
}
