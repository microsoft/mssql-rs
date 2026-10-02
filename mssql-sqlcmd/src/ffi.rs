// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! C ABI for native sqlcmd.
//!
//! Text crosses the boundary as UTF-16 with an explicit length, which is how
//! native sqlcmd holds its strings on every platform (it builds with
//! `-fshort-wchar` off Windows). Nothing needs a terminating NUL, and a value
//! may contain embedded NULs.
//!
//! The document is an opaque handle owned by the caller between
//! [`mssql_sqlcmd_json_new`] and [`mssql_sqlcmd_json_free`]. The rendered
//! document is a separate allocation, released with
//! [`mssql_sqlcmd_free_text`]. Every function that can fail returns an
//! [`MSSQL_SQLCMD_OK`]-style status rather than unwinding: a panic is caught at
//! the boundary and reported as [`MSSQL_SQLCMD_INTERNAL_ERROR`].
//!
//! The header for C and C++ callers is `include/mssql_sqlcmd.h`.

use std::panic::{AssertUnwindSafe, catch_unwind};

use crate::formatter::json::{Connection, JsonDocument, Message};

/// The call succeeded.
pub const MSSQL_SQLCMD_OK: i32 = 0;
/// A required pointer was null.
pub const MSSQL_SQLCMD_NULL_ARGUMENT: i32 = 1;
/// The call does not fit the document's state, e.g. a row before any result
/// set, or a row whose value count differs from the column count.
pub const MSSQL_SQLCMD_INVALID_STATE: i32 = 2;
/// A Rust panic was caught at the boundary.
pub const MSSQL_SQLCMD_INTERNAL_ERROR: i32 = 3;

/// A UTF-16 string. `data` may be null only where a value is optional, in which
/// case the value is absent (SQL `NULL` for a row value); `len` is then
/// ignored. Invalid UTF-16 is replaced with U+FFFD rather than rejected.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct MssqlSqlcmdText {
    pub data: *const u16,
    pub len: usize,
}

/// The `connection` object of the document. Any text field may be null.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct MssqlSqlcmdConnection {
    pub server: MssqlSqlcmdText,
    pub database: MssqlSqlcmdText,
    pub authentication: MssqlSqlcmdText,
    /// Non-zero when the connection is encrypted.
    pub encrypt: i32,
}

/// Opaque JSON document handle.
pub struct MssqlSqlcmdJsonDocument(JsonDocument);

/// Reads `text`, or `None` when its pointer is null.
///
/// # Safety
/// A non-null `text.data` must point to `text.len` readable `u16` values.
unsafe fn read_text(text: MssqlSqlcmdText) -> Option<String> {
    if text.data.is_null() {
        return None;
    }
    // SAFETY: the caller guarantees `len` readable values behind a non-null pointer.
    let units = unsafe { std::slice::from_raw_parts(text.data, text.len) };
    Some(String::from_utf16_lossy(units))
}

/// Reads an array of `count` texts.
///
/// # Safety
/// `texts` must point to `count` readable [`MssqlSqlcmdText`] values, each
/// satisfying [`read_text`]'s requirement. It may be null only when `count` is 0.
unsafe fn read_texts(texts: *const MssqlSqlcmdText, count: usize) -> Option<Vec<Option<String>>> {
    if count == 0 {
        return Some(Vec::new());
    }
    if texts.is_null() {
        return None;
    }
    // SAFETY: the caller guarantees `count` readable values.
    let texts = unsafe { std::slice::from_raw_parts(texts, count) };
    // SAFETY: each text satisfies `read_text`'s requirement, per the caller.
    Some(
        texts
            .iter()
            .map(|text| unsafe { read_text(*text) })
            .collect(),
    )
}

/// Runs `body` with the document behind `document`, turning a null handle and
/// a panic into status codes.
///
/// # Safety
/// `document` must be null or a live handle from [`mssql_sqlcmd_json_new`]
/// that no other call is using.
unsafe fn with_document(
    document: *mut MssqlSqlcmdJsonDocument,
    body: impl FnOnce(&mut JsonDocument) -> i32,
) -> i32 {
    if document.is_null() {
        return MSSQL_SQLCMD_NULL_ARGUMENT;
    }
    // SAFETY: a non-null handle is live and not aliased, per the caller.
    let document = unsafe { &mut (*document).0 };
    catch_unwind(AssertUnwindSafe(|| body(document))).unwrap_or(MSSQL_SQLCMD_INTERNAL_ERROR)
}

/// Creates an empty document. Returns null only if a panic was caught.
#[unsafe(no_mangle)]
pub extern "C" fn mssql_sqlcmd_json_new() -> *mut MssqlSqlcmdJsonDocument {
    catch_unwind(|| Box::into_raw(Box::new(MssqlSqlcmdJsonDocument(JsonDocument::new()))))
        .unwrap_or(std::ptr::null_mut())
}

/// Releases a document. A null handle is ignored.
///
/// # Safety
/// `document` must be null or a handle from [`mssql_sqlcmd_json_new`] that has
/// not been freed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mssql_sqlcmd_json_free(document: *mut MssqlSqlcmdJsonDocument) {
    if !document.is_null() {
        // SAFETY: the handle came from `Box::into_raw` and is freed once.
        drop(unsafe { Box::from_raw(document) });
    }
}

/// Starts a result set with `column_count` column names.
///
/// # Safety
/// `document` as for [`mssql_sqlcmd_json_free`]; `columns` must point to
/// `column_count` texts with non-null data.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mssql_sqlcmd_json_begin_result_set(
    document: *mut MssqlSqlcmdJsonDocument,
    columns: *const MssqlSqlcmdText,
    column_count: usize,
) -> i32 {
    // SAFETY: forwarded from the caller's guarantees.
    let Some(columns) = (unsafe { read_texts(columns, column_count) }) else {
        return MSSQL_SQLCMD_NULL_ARGUMENT;
    };
    let Some(columns) = columns.into_iter().collect::<Option<Vec<String>>>() else {
        return MSSQL_SQLCMD_NULL_ARGUMENT;
    };
    // SAFETY: forwarded from the caller's guarantees.
    unsafe {
        with_document(document, |document| {
            document.begin_result_set(columns);
            MSSQL_SQLCMD_OK
        })
    }
}

/// Adds a row to the current result set. A value whose `data` is null is SQL
/// `NULL`.
///
/// # Safety
/// `document` as for [`mssql_sqlcmd_json_free`]; `values` must point to
/// `value_count` texts.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mssql_sqlcmd_json_add_row(
    document: *mut MssqlSqlcmdJsonDocument,
    values: *const MssqlSqlcmdText,
    value_count: usize,
) -> i32 {
    // SAFETY: forwarded from the caller's guarantees.
    let Some(values) = (unsafe { read_texts(values, value_count) }) else {
        return MSSQL_SQLCMD_NULL_ARGUMENT;
    };
    // SAFETY: forwarded from the caller's guarantees.
    unsafe {
        with_document(document, |document| match document.add_row(values) {
            Ok(()) => MSSQL_SQLCMD_OK,
            Err(_) => MSSQL_SQLCMD_INVALID_STATE,
        })
    }
}

/// Records a statement's "(n rows affected)".
///
/// # Safety
/// `document` as for [`mssql_sqlcmd_json_free`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mssql_sqlcmd_json_add_rows_affected(
    document: *mut MssqlSqlcmdJsonDocument,
    count: i64,
) -> i32 {
    // SAFETY: forwarded from the caller's guarantees.
    unsafe {
        with_document(document, |document| {
            document.add_rows_affected(count);
            MSSQL_SQLCMD_OK
        })
    }
}

/// Records a server message (`is_error` zero) or error (non-zero).
///
/// # Safety
/// `document` as for [`mssql_sqlcmd_json_free`]; `text` must have non-null data.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mssql_sqlcmd_json_add_message(
    document: *mut MssqlSqlcmdJsonDocument,
    is_error: i32,
    number: i32,
    state: i32,
    severity: i32,
    text: MssqlSqlcmdText,
) -> i32 {
    // SAFETY: forwarded from the caller's guarantees.
    let Some(text) = (unsafe { read_text(text) }) else {
        return MSSQL_SQLCMD_NULL_ARGUMENT;
    };
    // SAFETY: forwarded from the caller's guarantees.
    unsafe {
        with_document(document, |document| {
            document.add_message(Message {
                is_error: is_error != 0,
                number,
                state,
                severity,
                text,
            });
            MSSQL_SQLCMD_OK
        })
    }
}

/// Renders the document as UTF-16 into a new allocation, stored in `*out` with
/// its length in `*out_len`. Release it with [`mssql_sqlcmd_free_text`]. The
/// document itself is unchanged and still has to be freed.
///
/// # Safety
/// `document` as for [`mssql_sqlcmd_json_free`]; `version` must have non-null
/// data; the texts in `connection` follow [`MssqlSqlcmdText`]; `out` and
/// `out_len` must be valid for writes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mssql_sqlcmd_json_render(
    document: *mut MssqlSqlcmdJsonDocument,
    version: MssqlSqlcmdText,
    connection: MssqlSqlcmdConnection,
    exit_code: i32,
    out: *mut *mut u16,
    out_len: *mut usize,
) -> i32 {
    if out.is_null() || out_len.is_null() {
        return MSSQL_SQLCMD_NULL_ARGUMENT;
    }
    // SAFETY: forwarded from the caller's guarantees.
    let (version, connection) = unsafe {
        let Some(version) = read_text(version) else {
            return MSSQL_SQLCMD_NULL_ARGUMENT;
        };
        let connection = Connection {
            server: read_text(connection.server),
            database: read_text(connection.database),
            authentication: read_text(connection.authentication),
            encrypt: connection.encrypt != 0,
        };
        (version, connection)
    };
    // SAFETY: the document follows the caller's guarantees; `out` and
    // `out_len` were checked non-null and are writable, per the caller.
    unsafe {
        with_document(document, |document| {
            let rendered: Box<[u16]> = document
                .render(&version, &connection, exit_code)
                .encode_utf16()
                .collect();
            *out_len = rendered.len();
            *out = Box::into_raw(rendered).cast::<u16>();
            MSSQL_SQLCMD_OK
        })
    }
}

/// Releases text returned by [`mssql_sqlcmd_json_render`]. A null pointer is
/// ignored.
///
/// # Safety
/// `text` and `len` must be exactly what a render call returned, freed once.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mssql_sqlcmd_free_text(text: *mut u16, len: usize) {
    if !text.is_null() {
        // SAFETY: rebuilds the boxed slice `mssql_sqlcmd_json_render` leaked.
        drop(unsafe { Box::from_raw(std::ptr::slice_from_raw_parts_mut(text, len)) });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn utf16(value: &str) -> Vec<u16> {
        value.encode_utf16().collect()
    }

    fn text(units: &[u16]) -> MssqlSqlcmdText {
        MssqlSqlcmdText {
            data: units.as_ptr(),
            len: units.len(),
        }
    }

    const NULL_TEXT: MssqlSqlcmdText = MssqlSqlcmdText {
        data: std::ptr::null(),
        len: 0,
    };

    fn render(document: *mut MssqlSqlcmdJsonDocument, connection: MssqlSqlcmdConnection) -> String {
        let version = utf16("18.5");
        let mut out = std::ptr::null_mut();
        let mut out_len = 0;
        // SAFETY: test pointers are valid for the call.
        let status = unsafe {
            mssql_sqlcmd_json_render(
                document,
                text(&version),
                connection,
                0,
                &mut out,
                &mut out_len,
            )
        };
        assert_eq!(status, MSSQL_SQLCMD_OK);
        // SAFETY: `out` holds `out_len` values until it is freed below.
        let rendered =
            String::from_utf16(unsafe { std::slice::from_raw_parts(out, out_len) }).unwrap();
        // SAFETY: exactly what render returned, freed once.
        unsafe { mssql_sqlcmd_free_text(out, out_len) };
        rendered
    }

    /// The full sequence a native caller makes: values in as UTF-16, the
    /// document out as UTF-16, NULL kept apart from text.
    #[test]
    fn a_native_caller_builds_and_renders_a_document() {
        let document = mssql_sqlcmd_json_new();
        let (id, name, one, a) = (utf16("id"), utf16("name"), utf16("1"), utf16("é"));
        let columns = [text(&id), text(&name)];
        let row = [text(&one), NULL_TEXT];
        let row2 = [text(&one), text(&a)];
        let message = utf16("boom");
        let server = utf16("localhost");
        // SAFETY: test pointers are valid for each call.
        unsafe {
            assert_eq!(
                mssql_sqlcmd_json_begin_result_set(document, columns.as_ptr(), 2),
                MSSQL_SQLCMD_OK
            );
            assert_eq!(
                mssql_sqlcmd_json_add_row(document, row.as_ptr(), 2),
                MSSQL_SQLCMD_OK
            );
            assert_eq!(
                mssql_sqlcmd_json_add_row(document, row2.as_ptr(), 2),
                MSSQL_SQLCMD_OK
            );
            assert_eq!(
                mssql_sqlcmd_json_add_rows_affected(document, 2),
                MSSQL_SQLCMD_OK
            );
            assert_eq!(
                mssql_sqlcmd_json_add_message(document, 1, 50000, 1, 16, text(&message)),
                MSSQL_SQLCMD_OK
            );
        }

        let rendered = render(
            document,
            MssqlSqlcmdConnection {
                server: text(&server),
                database: NULL_TEXT,
                authentication: NULL_TEXT,
                encrypt: 1,
            },
        );
        // SAFETY: freed once.
        unsafe { mssql_sqlcmd_json_free(document) };

        assert!(rendered.contains("\"server\": \"localhost\""), "{rendered}");
        assert!(rendered.contains("\"database\": null"), "{rendered}");
        assert!(rendered.contains("\"encrypt\": true"), "{rendered}");
        assert!(
            rendered.contains("[\"1\", null],\n        [\"1\", \"é\"]"),
            "{rendered}"
        );
        assert!(rendered.contains("\"count\": 2"), "{rendered}");
        assert!(rendered.contains("\"message\": \"boom\""), "{rendered}");
    }

    #[test]
    fn null_arguments_are_reported_not_dereferenced() {
        let one = utf16("1");
        let row = [text(&one)];
        // SAFETY: null handles and pointers are what is under test.
        unsafe {
            assert_eq!(
                mssql_sqlcmd_json_add_row(std::ptr::null_mut(), row.as_ptr(), 1),
                MSSQL_SQLCMD_NULL_ARGUMENT
            );
            let document = mssql_sqlcmd_json_new();
            assert_eq!(
                mssql_sqlcmd_json_begin_result_set(document, std::ptr::null(), 1),
                MSSQL_SQLCMD_NULL_ARGUMENT
            );
            assert_eq!(
                mssql_sqlcmd_json_begin_result_set(document, [NULL_TEXT].as_ptr(), 1),
                MSSQL_SQLCMD_NULL_ARGUMENT,
                "a column name cannot be NULL"
            );
            assert_eq!(
                mssql_sqlcmd_json_add_message(document, 1, 1, 1, 16, NULL_TEXT),
                MSSQL_SQLCMD_NULL_ARGUMENT
            );
            assert_eq!(
                mssql_sqlcmd_json_render(
                    document,
                    text(&one),
                    MssqlSqlcmdConnection {
                        server: NULL_TEXT,
                        database: NULL_TEXT,
                        authentication: NULL_TEXT,
                        encrypt: 0,
                    },
                    0,
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                ),
                MSSQL_SQLCMD_NULL_ARGUMENT
            );
            mssql_sqlcmd_json_free(document);
            mssql_sqlcmd_json_free(std::ptr::null_mut());
            mssql_sqlcmd_free_text(std::ptr::null_mut(), 0);
        }
    }

    #[test]
    fn rows_that_do_not_fit_the_result_set_are_rejected() {
        let one = utf16("1");
        let row = [text(&one)];
        let document = mssql_sqlcmd_json_new();
        // SAFETY: test pointers are valid for each call.
        unsafe {
            assert_eq!(
                mssql_sqlcmd_json_add_row(document, row.as_ptr(), 1),
                MSSQL_SQLCMD_INVALID_STATE,
                "no result set yet"
            );
            assert_eq!(
                mssql_sqlcmd_json_begin_result_set(document, [text(&one), text(&one)].as_ptr(), 2),
                MSSQL_SQLCMD_OK
            );
            assert_eq!(
                mssql_sqlcmd_json_add_row(document, row.as_ptr(), 1),
                MSSQL_SQLCMD_INVALID_STATE,
                "one value for two columns"
            );
            mssql_sqlcmd_json_free(document);
        }
    }

    /// Invalid UTF-16 from the caller is replaced, not rejected, so one bad
    /// value cannot lose the whole document.
    #[test]
    fn unpaired_surrogates_are_replaced() {
        let document = mssql_sqlcmd_json_new();
        let column = utf16("v");
        let lone = [0xD800_u16];
        // SAFETY: test pointers are valid for each call.
        unsafe {
            mssql_sqlcmd_json_begin_result_set(document, [text(&column)].as_ptr(), 1);
            assert_eq!(
                mssql_sqlcmd_json_add_row(document, [text(&lone)].as_ptr(), 1),
                MSSQL_SQLCMD_OK
            );
        }
        let rendered = render(
            document,
            MssqlSqlcmdConnection {
                server: NULL_TEXT,
                database: NULL_TEXT,
                authentication: NULL_TEXT,
                encrypt: 0,
            },
        );
        // SAFETY: freed once.
        unsafe { mssql_sqlcmd_json_free(document) };
        assert!(rendered.contains("[\"\u{FFFD}\"]"), "{rendered}");
    }
}
