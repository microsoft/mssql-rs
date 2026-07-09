// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Connection string parsing for ODBC `SQLDriverConnect`.
//!
//! Parses `;`-delimited `Key=Value` connection strings per the ODBC spec.
//! Supports `{braced values}` for values containing `;` or `=`.

use std::fmt;

use mssql_tds::connection::odbc_supported_auth_keywords::is_recognized_keyword;
use tracing::warn;

// Connection string keys (lowercase for matching)
const KEY_SERVER: &str = "server";
const KEY_DATABASE: &str = "database";
const KEY_INITIAL_CATALOG: &str = "initial catalog";
const KEY_UID: &str = "uid";
const KEY_USER_ID: &str = "user id";
const KEY_PWD: &str = "pwd";
const KEY_PASSWORD: &str = "password";
const KEY_TRUST_SRV_CERT: &str = "trustservercertificate";
const KEY_ENCRYPT: &str = "encrypt";
const KEY_AUTHENTICATION: &str = "authentication";
const KEY_TRUSTED_CONNECTION: &str = "trusted_connection";

// Known keys we currently accept but do not act on.
// These should not produce 01S00 warnings.
const KNOWN_IGNORED_KEYS: &[&str] = &[
    "driver",
    "dsn",
    "filedsn",
    "savefile",
    "app",
    "wsid",
    "language",
    "network",
    "net",
    "address",
    "addr",
    "mars_connection",
    "autotranslate",
    "quotedid",
    "ansi_npw",
    "regional",
];

// Valid attribute values
const YES_NO: &[&str] = &["yes", "no"];
const ENCRYPT_VALUES: &[&str] = &["yes", "mandatory", "no", "optional", "strict"];

// Recognized `Authentication=` keywords (mirrors mssql-tds `auth_method_from_keyword`).
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

/// Parse an ODBC connection string into key-value pairs.
///
/// Format: `Key1=Value1;Key2=Value2;...`
/// Values may be `{braced}` to contain `;` or `=` literally.
///
/// Returns `Ok((params, has_warnings))` on success, or `Err(InvalidAttrValue)`
/// for invalid attribute values on known keys.
/// `has_warnings` is true for malformed tokens and unknown keys (SQLSTATE 01S00).
pub(crate) fn parse_connection_string(
    input: &str,
) -> Result<(ConnectionParams, bool), InvalidAttrValue> {
    let (pairs, mut has_warnings) = tokenize(input);
    let mut params = ConnectionParams::default();
    let mut seen_slots = [false; ConnAttrKey::COUNT];

    for (key, value) in &pairs {
        let lower = key.to_ascii_lowercase();

        let slot = match lower.as_str() {
            KEY_SERVER => Some(ConnAttrKey::Server),
            KEY_DATABASE | KEY_INITIAL_CATALOG => Some(ConnAttrKey::Database),
            KEY_UID | KEY_USER_ID => Some(ConnAttrKey::Uid),
            KEY_PWD | KEY_PASSWORD => Some(ConnAttrKey::Pwd),
            KEY_TRUST_SRV_CERT => Some(ConnAttrKey::TrustServerCert),
            KEY_ENCRYPT => Some(ConnAttrKey::Encrypt),
            KEY_AUTHENTICATION => Some(ConnAttrKey::Authentication),
            KEY_TRUSTED_CONNECTION => Some(ConnAttrKey::TrustedConnection),
            _ if KNOWN_IGNORED_KEYS.contains(&lower.as_str()) => None,
            _ => {
                // Match msodbcsql behavior: unknown attributes are ignored,
                // but reported as warning (01S00) on successful connect.
                warn!(key = %key, "unknown connection string attribute");
                has_warnings = true;
                None
            }
        };

        if let Some(slot) = slot {
            let idx = slot.idx();
            if seen_slots[idx] {
                // Match msodbcsql behavior: ignore duplicate recognized attributes.
                continue;
            }
            seen_slots[idx] = true;

            match slot {
                ConnAttrKey::Server => params.server = value.clone(),
                ConnAttrKey::Database => params.database = value.clone(),
                ConnAttrKey::Uid => params.uid = value.clone(),
                ConnAttrKey::Pwd => params.pwd = value.clone(),
                ConnAttrKey::TrustServerCert => {
                    validate_attr(&lower, value, YES_NO)?;
                    params.trust_server_certificate = is_yes(value);
                }
                ConnAttrKey::Encrypt => {
                    validate_attr(&lower, value, ENCRYPT_VALUES)?;
                    params.encrypt = Some(value.clone());
                }
                ConnAttrKey::Authentication => {
                    // Recognized-keyword check is delegated to mssql-tds
                    // (the source of truth). Whether a recognized method is
                    // *implemented* is gated later (T1-T4); parsing only
                    // rejects values mssql-tds does not recognize.
                    if !is_recognized_keyword(value) {
                        return Err(InvalidAttrValue {
                            key: lower.clone(),
                            value: value.clone(),
                            expected: AUTHENTICATION_VALUES,
                        });
                    }
                    params.authentication = Some(value.clone());
                }
                ConnAttrKey::TrustedConnection => {
                    validate_attr(&lower, value, YES_NO)?;
                    params.trusted_connection = Some(is_yes(value));
                }
                ConnAttrKey::Count => {}
            }
            continue;
        }
    }
    Ok((params, has_warnings))
}

fn is_yes(value: &str) -> bool {
    value.eq_ignore_ascii_case("yes")
}

/// Tokenize a connection string into (key, value) pairs.
/// Returns the pairs and whether any malformed tokens were skipped.
fn tokenize(input: &str) -> (Vec<(String, String)>, bool) {
    let mut pairs = Vec::new();
    let mut has_warnings = false;
    let mut chars = input.chars().peekable();

    loop {
        // Skip leading whitespace and semicolons
        while chars.peek().is_some_and(|&c| c == ';' || c.is_whitespace()) {
            chars.next();
        }
        if chars.peek().is_none() {
            break;
        }

        // Read key (up to '=')
        let mut key = String::new();
        let mut found_eq = false;
        while let Some(&c) = chars.peek() {
            if c == '=' {
                chars.next(); // consume '='
                found_eq = true;
                break;
            }
            if c == ';' {
                break;
            }
            key.push(c);
            chars.next();
        }

        // No '=' found — skip this token (matches msodbcsql: S_FALSE / 01S00 warning)
        if !found_eq {
            warn!(token = %key, "invalid connection string attribute (no '=' separator)");
            has_warnings = true;
            continue;
        }
        let key = key.trim().to_string();
        if key.is_empty() {
            warn!("invalid connection string attribute (empty key)");
            has_warnings = true;
            continue;
        }

        // Read value
        let value = if chars.peek() == Some(&'{') {
            // Braced value — read until closing '}'
            chars.next(); // consume '{'
            let mut val = String::new();
            let mut found_closing_brace = false;
            for c in chars.by_ref() {
                if c == '}' {
                    found_closing_brace = true;
                    break;
                }
                val.push(c);
            }
            if !found_closing_brace {
                warn!(key = %key, "unterminated braced value in connection string");
                has_warnings = true;
            }
            // Skip trailing chars up to ';'
            while chars.peek().is_some_and(|&c| c != ';') {
                chars.next();
            }
            val
        } else {
            // Unbraced value — read until ';' or end
            let mut val = String::new();
            while let Some(&c) = chars.peek() {
                if c == ';' {
                    break;
                }
                val.push(c);
                chars.next();
            }
            val.trim().to_string()
        };

        pairs.push((key, value));
    }

    (pairs, has_warnings)
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
    fn key_aliases() {
        let (p, ..) = parse_connection_string(&cs(
            "SERVER=host;uid=user;<PASS>=pass;trustservercertificate=Yes",
        ))
        .unwrap();
        assert_eq!(p.server, "host");
        assert_eq!(p.uid, "user");
        assert_eq!(p.pwd, "pass");
        assert!(p.trust_server_certificate);

        let (p, ..) =
            parse_connection_string(&cs("Server=host;Initial Catalog=mydb;UID=u;<PW>=p")).unwrap();
        assert_eq!(p.database, "mydb");

        let (p, ..) = parse_connection_string(&cs("Server=host;User Id=admin;<PW>=p")).unwrap();
        assert_eq!(p.uid, "admin");
    }

    #[test]
    fn tokenizer_edge_cases() {
        let (p, ..) = parse_connection_string("").unwrap();
        assert_eq!(p.server, "");

        let (p, ..) = parse_connection_string(&cs("Server=host;Database=;UID=u;<PW>=p")).unwrap();
        assert_eq!(p.database, "");

        let (p, ..) = parse_connection_string(&cs("Server=host;UID=u;<PW>=p;")).unwrap();
        assert_eq!(p.server, "host");

        let (p, ..) = parse_connection_string(&cs("Server=host;;;UID=u;;<PW>=p")).unwrap();
        assert_eq!(p.uid, "u");

        let (p, ..) =
            parse_connection_string(&cs(" Server = host ; UID = user ; <PW> = pass ")).unwrap();
        assert_eq!(p.server, "host");
        assert_eq!(p.uid, "user");
        assert_eq!(p.pwd, "pass");
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
        // No '=' separator
        let (p, warn) = parse_connection_string(&cs("Server=h;bogus;UID=u;<PW>=p")).unwrap();
        assert!(warn);
        assert_eq!(p.server, "h");
        assert_eq!(p.uid, "u");

        // Empty key (=value with no key)
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
    fn authentication_empty_is_reset_ok() {
        // Empty Authentication is an intentional reset (mssql-tds treats it as
        // recognized); stored as Some("") to preserve the distinction from unset.
        let (p, warn) = parse_connection_string("Server=h;UID=u;PWD=p;Authentication=").unwrap();
        assert_eq!(p.authentication.as_deref(), Some(""));
        assert!(!warn);
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
}
