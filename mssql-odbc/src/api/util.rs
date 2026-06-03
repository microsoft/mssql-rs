// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use std::slice;

use crate::api::odbc_types::{SQL_NTS, SqlSmallInt, SqlWChar};

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
    use super::read_utf16;
    use crate::api::odbc_types::SQL_NTS;

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
}
