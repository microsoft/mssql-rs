// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Connection-local encoding of application `SQL_C_CHAR` values, independent of
//! the server collation. Inputs are complete Unicode text chunks; this codec
//! does not retain streaming state or alter the process locale.

use std::borrow::Cow;

use crate::api::sqlstate::{DiagMsg, ERR_INTERNAL_CONVERSION, ERR_INVALID_CHARACTER_VALUE};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ClientEncoding(u32);

#[derive(Debug)]
pub(crate) struct ClientEncoded<'a> {
    pub(crate) bytes: Cow<'a, [u8]>,
    pub(crate) had_loss: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allocation_capacity_overflow_is_a_conversion_error() {
        assert_eq!(
            zeroed_buffer::<u8>(usize::MAX).unwrap_err().state,
            ERR_INTERNAL_CONVERSION.state
        );
        assert!(zeroed_buffer::<u8>(0).unwrap().is_empty());
    }

    #[test]
    fn utf8_and_ascii_borrow_the_input() {
        for code_page in [65001, 1252, 932] {
            let encoding = ClientEncoding::for_code_page(code_page).unwrap();
            assert!(encoding.is_ascii_compatible());
            for text in ["", "ascii\0with nul"] {
                let encoded = encoding.encode(text).unwrap();
                assert!(matches!(encoded.bytes, Cow::Borrowed(_)));
                assert_eq!(&*encoded.bytes, text.as_bytes());
                assert!(!encoded.had_loss);
                assert!(matches!(
                    encoding.decode(text.as_bytes()).unwrap(),
                    Cow::Borrowed(_)
                ));
            }
        }
        let text = "é😀";
        assert!(matches!(
            ClientEncoding::UTF8.encode(text).unwrap().bytes,
            Cow::Borrowed(_)
        ));
        assert!(ClientEncoding::for_code_page(u32::MAX).is_none());
    }

    #[test]
    fn utf8_boundaries_and_strict_decoding() {
        let encoding = ClientEncoding::UTF8;
        let bytes = "Aé😀\0Z".as_bytes();
        for (capacity, expected) in [
            (0, 0),
            (1, 1),
            (2, 1),
            (3, 3),
            (6, 3),
            (7, 7),
            (8, 8),
            (usize::MAX, 9),
        ] {
            assert_eq!(encoding.prefix_len(bytes, capacity).unwrap(), expected);
        }
        for bytes in [&b"\xff"[..], &b"\xc3"[..], &b"\xed\xa0\x80"[..]] {
            assert_eq!(
                encoding.decode(bytes).unwrap_err().state,
                ERR_INVALID_CHARACTER_VALUE.state
            );
            assert_eq!(
                encoding.prefix_len(bytes, 1).unwrap_err().state,
                ERR_INVALID_CHARACTER_VALUE.state
            );
        }
    }

    #[test]
    fn client_byte_offsets_round_up_to_source_character_boundaries() {
        let encoding = ClientEncoding::for_code_page(932).unwrap();
        let text = "AあB";
        for (offset, expected) in [
            (0, 0),
            (1, 1),
            (2, "Aあ".len()),
            (3, "Aあ".len()),
            (4, text.len()),
            (usize::MAX, text.len()),
        ] {
            assert_eq!(
                encoding.utf8_offset_for_client_bytes(text, offset).unwrap(),
                expected
            );
        }
    }

    #[test]
    fn cp1252_round_trip_and_single_byte_boundaries() {
        let encoding = ClientEncoding::for_code_page(1252).unwrap();
        let text = "é €\0Z";
        let bytes = b"\xe9 \x80\0Z";
        let encoded = encoding.encode(text).unwrap();
        assert_eq!(&*encoded.bytes, bytes);
        assert!(!encoded.had_loss);
        assert_eq!(encoding.decode(bytes).unwrap(), text);
        for capacity in 0..=bytes.len() + 1 {
            assert_eq!(
                encoding.prefix_len(bytes, capacity).unwrap(),
                capacity.min(bytes.len())
            );
        }
    }

    #[test]
    fn unmappable_characters_use_native_substitution() {
        let encoding = ClientEncoding::for_code_page(1252).unwrap();
        let encoded = encoding.encode("A漢\0Z").unwrap();
        #[cfg(not(target_env = "musl"))]
        assert_eq!(&*encoded.bytes, b"A?\0Z");
        #[cfg(target_env = "musl")]
        assert_eq!(&*encoded.bytes, b"A*\0Z");
        #[cfg(any(target_env = "gnu", target_env = "musl"))]
        assert!(!encoded.had_loss);
        #[cfg(not(any(target_env = "gnu", target_env = "musl")))]
        assert!(encoded.had_loss);
        assert!(!encoded.bytes.windows(2).any(|pair| pair == b"&#"));
    }

    #[test]
    fn cp932_round_trip_and_complete_character_prefixes() {
        let encoding = ClientEncoding::for_code_page(932).unwrap();
        let text = "AあB\0";
        let bytes = b"A\x82\xa0B\0";
        let encoded = encoding.encode(text).unwrap();
        assert_eq!(&*encoded.bytes, bytes);
        assert!(!encoded.had_loss);
        assert_eq!(encoding.decode(bytes).unwrap(), text);
        for (capacity, expected) in [
            (0, 0),
            (1, 1),
            (2, 1),
            (3, 3),
            (4, 4),
            (5, 5),
            (usize::MAX, 5),
        ] {
            assert_eq!(encoding.prefix_len(bytes, capacity).unwrap(), expected);
        }
        let consecutive = encoding.encode("あい").unwrap();
        assert_eq!(encoding.prefix_len(&consecutive.bytes, 3).unwrap(), 2);
        let large = encoding
            .encode(&"あ".repeat(200))
            .unwrap()
            .bytes
            .into_owned();
        assert_eq!(encoding.prefix_len(&large, 399).unwrap(), 398);
    }

    #[test]
    fn malformed_code_page_input_is_not_silently_dropped() {
        let encoding = ClientEncoding::for_code_page(932).unwrap();
        match encoding.decode(b"A\x82") {
            Ok(decoded) => {
                assert!(decoded.starts_with('A'));
                assert_eq!(decoded.chars().count(), 2);
            }
            Err(diag) => assert_eq!(diag.state, ERR_INVALID_CHARACTER_VALUE.state),
        }
    }

    #[test]
    fn system_default_can_convert_ascii() {
        let encoding = ClientEncoding::system_default();
        assert_eq!(
            std::thread::spawn(ClientEncoding::system_default)
                .join()
                .unwrap(),
            encoding,
        );
        let encoded = encoding.encode("default\0encoding").unwrap();
        assert_eq!(
            encoding.decode(&encoded.bytes).unwrap(),
            "default\0encoding"
        );
    }

    #[cfg(windows)]
    #[test]
    fn windows_keeps_default_best_fit_and_checks_lengths() {
        let encoding = ClientEncoding::for_code_page(1252).unwrap();
        let encoded = encoding.encode("Ā").unwrap();
        assert_eq!(&*encoded.bytes, b"A");
        assert!(!encoded.had_loss);
    }
}

impl ClientEncoding {
    pub(crate) const UTF8: Self = Self(65001);

    /// Match `InitDbcCodePages` / `SystemLocale::AnsiCP`, not a server code page.
    pub(crate) fn system_default() -> Self {
        // Native SystemLocale::Singleton captures the locale on its first use.
        static DEFAULT: std::sync::OnceLock<ClientEncoding> = std::sync::OnceLock::new();
        *DEFAULT.get_or_init(platform::system_default)
    }

    pub(crate) fn is_utf8(self) -> bool {
        self == Self::UTF8
    }

    pub(crate) fn is_ascii_compatible(self) -> bool {
        self.0 != 12000
    }

    pub(crate) fn encode(self, text: &str) -> Result<ClientEncoded<'_>, DiagMsg> {
        if self.is_utf8() || (self.is_ascii_compatible() && text.is_ascii()) {
            return Ok(ClientEncoded {
                bytes: Cow::Borrowed(text.as_bytes()),
                had_loss: false,
            });
        }
        let (bytes, had_loss) = platform::encode(self, text)?;
        Ok(ClientEncoded {
            bytes: Cow::Owned(bytes),
            had_loss,
        })
    }

    pub(crate) fn decode(self, bytes: &[u8]) -> Result<Cow<'_, str>, DiagMsg> {
        if self.is_utf8() || (self.is_ascii_compatible() && bytes.is_ascii()) {
            return std::str::from_utf8(bytes)
                .map(Cow::Borrowed)
                .map_err(|error| {
                    tracing::error!(%error, "Invalid UTF-8 application character value");
                    ERR_INVALID_CHARACTER_VALUE
                });
        }
        platform::decode(self, bytes).map(Cow::Owned)
    }

    /// Returns the source UTF-8 offset after the client-encoded byte offset.
    /// An offset inside a multibyte character consumes that whole character;
    /// decoding a byte suffix at that position would fail or corrupt the carry.
    pub(crate) fn utf8_offset_for_client_bytes(
        self,
        text: &str,
        byte_offset: usize,
    ) -> Result<usize, DiagMsg> {
        if byte_offset == 0 {
            return Ok(0);
        }
        let mut encoded_bytes = 0_usize;
        for (source_offset, character) in text.char_indices() {
            let mut utf8 = [0; 4];
            let encoded = self.encode(character.encode_utf8(&mut utf8))?;
            encoded_bytes = encoded_bytes.saturating_add(encoded.bytes.len());
            if encoded_bytes >= byte_offset {
                return Ok(source_offset + character.len_utf8());
            }
        }
        Ok(text.len())
    }

    /// Bound-column truncation only. Resumable SQLGetData byte offsets need not
    /// fall on character boundaries and must not use this operation.
    pub(crate) fn prefix_len(self, bytes: &[u8], capacity: usize) -> Result<usize, DiagMsg> {
        let capacity = capacity.min(bytes.len());
        if self.is_utf8() {
            let text = self.decode(bytes)?;
            let mut end = capacity;
            while !text.is_char_boundary(end) {
                end -= 1;
            }
            return Ok(end);
        }
        if self.is_ascii_compatible() && bytes.is_ascii() {
            return Ok(capacity);
        }
        platform::prefix_len(self, bytes, capacity)
    }

    #[cfg(test)]
    pub(crate) fn for_code_page(code_page: u32) -> Option<Self> {
        platform::for_code_page(code_page)
    }
}

fn zeroed_buffer<T: Default + Clone>(len: usize) -> Result<Vec<T>, DiagMsg> {
    let mut buffer = Vec::new();
    buffer.try_reserve_exact(len).map_err(|error| {
        tracing::error!(%error, len, "Allocating client encoding buffer failed");
        ERR_INTERNAL_CONVERSION
    })?;
    buffer.resize(len, T::default());
    Ok(buffer)
}

#[cfg(unix)]
mod platform {
    use super::*;
    use std::ffi::CStr;

    #[cfg(not(target_os = "macos"))]
    use libc::{iconv, iconv_close, iconv_open};

    // libc's Apple declarations are deprecated, but Darwin's system libiconv
    // is the converter used by native ODBC, not an added package dependency.
    #[cfg(target_os = "macos")]
    #[link(name = "iconv")]
    unsafe extern "C" {
        fn iconv_open(to: *const libc::c_char, from: *const libc::c_char) -> libc::iconv_t;
        fn iconv(
            cd: libc::iconv_t,
            input: *mut *mut libc::c_char,
            input_left: *mut usize,
            output: *mut *mut libc::c_char,
            output_left: *mut usize,
        ) -> usize;
        fn iconv_close(cd: libc::iconv_t) -> libc::c_int;
    }

    fn charset(encoding: ClientEncoding, transliterate: bool) -> Result<&'static CStr, DiagMsg> {
        macro_rules! name {
            ($plain:expr, $translit:expr) => {
                if cfg!(target_env = "musl") || !transliterate {
                    $plain
                } else {
                    $translit
                }
            };
        }
        // LocalizationImpl.hpp cp_iconv::g_cp_iconv: retain true ISO-8859
        // mappings, rather than WHATWG aliases such as ISO-8859-1 => CP1252.
        Ok(match encoding.0 {
            65001 => c"UTF-8",
            12000 => c"UTF-32LE",
            437 => name!(c"CP437", c"CP437//TRANSLIT"),
            850 => name!(c"CP850", c"CP850//TRANSLIT"),
            874 => name!(c"CP874", c"CP874//TRANSLIT"),
            932 => name!(c"CP932", c"CP932//TRANSLIT"),
            936 => name!(c"CP936", c"CP936//TRANSLIT"),
            949 => name!(c"CP949", c"CP949//TRANSLIT"),
            950 => name!(c"CP950", c"CP950//TRANSLIT"),
            1250 => name!(c"CP1250", c"CP1250//TRANSLIT"),
            1251 => name!(c"CP1251", c"CP1251//TRANSLIT"),
            1252 => name!(c"CP1252", c"CP1252//TRANSLIT"),
            1253 => name!(c"CP1253", c"CP1253//TRANSLIT"),
            1254 => name!(c"CP1254", c"CP1254//TRANSLIT"),
            1255 => name!(c"CP1255", c"CP1255//TRANSLIT"),
            1256 => name!(c"CP1256", c"CP1256//TRANSLIT"),
            1257 => name!(c"CP1257", c"CP1257//TRANSLIT"),
            1258 => name!(c"CP1258", c"CP1258//TRANSLIT"),
            54936 => name!(c"GB18030", c"GB18030//TRANSLIT"),
            28591 => name!(c"ISO8859-1", c"ISO8859-1//TRANSLIT"),
            28592 => name!(c"ISO8859-2", c"ISO8859-2//TRANSLIT"),
            28593 => name!(c"ISO8859-3", c"ISO8859-3//TRANSLIT"),
            28594 => name!(c"ISO8859-4", c"ISO8859-4//TRANSLIT"),
            28595 => name!(c"ISO8859-5", c"ISO8859-5//TRANSLIT"),
            28596 => name!(c"ISO8859-6", c"ISO8859-6//TRANSLIT"),
            28597 => name!(c"ISO8859-7", c"ISO8859-7//TRANSLIT"),
            28598 => name!(c"ISO8859-8", c"ISO8859-8//TRANSLIT"),
            28599 => name!(c"ISO8859-9", c"ISO8859-9//TRANSLIT"),
            28603 => name!(c"ISO8859-13", c"ISO8859-13//TRANSLIT"),
            28605 => name!(c"ISO8859-15", c"ISO8859-15//TRANSLIT"),
            _ => return Err(ERR_INTERNAL_CONVERSION),
        })
    }

    fn from_codeset(codeset: &[u8]) -> ClientEncoding {
        for (alias, cp) in [
            (&b"utf8"[..], 65001),
            (&b"UTF-8"[..], 65001),
            (&b"BIG5"[..], 950),
            (&b"BIG5-HKSCS"[..], 950),
            (&b"gb18030"[..], 54936),
            (&b"gb2312"[..], 936),
            (&b"gbk"[..], 936),
            (&b"UTF-32LE"[..], 12000),
        ] {
            if codeset.eq_ignore_ascii_case(alias) {
                return ClientEncoding(cp);
            }
        }
        for cp in [
            437, 850, 874, 932, 936, 949, 950, 1250, 1251, 1252, 1253, 1254, 1255, 1256, 1257, 1258,
        ] {
            if charset(ClientEncoding(cp), false)
                .is_ok_and(|name| codeset.eq_ignore_ascii_case(name.to_bytes()))
            {
                return ClientEncoding(cp);
            }
        }
        for prefix in [
            &b"ISO-8859-"[..],
            &b"8859_"[..],
            &b"ISO8859-"[..],
            &b"ISO8859"[..],
            &b"ISO_8859-"[..],
            &b"ISO_8859_"[..],
        ] {
            if codeset
                .get(..prefix.len())
                .is_some_and(|head| head.eq_ignore_ascii_case(prefix))
            {
                let cp = match codeset.get(prefix.len()..) {
                    Some(b"1") => 28591,
                    Some(b"2") => 28592,
                    Some(b"3") => 28593,
                    Some(b"4") => 28594,
                    Some(b"5") => 28595,
                    Some(b"6") => 28596,
                    Some(b"7") => 28597,
                    Some(b"8") => 28598,
                    Some(b"9") => 28599,
                    Some(b"13") => 28603,
                    Some(b"15") => 28605,
                    _ => continue,
                };
                return ClientEncoding(cp);
            }
        }
        ClientEncoding::UTF8
    }

    pub(super) fn system_default() -> ClientEncoding {
        // SAFETY: nl_langinfo returns a NUL-terminated borrowed codeset name.
        // Read it immediately; never call setlocale or retain the pointer.
        let codeset = unsafe { libc::nl_langinfo(libc::CODESET) };
        if codeset.is_null() {
            return ClientEncoding::UTF8;
        }
        // SAFETY: The non-null result of nl_langinfo is NUL-terminated.
        from_codeset(unsafe { CStr::from_ptr(codeset) }.to_bytes())
    }

    struct Converter(libc::iconv_t);

    struct Converted {
        read: usize,
        written: usize,
        error: Option<std::io::Error>,
    }

    impl Converter {
        fn open(to: &CStr, from: &CStr) -> Result<Self, DiagMsg> {
            // SAFETY: Both names are valid NUL-terminated strings. The returned
            // descriptor is owned exclusively by this value and closed in Drop.
            let cd = unsafe { iconv_open(to.as_ptr(), from.as_ptr()) };
            if cd == usize::MAX as libc::iconv_t {
                let error = std::io::Error::last_os_error();
                tracing::error!(%error, ?to, ?from, "iconv_open failed");
                return Err(ERR_INTERNAL_CONVERSION);
            }
            Ok(Self(cd))
        }

        fn convert(&mut self, input: Option<&[u8]>, output: &mut [u8]) -> Converted {
            let flush = input.is_none();
            let input = input.unwrap_or_default();
            let mut input_ptr = input.as_ptr().cast::<libc::c_char>().cast_mut();
            let mut output_ptr = output.as_mut_ptr().cast::<libc::c_char>();
            let mut input_left = input.len();
            let mut output_left = output.len();
            // SAFETY: iconv only reads input, writes within output, and adjusts
            // these local pointers/counts. This descriptor is not shared.
            let result = unsafe {
                iconv(
                    self.0,
                    if flush {
                        std::ptr::null_mut()
                    } else {
                        &mut input_ptr
                    },
                    &mut input_left,
                    &mut output_ptr,
                    &mut output_left,
                )
            };
            Converted {
                read: input.len() - input_left,
                written: output.len() - output_left,
                error: (result == usize::MAX).then(std::io::Error::last_os_error),
            }
        }
    }

    impl Drop for Converter {
        fn drop(&mut self) {
            // SAFETY: This value owns a successfully opened descriptor.
            if unsafe { iconv_close(self.0) } != 0 {
                let error = std::io::Error::last_os_error();
                tracing::error!(%error, "iconv_close failed");
            }
        }
    }

    fn convert_all(
        input: &[u8],
        to: &CStr,
        from: &CStr,
        source_text: Option<&str>,
    ) -> Result<(Vec<u8>, bool), DiagMsg> {
        let mut size = input.len().max(16);
        'retry: loop {
            let mut output = zeroed_buffer(size)?;
            let mut converter = Converter::open(to, from)?;
            let mut read = 0;
            let mut written = 0;
            let mut loss = false;
            loop {
                let remaining = input.get(read..).ok_or(ERR_INTERNAL_CONVERSION)?;
                let tail = output.get_mut(written..).ok_or(ERR_INTERNAL_CONVERSION)?;
                let converted = converter.convert(Some(remaining), tail);
                read += converted.read;
                written += converted.written;
                // Globalization.h::EncodingConverter::Convert ignores successful
                // nonreversible counts; only its AddDefault recovery reports loss.
                if let Some(error) = converted.error {
                    if error.raw_os_error() == Some(libc::E2BIG) {
                        // Restart from complete input after growing the output.
                        size = size.checked_mul(2).ok_or(ERR_INTERNAL_CONVERSION)?;
                        continue 'retry;
                    }
                    if error.raw_os_error() == Some(libc::EILSEQ)
                        && let Some(text) = source_text
                    {
                        let ch = text
                            .get(read..)
                            .and_then(|s| s.chars().next())
                            .ok_or(ERR_INTERNAL_CONVERSION)?;
                        // Globalization.h EncodingConverter::Convert replaces
                        // one UTF-16 unit at a time if transliteration fails.
                        let replacements = ch.len_utf16();
                        if output.len() - written < replacements {
                            size = size.checked_mul(2).ok_or(ERR_INTERNAL_CONVERSION)?;
                            continue 'retry;
                        }
                        output
                            .get_mut(written..written + replacements)
                            .ok_or(ERR_INTERNAL_CONVERSION)?
                            .fill(b'?');
                        written += replacements;
                        read += ch.len_utf8();
                        loss = true;
                        tracing::trace!(%error, ?to, offset = read, "Substituting unrepresentable client character");
                        continue;
                    }
                    tracing::error!(%error, ?to, ?from, offset = read, "iconv failed");
                    return Err(match error.raw_os_error() {
                        Some(libc::EILSEQ | libc::EINVAL) => ERR_INVALID_CHARACTER_VALUE,
                        _ => ERR_INTERNAL_CONVERSION,
                    });
                }
                break;
            }
            if read != input.len() {
                tracing::error!(?to, ?from, "iconv did not consume the complete input");
                return Err(ERR_INTERNAL_CONVERSION);
            }
            // CP1258's glibc decoder buffers a base character to combine it
            // with a following tone mark. Flush even these stateless encodings.
            let tail = output.get_mut(written..).ok_or(ERR_INTERNAL_CONVERSION)?;
            let flushed = converter.convert(None, tail);
            if let Some(error) = flushed.error {
                if error.raw_os_error() == Some(libc::E2BIG) {
                    size = size.checked_mul(2).ok_or(ERR_INTERNAL_CONVERSION)?;
                    continue;
                }
                tracing::error!(%error, ?to, ?from, "iconv flush failed");
                return Err(ERR_INTERNAL_CONVERSION);
            }
            output.truncate(written + flushed.written);
            return Ok((output, loss));
        }
    }

    pub(super) fn encode(encoding: ClientEncoding, text: &str) -> Result<(Vec<u8>, bool), DiagMsg> {
        convert_all(
            text.as_bytes(),
            charset(encoding, true)?,
            c"UTF-8",
            Some(text),
        )
    }

    pub(super) fn decode(encoding: ClientEncoding, bytes: &[u8]) -> Result<String, DiagMsg> {
        let (converted, _) = convert_all(bytes, c"UTF-8", charset(encoding, false)?, None)?;
        String::from_utf8(converted).map_err(|error| {
            tracing::error!(%error, code_page = encoding.0, "iconv returned invalid UTF-8");
            ERR_INVALID_CHARACTER_VALUE
        })
    }

    pub(super) fn prefix_len(
        encoding: ClientEncoding,
        bytes: &[u8],
        capacity: usize,
    ) -> Result<usize, DiagMsg> {
        if !matches!(encoding.0, 932 | 936 | 949 | 950 | 54936 | 12000) {
            return Ok(capacity);
        }
        let mut converter = Converter::open(c"UTF-32LE", charset(encoding, false)?)?;
        let mut consumed = 0;
        let mut output = [0_u8; 256];
        while consumed < capacity {
            let input = bytes
                .get(consumed..capacity)
                .ok_or(ERR_INTERNAL_CONVERSION)?;
            let converted = converter.convert(Some(input), &mut output);
            consumed += converted.read;
            match converted.error {
                None => return Ok(consumed),
                Some(error) if error.raw_os_error() == Some(libc::EINVAL) => {
                    // Only a truncated final character is expected here.
                    if capacity < bytes.len() {
                        return Ok(consumed);
                    }
                    tracing::error!(%error, code_page = encoding.0, "Incomplete encoded character");
                    return Err(ERR_INVALID_CHARACTER_VALUE);
                }
                Some(error) if error.raw_os_error() == Some(libc::E2BIG) && converted.read != 0 => {
                }
                Some(error) => {
                    tracing::error!(%error, code_page = encoding.0, "iconv boundary scan failed");
                    return Err(ERR_INVALID_CHARACTER_VALUE);
                }
            }
        }
        Ok(consumed)
    }

    #[cfg(test)]
    pub(super) fn for_code_page(code_page: u32) -> Option<ClientEncoding> {
        let encoding = ClientEncoding(code_page);
        charset(encoding, false).ok()?;
        Some(encoding)
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn supported_iso_codesets_round_trip_without_windows_aliases() {
            for (suffix, code_page) in [
                (1, 28591),
                (2, 28592),
                (3, 28593),
                (4, 28594),
                (5, 28595),
                (6, 28596),
                (7, 28597),
                (8, 28598),
                (9, 28599),
                (13, 28603),
                (15, 28605),
            ] {
                let name = format!("ISO8859-{suffix}");
                assert_eq!(from_codeset(name.as_bytes()), ClientEncoding(code_page));
                let encoding = for_code_page(code_page).unwrap();
                let encoded = encoding.encode("\u{a0}").unwrap();
                assert_eq!(&*encoded.bytes, b"\xa0");
                assert!(!encoded.had_loss);
                assert_eq!(encoding.decode(&encoded.bytes).unwrap(), "\u{a0}");
            }
            for name in [b"ISO8859-0".as_slice(), b"ISO8859-10", b"ISO8859-"] {
                assert_eq!(from_codeset(name), ClientEncoding::UTF8);
            }
        }

        #[test]
        fn invalid_converter_names_report_internal_conversion_errors() {
            for (to, from) in [
                (c"unsupported-client-encoding", c"UTF-8"),
                (c"UTF-8", c"unsupported-client-encoding"),
            ] {
                assert_eq!(
                    convert_all(b"A", to, from, None).unwrap_err().state,
                    ERR_INTERNAL_CONVERSION.state
                );
            }
            let encoding = ClientEncoding(u32::MAX);
            assert_eq!(
                encoding.encode("é").unwrap_err().state,
                ERR_INTERNAL_CONVERSION.state
            );
            assert_eq!(
                encoding.decode(b"\xe9").unwrap_err().state,
                ERR_INTERNAL_CONVERSION.state
            );
            assert_eq!(
                encoding
                    .utf8_offset_for_client_bytes("é", 1)
                    .unwrap_err()
                    .state,
                ERR_INTERNAL_CONVERSION.state
            );
        }

        #[test]
        fn malformed_multibyte_prefixes_report_character_errors() {
            for (code_page, bytes) in [(932, b"A\x82".as_slice()), (12000, b"A\0\0".as_slice())] {
                let encoding = for_code_page(code_page).unwrap();
                assert_eq!(
                    encoding.prefix_len(bytes, bytes.len()).unwrap_err().state,
                    ERR_INVALID_CHARACTER_VALUE.state
                );
            }
            let encoding = for_code_page(12000).unwrap();
            let invalid_scalar = b"\0\xd8\0\0";
            assert_eq!(
                encoding
                    .prefix_len(invalid_scalar, invalid_scalar.len())
                    .unwrap_err()
                    .state,
                ERR_INVALID_CHARACTER_VALUE.state
            );
            assert_eq!(
                encoding.decode(invalid_scalar).unwrap_err().state,
                ERR_INVALID_CHARACTER_VALUE.state
            );
        }

        #[test]
        fn locale_aliases_and_native_utf8_fallback() {
            for codeset in [
                b"C".as_slice(),
                b"POSIX",
                b"ANSI_X3.4-1968",
                b"US-ASCII",
                b"SHIFT_JIS",
                b"eucJP",
                b"unknown",
            ] {
                assert_eq!(from_codeset(codeset), ClientEncoding::UTF8);
            }
            for (codeset, cp) in [
                (b"cp1252".as_slice(), 1252),
                (b"CP932", 932),
                (b"BIG5-HKSCS", 950),
                (b"gb2312", 936),
                (b"GB18030", 54936),
                (b"iso-8859-1", 28591),
                (b"8859_1", 28591),
                (b"ISO8859-1", 28591),
                (b"ISO88591", 28591),
                (b"ISO_8859-1", 28591),
                (b"ISO_8859_1", 28591),
                (b"ISO-8859-13", 28603),
                (b"ISO-8859-15", 28605),
                (b"UTF-32LE", 12000),
            ] {
                assert_eq!(from_codeset(codeset), ClientEncoding(cp));
            }
        }

        #[test]
        fn iso8859_1_is_not_windows1252() {
            let encoding = for_code_page(28591).unwrap();
            assert_eq!(encoding.decode(b"\x80").unwrap(), "\u{80}");
            assert_eq!(&*encoding.encode("\u{80}").unwrap().bytes, b"\x80");
            assert_eq!(for_code_page(1252).unwrap().decode(b"\x80").unwrap(), "€");
        }

        #[test]
        fn utf32le_does_not_borrow_ascii_or_split_units() {
            let encoding = for_code_page(12000).unwrap();
            assert!(!encoding.is_ascii_compatible());
            let encoded = encoding.encode("A\0😀").unwrap();
            assert!(matches!(encoded.bytes, Cow::Owned(_)));
            assert_eq!(encoded.bytes.len(), 12);
            assert_eq!(encoding.decode(&encoded.bytes).unwrap(), "A\0😀");
            for capacity in 0..=12 {
                assert_eq!(
                    encoding.prefix_len(&encoded.bytes, capacity).unwrap(),
                    capacity / 4 * 4
                );
            }
            assert_eq!(&*encoding.encode("").unwrap().bytes, b"");
            assert_eq!(encoding.decode(b"").unwrap(), "");
        }

        #[test]
        fn gb18030_preserves_multibyte_character_boundaries() {
            let encoding = for_code_page(54936).unwrap();
            // U+0100 uses a four-byte GB18030 sequence supported by older
            // platform tables that do not include supplementary mappings.
            let encoded: &[u8] = b"A\x81\x30\x8b\x38B";
            let character_start = 1;
            let character_end = 5;
            let decoded = encoding
                .decode(encoded)
                .unwrap_or_else(|_| panic!("GB18030 failed to decode {encoded:02x?}"));
            assert_eq!(decoded, "AĀB", "encoded bytes: {encoded:02x?}");

            for capacity in character_start..character_end {
                let prefix_len = encoding.prefix_len(encoded, capacity).unwrap_or_else(|_| {
                    panic!("GB18030 prefix scan failed at capacity {capacity} for {encoded:02x?}")
                });
                assert_eq!(
                    prefix_len, character_start,
                    "capacity {capacity}, character range {character_start}..{character_end}, bytes {encoded:02x?}"
                );
            }
            assert_eq!(
                encoding
                    .prefix_len(encoded, character_end)
                    .unwrap_or_else(|_| {
                        panic!(
                            "GB18030 prefix scan failed at complete-character boundary {character_end} for {encoded:02x?}"
                        )
                    }),
                character_end,
                "complete-character boundary, bytes {encoded:02x?}"
            );
            assert_eq!(
                encoding
                    .prefix_len(encoded, encoded.len())
                    .unwrap_or_else(|_| {
                        panic!("GB18030 prefix scan failed at full length for {encoded:02x?}")
                    }),
                encoded.len(),
                "full-length prefix, bytes {encoded:02x?}"
            );
        }

        #[cfg(target_env = "gnu")]
        #[test]
        fn successful_transliteration_does_not_report_diagnostic_loss_after_growth() {
            let encoding = for_code_page(28591).unwrap();
            let text = format!("漢{}", "Ⅷ".repeat(80));
            let encoded = encoding.encode(&text).unwrap();
            assert!(!encoded.had_loss);
            assert_eq!(encoded.bytes.len(), 1 + 4 * 80);
            assert_eq!(
                &*encoded.bytes,
                format!("?{}", "VIII".repeat(80)).as_bytes()
            );
        }

        #[test]
        fn cp1258_retains_final_character() {
            let encoding = for_code_page(1258).unwrap();
            assert_eq!(encoding.decode(b"\xf4").unwrap(), "ô");
            assert_eq!(&*encoding.encode("ô").unwrap().bytes, b"\xf4");
        }

        #[cfg(not(target_env = "musl"))]
        #[test]
        fn failed_transliteration_substitutes_utf16_units() {
            let text = "[漢][😀]\0Z";
            let (bytes, loss) =
                convert_all(text.as_bytes(), c"CP1252", c"UTF-8", Some(text)).unwrap();
            assert_eq!(bytes, b"[?][??]\0Z");
            assert!(loss);
        }
    }
}

#[cfg(windows)]
mod platform {
    use super::*;
    use windows::Win32::Foundation::{BOOL, GetLastError};
    use windows::Win32::Globalization::{
        CPINFO, GetACP, GetCPInfo, MULTI_BYTE_TO_WIDE_CHAR_FLAGS, MultiByteToWideChar,
        WideCharToMultiByte,
    };
    use windows::core::PCSTR;

    pub(super) fn system_default() -> ClientEncoding {
        // SAFETY: GetACP takes no pointers and reads the Windows ANSI code page.
        ClientEncoding(unsafe { GetACP() })
    }

    fn native_error(operation: &str, encoding: ClientEncoding) -> DiagMsg {
        // SAFETY: GetLastError takes no arguments and reads thread-local state.
        let error = unsafe { GetLastError() };
        tracing::error!(
            ?error,
            code_page = encoding.0,
            operation,
            "Client encoding conversion failed"
        );
        ERR_INTERNAL_CONVERSION
    }

    fn check_length(len: usize) -> Result<(), DiagMsg> {
        i32::try_from(len).map(|_| ()).map_err(|error| {
            tracing::error!(%error, len, "Client encoding input exceeds Windows API limit");
            ERR_INTERNAL_CONVERSION
        })
    }

    pub(super) fn encode(encoding: ClientEncoding, text: &str) -> Result<(Vec<u8>, bool), DiagMsg> {
        let wide_len = text.encode_utf16().count();
        check_length(wide_len)?;
        let mut wide = zeroed_buffer::<u16>(wide_len)?;
        for (unit, value) in wide.iter_mut().zip(text.encode_utf16()) {
            *unit = value;
        }
        let mut used_default = BOOL::default();
        // SAFETY: The slice length fits i32. Both calls use complete UTF-16
        // input, explicit lengths (including NULs), and valid output pointers.
        let required = unsafe {
            WideCharToMultiByte(
                encoding.0,
                0,
                &wide,
                None,
                PCSTR::null(),
                Some(&mut used_default),
            )
        };
        if required <= 0 {
            return Err(native_error("WideCharToMultiByte sizing", encoding));
        }
        let mut bytes =
            zeroed_buffer(usize::try_from(required).map_err(|_| ERR_INTERNAL_CONVERSION)?)?;
        // SAFETY: The destination is exactly the size returned by the same
        // conversion. Default best-fit behavior matches native ODBC.
        let written = unsafe {
            WideCharToMultiByte(
                encoding.0,
                0,
                &wide,
                Some(&mut bytes),
                PCSTR::null(),
                Some(&mut used_default),
            )
        };
        if written <= 0 {
            return Err(native_error("WideCharToMultiByte", encoding));
        }
        if written != required {
            tracing::error!(
                required,
                written,
                "Client encoding size changed during conversion"
            );
            return Err(ERR_INTERNAL_CONVERSION);
        }
        Ok((bytes, used_default.as_bool()))
    }

    pub(super) fn decode(encoding: ClientEncoding, bytes: &[u8]) -> Result<String, DiagMsg> {
        check_length(bytes.len())?;
        // SAFETY: Input length fits the API's i32 parameter; a missing output
        // slice requests the exact UTF-16 size.
        let required = unsafe {
            MultiByteToWideChar(encoding.0, MULTI_BYTE_TO_WIDE_CHAR_FLAGS(0), bytes, None)
        };
        if required <= 0 {
            return Err(native_error("MultiByteToWideChar sizing", encoding));
        }
        let mut wide =
            zeroed_buffer(usize::try_from(required).map_err(|_| ERR_INTERNAL_CONVERSION)?)?;
        // SAFETY: Explicit slice lengths are checked and the output is exactly
        // the size returned by the sizing call. Flags zero retain replacement.
        let written = unsafe {
            MultiByteToWideChar(
                encoding.0,
                MULTI_BYTE_TO_WIDE_CHAR_FLAGS(0),
                bytes,
                Some(&mut wide),
            )
        };
        if written <= 0 {
            return Err(native_error("MultiByteToWideChar", encoding));
        }
        if written != required {
            tracing::error!(
                required,
                written,
                "Client decoding size changed during conversion"
            );
            return Err(ERR_INTERNAL_CONVERSION);
        }
        String::from_utf16(&wide).map_err(|error| {
            tracing::error!(%error, code_page = encoding.0, "Windows returned invalid UTF-16");
            ERR_INVALID_CHARACTER_VALUE
        })
    }

    pub(super) fn prefix_len(
        encoding: ClientEncoding,
        bytes: &[u8],
        capacity: usize,
    ) -> Result<usize, DiagMsg> {
        let mut info = CPINFO::default();
        // SAFETY: info is a writable, correctly aligned CPINFO.
        unsafe { GetCPInfo(encoding.0, &mut info) }.map_err(|error| {
            tracing::error!(%error, code_page = encoding.0, "GetCPInfo failed");
            ERR_INTERNAL_CONVERSION
        })?;
        if info.MaxCharSize == 1 {
            return Ok(capacity);
        }
        if info.MaxCharSize != 2 {
            tracing::error!(
                code_page = encoding.0,
                "Unsupported ANSI code page character width"
            );
            return Err(ERR_INTERNAL_CONVERSION);
        }
        let mut end = 0;
        while end < capacity {
            let byte = *bytes.get(end).ok_or(ERR_INTERNAL_CONVERSION)?;
            let is_lead = info.LeadByte.chunks_exact(2).any(|range| {
                matches!(range, [start, stop] if *start != 0 && (*start..=*stop).contains(&byte))
            });
            let width = if is_lead { 2 } else { 1 };
            if width > capacity - end {
                break;
            }
            end += width;
        }
        Ok(end)
    }

    #[cfg(test)]
    pub(super) fn for_code_page(code_page: u32) -> Option<ClientEncoding> {
        if code_page == 65001 {
            return Some(ClientEncoding::UTF8);
        }
        if !matches!(
            code_page,
            437 | 850 | 874 | 932 | 936 | 949 | 950 | 1250..=1258 | 1361
        ) {
            return None;
        }
        let mut info = CPINFO::default();
        // SAFETY: info is a writable CPINFO.
        unsafe { GetCPInfo(code_page, &mut info) }.ok()?;
        Some(ClientEncoding(code_page))
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn native_conversion_failures_preserve_internal_error_diagnostics() {
            let encoding = ClientEncoding(u32::MAX);
            assert_eq!(
                encoding.encode("é").unwrap_err().state,
                ERR_INTERNAL_CONVERSION.state
            );
            assert_eq!(
                encoding.decode(b"\xe9").unwrap_err().state,
                ERR_INTERNAL_CONVERSION.state
            );
            assert_eq!(
                encoding.prefix_len(b"\xe9", 1).unwrap_err().state,
                ERR_INTERNAL_CONVERSION.state
            );
            assert_eq!(
                encoding
                    .utf8_offset_for_client_bytes("é", 1)
                    .unwrap_err()
                    .state,
                ERR_INTERNAL_CONVERSION.state
            );
        }

        #[test]
        fn windows_length_limit_is_checked_without_allocating() {
            let max_length = usize::try_from(i32::MAX).unwrap();
            assert!(check_length(max_length).is_ok());
            assert_eq!(
                check_length(max_length + 1).unwrap_err().state,
                ERR_INTERNAL_CONVERSION.state
            );
        }

        #[test]
        fn utf8_native_boundary_scan_rejects_unsupported_character_width() {
            assert_eq!(
                prefix_len(ClientEncoding::UTF8, "é".as_bytes(), 1)
                    .unwrap_err()
                    .state,
                ERR_INTERNAL_CONVERSION.state
            );
        }
    }
}
