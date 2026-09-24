// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use encoding_rs::{CoderResult, Decoder, DecoderResult, Encoding};
use std::borrow::Cow;

/// A wire encoding, including OEM code pages that `encoding_rs` does not support.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ResolvedEncoding {
    /// An encoding supported by `encoding_rs`.
    EncodingRs(&'static Encoding),
    /// OEM United States code page.
    Oem437,
    /// OEM multilingual Latin code page.
    Oem850,
}

impl From<&'static Encoding> for ResolvedEncoding {
    fn from(encoding: &'static Encoding) -> Self {
        Self::EncodingRs(encoding)
    }
}

impl ResolvedEncoding {
    /// Returns `None` for OEM encodings, which require the table-backed codec.
    pub fn as_encoding_rs(self) -> Option<&'static Encoding> {
        match self {
            Self::EncodingRs(encoding) => Some(encoding),
            Self::Oem437 | Self::Oem850 => None,
        }
    }

    /// Canonical encoding name for diagnostics.
    pub fn name(self) -> &'static str {
        match self {
            Self::EncodingRs(encoding) => encoding.name(),
            Self::Oem437 => "IBM437",
            Self::Oem850 => "IBM850",
        }
    }

    /// Decodes without treating BOM-shaped bytes as an encoding override.
    /// The boolean reports malformed input, as in `encoding_rs`.
    pub fn decode_without_bom_handling(self, bytes: &[u8]) -> (Cow<'_, str>, bool) {
        let table = match self {
            Self::EncodingRs(encoding) => return encoding.decode_without_bom_handling(bytes),
            Self::Oem437 => &CP437,
            Self::Oem850 => &CP850,
        };
        if bytes.is_ascii() {
            return (String::from_utf8_lossy(bytes), false);
        }
        (
            Cow::Owned(bytes.iter().map(|&byte| decode_oem(table, byte)).collect()),
            false,
        )
    }

    /// Encodes text, replacing unmappable characters with decimal HTML numeric
    /// character references, matching `encoding_rs`. Returns the encoding used
    /// and whether any characters were unmappable.
    pub fn encode(self, text: &str) -> (Cow<'_, [u8]>, Self, bool) {
        let table = match self {
            Self::EncodingRs(encoding) => {
                let (bytes, used, errors) = encoding.encode(text);
                return (bytes, used.into(), errors);
            }
            Self::Oem437 => &CP437,
            Self::Oem850 => &CP850,
        };
        if text.is_ascii() {
            return (Cow::Borrowed(text.as_bytes()), self, false);
        }
        let mut bytes = Vec::with_capacity(text.len());
        let mut errors = false;
        // Non-ASCII uses a bounded 128-entry scan to keep one mapping table
        // rather than allocate and maintain a separate reverse index.
        for character in text.chars() {
            if character.is_ascii() {
                bytes.push(character as u8);
            } else if let Some(index) = table.iter().position(|&entry| entry == character) {
                bytes.push(index as u8 + 128);
            } else {
                bytes.extend_from_slice(format!("&#{};", u32::from(character)).as_bytes());
                errors = true;
            }
        }
        (Cow::Owned(bytes), self, errors)
    }

    /// Creates an incremental decoder that preserves BOMs as payload.
    pub fn new_decoder_without_bom_handling(self) -> ResolvedDecoder {
        ResolvedDecoder {
            inner: match self {
                Self::EncodingRs(encoding) => {
                    DecoderKind::EncodingRs(Box::new(encoding.new_decoder_without_bom_handling()))
                }
                Self::Oem437 => DecoderKind::Oem(&CP437),
                Self::Oem850 => DecoderKind::Oem(&CP850),
            },
        }
    }

    /// Creates an incremental decoder, returning `None` if its storage cannot
    /// be allocated. OEM decoders do not require heap storage.
    pub fn try_new_decoder_without_bom_handling(self) -> Option<ResolvedDecoder> {
        let inner = match self {
            Self::EncodingRs(encoding) => {
                let layout = std::alloc::Layout::new::<Decoder>();
                // SAFETY: Decoder has nonzero size. A successful allocation is
                // aligned for Decoder, initialized once, and transferred to Box
                // using the same global allocator and layout.
                let decoder = unsafe {
                    let ptr = std::alloc::alloc(layout).cast::<Decoder>();
                    if ptr.is_null() {
                        return None;
                    }
                    ptr.write(encoding.new_decoder_without_bom_handling());
                    Box::from_raw(ptr)
                };
                DecoderKind::EncodingRs(decoder)
            }
            Self::Oem437 => DecoderKind::Oem(&CP437),
            Self::Oem850 => DecoderKind::Oem(&CP850),
        };
        Some(ResolvedDecoder { inner })
    }
}

/// Incremental decoding with `encoding_rs` buffer and result conventions.
/// OEM input bytes always map to one BMP scalar, including ASCII controls.
pub struct ResolvedDecoder {
    inner: DecoderKind,
}

enum DecoderKind {
    EncodingRs(Box<Decoder>),
    Oem(&'static [char; 128]),
}

impl ResolvedDecoder {
    /// Whether an ASCII-compatible narrow decoder holds an incomplete character.
    /// Query before finalization; OEM encodings never buffer source bytes.
    pub fn has_pending_narrow_character(&self) -> bool {
        match &self.inner {
            DecoderKind::EncodingRs(decoder) => {
                debug_assert!(decoder.encoding().is_ascii_compatible());
                decoder.latin1_byte_compatible_up_to(&[]).is_none()
            }
            DecoderKind::Oem(_) => false,
        }
    }

    /// Upper bound on UTF-8 output, including any buffered partial input.
    /// Returns `None` if the bound overflows `usize`.
    pub fn max_utf8_buffer_length(&self, byte_length: usize) -> Option<usize> {
        match &self.inner {
            DecoderKind::EncodingRs(decoder) => decoder.max_utf8_buffer_length(byte_length),
            DecoderKind::Oem(_) => byte_length.checked_mul(3),
        }
    }

    /// Upper bound on UTF-16 code units, including any buffered partial input.
    pub fn max_utf16_buffer_length(&self, byte_length: usize) -> Option<usize> {
        match &self.inner {
            DecoderKind::EncodingRs(decoder) => decoder.max_utf16_buffer_length(byte_length),
            DecoderKind::Oem(_) => Some(byte_length),
        }
    }

    /// Returns status, bytes consumed, bytes written, and whether input was malformed.
    /// `OutputFull` never consumes a character that does not fit in the output.
    pub fn decode_to_utf8(
        &mut self,
        src: &[u8],
        dst: &mut [u8],
        last: bool,
    ) -> (CoderResult, usize, usize, bool) {
        let table = match &mut self.inner {
            DecoderKind::EncodingRs(decoder) => return decoder.decode_to_utf8(src, dst, last),
            DecoderKind::Oem(table) => table,
        };
        let mut written = 0;
        for (read, &byte) in src.iter().enumerate() {
            let character = decode_oem(table, byte);
            if dst.len() - written < character.len_utf8() {
                return (CoderResult::OutputFull, read, written, false);
            }
            written += character.encode_utf8(&mut dst[written..]).len();
        }
        (CoderResult::InputEmpty, src.len(), written, false)
    }

    /// Like `encoding_rs`, stops at malformed input and reports its source length
    /// and the number of bytes consumed after it, without emitting U+FFFD.
    pub fn decode_to_utf8_without_replacement(
        &mut self,
        src: &[u8],
        dst: &mut [u8],
        last: bool,
    ) -> (DecoderResult, usize, usize) {
        if let DecoderKind::EncodingRs(decoder) = &mut self.inner {
            return decoder.decode_to_utf8_without_replacement(src, dst, last);
        }
        let (result, read, written, _) = self.decode_to_utf8(src, dst, last);
        let result = match result {
            CoderResult::InputEmpty => DecoderResult::InputEmpty,
            CoderResult::OutputFull => DecoderResult::OutputFull,
        };
        (result, read, written)
    }

    /// Returns status, bytes consumed, code units written, and whether input was malformed.
    pub fn decode_to_utf16(
        &mut self,
        src: &[u8],
        dst: &mut [u16],
        last: bool,
    ) -> (CoderResult, usize, usize, bool) {
        let table = match &mut self.inner {
            DecoderKind::EncodingRs(decoder) => return decoder.decode_to_utf16(src, dst, last),
            DecoderKind::Oem(table) => table,
        };
        let count = src.len().min(dst.len());
        for (&byte, unit) in src.iter().zip(dst.iter_mut()) {
            *unit = decode_oem(table, byte) as u16;
        }
        let result = if count == src.len() {
            CoderResult::InputEmpty
        } else {
            CoderResult::OutputFull
        };
        (result, count, count, false)
    }
}

fn decode_oem(table: &[char; 128], byte: u8) -> char {
    if byte < 128 {
        char::from(byte)
    } else {
        table[usize::from(byte - 128)]
    }
}

// Standard IBM mappings, generated from Python's cp437/cp850 codecs. Bytes
// 0x00..0x7F retain ASCII semantics, not the DOS screen-font glyphs.
const CP437: [char; 128] = [
    '\u{00c7}', '\u{00fc}', '\u{00e9}', '\u{00e2}', '\u{00e4}', '\u{00e0}', '\u{00e5}', '\u{00e7}',
    '\u{00ea}', '\u{00eb}', '\u{00e8}', '\u{00ef}', '\u{00ee}', '\u{00ec}', '\u{00c4}', '\u{00c5}',
    '\u{00c9}', '\u{00e6}', '\u{00c6}', '\u{00f4}', '\u{00f6}', '\u{00f2}', '\u{00fb}', '\u{00f9}',
    '\u{00ff}', '\u{00d6}', '\u{00dc}', '\u{00a2}', '\u{00a3}', '\u{00a5}', '\u{20a7}', '\u{0192}',
    '\u{00e1}', '\u{00ed}', '\u{00f3}', '\u{00fa}', '\u{00f1}', '\u{00d1}', '\u{00aa}', '\u{00ba}',
    '\u{00bf}', '\u{2310}', '\u{00ac}', '\u{00bd}', '\u{00bc}', '\u{00a1}', '\u{00ab}', '\u{00bb}',
    '\u{2591}', '\u{2592}', '\u{2593}', '\u{2502}', '\u{2524}', '\u{2561}', '\u{2562}', '\u{2556}',
    '\u{2555}', '\u{2563}', '\u{2551}', '\u{2557}', '\u{255d}', '\u{255c}', '\u{255b}', '\u{2510}',
    '\u{2514}', '\u{2534}', '\u{252c}', '\u{251c}', '\u{2500}', '\u{253c}', '\u{255e}', '\u{255f}',
    '\u{255a}', '\u{2554}', '\u{2569}', '\u{2566}', '\u{2560}', '\u{2550}', '\u{256c}', '\u{2567}',
    '\u{2568}', '\u{2564}', '\u{2565}', '\u{2559}', '\u{2558}', '\u{2552}', '\u{2553}', '\u{256b}',
    '\u{256a}', '\u{2518}', '\u{250c}', '\u{2588}', '\u{2584}', '\u{258c}', '\u{2590}', '\u{2580}',
    '\u{03b1}', '\u{00df}', '\u{0393}', '\u{03c0}', '\u{03a3}', '\u{03c3}', '\u{00b5}', '\u{03c4}',
    '\u{03a6}', '\u{0398}', '\u{03a9}', '\u{03b4}', '\u{221e}', '\u{03c6}', '\u{03b5}', '\u{2229}',
    '\u{2261}', '\u{00b1}', '\u{2265}', '\u{2264}', '\u{2320}', '\u{2321}', '\u{00f7}', '\u{2248}',
    '\u{00b0}', '\u{2219}', '\u{00b7}', '\u{221a}', '\u{207f}', '\u{00b2}', '\u{25a0}', '\u{00a0}',
];

const CP850: [char; 128] = [
    '\u{00c7}', '\u{00fc}', '\u{00e9}', '\u{00e2}', '\u{00e4}', '\u{00e0}', '\u{00e5}', '\u{00e7}',
    '\u{00ea}', '\u{00eb}', '\u{00e8}', '\u{00ef}', '\u{00ee}', '\u{00ec}', '\u{00c4}', '\u{00c5}',
    '\u{00c9}', '\u{00e6}', '\u{00c6}', '\u{00f4}', '\u{00f6}', '\u{00f2}', '\u{00fb}', '\u{00f9}',
    '\u{00ff}', '\u{00d6}', '\u{00dc}', '\u{00f8}', '\u{00a3}', '\u{00d8}', '\u{00d7}', '\u{0192}',
    '\u{00e1}', '\u{00ed}', '\u{00f3}', '\u{00fa}', '\u{00f1}', '\u{00d1}', '\u{00aa}', '\u{00ba}',
    '\u{00bf}', '\u{00ae}', '\u{00ac}', '\u{00bd}', '\u{00bc}', '\u{00a1}', '\u{00ab}', '\u{00bb}',
    '\u{2591}', '\u{2592}', '\u{2593}', '\u{2502}', '\u{2524}', '\u{00c1}', '\u{00c2}', '\u{00c0}',
    '\u{00a9}', '\u{2563}', '\u{2551}', '\u{2557}', '\u{255d}', '\u{00a2}', '\u{00a5}', '\u{2510}',
    '\u{2514}', '\u{2534}', '\u{252c}', '\u{251c}', '\u{2500}', '\u{253c}', '\u{00e3}', '\u{00c3}',
    '\u{255a}', '\u{2554}', '\u{2569}', '\u{2566}', '\u{2560}', '\u{2550}', '\u{256c}', '\u{00a4}',
    '\u{00f0}', '\u{00d0}', '\u{00ca}', '\u{00cb}', '\u{00c8}', '\u{0131}', '\u{00cd}', '\u{00ce}',
    '\u{00cf}', '\u{2518}', '\u{250c}', '\u{2588}', '\u{2584}', '\u{00a6}', '\u{00cc}', '\u{2580}',
    '\u{00d3}', '\u{00df}', '\u{00d4}', '\u{00d2}', '\u{00f5}', '\u{00d5}', '\u{00b5}', '\u{00fe}',
    '\u{00de}', '\u{00da}', '\u{00db}', '\u{00d9}', '\u{00fd}', '\u{00dd}', '\u{00af}', '\u{00b4}',
    '\u{00ad}', '\u{00b1}', '\u{2017}', '\u{00be}', '\u{00b6}', '\u{00a7}', '\u{00f7}', '\u{00b8}',
    '\u{00b0}', '\u{00a8}', '\u{00b7}', '\u{00b9}', '\u{00b3}', '\u{00b2}', '\u{25a0}', '\u{00a0}',
];

#[cfg(test)]
mod tests {
    #[test]
    fn fallible_decoder_creation_supports_narrow_and_oem_encodings() {
        for encoding in [
            super::ResolvedEncoding::from(encoding_rs::UTF_8),
            super::ResolvedEncoding::Oem437,
            super::ResolvedEncoding::Oem850,
        ] {
            let mut decoder = encoding.try_new_decoder_without_bom_handling().unwrap();
            let mut output = [0; 16];
            let (result, consumed, written, errors) =
                decoder.decode_to_utf8(b"42", &mut output, true);
            assert_eq!(result, encoding_rs::CoderResult::InputEmpty);
            assert_eq!((consumed, written, errors), (2, 2, false));
            assert_eq!(&output[..written], b"42");
        }
    }

    use super::*;

    #[test]
    fn finalizing_without_replacement_reports_buffered_source() {
        for (encoding, bytes, held) in [
            (encoding_rs::SHIFT_JIS, &b"A\x82"[..], 1),
            (encoding_rs::GBK, &b"A\xc4"[..], 1),
            (encoding_rs::GBK, &b"A\x81\x30"[..], 2),
            (encoding_rs::GBK, &b"A\x81\x30\x81"[..], 3),
            (encoding_rs::BIG5, &b"A\xa4"[..], 1),
            (encoding_rs::EUC_KR, &b"A\xb0"[..], 1),
        ] {
            for split in 0..=bytes.len() {
                let mut decoder =
                    ResolvedEncoding::from(encoding).new_decoder_without_bom_handling();
                let mut output = [0; 32];
                let (result, read, written, _) =
                    decoder.decode_to_utf8(&bytes[..split], &mut output, false);
                assert_eq!(result, CoderResult::InputEmpty);
                assert_eq!(read, split);
                let (result, read, rest, _) =
                    decoder.decode_to_utf8(&bytes[split..], &mut output[written..], false);
                assert_eq!(result, CoderResult::InputEmpty);
                assert_eq!(read, bytes.len() - split);
                assert_eq!(&output[..written + rest], b"A");
                let (result, read, written) =
                    decoder.decode_to_utf8_without_replacement(&[], &mut output, true);
                assert_eq!((read, written), (0, 0));
                let DecoderResult::Malformed(length, after) = result else {
                    panic!("expected buffered source for {}", encoding.name());
                };
                assert_eq!(length + after, held, "{}", encoding.name());
            }
        }
        for encoding in [
            ResolvedEncoding::Oem437,
            ResolvedEncoding::Oem850,
            encoding_rs::WINDOWS_1252.into(),
        ] {
            let mut decoder = encoding.new_decoder_without_bom_handling();
            assert_eq!(
                decoder.decode_to_utf8_without_replacement(b"\x82", &mut [], false),
                (DecoderResult::OutputFull, 0, 0)
            );
            let (result, read, written) =
                decoder.decode_to_utf8_without_replacement(b"\x82", &mut [0; 8], false);
            assert_eq!((result, read), (DecoderResult::InputEmpty, 1));
            assert!(written > 1);
            assert_eq!(
                decoder.decode_to_utf8_without_replacement(&[], &mut [0; 8], true),
                (DecoderResult::InputEmpty, 0, 0)
            );
        }
    }

    // Unicode output of bytes(range(128, 256)).decode("cp437"/"cp850").
    const OEM_FIXTURES: [(ResolvedEncoding, &str); 2] = [
        (
            ResolvedEncoding::Oem437,
            concat!(
                "ÇüéâäàåçêëèïîìÄÅÉæÆôöòûùÿÖÜ¢£¥₧ƒ",
                "áíóúñÑªº¿⌐¬½¼¡«»░▒▓│┤╡╢╖╕╣║╗╝╜╛┐",
                "└┴┬├─┼╞╟╚╔╩╦╠═╬╧╨╤╥╙╘╒╓╫╪┘┌█▄▌▐▀",
                "αßΓπΣσµτΦΘΩδ∞φε∩≡±≥≤⌠⌡÷≈°∙·√ⁿ²■\u{a0}",
            ),
        ),
        (
            ResolvedEncoding::Oem850,
            concat!(
                "ÇüéâäàåçêëèïîìÄÅÉæÆôöòûùÿÖÜø£Ø×ƒ",
                "áíóúñÑªº¿®¬½¼¡«»░▒▓│┤ÁÂÀ©╣║╗╝¢¥┐",
                "└┴┬├─┼ãÃ╚╔╩╦╠═╬¤ðÐÊËÈıÍÎÏ┘┌█▄¦Ì▀",
                "ÓßÔÒõÕµþÞÚÛÙýÝ¯´\u{ad}±‗¾¶§÷¸°¨·¹³²■\u{a0}",
            ),
        ),
    ];

    #[test]
    fn oem_all_bytes_materialized_roundtrip() {
        let bytes: Vec<u8> = (0..=255).collect();
        for (encoding, high_half) in OEM_FIXTURES {
            let expected: String = (0..128).map(char::from).chain(high_half.chars()).collect();
            let (decoded, errors) = encoding.decode_without_bom_handling(&bytes);
            assert!(!errors);
            assert_eq!(decoded, expected);
            let (encoded, used, errors) = encoding.encode(&expected);
            assert_eq!(encoded, bytes);
            assert_eq!(used, encoding);
            assert!(!errors);
        }
    }

    #[test]
    fn oem_all_bytes_incremental_output_boundaries() {
        for (encoding, high_half) in OEM_FIXTURES {
            let expected: Vec<char> = (0..128).map(char::from).chain(high_half.chars()).collect();
            let mut utf8_decoder = encoding.new_decoder_without_bom_handling();
            let mut utf16_decoder = encoding.new_decoder_without_bom_handling();
            assert!(!utf8_decoder.has_pending_narrow_character());
            assert!(!utf16_decoder.has_pending_narrow_character());
            for (byte, character) in (0..=255).zip(expected) {
                for capacity in 0..character.len_utf8() {
                    let mut output = [0; 3];
                    assert_eq!(
                        utf8_decoder.decode_to_utf8(&[byte], &mut output[..capacity], false),
                        (CoderResult::OutputFull, 0, 0, false),
                    );
                }
                let mut output = [0; 3];
                assert_eq!(
                    utf8_decoder.decode_to_utf8(&[byte], &mut output, false),
                    (CoderResult::InputEmpty, 1, character.len_utf8(), false),
                );
                assert_eq!(
                    std::str::from_utf8(&output[..character.len_utf8()]).unwrap(),
                    character.to_string(),
                );
                assert_eq!(
                    utf16_decoder.decode_to_utf16(&[byte], &mut [], false),
                    (CoderResult::OutputFull, 0, 0, false),
                );
                let mut output = [0; 1];
                assert_eq!(
                    utf16_decoder.decode_to_utf16(&[byte], &mut output, false),
                    (CoderResult::InputEmpty, 1, 1, false),
                );
                assert_eq!(output[0], character as u16);
                assert!(!utf8_decoder.has_pending_narrow_character());
                assert!(!utf16_decoder.has_pending_narrow_character());
            }
            assert_eq!(
                utf8_decoder.decode_to_utf8(&[], &mut [], true),
                (CoderResult::InputEmpty, 0, 0, false),
            );
            assert_eq!(
                utf16_decoder.decode_to_utf16(&[], &mut [], true),
                (CoderResult::InputEmpty, 0, 0, false),
            );
        }
    }

    #[test]
    fn oem_partial_output_consumes_only_complete_characters() {
        for (encoding, _) in OEM_FIXTURES {
            let mut decoder = encoding.new_decoder_without_bom_handling();
            let mut utf8 = [0; 3];
            assert_eq!(
                decoder.decode_to_utf8(b"A\xb3Z", &mut utf8, false),
                (CoderResult::OutputFull, 1, 1, false),
            );
            assert_eq!(utf8[0], b'A');
            assert_eq!(
                decoder.decode_to_utf8(b"\xb3Z", &mut utf8, true),
                (CoderResult::OutputFull, 1, 3, false),
            );
            assert_eq!(&utf8, "│".as_bytes());
            assert_eq!(
                decoder.decode_to_utf8(b"Z", &mut utf8, true),
                (CoderResult::InputEmpty, 1, 1, false),
            );
            let mut utf16 = [0; 2];
            assert_eq!(
                decoder.decode_to_utf16(b"A\xb3Z", &mut utf16, true),
                (CoderResult::OutputFull, 2, 2, false),
            );
            assert_eq!(utf16, [b'A' as u16, '│' as u16]);
        }
    }

    #[test]
    fn oem_ascii_empty_and_unmappable_characters() {
        let ascii: String = (0..128).map(char::from).collect();
        for (encoding, _) in OEM_FIXTURES {
            for text in ["", ascii.as_str()] {
                let (decoded, errors) = encoding.decode_without_bom_handling(text.as_bytes());
                assert!(matches!(decoded, Cow::Borrowed(_)));
                assert_eq!(decoded, text);
                assert!(!errors);
                let (encoded, used, errors) = encoding.encode(text);
                assert!(matches!(encoded, Cow::Borrowed(_)));
                assert_eq!(encoded, text.as_bytes());
                assert_eq!(used, encoding);
                assert!(!errors);
            }
            let (encoded, _, errors) = encoding.encode("é日😀\u{10ffff}");
            assert_eq!(encoded.as_ref(), b"\x82&#26085;&#128512;&#1114111;");
            assert!(errors);
            assert_eq!(encoding.as_encoding_rs(), None);
            assert!(encoding.name().starts_with("IBM"));
        }
    }

    #[test]
    fn oem_buffer_bounds() {
        for (encoding, _) in OEM_FIXTURES {
            let decoder = encoding.new_decoder_without_bom_handling();
            assert_eq!(decoder.max_utf8_buffer_length(0), Some(0));
            assert_eq!(decoder.max_utf8_buffer_length(256), Some(768));
            assert_eq!(decoder.max_utf8_buffer_length(usize::MAX), None);
            assert_eq!(decoder.max_utf16_buffer_length(0), Some(0));
            assert_eq!(decoder.max_utf16_buffer_length(256), Some(256));
            assert_eq!(
                decoder.max_utf16_buffer_length(usize::MAX),
                Some(usize::MAX)
            );
        }
    }

    #[test]
    fn encoding_rs_incremental_sequences_and_bom_preservation() {
        for (encoding, input, expected) in [
            (encoding_rs::UTF_8, "é😀".as_bytes(), "é😀"),
            (encoding_rs::UTF_16LE, &[0xff, 0xfe, 0x41, 0], "\u{feff}A"),
            (encoding_rs::SHIFT_JIS, &[0x82, 0xa0], "あ"),
        ] {
            let encoding = ResolvedEncoding::from(encoding);
            assert_eq!(encoding.decode_without_bom_handling(input).0, expected);
            let mut utf8_decoder = encoding.new_decoder_without_bom_handling();
            let mut utf16_decoder = encoding.new_decoder_without_bom_handling();
            let mut utf8 = Vec::new();
            let mut utf16 = Vec::new();
            for (index, byte) in input.iter().enumerate() {
                let last = index == input.len() - 1;
                let mut output = vec![0; utf8_decoder.max_utf8_buffer_length(1).unwrap()];
                let (status, read, written, errors) =
                    utf8_decoder.decode_to_utf8(&[*byte], &mut output, last);
                assert_eq!((status, read, errors), (CoderResult::InputEmpty, 1, false));
                utf8.extend_from_slice(&output[..written]);
                let mut output = vec![0; utf16_decoder.max_utf16_buffer_length(1).unwrap()];
                let (status, read, written, errors) =
                    utf16_decoder.decode_to_utf16(&[*byte], &mut output, last);
                assert_eq!((status, read, errors), (CoderResult::InputEmpty, 1, false));
                utf16.extend_from_slice(&output[..written]);
            }
            assert_eq!(utf8, expected.as_bytes());
            assert_eq!(utf16, expected.encode_utf16().collect::<Vec<_>>());
        }
    }

    #[test]
    fn encoding_rs_malformed_final_input() {
        let encoding = ResolvedEncoding::from(encoding_rs::UTF_8);
        assert_eq!(encoding.as_encoding_rs(), Some(encoding_rs::UTF_8));
        assert_eq!(encoding.name(), "UTF-8");
        assert_eq!(
            encoding.decode_without_bom_handling(b"\xc3"),
            (Cow::Borrowed("\u{fffd}"), true),
        );
        let mut decoder = encoding.new_decoder_without_bom_handling();
        let mut output = [0; 4];
        assert_eq!(
            decoder.decode_to_utf8(b"\xc3", &mut output, false),
            (CoderResult::InputEmpty, 1, 0, false),
        );
        assert_eq!(
            decoder.decode_to_utf8(&[], &mut output, true),
            (CoderResult::InputEmpty, 0, 3, true),
        );
        assert_eq!(&output[..3], "\u{fffd}".as_bytes());
    }
}
