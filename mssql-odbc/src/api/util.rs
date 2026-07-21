// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use std::slice;

use crate::api::odbc_types::{SQL_NTS, SqlSmallInt, SqlWChar};

/// Write `value` to `ptr` if non-null. Every ODBC out-parameter pointer may
/// legitimately be null (caller opting out of that value), so the
/// `if (p) *p = v;` idiom appears at almost every entry point. Centralizing
/// it keeps individual call sites clean and puts a single chokepoint to audit
/// when reviewing pointer writes.
///
/// # Safety
/// `ptr`, if non-null, must be valid and properly aligned for one `T`.
pub(crate) unsafe fn write_if_some<T: Copy>(ptr: *mut T, value: T) {
    if !ptr.is_null() {
        unsafe { ptr.write(value) };
    }
}

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

/// Rewrites ODBC `?` parameter markers to SQL Server named markers (`@P1`,
/// `@P2`, …) and returns the rewritten SQL together with the marker count.
///
/// `?` inside string literals (`'…'`), quoted identifiers (`"…"` / `[…]`), and
/// comments (`-- …` to end of line, `/* … */`) is left untouched. `''`, `""`,
/// and `]]` are treated as escaped quote characters, not delimiters. Markers are
/// numbered 1-based in source order, matching the `@P1…` names used to build the
/// `sp_prepare` parameter declaration.
///
/// This intentionally mirrors msodbcsql's marker lexer
/// (`ComputeParamInfo` / `CParamOffsetInfo::GetParameterInfo` in `sqlccmd.cpp`)
/// for behavioral parity, including two deliberate quirks that differ from a
/// strict T-SQL lexer:
/// - **Block comments do not nest** — scanning stops at the first `*/`, so a `?`
///   between an inner `*/` and the outer `*/` in `/* … /* … */ ? … */` is
///   treated as a marker
/// - **`--(* … *)--` vendor canonical-extension escape is not a comment** — a
///   `--` that opens `--(*`, or that is immediately preceded by `*)`, is passed
///   through rather than starting a line comment. A shared consequence is that
///   `COUNT(*)--…` is *not* treated as a line comment (a `?` inside it is
///   counted), matching msodbcsql.
pub(crate) fn rewrite_param_markers(sql: &str) -> (String, usize) {
    #[derive(PartialEq)]
    enum State {
        Normal,
        SingleQuote,
        DoubleQuote,
        Bracket,
        LineComment,
        BlockComment,
    }

    let mut out = String::with_capacity(sql.len() + 8);
    let mut count: usize = 0;
    let mut state = State::Normal;
    // The two preceding characters, used to detect the `*)--` close of an ODBC
    // canonical-extension escape
    let mut prev1: Option<char> = None;
    let mut prev2: Option<char> = None;
    let mut chars = sql.chars().peekable();

    while let Some(c) = chars.next() {
        match state {
            State::Normal => match c {
                '?' => {
                    count += 1;
                    out.push_str("@P");
                    out.push_str(&count.to_string());
                }
                '\'' => {
                    state = State::SingleQuote;
                    out.push(c);
                }
                '"' => {
                    state = State::DoubleQuote;
                    out.push(c);
                }
                '[' => {
                    state = State::Bracket;
                    out.push(c);
                }
                '-' if chars.peek() == Some(&'-') => {
                    // A `--` is a line comment unless it opens (`--(*`) or closes
                    // (`*)--`, detected via the two preceding chars) an ODBC vendor
                    // canonical extension, which is passed through as normal text.
                    let starts_canonical_extension = matches!(chars.clone().nth(1), Some('('))
                        && matches!(chars.clone().nth(2), Some('*'));
                    let ends_canonical_extension =
                        matches!(prev2, Some('*')) && matches!(prev1, Some(')'));

                    if !starts_canonical_extension && !ends_canonical_extension {
                        out.push(c);
                        if let Some(n) = chars.next() {
                            out.push(n);
                        }
                        state = State::LineComment;
                    } else {
                        out.push(c);
                    }
                }
                '/' if chars.peek() == Some(&'*') => {
                    out.push(c);
                    if let Some(n) = chars.next() {
                        out.push(n);
                    }
                    state = State::BlockComment;
                }
                _ => out.push(c),
            },
            State::SingleQuote => {
                out.push(c);
                if c == '\'' {
                    // Doubled single quotes -> escaped quote, not the end of the literal
                    if chars.peek() == Some(&'\'') {
                        if let Some(n) = chars.next() {
                            out.push(n);
                        }
                    } else {
                        // lone quote → end of literal
                        state = State::Normal;
                    }
                }
            }
            State::DoubleQuote => {
                out.push(c);
                if c == '"' {
                    if chars.peek() == Some(&'"') {
                        if let Some(n) = chars.next() {
                            out.push(n);
                        }
                    } else {
                        state = State::Normal;
                    }
                }
            }
            State::Bracket => {
                out.push(c);
                if c == ']' {
                    if chars.peek() == Some(&']') {
                        if let Some(n) = chars.next() {
                            out.push(n);
                        }
                    } else {
                        state = State::Normal;
                    }
                }
            }
            State::LineComment => {
                out.push(c);
                if c == '\n' || c == '\r' {
                    state = State::Normal;
                }
            }
            State::BlockComment => {
                // Non-nesting: the first `*/` closes the comment (msodbcsql parity).
                out.push(c);
                if c == '*' && chars.peek() == Some(&'/') {
                    if let Some(n) = chars.next() {
                        out.push(n);
                    }
                    state = State::Normal;
                }
            }
        }

        prev2 = prev1;
        prev1 = Some(c);
    }

    (out, count)
}

#[cfg(test)]
mod tests {
    use super::{copy_with_nul, read_utf16, rewrite_param_markers, write_if_some};
    use crate::api::odbc_types::{SQL_NTS, SqlWChar};

    #[test]
    fn rewrite_no_markers_is_unchanged() {
        let (out, n) = rewrite_param_markers("SELECT 1");
        assert_eq!(out, "SELECT 1");
        assert_eq!(n, 0);
    }

    #[test]
    fn rewrite_single_marker() {
        let (out, n) = rewrite_param_markers("SELECT * FROM t WHERE id = ?");
        assert_eq!(out, "SELECT * FROM t WHERE id = @P1");
        assert_eq!(n, 1);
    }

    #[test]
    fn rewrite_multiple_markers_numbered_in_order() {
        let (out, n) = rewrite_param_markers("a = ? AND b = ? OR c = ?");
        assert_eq!(out, "a = @P1 AND b = @P2 OR c = @P3");
        assert_eq!(n, 3);
    }

    #[test]
    fn rewrite_skips_single_quoted_literal() {
        let (out, n) = rewrite_param_markers("SELECT '?' AS q, col WHERE x = ?");
        assert_eq!(out, "SELECT '?' AS q, col WHERE x = @P1");
        assert_eq!(n, 1);
    }

    #[test]
    fn rewrite_skips_escaped_quote_in_literal() {
        let (out, n) = rewrite_param_markers("WHERE a = '''?''' AND b = ?");
        assert_eq!(out, "WHERE a = '''?''' AND b = @P1");
        assert_eq!(n, 1);
    }

    #[test]
    fn rewrite_skips_bracket_identifier() {
        let (out, n) = rewrite_param_markers("SELECT [a?b] FROM t WHERE x = ?");
        assert_eq!(out, "SELECT [a?b] FROM t WHERE x = @P1");
        assert_eq!(n, 1);
    }

    #[test]
    fn rewrite_skips_double_quoted_identifier() {
        let (out, n) = rewrite_param_markers("SELECT \"a?b\" WHERE x = ?");
        assert_eq!(out, "SELECT \"a?b\" WHERE x = @P1");
        assert_eq!(n, 1);
    }

    #[test]
    fn rewrite_skips_doubled_double_quote_in_identifier() {
        let (out, n) = rewrite_param_markers("SELECT \"a\"\"?\"\"b\" WHERE x = ?");
        assert_eq!(out, "SELECT \"a\"\"?\"\"b\" WHERE x = @P1");
        assert_eq!(n, 1);
    }

    #[test]
    fn rewrite_skips_doubled_bracket_in_identifier() {
        let (out, n) = rewrite_param_markers("SELECT [a]]?]]b] WHERE x = ?");
        assert_eq!(out, "SELECT [a]]?]]b] WHERE x = @P1");
        assert_eq!(n, 1);
    }

    #[test]
    fn rewrite_skips_line_comment() {
        let (out, n) = rewrite_param_markers("SELECT 1 -- ? not a param\nWHERE x = ?");
        assert_eq!(out, "SELECT 1 -- ? not a param\nWHERE x = @P1");
        assert_eq!(n, 1);
    }

    #[test]
    fn rewrite_line_comment_ends_on_carriage_return() {
        let (out, n) = rewrite_param_markers("SELECT 1 -- ? not a param\rWHERE x = ?");
        assert_eq!(out, "SELECT 1 -- ? not a param\rWHERE x = @P1");
        assert_eq!(n, 1);
    }

    #[test]
    fn rewrite_skips_block_comment_until_first_close() {
        // Block comments do not nest (msodbcsql parity)
        let (out, n) = rewrite_param_markers("/* a /* ? */ ? */ x = ?");
        assert_eq!(out, "/* a /* ? */ @P1 */ x = @P2");
        assert_eq!(n, 2);
    }

    #[test]
    fn rewrite_count_star_line_comment_is_not_a_comment_msodbcsql_parity() {
        // Known quirk shared with msodbcsql: a `--` immediately preceded by `*)`
        // is treated as the close of a `--(* … *)--` canonical extension, not a
        // line comment. So `COUNT(*)--…` is NOT a comment and a `?` inside it is
        // counted as a marker. (A strict T-SQL lexer would treat it as a comment.)
        let (out, n) = rewrite_param_markers("SELECT COUNT(*)--has a ? here\nWHERE a = ?");
        assert_eq!(out, "SELECT COUNT(*)--has a @P1 here\nWHERE a = @P2");
        assert_eq!(n, 2);
    }

    #[test]
    fn rewrite_does_not_treat_canonical_extension_comment_syntax_as_comment() {
        let (out, n) = rewrite_param_markers(
            "SELECT --(* vendor (foo) product (bar) extension*)-- WHERE x = ?",
        );
        assert_eq!(
            out,
            "SELECT --(* vendor (foo) product (bar) extension*)-- WHERE x = @P1"
        );
        assert_eq!(n, 1);
    }

    #[test]
    fn rewrite_supports_question_equal_pattern() {
        let (out, n) = rewrite_param_markers("?= EXEC dbo.p ?");
        assert_eq!(out, "@P1= EXEC dbo.p @P2");
        assert_eq!(n, 2);
    }

    #[test]
    fn rewrite_supports_odbc_call_escape_with_return_marker() {
        let (out, n) = rewrite_param_markers("{?= call dbo.p(?)}");
        assert_eq!(out, "{@P1= call dbo.p(@P2)}");
        assert_eq!(n, 2);
    }

    #[test]
    fn write_if_some_writes_through_non_null_ptr() {
        let mut value: i32 = 0;
        unsafe { write_if_some(&mut value as *mut i32, 42) };
        assert_eq!(value, 42);
    }

    #[test]
    fn write_if_some_no_op_on_null() {
        // Must not deref or panic on null. Compile-only smoke test.
        unsafe { write_if_some::<i32>(std::ptr::null_mut(), 42) };
    }

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
