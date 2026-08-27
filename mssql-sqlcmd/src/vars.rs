// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Scripting variables.
//!
//! Names are matched case-insensitively and stored upper-cased, which is what
//! `:listvar` shows. The built-in defaults here were read out of the shipped
//! binary with `:listvar` rather than taken from documentation.

use std::collections::BTreeMap;

use crate::compat::Compat;

/// Built-ins the user is not allowed to reassign, as reported by the reference
/// when `:setvar` is attempted against each of them in turn.
const READ_ONLY: &[&str] = &[
    "SQLCMDDBNAME",
    "SQLCMDINI",
    "SQLCMDPACKETSIZE",
    "SQLCMDSERVER",
    "SQLCMDUSER",
    "SQLCMDWORKSTATION",
];

/// Built-ins seeded from the environment at startup, unless `-X` says otherwise.
const FROM_ENVIRONMENT: &[&str] = &[
    "SQLCMDUSER",
    "SQLCMDPASSWORD",
    "SQLCMDSERVER",
    "SQLCMDWORKSTATION",
    "SQLCMDDBNAME",
    "SQLCMDLOGINTIMEOUT",
    "SQLCMDSTATTIMEOUT",
    "SQLCMDHEADERS",
    "SQLCMDCOLSEP",
    "SQLCMDCOLWIDTH",
    "SQLCMDPACKETSIZE",
    "SQLCMDERRORLEVEL",
    "SQLCMDMAXVARTYPEWIDTH",
    "SQLCMDMAXFIXEDTYPEWIDTH",
    "SQLCMDEDITOR",
    "SQLCMDINI",
    "SQLCMDUSEAAD",
    // Added by go-sqlcmd.
    "SQLCMDFORMAT",
    "SQLCMDCOLORSCHEME",
];

/// `osql` predates `sqlcmd` and its variables are still honoured as a fallback.
fn legacy_alias(name: &str) -> Option<&'static str> {
    match name {
        "SQLCMDUSER" => Some("OSQLUSER"),
        "SQLCMDPASSWORD" => Some("OSQLPASSWORD"),
        "SQLCMDSERVER" => Some("OSQLSERVER"),
        "SQLCMDWORKSTATION" => Some("OSQLWORKSTATION"),
        "SQLCMDDBNAME" => Some("OSQLDBNAME"),
        "SQLCMDLOGINTIMEOUT" => Some("OSQLLOGINTIMEOUT"),
        "SQLCMDHEADERS" => Some("OSQLHEADERS"),
        "SQLCMDCOLSEP" => Some("OSQLCOLSEP"),
        "SQLCMDCOLWIDTH" => Some("OSQLCOLWIDTH"),
        "SQLCMDPACKETSIZE" => Some("OSQLPACKETSIZE"),
        _ => None,
    }
}

/// Refusal reason from [`Variables::set`].
#[derive(Debug, PartialEq, Eq)]
pub enum SetError {
    /// The name is one of the read-only built-ins.
    ReadOnly,
    /// The name is empty, starts with a digit, or contains a forbidden character.
    InvalidName,
}

#[derive(Debug, Default, Clone)]
pub struct Variables {
    values: BTreeMap<String, String>,
    /// Built-ins that `:listvar` prints after the alphabetical run rather than
    /// within it.
    trailing: Vec<String>,
}

/// The reference accepts letters, digits, underscore and hyphen, and will not
/// take a name that starts with a digit.
fn is_valid_name(name: &str) -> bool {
    let mut chars = name.chars();
    match chars.next() {
        None => false,
        Some(first) if first.is_ascii_digit() => false,
        Some(first) if !is_name_char(first) => false,
        _ => chars.all(is_name_char),
    }
}

fn is_name_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_' || c == '-'
}

impl Variables {
    /// Built-in defaults only — no environment, no command line.
    ///
    /// `compat` picks the defaults apart where the two tools disagree:
    /// go-sqlcmd waits 30 seconds to log in rather than 8, offers `notepad.exe`
    /// as the editor rather than `edit.com`, and lists `SQLCMDFORMAT`, which
    /// ODBC has no notion of.
    pub fn with_defaults(workstation: &str, compat: Compat) -> Self {
        let mut vars = Variables::default();
        for (name, value) in [
            ("SQLCMDCOLSEP", " "),
            ("SQLCMDCOLWIDTH", "0"),
            ("SQLCMDDBNAME", ""),
            ("SQLCMDEDITOR", default_editor(compat)),
            ("SQLCMDERRORLEVEL", "0"),
            ("SQLCMDHEADERS", "0"),
            ("SQLCMDINI", ""),
            (
                "SQLCMDLOGINTIMEOUT",
                if compat.is_go() { "30" } else { "8" },
            ),
            ("SQLCMDMAXFIXEDTYPEWIDTH", "0"),
            ("SQLCMDMAXVARTYPEWIDTH", "256"),
            ("SQLCMDPACKETSIZE", "4096"),
            ("SQLCMDSERVER", ""),
            ("SQLCMDSTATTIMEOUT", "0"),
            ("SQLCMDUSER", ""),
            ("SQLCMDWORKSTATION", workstation),
        ] {
            vars.values.insert(name.to_string(), value.to_string());
        }
        if compat.is_go() {
            // go-sqlcmd lists three variables ODBC has no notion of. Their
            // order in `:listvar` is not alphabetical: `SQLCMDUSEAAD` sorts in
            // with the rest, but `SQLCMDCOLORSCHEME` is appended after them.
            vars.values
                .insert("SQLCMDFORMAT".to_string(), String::new());
            vars.values
                .insert("SQLCMDUSEAAD".to_string(), String::new());
            vars.trailing.push("SQLCMDCOLORSCHEME".to_string());
            vars.values
                .insert("SQLCMDCOLORSCHEME".to_string(), String::new());
        }
        vars
    }

    /// Overlays `SQLCMD*` (and the older `OSQL*`) values found in the
    /// environment. Skipped entirely under `-X`.
    pub fn seed_from_environment(&mut self) {
        for name in FROM_ENVIRONMENT {
            let found = std::env::var(name)
                .ok()
                .or_else(|| legacy_alias(name).and_then(|alias| std::env::var(alias).ok()));
            if let Some(value) = found {
                self.values.insert((*name).to_string(), value);
            }
        }
    }

    pub fn is_read_only(name: &str) -> bool {
        READ_ONLY.contains(&name.to_ascii_uppercase().as_str())
    }

    pub fn get(&self, name: &str) -> Option<&str> {
        self.values
            .get(&name.to_ascii_uppercase())
            .map(String::as_str)
    }

    /// Reads a variable that should hold a number, falling back when it does not.
    pub fn get_int(&self, name: &str, fallback: i64) -> i64 {
        self.get(name)
            .and_then(|v| v.trim().parse::<i64>().ok())
            .unwrap_or(fallback)
    }

    /// Assignment on behalf of the user — honours the read-only set.
    pub fn set(&mut self, name: &str, value: &str) -> Result<(), SetError> {
        if !is_valid_name(name) {
            return Err(SetError::InvalidName);
        }
        if Self::is_read_only(name) {
            return Err(SetError::ReadOnly);
        }
        self.values
            .insert(name.to_ascii_uppercase(), value.to_string());
        Ok(())
    }

    /// Assignment on behalf of sqlcmd itself, which may write the read-only
    /// built-ins — `SQLCMDSERVER` after `:connect`, for instance.
    pub fn set_internal(&mut self, name: &str, value: &str) {
        self.values
            .insert(name.to_ascii_uppercase(), value.to_string());
    }

    pub fn remove(&mut self, name: &str) -> Result<(), SetError> {
        if Self::is_read_only(name) {
            return Err(SetError::ReadOnly);
        }
        self.values.remove(&name.to_ascii_uppercase());
        Ok(())
    }

    /// `:listvar` order — built-ins alphabetically, then any that sort late by
    /// convention rather than by name, then user variables.
    pub fn listing(&self) -> Vec<(&str, &str)> {
        let mut builtin: Vec<(&str, &str)> = Vec::new();
        let mut late: Vec<(&str, &str)> = Vec::new();
        let mut user: Vec<(&str, &str)> = Vec::new();
        for (name, value) in &self.values {
            if self.trailing.iter().any(|t| t == name) {
                late.push((name.as_str(), value.as_str()));
            } else if name.starts_with("SQLCMD") {
                builtin.push((name.as_str(), value.as_str()));
            } else {
                user.push((name.as_str(), value.as_str()));
            }
        }
        builtin.extend(late);
        builtin.extend(user);
        builtin
    }
}

/// The editor `:ed` hands the statement cache to.
///
/// ODBC names `edit.com` on every platform, including the ones where no such
/// program exists. go-sqlcmd picks one that does.
fn default_editor(compat: Compat) -> &'static str {
    if !compat.is_go() {
        "edit.com"
    } else if cfg!(windows) {
        "notepad.exe"
    } else {
        "vi"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_match_the_reference_listvar() {
        let vars = Variables::with_defaults("HOST", Compat::Odbc);
        assert_eq!(vars.get("SQLCMDCOLSEP"), Some(" "));
        assert_eq!(vars.get("SQLCMDLOGINTIMEOUT"), Some("8"));
        assert_eq!(vars.get("SQLCMDMAXVARTYPEWIDTH"), Some("256"));
        assert_eq!(vars.get("SQLCMDMAXFIXEDTYPEWIDTH"), Some("0"));
        assert_eq!(vars.get("SQLCMDPACKETSIZE"), Some("4096"));
        assert_eq!(vars.get("SQLCMDWORKSTATION"), Some("HOST"));
    }

    #[test]
    fn lookup_ignores_case_but_storage_is_upper() {
        let mut vars = Variables::default();
        vars.set("MyVar", "1").unwrap();
        assert_eq!(vars.get("myvar"), Some("1"));
        assert_eq!(vars.listing(), vec![("MYVAR", "1")]);
    }

    #[test]
    fn the_six_read_only_builtins_reject_assignment() {
        let mut vars = Variables::with_defaults("HOST", Compat::Odbc);
        for name in [
            "SQLCMDSERVER",
            "SQLCMDUSER",
            "SQLCMDDBNAME",
            "SQLCMDINI",
            "SQLCMDPACKETSIZE",
            "SQLCMDWORKSTATION",
        ] {
            assert_eq!(vars.set(name, "x"), Err(SetError::ReadOnly), "{name}");
        }
        // Everything else in the built-in set is writable.
        for name in [
            "SQLCMDLOGINTIMEOUT",
            "SQLCMDCOLSEP",
            "SQLCMDHEADERS",
            "SQLCMDERRORLEVEL",
            "SQLCMDEDITOR",
            "SQLCMDSTATTIMEOUT",
            "SQLCMDMAXVARTYPEWIDTH",
            "SQLCMDMAXFIXEDTYPEWIDTH",
            "SQLCMDCOLWIDTH",
        ] {
            assert!(vars.set(name, "1").is_ok(), "{name}");
        }
    }

    #[test]
    fn sqlcmd_itself_may_write_read_only_builtins() {
        let mut vars = Variables::with_defaults("HOST", Compat::Odbc);
        vars.set_internal("SQLCMDSERVER", "other");
        assert_eq!(vars.get("SQLCMDSERVER"), Some("other"));
    }

    #[test]
    fn names_starting_with_a_digit_are_rejected() {
        let mut vars = Variables::default();
        assert_eq!(vars.set("1abc", "x"), Err(SetError::InvalidName));
        assert_eq!(vars.set("", "x"), Err(SetError::InvalidName));
        assert_eq!(vars.set("has space", "x"), Err(SetError::InvalidName));
        assert!(vars.set("_ok", "x").is_ok());
        assert!(vars.set("a-b", "x").is_ok());
    }

    #[test]
    fn user_variables_are_listed_after_builtins() {
        let mut vars = Variables::with_defaults("HOST", Compat::Odbc);
        vars.set("FOO", "bar").unwrap();
        let listing = vars.listing();
        assert_eq!(listing.last(), Some(&("FOO", "bar")));
    }
}
