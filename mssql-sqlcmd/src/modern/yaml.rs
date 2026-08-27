// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! The little of YAML that `sqlconfig` needs.
//!
//! go-sqlcmd stores its configuration as YAML, so reading and writing it means
//! speaking enough of the format to round-trip that one schema: nested maps,
//! lists of maps, plain scalars and empty collections. That is a small enough
//! corner to implement directly, which is preferable to taking a dependency on
//! a general YAML crate for a file this shape.
//!
//! Anchors, tags, flow mappings, multi-line scalars and documents are not
//! supported and are reported as errors rather than being quietly mis-parsed.

use std::fmt::Write as _;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Yaml {
    Scalar(String),
    List(Vec<Yaml>),
    Map(Vec<(String, Yaml)>),
}

impl Yaml {
    pub fn scalar(value: impl Into<String>) -> Self {
        Yaml::Scalar(value.into())
    }

    pub fn get(&self, key: &str) -> Option<&Yaml> {
        match self {
            Yaml::Map(entries) => entries.iter().find(|(k, _)| k == key).map(|(_, v)| v),
            _ => None,
        }
    }

    pub fn as_str(&self) -> Option<&str> {
        match self {
            Yaml::Scalar(text) => Some(text),
            _ => None,
        }
    }

    pub fn as_list(&self) -> &[Yaml] {
        match self {
            Yaml::List(items) => items,
            _ => &[],
        }
    }

    /// The string at `key`, or empty if absent.
    pub fn str_at(&self, key: &str) -> &str {
        self.get(key).and_then(Yaml::as_str).unwrap_or_default()
    }
}

/// Renders a document. Top-level maps are written without leading indentation.
pub fn emit(value: &Yaml) -> String {
    let mut out = String::new();
    write_value(&mut out, value, 0, true);
    out
}

fn write_value(out: &mut String, value: &Yaml, indent: usize, top: bool) {
    match value {
        Yaml::Scalar(text) => {
            let _ = writeln!(out, "{}", quote(text));
        }
        Yaml::List(items) if items.is_empty() => {
            let _ = writeln!(out, "[]");
        }
        Yaml::List(items) => {
            if !top {
                let _ = writeln!(out);
            }
            for item in items {
                write_list_item(out, item, indent);
            }
        }
        Yaml::Map(entries) if entries.is_empty() => {
            let _ = writeln!(out, "{{}}");
        }
        Yaml::Map(entries) => {
            if !top {
                let _ = writeln!(out);
            }
            for (key, child) in entries {
                let _ = write!(out, "{:indent$}{key}:", "", indent = indent);
                write_child(out, child, indent + 2);
            }
        }
    }
}

/// A list item carries its `- ` marker on the first line, and the rest of a
/// nested collection lines up under it.
fn write_list_item(out: &mut String, item: &Yaml, indent: usize) {
    match item {
        Yaml::Scalar(text) => {
            let _ = writeln!(out, "{:indent$}- {}", "", quote(text), indent = indent);
        }
        Yaml::Map(entries) if entries.is_empty() => {
            let _ = writeln!(out, "{:indent$}- {{}}", "", indent = indent);
        }
        Yaml::Map(entries) => {
            for (position, (key, child)) in entries.iter().enumerate() {
                let lead = if position == 0 { "- " } else { "  " };
                let _ = write!(out, "{:indent$}{lead}{key}:", "", indent = indent);
                write_child(out, child, indent + 4);
            }
        }
        Yaml::List(_) => {
            let _ = writeln!(out, "{:indent$}-", "", indent = indent);
            write_value(out, item, indent + 2, false);
        }
    }
}

/// Scalars and empty collections stay on the key's line; populated ones start
/// on the next.
fn write_child(out: &mut String, child: &Yaml, indent: usize) {
    match child {
        Yaml::Scalar(text) => {
            let _ = writeln!(out, " {}", quote(text));
        }
        Yaml::List(items) if items.is_empty() => {
            let _ = writeln!(out, " []");
        }
        Yaml::Map(entries) if entries.is_empty() => {
            let _ = writeln!(out, " {{}}");
        }
        // A list under a key sits at the key's own indentation, not deeper.
        Yaml::List(items) => {
            let _ = writeln!(out);
            for item in items {
                write_list_item(out, item, indent.saturating_sub(2));
            }
        }
        Yaml::Map(_) => write_value(out, child, indent, false),
    }
}

/// Quotes only where a bare scalar would be ambiguous: an empty string, or one
/// that would otherwise read back as a different type.
fn quote(text: &str) -> String {
    let bare_is_safe = !text.is_empty()
        && !text.starts_with(' ')
        && !text.ends_with(' ')
        && !text.starts_with([
            '&', '*', '!', '%', '@', '`', '\'', '"', '[', '{', '-', '?', '>', '|',
        ])
        && !text.contains([':', '#', '\n', '\r', '\t'])
        && !matches!(
            text.to_ascii_lowercase().as_str(),
            "true" | "false" | "null" | "yes" | "no" | "on" | "off" | "~"
        );
    if bare_is_safe {
        text.to_string()
    } else {
        format!("\"{}\"", text.replace('\\', "\\\\").replace('"', "\\\""))
    }
}

#[derive(Debug)]
pub struct ParseError(pub String);

impl std::fmt::Display for ParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Parses a document. Only the subset `emit` produces is understood.
pub fn parse(text: &str) -> Result<Yaml, ParseError> {
    let lines: Vec<Line> = text
        .lines()
        .enumerate()
        .filter_map(|(number, raw)| Line::new(number + 1, raw))
        .collect();
    if lines.is_empty() {
        return Ok(Yaml::Map(Vec::new()));
    }
    let mut cursor = 0;
    let value = parse_block(&lines, &mut cursor, lines[0].indent)?;
    Ok(value)
}

struct Line {
    number: usize,
    indent: usize,
    content: String,
}

impl Line {
    fn new(number: usize, raw: &str) -> Option<Self> {
        let indent = raw.len() - raw.trim_start().len();
        let content = raw.trim();
        if content.is_empty() || content.starts_with('#') {
            return None;
        }
        Some(Line {
            number,
            indent,
            content: content.to_string(),
        })
    }
}

fn parse_block(lines: &[Line], cursor: &mut usize, indent: usize) -> Result<Yaml, ParseError> {
    if lines[*cursor].content.starts_with("- ") || lines[*cursor].content == "-" {
        parse_list(lines, cursor, indent)
    } else {
        parse_map(lines, cursor, indent)
    }
}

fn parse_map(lines: &[Line], cursor: &mut usize, indent: usize) -> Result<Yaml, ParseError> {
    let mut entries = Vec::new();
    while *cursor < lines.len() && lines[*cursor].indent >= indent {
        if lines[*cursor].indent > indent {
            return Err(ParseError(format!(
                "unexpected indentation on line {}",
                lines[*cursor].number
            )));
        }
        let line = &lines[*cursor];
        if line.content.starts_with("- ") {
            break;
        }
        let (key, rest) = split_key(line)?;
        *cursor += 1;
        entries.push((key, parse_after_key(lines, cursor, indent, rest)?));
    }
    Ok(Yaml::Map(entries))
}

fn parse_list(lines: &[Line], cursor: &mut usize, indent: usize) -> Result<Yaml, ParseError> {
    let mut items = Vec::new();
    while *cursor < lines.len() && lines[*cursor].indent == indent {
        let line = &lines[*cursor];
        let Some(rest) = line.content.strip_prefix("- ") else {
            break;
        };
        let rest = rest.to_string();
        let number = line.number;
        *cursor += 1;

        if let Some((key, value)) = rest.split_once(':') {
            // A map whose first key shares the `- ` line; its remaining keys are
            // indented to just past the marker.
            let mut entries = vec![(
                key.trim().to_string(),
                parse_after_key(lines, cursor, indent + 2, value.trim().to_string())?,
            )];
            while *cursor < lines.len() && lines[*cursor].indent == indent + 2 {
                let inner = &lines[*cursor];
                if inner.content.starts_with("- ") {
                    break;
                }
                let (key, rest) = split_key(inner)?;
                *cursor += 1;
                entries.push((key, parse_after_key(lines, cursor, indent + 2, rest)?));
            }
            items.push(Yaml::Map(entries));
        } else if rest.is_empty() {
            return Err(ParseError(format!("empty list item on line {number}")));
        } else {
            items.push(Yaml::Scalar(unquote(&rest)));
        }
    }
    Ok(Yaml::List(items))
}

/// A key's value is either on its own line or in the block beneath it.
fn parse_after_key(
    lines: &[Line],
    cursor: &mut usize,
    indent: usize,
    inline: String,
) -> Result<Yaml, ParseError> {
    match inline.as_str() {
        "[]" => return Ok(Yaml::List(Vec::new())),
        "{}" => return Ok(Yaml::Map(Vec::new())),
        "" => {}
        _ => return Ok(Yaml::Scalar(unquote(&inline))),
    }

    if *cursor >= lines.len() {
        return Ok(Yaml::Scalar(String::new()));
    }
    let next = &lines[*cursor];
    // A list under a key may sit at the key's indentation or deeper.
    if next.indent > indent || (next.indent == indent && next.content.starts_with("- ")) {
        parse_block(lines, cursor, next.indent)
    } else {
        Ok(Yaml::Scalar(String::new()))
    }
}

fn split_key(line: &Line) -> Result<(String, String), ParseError> {
    let (key, rest) = line.content.split_once(':').ok_or_else(|| {
        ParseError(format!(
            "expected `key: value` on line {}, found `{}`",
            line.number, line.content
        ))
    })?;
    Ok((unquote(key.trim()), rest.trim().to_string()))
}

fn unquote(text: &str) -> String {
    let bytes = text.as_bytes();
    if bytes.len() >= 2 && bytes[0] == b'"' && bytes[bytes.len() - 1] == b'"' {
        return text[1..text.len() - 1]
            .replace("\\\"", "\"")
            .replace("\\\\", "\\");
    }
    if bytes.len() >= 2 && bytes[0] == b'\'' && bytes[bytes.len() - 1] == b'\'' {
        return text[1..text.len() - 1].replace("''", "'");
    }
    text.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn map(entries: &[(&str, Yaml)]) -> Yaml {
        Yaml::Map(
            entries
                .iter()
                .map(|(k, v)| ((*k).to_string(), v.clone()))
                .collect(),
        )
    }

    #[test]
    fn an_empty_collection_stays_on_its_key_line() {
        let doc = map(&[
            ("contexts", Yaml::List(Vec::new())),
            ("currentcontext", Yaml::scalar("")),
            ("version", Yaml::scalar("v1")),
        ]);
        assert_eq!(
            emit(&doc),
            "contexts: []\ncurrentcontext: \"\"\nversion: v1\n"
        );
    }

    #[test]
    fn a_list_of_maps_lines_up_under_its_marker() {
        let doc = map(&[(
            "endpoints",
            Yaml::List(vec![map(&[
                (
                    "endpoint",
                    map(&[
                        ("address", Yaml::scalar("localhost")),
                        ("port", Yaml::scalar("1433")),
                    ]),
                ),
                ("name", Yaml::scalar("ep1")),
            ])]),
        )]);
        assert_eq!(
            emit(&doc),
            "endpoints:\n- endpoint:\n    address: localhost\n    port: 1433\n  name: ep1\n"
        );
    }

    #[test]
    fn what_is_emitted_parses_back_unchanged() {
        let doc = map(&[
            ("version", Yaml::scalar("v1")),
            (
                "endpoints",
                Yaml::List(vec![map(&[
                    (
                        "endpoint",
                        map(&[
                            ("address", Yaml::scalar("localhost")),
                            ("port", Yaml::scalar("1433")),
                        ]),
                    ),
                    ("name", Yaml::scalar("ep1")),
                ])]),
            ),
            ("currentcontext", Yaml::scalar("")),
            ("users", Yaml::List(Vec::new())),
        ]);
        assert_eq!(parse(&emit(&doc)).unwrap(), doc);
    }

    #[test]
    fn a_value_that_would_read_back_as_another_type_is_quoted() {
        assert_eq!(quote("true"), "\"true\"");
        assert_eq!(quote("null"), "\"null\"");
        assert_eq!(quote(""), "\"\"");
        assert_eq!(quote("a: b"), "\"a: b\"");
        assert_eq!(quote("1433"), "1433");
        assert_eq!(quote("localhost"), "localhost");
    }

    #[test]
    fn quotes_survive_a_round_trip() {
        let doc = map(&[("password", Yaml::scalar("p@ss: \"word\"#1"))]);
        let parsed = parse(&emit(&doc)).unwrap();
        assert_eq!(parsed.str_at("password"), "p@ss: \"word\"#1");
    }

    #[test]
    fn an_empty_document_is_an_empty_map() {
        assert_eq!(parse("").unwrap(), Yaml::Map(Vec::new()));
        assert_eq!(parse("# just a comment\n").unwrap(), Yaml::Map(Vec::new()));
    }

    #[test]
    fn the_file_go_sqlcmd_writes_is_understood() {
        let text = "contexts: []\ncurrentcontext: \"\"\nendpoints: []\nusers: []\nversion: v1\n";
        let doc = parse(text).unwrap();
        assert_eq!(doc.str_at("version"), "v1");
        assert_eq!(doc.str_at("currentcontext"), "");
        assert!(doc.get("endpoints").unwrap().as_list().is_empty());
    }

    #[test]
    fn a_populated_file_is_understood() {
        let text = concat!(
            "contexts:\n",
            "- context:\n",
            "    endpoint: ep1\n",
            "    user: u1\n",
            "  name: ctx1\n",
            "currentcontext: ctx1\n",
            "endpoints:\n",
            "- endpoint:\n",
            "    address: localhost\n",
            "    port: 1433\n",
            "  name: ep1\n",
            "version: v1\n",
        );
        let doc = parse(text).unwrap();
        let contexts = doc.get("contexts").unwrap().as_list();
        assert_eq!(contexts.len(), 1);
        assert_eq!(contexts[0].str_at("name"), "ctx1");
        assert_eq!(
            contexts[0].get("context").unwrap().str_at("endpoint"),
            "ep1"
        );
        let endpoints = doc.get("endpoints").unwrap().as_list();
        assert_eq!(endpoints[0].get("endpoint").unwrap().str_at("port"), "1433");
    }

    #[test]
    fn a_line_without_a_colon_is_refused() {
        assert!(parse("version v1\n").is_err());
    }
}
