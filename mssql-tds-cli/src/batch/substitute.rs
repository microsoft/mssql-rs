// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! `$(var)` expansion.
//!
//! Substitution is deliberately blind to syntax: the reference expands
//! references inside string literals and comments too, which is what makes
//! `$(var)` usable to build up identifiers and literals.
//!
//! An undefined reference is a warning rather than an error. The reference
//! leaves the text as it stands and sends the batch anyway, so the server is
//! what ultimately complains.

use crate::vars::Variables;

/// Guards against `:setvar A $(A)`-style loops.
const MAX_DEPTH: usize = 32;

/// The expanded text, together with any names that had no value.
pub struct Expansion {
    pub text: String,
    pub undefined: Vec<String>,
}

pub fn expand(text: &str, vars: &Variables) -> Expansion {
    let mut undefined = Vec::new();
    let text = expand_at(text, vars, 0, &mut undefined);
    Expansion { text, undefined }
}

fn expand_at(text: &str, vars: &Variables, depth: usize, undefined: &mut Vec<String>) -> String {
    if depth >= MAX_DEPTH || !text.contains("$(") {
        return text.to_string();
    }

    let mut out = String::with_capacity(text.len());
    let bytes = text.as_bytes();
    let mut i = 0;

    while i < bytes.len() {
        if bytes[i] == b'$'
            && bytes.get(i + 1) == Some(&b'(')
            && let Some(close) = find_close(text, i + 2)
        {
            let name = &text[i + 2..close];
            if is_valid_reference(name) {
                match vars.get(name) {
                    Some(value) => {
                        // A value may itself contain a reference.
                        out.push_str(&expand_at(value, vars, depth + 1, undefined));
                    }
                    None => {
                        if !undefined.iter().any(|seen| seen == name) {
                            undefined.push(name.to_string());
                        }
                        // Left as written, which is what the reference sends.
                        out.push_str(&text[i..=close]);
                    }
                }
                i = close + 1;
                continue;
            }
        }
        // Not a reference — copy one character and move on.
        let ch = text[i..].chars().next().unwrap_or('\0');
        out.push(ch);
        i += ch.len_utf8();
    }

    out
}

fn find_close(text: &str, from: usize) -> Option<usize> {
    text[from..].find(')').map(|offset| from + offset)
}

/// An empty name, or one holding characters a variable name cannot contain,
/// means the `$(` was incidental text rather than a reference.
fn is_valid_reference(name: &str) -> bool {
    !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}
