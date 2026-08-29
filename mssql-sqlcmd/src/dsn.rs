// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! `-D` data source names.
//!
//! A DSN is pure configuration, so it is read directly rather than by linking
//! an ODBC driver manager: `odbc.ini` on unix, the ODBC registry keys on
//! Windows. Only the keys that map onto a connection are understood; the rest
//! are ignored the way an unknown attribute would be.

use crate::cli::validate::Options;

/// The settings a DSN can contribute. Anything left `None` keeps whatever the
/// command line said.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Dsn {
    pub server: Option<String>,
    pub database: Option<String>,
    pub user: Option<String>,
    pub password: Option<String>,
    pub trusted_connection: Option<bool>,
    pub trust_server_certificate: Option<bool>,
    pub encrypt: Option<String>,
    pub application_intent: Option<String>,
    pub multi_subnet_failover: Option<bool>,
    pub host_name_in_certificate: Option<String>,
}

impl Dsn {
    /// Command-line options win over the DSN, matching ODBC's own precedence.
    pub fn apply_to(self, options: &mut Options) {
        fill(&mut options.server, self.server);
        fill(&mut options.database, self.database);
        fill(&mut options.user, self.user);
        fill(&mut options.password, self.password);
        fill(
            &mut options.host_name_in_certificate,
            self.host_name_in_certificate,
        );
        fill(&mut options.application_intent, self.application_intent);

        if let Some(true) = self.trusted_connection {
            options.trusted_connection = true;
        }
        if let Some(true) = self.trust_server_certificate {
            options.trust_server_certificate = true;
        }
        if let Some(true) = self.multi_subnet_failover {
            options.multi_subnet_failover = true;
        }
        if options.encrypt.is_none()
            && let Some(value) = self.encrypt.as_deref()
        {
            options.encrypt = encrypt_from(value);
        }
    }
}

fn fill(target: &mut Option<String>, value: Option<String>) {
    if target.is_none() {
        *target = value;
    }
}

/// `Encrypt` in a DSN is spelled as a word rather than as `-N`'s suffix letter.
fn encrypt_from(value: &str) -> Option<crate::cli::validate::Encrypt> {
    use crate::cli::validate::Encrypt;
    match value.trim().to_ascii_lowercase().as_str() {
        "strict" => Some(Encrypt::Strict),
        "yes" | "true" | "mandatory" | "1" => Some(Encrypt::Mandatory),
        "no" | "false" | "optional" | "0" => Some(Encrypt::Optional),
        _ => None,
    }
}

/// Reads a named DSN, returning `None` when it cannot be found.
pub fn load(name: &str) -> Option<Dsn> {
    #[cfg(windows)]
    {
        registry::load(name)
    }
    #[cfg(not(windows))]
    {
        unix::load(name)
    }
}

/// Turns a flat list of `key = value` pairs into a [`Dsn`].
fn from_pairs(pairs: impl IntoIterator<Item = (String, String)>) -> Dsn {
    let mut dsn = Dsn::default();
    for (key, value) in pairs {
        let value = value.trim().to_string();
        match key.trim().to_ascii_lowercase().as_str() {
            "server" | "servername" | "address" => dsn.server = Some(value),
            "database" => dsn.database = Some(value),
            "uid" | "user" | "username" | "logonid" => dsn.user = Some(value),
            "pwd" | "password" => dsn.password = Some(value),
            "trusted_connection" => dsn.trusted_connection = Some(is_yes(&value)),
            "trustservercertificate" => dsn.trust_server_certificate = Some(is_yes(&value)),
            "encrypt" => dsn.encrypt = Some(value),
            "applicationintent" => dsn.application_intent = Some(value),
            "multisubnetfailover" => dsn.multi_subnet_failover = Some(is_yes(&value)),
            "hostnameincertificate" => dsn.host_name_in_certificate = Some(value),
            _ => {}
        }
    }
    dsn
}

fn is_yes(value: &str) -> bool {
    matches!(
        value.trim().to_ascii_lowercase().as_str(),
        "yes" | "true" | "1"
    )
}

#[cfg(not(windows))]
mod unix {
    use super::{Dsn, from_pairs};

    /// A user's own `odbc.ini` takes precedence over the system one.
    pub fn load(name: &str) -> Option<Dsn> {
        let mut candidates = Vec::new();
        if let Ok(home) = std::env::var("HOME") {
            candidates.push(format!("{home}/.odbc.ini"));
        }
        if let Ok(explicit) = std::env::var("ODBCINI") {
            candidates.insert(0, explicit);
        }
        candidates.push("/etc/odbc.ini".to_string());

        for path in candidates {
            if let Ok(text) = std::fs::read_to_string(&path)
                && let Some(pairs) = section(&text, name)
            {
                return Some(from_pairs(pairs));
            }
        }
        None
    }

    /// Extracts one `[section]` from an ini file.
    pub(super) fn section(text: &str, name: &str) -> Option<Vec<(String, String)>> {
        let mut inside = false;
        let mut pairs = Vec::new();
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with(['#', ';']) {
                continue;
            }
            if let Some(header) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
                if inside {
                    break;
                }
                inside = header.trim().eq_ignore_ascii_case(name);
                continue;
            }
            if inside && let Some((key, value)) = line.split_once('=') {
                pairs.push((key.trim().to_string(), value.trim().to_string()));
            }
        }
        inside.then_some(pairs)
    }
}

#[cfg(windows)]
mod registry {
    use super::{Dsn, from_pairs};

    /// Reads `Software\ODBC\ODBC.INI\<name>`, user hive first.
    pub fn load(name: &str) -> Option<Dsn> {
        for root in ["HKCU", "HKLM"] {
            if let Some(pairs) = read_key(root, name) {
                return Some(from_pairs(pairs));
            }
        }
        None
    }

    /// `reg.exe` is used rather than a registry crate to keep the dependency
    /// list short; DSN lookup happens once at startup, so the process cost is
    /// not worth a new dependency.
    fn read_key(root: &str, name: &str) -> Option<Vec<(String, String)>> {
        let path = format!(r"{root}\Software\ODBC\ODBC.INI\{name}");
        let output = std::process::Command::new("reg")
            .args(["query", &path])
            .output()
            .ok()?;
        if !output.status.success() {
            return None;
        }
        Some(parse_reg_query(&String::from_utf8_lossy(&output.stdout)))
    }

    /// `reg query` prints `    Name    REG_SZ    Value` per value.
    pub(super) fn parse_reg_query(text: &str) -> Vec<(String, String)> {
        let mut pairs = Vec::new();
        for line in text.lines() {
            let trimmed = line.trim();
            if trimmed.is_empty() || trimmed.starts_with("HKEY_") {
                continue;
            }
            let mut parts = trimmed.splitn(3, "    ").map(str::trim);
            if let (Some(name), Some(kind), Some(value)) =
                (parts.next(), parts.next(), parts.next())
                && kind.starts_with("REG_")
            {
                pairs.push((name.to_string(), value.to_string()));
            }
        }
        pairs
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_keys_are_mapped_and_unknown_ones_ignored() {
        let dsn = from_pairs([
            ("Server".to_string(), "myhost".to_string()),
            ("Database".to_string(), "mydb".to_string()),
            ("UID".to_string(), "sa".to_string()),
            ("Trusted_Connection".to_string(), "Yes".to_string()),
            ("SomethingElse".to_string(), "ignored".to_string()),
        ]);
        assert_eq!(dsn.server.as_deref(), Some("myhost"));
        assert_eq!(dsn.database.as_deref(), Some("mydb"));
        assert_eq!(dsn.user.as_deref(), Some("sa"));
        assert_eq!(dsn.trusted_connection, Some(true));
    }

    #[test]
    fn key_names_are_matched_without_regard_to_case() {
        let dsn = from_pairs([("SERVER".to_string(), "h".to_string())]);
        assert_eq!(dsn.server.as_deref(), Some("h"));
    }

    #[test]
    fn the_command_line_wins_over_the_dsn() {
        let mut options = Options {
            server: Some("from-cli".to_string()),
            ..Options::default()
        };
        let dsn = Dsn {
            server: Some("from-dsn".to_string()),
            database: Some("from-dsn".to_string()),
            ..Dsn::default()
        };
        dsn.apply_to(&mut options);
        assert_eq!(options.server.as_deref(), Some("from-cli"));
        assert_eq!(options.database.as_deref(), Some("from-dsn"));
    }

    #[test]
    fn encrypt_accepts_the_word_forms_a_dsn_uses() {
        use crate::cli::validate::Encrypt;
        assert_eq!(encrypt_from("strict"), Some(Encrypt::Strict));
        assert_eq!(encrypt_from("Yes"), Some(Encrypt::Mandatory));
        assert_eq!(encrypt_from("no"), Some(Encrypt::Optional));
        assert_eq!(encrypt_from("nonsense"), None);
    }

    #[cfg(not(windows))]
    #[test]
    fn an_ini_section_stops_at_the_next_header() {
        let text = "[one]\nServer = a\n\n[two]\nServer = b\n";
        let pairs = unix::section(text, "one").unwrap();
        assert_eq!(pairs, vec![("Server".to_string(), "a".to_string())]);
        assert!(unix::section(text, "missing").is_none());
    }

    #[cfg(not(windows))]
    #[test]
    fn ini_comments_are_skipped() {
        let text = "[one]\n# a comment\n; another\nServer = a\n";
        let pairs = unix::section(text, "one").unwrap();
        assert_eq!(pairs, vec![("Server".to_string(), "a".to_string())]);
    }

    #[cfg(windows)]
    #[test]
    fn reg_query_output_is_parsed_into_pairs() {
        // `reg.exe` always emits CRLF, whatever the platform terminator is.
        let text = "\r\nHKEY_CURRENT_USER\\Software\\ODBC\\ODBC.INI\\mydsn\r\n\
                        Server    REG_SZ    myhost\r\n\
                    Database    REG_SZ    mydb\r\n";
        let pairs = registry::parse_reg_query(text);
        assert_eq!(
            pairs,
            vec![
                ("Server".to_string(), "myhost".to_string()),
                ("Database".to_string(), "mydb".to_string()),
            ]
        );
    }
}
