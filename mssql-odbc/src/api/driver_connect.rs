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
use crate::handles::dbc::{ConnectionIdentity, ConnectionState, DbcState, VendorConnOverrides};
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

/// # Safety
/// `connection_handle` must be null or point to a live `DbcHandle`.
/// `in_connection_string`, when non-null, must be readable for
/// `string_length_1` UTF-16 code units, or through a NUL terminator when the
/// length is `SQL_NTS`. `out_connection_string`, when non-null, must be writable
/// for `buffer_length` UTF-16 code units, and `string_length_2_ptr`, when
/// non-null, must be writable for one `SqlSmallInt`.
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

    seed_and_apply_connection_params(&mut context, state.packet_size, &params);
    // Capture the fully-resolved pre-negotiation size (attr seed, then any
    // `PacketSize=` override) before `context` is moved into
    // `create_client` below; only published to `state.effective_packet_size`
    // once the connection actually succeeds (see the success path), so a
    // failed attempt does not leave a reusable DBC requesting a size it never
    // actually established.
    let resolved_packet_size = u32::from(context.packet_size);

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
    state.identity = ConnectionIdentity {
        data_source_name: params.dsn.clone(),
        // The server names itself in the INFO tokens it sends at login; the host
        // the caller dialled is only a fallback for a server that sent none.
        server_name: client
            .server_reported_name()
            .filter(|name| !name.is_empty())
            .unwrap_or(params.server.as_str())
            .to_string(),
        user_name: params.uid.clone(),
    };
    // Published here (not right after resolving it above) for the same
    // failed-connect reason as the other fields in this block: kept separate
    // from `state.packet_size` (the app-set attribute/default) so a
    // connection-string keyword never outlives this connection and leaks
    // onto the handle's next attempt — cleared again in `sql_disconnect_safe`.
    // Never the ENVCHANGE-negotiated value: msodbcsql's own
    // `SQLGetConnectAttr`/`SQLGetInfo` both read the single `dwOptions`
    // slot that only ever holds the requested size (`sqlcconn.cpp:3326`
    // builds LOGIN7 from it, `sqlcmisc.cpp:3465`/`sqlcinfo.cpp:1186` read it
    // back) — nothing in msodbcsql writes the negotiated size into that
    // slot; the negotiated value only resizes msodbcsql's own TDS buffer
    // (`TdsHlp.cpp: BATCHCTX::NewPacketSize`), a separate internal detail.
    state.effective_packet_size = Some(resolved_packet_size);
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
///
/// Also reused by `set_connect_attr::SQL_ATTR_PACKET_SIZE` to clamp
/// `DbcState::packet_size` at the point it is set, so no unclamped value can
/// reach `get_info::max_statement_len`'s `128 * packet_size` before connect.
pub(super) const MIN_PACKET_SIZE: u32 = 512;
pub(super) const MAX_PACKET_SIZE: u32 = 32768;

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
    if let Some(application_name) = &params.application_name {
        context.application_name = application_name.clone();
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
    // A `PacketSize=0` keyword is msodbcsql's same "unspecified" sentinel as
    // `SQL_ATTR_PACKET_SIZE, 0` (`sqlcconn.cpp` stores the keyword's parsed
    // value into the same `dwOptions[SQL_PACKET_SIZE]` slot verbatim, with no
    // clamp of its own), so it is exempted here too and `context.packet_size`
    // keeps its `ClientContext::default()` value instead of being forced to
    // `MIN_PACKET_SIZE` — seeding a literal `0` would fail `ClientContext`'s
    // `[MIN_PACKET_SIZE, MAX_PACKET_SIZE]` validation regardless.
    if let Some(size) = params.packet_size
        && size != 0
    {
        context.packet_size =
            u16::try_from(size.clamp(MIN_PACKET_SIZE, MAX_PACKET_SIZE)).unwrap_or(u16::MAX);
    }
}

/// Seeds `context.packet_size` from any pre-connect
/// `SQLSetConnectAttr(SQL_ATTR_PACKET_SIZE)` (msodbcsql applies the attribute
/// to the connect request too) before applying the rest of the connection
/// params, so a `PacketSize=` connection-string keyword still overrides it.
/// Zero is msodbcsql's sentinel for "unspecified" (`sqlcmisc.cpp:1909-1917`
/// exempts it from the packet-size clamp), so it is left out of the seed
/// entirely and `context.packet_size` keeps its `ClientContext::default()`
/// value instead — seeding zero directly would fail `ClientContext`'s
/// `[MIN_PACKET_SIZE, MAX_PACKET_SIZE]` validation. Any other
/// `state_packet_size` is always clamped to `[MIN_PACKET_SIZE,
/// MAX_PACKET_SIZE]` (both well within u16), so that cast never truncates.
/// Extracted out of `do_connect` so a test can drive it directly rather than
/// re-typing its two statements, which would silently stop guarding the real
/// code path the moment the two drifted apart.
fn seed_and_apply_connection_params(
    context: &mut ClientContext,
    state_packet_size: u32,
    params: &ConnectionParams,
) {
    if state_packet_size != 0 {
        context.packet_size = state_packet_size as u16;
    }
    apply_connection_params(context, params);
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
    ///
    /// # Safety
    /// `dbc` must point to a live `DbcHandle`.
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
    fn apply_params_leaves_packet_size_zero_keyword_unseeded() {
        // `PacketSize=0` is msodbcsql's same "unspecified" sentinel as
        // `SQL_ATTR_PACKET_SIZE, 0` — both write the same dwOptions slot
        // verbatim with no clamp — so it must not be forced up to
        // `MIN_PACKET_SIZE` any more than the attribute path is.
        let default_packet_size = ClientContext::default().packet_size;
        let mut ctx = ClientContext::default();
        ctx.packet_size = 4096; // simulate a prior nonzero seed being left alone
        apply_connection_params(
            &mut ctx,
            &ConnectionParams {
                packet_size: Some(0),
                ..Default::default()
            },
        );
        assert_eq!(ctx.packet_size, 4096, "a zero keyword must not overwrite");

        let mut fresh_ctx = ClientContext::default();
        apply_connection_params(
            &mut fresh_ctx,
            &ConnectionParams {
                packet_size: Some(0),
                ..Default::default()
            },
        );
        assert_eq!(fresh_ctx.packet_size, default_packet_size);
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
    fn apply_params_maps_app_to_application_name() {
        let mut ctx = ClientContext::default();
        apply_connection_params(
            &mut ctx,
            &ConnectionParams {
                application_name: Some("MSSQL-Python".to_string()),
                ..Default::default()
            },
        );
        assert_eq!(ctx.application_name, "MSSQL-Python");
    }

    #[test]
    fn apply_params_leaves_unset_fields_at_defaults() {
        let mut ctx = ClientContext::default();
        let before_packet = ctx.packet_size;
        let before_retry = ctx.connect_retry_count;
        let before_app_name = ctx.application_name.clone();
        apply_connection_params(&mut ctx, &ConnectionParams::default());
        assert_eq!(ctx.packet_size, before_packet);
        assert_eq!(ctx.connect_retry_count, before_retry);
        assert_eq!(ctx.application_name, before_app_name);
        assert_eq!(ctx.encryption_options.host_name_in_cert, None);
        assert_eq!(ctx.encryption_options.server_certificate, None);
    }

    /// Drives `do_connect`'s actual `seed_and_apply_connection_params` helper
    /// (not a re-typed copy of it): a pre-connect
    /// `SQLSetConnectAttr(SQL_ATTR_PACKET_SIZE)` must reach the login
    /// request when the connection string is silent on `PacketSize=`, but a
    /// `PacketSize=` keyword must still win when both are present. Deleting
    /// the seeding line inside the helper fails this test.
    #[test]
    fn preconnect_packet_size_attr_seeds_context_but_connection_string_keyword_wins() {
        let mut ctx = ClientContext::default();
        seed_and_apply_connection_params(&mut ctx, 16384, &ConnectionParams::default());
        assert_eq!(
            ctx.packet_size, 16384,
            "a pre-connect SQL_ATTR_PACKET_SIZE must reach the login request"
        );

        let mut ctx = ClientContext::default();
        seed_and_apply_connection_params(
            &mut ctx,
            16384,
            &ConnectionParams {
                packet_size: Some(4096),
                ..Default::default()
            },
        );
        assert_eq!(
            ctx.packet_size, 4096,
            "PacketSize= in the connection string must override the pre-connect attribute"
        );
    }

    /// `SQLSetConnectAttr(SQL_ATTR_PACKET_SIZE, 0)` stores the msodbcsql
    /// "unspecified" sentinel (`set_connect_attr.rs`), which this helper must
    /// not seed verbatim: `ClientContext` rejects a zero packet size
    /// (`client_context.rs`'s `[MIN_PACKET_SIZE, MAX_PACKET_SIZE]`
    /// validation), so seeding it would turn every connect attempt on a
    /// zeroed handle into a hard failure instead of falling back to the
    /// context's own default.
    #[test]
    fn zero_state_packet_size_leaves_the_context_default_unseeded() {
        let default_packet_size = ClientContext::default().packet_size;

        let mut ctx = ClientContext::default();
        seed_and_apply_connection_params(&mut ctx, 0, &ConnectionParams::default());
        assert_eq!(
            ctx.packet_size, default_packet_size,
            "a zero SQL_ATTR_PACKET_SIZE must not override the context default"
        );

        // A `PacketSize=` keyword must still be honored even though the
        // attribute itself is the zero sentinel.
        let mut ctx = ClientContext::default();
        seed_and_apply_connection_params(
            &mut ctx,
            0,
            &ConnectionParams {
                packet_size: Some(4096),
                ..Default::default()
            },
        );
        assert_eq!(
            ctx.packet_size, 4096,
            "PacketSize= must still apply when the pre-connect attribute is the zero sentinel"
        );
    }

    /// `SQLGetConnectAttr(SQL_ATTR_PACKET_SIZE)` and `SQLGetInfo`'s
    /// `128 * packet_size` limits must both keep reporting the *requested*
    /// packet size after connecting, even though the server negotiates a
    /// different one, matching msodbcsql: its `SQLGetConnectAttr`/
    /// `SQLGetInfo` read the single `dwOptions` slot the LOGIN7 request was
    /// built from, and nothing writes the ENVCHANGE-negotiated size back
    /// into it. This mock server always negotiates down to 4096 regardless
    /// of what is requested (`mssql-mock-tds/src/protocol.rs`), so
    /// requesting a different size reliably exercises this.
    #[test]
    fn connect_keeps_reporting_the_requested_packet_size_not_the_negotiated_one() {
        use crate::api::get_connect_attr::sql_get_connect_attr_w;
        use crate::api::get_info::sql_get_info_w;
        use crate::api::odbc_types::{
            SQL_ATTR_PACKET_SIZE, SQL_MAX_CHAR_LITERAL_LEN, SqlInteger, SqlPointer, SqlSmallInt,
        };
        use mssql_mock_tds::MockTdsServer;
        use std::time::Duration;

        let server_runtime =
            tokio::runtime::Runtime::new().expect("failed to build mock-server runtime");
        let (server_addr, shutdown_tx, server_handle) = server_runtime.block_on(async {
            let server = MockTdsServer::new("127.0.0.1:0")
                .await
                .expect("failed to start mock server");
            let addr = server.local_addr();
            let (tx, rx) = tokio::sync::oneshot::channel();
            let handle = tokio::spawn(async move {
                let _ = server.run_with_shutdown(rx).await;
            });
            tokio::time::sleep(Duration::from_millis(100)).await;
            (addr, tx, handle)
        });

        let h = TestHandles::with_env_dbc();
        let requested_packet_size: u32 = 16384;
        let conn_str: Vec<u16> = cs(&format!(
            "Server=tcp:{},{};UID=sa;<PW>=unused;Database=master;Encrypt=no;\
             TrustServerCertificate=yes;PacketSize={requested_packet_size}",
            server_addr.ip(),
            server_addr.port()
        ))
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
        assert!(
            matches!(ret, SQL_SUCCESS | SQL_SUCCESS_WITH_INFO),
            "connect failed: {ret}"
        );

        let dbc = unsafe { handle_from_raw::<DbcHandle>(h.dbc) };
        let negotiated = dbc
            .inner
            .lock()
            .unwrap()
            .client
            .as_ref()
            .expect("connected")
            .packet_size();
        assert_eq!(
            negotiated, 4096,
            "mock server is expected to always negotiate down to 4096"
        );

        let mut reported: u32 = 0;
        let get_ret = unsafe {
            sql_get_connect_attr_w(
                h.dbc,
                SQL_ATTR_PACKET_SIZE,
                &mut reported as *mut u32 as SqlPointer,
                std::mem::size_of::<u32>() as SqlInteger,
                std::ptr::null_mut(),
            )
        };
        assert_eq!(get_ret, SQL_SUCCESS);
        assert_eq!(
            reported, requested_packet_size,
            "SQLGetConnectAttr must keep reporting the requested size, not the negotiated one"
        );

        let mut max_char_literal_len: u32 = 0;
        let info_ret = unsafe {
            sql_get_info_w(
                h.dbc,
                SQL_MAX_CHAR_LITERAL_LEN,
                &mut max_char_literal_len as *mut u32 as SqlPointer,
                std::mem::size_of::<u32>() as SqlSmallInt,
                std::ptr::null_mut(),
            )
        };
        assert_eq!(info_ret, SQL_SUCCESS);
        assert_eq!(
            max_char_literal_len,
            128 * requested_packet_size,
            "SQLGetInfo must derive the limit from the requested size too, agreeing with \
             SQLGetConnectAttr"
        );

        let _ = shutdown_tx.send(());
        let _ = server_runtime
            .block_on(async { tokio::time::timeout(Duration::from_secs(2), server_handle).await });
    }

    /// `SQLDisconnect` must reset `SQL_ATTR_PACKET_SIZE` (and the derived
    /// `SQLGetInfo` limits) back to the handle's pre-connect value: a
    /// connection-string `PacketSize=` must not outlive the connection it
    /// came from and leak onto the handle's next connect attempt.
    #[test]
    fn packet_size_resets_on_disconnect_to_pre_connect_value() {
        use crate::api::disconnect::sql_disconnect;
        use crate::api::get_connect_attr::sql_get_connect_attr_w;
        use crate::api::odbc_types::{
            DEFAULT_PACKET_SIZE, SQL_ATTR_PACKET_SIZE, SqlInteger, SqlPointer,
        };
        use mssql_mock_tds::MockTdsServer;
        use std::time::Duration;

        let server_runtime =
            tokio::runtime::Runtime::new().expect("failed to build mock-server runtime");
        let (server_addr, shutdown_tx, server_handle) = server_runtime.block_on(async {
            let server = MockTdsServer::new("127.0.0.1:0")
                .await
                .expect("failed to start mock server");
            let addr = server.local_addr();
            let (tx, rx) = tokio::sync::oneshot::channel();
            let handle = tokio::spawn(async move {
                let _ = server.run_with_shutdown(rx).await;
            });
            tokio::time::sleep(Duration::from_millis(100)).await;
            (addr, tx, handle)
        });

        let h = TestHandles::with_env_dbc();
        let conn_str: Vec<u16> = cs(&format!(
            "Server=tcp:{},{};UID=sa;<PW>=unused;Database=master;Encrypt=no;\
             TrustServerCertificate=yes;PacketSize=16384",
            server_addr.ip(),
            server_addr.port()
        ))
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
        assert!(
            matches!(ret, SQL_SUCCESS | SQL_SUCCESS_WITH_INFO),
            "connect failed: {ret}"
        );

        let disconnect_ret = unsafe { sql_disconnect(h.dbc) };
        assert_eq!(disconnect_ret, SQL_SUCCESS);

        let mut reported: u32 = 0;
        let get_ret = unsafe {
            sql_get_connect_attr_w(
                h.dbc,
                SQL_ATTR_PACKET_SIZE,
                &mut reported as *mut u32 as SqlPointer,
                std::mem::size_of::<u32>() as SqlInteger,
                std::ptr::null_mut(),
            )
        };
        assert_eq!(get_ret, SQL_SUCCESS);
        assert_eq!(
            reported, DEFAULT_PACKET_SIZE,
            "a keyword-derived packet size must not survive SQLDisconnect"
        );

        let _ = shutdown_tx.send(());
        let _ = server_runtime
            .block_on(async { tokio::time::timeout(Duration::from_secs(2), server_handle).await });
    }

    /// A connect attempt that fails after resolving its packet size (but
    /// before mssql-tds returns a client) must not leave that attempted size
    /// behind on the DBC: a subsequent connect on the same handle should
    /// still request the handle's prior value, not the failed attempt's.
    #[test]
    fn failed_connect_does_not_persist_the_attempted_packet_size() {
        use crate::api::get_connect_attr::sql_get_connect_attr_w;
        use crate::api::odbc_types::{
            DEFAULT_PACKET_SIZE, SQL_ATTR_PACKET_SIZE, SqlInteger, SqlPointer,
        };

        let h = TestHandles::with_env_dbc();

        // Reserve a port, then drop the listener so nothing accepts on it —
        // `create_client` fails quickly with a real connection error, well
        // past connection-string validation and packet-size resolution.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("local_addr");
        drop(listener);

        let conn_str: Vec<u16> = cs(&format!(
            "Server=tcp:{},{};UID=sa;<PW>=unused;Database=master;Encrypt=no;\
             TrustServerCertificate=yes;PacketSize=16384;ConnectRetryCount=0",
            addr.ip(),
            addr.port()
        ))
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
        assert_eq!(ret, SQL_ERROR, "connect to an unused port must fail");

        let dbc = unsafe { handle_from_raw::<DbcHandle>(h.dbc) };
        assert_eq!(
            dbc.inner.lock().unwrap().connection_state,
            ConnectionState::Disconnected
        );

        let mut reported: u32 = 0;
        let get_ret = unsafe {
            sql_get_connect_attr_w(
                h.dbc,
                SQL_ATTR_PACKET_SIZE,
                &mut reported as *mut u32 as SqlPointer,
                std::mem::size_of::<u32>() as SqlInteger,
                std::ptr::null_mut(),
            )
        };
        assert_eq!(get_ret, SQL_SUCCESS);
        assert_eq!(
            reported, DEFAULT_PACKET_SIZE,
            "a failed connect must not leave the attempted PacketSize= behind"
        );
    }

    /// A pre-connect `SQLSetConnectAttr(SQL_ATTR_PACKET_SIZE, 0)` — the
    /// msodbcsql "unspecified" sentinel — must survive a failed connect
    /// attempt exactly like any other stored value: unlike `packet_size`
    /// itself, `effective_packet_size` is never populated on a failed
    /// attempt, so the handle must keep reporting the zero it was
    /// explicitly given, not silently drift to `DEFAULT_PACKET_SIZE`.
    #[test]
    fn zero_packet_size_survives_a_failed_connect() {
        use crate::api::get_connect_attr::sql_get_connect_attr_w;
        use crate::api::odbc_types::{SQL_ATTR_PACKET_SIZE, SqlInteger, SqlPointer};
        use crate::api::set_connect_attr::sql_set_connect_attr_w;

        let h = TestHandles::with_env_dbc();
        let set_ret =
            unsafe { sql_set_connect_attr_w(h.dbc, SQL_ATTR_PACKET_SIZE, 0usize as SqlPointer, 0) };
        assert_eq!(set_ret, SQL_SUCCESS);

        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("local_addr");
        drop(listener);

        let conn_str: Vec<u16> = cs(&format!(
            "Server=tcp:{},{};UID=sa;<PW>=unused;Database=master;Encrypt=no;\
             TrustServerCertificate=yes;ConnectRetryCount=0",
            addr.ip(),
            addr.port()
        ))
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
        assert_eq!(ret, SQL_ERROR, "connect to an unused port must fail");

        let mut reported: u32 = 0xAAAA_AAAA;
        let get_ret = unsafe {
            sql_get_connect_attr_w(
                h.dbc,
                SQL_ATTR_PACKET_SIZE,
                &mut reported as *mut u32 as SqlPointer,
                std::mem::size_of::<u32>() as SqlInteger,
                std::ptr::null_mut(),
            )
        };
        assert_eq!(get_ret, SQL_SUCCESS);
        assert_eq!(
            reported, 0,
            "a failed connect must not disturb the explicitly-set zero sentinel"
        );
    }

    /// The zero sentinel must round-trip through a full successful
    /// connect/disconnect/reconnect cycle: while connected,
    /// `SQL_ATTR_PACKET_SIZE` reports the resolved (non-zero) size actually
    /// used for the login, but after `SQLDisconnect` it must go back to
    /// reporting the zero the caller explicitly asked for — not
    /// `DEFAULT_PACKET_SIZE` — and a second connect must resolve the same
    /// way.
    #[test]
    fn zero_packet_size_survives_connect_disconnect_reconnect() {
        use crate::api::disconnect::sql_disconnect;
        use crate::api::get_connect_attr::sql_get_connect_attr_w;
        use crate::api::odbc_types::{SQL_ATTR_PACKET_SIZE, SqlInteger, SqlPointer};
        use crate::api::set_connect_attr::sql_set_connect_attr_w;
        use mssql_mock_tds::MockTdsServer;
        use std::time::Duration;

        let server_runtime =
            tokio::runtime::Runtime::new().expect("failed to build mock-server runtime");
        let (server_addr, shutdown_tx, server_handle) = server_runtime.block_on(async {
            let server = MockTdsServer::new("127.0.0.1:0")
                .await
                .expect("failed to start mock server");
            let addr = server.local_addr();
            let (tx, rx) = tokio::sync::oneshot::channel();
            let handle = tokio::spawn(async move {
                let _ = server.run_with_shutdown(rx).await;
            });
            tokio::time::sleep(Duration::from_millis(100)).await;
            (addr, tx, handle)
        });

        let h = TestHandles::with_env_dbc();
        let set_ret =
            unsafe { sql_set_connect_attr_w(h.dbc, SQL_ATTR_PACKET_SIZE, 0usize as SqlPointer, 0) };
        assert_eq!(set_ret, SQL_SUCCESS);

        let conn_str: Vec<u16> = cs(&format!(
            "Server=tcp:{},{};UID=sa;<PW>=unused;Database=master;Encrypt=no;\
             TrustServerCertificate=yes",
            server_addr.ip(),
            server_addr.port()
        ))
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();

        for _ in 0..2 {
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
            assert!(
                matches!(ret, SQL_SUCCESS | SQL_SUCCESS_WITH_INFO),
                "connect failed: {ret}"
            );

            let mut connected_reported: u32 = 0;
            let get_ret = unsafe {
                sql_get_connect_attr_w(
                    h.dbc,
                    SQL_ATTR_PACKET_SIZE,
                    &mut connected_reported as *mut u32 as SqlPointer,
                    std::mem::size_of::<u32>() as SqlInteger,
                    std::ptr::null_mut(),
                )
            };
            assert_eq!(get_ret, SQL_SUCCESS);
            assert_ne!(
                connected_reported, 0,
                "while connected the zero sentinel must resolve to an actual size"
            );

            let disconnect_ret = unsafe { sql_disconnect(h.dbc) };
            assert_eq!(disconnect_ret, SQL_SUCCESS);

            let mut reported: u32 = 0xAAAA_AAAA;
            let get_ret = unsafe {
                sql_get_connect_attr_w(
                    h.dbc,
                    SQL_ATTR_PACKET_SIZE,
                    &mut reported as *mut u32 as SqlPointer,
                    std::mem::size_of::<u32>() as SqlInteger,
                    std::ptr::null_mut(),
                )
            };
            assert_eq!(get_ret, SQL_SUCCESS);
            assert_eq!(
                reported, 0,
                "SQLDisconnect must restore the explicitly-set zero sentinel, not \
                 DEFAULT_PACKET_SIZE"
            );
        }

        let _ = shutdown_tx.send(());
        let _ = server_runtime
            .block_on(async { tokio::time::timeout(Duration::from_secs(2), server_handle).await });
    }
}
