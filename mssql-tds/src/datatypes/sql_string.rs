// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use crate::{
    query::metadata::ColumnMetadata,
    token::tokens::{CODE_PAGE_FROM_SORT_ID, SqlCollation},
};
use core::fmt;
use std::{fmt::Debug, fmt::Display};
use tracing::warn;

mod encoding;
pub use encoding::{ResolvedDecoder, ResolvedEncoding};

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
    /// Encoding derived from SQL collation UTF-8 flag, sort ID, or LCID.
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

/// Maps a collation's LCID to an encoding, falling back to Windows-1252 with a
/// warning when the LCID is not one we map.
fn lcid_encoding_or_fallback(collation: SqlCollation) -> &'static encoding_rs::Encoding {
    // LCID lives in the lower 20 bits of collation.info.
    let lcid = collation.info & 0x000F_FFFF;
    match lcid_to_encoding(lcid) {
        Ok(encoding) => encoding,
        Err(e) => {
            warn!(
                "Unsupported LCID 0x{:04X} ({}), falling back to Windows-1252. Error: {}",
                lcid, lcid, e
            );
            encoding_rs::WINDOWS_1252
        }
    }
}

fn resolve_collation(collation: SqlCollation) -> ResolvedEncoding {
    if collation.utf8() {
        return encoding_rs::UTF_8.into();
    }
    if let Some(code_page) = CODE_PAGE_FROM_SORT_ID[usize::from(collation.sort_id)] {
        let encoding = match code_page {
            437 => return ResolvedEncoding::Oem437,
            850 => return ResolvedEncoding::Oem850,
            874 => encoding_rs::WINDOWS_874,
            932 => encoding_rs::SHIFT_JIS,
            936 => encoding_rs::GBK,
            949 => encoding_rs::EUC_KR,
            950 => encoding_rs::BIG5,
            1250 => encoding_rs::WINDOWS_1250,
            1251 => encoding_rs::WINDOWS_1251,
            1252 => encoding_rs::WINDOWS_1252,
            1253 => encoding_rs::WINDOWS_1253,
            1254 => encoding_rs::WINDOWS_1254,
            1255 => encoding_rs::WINDOWS_1255,
            1256 => encoding_rs::WINDOWS_1256,
            1257 => encoding_rs::WINDOWS_1257,
            _ => {
                warn!(
                    "Unsupported code page {} for SQL sort ID {}, falling back to LCID",
                    code_page, collation.sort_id
                );
                return lcid_encoding_or_fallback(collation).into();
            }
        };
        return encoding.into();
    }
    lcid_encoding_or_fallback(collation).into()
}

/// Byte substituted for a character the target narrow encoding cannot
/// represent.
///
/// Matches msodbcsql, which converts with `WideCharToMultiByte`/`iconv` and
/// takes the code page's default character — hardcoded `0x3f` in its own
/// cross-platform converter (`Globalization.h`, `iconv_buffer::DefaultChar`).
///
/// SQL Server itself substitutes the same byte for a `CAST(N'…' AS varchar(n))`
/// it cannot best-fit map: measured on `SQL_Latin1_General_CP1_CI_AS`,
/// `ASCII(CAST(N'日' AS varchar(4)))` is 63. Characters it *can* best-fit
/// (`Ł`→`L`, `Ć`→`C`, `‐`→`-`) are transliterated rather than substituted, and
/// this driver does not reproduce that — see `docs/parity-deviations.md`.
pub const NARROW_SUBSTITUTE_BYTE: u8 = b'?';

/// Wire bytes from a narrow encode, plus whether producing them lost anything.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NarrowEncoded {
    /// Encoded bytes, ready for the wire.
    pub bytes: Vec<u8>,
    /// `true` when at least one character had no representation in the target
    /// encoding and was replaced with [`NARROW_SUBSTITUTE_BYTE`]. The caller
    /// decides whether that is worth reporting; `mssql-odbc` surfaces it as
    /// SQLSTATE `01000` under `SQL_COPT_SS_WARN_ON_CP_ERROR`.
    pub had_loss: bool,
}

impl NarrowEncoded {
    /// Bytes that encoded exactly.
    fn exact(bytes: Vec<u8>) -> Self {
        Self {
            bytes,
            had_loss: false,
        }
    }
}

/// Encodes `text` for the wire under `collation`'s narrow encoding: UTF-8 when
/// the collation is UTF-8-aware, then its SQL sort ID code page, then its LCID
/// (falling back to Windows-1252 for an LCID this crate does not map).
///
/// Mirrors the encoding step [`get_encoding_type`] performs for a materialized
/// value, for a caller that must produce collation-correct wire bytes
/// directly instead of routing through the parameter serializer -- e.g. a
/// data-at-execution write, which streams bytes to the wire before the normal
/// parameter-serialization path runs. Does *not* mirror that serializer's own
/// `VARCHAR | CHAR | TEXT` arm (`tds_value_serializer.rs`): that arm still
/// resolves only the LCID, ignoring both the UTF-8 flag and SQL sort ID.
/// Inline string parameters can therefore encode differently from this helper
/// and from fetched values under UTF-8 or SQL sort-ID collations. Unifying that
/// serializer path is deferred alongside the UTF-8 discrepancy in AB#47590.
///
/// A character the encoding cannot represent becomes
/// [`NARROW_SUBSTITUTE_BYTE`] and sets [`NarrowEncoded::had_loss`]. It must not
/// be left to `encoding_rs`, whose `encode` implements WHATWG form-submission
/// semantics and emits a decimal numeric character reference instead --
/// `U+65E5` as the eight ASCII bytes `&#26085;` -- so the server would store
/// markup in place of the value, and one character would count as eight against
/// the column's length (AB#47598).
pub fn encode_narrow(text: &str, collation: SqlCollation) -> NarrowEncoded {
    let (encoded, encoding_used, had_errors) = resolve_collation(collation).encode(text);
    if !had_errors {
        return NarrowEncoded::exact(encoded.into_owned());
    }
    NarrowEncoded {
        bytes: substitute_unmappable(text, encoding_used),
        had_loss: true,
    }
}

/// Re-encodes `text` one character at a time, replacing each character
/// `encoding` cannot represent with [`NARROW_SUBSTITUTE_BYTE`].
///
/// Only reached once a whole-string encode has already reported a substitution,
/// so the per-character cost never lands on a value that encodes cleanly. Every
/// encoding this resolves to is stateless, so a character encodes the same
/// alone as it does in context.
///
/// One substitute byte per **UTF-16 code unit**, not per character, so an
/// astral character yields two. `WideCharToMultiByte` counts that way
/// (measured: `U+1F600` gives `3F 3F` under CP1252 and CP932), as does SQL
/// Server (`DATALENGTH(CAST(N'😀' AS varchar(4)))` is 2), and so do
/// msodbcsql's iconv legs -- `EncodingConverter::Convert`'s `EILSEQ` arm
/// advances one `WCHAR` via `SkipSingleCh()` and writes one `DefaultChar`
/// (`Globalization.h`). Counting scalars instead would make a substituted value
/// one byte shorter than the same value through msodbcsql on every platform,
/// and a `varchar(n)` would accept a string the reference driver rejects.
pub(crate) fn substitute_unmappable(text: &str, encoding: ResolvedEncoding) -> Vec<u8> {
    let mut out = Vec::with_capacity(text.len());
    let mut buf = [0u8; 4];
    for character in text.chars() {
        let (bytes, _, failed) = encoding.encode(character.encode_utf8(&mut buf));
        if failed {
            out.extend(std::iter::repeat_n(
                NARROW_SUBSTITUTE_BYTE,
                character.len_utf16(),
            ));
        } else {
            out.extend_from_slice(&bytes);
        }
    }
    out
}

impl EncodingType {
    /// The encoding these bytes are in, or `None` when the collation is not yet
    /// known ([`EncodingType::DelayedSet`]) or requires an OEM codec (CP437/850)
    /// unavailable in `encoding_rs`. Use [`Self::resolved_encoding`] for all
    /// supported SQL collations.
    ///
    /// Retained for compatibility with callers using `encoding_rs` directly.
    /// New conversion code should use [`Self::resolved_encoding`] to include
    /// the OEM codecs.
    ///
    /// Decoding through this substitutes U+FFFD on malformed input, whereas
    /// [`SqlString::to_utf8_string`] panics on invalid UTF-8 under
    /// [`EncodingType::Utf8`]. Use this when replacement is the wanted
    /// behaviour, not as a drop-in for `to_utf8_string`.
    pub fn encoding(&self) -> Option<&'static encoding_rs::Encoding> {
        self.resolved_encoding()
            .and_then(ResolvedEncoding::as_encoding_rs)
    }

    /// Resolves UTF-8 first, then a recognized nonzero SQL sort ID, then LCID.
    /// Unknown LCIDs retain the warning and Windows-1252 fallback.
    pub fn resolved_encoding(&self) -> Option<ResolvedEncoding> {
        match self {
            EncodingType::Utf8 => Some(encoding_rs::UTF_8.into()),
            EncodingType::Utf16 => Some(encoding_rs::UTF_16LE.into()),
            EncodingType::LcidBased(collation) => Some(resolve_collation(*collation)),
            EncodingType::DelayedSet => None,
        }
    }
}

impl SqlString {
    /// Creates a `SqlString` from raw bytes and an encoding type.
    pub fn new(bytes: Vec<u8>, encoding_type: EncodingType) -> Self {
        SqlString {
            bytes,
            encoding_type,
        }
    }

    /// Splits into the raw encoded bytes and their encoding.
    pub fn into_parts(self) -> (Vec<u8>, EncodingType) {
        (self.bytes, self.encoding_type)
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
        Self::decode(&self.bytes, self.encoding_type)
    }

    /// Decodes wire bytes in `encoding_type` into a Rust `String`.
    /// The explicit encoding is authoritative; BOM-shaped prefixes are payload.
    ///
    /// Lets a writer handed borrowed bytes by
    /// [`RowWriter::write_string`](crate::datatypes::row_writer::RowWriter::write_string)
    /// decode them without first copying into an owned [`SqlString`].
    pub fn decode(bytes: &[u8], encoding_type: EncodingType) -> String {
        match encoding_type {
            // TODO: Investigation needed. When creating a Utf8 strings from the vector, the string is weirdly encoded.
            // UTF16 decode works better.
            EncodingType::Utf8 => String::from_utf8(bytes.to_vec()).unwrap(),
            EncodingType::Utf16 => {
                // Use encoding_rs for efficient UTF-16LE decoding without intermediate Vec<u16> allocation
                let (decoded, _) = encoding_rs::UTF_16LE.decode_without_bom_handling(bytes);
                decoded.into_owned()
            }
            EncodingType::LcidBased(collation) => {
                // Extract LCID from the lower 20 bits of collation.info
                let lcid = collation.info & 0x000F_FFFF;
                let encoding = resolve_collation(collation);

                // Decode bytes using the determined encoding
                let (decoded, had_errors) = encoding.decode_without_bom_handling(bytes);

                if had_errors {
                    warn!(
                        "Encountered decoding errors while converting {} encoded data (LCID 0x{:04X}, SQL sort ID {}). \
                         Some characters may have been replaced with U+FFFD.",
                        encoding.name(),
                        lcid,
                        collation.sort_id
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
    } else if collation.is_some() && collation.unwrap().utf8() {
        EncodingType::Utf8
    } else {
        EncodingType::LcidBased(collation.unwrap())
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

    fn collation(lcid: u32) -> SqlCollation {
        SqlCollation {
            info: lcid,
            lcid_language_id: lcid as i32,
            col_flags: 0,
            sort_id: 0,
        }
    }

    #[test]
    fn encoding_maps_the_unicode_variants() {
        assert_eq!(EncodingType::Utf8.encoding(), Some(encoding_rs::UTF_8));
        assert_eq!(EncodingType::Utf16.encoding(), Some(encoding_rs::UTF_16LE));
    }

    #[test]
    fn encoding_is_none_until_the_collation_is_known() {
        assert_eq!(EncodingType::DelayedSet.encoding(), None);
    }

    #[test]
    fn encoding_resolves_a_known_lcid() {
        // 0x0409 (en-US) maps to Windows-1252, and the fallback would also
        // produce Windows-1252, so pin an LCID whose encoding is distinct from
        // the fallback to prove the lookup actually ran.
        let encoding = EncodingType::LcidBased(collation(0x0419)).encoding();
        assert_eq!(encoding, Some(lcid_to_encoding(0x0419).unwrap()));
        assert_ne!(encoding, Some(encoding_rs::WINDOWS_1252));
    }

    #[test]
    fn encoding_falls_back_for_an_unmapped_lcid() {
        let unmapped = 0x000F_FFFF;
        assert!(lcid_to_encoding(unmapped).is_err(), "LCID must be unmapped");
        assert_eq!(
            EncodingType::LcidBased(collation(unmapped)).encoding(),
            Some(encoding_rs::WINDOWS_1252)
        );
    }

    #[test]
    fn encoding_agrees_with_to_utf8_string_for_lcid_bytes() {
        // The accessor has to decode to the same text `to_utf8_string` would,
        // otherwise a writer using it would silently diverge from the owned path.
        let encoding_type = EncodingType::LcidBased(collation(0x0419));
        let bytes = vec![0xCF, 0xF0, 0xE8, 0xE2, 0xE5, 0xF2];

        let via_accessor = encoding_type
            .encoding()
            .expect("LCID encoding is known")
            .decode(&bytes)
            .0
            .into_owned();

        assert_eq!(
            via_accessor,
            SqlString::new(bytes, encoding_type).to_utf8_string()
        );
    }

    #[test]
    fn decode_matches_to_utf8_string_across_encodings() {
        // A writer decoding borrowed bytes must land on exactly the text the
        // owned path produces, or the two row-write paths silently diverge.
        let cases = [
            (b"h\0i\0".to_vec(), EncodingType::Utf16),
            (b"hi".to_vec(), EncodingType::Utf8),
            (
                vec![0xCF, 0xF0, 0xE8, 0xE2, 0xE5, 0xF2],
                EncodingType::LcidBased(collation(0x0419)),
            ),
        ];

        for (bytes, encoding_type) in cases {
            assert_eq!(
                SqlString::decode(&bytes, encoding_type),
                SqlString::new(bytes.clone(), encoding_type).to_utf8_string(),
                "mismatch for {encoding_type:?}"
            );
        }
    }

    #[test]
    fn decode_preserves_bom_shaped_sql_payloads() {
        let cp1252 = EncodingType::LcidBased(collation(0x0409));
        assert_eq!(cp1252.encoding(), Some(encoding_rs::WINDOWS_1252));
        let cases: &[(&[u8], EncodingType, &str)] = &[
            (b"\xEF\xBB\xBFA", cp1252, "\u{EF}\u{BB}\u{BF}A"),
            (b"\xFF\xFEA\0", cp1252, "\u{FF}\u{FE}A\0"),
            (b"\xFE\xFFA\0", cp1252, "\u{FE}\u{FF}A\0"),
            (b"\xFF\xFEA\0", EncodingType::Utf16, "\u{FEFF}A"),
            (b"\xFE\xFFA\0", EncodingType::Utf16, "\u{FFFE}A"),
            (b"\xEF\xBB\xBFA", EncodingType::Utf8, "\u{FEFF}A"),
            (b"A\xEF\xBB\xBF", cp1252, "A\u{EF}\u{BB}\u{BF}"),
            (b"caf\xE9", cp1252, "caf\u{E9}"),
            (b"A\0", EncodingType::Utf16, "A"),
            (b"\x3D\xD8\0\xDE\0\0", EncodingType::Utf16, "\u{1F600}\0"),
            (b"\0\xD8", EncodingType::Utf16, "\u{FFFD}"),
            (b"\0\xDC", EncodingType::Utf16, "\u{FFFD}"),
            (b"A\0B", EncodingType::Utf16, "A\u{FFFD}"),
            (b"", cp1252, ""),
            (b"", EncodingType::Utf16, ""),
        ];
        for &(bytes, encoding, expected) in cases {
            assert_eq!(
                SqlString::decode(bytes, encoding),
                expected,
                "{encoding:?}: {bytes:02X?}"
            );
            assert_eq!(
                SqlString::new(bytes.to_vec(), encoding).to_utf8_string(),
                expected,
                "{encoding:?}: {bytes:02X?}"
            );
        }
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

    #[test]
    fn encode_narrow_uses_the_lcid_codepage_for_a_non_utf8_collation() {
        let collation = SqlCollation {
            info: 0x0409, // US English LCID -> Windows-1252
            lcid_language_id: 0,
            col_flags: 0,
            sort_id: 0,
        };
        let encoded = encode_narrow("Caf\u{e9}", collation);
        assert_eq!(encoded.bytes, b"Caf\xe9");
        assert!(!encoded.had_loss);
    }

    #[test]
    fn encode_narrow_passes_through_utf8_for_a_utf8_collation() {
        let collation = SqlCollation {
            info: 0x0409,
            lcid_language_id: 0,
            col_flags: 0x40, // fUTF8
            sort_id: 0,
        };
        assert_eq!(
            encode_narrow("Caf\u{e9}", collation).bytes,
            "Caf\u{e9}".as_bytes()
        );
    }

    #[test]
    fn encode_narrow_falls_back_to_windows_1252_for_an_unmapped_lcid() {
        let collation = SqlCollation {
            info: 0x000F_FFFF,
            lcid_language_id: 0,
            col_flags: 0,
            sort_id: 0,
        };
        assert_eq!(encode_narrow("Caf\u{e9}", collation).bytes, b"Caf\xe9");
    }

    /// U+65E5 has no Windows-1252 representation. Left to `encoding_rs` it
    /// would become the eight ASCII bytes `&#26085;` -- WHATWG
    /// form-submission semantics -- so the server would store markup and one
    /// character would count as eight against the column. It is substituted
    /// with a single `?` instead, matching msodbcsql and SQL Server's own
    /// `CAST`, and the loss is reported for the caller to surface (AB#47598).
    #[test]
    fn encode_narrow_substitutes_a_character_the_codepage_cannot_represent() {
        let collation = SqlCollation {
            info: 0x0409, // US English LCID -> Windows-1252
            lcid_language_id: 0,
            col_flags: 0,
            sort_id: 0,
        };
        let encoded = encode_narrow("Caf\u{65e5}", collation);
        assert_eq!(encoded.bytes, b"Caf?");
        assert!(encoded.had_loss);
    }

    /// Only the unmappable characters are substituted: `é` encodes fine in
    /// Windows-1252 and must survive a value that also carries one that does
    /// not.
    #[test]
    fn encode_narrow_substitutes_only_the_unmappable_characters() {
        let collation = SqlCollation {
            info: 0x0409,
            lcid_language_id: 0,
            col_flags: 0,
            sort_id: 0,
        };
        let encoded = encode_narrow("\u{e9}\u{65e5}\u{e9}", collation);
        assert_eq!(encoded.bytes, b"\xe9?\xe9");
        assert!(encoded.had_loss);
    }

    /// A multi-byte mappable character keeps all of its bytes while an
    /// unmappable neighbour collapses to one, so the substitution cannot be a
    /// per-character byte-for-byte assumption.
    ///
    /// Measured with `WideCharToMultiByte(932, 0, ...)`: `U+3042` is `82 A0`
    /// with no loss flag, `U+0141` is `3F` with it set, and the pair is
    /// `82 A0 3F`.
    #[test]
    fn encode_narrow_substitutes_within_a_dbcs_code_page() {
        let collation = SqlCollation {
            info: 0x0411, // Japanese -> Shift_JIS
            lcid_language_id: 0,
            col_flags: 0,
            sort_id: 0,
        };
        let encoded = encode_narrow("\u{3042}\u{0141}", collation);
        assert_eq!(encoded.bytes, b"\x82\xa0?");
        assert!(encoded.had_loss);
    }

    /// The OEM code pages take the table-backed codec rather than `encoding_rs`
    /// and emit the same numeric character reference, so they must substitute on
    /// the same terms.
    ///
    /// Measured with `WideCharToMultiByte(437, 0, ...)`: `U+65E5` is `3F` with
    /// the loss flag set. `U+0141` is deliberately not used here - CP437
    /// best-fits it to `4C` (`L`), which is parity-deviations entry 18 rather
    /// than a substitution.
    #[test]
    fn encode_narrow_substitutes_under_an_oem_code_page() {
        let collation = SqlCollation {
            info: 0x0409,
            lcid_language_id: 0,
            col_flags: 0,
            sort_id: 30, // CP437
        };
        let encoded = encode_narrow("\u{65e5}", collation);
        assert_eq!(encoded.bytes, b"?");
        assert!(encoded.had_loss);
    }

    /// An astral character is two UTF-16 code units and substitutes as two
    /// bytes, matching `WideCharToMultiByte` (measured: `U+1F600` gives `3F 3F`
    /// under CP1252 *and* CP932) and SQL Server
    /// (`DATALENGTH(CAST(N'😀' AS varchar(4)))` is 2). msodbcsql's non-Windows
    /// legs agree by a different route: `EncodingConverter::Convert`'s `EILSEQ`
    /// arm calls `SkipSingleCh()`, which advances one `WCHAR`, then writes one
    /// `DefaultChar` (`Globalization.h`). Counting scalars would make the value
    /// a byte shorter here than through msodbcsql on every platform.
    #[test]
    fn encode_narrow_substitutes_one_byte_per_utf16_unit() {
        let collation = SqlCollation {
            info: 0x0409, // Windows-1252
            lcid_language_id: 0,
            col_flags: 0,
            sort_id: 0,
        };
        let encoded = encode_narrow("\u{1F600}", collation);
        assert_eq!(encoded.bytes, b"??", "one substitute per UTF-16 unit");
        assert!(encoded.had_loss);

        // Mixed with a BMP unmappable, which stays one byte.
        assert_eq!(encode_narrow("\u{65e5}\u{1F600}", collation).bytes, b"???");
    }

    #[test]
    fn sort_id_overrides_lcid_for_decode_and_encode() {
        for (sort_id, bytes, text) in [
            (30, b"\x82\xb3\xe0".as_slice(), "é│α"),
            (40, b"\x82\xb3\xd0".as_slice(), "é│ð"),
            (50, b"\xe9".as_slice(), "é"),
            (85, b"\xa3".as_slice(), "Ł"),
            (105, b"\xc6".as_slice(), "Ж"),
            (112, b"\xc1".as_slice(), "Α"),
            (122, b"\xc1".as_slice(), "Α"),
            (130, b"\xd0".as_slice(), "Ğ"),
            (137, b"\xe0".as_slice(), "א"),
            (145, b"\xc7".as_slice(), "ا"),
            (153, b"\xc0".as_slice(), "Ą"),
            (183, b"\xe9".as_slice(), "é"),
            (192, b"\x82\xa0".as_slice(), "あ"),
            (194, b"\xb0\xa1".as_slice(), "가"),
            (196, b"\xa4\xa4".as_slice(), "中"),
            (203, b"\xd6\xd0".as_slice(), "中"),
            (204, b"\xa1".as_slice(), "ก"),
            (210, b"\xe9".as_slice(), "é"),
            (217, b"\xe9".as_slice(), "é"),
        ] {
            let collation = SqlCollation {
                info: 0x0409,
                lcid_language_id: 0,
                col_flags: 0,
                sort_id,
            };
            let encoding = EncodingType::LcidBased(collation);
            assert_eq!(SqlString::decode(bytes, encoding), text);
            assert_eq!(encode_narrow(text, collation).bytes, bytes);
            if matches!(sort_id, 30 | 40) {
                assert_eq!(encoding.encoding(), None);
            }
        }
    }

    #[test]
    fn sort_id_table_matches_reference_assignments() {
        // msodbcsql Sql/Common/include/tdssort.h, x_rguiCodepageFromSortid.
        // SQL Server 2022 also emits 122 and 210..=217, absent from that header.
        for (sort_id, actual) in CODE_PAGE_FROM_SORT_ID.iter().enumerate() {
            let expected = match sort_id {
                30..=34 => Some(437),
                40..=44 | 49 | 55..=61 => Some(850),
                50..=54 | 71..=75 | 183..=186 | 210..=217 => Some(1252),
                80..=97 => Some(1250),
                104..=108 => Some(1251),
                112..=114 | 120..=122 | 124 => Some(1253),
                128..=130 => Some(1254),
                136..=138 => Some(1255),
                144..=146 => Some(1256),
                152..=160 => Some(1257),
                192 | 193 | 200 => Some(932),
                194 | 195 | 201 => Some(949),
                196 | 197 | 202 => Some(950),
                198 | 199 | 203 => Some(936),
                204..=206 => Some(874),
                _ => None,
            };
            assert_eq!(*actual, expected, "sort ID {sort_id}");
        }
    }

    #[test]
    fn every_sort_table_entry_resolves_before_unknown_lcid() {
        for (sort_id, code_page) in CODE_PAGE_FROM_SORT_ID.iter().enumerate() {
            let Some(code_page) = code_page else {
                continue;
            };
            let collation = SqlCollation {
                info: 0x000f_ffff,
                lcid_language_id: 0,
                col_flags: 0,
                sort_id: sort_id as u8,
            };
            let expected = match code_page {
                437 => "IBM437",
                850 => "IBM850",
                874 => "windows-874",
                932 => "Shift_JIS",
                936 => "GBK",
                949 => "EUC-KR",
                950 => "Big5",
                _ => "",
            };
            let expected = if expected.is_empty() {
                format!("windows-{code_page}")
            } else {
                expected.to_string()
            };
            assert_eq!(resolve_collation(collation).name(), expected);
        }
    }

    #[test]
    fn utf8_overrides_every_sort_id_and_lcid() {
        for sort_id in 0..=255 {
            let collation = SqlCollation {
                info: 0x0400_0409,
                lcid_language_id: 0,
                col_flags: 0x40,
                sort_id,
            };
            let encoding = EncodingType::LcidBased(collation);
            assert_eq!(encoding.encoding(), Some(encoding_rs::UTF_8));
            assert_eq!(SqlString::decode("é😀".as_bytes(), encoding), "é😀");
            assert_eq!(encode_narrow("é😀", collation).bytes, "é😀".as_bytes());
        }
    }

    #[test]
    fn absent_or_unknown_sort_id_preserves_lcid_and_unknown_lcid_fallback() {
        for sort_id in [0, 1, 255] {
            for (lcid, expected, text) in [
                (0x0409, encoding_rs::WINDOWS_1252, "Æ"),
                (0x0419, encoding_rs::WINDOWS_1251, "Ж"),
                (0x000f_ffff, encoding_rs::WINDOWS_1252, "Æ"),
            ] {
                let collation = SqlCollation {
                    info: lcid | 0x2000_0000,
                    lcid_language_id: 0,
                    col_flags: 0,
                    sort_id,
                };
                let encoding = EncodingType::LcidBased(collation);
                assert_eq!(encoding.encoding(), Some(expected));
                assert_eq!(SqlString::decode(b"\xc6", encoding), text);
                assert_eq!(encode_narrow(text, collation).bytes, b"\xc6");
            }
        }
        assert_eq!(EncodingType::DelayedSet.resolved_encoding(), None);
    }
}
