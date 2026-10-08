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

use crate::formatter::json::{Column, Connection, JsonDocument, Message, RunEnd};

/// The call succeeded.
pub const MSSQL_SQLCMD_OK: i32 = 0;
/// A required pointer was null.
pub const MSSQL_SQLCMD_NULL_ARGUMENT: i32 = 1;
/// The call does not fit the document's state, e.g. a row before any result
/// set, or a row whose value count differs from the column count.
pub const MSSQL_SQLCMD_INVALID_STATE: i32 = 2;
/// A Rust panic was caught at the boundary.
pub const MSSQL_SQLCMD_INTERNAL_ERROR: i32 = 3;
/// An argument is out of its range, e.g. an unknown `MSSQL_SQLCMD_RUN_*` value.
pub const MSSQL_SQLCMD_INVALID_ARGUMENT: i32 = 4;

/// [`mssql_sqlcmd_json_render`]'s `end`: sqlcmd ran to its end, and the exit
/// code says whether the work succeeded.
pub const MSSQL_SQLCMD_RUN_FINISHED: i32 = 0;
/// The run was canceled, e.g. with Ctrl+C.
pub const MSSQL_SQLCMD_RUN_CANCELED: i32 = 1;
/// The command line was rejected; nothing ran.
pub const MSSQL_SQLCMD_RUN_INVALID_INVOCATION: i32 = 2;

/// The crate version, NUL-terminated: the release of the library a caller
/// linked. Test packages (`-dev`, `-nightly`) built from the same crate version
/// report the same value; the package version itself identifies those builds.
static VERSION: &str = concat!(env!("CARGO_PKG_VERSION"), "\0");

/// Returns the library's crate version as a NUL-terminated UTF-8 string, e.g.
/// `0.1.0`. The string is static: the caller must not free it.
#[unsafe(no_mangle)]
pub extern "C" fn mssql_sqlcmd_version() -> *const std::ffi::c_char {
    VERSION.as_ptr().cast()
}

/// A UTF-16 string with an explicit length. It need not be NUL-terminated and
/// may contain embedded NULs. `data` may be null only where a value is
/// optional, in which case the value is absent (SQL `NULL` for a row value);
/// `len` is then ignored. An empty optional text is absent too, except a row
/// value, which is then the empty string. Invalid UTF-16 is replaced with
/// U+FFFD rather than rejected.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct MssqlSqlcmdText {
    pub data: *const u16,
    pub len: usize,
}

/// A result-set column, as the driver describes it. Everything but the name
/// is optional: a negative number or a null or empty type name means the
/// driver did not expose it, and the field is left out of the document.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct MssqlSqlcmdColumn {
    /// The column name; null or empty for an unnamed column (JSON `null`).
    pub name: MssqlSqlcmdText,
    /// The type name exactly as the driver reports it, e.g. `nvarchar` or
    /// `int identity`.
    pub driver_type: MssqlSqlcmdText,
    /// Size in characters (character types) or bytes (binary types).
    pub size: i64,
    pub precision: i32,
    pub scale: i32,
    /// 0 not nullable, 1 nullable, anything else unknown.
    pub nullable: i32,
}
/// The `connection` object of the document. A null or empty text is unknown
/// (JSON `null`).
#[repr(C)]
#[derive(Clone, Copy)]
pub struct MssqlSqlcmdConnection {
    pub server: MssqlSqlcmdText,
    pub database: MssqlSqlcmdText,
    pub authentication: MssqlSqlcmdText,
    /// Non-zero when the connection is encrypted.
    pub encrypt: i32,
}

/// A message or error, with what the console shows for it. Every text but
/// `text` is optional (null or empty when absent).
#[repr(C)]
#[derive(Clone, Copy)]
pub struct MssqlSqlcmdMessage {
    /// Non-zero for an error, zero for an informational message.
    pub is_error: i32,
    /// The error number (`Msg`), 0 when there is none.
    pub number: i32,
    /// The severity (`Level`).
    pub severity: i32,
    pub state: i32,
    /// The line the server reported, or 0 or less for none.
    pub line: i32,
    /// The server that sent it.
    pub server: MssqlSqlcmdText,
    /// The stored procedure it came from.
    pub procedure: MssqlSqlcmdText,
    /// Who reported it when it was not the server: the driver, or `Sqlcmd`.
    pub source: MssqlSqlcmdText,
    /// The ODBC SQLSTATE.
    pub sql_state: MssqlSqlcmdText,
    /// The message text; must have non-null data.
    pub text: MssqlSqlcmdText,
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

/// Reads an array of `count` values with `read`; `None` when `items` is null
/// and `count` is not 0.
///
/// # Safety
/// `items` must point to `count` readable values, each satisfying `read`'s
/// requirements. It may be null only when `count` is 0.
unsafe fn read_array<T: Copy, R>(
    items: *const T,
    count: usize,
    read: impl FnMut(T) -> R,
) -> Option<Vec<R>> {
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

/// The boundary every document call goes through. A null handle is reported
/// before `body` reads any other input; `body` reads the inputs and applies
/// the call, returning `Err(status)` to report a failure. A panic anywhere in
/// `body` is caught and reported as [`MSSQL_SQLCMD_INTERNAL_ERROR`] rather
/// than unwinding into the native caller.
///
/// # Safety
/// `document` must be null or a live handle from [`mssql_sqlcmd_json_new`]
/// that no other call is using concurrently: the body gets exclusive access.
unsafe fn with_document(
    document: *mut MssqlSqlcmdJsonDocument,
    body: impl FnOnce(&mut JsonDocument) -> Result<(), i32>,
) -> i32 {
    if document.is_null() {
        return MSSQL_SQLCMD_NULL_ARGUMENT;
    }
    // SAFETY: a non-null handle is live and not aliased, per the caller.
    let document = unsafe { &mut (*document).0 };
    match catch_unwind(AssertUnwindSafe(|| body(document))) {
        Ok(Ok(())) => MSSQL_SQLCMD_OK,
        Ok(Err(status)) => status,
        Err(_) => MSSQL_SQLCMD_INTERNAL_ERROR,
    }
}

/// Creates an empty document and starts its clock. Returns null only if a
/// panic was caught.
#[unsafe(no_mangle)]
pub extern "C" fn mssql_sqlcmd_json_new() -> *mut MssqlSqlcmdJsonDocument {
    catch_unwind(|| Box::into_raw(Box::new(MssqlSqlcmdJsonDocument(JsonDocument::new()))))
        .unwrap_or(std::ptr::null_mut())
}

/// Releases a document. A null handle is ignored. Nothing unwinds across the
/// boundary: a panic while dropping is caught and the handle is gone either way.
///
/// # Safety
/// `document` must be null or a handle from [`mssql_sqlcmd_json_new`] that has
/// not been freed and that no other call is using concurrently.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mssql_sqlcmd_json_free(document: *mut MssqlSqlcmdJsonDocument) {
    if !document.is_null() {
        // SAFETY: the handle came from `Box::into_raw` and is freed once.
        let document = unsafe { Box::from_raw(document) };
        let _ = catch_unwind(AssertUnwindSafe(|| drop(document)));
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
            Ok(())
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
    unsafe {
        with_document(document, |document| {
            document.connected(read_optional_text(server_version));
            Ok(())
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
    unsafe {
        with_document(document, |document| {
            document.begin_batch(read_text(text));
            Ok(())
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
            Ok(())
        })
    }
}

/// Starts a result set with `column_count` columns.
///
/// # Safety
/// `document` as for [`mssql_sqlcmd_json_free`]; `columns` must point to
/// `column_count` columns whose texts follow [`MssqlSqlcmdText`]. It may be null
/// when `column_count` is 0.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mssql_sqlcmd_json_begin_result_set(
    document: *mut MssqlSqlcmdJsonDocument,
    columns: *const MssqlSqlcmdColumn,
    column_count: usize,
) -> i32 {
    // SAFETY: forwarded from the caller's guarantees.
    unsafe {
        with_document(document, |document| {
            let columns = read_array(columns, column_count, |column: MssqlSqlcmdColumn| Column {
                name: read_optional_text(column.name),
                driver_type: read_optional_text(column.driver_type),
                size: (column.size >= 0).then_some(column.size),
                precision: (column.precision >= 0).then_some(column.precision),
                scale: (column.scale >= 0).then_some(column.scale),
                nullable: match column.nullable {
                    0 => Some(false),
                    1 => Some(true),
                    _ => None,
                },
            })
            .ok_or(MSSQL_SQLCMD_NULL_ARGUMENT)?;
            document.begin_result_set(columns);
            Ok(())
        })
    }
}

/// Adds a row to the current result set. A value whose `data` is null is SQL
/// `NULL`.
///
/// # Safety
/// `document` as for [`mssql_sqlcmd_json_free`]; `values` must point to
/// `value_count` texts. It may be null when `value_count` is 0.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mssql_sqlcmd_json_add_row(
    document: *mut MssqlSqlcmdJsonDocument,
    values: *const MssqlSqlcmdText,
    value_count: usize,
) -> i32 {
    // SAFETY: forwarded from the caller's guarantees.
    unsafe {
        with_document(document, |document| {
            let values = read_array(values, value_count, |value| read_text(value))
                .ok_or(MSSQL_SQLCMD_NULL_ARGUMENT)?;
            document
                .add_row(values)
                .map_err(|_| MSSQL_SQLCMD_INVALID_STATE)
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
    // SAFETY: forwarded from the caller's guarantees.
    unsafe {
        with_document(document, |document| {
            document
                .end_result_set((has_count != 0).then_some(count))
                .map_err(|_| MSSQL_SQLCMD_INVALID_STATE)
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
            Ok(())
        })
    }
}

/// Records a message or error.
///
/// # Safety
/// `document` as for [`mssql_sqlcmd_json_free`]; `message` must point to a
/// readable [`MssqlSqlcmdMessage`] whose texts follow [`MssqlSqlcmdText`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mssql_sqlcmd_json_add_message(
    document: *mut MssqlSqlcmdJsonDocument,
    message: *const MssqlSqlcmdMessage,
) -> i32 {
    // SAFETY: forwarded from the caller's guarantees.
    unsafe {
        with_document(document, |document| {
            if message.is_null() {
                return Err(MSSQL_SQLCMD_NULL_ARGUMENT);
            }
            let message = *message;
            document.add_message(Message {
                is_error: message.is_error != 0,
                number: message.number,
                severity: message.severity,
                state: message.state,
                server: read_optional_text(message.server),
                procedure: read_optional_text(message.procedure),
                line: (message.line > 0).then_some(message.line),
                source: read_optional_text(message.source),
                sql_state: read_optional_text(message.sql_state),
                text: read_text(message.text).ok_or(MSSQL_SQLCMD_NULL_ARGUMENT)?,
            });
            Ok(())
        })
    }
}

/// The run end for a `MSSQL_SQLCMD_RUN_*` value; `None` for an unknown value.
fn run_end_from(value: i32) -> Option<RunEnd> {
    match value {
        MSSQL_SQLCMD_RUN_FINISHED => Some(RunEnd::Finished),
        MSSQL_SQLCMD_RUN_CANCELED => Some(RunEnd::Canceled),
        MSSQL_SQLCMD_RUN_INVALID_INVOCATION => Some(RunEnd::InvalidInvocation),
        _ => None,
    }
}

/// Renders the document as UTF-16 into a new allocation, stored in `*out` with
/// its length in `*out_len`. Release it with [`mssql_sqlcmd_free_text`]. The
/// document itself is unchanged and still has to be freed. On any failure
/// `*out` is null and `*out_len` is 0, so nothing stale can be freed.
///
/// `end` is a `MSSQL_SQLCMD_RUN_*` value: whether the run was canceled or its
/// command line rejected; otherwise the exit code decides the outcome.
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
    end: i32,
    out: *mut *const u16,
    out_len: *mut usize,
) -> i32 {
    if out.is_null() || out_len.is_null() {
        return MSSQL_SQLCMD_NULL_ARGUMENT;
    }
    // SAFETY: both were checked non-null and are writable, per the caller;
    // the rest is forwarded from the caller's guarantees.
    unsafe {
        *out = std::ptr::null();
        *out_len = 0;
        with_document(document, |document| {
            let end = run_end_from(end).ok_or(MSSQL_SQLCMD_INVALID_ARGUMENT)?;
            let version = read_text(version).ok_or(MSSQL_SQLCMD_NULL_ARGUMENT)?;
            let connection = Connection {
                server: read_optional_text(connection.server),
                database: read_optional_text(connection.database),
                authentication: read_optional_text(connection.authentication),
                encrypt: connection.encrypt != 0,
            };
            let rendered: Box<[u16]> = document
                .render(&version, &connection, exit_code, end)
                .encode_utf16()
                .collect();
            *out_len = rendered.len();
            *out = Box::into_raw(rendered).cast::<u16>().cast_const();
            Ok(())
        })
    }
}

/// Releases text returned by [`mssql_sqlcmd_json_render`]. A null pointer is
/// ignored. Nothing unwinds across the boundary.
///
/// # Safety
/// `text` and `len` must be exactly what a render call returned, freed once.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mssql_sqlcmd_free_text(text: *const u16, len: usize) {
    if !text.is_null() {
        // SAFETY: rebuilds the boxed slice `mssql_sqlcmd_json_render` leaked;
        // it was allocated mutable, so casting back is sound.
        let text =
            unsafe { Box::from_raw(std::ptr::slice_from_raw_parts_mut(text.cast_mut(), len)) };
        let _ = catch_unwind(AssertUnwindSafe(|| drop(text)));
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

    fn column(name: &[u16], driver_type: &[u16], size: i64) -> MssqlSqlcmdColumn {
        MssqlSqlcmdColumn {
            name: text(name),
            driver_type: text(driver_type),
            size,
            precision: -1,
            scale: -1,
            nullable: -1,
        }
    }

    fn render_with(
        document: *mut MssqlSqlcmdJsonDocument,
        connection: MssqlSqlcmdConnection,
        exit_code: i32,
        end: i32,
    ) -> String {
        let version = utf16("18.5");
        let mut out = std::ptr::null();
        let mut out_len = 0;
        // SAFETY: test pointers are valid for the call.
        let status = unsafe {
            mssql_sqlcmd_json_render(
                document,
                text(&version),
                connection,
                exit_code,
                end,
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

    /// A message with only its text and the required numbers set.
    fn message(is_error: i32, text_units: &[u16]) -> MssqlSqlcmdMessage {
        MssqlSqlcmdMessage {
            is_error,
            number: 0,
            severity: 0,
            state: 1,
            line: 0,
            server: NULL_TEXT,
            procedure: NULL_TEXT,
            source: NULL_TEXT,
            sql_state: NULL_TEXT,
            text: text(text_units),
        }
    }

    fn render(document: *mut MssqlSqlcmdJsonDocument, connection: MssqlSqlcmdConnection) -> String {
        render_with(document, connection, 0, MSSQL_SQLCMD_RUN_FINISHED)
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
        let (message_text, procedure, db01, sql_state) =
            (utf16("boom"), utf16("p"), utf16("db01"), utf16("42000"));
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
                    &MssqlSqlcmdMessage {
                        number: 50000,
                        severity: 16,
                        line: 2,
                        server: text(&db01),
                        procedure: text(&procedure),
                        sql_state: text(&sql_state),
                        ..message(1, &message_text)
                    }
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
            MSSQL_SQLCMD_RUN_FINISHED,
        );
        // SAFETY: freed once.
        unsafe { mssql_sqlcmd_json_free(document) };

        for expected in [
            "\"contractVersion\": \"1.0\"",
            "\"server\": \"localhost\"",
            "\"database\": null",
            "\"encrypt\": true",
            "\"serverVersion\": \"17.00.1000\"",
            "\"connectMs\": ",
            "\"startTime\": \"",
            "\"durationMs\": ",
            "\"executionStatus\": \"completed\",\n  \"operationOutcome\": \"failed\",\n  \"exitCode\": 1",
            "\"type\": \"batch\",\n      \"index\": 1,\n      \"durationMs\": ",
            "\"text\": \"SELECT 1\"",
            "{ \"ordinal\": 0, \"name\": \"id\", \"driverType\": \"int\", \"size\": 4 }",
            "{ \"ordinal\": 1, \"name\": \"name\", \"driverType\": \"nvarchar\", \"size\": 50 }",
            "[\"1\", null],\n        [\"1\", \"é\"]",
            "\"rowsAffected\": 2",
            "\"count\": 3",
            "\"number\": 50000,\n      \"severity\": 16,\n      \"state\": 1,\n      \"server\": \"db01\",\n      \"procedure\": \"p\",\n      \"line\": 2,\n      \"sqlState\": \"42000\",\n      \"text\": \"boom\"",
        ] {
            assert!(
                rendered.contains(expected),
                "missing {expected:?} in {rendered}"
            );
        }
    }

    /// A null batch text leaves the text out; an empty or null server version,
    /// and an empty or null optional message field, or a line of 0, are absent.
    #[test]
    fn absent_optional_values_are_left_out() {
        let document = mssql_sqlcmd_json_new();
        let (empty, hi) = (utf16(""), utf16("hi"));
        // SAFETY: test pointers are valid for each call.
        unsafe {
            mssql_sqlcmd_json_connected(document, text(&empty));
            mssql_sqlcmd_json_begin_batch(document, NULL_TEXT);
            let mut entry = message(0, &hi);
            entry.server = text(&empty);
            entry.procedure = text(&empty);
            mssql_sqlcmd_json_add_message(document, &entry);
        }
        let rendered = render(document, NO_CONNECTION);
        // SAFETY: freed once.
        unsafe { mssql_sqlcmd_json_free(document) };
        let batch = rendered
            .split("\"type\": \"batch\"")
            .nth(1)
            .and_then(|rest| rest.split('}').next())
            .unwrap();
        assert!(!batch.contains("\"text\""), "batch text in {rendered}");
        let entry = rendered.split("\"type\": \"message\"").nth(1).unwrap();
        for absent in [
            "\"line\"",
            "\"procedure\"",
            "\"server\"",
            "\"source\"",
            "\"sqlState\"",
        ] {
            assert!(!entry.contains(absent), "{absent} in {rendered}");
        }
        assert!(entry.contains("\"text\": \"hi\""), "{rendered}");
        assert!(!rendered.contains("serverVersion"), "{rendered}");
        assert!(rendered.contains("\"connectMs\": "), "{rendered}");
        assert!(
            rendered.contains("\"operationOutcome\": \"succeeded\""),
            "{rendered}"
        );
    }

    /// An empty connection text or column type is unknown, as a null one is:
    /// `null` in the connection, left out of the column.
    #[test]
    fn empty_connection_texts_and_type_names_are_absent() {
        let (empty, v) = (utf16(""), utf16("v"));
        let document = mssql_sqlcmd_json_new();
        // SAFETY: test pointers are valid for the call.
        let status = unsafe {
            mssql_sqlcmd_json_begin_result_set(document, [column(&v, &empty, -1)].as_ptr(), 1)
        };
        assert_eq!(status, MSSQL_SQLCMD_OK);
        let rendered = render(
            document,
            MssqlSqlcmdConnection {
                server: text(&empty),
                database: text(&empty),
                authentication: text(&empty),
                encrypt: 0,
            },
        );
        // SAFETY: freed once.
        unsafe { mssql_sqlcmd_json_free(document) };
        for expected in [
            "\"server\": null",
            "\"database\": null",
            "\"authentication\": null",
            "{ \"ordinal\": 0, \"name\": \"v\" }",
        ] {
            assert!(
                rendered.contains(expected),
                "missing {expected:?} in {rendered}"
            );
        }
    }
    #[test]
    fn every_run_end_value_maps_to_its_end() {
        assert_eq!(
            run_end_from(MSSQL_SQLCMD_RUN_FINISHED),
            Some(RunEnd::Finished)
        );
        assert_eq!(
            run_end_from(MSSQL_SQLCMD_RUN_CANCELED),
            Some(RunEnd::Canceled)
        );
        assert_eq!(
            run_end_from(MSSQL_SQLCMD_RUN_INVALID_INVOCATION),
            Some(RunEnd::InvalidInvocation)
        );
        assert_eq!(run_end_from(3), None);
        assert_eq!(run_end_from(-1), None);
    }

    /// Each run end reaches the document's statuses through the ABI.
    #[test]
    fn the_run_end_and_exit_code_decide_the_statuses() {
        for (end, exit_code, expected) in [
            (
                MSSQL_SQLCMD_RUN_FINISHED,
                0,
                "\"completed\",\n  \"operationOutcome\": \"succeeded\"",
            ),
            (
                MSSQL_SQLCMD_RUN_FINISHED,
                1,
                "\"completed\",\n  \"operationOutcome\": \"failed\"",
            ),
            (
                MSSQL_SQLCMD_RUN_CANCELED,
                0,
                "\"canceled\",\n  \"operationOutcome\": \"canceled\"",
            ),
            (
                MSSQL_SQLCMD_RUN_INVALID_INVOCATION,
                1,
                "\"invalidInvocation\",\n  \"operationOutcome\": \"notExecuted\"",
            ),
        ] {
            let document = mssql_sqlcmd_json_new();
            let rendered = render_with(document, NO_CONNECTION, exit_code, end);
            // SAFETY: freed once.
            unsafe { mssql_sqlcmd_json_free(document) };
            assert!(
                rendered.contains(&format!("\"executionStatus\": {expected}")),
                "{rendered}"
            );
            assert!(!rendered.contains("failure"), "{rendered}");
        }
    }

    #[test]
    fn an_unknown_run_end_is_rejected_and_clears_the_outputs() {
        let document = mssql_sqlcmd_json_new();
        let version = utf16("18.5");
        let mut out = std::ptr::NonNull::<u16>::dangling().as_ptr().cast_const();
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
        let entry = message(1, &one);
        // SAFETY: null handles and pointers are what is under test.
        unsafe {
            // A null handle is reported for every entry point that takes one,
            // before any other input is read.
            assert_eq!(
                mssql_sqlcmd_json_add_row(std::ptr::null_mut(), row.as_ptr(), 1),
                MSSQL_SQLCMD_NULL_ARGUMENT
            );
            assert_eq!(
                mssql_sqlcmd_json_connecting(std::ptr::null_mut()),
                MSSQL_SQLCMD_NULL_ARGUMENT
            );
            assert_eq!(
                mssql_sqlcmd_json_connected(std::ptr::null_mut(), text(&one)),
                MSSQL_SQLCMD_NULL_ARGUMENT
            );
            assert_eq!(
                mssql_sqlcmd_json_begin_batch(std::ptr::null_mut(), text(&one)),
                MSSQL_SQLCMD_NULL_ARGUMENT
            );
            assert_eq!(
                mssql_sqlcmd_json_end_batch(std::ptr::null_mut()),
                MSSQL_SQLCMD_NULL_ARGUMENT
            );
            assert_eq!(
                mssql_sqlcmd_json_begin_result_set(std::ptr::null_mut(), std::ptr::null(), 0),
                MSSQL_SQLCMD_NULL_ARGUMENT
            );
            assert_eq!(
                mssql_sqlcmd_json_add_message(std::ptr::null_mut(), &entry),
                MSSQL_SQLCMD_NULL_ARGUMENT
            );
            let document = mssql_sqlcmd_json_new();
            assert_eq!(
                mssql_sqlcmd_json_begin_result_set(document, std::ptr::null(), 1),
                MSSQL_SQLCMD_NULL_ARGUMENT
            );
            assert_eq!(
                mssql_sqlcmd_json_begin_result_set(document, std::ptr::null(), 0),
                MSSQL_SQLCMD_OK,
                "an empty column list may be null"
            );
            let bare = MssqlSqlcmdColumn {
                name: NULL_TEXT,
                driver_type: NULL_TEXT,
                ..column(&one, &one, -1)
            };
            assert_eq!(
                mssql_sqlcmd_json_begin_result_set(document, [bare].as_ptr(), 1),
                MSSQL_SQLCMD_OK,
                "a column needs neither a name nor any metadata"
            );
            assert_eq!(
                mssql_sqlcmd_json_add_message(document, std::ptr::null()),
                MSSQL_SQLCMD_NULL_ARGUMENT
            );
            assert_eq!(
                mssql_sqlcmd_json_add_message(document, &message(1, &[])),
                MSSQL_SQLCMD_OK,
                "an empty text is a text"
            );
            let no_text = MssqlSqlcmdMessage {
                text: NULL_TEXT,
                ..entry
            };
            assert_eq!(
                mssql_sqlcmd_json_add_message(document, &no_text),
                MSSQL_SQLCMD_NULL_ARGUMENT
            );
            assert_eq!(
                mssql_sqlcmd_json_render(
                    document,
                    text(&one),
                    NO_CONNECTION,
                    0,
                    MSSQL_SQLCMD_RUN_FINISHED,
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                ),
                MSSQL_SQLCMD_NULL_ARGUMENT
            );
            mssql_sqlcmd_json_free(document);
            mssql_sqlcmd_json_free(std::ptr::null_mut());
            mssql_sqlcmd_free_text(std::ptr::null(), 0);
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
            with_document(document, |_| -> Result<(), i32> {
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
        let mut out = std::ptr::NonNull::<u16>::dangling().as_ptr().cast_const();
        let mut out_len = 42;
        // SAFETY: a null document is reported before anything is read.
        let status = unsafe {
            mssql_sqlcmd_json_render(
                std::ptr::null_mut(),
                text(&version),
                NO_CONNECTION,
                0,
                MSSQL_SQLCMD_RUN_FINISHED,
                &mut out,
                &mut out_len,
            )
        };
        assert_eq!(status, MSSQL_SQLCMD_NULL_ARGUMENT);
        assert!(out.is_null());
        assert_eq!(out_len, 0);
    }
}
