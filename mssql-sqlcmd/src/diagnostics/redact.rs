// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Share-safe output: identifiers are pseudonymized by default.
//!
//! Server, instance, database, host, IP address and user identifiers become
//! opaque labels such as `host-1`. A label stands for one value throughout one
//! run, so equal values stay visibly equal, but labels mean nothing across
//! runs. Free text (error messages) gets the same labels wherever a known value
//! appears as a whole word; anything else in it is flagged for review rather
//! than trusted. With local detail requested, values are shown as they are and
//! the output is marked as not share-safe.

use std::collections::HashMap;

/// The kind of identifier a value is.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Category {
    Server,
    Host,
    Instance,
    Address,
    Database,
    User,
    /// A Kerberos service principal name, which holds a host name.
    Spn,
    /// A named pipe path, which holds a host name.
    Pipe,
}

impl Category {
    fn label(self) -> &'static str {
        match self {
            Category::Server => "server",
            Category::Host => "host",
            Category::Instance => "instance",
            Category::Address => "address",
            Category::Database => "database",
            Category::User => "user",
            Category::Spn => "spn",
            Category::Pipe => "pipe",
        }
    }
}

/// Maps the identifiers of one run to their labels.
#[derive(Debug, Default)]
pub struct Redactor {
    reveal: bool,
    /// (category, lower-cased value) to label.
    labels: HashMap<(Category, String), String>,
    counts: HashMap<Category, usize>,
    /// Every value seen, with its label, for redacting free text.
    known: Vec<(String, String)>,
}

impl Redactor {
    /// `reveal`: show values as they are (local detail).
    pub fn new(reveal: bool) -> Self {
        Self {
            reveal,
            ..Self::default()
        }
    }

    pub fn reveals(&self) -> bool {
        self.reveal
    }

    /// The value as it may be shown: its label, or itself with local detail.
    /// An empty value is returned as is.
    pub fn name(&mut self, category: Category, value: &str) -> String {
        if self.reveal || value.is_empty() {
            return value.to_string();
        }
        let key = (category, value.to_lowercase());
        if let Some(label) = self.labels.get(&key) {
            return label.clone();
        }
        let count = self.counts.entry(category).or_insert(0);
        *count += 1;
        let label = format!("{}-{}", category.label(), count);
        self.labels.insert(key, label.clone());
        self.known.push((value.to_string(), label.clone()));
        label
    }

    /// Free text with every known value, as a whole word, replaced by its label.
    /// One pass over the original text, so a label already put in is never
    /// matched again (a user `1` does not turn `host-1` into `host-user-1`).
    /// Where values overlap, the longest wins: `db01.contoso.com` over `db01`.
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
        known.sort_by_key(|(value, _)| std::cmp::Reverse(value.len()));
        let mut out = String::with_capacity(text.len());
        let mut at = 0;
        let mut previous: Option<char> = None;
        while let Some(next) = text[at..].chars().next() {
            let rest = &text[at..];
            if previous.is_none_or(|c| !is_word(c))
                && let Some((value, label)) =
                    known.iter().find(|(value, _)| word_at_start(rest, value))
            {
                out.push_str(label);
                previous = rest[..value.len()].chars().next_back();
                at += value.len();
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

/// Whether `text` starts with `value`, case-insensitively, followed by a word
/// boundary.
fn word_at_start(text: &str, value: &str) -> bool {
    text.len() >= value.len()
        && text.is_char_boundary(value.len())
        && text[..value.len()].to_lowercase() == value.to_lowercase()
        && text[value.len()..]
            .chars()
            .next()
            .is_none_or(|c| !is_word(c))
}
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn values_get_labels_per_category_stable_within_the_run() {
        let mut r = Redactor::new(false);
        assert_eq!(r.name(Category::Host, "db01.contoso.com"), "host-1");
        assert_eq!(
            r.name(Category::Host, "DB01.contoso.com"),
            "host-1",
            "case-insensitive"
        );
        assert_eq!(r.name(Category::Host, "db02"), "host-2");
        assert_eq!(r.name(Category::Address, "10.0.0.1"), "address-1");
        assert_eq!(r.name(Category::User, "sa"), "user-1");
        assert_eq!(r.name(Category::Database, ""), "");
    }

    #[test]
    fn local_detail_shows_values() {
        let mut r = Redactor::new(true);
        assert_eq!(r.name(Category::Host, "db01"), "db01");
        assert_eq!(
            r.text("Login failed for user 'sa'."),
            "Login failed for user 'sa'."
        );
    }

    #[test]
    fn known_values_are_replaced_in_text_as_whole_words() {
        let mut r = Redactor::new(false);
        r.name(Category::User, "sa");
        r.name(Category::Address, "10.0.0.1");
        r.name(Category::Host, "db01");
        r.name(Category::Host, "db01.contoso.com");
        assert_eq!(
            r.text("Login failed for user 'SA'. Usage of 10.0.0.12 and 10.0.0.1:1433 on db01.contoso.com (db01)"),
            "Login failed for user 'user-1'. Usage of 10.0.0.12 and address-1:1433 on host-2 (host-1)"
        );
    }

    #[test]
    fn redacting_preserves_unrelated_unicode() {
        let mut r = Redactor::new(false);
        r.name(Category::Database, "Straße");
        assert_eq!(r.text("database \"Straße\" é"), "database \"database-1\" é");
    }
    #[test]
    fn a_label_already_put_in_is_not_matched_again() {
        let mut r = Redactor::new(false);
        assert_eq!(r.name(Category::Host, "db01"), "host-1");
        assert_eq!(r.name(Category::User, "1"), "user-1");
        assert_eq!(
            r.text("login to db01 failed for 1"),
            "login to host-1 failed for user-1"
        );
    }
    /// `-S .` gets its label in structured fields, but names nothing in free
    /// text: the periods of a message stay periods.
    #[test]
    fn a_value_without_letters_or_digits_is_not_matched_in_text() {
        let mut r = Redactor::new(false);
        assert_eq!(r.name(Category::Server, "."), "server-1");
        assert_eq!(r.name(Category::Host, "db01"), "host-1");
        // A period after `)` or a space is at a word boundary on both sides.
        assert_eq!(
            r.text("Login failed (error 18456). db01 refused . (os error 10061)."),
            "Login failed (error 18456). host-1 refused . (os error 10061)."
        );
    }
}
