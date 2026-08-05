// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Narrow (`SQL_C_CHAR`) character encoding.
//!
//! ODBC's narrow C character type carries text in the driver's *ANSI* encoding,
//! not UTF-8. msodbcsql uses the Windows active code page (CP1252 on a typical
//! US/Western-European install) so that a `VARCHAR(n)` round-trips byte-for-byte
//! through a narrow buffer. On non-Windows platforms msodbcsql uses UTF-8, which
//! is what `String` already holds.

/// Encodes `text` into the client ANSI code page.
///
/// Unmappable characters become `?`, matching `WideCharToMultiByte`'s default
/// replacement behaviour.
pub(crate) fn encode(text: &str) -> Vec<u8> {
    #[cfg(windows)]
    {
        windows_acp::encode(text)
    }
    #[cfg(not(windows))]
    {
        text.as_bytes().to_vec()
    }
}

/// Decodes `bytes` from the client ANSI code page.
///
/// Invalid sequences are replaced rather than rejected; ODBC has no way to
/// report a decoding failure on the parameter path.
pub(crate) fn decode(bytes: &[u8]) -> String {
    #[cfg(windows)]
    {
        windows_acp::decode(bytes)
    }
    #[cfg(not(windows))]
    {
        String::from_utf8_lossy(bytes).into_owned()
    }
}

#[cfg(windows)]
mod windows_acp {
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetACP() -> u32;
        fn WideCharToMultiByte(
            code_page: u32,
            flags: u32,
            wide_char_str: *const u16,
            wide_char: i32,
            multi_byte_str: *mut u8,
            multi_byte: i32,
            default_char: *const u8,
            used_default_char: *mut i32,
        ) -> i32;
        fn MultiByteToWideChar(
            code_page: u32,
            flags: u32,
            multi_byte_str: *const u8,
            multi_byte: i32,
            wide_char_str: *mut u16,
            wide_char: i32,
        ) -> i32;
    }

    const CP_UTF8: u32 = 65001;

    fn acp() -> u32 {
        unsafe { GetACP() }
    }

    pub(super) fn encode(text: &str) -> Vec<u8> {
        let cp = acp();
        if cp == CP_UTF8 {
            return text.as_bytes().to_vec();
        }
        let wide: Vec<u16> = text.encode_utf16().collect();
        if wide.is_empty() {
            return Vec::new();
        }
        let needed = unsafe {
            WideCharToMultiByte(
                cp,
                0,
                wide.as_ptr(),
                wide.len() as i32,
                std::ptr::null_mut(),
                0,
                std::ptr::null(),
                std::ptr::null_mut(),
            )
        };
        if needed <= 0 {
            return text.as_bytes().to_vec();
        }
        let mut out = vec![0u8; needed as usize];
        let written = unsafe {
            WideCharToMultiByte(
                cp,
                0,
                wide.as_ptr(),
                wide.len() as i32,
                out.as_mut_ptr(),
                needed,
                std::ptr::null(),
                std::ptr::null_mut(),
            )
        };
        if written <= 0 {
            return text.as_bytes().to_vec();
        }
        out.truncate(written as usize);
        out
    }

    pub(super) fn decode(bytes: &[u8]) -> String {
        let cp = acp();
        if cp == CP_UTF8 {
            return String::from_utf8_lossy(bytes).into_owned();
        }
        if bytes.is_empty() {
            return String::new();
        }
        let needed = unsafe {
            MultiByteToWideChar(
                cp,
                0,
                bytes.as_ptr(),
                bytes.len() as i32,
                std::ptr::null_mut(),
                0,
            )
        };
        if needed <= 0 {
            return String::from_utf8_lossy(bytes).into_owned();
        }
        let mut wide = vec![0u16; needed as usize];
        let written = unsafe {
            MultiByteToWideChar(
                cp,
                0,
                bytes.as_ptr(),
                bytes.len() as i32,
                wide.as_mut_ptr(),
                needed,
            )
        };
        if written <= 0 {
            return String::from_utf8_lossy(bytes).into_owned();
        }
        wide.truncate(written as usize);
        String::from_utf16_lossy(&wide)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ascii_round_trips() {
        assert_eq!(encode("hello"), b"hello");
        assert_eq!(decode(b"hello"), "hello");
    }

    #[test]
    fn latin1_round_trips() {
        let round = decode(&encode("café René"));
        assert_eq!(round, "café René");
    }

    #[test]
    fn empty_is_empty() {
        assert!(encode("").is_empty());
        assert_eq!(decode(&[]), "");
    }
}
