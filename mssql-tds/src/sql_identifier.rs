// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! SQL Server identifier parsing and escaping utilities.
//!
//! This module provides functionality to parse multipart SQL Server identifiers
//! (server.database.schema.table) and escape them properly to prevent SQL injection.

use crate::core::TdsResult;

/// Indices for multipart identifier array
#[allow(dead_code)]
pub const SERVER_INDEX: usize = 0;
pub const CATALOG_INDEX: usize = 1;
pub const SCHEMA_INDEX: usize = 2;
pub const TABLE_INDEX: usize = 3;
pub const MAX_PARTS: usize = 4;

/// Parse a SQL Server multipart identifier.
///
/// Parses identifiers in the format: `[server].[database].[schema].[table]`
/// The parser uses a finite state machine to handle:
/// - Quoted identifiers with `[...]` or `"..."` syntax
/// - Escaped brackets (`]]` inside quoted identifiers)
/// - Whitespace handling
/// - Separator (`.`) detection
///
/// # Arguments
/// * `name` - The identifier string to parse (e.g., "db.schema.table")
/// * `allow_empty` - Whether to allow completely empty identifiers
///
/// # Returns
/// Array of 4 optional strings: [server, catalog, schema, table]
/// Missing parts are None, present parts are Some(String)
/// The array is right-justified (trailing positions filled first).
///
/// # Example
/// ```
/// use mssql_tds::sql_identifier::{parse_multipart_identifier, CATALOG_INDEX, SCHEMA_INDEX, TABLE_INDEX};
///
/// let parts = parse_multipart_identifier("MyDB.dbo.Users", false).unwrap();
/// assert_eq!(parts[CATALOG_INDEX], Some("MyDB".to_string()));
/// assert_eq!(parts[SCHEMA_INDEX], Some("dbo".to_string()));
/// assert_eq!(parts[TABLE_INDEX], Some("Users".to_string()));
/// ```
pub fn parse_multipart_identifier(
    name: &str,
    allow_empty: bool,
) -> TdsResult<[Option<String>; MAX_PARTS]> {
    // State machine states
    #[derive(Debug, PartialEq)]
    enum State {
        Init,         // Initial state
        InPart,       // Reading unquoted part
        InQuotedPart, // Reading quoted part
        AfterQuote,   // After closing quote
        AfterDot,     // After separator dot
    }

    let mut state = State::Init;
    let mut parts: Vec<String> = Vec::with_capacity(MAX_PARTS);
    let mut current_part = String::new();
    let mut quote_char: Option<char> = None;
    let chars: Vec<char> = name.chars().collect();
    let mut i = 0;

    while i < chars.len() {
        let ch = chars[i];

        match state {
            State::Init => {
                if ch.is_whitespace() {
                    // Skip leading whitespace
                    i += 1;
                    continue;
                } else if ch == '[' || ch == '"' {
                    // Start quoted identifier
                    quote_char = Some(ch);
                    state = State::InQuotedPart;
                } else if ch == '.' {
                    // Empty part before dot
                    if !allow_empty && parts.is_empty() {
                        return Err(crate::error::Error::UsageError(
                            "Empty identifier part is not allowed".to_string(),
                        ));
                    }
                    parts.push(String::new());
                    state = State::AfterDot;
                } else {
                    // Start unquoted identifier
                    current_part.push(ch);
                    state = State::InPart;
                }
            }
            State::InPart => {
                if ch == '.' {
                    // End of part
                    parts.push(current_part.trim_end().to_string());
                    current_part.clear();
                    state = State::AfterDot;
                } else {
                    current_part.push(ch);
                }
            }
            State::InQuotedPart => {
                let closing_quote = match quote_char {
                    Some('[') => ']',
                    Some('"') => '"',
                    _ => unreachable!(),
                };

                if ch == closing_quote {
                    // Check for escaped quote (]] or "")
                    if i + 1 < chars.len() && chars[i + 1] == closing_quote {
                        // Escaped quote - add one closing quote to the part
                        current_part.push(closing_quote);
                        i += 1; // Skip the next quote
                    } else {
                        // End of quoted part
                        state = State::AfterQuote;
                    }
                } else {
                    current_part.push(ch);
                }
            }
            State::AfterQuote => {
                if ch.is_whitespace() {
                    // Skip whitespace after quote
                    i += 1;
                    continue;
                } else if ch == '.' {
                    // End of part
                    parts.push(current_part.clone());
                    current_part.clear();
                    quote_char = None;
                    state = State::AfterDot;
                } else {
                    return Err(crate::error::Error::UsageError(format!(
                        "Unexpected character '{}' after closing quote",
                        ch
                    )));
                }
            }
            State::AfterDot => {
                if ch.is_whitespace() {
                    // Skip whitespace after dot
                    i += 1;
                    continue;
                } else if ch == '[' || ch == '"' {
                    // Start quoted identifier
                    quote_char = Some(ch);
                    state = State::InQuotedPart;
                } else if ch == '.' {
                    // Empty part
                    if !allow_empty {
                        return Err(crate::error::Error::UsageError(
                            "Empty identifier part is not allowed".to_string(),
                        ));
                    }
                    parts.push(String::new());
                } else {
                    // Start unquoted identifier
                    current_part.push(ch);
                    state = State::InPart;
                }
            }
        }

        i += 1;
    }

    // Handle final part
    match state {
        State::InPart | State::AfterQuote => {
            parts.push(current_part.trim_end().to_string());
        }
        State::InQuotedPart => {
            return Err(crate::error::Error::UsageError(
                "Unclosed quoted identifier".to_string(),
            ));
        }
        State::Init if !allow_empty => {
            return Err(crate::error::Error::UsageError(
                "Empty identifier is not allowed".to_string(),
            ));
        }
        _ => {}
    }

    // Validate part count
    if parts.len() > MAX_PARTS {
        return Err(crate::error::Error::UsageError(format!(
            "Too many identifier parts: {} (maximum is {})",
            parts.len(),
            MAX_PARTS
        )));
    }

    // Right-justify the parts array
    let mut result: [Option<String>; MAX_PARTS] = [None, None, None, None];
    let start_index = MAX_PARTS - parts.len();
    for (i, part) in parts.into_iter().enumerate() {
        result[start_index + i] = Some(part);
    }

    Ok(result)
}

/// Escape a SQL Server identifier by wrapping in brackets and escaping existing brackets.
///
/// # Arguments
/// * `name` - The identifier to escape
///
/// # Returns
/// The escaped identifier wrapped in square brackets
///
/// # Example
/// ```
/// use mssql_tds::sql_identifier::escape_identifier;
///
/// assert_eq!(escape_identifier("MyTable"), "[MyTable]");
/// assert_eq!(escape_identifier("My]Table"), "[My]]Table]");
/// ```
pub fn escape_identifier(name: &str) -> String {
    format!("[{}]", name.replace("]", "]]"))
}

/// Escape a string for use in a SQL literal (within '...').
///
/// This escapes single quotes by doubling them.
///
/// # Arguments
/// * `input` - The string to escape
///
/// # Returns
/// The escaped string suitable for use in a SQL literal
///
/// # Example
/// ```
/// use mssql_tds::sql_identifier::escape_string_literal;
///
/// assert_eq!(escape_string_literal("O'Brien"), "O''Brien");
/// assert_eq!(escape_string_literal("It's"), "It''s");
/// ```
pub fn escape_string_literal(input: &str) -> String {
    input.replace("'", "''")
}

/// Build a multipart name from components.
///
/// Reconstructs a qualified identifier from its parts, omitting leading None values.
///
/// # Arguments
/// * `parts` - Array of optional identifier parts [server, catalog, schema, table]
///
/// # Returns
/// The reconstructed identifier with each part escaped and separated by dots
///
/// # Example
/// ```
/// use mssql_tds::sql_identifier::build_multipart_name;
///
/// let parts = [None, Some("MyDB".to_string()), Some("dbo".to_string()), Some("Users".to_string())];
/// assert_eq!(build_multipart_name(&parts), "[MyDB].[dbo].[Users]");
/// ```
pub fn build_multipart_name(parts: &[Option<String>; MAX_PARTS]) -> String {
    let mut result = String::new();

    for part in parts.iter().flatten() {
        if !result.is_empty() {
            result.push('.');
        }
        result.push_str(&escape_identifier(part));
    }

    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_one_part() {
        let parts = parse_multipart_identifier("Users", false).unwrap();
        assert_eq!(parts[SERVER_INDEX], None);
        assert_eq!(parts[CATALOG_INDEX], None);
        assert_eq!(parts[SCHEMA_INDEX], None);
        assert_eq!(parts[TABLE_INDEX], Some("Users".to_string()));
    }

    #[test]
    fn test_parse_two_parts() {
        let parts = parse_multipart_identifier("dbo.Users", false).unwrap();
        assert_eq!(parts[SERVER_INDEX], None);
        assert_eq!(parts[CATALOG_INDEX], None);
        assert_eq!(parts[SCHEMA_INDEX], Some("dbo".to_string()));
        assert_eq!(parts[TABLE_INDEX], Some("Users".to_string()));
    }

    #[test]
    fn test_parse_three_parts() {
        let parts = parse_multipart_identifier("MyDB.dbo.Users", false).unwrap();
        assert_eq!(parts[SERVER_INDEX], None);
        assert_eq!(parts[CATALOG_INDEX], Some("MyDB".to_string()));
        assert_eq!(parts[SCHEMA_INDEX], Some("dbo".to_string()));
        assert_eq!(parts[TABLE_INDEX], Some("Users".to_string()));
    }

    #[test]
    fn test_parse_four_parts() {
        let parts = parse_multipart_identifier("Server.MyDB.dbo.Users", false).unwrap();
        assert_eq!(parts[SERVER_INDEX], Some("Server".to_string()));
        assert_eq!(parts[CATALOG_INDEX], Some("MyDB".to_string()));
        assert_eq!(parts[SCHEMA_INDEX], Some("dbo".to_string()));
        assert_eq!(parts[TABLE_INDEX], Some("Users".to_string()));
    }

    #[test]
    fn test_parse_quoted_identifier() {
        let parts = parse_multipart_identifier("[My Table]", false).unwrap();
        assert_eq!(parts[TABLE_INDEX], Some("My Table".to_string()));
    }

    #[test]
    fn test_parse_escaped_brackets() {
        let parts = parse_multipart_identifier("[My]]Table]", false).unwrap();
        assert_eq!(parts[TABLE_INDEX], Some("My]Table".to_string()));
    }

    #[test]
    fn test_parse_double_quotes() {
        let parts = parse_multipart_identifier("\"My Table\"", false).unwrap();
        assert_eq!(parts[TABLE_INDEX], Some("My Table".to_string()));
    }

    #[test]
    fn test_parse_mixed_quotes_and_unquoted() {
        let parts = parse_multipart_identifier("[My DB].dbo.[My Table]", false).unwrap();
        assert_eq!(parts[CATALOG_INDEX], Some("My DB".to_string()));
        assert_eq!(parts[SCHEMA_INDEX], Some("dbo".to_string()));
        assert_eq!(parts[TABLE_INDEX], Some("My Table".to_string()));
    }

    #[test]
    fn test_parse_whitespace_handling() {
        let parts = parse_multipart_identifier("  MyDB . dbo . Users  ", false).unwrap();
        assert_eq!(parts[CATALOG_INDEX], Some("MyDB".to_string()));
        assert_eq!(parts[SCHEMA_INDEX], Some("dbo".to_string()));
        assert_eq!(parts[TABLE_INDEX], Some("Users".to_string()));
    }

    #[test]
    fn test_parse_too_many_parts() {
        let result = parse_multipart_identifier("A.B.C.D.E", false);
        assert!(result.is_err());
    }

    #[test]
    fn test_parse_empty_not_allowed() {
        let result = parse_multipart_identifier("", false);
        assert!(result.is_err());
    }

    #[test]
    fn test_parse_empty_allowed() {
        let result = parse_multipart_identifier("", true);
        assert!(result.is_ok());
    }

    #[test]
    fn test_parse_unclosed_quote() {
        let result = parse_multipart_identifier("[MyTable", false);
        assert!(result.is_err());
    }

    #[test]
    fn test_escape_identifier() {
        assert_eq!(escape_identifier("MyTable"), "[MyTable]");
        assert_eq!(escape_identifier("My]Table"), "[My]]Table]");
        assert_eq!(escape_identifier("My]]Table"), "[My]]]]Table]");
        assert_eq!(escape_identifier(""), "[]");
    }

    #[test]
    fn test_escape_string_literal() {
        assert_eq!(escape_string_literal("O'Brien"), "O''Brien");
        assert_eq!(escape_string_literal("It's"), "It''s");
        assert_eq!(escape_string_literal("No quotes"), "No quotes");
        assert_eq!(escape_string_literal(""), "");
    }

    #[test]
    fn test_build_multipart_name() {
        let parts = [
            None,
            Some("MyDB".to_string()),
            Some("dbo".to_string()),
            Some("Users".to_string()),
        ];
        assert_eq!(build_multipart_name(&parts), "[MyDB].[dbo].[Users]");
    }

    #[test]
    fn test_build_multipart_name_with_special_chars() {
        let parts = [
            None,
            Some("My]DB".to_string()),
            Some("dbo".to_string()),
            Some("My]Table".to_string()),
        ];
        assert_eq!(build_multipart_name(&parts), "[My]]DB].[dbo].[My]]Table]");
    }

    #[test]
    fn test_build_multipart_name_single_part() {
        let parts = [None, None, None, Some("Users".to_string())];
        assert_eq!(build_multipart_name(&parts), "[Users]");
    }

    #[test]
    fn test_build_multipart_name_all_parts() {
        let parts = [
            Some("Server".to_string()),
            Some("DB".to_string()),
            Some("schema".to_string()),
            Some("table".to_string()),
        ];
        assert_eq!(
            build_multipart_name(&parts),
            "[Server].[DB].[schema].[table]"
        );
    }
}
