// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use std::slice;

use crate::api::odbc_types::{SQL_NTS, SqlSmallInt, SqlWChar};

/// Copies a UTF-16 string into a caller buffer, NUL-terminating within the
/// buffer. Returns `true` if `src` was truncated (i.e., did not fit including
/// the NUL terminator). A null `dst` reports no truncation — callers use it to
/// query the required length without copying.
///
/// Mirrors msodbcsql's `StringCchCopyN(dst, cchBuf, src, cchSrc)` semantics:
/// at most `buf_chars - 1` source characters are copied, followed by a single
/// NUL. If `buf_chars == 0`, nothing is written and truncation is reported
/// when `src` is non-empty.
///
/// # Safety
/// `dst`, if non-null, must be writable for `buf_chars` `SqlWChar`s.
pub(crate) unsafe fn copy_utf16_with_nul(
    dst: *mut SqlWChar,
    buf_chars: usize,
    src: &[u16],
) -> bool {
    if dst.is_null() {
        return false;
    }
    if buf_chars == 0 {
        return !src.is_empty();
    }
    let copy_len = src.len().min(buf_chars - 1);
    for (i, ch) in src.iter().copied().take(copy_len).enumerate() {
        unsafe { dst.add(i).write(ch) };
    }
    unsafe { dst.add(copy_len).write(0) };
    copy_len < src.len()
}

/// Read a UTF-16 string from a raw pointer and an explicit or NUL-terminated length.
///
/// # Safety
/// - `ptr` must be readable for `length` `SQLWCHAR`s, or for all characters up to
///   and including the first NUL terminator when `length == SQL_NTS`.
pub(crate) unsafe fn read_utf16(ptr: *const SqlWChar, length: SqlSmallInt) -> String {
    let slice = if length == SQL_NTS {
        let mut len = 0usize;
        unsafe {
            while *ptr.add(len) != 0 {
                len += 1;
            }
        }
        unsafe { slice::from_raw_parts(ptr, len) }
    } else {
        unsafe { slice::from_raw_parts(ptr, length as usize) }
    };
    String::from_utf16_lossy(slice)
}

#[cfg(test)]
mod tests {
    use super::{copy_utf16_with_nul, read_utf16};
    use crate::api::odbc_types::{SQL_NTS, SqlWChar};

    #[test]
    fn read_utf16_nts() {
        let input: Vec<u16> = "SELECT 1"
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect();
        let result = unsafe { read_utf16(input.as_ptr(), SQL_NTS) };
        assert_eq!(result, "SELECT 1");
    }

    #[test]
    fn read_utf16_explicit_length() {
        let input: Vec<u16> = "SELECT 1 EXTRA".encode_utf16().collect();
        let result = unsafe { read_utf16(input.as_ptr(), 8) };
        assert_eq!(result, "SELECT 1");
    }

    /// Helper: read NUL-terminated UTF-16 from a buffer, honoring an upper bound.
    fn read_until_nul(buf: &[SqlWChar]) -> String {
        let len = buf.iter().position(|c| *c == 0).unwrap_or(buf.len());
        String::from_utf16(&buf[..len]).unwrap()
    }

    #[test]
    fn copy_null_dst_reports_no_truncation() {
        let src: Vec<u16> = "hello".encode_utf16().collect();
        // Caller pattern: ColumnName=NULL, BufferLength=anything → "size query"
        let truncated = unsafe { copy_utf16_with_nul(std::ptr::null_mut(), 0, &src) };
        assert!(!truncated, "null dst must not report truncation");
        let truncated = unsafe { copy_utf16_with_nul(std::ptr::null_mut(), 100, &src) };
        assert!(!truncated, "null dst must not report truncation");
    }

    #[test]
    fn copy_zero_buffer_with_empty_src_reports_no_truncation() {
        let src: Vec<u16> = Vec::new();
        let mut buf: [SqlWChar; 1] = [0xDEAD];
        let truncated = unsafe { copy_utf16_with_nul(buf.as_mut_ptr(), 0, &src) };
        assert!(!truncated);
        // Buffer untouched.
        assert_eq!(buf[0], 0xDEAD);
    }

    #[test]
    fn copy_zero_buffer_with_nonempty_src_reports_truncation() {
        let src: Vec<u16> = "x".encode_utf16().collect();
        let mut buf: [SqlWChar; 1] = [0xDEAD];
        let truncated = unsafe { copy_utf16_with_nul(buf.as_mut_ptr(), 0, &src) };
        assert!(truncated, "0-char buffer cannot fit non-empty src");
        // Buffer untouched.
        assert_eq!(buf[0], 0xDEAD);
    }

    #[test]
    fn copy_one_char_buffer_writes_only_nul() {
        let src: Vec<u16> = "abc".encode_utf16().collect();
        let mut buf: [SqlWChar; 4] = [0xDEAD; 4];
        let truncated = unsafe { copy_utf16_with_nul(buf.as_mut_ptr(), 1, &src) };
        assert!(truncated);
        assert_eq!(buf[0], 0, "buf_chars=1 means only the NUL fits");
        // Bytes past the written NUL must not be touched.
        assert_eq!(&buf[1..], &[0xDEAD, 0xDEAD, 0xDEAD]);
    }

    #[test]
    fn copy_exact_fit_writes_full_string_and_nul() {
        let src: Vec<u16> = "abc".encode_utf16().collect();
        // Need src.len() + 1 = 4 chars to fit "abc\0"
        let mut buf: [SqlWChar; 5] = [0xDEAD; 5];
        let truncated = unsafe { copy_utf16_with_nul(buf.as_mut_ptr(), 4, &src) };
        assert!(!truncated);
        assert_eq!(read_until_nul(&buf[..4]), "abc");
        assert_eq!(buf[3], 0);
        // Trailing slot past buf_chars must not be touched.
        assert_eq!(buf[4], 0xDEAD);
    }

    #[test]
    fn copy_oversized_buffer_writes_string_and_does_not_touch_extra() {
        let src: Vec<u16> = "ab".encode_utf16().collect();
        let mut buf: [SqlWChar; 8] = [0xDEAD; 8];
        let truncated = unsafe { copy_utf16_with_nul(buf.as_mut_ptr(), 8, &src) };
        assert!(!truncated);
        assert_eq!(read_until_nul(&buf), "ab");
        assert_eq!(buf[2], 0);
        // Untouched slots after the NUL.
        assert_eq!(&buf[3..], &[0xDEAD; 5]);
    }

    #[test]
    fn copy_truncation_writes_partial_and_nul() {
        let src: Vec<u16> = "abcdef".encode_utf16().collect();
        let mut buf: [SqlWChar; 4] = [0xDEAD; 4];
        let truncated = unsafe { copy_utf16_with_nul(buf.as_mut_ptr(), 4, &src) };
        assert!(truncated);
        assert_eq!(read_until_nul(&buf), "abc");
        assert_eq!(buf[3], 0, "NUL must be written within the buffer");
    }

    #[test]
    fn copy_truncation_at_boundary_reports_truncation() {
        // src.len() == buf_chars: exactly one slot is needed for NUL → truncated.
        let src: Vec<u16> = "abc".encode_utf16().collect();
        let mut buf: [SqlWChar; 3] = [0xDEAD; 3];
        let truncated = unsafe { copy_utf16_with_nul(buf.as_mut_ptr(), 3, &src) };
        assert!(truncated);
        assert_eq!(read_until_nul(&buf), "ab");
        assert_eq!(buf[2], 0);
    }

    #[test]
    fn copy_preserves_non_ascii_utf16() {
        // "héllo" — 'é' is U+00E9, single u16 unit
        let src: Vec<u16> = "héllo".encode_utf16().collect();
        let mut buf: [SqlWChar; 8] = [0xDEAD; 8];
        let truncated = unsafe { copy_utf16_with_nul(buf.as_mut_ptr(), 8, &src) };
        assert!(!truncated);
        assert_eq!(read_until_nul(&buf), "héllo");
    }

    #[test]
    fn copy_preserves_surrogate_pair_when_room() {
        // U+1F600 (😀) is encoded as a surrogate pair: D83D DE00
        let src: Vec<u16> = "a😀b".encode_utf16().collect();
        assert_eq!(src.len(), 4); // a + 2 surrogates + b
        let mut buf: [SqlWChar; 8] = [0xDEAD; 8];
        let truncated = unsafe { copy_utf16_with_nul(buf.as_mut_ptr(), 8, &src) };
        assert!(!truncated);
        assert_eq!(read_until_nul(&buf), "a😀b");
    }

    #[test]
    fn copy_truncation_can_split_surrogate_pair() {
        // Documents current behavior: helper copies units, not codepoints.
        // A naive caller may end up with a lone high surrogate. Same as msodbcsql.
        let src: Vec<u16> = "😀".encode_utf16().collect();
        assert_eq!(src.len(), 2);
        let mut buf: [SqlWChar; 2] = [0xDEAD; 2];
        let truncated = unsafe { copy_utf16_with_nul(buf.as_mut_ptr(), 2, &src) };
        assert!(truncated);
        // First half of the surrogate pair was written, then NUL.
        assert_eq!(buf[0], 0xD83D);
        assert_eq!(buf[1], 0);
    }
}
