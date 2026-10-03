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
//! [`mssql_sqlcmd_json_new`] and [`mssql_sqlcmd_json_free`]. A handle is not
//! thread-safe: calls on one handle, including the final free, must not run
//! concurrently (native sqlcmd makes them all from one thread). Different
//! handles are independent. The rendered document is a separate allocation,
//! released with [`mssql_sqlcmd_free_text`]. Every function that can fail returns an
//! [`MSSQL_SQLCMD_OK`]-style status rather than unwinding: a panic is caught at
//! the boundary and reported as [`MSSQL_SQLCMD_INTERNAL_ERROR`].
//!
//! The document times the run itself: [`mssql_sqlcmd_json_new`] starts the
//! clock, so native sqlcmd creates it at startup, before it connects.
//!
//! The header for C and C++ callers is `include/mssql_sqlcmd.h`.

use std::panic::{AssertUnwindSafe, catch_unwind};

use crate::formatter::json::{Column, Connection, Failure, JsonDocument, Message, sql_type};

/// The call succeeded.
pub const MSSQL_SQLCMD_OK: i32 = 0;
/// A required pointer was null.
pub const MSSQL_SQLCMD_NULL_ARGUMENT: i32 = 1;
/// The call does not fit the document's state, e.g. a row before any result
/// set, or a row whose value count differs from the column count.
pub const MSSQL_SQLCMD_INVALID_STATE: i32 = 2;
/// A Rust panic was caught at the boundary.
pub const MSSQL_SQLCMD_INTERNAL_ERROR: i32 = 3;
/// An argument is out of its range, e.g. an unknown failure kind.
pub const MSSQL_SQLCMD_INVALID_ARGUMENT: i32 = 4;

/// The run succeeded; [`mssql_sqlcmd_json_render`]'s `failure` argument.
pub const MSSQL_SQLCMD_FAILURE_NONE: i32 = 0;
/// The server could not be reached, or the connection was lost.
pub const MSSQL_SQLCMD_FAILURE_CONNECTION: i32 = 1;
/// The server refused the login.
pub const MSSQL_SQLCMD_FAILURE_AUTHENTICATION: i32 = 2;
/// A statement error stopped the run.
pub const MSSQL_SQLCMD_FAILURE_QUERY: i32 = 3;
/// The last error before the run failed was a query timeout.
pub const MSSQL_SQLCMD_FAILURE_TIMEOUT: i32 = 4;
/// The run was cancelled.
pub const MSSQL_SQLCMD_FAILURE_CANCELLED: i32 = 5;
/// Any other reason.
pub const MSSQL_SQLCMD_FAILURE_OTHER: i32 = 6;

/// The crate version, NUL-terminated, so a caller can confirm which build of
/// the library it linked.
static VERSION: &str = concat!(env!("CARGO_PKG_VERSION"), "\0");

/// Returns the library's version as a NUL-terminated UTF-8 string, e.g.
/// `0.1.0`. The string is static: the caller must not free it.
#[unsafe(no_mangle)]
pub extern "C" fn mssql_sqlcmd_version() -> *const std::ffi::c_char {
    VERSION.as_ptr().cast()
}

/// A UTF-16 string. `data` may be null only where a value is optional, in which
/// case the value is absent (SQL `NULL` for a row value); `len` is then
/// ignored. Invalid UTF-16 is replaced with U+FFFD rather than rejected.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct MssqlSqlcmdText {
    pub data: *const u16,
    pub len: usize,
}

/// A result-set column. The type is given as the driver describes it, and
/// written out the way T-SQL declares it (e.g. `nvarchar(50)`).
#[repr(C)]
#[derive(Clone, Copy)]
pub struct MssqlSqlcmdColumn {
    /// The column name; may not be null (an unnamed column is empty).
    pub name: MssqlSqlcmdText,
    /// The server's type name, e.g. `nvarchar` or `decimal`; may not be null.
    pub type_name: MssqlSqlcmdText,
    /// Length in characters (character types) or bytes (binary types); 0 or
    /// less means `max`.
    pub length: i64,
    /// Precision of `decimal` and `numeric`.
    pub precision: i32,
    /// Scale of `decimal` and `numeric`; fractional-second digits of
    /// `datetime2`, `time` and `datetimeoffset`.
    pub scale: i32,
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

/// Reads an optional text, where both a null pointer and an empty text mean
/// "absent".
///
/// # Safety
/// As for [`read_text`].
unsafe fn read_optional_text(text: MssqlSqlcmdText) -> Option<String> {
    // SAFETY: forwarded from the caller's guarantees.
    unsafe { read_text(text) }.filter(|text| !text.is_empty())
}

/// Reads an array of `count` values with `read`.
///
/// # Safety
/// `items` must point to `count` readable values, each satisfying `read`'s
/// requirements. It may be null only when `count` is 0.
unsafe fn read_array<T: Copy, R>(
    items: *const T,
    count: usize,
    read: impl FnMut(T) -> Option<R>,
) -> Option<Vec<Option<R>>> {
    if count == 0 {
        return Some(Vec::new());
    }
    if items.is_null() {
        return None;
    }
    // SAFETY: the caller guarantees `count` readable values.
    let items = unsafe { std::slice::from_raw_parts(items, count) };
    Some(items.iter().copied().map(read).collect())
}

/// Runs `body` with the document behind `document`, turning a null handle and
/// a panic into status codes.
///
/// # Safety
/// `document` must be null or a live handle from [`mssql_sqlcmd_json_new`]
/// that no other call is using concurrently: the body gets exclusive access.
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

/// Creates an empty document and starts its clock. Returns null only if a
/// panic was caught.
#[unsafe(no_mangle)]
pub extern "C" fn mssql_sqlcmd_json_new() -> *mut MssqlSqlcmdJsonDocument {
    catch_unwind(|| Box::into_raw(Box::new(MssqlSqlcmdJsonDocument(JsonDocument::new()))))
        .unwrap_or(std::ptr::null_mut())
}

/// Releases a document. A null handle is ignored.
///
/// # Safety
/// `document` must be null or a handle from [`mssql_sqlcmd_json_new`] that has
/// not been freed and that no other call is using concurrently.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mssql_sqlcmd_json_free(document: *mut MssqlSqlcmdJsonDocument) {
    if !document.is_null() {
        // SAFETY: the handle came from `Box::into_raw` and is freed once.
        drop(unsafe { Box::from_raw(document) });
    }
}

/// Notes that sqlcmd started connecting.
///
/// # Safety
/// `document` as for [`mssql_sqlcmd_json_free`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mssql_sqlcmd_json_connecting(
    document: *mut MssqlSqlcmdJsonDocument,
) -> i32 {
    // SAFETY: forwarded from the caller's guarantees.
    unsafe {
        with_document(document, |document| {
            document.connecting();
            MSSQL_SQLCMD_OK
        })
    }
}

/// Notes that sqlcmd connected, with the version the server reported (null or
/// empty when unknown). The time since [`mssql_sqlcmd_json_connecting`]
/// becomes `connectMs`.
///
/// # Safety
/// `document` as for [`mssql_sqlcmd_json_free`]; `server_version` follows
/// [`MssqlSqlcmdText`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mssql_sqlcmd_json_connected(
    document: *mut MssqlSqlcmdJsonDocument,
    server_version: MssqlSqlcmdText,
) -> i32 {
    // SAFETY: forwarded from the caller's guarantees.
    let server_version = unsafe { read_optional_text(server_version) };
    // SAFETY: forwarded from the caller's guarantees.
    unsafe {
        with_document(document, |document| {
            document.connected(server_version);
            MSSQL_SQLCMD_OK
        })
    }
}

/// Starts a batch sent to the server. `text` is the batch as sent, or null to
/// leave it out. A batch still running is ended first.
///
/// # Safety
/// `document` as for [`mssql_sqlcmd_json_free`]; `text` follows
/// [`MssqlSqlcmdText`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mssql_sqlcmd_json_begin_batch(
    document: *mut MssqlSqlcmdJsonDocument,
    text: MssqlSqlcmdText,
) -> i32 {
    // SAFETY: forwarded from the caller's guarantees.
    let text = unsafe { read_text(text) };
    // SAFETY: forwarded from the caller's guarantees.
    unsafe {
        with_document(document, |document| {
            document.begin_batch(text);
            MSSQL_SQLCMD_OK
        })
    }
}

/// Ends the running batch, recording its duration. Does nothing when no batch
/// is running.
///
/// # Safety
/// `document` as for [`mssql_sqlcmd_json_free`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mssql_sqlcmd_json_end_batch(
    document: *mut MssqlSqlcmdJsonDocument,
) -> i32 {
    // SAFETY: forwarded from the caller's guarantees.
    unsafe {
        with_document(document, |document| {
            document.end_batch();
            MSSQL_SQLCMD_OK
        })
    }
}

/// Starts a result set with `column_count` columns.
///
/// # Safety
/// `document` as for [`mssql_sqlcmd_json_free`]; `columns` must point to
/// `column_count` columns whose name and type name have non-null data.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mssql_sqlcmd_json_begin_result_set(
    document: *mut MssqlSqlcmdJsonDocument,
    columns: *const MssqlSqlcmdColumn,
    column_count: usize,
) -> i32 {
    // SAFETY: forwarded from the caller's guarantees.
    let columns = unsafe {
        read_array(columns, column_count, |column: MssqlSqlcmdColumn| {
            Some(Column {
                name: read_text(column.name)?,
                sql_type: sql_type(
                    &read_text(column.type_name)?,
                    column.length,
                    column.precision,
                    column.scale,
                ),
            })
        })
    };
    let Some(columns) = columns.and_then(|columns| columns.into_iter().collect::<Option<Vec<_>>>())
    else {
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
    let Some(values) = (unsafe { read_array(values, value_count, |value| read_text(value)) })
    else {
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

/// Ends the current result set with the "(n rows affected)" sqlcmd reports
/// for it, when `has_count` is non-zero, or with none (`SET NOCOUNT ON`).
///
/// # Safety
/// `document` as for [`mssql_sqlcmd_json_free`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mssql_sqlcmd_json_end_result_set(
    document: *mut MssqlSqlcmdJsonDocument,
    has_count: i32,
    count: i64,
) -> i32 {
    let count = (has_count != 0).then_some(count);
    // SAFETY: forwarded from the caller's guarantees.
    unsafe {
        with_document(document, |document| match document.end_result_set(count) {
            Ok(()) => MSSQL_SQLCMD_OK,
            Err(_) => MSSQL_SQLCMD_INVALID_STATE,
        })
    }
}

/// Records the "(n rows affected)" of a statement that returned no result set.
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

/// Records a server message (`is_error` zero) or error (non-zero). `line` is
/// the line the server reported, or 0 or less for none; `procedure` is null or
/// empty when the message did not come from a stored procedure.
///
/// # Safety
/// `document` as for [`mssql_sqlcmd_json_free`]; `procedure` follows
/// [`MssqlSqlcmdText`]; `text` must have non-null data.
#[unsafe(no_mangle)]
#[allow(clippy::too_many_arguments)]
pub unsafe extern "C" fn mssql_sqlcmd_json_add_message(
    document: *mut MssqlSqlcmdJsonDocument,
    is_error: i32,
    number: i32,
    state: i32,
    severity: i32,
    line: i32,
    procedure: MssqlSqlcmdText,
    text: MssqlSqlcmdText,
) -> i32 {
    // SAFETY: forwarded from the caller's guarantees.
    let (procedure, text) = unsafe { (read_optional_text(procedure), read_text(text)) };
    let Some(text) = text else {
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
                line: (line > 0).then_some(line),
                procedure,
                text,
            });
            MSSQL_SQLCMD_OK
        })
    }
}

/// The failure kind for a `MSSQL_SQLCMD_FAILURE_*` value; `None` for
/// [`MSSQL_SQLCMD_FAILURE_NONE`], `Err` for an unknown value.
fn failure_from(value: i32) -> Result<Option<Failure>, ()> {
    Ok(Some(match value {
        MSSQL_SQLCMD_FAILURE_NONE => return Ok(None),
        MSSQL_SQLCMD_FAILURE_CONNECTION => Failure::Connection,
        MSSQL_SQLCMD_FAILURE_AUTHENTICATION => Failure::Authentication,
        MSSQL_SQLCMD_FAILURE_QUERY => Failure::Query,
        MSSQL_SQLCMD_FAILURE_TIMEOUT => Failure::Timeout,
        MSSQL_SQLCMD_FAILURE_CANCELLED => Failure::Cancelled,
        MSSQL_SQLCMD_FAILURE_OTHER => Failure::Other,
        _ => return Err(()),
    }))
}

/// Renders the document as UTF-16 into a new allocation, stored in `*out` with
/// its length in `*out_len`. Release it with [`mssql_sqlcmd_free_text`]. The
/// document itself is unchanged and still has to be freed. On any failure
/// `*out` is null and `*out_len` is 0, so nothing stale can be freed.
///
/// `failure` is a `MSSQL_SQLCMD_FAILURE_*` value saying why the run failed, or
/// [`MSSQL_SQLCMD_FAILURE_NONE`].
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
    failure: i32,
    out: *mut *mut u16,
    out_len: *mut usize,
) -> i32 {
    if out.is_null() || out_len.is_null() {
        return MSSQL_SQLCMD_NULL_ARGUMENT;
    }
    // SAFETY: both were checked non-null and are writable, per the caller.
    unsafe {
        *out = std::ptr::null_mut();
        *out_len = 0;
    }
    let Ok(failure) = failure_from(failure) else {
        return MSSQL_SQLCMD_INVALID_ARGUMENT;
    };
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
                .render(&version, &connection, exit_code, failure)
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

    #[test]
    fn the_version_is_the_crate_version() {
        // SAFETY: `mssql_sqlcmd_version` returns a static NUL-terminated string.
        let version = unsafe { std::ffi::CStr::from_ptr(mssql_sqlcmd_version()) };
        assert_eq!(version.to_str().unwrap(), env!("CARGO_PKG_VERSION"));
    }

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

    const NO_CONNECTION: MssqlSqlcmdConnection = MssqlSqlcmdConnection {
        server: NULL_TEXT,
        database: NULL_TEXT,
        authentication: NULL_TEXT,
        encrypt: 0,
    };

    fn column(name: &[u16], type_name: &[u16], length: i64) -> MssqlSqlcmdColumn {
        MssqlSqlcmdColumn {
            name: text(name),
            type_name: text(type_name),
            length,
            precision: 0,
            scale: 0,
        }
    }

    fn render_with(
        document: *mut MssqlSqlcmdJsonDocument,
        connection: MssqlSqlcmdConnection,
        exit_code: i32,
        failure: i32,
    ) -> String {
        let version = utf16("18.5");
        let mut out = std::ptr::null_mut();
        let mut out_len = 0;
        // SAFETY: test pointers are valid for the call.
        let status = unsafe {
            mssql_sqlcmd_json_render(
                document,
                text(&version),
                connection,
                exit_code,
                failure,
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

    fn render(document: *mut MssqlSqlcmdJsonDocument, connection: MssqlSqlcmdConnection) -> String {
        render_with(document, connection, 0, MSSQL_SQLCMD_FAILURE_NONE)
    }

    /// The full sequence a native caller makes: values in as UTF-16, the
    /// document out as UTF-16, NULL kept apart from text.
    #[test]
    fn a_native_caller_builds_and_renders_a_document() {
        let document = mssql_sqlcmd_json_new();
        let (id, name, int, nvarchar) =
            (utf16("id"), utf16("name"), utf16("int"), utf16("nvarchar"));
        let (one, a, batch) = (utf16("1"), utf16("é"), utf16("SELECT 1"));
        let columns = [column(&id, &int, 4), column(&name, &nvarchar, 50)];
        let row = [text(&one), NULL_TEXT];
        let row2 = [text(&one), text(&a)];
        let (message, procedure) = (utf16("boom"), utf16("p"));
        let (server, server_version) = (utf16("localhost"), utf16("17.00.1000"));
        // SAFETY: test pointers are valid for each call.
        unsafe {
            assert_eq!(mssql_sqlcmd_json_connecting(document), MSSQL_SQLCMD_OK);
            assert_eq!(
                mssql_sqlcmd_json_connected(document, text(&server_version)),
                MSSQL_SQLCMD_OK
            );
            assert_eq!(
                mssql_sqlcmd_json_begin_batch(document, text(&batch)),
                MSSQL_SQLCMD_OK
            );
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
                mssql_sqlcmd_json_end_result_set(document, 1, 2),
                MSSQL_SQLCMD_OK
            );
            assert_eq!(
                mssql_sqlcmd_json_add_rows_affected(document, 3),
                MSSQL_SQLCMD_OK
            );
            assert_eq!(
                mssql_sqlcmd_json_add_message(
                    document,
                    1,
                    50000,
                    1,
                    16,
                    2,
                    text(&procedure),
                    text(&message)
                ),
                MSSQL_SQLCMD_OK
            );
            assert_eq!(mssql_sqlcmd_json_end_batch(document), MSSQL_SQLCMD_OK);
        }

        let rendered = render_with(
            document,
            MssqlSqlcmdConnection {
                server: text(&server),
                encrypt: 1,
                ..NO_CONNECTION
            },
            1,
            MSSQL_SQLCMD_FAILURE_QUERY,
        );
        // SAFETY: freed once.
        unsafe { mssql_sqlcmd_json_free(document) };

        for expected in [
            "\"formatVersion\": 1",
            "\"server\": \"localhost\"",
            "\"database\": null",
            "\"encrypt\": true",
            "\"serverVersion\": \"17.00.1000\"",
            "\"connectMs\": ",
            "\"startTime\": \"",
            "\"durationMs\": ",
            "\"status\": \"failed\",\n  \"failure\": {\n    \"kind\": \"query\"\n  },\n  \"exitCode\": 1",
            "\"type\": \"batch\",\n      \"index\": 1,\n      \"durationMs\": ",
            "\"text\": \"SELECT 1\"",
            "{ \"name\": \"id\", \"type\": \"int\" }",
            "{ \"name\": \"name\", \"type\": \"nvarchar(50)\" }",
            "[\"1\", null],\n        [\"1\", \"é\"]",
            "\"rowsAffected\": 2",
            "\"count\": 3",
            "\"line\": 2,\n      \"procedure\": \"p\",\n      \"message\": \"boom\"",
        ] {
            assert!(
                rendered.contains(expected),
                "missing {expected:?} in {rendered}"
            );
        }
    }

    /// A null batch text leaves the text out; an empty or null server version
    /// and procedure, and a line of 0, are absent.
    #[test]
    fn absent_optional_values_are_left_out() {
        let document = mssql_sqlcmd_json_new();
        let (empty, message) = (utf16(""), utf16("hi"));
        // SAFETY: test pointers are valid for each call.
        unsafe {
            mssql_sqlcmd_json_connected(document, text(&empty));
            mssql_sqlcmd_json_begin_batch(document, NULL_TEXT);
            mssql_sqlcmd_json_add_message(document, 0, 0, 1, 0, 0, text(&empty), text(&message));
        }
        let rendered = render(document, NO_CONNECTION);
        // SAFETY: freed once.
        unsafe { mssql_sqlcmd_json_free(document) };
        for absent in ["serverVersion", "\"text\"", "\"line\"", "\"procedure\""] {
            assert!(!rendered.contains(absent), "{absent} in {rendered}");
        }
        assert!(rendered.contains("\"connectMs\": "), "{rendered}");
        assert!(rendered.contains("\"status\": \"success\""), "{rendered}");
    }

    #[test]
    fn every_failure_value_maps_to_its_kind() {
        assert_eq!(failure_from(MSSQL_SQLCMD_FAILURE_NONE), Ok(None));
        let kinds = [
            (MSSQL_SQLCMD_FAILURE_CONNECTION, Failure::Connection),
            (MSSQL_SQLCMD_FAILURE_AUTHENTICATION, Failure::Authentication),
            (MSSQL_SQLCMD_FAILURE_QUERY, Failure::Query),
            (MSSQL_SQLCMD_FAILURE_TIMEOUT, Failure::Timeout),
            (MSSQL_SQLCMD_FAILURE_CANCELLED, Failure::Cancelled),
            (MSSQL_SQLCMD_FAILURE_OTHER, Failure::Other),
        ];
        for (value, kind) in kinds {
            assert_eq!(failure_from(value), Ok(Some(kind)));
        }
        assert_eq!(failure_from(7), Err(()));
        assert_eq!(failure_from(-1), Err(()));
    }

    #[test]
    fn an_unknown_failure_value_is_rejected_and_clears_the_outputs() {
        let document = mssql_sqlcmd_json_new();
        let version = utf16("18.5");
        let mut out = std::ptr::NonNull::<u16>::dangling().as_ptr();
        let mut out_len = 42;
        // SAFETY: test pointers are valid for the call.
        let status = unsafe {
            mssql_sqlcmd_json_render(
                document,
                text(&version),
                NO_CONNECTION,
                1,
                99,
                &mut out,
                &mut out_len,
            )
        };
        // SAFETY: freed once.
        unsafe { mssql_sqlcmd_json_free(document) };
        assert_eq!(status, MSSQL_SQLCMD_INVALID_ARGUMENT);
        assert!(out.is_null());
        assert_eq!(out_len, 0);
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
            assert_eq!(
                mssql_sqlcmd_json_connecting(std::ptr::null_mut()),
                MSSQL_SQLCMD_NULL_ARGUMENT
            );
            assert_eq!(
                mssql_sqlcmd_json_end_batch(std::ptr::null_mut()),
                MSSQL_SQLCMD_NULL_ARGUMENT
            );
            let document = mssql_sqlcmd_json_new();
            assert_eq!(
                mssql_sqlcmd_json_begin_result_set(document, std::ptr::null(), 1),
                MSSQL_SQLCMD_NULL_ARGUMENT
            );
            assert_eq!(
                mssql_sqlcmd_json_begin_result_set(document, [column(&one, &[], 0)].as_ptr(), 1),
                MSSQL_SQLCMD_OK,
                "an empty type name is still a type name"
            );
            let unnamed = MssqlSqlcmdColumn {
                name: NULL_TEXT,
                ..column(&one, &one, 0)
            };
            assert_eq!(
                mssql_sqlcmd_json_begin_result_set(document, [unnamed].as_ptr(), 1),
                MSSQL_SQLCMD_NULL_ARGUMENT,
                "a column name cannot be NULL"
            );
            let untyped = MssqlSqlcmdColumn {
                type_name: NULL_TEXT,
                ..column(&one, &one, 0)
            };
            assert_eq!(
                mssql_sqlcmd_json_begin_result_set(document, [untyped].as_ptr(), 1),
                MSSQL_SQLCMD_NULL_ARGUMENT,
                "a type name cannot be NULL"
            );
            assert_eq!(
                mssql_sqlcmd_json_add_message(document, 1, 1, 1, 16, 0, NULL_TEXT, NULL_TEXT),
                MSSQL_SQLCMD_NULL_ARGUMENT
            );
            assert_eq!(
                mssql_sqlcmd_json_render(
                    document,
                    text(&one),
                    NO_CONNECTION,
                    0,
                    MSSQL_SQLCMD_FAILURE_NONE,
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
    fn rows_and_counts_that_do_not_fit_the_result_set_are_rejected() {
        let (one, int) = (utf16("1"), utf16("int"));
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
                mssql_sqlcmd_json_end_result_set(document, 1, 1),
                MSSQL_SQLCMD_INVALID_STATE,
                "no result set to end"
            );
            let columns = [column(&one, &int, 4), column(&one, &int, 4)];
            assert_eq!(
                mssql_sqlcmd_json_begin_result_set(document, columns.as_ptr(), 2),
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
        let (name, int) = (utf16("v"), utf16("int"));
        let lone = [0xD800_u16];
        // SAFETY: test pointers are valid for each call.
        unsafe {
            mssql_sqlcmd_json_begin_result_set(document, [column(&name, &int, 4)].as_ptr(), 1);
            assert_eq!(
                mssql_sqlcmd_json_add_row(document, [text(&lone)].as_ptr(), 1),
                MSSQL_SQLCMD_OK
            );
        }
        let rendered = render(document, NO_CONNECTION);
        // SAFETY: freed once.
        unsafe { mssql_sqlcmd_json_free(document) };
        assert!(rendered.contains("[\"\u{FFFD}\"]"), "{rendered}");
    }

    /// A panic inside a call is caught at the boundary and reported as a
    /// status, rather than unwinding into the native caller; the handle stays
    /// usable afterwards.
    #[test]
    fn a_panic_is_reported_as_an_internal_error() {
        let document = mssql_sqlcmd_json_new();
        // SAFETY: a live handle, used by this call only.
        let status = unsafe {
            with_document(document, |_| -> i32 {
                panic!("forced panic at the boundary")
            })
        };
        assert_eq!(status, MSSQL_SQLCMD_INTERNAL_ERROR);

        let rendered = render(document, NO_CONNECTION);
        // SAFETY: freed once.
        unsafe { mssql_sqlcmd_json_free(document) };
        assert!(rendered.contains("\"output\": []"), "{rendered}");
    }

    /// A failed render leaves the caller's out-parameters null and empty, so
    /// an error path that frees them anyway frees nothing.
    #[test]
    fn a_failed_render_clears_the_outputs() {
        let version = utf16("18.5");
        let mut out = std::ptr::NonNull::<u16>::dangling().as_ptr();
        let mut out_len = 42;
        // SAFETY: a null document is reported before anything is read.
        let status = unsafe {
            mssql_sqlcmd_json_render(
                std::ptr::null_mut(),
                text(&version),
                NO_CONNECTION,
                0,
                MSSQL_SQLCMD_FAILURE_NONE,
                &mut out,
                &mut out_len,
            )
        };
        assert_eq!(status, MSSQL_SQLCMD_NULL_ARGUMENT);
        assert!(out.is_null());
        assert_eq!(out_len, 0);
    }
}
