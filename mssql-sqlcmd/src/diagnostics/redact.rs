// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! What a report may show: diagnose output is for review before sharing; it
//! is never share-safe.
//!
//! Server, instance and host names, IP addresses, named pipe paths and
//! Kerberos SPNs identify the endpoint and are shown as they are; every report
//! begins with a warning that it may hold them. Database and user names are
//! pseudonymized by default: they become opaque labels such as `database-1`.
//! A label stands for one value throughout one run, so equal values stay
//! visibly equal, but labels mean nothing across runs. Free text (error
//! messages) gets the same labels wherever a known database or user name
//! appears as a whole word; anything else in it is flagged for review rather
//! than trusted. With local detail requested, database and user names are
//! shown as they are too.
//!
//! Certificate subject or SAN names and file paths would be pseudonymized the
//! same way; no check reports one. (`-F`, the host name the server certificate
//! must carry, names the endpoint and is shown as it is.)

use std::collections::HashMap;

/// The kind of identifier a value is.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Category {
    // Endpoint identifiers, shown as they are.
    Server,
    Host,
    Instance,
    Address,
    /// A Kerberos service principal name, which holds a host name.
    Spn,
    /// A named pipe path, which holds a host name.
    Pipe,
    // Pseudonymized unless local detail is requested.
    Database,
    User,
}

impl Category {
    /// The label's prefix, for a category that is pseudonymized.
    fn label(self) -> Option<&'static str> {
        match self {
            Category::Server
            | Category::Host
            | Category::Instance
            | Category::Address
            | Category::Spn
            | Category::Pipe => None,
            Category::Database => Some("database"),
            Category::User => Some("user"),
        }
    }
}

/// Maps the identifiers of one run to their labels.
#[derive(Debug, Default)]
pub struct Redactor {
    reveal: bool,
    /// (category, case-folded value) to label.
    labels: HashMap<(Category, String), String>,
    counts: HashMap<Category, usize>,
    /// Every value seen, with its label, for redacting free text.
    known: Vec<(String, String)>,
}

impl Redactor {
    /// `reveal`: show database and user names as they are (local detail).
    pub fn new(reveal: bool) -> Self {
        Self {
            reveal,
            ..Self::default()
        }
    }

    pub fn reveals(&self) -> bool {
        self.reveal
    }

    /// The value as it may be shown: an endpoint identifier as it is; a
    /// database or user name as its label, or as it is with local detail. An
    /// empty value is returned as is.
    pub fn name(&mut self, category: Category, value: &str) -> String {
        let Some(prefix) = category.label() else {
            return value.to_string();
        };
        if self.reveal || value.is_empty() {
            return value.to_string();
        }
        let key = (category, fold(value));
        if let Some(label) = self.labels.get(&key) {
            return label.clone();
        }
        let count = self.counts.entry(category).or_insert(0);
        *count += 1;
        let label = format!("{prefix}-{count}");
        self.labels.insert(key, label.clone());
        self.known.push((value.to_string(), label.clone()));
        label
    }

    /// Free text with every known database or user name, as a whole word,
    /// replaced by its label. One pass over the original text, so a label
    /// already put in is never matched again (a user `1` does not turn
    /// `database-1` into `database-user-1`). Where values overlap, the longest
    /// wins: `sales_eu` over `sales`. Endpoint identifiers are left as they are.
    pub fn text(&self, text: &str) -> String {
        if self.reveal {
            return text.to_string();
        }
        // A value with no letter or digit (`-S .`) names nothing in free
        // text and would match every `.`; its structured fields still get
        // their label.
        let mut known: Vec<&(String, String)> = self
            .known
            .iter()
            .filter(|(value, _)| value.chars().any(char::is_alphanumeric))
            .collect();
        known.sort_by_key(|(value, _)| std::cmp::Reverse(fold(value).chars().count()));
        let mut out = String::with_capacity(text.len());
        let mut at = 0;
        let mut previous: Option<char> = None;
        while let Some(next) = text[at..].chars().next() {
            let rest = &text[at..];
            if previous.is_none_or(|c| !is_word(c))
                && let Some((len, label)) = known
                    .iter()
                    .find_map(|(value, label)| word_at_start(rest, value).map(|len| (len, label)))
            {
                out.push_str(label);
                previous = rest[..len].chars().next_back();
                at += len;
                continue;
            }
            out.push(next);
            previous = Some(next);
            at += next.len_utf8();
        }
        out
    }
}

/// A word boundary is any character that is not alphanumeric or `_`, so `sa`
/// is a word in `user 'sa'` but not in `usage`, and `10.0.0.1` is not one in
/// `10.0.0.12`.
fn is_word(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// A character case-folded for matching names: lower-cased, with the final
/// sigma `ς` taken as `σ`, so every case form of a name compares equal even
/// where they differ in length (`ẞ` is 3 bytes, `ß` 2).
fn folded(c: char) -> impl Iterator<Item = char> {
    c.to_lowercase().map(|l| if l == 'ς' { 'σ' } else { l })
}

fn fold(value: &str) -> String {
    value.chars().flat_map(folded).collect()
}

/// The length in bytes of the start of `text` that is `value`,
/// case-insensitively, when a word boundary follows it. It is measured in
/// `text`, as a case form of `value` may differ from it in length.
fn word_at_start(text: &str, value: &str) -> Option<usize> {
    let target: Vec<char> = value.chars().flat_map(folded).collect();
    if target.is_empty() {
        return None;
    }
    let mut matched = 0;
    for (at, c) in text.char_indices() {
        for l in folded(c) {
            if target.get(matched) != Some(&l) {
                return None;
            }
            matched += 1;
        }
        if matched == target.len() {
            let end = at + c.len_utf8();
            return text[end..]
                .chars()
                .next()
                .is_none_or(|n| !is_word(n))
                .then_some(end);
        }
    }
    None
}
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn database_and_user_names_get_labels_stable_within_the_run() {
        let mut r = Redactor::new(false);
        assert_eq!(r.name(Category::Database, "Sales"), "database-1");
        assert_eq!(
            r.name(Category::Database, "SALES"),
            "database-1",
            "case-insensitive"
        );
        assert_eq!(r.name(Category::Database, "hr"), "database-2");
        assert_eq!(r.name(Category::User, "sa"), "user-1");
        assert_eq!(r.name(Category::Database, ""), "");
    }

    #[test]
    fn endpoint_identifiers_are_shown_as_they_are() {
        for reveal in [false, true] {
            let mut r = Redactor::new(reveal);
            for (category, value) in [
                (Category::Server, "tcp:db01.contoso.com,1433"),
                (Category::Host, "db01.contoso.com"),
                (Category::Instance, "SQLEXPRESS"),
                (Category::Address, "10.0.0.1"),
                (Category::Address, "fe80::1"),
                (Category::Pipe, r"\\db01\pipe\sql\query"),
                (Category::Spn, "MSSQLSvc/db01.contoso.com:1433"),
            ] {
                assert_eq!(r.name(category, value), value);
            }
            assert_eq!(
                r.text("TCP to db01.contoso.com (10.0.0.1) failed."),
                "TCP to db01.contoso.com (10.0.0.1) failed."
            );
        }
    }

    #[test]
    fn local_detail_shows_database_and_user_names() {
        let mut r = Redactor::new(true);
        assert_eq!(r.name(Category::Database, "Sales"), "Sales");
        assert_eq!(r.name(Category::User, "sa"), "sa");
        assert_eq!(
            r.text("Login failed for user 'sa'."),
            "Login failed for user 'sa'."
        );
    }

    #[test]
    fn known_names_are_replaced_in_text_as_whole_words() {
        let mut r = Redactor::new(false);
        r.name(Category::User, "sa");
        r.name(Category::Database, "sales");
        r.name(Category::Database, "sales_eu");
        r.name(Category::Host, "db01");
        assert_eq!(
            r.text("Login failed for user 'SA'. Usage of sales_eu and sales on db01 (salesman)"),
            "Login failed for user 'user-1'. Usage of database-2 and database-1 on db01 (salesman)"
        );
    }

    #[test]
    fn redacting_preserves_unrelated_unicode() {
        let mut r = Redactor::new(false);
        r.name(Category::Database, "Straße");
        assert_eq!(r.text("database \"Straße\" é"), "database \"database-1\" é");
    }

    /// Case forms that differ in length (`ẞ` is 3 bytes, `ß` 2; the Greek
    /// final sigma) get the same label and are replaced in text either way.
    #[test]
    fn case_forms_of_a_different_length_are_the_same_name() {
        let mut r = Redactor::new(false);
        assert_eq!(r.name(Category::User, "ẞ"), "user-1");
        assert_eq!(r.name(Category::User, "ß"), "user-1");
        assert_eq!(
            r.text("Login failed for user 'ß'."),
            "Login failed for user 'user-1'."
        );

        let mut r = Redactor::new(false);
        assert_eq!(r.name(Category::User, "ß"), "user-1");
        assert_eq!(
            r.text("Login failed for user 'ẞ'. ẞx stays."),
            "Login failed for user 'user-1'. ẞx stays."
        );

        let mut r = Redactor::new(false);
        assert_eq!(r.name(Category::Database, "ΟΔΟΣ"), "database-1");
        assert_eq!(r.name(Category::Database, "οδος"), "database-1");
        assert_eq!(r.text("οδος and ΟΔΟΣ"), "database-1 and database-1");
    }

    #[test]
    fn a_label_already_put_in_is_not_matched_again() {
        let mut r = Redactor::new(false);
        assert_eq!(r.name(Category::Database, "db01"), "database-1");
        assert_eq!(r.name(Category::User, "1"), "user-1");
        assert_eq!(
            r.text("login to db01 failed for 1"),
            "login to database-1 failed for user-1"
        );
    }
    /// A database named `.` gets its label in structured fields, but names
    /// nothing in free text: the periods of a message stay periods.
    #[test]
    fn a_value_without_letters_or_digits_is_not_matched_in_text() {
        let mut r = Redactor::new(false);
        assert_eq!(r.name(Category::Database, "."), "database-1");
        assert_eq!(r.name(Category::User, "dba"), "user-1");
        // A period after `)` or a space is at a word boundary on both sides.
        assert_eq!(
            r.text("Login failed (error 18456). dba refused . (os error 10061)."),
            "Login failed (error 18456). user-1 refused . (os error 10061)."
        );
    }
}
