// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use std::slice;

use crate::api::odbc_types::{SQL_NTS, SqlSmallInt, SqlWChar};

/// Copies `src` into a caller buffer, NUL-terminating within the buffer.
/// Returns `true` if `src` was truncated (i.e., did not fit including the NUL
/// terminator). A null `dst` reports no truncation - callers use it to query
/// the required length without copying.
///
/// Generic over the element type, so the same routine handles both narrow
/// (`u8`) ODBC strings (`SQL_C_CHAR`) and wide (`u16`) ones (`SQL_C_WCHAR`).
/// `buf_len` and `src.len()` are both element counts of `T`, not byte counts -
/// callers passing a byte length for `T = u16` must divide by `size_of::<u16>()`
/// first.
///
/// Mirrors msodbcsql's `StringCchCopyN(dst, cchBuf, src, cchSrc)` semantics:
/// at most `buf_len - 1` source elements are copied, followed by a single NUL
/// (`T::default()`). If `buf_len == 0`, nothing is written and truncation is
/// reported when `src` is non-empty.
///
/// # Safety
/// - `dst`, if non-null, must be writable for `buf_len` `T`s.
/// - `dst` and `src` must not overlap.
pub(crate) unsafe fn copy_with_nul<T: Copy + Default>(
    dst: *mut T,
    buf_len: usize,
    src: &[T],
) -> bool {
    if dst.is_null() {
        return false;
    }
    if buf_len == 0 {
        return !src.is_empty();
    }
    let copy_len = src.len().min(buf_len - 1);
    unsafe {
        std::ptr::copy_nonoverlapping(src.as_ptr(), dst, copy_len);
        dst.add(copy_len).write(T::default());
    }
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
    use super::{copy_with_nul, read_utf16};
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
        // Caller pattern: ColumnName=NULL → "size query", never truncation.
        let src: Vec<u16> = "hello".encode_utf16().collect();
        for buf_len in [0, 100] {
            let truncated = unsafe { copy_with_nul(std::ptr::null_mut(), buf_len, &src) };
            assert!(!truncated, "null dst must not report truncation");
        }
    }

    #[test]
    fn copy_zero_buffer_leaves_buffer_untouched() {
        // buf_len=0: never write; truncated iff src is non-empty.
        let mut buf: [SqlWChar; 1] = [0xDEAD];
        let truncated = unsafe { copy_with_nul(buf.as_mut_ptr(), 0, &[]) };
        assert!(!truncated);
        assert_eq!(buf[0], 0xDEAD);

        let src: Vec<u16> = "x".encode_utf16().collect();
        let truncated = unsafe { copy_with_nul(buf.as_mut_ptr(), 0, &src) };
        assert!(truncated);
        assert_eq!(buf[0], 0xDEAD);
    }

    #[test]
    fn copy_one_unit_buffer_writes_only_nul() {
        let src: Vec<u16> = "abc".encode_utf16().collect();
        let mut buf: [SqlWChar; 4] = [0xDEAD; 4];
        let truncated = unsafe { copy_with_nul(buf.as_mut_ptr(), 1, &src) };
        assert!(truncated);
        assert_eq!(buf[0], 0, "buf_len=1 means only the NUL fits");
        // Slots past the written NUL must not be touched.
        assert_eq!(&buf[1..], &[0xDEAD; 3]);
    }

    #[test]
    fn copy_exact_fit_writes_full_string_and_nul() {
        // buf_len == src.len() + 1: just enough; no truncation; nothing touched
        // past buf_len.
        let src: Vec<u16> = "abc".encode_utf16().collect();
        let mut buf: [SqlWChar; 5] = [0xDEAD; 5];
        let truncated = unsafe { copy_with_nul(buf.as_mut_ptr(), 4, &src) };
        assert!(!truncated);
        assert_eq!(read_until_nul(&buf[..4]), "abc");
        assert_eq!(buf[3], 0);
        assert_eq!(buf[4], 0xDEAD);
    }

    #[test]
    fn copy_oversized_buffer_does_not_touch_beyond_nul() {
        // buf_len > src.len() + 1: helper must NOT scribble past the NUL.
        let src: Vec<u16> = "ab".encode_utf16().collect();
        let mut buf: [SqlWChar; 8] = [0xDEAD; 8];
        let truncated = unsafe { copy_with_nul(buf.as_mut_ptr(), 8, &src) };
        assert!(!truncated);
        assert_eq!(read_until_nul(&buf), "ab");
        assert_eq!(buf[2], 0);
        assert_eq!(&buf[3..], &[0xDEAD; 5]);
    }

    #[test]
    fn copy_truncation_writes_partial_and_nul() {
        // buf_len <= src.len(): copy buf_len-1 units, write NUL at buf_len-1.
        let src: Vec<u16> = "abcdef".encode_utf16().collect();
        let mut buf: [SqlWChar; 4] = [0xDEAD; 4];
        let truncated = unsafe { copy_with_nul(buf.as_mut_ptr(), 4, &src) };
        assert!(truncated);
        assert_eq!(read_until_nul(&buf), "abc");
        assert_eq!(buf[3], 0);
    }

    #[test]
    fn copy_preserves_surrogate_pair_when_room() {
        // U+1F600 (😀) — surrogate pair D83D DE00. Also exercises BMP non-ASCII
        // ('a', 'b') in the same pass since copy_nonoverlapping is byte-level.
        let src: Vec<u16> = "a😀b".encode_utf16().collect();
        assert_eq!(src.len(), 4);
        let mut buf: [SqlWChar; 8] = [0xDEAD; 8];
        let truncated = unsafe { copy_with_nul(buf.as_mut_ptr(), 8, &src) };
        assert!(!truncated);
        assert_eq!(read_until_nul(&buf), "a😀b");
    }

    #[test]
    fn copy_truncation_can_split_surrogate_pair() {
        // Documents current behavior: helper copies units, not codepoints, so a
        // naive caller can be left with a lone high surrogate. Same as msodbcsql.
        let src: Vec<u16> = "😀".encode_utf16().collect();
        let mut buf: [SqlWChar; 2] = [0xDEAD; 2];
        let truncated = unsafe { copy_with_nul(buf.as_mut_ptr(), 2, &src) };
        assert!(truncated);
        assert_eq!(buf[0], 0xD83D);
        assert_eq!(buf[1], 0);
    }

    // Lock in that the generic helper works for narrow (`SQL_C_CHAR`) buffers,
    // not just wide ones.
    #[test]
    fn copy_u8_instantiation_truncates_and_terminates() {
        // Truncation case.
        let mut buf: [u8; 4] = [0xAA; 4];
        let truncated = unsafe { copy_with_nul(buf.as_mut_ptr(), 4, b"abcdef") };
        assert!(truncated);
        assert_eq!(&buf[..3], b"abc");
        assert_eq!(buf[3], 0);

        // Exact fit, with sentinel past buf_len untouched.
        let mut buf: [u8; 5] = [0xAA; 5];
        let truncated = unsafe { copy_with_nul(buf.as_mut_ptr(), 4, b"abc") };
        assert!(!truncated);
        assert_eq!(&buf[..3], b"abc");
        assert_eq!(buf[3], 0);
        assert_eq!(buf[4], 0xAA);
    }
}
