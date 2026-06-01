// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Connection string parsing for ODBC `SQLDriverConnect`.
//!
//! Parses `;`-delimited `Key=Value` connection strings per the ODBC spec.
//! Supports `{braced values}` for values containing `;` or `=`.

use std::fmt;

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
    "trusted_connection",
    "autotranslate",
    "quotedid",
    "ansi_npw",
    "regional",
];

// Valid attribute values
const YES_NO: &[&str] = &["yes", "no"];
const ENCRYPT_VALUES: &[&str] = &["yes", "mandatory", "no", "optional", "strict"];

#[derive(Copy, Clone)]
enum ConnAttrKey {
    Server,
    Database,
    Uid,
    Pwd,
    TrustServerCert,
    Encrypt,
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

    #[test]
    fn basic_connection_string() {
        let (params, has_warnings) = parse_connection_string(
            "Server=localhost,1434;Database=master;UID=sa;PWD=secret;TrustServerCertificate=yes",
        )
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
        let (p, ..) = parse_connection_string("Server=myhost;UID=u;PWD=p").unwrap();
        assert_eq!(p.server, "myhost");

        let (p, ..) = parse_connection_string("Server=tcp:myhost,2000;UID=u;PWD=p").unwrap();
        assert_eq!(p.server, "tcp:myhost,2000");

        let (p, ..) = parse_connection_string("Server=::1;UID=u;PWD=p").unwrap();
        assert_eq!(p.server, "::1");

        let (p, ..) = parse_connection_string("Server=host,abc;UID=u;PWD=p").unwrap();
        assert_eq!(p.server, "host,abc");
    }

    #[test]
    fn braced_values() {
        let (p, ..) =
            parse_connection_string("Server=host;PWD={pass;with=special};UID=user").unwrap();
        assert_eq!(p.pwd, "pass;with=special");
        assert_eq!(p.uid, "user");

        let (p, ..) = parse_connection_string("Server=h;PWD={a=b;c=d};UID=u").unwrap();
        assert_eq!(p.pwd, "a=b;c=d");
        assert_eq!(p.uid, "u");
    }

    #[test]
    fn key_aliases() {
        let (p, ..) = parse_connection_string(
            "SERVER=host;uid=user;PASSWORD=pass;trustservercertificate=Yes",
        )
        .unwrap();
        assert_eq!(p.server, "host");
        assert_eq!(p.uid, "user");
        assert_eq!(p.pwd, "pass");
        assert!(p.trust_server_certificate);

        let (p, ..) =
            parse_connection_string("Server=host;Initial Catalog=mydb;UID=u;PWD=p").unwrap();
        assert_eq!(p.database, "mydb");

        let (p, ..) = parse_connection_string("Server=host;User Id=admin;PWD=p").unwrap();
        assert_eq!(p.uid, "admin");
    }

    #[test]
    fn tokenizer_edge_cases() {
        let (p, ..) = parse_connection_string("").unwrap();
        assert_eq!(p.server, "");

        let (p, ..) = parse_connection_string("Server=host;Database=;UID=u;PWD=p").unwrap();
        assert_eq!(p.database, "");

        let (p, ..) = parse_connection_string("Server=host;UID=u;PWD=p;").unwrap();
        assert_eq!(p.server, "host");

        let (p, ..) = parse_connection_string("Server=host;;;UID=u;;PWD=p").unwrap();
        assert_eq!(p.uid, "u");

        let (p, ..) = parse_connection_string(" Server = host ; UID = user ; PWD = pass ").unwrap();
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
                parse_connection_string(&format!("Server=h;UID=u;PWD=p;Encrypt={input}")).unwrap();
            assert_eq!(p.encrypt.as_deref(), Some(expected));
        }
    }

    #[test]
    fn duplicates_follow_first_wins() {
        let (p, ..) = parse_connection_string("Server=first;Server=second;UID=u;PWD=p").unwrap();
        assert_eq!(p.server, "first");

        let (p, ..) = parse_connection_string("UID=first;User Id=second;PWD=p;Server=h").unwrap();
        assert_eq!(p.uid, "first");

        let (p, ..) =
            parse_connection_string("Server=h;UID=u;PWD=p;Encrypt=yes;Encrypt=banana").unwrap();
        assert_eq!(p.encrypt.as_deref(), Some("yes"));
    }

    #[test]
    fn invalid_attr_values() {
        let err = parse_connection_string("Server=h;UID=u;PWD=p;Encrypt=true").unwrap_err();
        assert_eq!(err.key, "encrypt");
        assert_eq!(err.value, "true");

        let err =
            parse_connection_string("Server=h;UID=u;PWD=p;TrustServerCertificate=1").unwrap_err();
        assert_eq!(err.key, "trustservercertificate");
        assert_eq!(err.value, "1");
    }

    #[test]
    fn malformed_tokens_set_warning() {
        // No '=' separator
        let (p, warn) = parse_connection_string("Server=h;bogus;UID=u;PWD=p").unwrap();
        assert!(warn);
        assert_eq!(p.server, "h");
        assert_eq!(p.uid, "u");

        // Empty key (=value with no key)
        let (p, warn) = parse_connection_string("Server=h;=orphan;UID=u;PWD=p").unwrap();
        assert!(warn);
        assert_eq!(p.uid, "u");

        // Both in one string
        let (_, warn) = parse_connection_string("noequals;=empty;Server=h;UID=u;PWD=p").unwrap();
        assert!(warn);

        // Clean string has no warnings
        let (_, warn) = parse_connection_string("Server=h;UID=u;PWD=p").unwrap();
        assert!(!warn);

        // Unknown but well-formed keys are ignored with warning.
        let (_, warn) = parse_connection_string("Server=h;UID=u;PWD=p;FooBar=1").unwrap();
        assert!(warn);

        // Known-but-ignored keys should not warn.
        let (_, warn) =
            parse_connection_string("Driver={ODBC Driver 18 for SQL Server};Server=h;UID=u;PWD=p")
                .unwrap();
        assert!(!warn);
    }

    #[test]
    fn unterminated_brace_sets_warning() {
        // Missing closing '}' — consumes rest of string as value, warns
        let (p, warn) = parse_connection_string("Server=h;PWD={abc;UID=u").unwrap();
        assert!(warn);
        assert_eq!(p.pwd, "abc;UID=u");
        assert_eq!(p.uid, ""); // UID was swallowed into the braced value
    }

    #[test]
    fn debug_redacts_password() {
        let (p, _) = parse_connection_string("Server=h;UID=u;PWD=secret123").unwrap();
        let debug_str = format!("{p:?}");
        assert!(debug_str.contains("<REDACTED>"));
        assert!(!debug_str.contains("secret123"));
    }

    #[test]
    fn fmt_as_odbc_conn_str_redacts_password() {
        let (p, _) =
            parse_connection_string("Server=h;Database=db;UID=u;PWD=secret;Encrypt=strict")
                .unwrap();
        assert_eq!(
            p.fmt_as_odbc_conn_str(),
            "Server=h;Database=db;UID=u;PWD=******;Encrypt=strict"
        );
    }
}
