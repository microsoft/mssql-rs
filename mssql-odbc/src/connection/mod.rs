// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Connection string parsing for ODBC `SQLDriverConnect`.
//!
//! A single-pass, character-by-character state machine that mirrors the behavior
//! of the msodbcsql driver's `ParseAttrStr`
//! (`Sql/Ntdbms/sqlncli/odbc/sqlcconn.cpp`). The intent is byte-for-byte parity
//! with the shipping ODBC driver, including its quirks. See
//! [`docs/connection_string_parser.md`](../../docs/connection_string_parser.md).
//!
//! Key behaviors reproduced from msodbcsql:
//! - The key is scanned until `=` and **reads through `;`** — a token without its
//!   own `=` is merged with following text until an `=` or end-of-string.
//! - If no `=` is found in the remainder, parsing **stops** (whatever was parsed
//!   so far is kept) and a warning (`01S00`) is raised.
//! - Keys and values are **never trimmed**; only leading whitespace/`;` before a
//!   key is skipped. `Server =host` therefore does *not* match (trailing space).
//! - `{braced}` values end at a single `}`; `}}` is an escape for a literal `}`.
//! - A braced value must be followed by `;` or end-of-string; trailing junk after
//!   `}` stops parsing with a warning.
//! - Unknown keywords never fail the parse — they are ignored with a warning.
//!   Only an invalid *value* for a recognized, validated key is a hard error.

use std::fmt;

use crate::connection::odbc_supported_auth_keywords::is_recognized_keyword;
use tracing::warn;

pub(crate) mod odbc_authentication_transformer;
pub(crate) mod odbc_authentication_validator;
mod odbc_supported_auth_keywords;

// Connection string keys (lowercase for matching)
const KEY_SERVER: &str = "server";
const KEY_DATABASE: &str = "database";
const KEY_UID: &str = "uid";
const KEY_PWD: &str = "pwd";
const KEY_TRUST_SRV_CERT: &str = "trustservercertificate";
const KEY_ENCRYPT: &str = "encrypt";
const KEY_AUTHENTICATION: &str = "authentication";
const KEY_TRUSTED_CONNECTION: &str = "trusted_connection";

// Recognized msodbcsql keywords we accept but do not act on. Mirrors the
// non-acted-on entries of msodbcsql's `x_rgLookup` table (including synonyms and
// deprecated keys). Recognized keys never raise the 01S00 "invalid attribute"
// warning even when unsupported; only genuinely unknown keys do.
//
// Note: unlike ADO.NET/OLE DB, the msodbcsql ODBC parser does NOT recognize
// `Initial Catalog` or `User Id`; those are intentionally absent here so they are
// treated as unknown, matching the driver.
const KNOWN_IGNORED_KEYS: &[&str] = &[
    "savefile",
    "filedsn",
    "dsn",
    "description",
    "desc",
    "driver",
    "app",
    "wsid",
    "language",
    "network",
    "net",
    "address",
    "addr",
    "mars_connection",
    "failover_partner",
    "failoverpartnerspn",
    "autotranslate",
    "querylog_on",
    "querylogfile",
    "querylogtime",
    "statslog_on",
    "statslogfile",
    "regional",
    "quotedid",
    "ansinpw",
    "attachdbfilename",
    "serverspn",
    "applicationintent",
    "multisubnetfailover",
    "connectretrycount",
    "connectretryinterval",
    "clientcertificate",
    "columnencryption",
    "transparentnetworkipresolution",
    "keystoreauthentication",
    "keystoreprincipalid",
    "keystoresecret",
    "keystorelocation",
    "usefmtonly",
    "clientkey",
    "keepalive",
    "keepaliveinterval",
    "replication",
    "longasmax",
    "hostnameincertificate",
    "getdataextensions",
    "ipaddresspreference",
    "servercertificate",
    "retryexec",
    "concatnullyieldsnull",
    "packetsize",
    "vectortypesupport",
    // Deprecated keys kept for back-compat (msodbcsql KEY_UNUSED entries).
    "oemtoansi",
    "translationname",
    "translationoption",
    "translationdll",
    "fastconnectoption",
    "useprocforprepare",
    "fallback",
];

// Valid attribute values
const YES_NO: &[&str] = &["yes", "no"];
const ENCRYPT_VALUES: &[&str] = &["yes", "mandatory", "no", "optional", "strict"];

// Recognized `Authentication=` keywords (mirrors `auth_method_from_keyword`).
// Used only for the diagnostic hint; the accept/reject decision is delegated to
// `is_recognized_keyword` so the two never drift.
const AUTHENTICATION_VALUES: &[&str] = &[
    "SqlPassword",
    "ActiveDirectoryIntegrated",
    "ActiveDirectoryPassword",
    "ActiveDirectoryInteractive",
    "ActiveDirectoryMSI",
    "ActiveDirectoryServicePrincipal",
    "ActiveDirectoryDefault",
    "ActiveDirectoryDeviceCodeFlow",
    "ActiveDirectoryWorkloadIdentity",
];

#[derive(Copy, Clone)]
enum ConnAttrKey {
    Server,
    Database,
    Uid,
    Pwd,
    TrustServerCert,
    Encrypt,
    Authentication,
    TrustedConnection,
    Count,
}

impl ConnAttrKey {
    const COUNT: usize = ConnAttrKey::Count as usize;

    const fn idx(self) -> usize {
        self as usize
    }
}

fn validate_attr(
    key: &str,
    value: &str,
    valid: &'static [&'static str],
) -> Result<(), InvalidAttrValue> {
    if valid.iter().any(|v| v.eq_ignore_ascii_case(value)) {
        Ok(())
    } else {
        Err(InvalidAttrValue {
            key: key.to_string(),
            value: value.to_string(),
            expected: valid,
        })
    }
}

#[derive(Debug, Clone)]
pub(crate) struct InvalidAttrValue {
    pub(crate) key: String,
    pub(crate) value: String,
    pub(crate) expected: &'static [&'static str],
}

impl fmt::Display for InvalidAttrValue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "invalid value '{}' for '{}'; expected one of: {}",
            self.value,
            self.key,
            self.expected.join(", ")
        )
    }
}

/// Parsed connection parameters extracted from an ODBC connection string.
#[derive(Clone, Default)]
pub(crate) struct ConnectionParams {
    pub(crate) server: String,
    pub(crate) database: String,
    pub(crate) uid: String,
    pub(crate) pwd: String,
    pub(crate) trust_server_certificate: bool,
    pub(crate) encrypt: Option<String>,
    pub(crate) authentication: Option<String>,
    pub(crate) trusted_connection: Option<bool>,
}

impl ConnectionParams {
    pub(crate) fn fmt_as_odbc_conn_str(&self) -> String {
        let mut parts = Vec::new();

        if !self.server.is_empty() {
            parts.push(format!("Server={}", self.server));
        }
        if !self.database.is_empty() {
            parts.push(format!("Database={}", self.database));
        }
        if !self.uid.is_empty() {
            parts.push(format!("UID={}", self.uid));
        }
        if !self.pwd.is_empty() {
            parts.push("PWD=******".to_string());
        }
        if self.trust_server_certificate {
            parts.push("TrustServerCertificate=yes".to_string());
        }
        if let Some(encrypt) = &self.encrypt {
            parts.push(format!("Encrypt={encrypt}"));
        }
        if let Some(authentication) = &self.authentication {
            parts.push(format!("Authentication={authentication}"));
        }
        if let Some(trusted_connection) = self.trusted_connection {
            parts.push(format!(
                "Trusted_Connection={}",
                if trusted_connection { "yes" } else { "no" }
            ));
        }

        parts.join(";")
    }
}

impl fmt::Debug for ConnectionParams {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ConnectionParams")
            .field("server", &self.server)
            .field("database", &self.database)
            .field("uid", &self.uid)
            .field("pwd", &"<REDACTED>")
            .field("trust_server_certificate", &self.trust_server_certificate)
            .field("encrypt", &self.encrypt)
            .field("authentication", &self.authentication)
            .field("trusted_connection", &self.trusted_connection)
            .finish()
    }
}

/// Classification of a connection-string key against the msodbcsql keyword table.
enum KeyClass {
    /// Recognized and acted upon; maps to a [`ConnectionParams`] field.
    Mapped(ConnAttrKey),
    /// Recognized by msodbcsql but not acted upon here; raises no warning.
    Ignored,
    /// Not a recognized keyword; raises an `01S00` warning but never fails.
    Unknown,
}

/// The whitespace set used by msodbcsql's `ISSPACE`: space, form-feed, newline,
/// carriage return, tab, and vertical tab. Deliberately narrower than
/// [`char::is_whitespace`] to match the driver exactly.
fn is_odbc_space(c: char) -> bool {
    matches!(c, ' ' | '\u{0c}' | '\n' | '\r' | '\t' | '\u{0b}')
}

fn classify_key(lower: &str) -> KeyClass {
    match lower {
        KEY_SERVER => KeyClass::Mapped(ConnAttrKey::Server),
        KEY_DATABASE => KeyClass::Mapped(ConnAttrKey::Database),
        KEY_UID => KeyClass::Mapped(ConnAttrKey::Uid),
        KEY_PWD => KeyClass::Mapped(ConnAttrKey::Pwd),
        KEY_TRUST_SRV_CERT => KeyClass::Mapped(ConnAttrKey::TrustServerCert),
        KEY_ENCRYPT => KeyClass::Mapped(ConnAttrKey::Encrypt),
        KEY_AUTHENTICATION => KeyClass::Mapped(ConnAttrKey::Authentication),
        KEY_TRUSTED_CONNECTION => KeyClass::Mapped(ConnAttrKey::TrustedConnection),
        _ if KNOWN_IGNORED_KEYS.contains(&lower) => KeyClass::Ignored,
        _ => KeyClass::Unknown,
    }
}

/// Validate and store a parsed value for a recognized, acted-upon key.
///
/// Returns `Err(InvalidAttrValue)` for an invalid value on a validated key,
/// mirroring msodbcsql's `E_FAIL` from `IsAttrStrValid` (a hard connect failure).
fn assign_value(
    params: &mut ConnectionParams,
    slot: ConnAttrKey,
    lower: &str,
    value: &str,
) -> Result<(), InvalidAttrValue> {
    match slot {
        ConnAttrKey::Server => params.server = value.to_string(),
        ConnAttrKey::Database => params.database = value.to_string(),
        ConnAttrKey::Uid => params.uid = value.to_string(),
        ConnAttrKey::Pwd => params.pwd = value.to_string(),
        ConnAttrKey::TrustServerCert => {
            validate_attr(lower, value, YES_NO)?;
            params.trust_server_certificate = is_yes(value);
        }
        ConnAttrKey::Encrypt => {
            validate_attr(lower, value, ENCRYPT_VALUES)?;
            params.encrypt = Some(value.to_string());
        }
        ConnAttrKey::Authentication => {
            // Recognized-keyword check is delegated to mssql-tds (the source of
            // truth). Whether a recognized method is *implemented* is gated later;
            // parsing only rejects values mssql-tds does not recognize.
            if !is_recognized_keyword(value) {
                return Err(InvalidAttrValue {
                    key: lower.to_string(),
                    value: value.to_string(),
                    expected: AUTHENTICATION_VALUES,
                });
            }
            params.authentication = Some(value.to_string());
        }
        ConnAttrKey::TrustedConnection => {
            validate_attr(lower, value, YES_NO)?;
            params.trusted_connection = Some(is_yes(value));
        }
        ConnAttrKey::Count => {}
    }
    Ok(())
}

/// Parse an ODBC connection string into [`ConnectionParams`].
///
/// A single-pass, character-by-character state machine that reproduces the
/// behavior of msodbcsql's `ParseAttrStr`. See the module-level documentation for
/// the full list of reproduced quirks.
///
/// Returns `Ok((params, has_warnings))` on success, or `Err(InvalidAttrValue)`
/// for an invalid value on a recognized, validated key (msodbcsql `E_FAIL`).
/// `has_warnings` is true when any `01S00` condition was hit (unknown key, missing
/// `=`, missing value, unterminated brace, or data after a braced value).
pub(crate) fn parse_connection_string(
    input: &str,
) -> Result<(ConnectionParams, bool), InvalidAttrValue> {
    let chars: Vec<char> = input.chars().collect();
    let n = chars.len();
    let mut i = 0;

    let mut params = ConnectionParams::default();
    let mut seen_slots = [false; ConnAttrKey::COUNT];
    let mut has_warnings = false;

    loop {
        // Skip leading whitespace and separators before the key.
        while i < n && (is_odbc_space(chars[i]) || chars[i] == ';') {
            i += 1;
        }
        if i >= n {
            break;
        }

        // Read the key up to '='. This reads *through* ';' — a token without its
        // own '=' is merged with the following text until an '=' or end-of-input.
        let key_start = i;
        while i < n && chars[i] != '=' {
            i += 1;
        }
        if i >= n {
            // No '=' in the remainder: stop parsing, keeping what we have (S_FALSE).
            warn!("invalid connection string attribute (no '=' separator)");
            has_warnings = true;
            break;
        }
        let key: String = chars[key_start..i].iter().collect();
        i += 1; // consume '='
        let lower = key.to_ascii_lowercase();

        // A recognized, first-seen, acted-upon key receives the value; everything
        // else parses the value but discards it (mirrors msodbcsql's lpszValue).
        let target = match classify_key(&lower) {
            KeyClass::Mapped(slot) => {
                let idx = slot.idx();
                if seen_slots[idx] {
                    None // duplicate recognized key: first occurrence wins
                } else {
                    seen_slots[idx] = true;
                    Some(slot)
                }
            }
            KeyClass::Ignored => None,
            KeyClass::Unknown => {
                warn!(key = %key, "unknown connection string attribute");
                has_warnings = true;
                None
            }
        };

        // No value after '=': stop parsing (S_FALSE).
        if i >= n {
            warn!("invalid connection string attribute (missing value)");
            has_warnings = true;
            break;
        }

        // Read the value.
        let mut value = String::new();
        let mut stop_after = false;
        if chars[i] == '{' {
            i += 1;
            let mut terminated = false;
            while i < n {
                if chars[i] == '}' {
                    // '}}' is an escape for a literal '}'.
                    if i + 1 < n && chars[i + 1] == '}' {
                        value.push('}');
                        i += 2;
                        continue;
                    }
                    terminated = true;
                    break;
                }
                value.push(chars[i]);
                i += 1;
            }
            if terminated {
                i += 1; // consume closing '}'
                // A braced value must be followed by ';' or end-of-input.
                if i < n && chars[i] != ';' {
                    warn!("invalid connection string attribute (data after braced value)");
                    has_warnings = true;
                    stop_after = true;
                }
            } else {
                // Unterminated brace: value ran to the end of the string.
                warn!("unterminated braced value in connection string");
                has_warnings = true;
                stop_after = true;
            }
        } else {
            while i < n && chars[i] != ';' {
                value.push(chars[i]);
                i += 1;
            }
        }

        // Validate and store. msodbcsql stores the value before its brace-close
        // checks, so a value with trailing junk is still stored before we stop.
        // An invalid value on a validated key fails immediately (E_FAIL).
        if let Some(slot) = target {
            assign_value(&mut params, slot, &lower, &value)?;
        }

        if stop_after {
            break;
        }

        // Consume the trailing separator (if present) and continue.
        if i < n {
            i += 1;
        }
    }

    Ok((params, has_warnings))
}

fn is_yes(value: &str) -> bool {
    value.eq_ignore_ascii_case("yes")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::cs;

    #[test]
    fn basic_connection_string() {
        let (params, has_warnings) = parse_connection_string(&cs(
            "Server=localhost,1434;Database=master;UID=sa;<PW>=secret;TrustServerCertificate=yes",
        ))
        .unwrap();
        assert!(!has_warnings);
        assert_eq!(params.server, "localhost,1434");
        assert_eq!(params.database, "master");
        assert_eq!(params.uid, "sa");
        assert_eq!(params.pwd, "secret");
        assert!(params.trust_server_certificate);
    }

    #[test]
    fn server_formats() {
        let (p, ..) = parse_connection_string(&cs("Server=myhost;UID=u;<PW>=p")).unwrap();
        assert_eq!(p.server, "myhost");

        let (p, ..) = parse_connection_string(&cs("Server=tcp:myhost,2000;UID=u;<PW>=p")).unwrap();
        assert_eq!(p.server, "tcp:myhost,2000");

        let (p, ..) = parse_connection_string(&cs("Server=::1;UID=u;<PW>=p")).unwrap();
        assert_eq!(p.server, "::1");

        let (p, ..) = parse_connection_string(&cs("Server=host,abc;UID=u;<PW>=p")).unwrap();
        assert_eq!(p.server, "host,abc");
    }

    #[test]
    fn braced_values() {
        let (p, ..) =
            parse_connection_string(&cs("Server=host;<PW>={pass;with=special};UID=user")).unwrap();
        assert_eq!(p.pwd, "pass;with=special");
        assert_eq!(p.uid, "user");

        let (p, ..) = parse_connection_string(&cs("Server=h;<PW>={a=b;c=d};UID=u")).unwrap();
        assert_eq!(p.pwd, "a=b;c=d");
        assert_eq!(p.uid, "u");
    }

    #[test]
    fn key_matching_is_case_insensitive() {
        let (p, warn) = parse_connection_string(&cs(
            "SERVER=host;uid=user;<PW>=pass;trustservercertificate=Yes",
        ))
        .unwrap();
        assert!(!warn);
        assert_eq!(p.server, "host");
        assert_eq!(p.uid, "user");
        assert_eq!(p.pwd, "pass");
        assert!(p.trust_server_certificate);
    }

    #[test]
    fn adonet_only_keywords_are_unknown() {
        // msodbcsql's ODBC parser does NOT recognize `Initial Catalog`, `User Id`,
        // or `Password` (those are ADO.NET / OLE DB spellings). They are treated as
        // unknown keys: the mapped field stays unset and a warning is raised.
        let (p, warn) =
            parse_connection_string("Server=host;Initial Catalog=mydb;UID=u;PWD=p").unwrap();
        assert!(warn);
        assert_eq!(p.database, "");

        let (p, warn) = parse_connection_string("Server=host;User Id=admin;PWD=p").unwrap();
        assert!(warn);
        assert_eq!(p.uid, "");

        let (p, warn) = parse_connection_string("Server=host;UID=u;Password=p").unwrap();
        assert!(warn);
        assert_eq!(p.pwd, "");
    }

    #[test]
    fn separator_and_empty_edge_cases() {
        let (p, warn) = parse_connection_string("").unwrap();
        assert!(!warn);
        assert_eq!(p.server, "");

        // Empty value mid-string is fine (only end-of-string right after '=' stops).
        let (p, warn) = parse_connection_string(&cs("Server=host;Database=;UID=u;<PW>=p")).unwrap();
        assert!(!warn);
        assert_eq!(p.database, "");
        assert_eq!(p.uid, "u");

        // Trailing separator is skipped cleanly.
        let (p, warn) = parse_connection_string(&cs("Server=host;UID=u;<PW>=p;")).unwrap();
        assert!(!warn);
        assert_eq!(p.server, "host");

        // Runs of separators are skipped.
        let (p, warn) = parse_connection_string(&cs("Server=host;;;UID=u;;<PW>=p")).unwrap();
        assert!(!warn);
        assert_eq!(p.uid, "u");

        // Leading separators are skipped.
        let (p, warn) = parse_connection_string(&cs(";;Server=host;UID=u;<PW>=p")).unwrap();
        assert!(!warn);
        assert_eq!(p.server, "host");
    }

    #[test]
    fn whitespace_is_not_trimmed() {
        // msodbcsql skips only leading whitespace before a key. Trailing space in a
        // key (before '=') stays part of the key, so it no longer matches; spaces in
        // a value are preserved verbatim.
        let (p, warn) =
            parse_connection_string(&cs(" Server = host ; UID = user ; <PW> = pass ")).unwrap();
        assert!(warn); // "Server ", " UID ", " PWD " are all unknown
        assert_eq!(p.server, "");
        assert_eq!(p.uid, "");
        assert_eq!(p.pwd, "");

        // With exact keys, values keep their surrounding spaces.
        let (p, warn) = parse_connection_string("Server= host ;UID=u;PWD=p").unwrap();
        assert!(!warn);
        assert_eq!(p.server, " host ");
    }

    #[test]
    fn encrypt_values() {
        for (input, expected) in [
            ("yes", "yes"),
            ("strict", "strict"),
            ("Mandatory", "Mandatory"),
            ("Optional", "Optional"),
        ] {
            let (p, _) =
                parse_connection_string(&cs(&format!("Server=h;UID=u;<PW>=p;Encrypt={input}")))
                    .unwrap();
            assert_eq!(p.encrypt.as_deref(), Some(expected));
        }
    }

    #[test]
    fn duplicates_follow_first_wins() {
        let (p, ..) =
            parse_connection_string(&cs("Server=first;Server=second;UID=u;<PW>=p")).unwrap();
        assert_eq!(p.server, "first");

        let (p, ..) =
            parse_connection_string(&cs("UID=first;User Id=second;<PW>=p;Server=h")).unwrap();
        assert_eq!(p.uid, "first");

        let (p, ..) =
            parse_connection_string(&cs("Server=h;UID=u;<PW>=p;Encrypt=yes;Encrypt=banana"))
                .unwrap();
        assert_eq!(p.encrypt.as_deref(), Some("yes"));
    }

    #[test]
    fn invalid_attr_values() {
        let err = parse_connection_string(&cs("Server=h;UID=u;<PW>=p;Encrypt=true")).unwrap_err();
        assert_eq!(err.key, "encrypt");
        assert_eq!(err.value, "true");

        let err = parse_connection_string(&cs("Server=h;UID=u;<PW>=p;TrustServerCertificate=1"))
            .unwrap_err();
        assert_eq!(err.key, "trustservercertificate");
        assert_eq!(err.value, "1");
    }

    #[test]
    fn malformed_tokens_set_warning() {
        // A token without its own '=' merges with the following text (the key scan
        // reads through ';'). Here the key becomes "bogus;UID", so UID is swallowed
        // and never set; PWD still parses afterwards.
        let (p, warn) = parse_connection_string(&cs("Server=h;bogus;UID=u;<PW>=p")).unwrap();
        assert!(warn);
        assert_eq!(p.server, "h");
        assert_eq!(p.uid, "");
        assert_eq!(p.pwd, "p");

        // Empty key (value with no key) warns but parsing continues past it.
        let (p, warn) = parse_connection_string(&cs("Server=h;=orphan;UID=u;<PW>=p")).unwrap();
        assert!(warn);
        assert_eq!(p.uid, "u");

        // Both in one string
        let (_, warn) =
            parse_connection_string(&cs("noequals;=empty;Server=h;UID=u;<PW>=p")).unwrap();
        assert!(warn);

        // Clean string has no warnings
        let (_, warn) = parse_connection_string(&cs("Server=h;UID=u;<PW>=p")).unwrap();
        assert!(!warn);

        // Unknown but well-formed keys are ignored with warning.
        let (_, warn) = parse_connection_string(&cs("Server=h;UID=u;<PW>=p;FooBar=1")).unwrap();
        assert!(warn);

        // Known-but-ignored keys should not warn.
        let (_, warn) = parse_connection_string(&cs(
            "Driver={ODBC Driver 18 for SQL Server};Server=h;UID=u;<PW>=p",
        ))
        .unwrap();
        assert!(!warn);
    }

    #[test]
    fn unterminated_brace_sets_warning() {
        // Missing closing '}' — consumes rest of string as value, warns
        let (p, warn) = parse_connection_string(&cs("Server=h;<PW>={abc;UID=u")).unwrap();
        assert!(warn);
        assert_eq!(p.pwd, "abc;UID=u");
        assert_eq!(p.uid, ""); // UID was swallowed into the braced value
    }

    #[test]
    fn debug_redacts_password() {
        let (p, _) = parse_connection_string(&cs("Server=h;UID=u;<PW>=secret123")).unwrap();
        let debug_str = format!("{p:?}");
        assert!(debug_str.contains("<REDACTED>"));
        assert!(!debug_str.contains("secret123"));
    }

    #[test]
    fn fmt_as_odbc_conn_str_redacts_password() {
        let (p, _) =
            parse_connection_string(&cs("Server=h;Database=db;UID=u;<PW>=secret;Encrypt=strict"))
                .unwrap();
        assert_eq!(
            p.fmt_as_odbc_conn_str(),
            cs("Server=h;Database=db;UID=u;<PW>=******;Encrypt=strict")
        );
    }

    // ── Authentication / Trusted_Connection (T0) ─────────────

    #[test]
    fn authentication_recognized_keywords() {
        for kw in [
            "SqlPassword",
            "ActiveDirectoryIntegrated",
            "ActiveDirectoryPassword",
            "ActiveDirectoryInteractive",
            "ActiveDirectoryMSI",
            "ActiveDirectoryServicePrincipal",
            "ActiveDirectoryDefault",
            "ActiveDirectoryDeviceCodeFlow",
            "ActiveDirectoryWorkloadIdentity",
        ] {
            let (p, warn) =
                parse_connection_string(&format!("Server=h;UID=u;PWD=p;Authentication={kw}"))
                    .unwrap();
            assert_eq!(p.authentication.as_deref(), Some(kw), "keyword {kw}");
            assert!(!warn, "recognized Authentication should not warn: {kw}");
        }
    }

    #[test]
    fn authentication_case_insensitive_recognized() {
        // Recognized case-insensitively; the raw value is preserved as given.
        let (p, ..) = parse_connection_string(
            "Server=h;UID=u;PWD=p;authentication=activedirectoryintegrated",
        )
        .unwrap();
        assert_eq!(
            p.authentication.as_deref(),
            Some("activedirectoryintegrated")
        );
    }

    #[test]
    fn authentication_unrecognized_is_error() {
        let err =
            parse_connection_string("Server=h;UID=u;PWD=p;Authentication=NotReal").unwrap_err();
        assert_eq!(err.key, "authentication");
        assert_eq!(err.value, "NotReal");
    }

    #[test]
    fn authentication_empty_reset_vs_end_of_string() {
        // Empty Authentication mid-string is an intentional reset (mssql-tds treats
        // it as recognized); stored as Some("") to preserve the distinction from unset.
        let (p, warn) =
            parse_connection_string("Server=h;Authentication=;UID=u;PWD=p").unwrap();
        assert_eq!(p.authentication.as_deref(), Some(""));
        assert!(!warn);

        // But `Authentication=` at the very end of the string hits end-of-input right
        // after '=' — msodbcsql stops with S_FALSE and the value is never set.
        let (p, warn) = parse_connection_string("Server=h;UID=u;PWD=p;Authentication=").unwrap();
        assert_eq!(p.authentication, None);
        assert!(warn);
    }

    #[test]
    fn trusted_connection_yes_no() {
        let (p, ..) = parse_connection_string("Server=h;Trusted_Connection=Yes").unwrap();
        assert_eq!(p.trusted_connection, Some(true));

        let (p, ..) = parse_connection_string("Server=h;Trusted_Connection=no").unwrap();
        assert_eq!(p.trusted_connection, Some(false));
    }

    #[test]
    fn trusted_connection_invalid_is_error() {
        let err = parse_connection_string("Server=h;Trusted_Connection=1").unwrap_err();
        assert_eq!(err.key, "trusted_connection");
        assert_eq!(err.value, "1");

        let err = parse_connection_string("Server=h;Trusted_Connection=true").unwrap_err();
        assert_eq!(err.key, "trusted_connection");
    }

    #[test]
    fn trusted_connection_no_longer_silently_ignored() {
        // Previously in KNOWN_IGNORED_KEYS and dropped without capture. Now parsed;
        // still no 01S00 warning, but the value is retained.
        let (p, warn) = parse_connection_string("Server=h;Trusted_Connection=Yes").unwrap();
        assert!(!warn);
        assert_eq!(p.trusted_connection, Some(true));
    }

    #[test]
    fn auth_keys_follow_first_wins() {
        let (p, ..) = parse_connection_string(
            "Server=h;Authentication=ActiveDirectoryIntegrated;Authentication=SqlPassword",
        )
        .unwrap();
        assert_eq!(
            p.authentication.as_deref(),
            Some("ActiveDirectoryIntegrated")
        );

        let (p, ..) =
            parse_connection_string("Server=h;Trusted_Connection=Yes;Trusted_Connection=No")
                .unwrap();
        assert_eq!(p.trusted_connection, Some(true));
    }

    #[test]
    fn auth_and_existing_keys_together() {
        let (p, warn) = parse_connection_string(
            "Server=h;Database=db;UID=u;PWD=p;Encrypt=strict;Authentication=ActiveDirectoryServicePrincipal",
        )
        .unwrap();
        assert!(!warn);
        assert_eq!(p.server, "h");
        assert_eq!(p.database, "db");
        assert_eq!(p.uid, "u");
        assert_eq!(p.pwd, "p");
        assert_eq!(p.encrypt.as_deref(), Some("strict"));
        assert_eq!(
            p.authentication.as_deref(),
            Some("ActiveDirectoryServicePrincipal")
        );
    }

    #[test]
    fn new_auth_fields_default_none() {
        let (p, ..) = parse_connection_string("Server=h;UID=u;PWD=p").unwrap();
        assert_eq!(p.authentication, None);
        assert_eq!(p.trusted_connection, None);
    }

    #[test]
    fn auth_fields_render_without_leaking_secrets() {
        let (p, ..) = parse_connection_string(
            "Server=h;UID=u;PWD=secret;Authentication=ActiveDirectoryIntegrated;Trusted_Connection=Yes",
        )
        .unwrap();

        let dbg = format!("{p:?}");
        assert!(dbg.contains("ActiveDirectoryIntegrated"));
        assert!(dbg.contains("<REDACTED>"));
        assert!(!dbg.contains("secret"));

        let s = p.fmt_as_odbc_conn_str();
        assert!(s.contains("Authentication=ActiveDirectoryIntegrated"));
        assert!(s.contains("Trusted_Connection=yes"));
        assert!(s.contains("PWD=******"));
        assert!(!s.contains("secret"));
    }

    // ── Exhaustive msodbcsql `ParseAttrStr` fidelity quirks ──────────────

    #[test]
    fn key_scan_reads_through_separator() {
        // A token without its own '=' merges with following text: the key scan does
        // not stop on ';'. Here the key is "foo;Server", so Server is never set.
        let (p, warn) = parse_connection_string("foo;Server=host;UID=u;PWD=p").unwrap();
        assert!(warn);
        assert_eq!(p.server, "");
        assert_eq!(p.uid, "u");
    }

    #[test]
    fn missing_equals_stops_parsing() {
        // No '=' in the remainder → S_FALSE, stop. Everything parsed so far is kept,
        // but nothing after the malformed tail is parsed.
        let (p, warn) = parse_connection_string("Server=host;UID=u;trailingjunk").unwrap();
        assert!(warn);
        assert_eq!(p.server, "host");
        assert_eq!(p.uid, "u");
        assert_eq!(p.pwd, "");
    }

    #[test]
    fn missing_value_at_end_stops_parsing() {
        // End-of-input immediately after '=' → S_FALSE, stop with the value unset.
        let (p, warn) = parse_connection_string("Server=host;UID=u;Database=").unwrap();
        assert!(warn);
        assert_eq!(p.server, "host");
        assert_eq!(p.uid, "u");
        assert_eq!(p.database, "");
    }

    #[test]
    fn empty_value_midstring_is_not_a_stop() {
        // An empty value is fine when a ';' (not end-of-input) follows the '='.
        let (p, warn) = parse_connection_string("Server=host;Database=;UID=u;PWD=p").unwrap();
        assert!(!warn);
        assert_eq!(p.database, "");
        assert_eq!(p.uid, "u");
        assert_eq!(p.pwd, "p");
    }

    #[test]
    fn braced_double_brace_escape() {
        // '}}' inside a braced value is a literal '}'.
        let (p, warn) = parse_connection_string("Server=h;PWD={a}}b};UID=u").unwrap();
        assert!(!warn);
        assert_eq!(p.pwd, "a}b");
        assert_eq!(p.uid, "u");

        // Multiple escapes.
        let (p, warn) = parse_connection_string("Server=h;PWD={p}}}}q};UID=u").unwrap();
        assert!(!warn);
        assert_eq!(p.pwd, "p}}q");
        assert_eq!(p.uid, "u");
    }

    #[test]
    fn braced_value_preserves_separators_and_equals() {
        let (p, warn) = parse_connection_string("Server=h;PWD={a;b=c d};UID=u").unwrap();
        assert!(!warn);
        assert_eq!(p.pwd, "a;b=c d");
        assert_eq!(p.uid, "u");
    }

    #[test]
    fn junk_after_braced_value_stops_parsing() {
        // A braced value must be followed by ';' or end-of-input. Trailing junk after
        // '}' stops parsing with a warning — the value is still stored first.
        let (p, warn) = parse_connection_string("Server=h;PWD={val}junk;UID=u").unwrap();
        assert!(warn);
        assert_eq!(p.pwd, "val");
        assert_eq!(p.uid, ""); // parsing stopped, UID never reached
    }

    #[test]
    fn braced_value_at_end_of_string() {
        let (p, warn) = parse_connection_string("Server=h;UID=u;PWD={secret}").unwrap();
        assert!(!warn);
        assert_eq!(p.pwd, "secret");
    }

    #[test]
    fn unterminated_brace_swallows_rest_and_stops() {
        let (p, warn) = parse_connection_string("Server=h;PWD={abc;UID=u").unwrap();
        assert!(warn);
        assert_eq!(p.pwd, "abc;UID=u");
        assert_eq!(p.uid, "");
    }

    #[test]
    fn brace_not_at_value_start_is_literal() {
        // '{' is only special as the first character of the value.
        let (p, warn) = parse_connection_string("Server=h;UID=u;PWD=a{b}c").unwrap();
        assert!(!warn);
        assert_eq!(p.pwd, "a{b}c");
    }

    #[test]
    fn recognized_but_ignored_keys_do_not_warn() {
        for key in [
            "Driver", "DSN", "APP", "WSID", "Language", "Network", "Address",
            "MARS_Connection", "AutoTranslate", "QuotedId", "ApplicationIntent",
            "MultiSubnetFailover", "ConnectRetryCount", "PacketSize", "ColumnEncryption",
            "TransparentNetworkIPResolution", "OEMToANSI",
        ] {
            let s = format!("Server=h;{key}=whatever;UID=u;PWD=p");
            let (p, warn) = parse_connection_string(&s).unwrap();
            assert!(!warn, "recognized-but-ignored key should not warn: {key}");
            assert_eq!(p.server, "h", "key {key}");
            assert_eq!(p.uid, "u", "key {key}");
        }
    }

    #[test]
    fn unknown_keys_warn_but_never_fail() {
        let (p, warn) = parse_connection_string("Server=h;TotallyMadeUp=1;UID=u;PWD=p").unwrap();
        assert!(warn);
        assert_eq!(p.server, "h");
        assert_eq!(p.uid, "u");
        assert_eq!(p.pwd, "p");
    }

    #[test]
    fn duplicate_recognized_key_first_wins() {
        let (p, warn) =
            parse_connection_string("Server=first;Server=second;UID=a;UID=b;PWD=p").unwrap();
        assert!(!warn);
        assert_eq!(p.server, "first");
        assert_eq!(p.uid, "a");
    }

    #[test]
    fn invalid_value_is_hard_error_not_warning() {
        // Encrypt is a validated key; an invalid value is E_FAIL (Err), not a warning.
        let err = parse_connection_string("Server=h;UID=u;PWD=p;Encrypt=banana").unwrap_err();
        assert_eq!(err.key, "encrypt");
        assert_eq!(err.value, "banana");

        let err =
            parse_connection_string("Server=h;Trusted_Connection=maybe;UID=u").unwrap_err();
        assert_eq!(err.key, "trusted_connection");
        assert_eq!(err.value, "maybe");
    }

    #[test]
    fn value_validation_is_case_insensitive() {
        let (p, warn) = parse_connection_string("Server=h;Encrypt=STRICT;UID=u").unwrap();
        assert!(!warn);
        assert_eq!(p.encrypt.as_deref(), Some("STRICT"));

        let (p, ..) = parse_connection_string("Server=h;TrustServerCertificate=YES;UID=u").unwrap();
        assert!(p.trust_server_certificate);
    }

    #[test]
    fn only_ascii_odbc_whitespace_is_skipped_before_key() {
        // Tab / newline / CR before a key are skipped like a space.
        let (p, warn) = parse_connection_string("Server=h;\t\r\nUID=u;PWD=p").unwrap();
        assert!(!warn);
        assert_eq!(p.uid, "u");

        // A non-breaking space is NOT ODBC whitespace, so it becomes part of the key.
        let (p, warn) = parse_connection_string("Server=h;\u{00a0}UID=u;PWD=p").unwrap();
        assert!(warn);
        assert_eq!(p.uid, "");
    }

    #[test]
    fn empty_and_separator_only_inputs() {
        for input in ["", ";", ";;;", "   ", " ; ; "] {
            let (p, warn) = parse_connection_string(input).unwrap();
            assert!(!warn, "input {input:?} should not warn");
            assert_eq!(p.server, "", "input {input:?}");
        }
    }
}
