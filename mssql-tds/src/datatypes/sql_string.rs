// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use crate::{query::metadata::ColumnMetadata, token::tokens::SqlCollation};
use core::fmt;
use std::{fmt::Debug, fmt::Display};
use tracing::warn;

use super::{
    lcid_encoding::lcid_to_encoding,
    sqldatatypes::{TypeInfoVariant, is_unicode_type},
};

/// Character encoding used by a [`SqlString`].
#[derive(Debug, PartialEq, Clone, Copy)]
pub enum EncodingType {
    /// UTF-8 encoding.
    Utf8,
    /// UTF-16LE encoding.
    Utf16,
    /// Encoding derived from SQL collation LCID.
    LcidBased(SqlCollation),
    /// Placeholder set before the connection collation is known.
    // This is to be used when we want to have an empty encoding, which
    // is later written over the protocol by getting the collation from the connection.
    DelayedSet,
}

/// Encoded string value from a TDS character column.
#[derive(PartialEq, Clone)]
pub struct SqlString {
    /// Raw encoded bytes.
    pub bytes: Vec<u8>,
    encoding_type: EncodingType,
}

impl SqlString {
    /// Creates a `SqlString` from raw bytes and an encoding type.
    pub fn new(bytes: Vec<u8>, encoding_type: EncodingType) -> Self {
        SqlString {
            bytes,
            encoding_type,
        }
    }

    /// Creates a UTF-16LE–encoded `SqlString` from a Rust `String`.
    pub fn from_utf8_string(string: String) -> Self {
        let utf16_bytes = string
            .encode_utf16()
            .flat_map(|f| f.to_le_bytes())
            .collect::<Vec<u8>>();
        SqlString::new(utf16_bytes, EncodingType::Utf16)
    }

    /// Decodes the stored bytes into a Rust `String` according to the encoding type.
    pub fn to_utf8_string(&self) -> String {
        match self.encoding_type {
            // TODO: Investigation needed. When creating a Utf8 strings from the vector, the string is weirdly encoded.
            // UTF16 decode works better.
            EncodingType::Utf8 => String::from_utf8(self.bytes.clone()).unwrap(),
            EncodingType::Utf16 => {
                // Use encoding_rs for efficient UTF-16LE decoding without intermediate Vec<u16> allocation
                let (decoded, _, _) = encoding_rs::UTF_16LE.decode(&self.bytes);
                decoded.into_owned()
            }
            EncodingType::LcidBased(collation) => {
                // Extract LCID from the lower 20 bits of collation.info
                let lcid = collation.info & 0x000F_FFFF;

                // Map LCID to encoding
                let encoding = match lcid_to_encoding(lcid) {
                    Ok(enc) => enc,
                    Err(e) => {
                        warn!(
                            "Unsupported LCID 0x{:04X} ({}), falling back to Windows-1252. Error: {}",
                            lcid, lcid, e
                        );
                        // Fall back to Windows-1252 for unsupported LCIDs
                        encoding_rs::WINDOWS_1252
                    }
                };

                // Decode bytes using the determined encoding
                let (decoded, _used_encoding, had_errors) = encoding.decode(&self.bytes);

                if had_errors {
                    warn!(
                        "Encountered decoding errors while converting LCID 0x{:04X} ({}) encoded data. \
                         Some characters may have been replaced with U+FFFD.",
                        lcid, lcid
                    );
                }

                decoded.into_owned()
            }
            EncodingType::DelayedSet => {
                // DelayedSet encoding is not defined, so we return the bytes as a UTF-8 string.
                unimplemented!("DelayedSet encoding conversion to UTF8 not implemented");
            }
        }
    }

    /// Returns true if this SqlString is already encoded as UTF-16
    #[inline]
    pub fn is_utf16(&self) -> bool {
        matches!(self.encoding_type, EncodingType::Utf16)
    }

    /// Returns the raw UTF-16 bytes if already encoded, otherwise None
    /// This avoids re-encoding strings that are already in UTF-16 format
    #[inline]
    pub fn as_utf16_bytes(&self) -> Option<&[u8]> {
        if self.is_utf16() {
            Some(&self.bytes)
        } else {
            None
        }
    }

    /// Returns the raw bytes when they should be written directly to the wire
    /// without encoding conversion. This is the case for DelayedSet and LcidBased
    /// encodings where the bytes are already in the correct wire format.
    #[inline]
    pub fn as_raw_wire_bytes(&self) -> Option<&[u8]> {
        match &self.encoding_type {
            EncodingType::DelayedSet | EncodingType::LcidBased(_) => Some(&self.bytes),
            _ => None,
        }
    }

    /// Returns the encoding type of this SqlString
    #[inline]
    pub fn encoding_type(&self) -> &EncodingType {
        &self.encoding_type
    }
}

impl Debug for SqlString {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.encoding_type {
            EncodingType::LcidBased(_) => write!(f, "{:?}", self.bytes),
            EncodingType::DelayedSet => write!(f, "DelayedSet encoded: {:?}", self.bytes.len()),
            _ => write!(f, "{:?}", self.to_utf8_string()),
        }
    }
}

impl Display for SqlString {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let EncodingType::LcidBased(_) = self.encoding_type {
            write!(f, "{:?}", self.bytes)
        } else {
            write!(f, "{}", self.to_utf8_string())
        }
    }
}

/// Determines the character encoding for a column from its metadata.
pub fn get_encoding_type(metadata: &ColumnMetadata) -> EncodingType {
    let collation = match metadata.type_info.type_info_variant {
        TypeInfoVariant::PartialLen(_, _, collation, _, _) => collation,
        TypeInfoVariant::VarLenString(_, _, collation) => collation,
        _ => None,
    };

    if is_unicode_type(metadata.data_type) {
        EncodingType::Utf16
    } else if collation.is_some_and(|c| c.utf8()) {
        EncodingType::Utf8
    } else {
        // A non-Unicode string column should always carry a collation, but this
        // now runs while parsing COLMETADATA rather than while decoding a cell, so
        // a malformed token must not be able to panic the parser.
        EncodingType::LcidBased(collation.unwrap_or_default())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_sql_string_new() {
        let bytes = vec![72, 0, 101, 0, 108, 0, 108, 0, 111, 0];
        let sql_str = SqlString::new(bytes.clone(), EncodingType::Utf16);
        assert_eq!(sql_str.bytes, bytes);
    }

    #[test]
    fn test_from_utf8_string() {
        let input = "Hello World".to_string();
        let sql_str = SqlString::from_utf8_string(input.clone());
        assert_eq!(sql_str.to_utf8_string(), input);
    }

    #[test]
    fn test_to_utf8_string_utf16() {
        let bytes = vec![72, 0, 105, 0];
        let sql_str = SqlString::new(bytes, EncodingType::Utf16);
        assert_eq!(sql_str.to_utf8_string(), "Hi");
    }

    #[test]
    fn test_to_utf8_string_utf8() {
        let bytes = "Test".as_bytes().to_vec();
        let sql_str = SqlString::new(bytes, EncodingType::Utf8);
        assert_eq!(sql_str.to_utf8_string(), "Test");
    }

    #[test]
    fn test_sql_string_clone() {
        let sql_str = SqlString::from_utf8_string("Clone test".to_string());
        let cloned = sql_str.clone();
        assert_eq!(sql_str.bytes, cloned.bytes);
    }

    #[test]
    fn test_sql_string_debug_utf16() {
        let sql_str = SqlString::from_utf8_string("Debug".to_string());
        let debug_str = format!("{sql_str:?}");
        assert!(debug_str.contains("Debug"));
    }

    #[test]
    fn test_sql_string_debug_delayed_set() {
        let sql_str = SqlString::new(vec![1, 2, 3, 4, 5], EncodingType::DelayedSet);
        let debug_str = format!("{sql_str:?}");
        assert!(debug_str.contains("DelayedSet"));
        assert!(debug_str.contains("5"));
    }

    #[test]
    fn test_sql_string_display_utf16() {
        let sql_str = SqlString::from_utf8_string("Display".to_string());
        let display_str = format!("{sql_str}");
        assert_eq!(display_str, "Display");
    }

    #[test]
    fn test_sql_string_equality() {
        let sql_str1 = SqlString::from_utf8_string("Equal".to_string());
        let sql_str2 = SqlString::from_utf8_string("Equal".to_string());
        let sql_str3 = SqlString::from_utf8_string("Different".to_string());
        assert_eq!(sql_str1, sql_str2);
        assert_ne!(sql_str1, sql_str3);
    }

    #[test]
    fn test_from_utf8_string_empty() {
        let sql_str = SqlString::from_utf8_string(String::new());
        assert_eq!(sql_str.to_utf8_string(), "");
        assert!(sql_str.bytes.is_empty());
    }

    #[test]
    fn test_from_utf8_string_special_chars() {
        let input = "Hello! @#$%^&*()".to_string();
        let sql_str = SqlString::from_utf8_string(input.clone());
        assert_eq!(sql_str.to_utf8_string(), input);
    }

    #[test]
    fn test_from_utf8_string_unicode() {
        let input = "Hello World".to_string();
        let sql_str = SqlString::from_utf8_string(input.clone());
        assert_eq!(sql_str.to_utf8_string(), input);
    }

    #[test]
    fn test_sql_string_new_utf8() {
        let bytes = "UTF8 String".as_bytes().to_vec();
        let sql_str = SqlString::new(bytes.clone(), EncodingType::Utf8);
        assert_eq!(sql_str.bytes, bytes);
        assert_eq!(sql_str.to_utf8_string(), "UTF8 String");
    }

    #[test]
    fn test_sql_string_new_delayed_set() {
        let bytes = vec![1, 2, 3, 4];
        let sql_str = SqlString::new(bytes.clone(), EncodingType::DelayedSet);
        assert_eq!(sql_str.bytes, bytes);
    }

    // ========================================================================
    // LCID Encoding Tests
    // ========================================================================

    #[test]
    fn test_lcid_based_encoding_us_english() {
        // Test US English (Windows-1252) encoding
        // "Hello, World!" in Windows-1252
        let text = b"Hello, World!";
        let collation = SqlCollation {
            info: 0x0409, // US English LCID
            lcid_language_id: 0,
            col_flags: 0,
            sort_id: 0,
        };
        let sql_str = SqlString::new(text.to_vec(), EncodingType::LcidBased(collation));
        assert_eq!(sql_str.to_utf8_string(), "Hello, World!");
    }

    #[test]
    fn test_lcid_based_encoding_special_chars_windows1252() {
        // Test special characters in Windows-1252
        // "Café résumé naïve" with special chars
        let text = b"Caf\xe9 r\xe9sum\xe9 na\xefve"; // é = 0xE9, ï = 0xEF in Windows-1252
        let collation = SqlCollation {
            info: 0x0409, // US English LCID
            lcid_language_id: 0,
            col_flags: 0,
            sort_id: 0,
        };
        let sql_str = SqlString::new(text.to_vec(), EncodingType::LcidBased(collation));
        assert_eq!(sql_str.to_utf8_string(), "Café résumé naïve");
    }

    #[test]
    fn test_lcid_based_encoding_japanese() {
        // Test Japanese Shift_JIS encoding
        // "こんにちは" (Konnichiwa) in Shift_JIS: 82B1 82F1 82C9 82BF 82CD
        let text = vec![0x82, 0xB1, 0x82, 0xF1, 0x82, 0xC9, 0x82, 0xBF, 0x82, 0xCD];
        let collation = SqlCollation {
            info: 0x0411, // Japanese LCID
            lcid_language_id: 0,
            col_flags: 0,
            sort_id: 0,
        };
        let sql_str = SqlString::new(text, EncodingType::LcidBased(collation));
        assert_eq!(sql_str.to_utf8_string(), "こんにちは");
    }

    #[test]
    fn test_lcid_based_encoding_with_flags() {
        // Test LCID extraction with flags set in upper bits
        // US English LCID (0x0409) with flags (0x00D00409)
        let text = b"Test";
        let collation = SqlCollation {
            info: 0x00D0_0409, // LCID with comparison flags
            lcid_language_id: 0,
            col_flags: 0,
            sort_id: 0,
        };
        let sql_str = SqlString::new(text.to_vec(), EncodingType::LcidBased(collation));
        // Should still decode as US English (lower 20 bits = 0x0409)
        assert_eq!(sql_str.to_utf8_string(), "Test");
    }

    #[test]
    fn test_lcid_based_encoding_empty_string() {
        // Test empty string
        let text = vec![];
        let collation = SqlCollation {
            info: 0x0409, // US English LCID
            lcid_language_id: 0,
            col_flags: 0,
            sort_id: 0,
        };
        let sql_str = SqlString::new(text, EncodingType::LcidBased(collation));
        assert_eq!(sql_str.to_utf8_string(), "");
    }

    #[test]
    fn test_is_utf16() {
        let utf16_str = SqlString::from_utf8_string("test".to_string());
        assert!(utf16_str.is_utf16());

        let utf8_str = SqlString::new(b"test".to_vec(), EncodingType::Utf8);
        assert!(!utf8_str.is_utf16());
    }

    #[test]
    fn test_as_utf16_bytes() {
        let utf16_str = SqlString::from_utf8_string("Hi".to_string());
        let bytes = utf16_str.as_utf16_bytes();
        assert!(bytes.is_some());
        assert_eq!(bytes.unwrap(), &[72, 0, 105, 0]); // "Hi" in UTF-16LE

        let utf8_str = SqlString::new(b"test".to_vec(), EncodingType::Utf8);
        assert!(utf8_str.as_utf16_bytes().is_none());
    }
}
