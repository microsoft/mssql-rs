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
//! the boundary and reported as [`MSSQL_SQLCMD_INTERNAL_ERROR`]. The host
//! process's panic hook still runs first, so the panic message goes to stderr;
//! stdout is never written to.
//!
//! The document times the run itself: [`mssql_sqlcmd_json_new`] starts the
//! clock, so native sqlcmd creates it at startup, before it connects.
//!
//! The header for C and C++ callers is `include/mssql_sqlcmd.h`.

use std::panic::{AssertUnwindSafe, catch_unwind};

#[cfg(target_pointer_width = "64")]
use crate::diagnostics::{self, Authentication, Depth, Encrypt, Request};
use crate::formatter::json::{Column, Connection, JsonDocument, Message, RunEnd};
use crate::i18n;

/// The call succeeded.
pub const MSSQL_SQLCMD_OK: i32 = 0;
/// A required pointer was null.
pub const MSSQL_SQLCMD_NULL_ARGUMENT: i32 = 1;
/// The call does not fit the document's state, e.g. a row before any result
/// set, a result set started while another is open, or a row whose value count
/// differs from the column count.
pub const MSSQL_SQLCMD_INVALID_STATE: i32 = 2;
/// A Rust panic was caught at the boundary.
pub const MSSQL_SQLCMD_INTERNAL_ERROR: i32 = 3;
/// An argument is out of its range, e.g. an unknown `MSSQL_SQLCMD_RUN_*` value.
pub const MSSQL_SQLCMD_INVALID_ARGUMENT: i32 = 4;
/// This build does not have the feature: diagnostics in a 32-bit build.
pub const MSSQL_SQLCMD_UNSUPPORTED: i32 = 5;

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

/// Sets the locale used by Rust-generated human text.
///
/// Native sqlcmd calls this once at startup with the language it resolved for
/// SQLCMD.rll. On Windows that may be an LCID as decimal text, for example
/// `1031`. If this is never called, the library resolves the locale from the
/// environment and, on Windows, the user default UI language.
/// A null pointer with zero length resets the locale to the English fallback
/// and returns [`MSSQL_SQLCMD_INVALID_ARGUMENT`] because an empty locale is not
/// recognized.
///
/// # Safety
/// A non-null `locale.data` must point to `locale.len` readable `u16` values.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mssql_sqlcmd_set_locale(locale: MssqlSqlcmdText) -> i32 {
    catch_unwind(AssertUnwindSafe(|| {
        if locale.data.is_null() {
            if locale.len != 0 {
                return MSSQL_SQLCMD_NULL_ARGUMENT;
            }
            i18n::set_locale("");
            return MSSQL_SQLCMD_INVALID_ARGUMENT;
        }
        // SAFETY: forwarded from the caller's guarantees.
        let Some(locale) = (unsafe { read_text(locale) }) else {
            return MSSQL_SQLCMD_NULL_ARGUMENT;
        };
        if i18n::set_locale(&locale) {
            MSSQL_SQLCMD_OK
        } else {
            MSSQL_SQLCMD_INVALID_ARGUMENT
        }
    }))
    .unwrap_or(MSSQL_SQLCMD_INTERNAL_ERROR)
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
    /// The length in UTF-16 code units, not bytes.
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
/// before `body` reads any other input (render alone first clears its outputs;
/// see [`mssql_sqlcmd_json_render`]); `body` reads the inputs and applies
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

/// Starts a batch sent to the server. `text` is the batch as sent, or null or
/// empty to leave it out. A batch still running is ended first, and a result
/// set still open is closed without a count, so the new batch's rows cannot
/// land in it. That close is not reported here; a following
/// [`mssql_sqlcmd_json_add_row`] or [`mssql_sqlcmd_json_end_result_set`]
/// returns [`MSSQL_SQLCMD_INVALID_STATE`].
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
            document.begin_batch(read_optional_text(text));
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

/// Starts a result set with `column_count` columns. The previous result set must
/// have been ended: starting one while another is open is
/// [`MSSQL_SQLCMD_INVALID_STATE`], and closes the open one without a count, so
/// no later row can land in it.
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
            document
                .begin_result_set(columns)
                .map_err(|_| MSSQL_SQLCMD_INVALID_STATE)
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
/// its length in UTF-16 code units (not bytes) in `*out_len`. Release it with
/// [`mssql_sqlcmd_free_text`], passing that same length back. The
/// document itself is unchanged and still has to be freed. On any failure
/// `*out` is null and `*out_len` is 0, so nothing stale can be freed.
///
/// Unlike the other calls, `out` and `out_len` are cleared first, each one that
/// is non-null, before the handle is checked: that is what keeps a failure from
/// leaving them stale, a null handle or the other output being null included.
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
    // Each output is cleared on its own, so one null cannot leave the other stale.
    // SAFETY: each is written only when non-null; the caller guarantees it is
    // then writable.
    unsafe {
        if !out.is_null() {
            *out = std::ptr::null();
        }
        if !out_len.is_null() {
            *out_len = 0;
        }
    }
    if out.is_null() || out_len.is_null() {
        return MSSQL_SQLCMD_NULL_ARGUMENT;
    }
    // SAFETY: both were checked non-null and are writable, per the caller;
    // the rest is forwarded from the caller's guarantees.
    unsafe {
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
                .map_err(|_| MSSQL_SQLCMD_INTERNAL_ERROR)?
                .encode_utf16()
                .collect();
            *out_len = rendered.len();
            *out = Box::into_raw(rendered).cast::<u16>().cast_const();
            Ok(())
        })
    }
}

/// Releases text returned by [`mssql_sqlcmd_json_render`] or
/// [`mssql_sqlcmd_diagnostics_run`]. A null pointer is ignored. Nothing unwinds
/// across the boundary.
///
/// # Safety
/// `text` and `len` must be exactly what a render call returned (`len` in
/// UTF-16 code units, not bytes), freed once.
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

/// `authentication` of [`MssqlSqlcmdDiagnosticsRequest`]: SQL Server
/// authentication with `user` and `password`.
pub const MSSQL_SQLCMD_AUTH_SQL_PASSWORD: i32 = 0;
/// Windows authentication (Kerberos off Windows); `user` and `password` are
/// ignored.
pub const MSSQL_SQLCMD_AUTH_INTEGRATED: i32 = 1;

/// `encrypt` of [`MssqlSqlcmdDiagnosticsRequest`], as sqlcmd's `-N` sets it.
pub const MSSQL_SQLCMD_ENCRYPT_OPTIONAL: i32 = 0;
pub const MSSQL_SQLCMD_ENCRYPT_MANDATORY: i32 = 1;
pub const MSSQL_SQLCMD_ENCRYPT_STRICT: i32 = 2;

/// `format` of [`mssql_sqlcmd_diagnostics_run`].
pub const MSSQL_SQLCMD_REPORT_TEXT: i32 = 0;
pub const MSSQL_SQLCMD_REPORT_JSON: i32 = 1;

/// `depth` of [`MssqlSqlcmdDiagnosticsRequest`]: how far the diagnosis goes.
/// Each depth includes the ones before it.
pub const MSSQL_SQLCMD_DEPTH_DEFAULT: i32 = -1;
pub const MSSQL_SQLCMD_DEPTH_CONNECTION_INPUT: i32 = 0;
pub const MSSQL_SQLCMD_DEPTH_ENDPOINT_RESOLUTION: i32 = 1;
pub const MSSQL_SQLCMD_DEPTH_NETWORK_REACHABILITY: i32 = 2;
pub const MSSQL_SQLCMD_DEPTH_CONNECTION_ATTEMPT: i32 = 3;
pub const MSSQL_SQLCMD_DEPTH_SESSION_VALIDATION: i32 = 4;

/// What `sqlcmd diagnose` checks.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct MssqlSqlcmdDiagnosticsRequest {
    /// As given to `-S`. Required.
    pub server: MssqlSqlcmdText,
    /// Null or empty: the login's default database.
    pub database: MssqlSqlcmdText,
    /// An `MSSQL_SQLCMD_AUTH_*` value.
    pub authentication: i32,
    pub user: MssqlSqlcmdText,
    pub password: MssqlSqlcmdText,
    /// An `MSSQL_SQLCMD_ENCRYPT_*` value.
    pub encrypt: i32,
    /// Non-zero for `-C`.
    pub trust_server_certificate: i32,
    /// `-F`; null or empty when not given.
    pub host_name_in_certificate: MssqlSqlcmdText,
    /// `-l`, in seconds; 0 or less for the default.
    pub login_timeout_seconds: i32,
    /// An `MSSQL_SQLCMD_DEPTH_*` value; [`MSSQL_SQLCMD_DEPTH_DEFAULT`] is session
    /// validation, the deepest.
    pub depth: i32,
    /// Non-zero to show identifiers as they are instead of share-safe labels.
    pub local_detail: i32,
    /// Why sqlcmd rejected the request; null or empty when it is valid. A
    /// rejected request runs nothing and reports itself as an invalid
    /// invocation (`server` may then be empty).
    pub invalid_reason: MssqlSqlcmdText,
}

/// Diagnoses the connection `request` describes, up to its depth, and renders
/// the report as text or JSON (`format`, an `MSSQL_SQLCMD_REPORT_*` value) into
/// `*out` / `*out_len`, as UTF-16; release it with [`mssql_sqlcmd_free_text`].
/// `*exit_code` is sqlcmd's exit code for the outcome: 0 passed, 1 issue
/// detected, 2 inconclusive or not evaluated, 3 partial, 4 canceled, 5 internal
/// failure, 6 invalid invocation. Opens at most one connection, and runs only a
/// minimal query on it (session validation); blocks until the diagnosis ends.
/// `version` is sqlcmd's, for the JSON report.
///
/// A failed connection is a report, not an error: the call returns
/// [`MSSQL_SQLCMD_OK`]. On an error `*out` is null, `*out_len` 0 and
/// `*exit_code` 5.
///
/// # Safety
/// `request`, `out`, `out_len` and `exit_code` must be null or valid; the
/// texts in `request` and `version` follow [`MssqlSqlcmdText`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mssql_sqlcmd_diagnostics_run(
    request: *const MssqlSqlcmdDiagnosticsRequest,
    version: MssqlSqlcmdText,
    format: i32,
    out: *mut *const u16,
    out_len: *mut usize,
    exit_code: *mut i32,
) -> i32 {
    // Each output is cleared on its own, so one null cannot leave another stale.
    // SAFETY: each is written only when non-null, and is then writable, per the caller.
    unsafe {
        if !out.is_null() {
            *out = std::ptr::null();
        }
        if !out_len.is_null() {
            *out_len = 0;
        }
        if !exit_code.is_null() {
            *exit_code = 5;
        }
    }
    if out.is_null() || out_len.is_null() || exit_code.is_null() {
        return MSSQL_SQLCMD_NULL_ARGUMENT;
    }
    if request.is_null() {
        return MSSQL_SQLCMD_NULL_ARGUMENT;
    }
    // SAFETY: a non-null request is valid, per the caller.
    let request = unsafe { *request };
    let valid = matches!(
        request.authentication,
        MSSQL_SQLCMD_AUTH_SQL_PASSWORD | MSSQL_SQLCMD_AUTH_INTEGRATED
    ) && matches!(
        request.encrypt,
        MSSQL_SQLCMD_ENCRYPT_OPTIONAL
            | MSSQL_SQLCMD_ENCRYPT_MANDATORY
            | MSSQL_SQLCMD_ENCRYPT_STRICT
    ) && matches!(format, MSSQL_SQLCMD_REPORT_TEXT | MSSQL_SQLCMD_REPORT_JSON)
        && (MSSQL_SQLCMD_DEPTH_DEFAULT..=MSSQL_SQLCMD_DEPTH_SESSION_VALIDATION)
            .contains(&request.depth);
    if !valid {
        return MSSQL_SQLCMD_INVALID_ARGUMENT;
    }
    // SAFETY: the texts follow the caller's guarantees.
    let input = unsafe {
        let invalid_reason = read_optional_text(request.invalid_reason);
        let server = read_optional_text(request.server);
        let Some(server) = server.or_else(|| invalid_reason.as_ref().map(|_| String::new())) else {
            return MSSQL_SQLCMD_NULL_ARGUMENT;
        };
        let Some(version) = read_text(version) else {
            return MSSQL_SQLCMD_NULL_ARGUMENT;
        };
        DiagnosticsInput {
            server,
            database: read_optional_text(request.database),
            integrated: request.authentication == MSSQL_SQLCMD_AUTH_INTEGRATED,
            user: read_text(request.user).unwrap_or_default(),
            password: read_text(request.password).unwrap_or_default(),
            encrypt: request.encrypt,
            trust_server_certificate: request.trust_server_certificate != 0,
            host_name_in_certificate: read_optional_text(request.host_name_in_certificate),
            login_timeout_seconds: u32::try_from(request.login_timeout_seconds).unwrap_or(0),
            depth: request.depth,
            local_detail: request.local_detail != 0,
            invalid_reason,
            version,
            json: format == MSSQL_SQLCMD_REPORT_JSON,
        }
    };

    let Ok(rendered) = catch_unwind(AssertUnwindSafe(|| diagnose(input))) else {
        return MSSQL_SQLCMD_INTERNAL_ERROR;
    };
    let Some((text, code)) = rendered else {
        return MSSQL_SQLCMD_UNSUPPORTED;
    };
    let rendered: Box<[u16]> = text.encode_utf16().collect();
    // SAFETY: checked non-null and writable above.
    unsafe {
        *out_len = rendered.len();
        *out = Box::into_raw(rendered).cast::<u16>().cast_const();
        *exit_code = code;
    }
    MSSQL_SQLCMD_OK
}

/// A diagnostics request read from its C form, with the arguments already
/// checked.
#[cfg_attr(not(target_pointer_width = "64"), allow(dead_code))]
struct DiagnosticsInput {
    server: String,
    database: Option<String>,
    integrated: bool,
    user: String,
    password: String,
    encrypt: i32,
    trust_server_certificate: bool,
    host_name_in_certificate: Option<String>,
    login_timeout_seconds: u32,
    depth: i32,
    local_detail: bool,
    invalid_reason: Option<String>,
    version: String,
    json: bool,
}

/// Runs the diagnosis and renders the report, with sqlcmd's exit code.
#[cfg(target_pointer_width = "64")]
fn diagnose(input: DiagnosticsInput) -> Option<(String, i32)> {
    let request = Request {
        server: input.server,
        database: input.database,
        authentication: if input.integrated {
            Authentication::Integrated
        } else {
            Authentication::SqlPassword {
                user: input.user,
                password: input.password,
            }
        },
        encrypt: match input.encrypt {
            MSSQL_SQLCMD_ENCRYPT_OPTIONAL => Encrypt::Optional,
            MSSQL_SQLCMD_ENCRYPT_STRICT => Encrypt::Strict,
            _ => Encrypt::Mandatory,
        },
        trust_server_certificate: input.trust_server_certificate,
        host_name_in_certificate: input.host_name_in_certificate,
        login_timeout_seconds: input.login_timeout_seconds,
        depth: match input.depth {
            MSSQL_SQLCMD_DEPTH_CONNECTION_INPUT => Depth::ConnectionInput,
            MSSQL_SQLCMD_DEPTH_ENDPOINT_RESOLUTION => Depth::EndpointResolution,
            MSSQL_SQLCMD_DEPTH_NETWORK_REACHABILITY => Depth::NetworkReachability,
            MSSQL_SQLCMD_DEPTH_CONNECTION_ATTEMPT => Depth::ConnectionAttempt,
            _ => Depth::SessionValidation,
        },
        depth_selected: input.depth != MSSQL_SQLCMD_DEPTH_DEFAULT,
        local_detail: input.local_detail,
        invalid: input.invalid_reason,
    };
    let report = diagnostics::run(request);
    let text = if input.json {
        // These view types cannot fail to serialize; if they did, the FFI
        // boundary reports the panic as an internal error.
        diagnostics::report::json(&report, &input.version).expect("a report always serializes")
    } else {
        diagnostics::report::text(&report)
    };
    Some((text, report.exit_code()))
}

/// 32-bit builds have no diagnose: `mssql-tds` is 64-bit only.
#[cfg(not(target_pointer_width = "64"))]
fn diagnose(_input: DiagnosticsInput) -> Option<(String, i32)> {
    None
}
/// Cancels the diagnosis [`mssql_sqlcmd_diagnostics_run`] is running: it
/// returns soon after with the checks that finished, the others skipped as
/// canceled, and exit code 4. Callable from any thread, a console control or
/// signal handler included (it only updates an atomic, without locking).
/// Does nothing when no diagnosis is running, and never cancels a later one. 32-bit builds return
/// [`MSSQL_SQLCMD_UNSUPPORTED`].
#[unsafe(no_mangle)]
pub extern "C" fn mssql_sqlcmd_diagnostics_cancel() -> i32 {
    #[cfg(target_pointer_width = "64")]
    {
        crate::diagnostics::cancel();
        MSSQL_SQLCMD_OK
    }
    #[cfg(not(target_pointer_width = "64"))]
    {
        MSSQL_SQLCMD_UNSUPPORTED
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
        // Both nullability answers the driver gives, so each mapping is pinned.
        let columns = [
            MssqlSqlcmdColumn {
                nullable: 0,
                ..column(&id, &int, 4)
            },
            MssqlSqlcmdColumn {
                nullable: 1,
                ..column(&name, &nvarchar, 50)
            },
        ];
        let empty = utf16("");
        let row = [text(&one), NULL_TEXT];
        let row2 = [text(&one), text(&a)];
        // An empty row value is the empty string, unlike other empty texts.
        let row3 = [text(&empty), NULL_TEXT];
        let (message_text, procedure, db01, sql_state) =
            (utf16("boom"), utf16("p"), utf16("db01"), utf16("42000"));
        let (server, server_version) = (utf16("localhost"), utf16("17.00.1000"));
        let (database, authentication) = (utf16("master"), utf16("SqlPassword"));
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
                mssql_sqlcmd_json_add_row(document, row3.as_ptr(), 2),
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
                database: text(&database),
                authentication: text(&authentication),
                encrypt: 1,
            },
            1,
            MSSQL_SQLCMD_RUN_FINISHED,
        );
        // SAFETY: freed once.
        unsafe { mssql_sqlcmd_json_free(document) };

        for expected in [
            "\"contractVersion\": \"1.0\"",
            "\"server\": \"localhost\"",
            "\"database\": \"master\"",
            "\"authentication\": \"SqlPassword\"",
            "\"encrypt\": true",
            "\"serverVersion\": \"17.00.1000\"",
            "\"connectMs\": ",
            "\"startTime\": \"",
            "\"durationMs\": ",
            "\"executionStatus\": \"completed\",\n  \"operationOutcome\": \"failed\",\n  \"exitCode\": 1",
            "\"type\": \"batch\",\n      \"index\": 1,\n      \"durationMs\": ",
            "\"text\": \"SELECT 1\"",
            "\"rowsAffected\": 2",
            "\"count\": 3",
            "\"number\": 50000,\n      \"severity\": 16,\n      \"state\": 1,\n      \"server\": \"db01\",\n      \"procedure\": \"p\",\n      \"line\": 2,\n      \"sqlState\": \"42000\",\n      \"text\": \"boom\"",
        ] {
            assert!(
                rendered.contains(expected),
                "missing {expected:?} in {rendered}"
            );
        }
        let document: serde_json::Value = serde_json::from_str(&rendered).unwrap();
        let result_set = &document["output"][1];
        assert_eq!(
            result_set["columns"],
            serde_json::json!([
                { "ordinal": 0, "name": "id", "driverType": "int", "size": 4, "nullable": false },
                { "ordinal": 1, "name": "name", "driverType": "nvarchar", "size": 50, "nullable": true }
            ]),
            "{rendered}"
        );
        assert_eq!(
            result_set["rows"],
            serde_json::json!([["1", null], ["1", "é"], ["", null]]),
            "{rendered}"
        );
    }

    /// A nullability the driver does not know leaves `nullable` out; precision
    /// and scale cross as given, a scale of 0 included.
    #[test]
    fn column_details_cross_as_the_driver_gives_them() {
        let document = mssql_sqlcmd_json_new();
        let (id, int, amount, decimal) =
            (utf16("id"), utf16("int"), utf16("amount"), utf16("decimal"));
        let columns = [
            column(&id, &int, 4),
            MssqlSqlcmdColumn {
                precision: 10,
                scale: 0,
                ..column(&amount, &decimal, 10)
            },
        ];
        // SAFETY: test pointers are valid for the call.
        let status = unsafe { mssql_sqlcmd_json_begin_result_set(document, columns.as_ptr(), 2) };
        assert_eq!(status, MSSQL_SQLCMD_OK);
        let rendered = render(document, NO_CONNECTION);
        // SAFETY: freed once.
        unsafe { mssql_sqlcmd_json_free(document) };
        let parsed: serde_json::Value = serde_json::from_str(&rendered).unwrap();
        assert_eq!(
            parsed["output"][0]["columns"],
            serde_json::json!([
                { "ordinal": 0, "name": "id", "driverType": "int", "size": 4 },
                {
                    "ordinal": 1, "name": "amount", "driverType": "decimal", "size": 10,
                    "precision": 10, "scale": 0
                }
            ]),
            "{rendered}"
        );
    }

    /// A null or empty batch text leaves the text out; an empty or null server version,
    /// and an empty or null optional message field, or a line of 0, are absent.
    #[test]
    fn absent_optional_values_are_left_out() {
        let document = mssql_sqlcmd_json_new();
        let (empty, hi) = (utf16(""), utf16("hi"));
        // SAFETY: test pointers are valid for each call.
        unsafe {
            mssql_sqlcmd_json_connected(document, text(&empty));
            mssql_sqlcmd_json_begin_batch(document, NULL_TEXT);
            mssql_sqlcmd_json_begin_batch(document, text(&empty));
            let mut entry = message(0, &hi);
            entry.server = text(&empty);
            entry.procedure = text(&empty);
            mssql_sqlcmd_json_add_message(document, &entry);
        }
        let rendered = render(document, NO_CONNECTION);
        // SAFETY: freed once.
        unsafe { mssql_sqlcmd_json_free(document) };
        let parsed: serde_json::Value = serde_json::from_str(&rendered).unwrap();
        let batches: Vec<_> = parsed["output"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|entry| entry["type"] == "batch")
            .collect();
        assert_eq!(batches.len(), 2, "{rendered}");
        for batch in batches {
            assert!(batch.get("text").is_none(), "batch text in {rendered}");
        }
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
        ] {
            assert!(
                rendered.contains(expected),
                "missing {expected:?} in {rendered}"
            );
        }
        let document: serde_json::Value = serde_json::from_str(&rendered).unwrap();
        assert_eq!(
            document["output"][0]["columns"],
            serde_json::json!([{ "ordinal": 0, "name": "v" }]),
            "{rendered}"
        );
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
                mssql_sqlcmd_json_end_result_set(std::ptr::null_mut(), 0, 0),
                MSSQL_SQLCMD_NULL_ARGUMENT
            );
            assert_eq!(
                mssql_sqlcmd_json_add_rows_affected(std::ptr::null_mut(), 1),
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
            assert_eq!(
                mssql_sqlcmd_json_end_result_set(document, 0, 0),
                MSSQL_SQLCMD_OK
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
            assert_eq!(
                mssql_sqlcmd_json_begin_result_set(document, columns.as_ptr(), 2),
                MSSQL_SQLCMD_INVALID_STATE,
                "a result set started while one is open"
            );
            assert_eq!(
                mssql_sqlcmd_json_add_row(document, [text(&one), text(&one)].as_ptr(), 2),
                MSSQL_SQLCMD_INVALID_STATE,
                "the rejected call closed the open set, so a matching row is not put in it"
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
        let document: serde_json::Value = serde_json::from_str(&rendered).unwrap();
        assert_eq!(
            document["output"][0]["rows"],
            serde_json::json!([["\u{FFFD}"]]),
            "{rendered}"
        );
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
    /// an error path that frees them anyway frees nothing. That holds for a
    /// null handle too: render clears its outputs before checking the handle.
    #[test]
    fn a_failed_render_clears_the_outputs() {
        let version = utf16("18.5");
        let mut out = std::ptr::NonNull::<u16>::dangling().as_ptr().cast_const();
        let mut out_len = 42;
        // SAFETY: the outputs are valid for writes; the null document is
        // reported before any other input is read.
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

    /// With one output null, the other is still cleared.
    #[test]
    fn a_null_output_still_clears_the_other() {
        let document = mssql_sqlcmd_json_new();
        let version = utf16("18.5");
        let mut out = std::ptr::NonNull::<u16>::dangling().as_ptr().cast_const();
        let mut out_len = 42;
        // SAFETY: the non-null output is valid for writes; the null one is
        // reported, not written.
        let (out_only, len_only) = unsafe {
            let out_only = mssql_sqlcmd_json_render(
                document,
                text(&version),
                NO_CONNECTION,
                0,
                MSSQL_SQLCMD_RUN_FINISHED,
                &mut out,
                std::ptr::null_mut(),
            );
            let len_only = mssql_sqlcmd_json_render(
                document,
                text(&version),
                NO_CONNECTION,
                0,
                MSSQL_SQLCMD_RUN_FINISHED,
                std::ptr::null_mut(),
                &mut out_len,
            );
            mssql_sqlcmd_json_free(document);
            (out_only, len_only)
        };
        assert_eq!(out_only, MSSQL_SQLCMD_NULL_ARGUMENT);
        assert_eq!(len_only, MSSQL_SQLCMD_NULL_ARGUMENT);
        assert!(out.is_null());
        assert_eq!(out_len, 0);
    }

    fn diagnostics_request(
        server: &[u16],
        user: &[u16],
        password: &[u16],
    ) -> MssqlSqlcmdDiagnosticsRequest {
        MssqlSqlcmdDiagnosticsRequest {
            server: text(server),
            database: NULL_TEXT,
            authentication: MSSQL_SQLCMD_AUTH_SQL_PASSWORD,
            user: text(user),
            password: text(password),
            encrypt: MSSQL_SQLCMD_ENCRYPT_MANDATORY,
            trust_server_certificate: 1,
            host_name_in_certificate: NULL_TEXT,
            login_timeout_seconds: 5,
            depth: MSSQL_SQLCMD_DEPTH_DEFAULT,
            local_detail: 0,
            invalid_reason: NULL_TEXT,
        }
    }

    fn run_diagnostics(request: &MssqlSqlcmdDiagnosticsRequest, format: i32) -> (i32, String, i32) {
        let version = utf16("18.7.0001.1");
        let mut out = std::ptr::null();
        let mut out_len = 0;
        let mut exit_code = -1;
        // SAFETY: every pointer is valid for the call.
        let status = unsafe {
            mssql_sqlcmd_diagnostics_run(
                request,
                text(&version),
                format,
                &mut out,
                &mut out_len,
                &mut exit_code,
            )
        };
        let rendered = if out.is_null() {
            String::new()
        } else {
            // SAFETY: the library returned `out_len` values at `out`, freed once here.
            unsafe {
                let rendered = String::from_utf16_lossy(std::slice::from_raw_parts(out, out_len));
                mssql_sqlcmd_free_text(out, out_len);
                rendered
            }
        };
        (status, rendered, exit_code)
    }

    #[cfg(target_pointer_width = "64")]
    #[test]
    fn diagnose_of_a_refused_connection_renders_as_text_and_json() {
        let (server, user, password) = (utf16("tcp:127.0.0.1,1"), utf16("sa"), utf16("not-shown"));
        let request = diagnostics_request(&server, &user, &password);

        let (status, rendered, exit_code) = run_diagnostics(&request, MSSQL_SQLCMD_REPORT_TEXT);
        assert_eq!(status, MSSQL_SQLCMD_OK);
        assert_eq!(exit_code, 1, "issue detected");
        assert!(
            rendered.starts_with("sqlcmd diagnose: server-1\n"),
            "{rendered}"
        );
        assert!(
            rendered.contains("TCP connect           DIAGNOSED"),
            "{rendered}"
        );
        assert!(!rendered.contains("not-shown"));
        assert!(!rendered.contains("127.0.0.1"), "share-safe by default");

        let (status, rendered, exit_code) = run_diagnostics(&request, MSSQL_SQLCMD_REPORT_JSON);
        assert_eq!(status, MSSQL_SQLCMD_OK);
        assert_eq!(exit_code, 1);
        assert!(rendered.contains("\"command\": \"diagnose\""), "{rendered}");
        assert!(
            rendered.contains("\"diagnosticOutcome\": \"issueDetected\""),
            "{rendered}"
        );
        assert!(rendered.contains("\"exitCode\": 1"), "{rendered}");
        assert!(!rendered.contains("not-shown"));
    }

    #[cfg(target_pointer_width = "64")]
    #[test]
    fn diagnose_honours_depth_local_detail_and_an_invalid_request() {
        let (server, user, password) = (utf16("tcp:127.0.0.1,1"), utf16("sa"), utf16("x"));
        let mut request = diagnostics_request(&server, &user, &password);
        request.depth = MSSQL_SQLCMD_DEPTH_CONNECTION_INPUT;
        request.local_detail = 1;
        let (status, rendered, exit_code) = run_diagnostics(&request, MSSQL_SQLCMD_REPORT_JSON);
        assert_eq!(status, MSSQL_SQLCMD_OK);
        assert_eq!(exit_code, 0, "parsing alone passes");
        assert!(
            rendered.contains("\"requestedDepth\": \"connectionInput\""),
            "{rendered}"
        );
        assert!(rendered.contains("\"shareSafe\": false"), "{rendered}");
        assert!(rendered.contains("\"host\": \"127.0.0.1\""), "{rendered}");
        assert!(!rendered.contains("tcpConnect"), "{rendered}");

        let reason = utf16("-Q cannot be used");
        let mut invalid = diagnostics_request(&[], &user, &password);
        invalid.server = NULL_TEXT;
        invalid.invalid_reason = text(&reason);
        let (status, rendered, exit_code) = run_diagnostics(&invalid, MSSQL_SQLCMD_REPORT_JSON);
        assert_eq!(status, MSSQL_SQLCMD_OK);
        assert_eq!(exit_code, 6);
        assert!(
            rendered.contains("\"executionStatus\": \"invalidInvocation\""),
            "{rendered}"
        );
    }

    #[cfg(target_pointer_width = "64")]
    #[test]
    fn diagnose_invalid_target_renders_in_the_selected_locale() {
        let pseudo = utf16("qps-ploc");
        // SAFETY: test text points to readable UTF-16 data.
        assert_eq!(
            unsafe { mssql_sqlcmd_set_locale(text(&pseudo)) },
            MSSQL_SQLCMD_OK
        );

        let (server, user, password) = (utf16("tcp:127.0.0.1,0"), utf16("sa"), utf16("x"));
        let mut request = diagnostics_request(&server, &user, &password);
        request.depth = MSSQL_SQLCMD_DEPTH_CONNECTION_INPUT;
        let (status, rendered, exit_code) = run_diagnostics(&request, MSSQL_SQLCMD_REPORT_JSON);
        assert_eq!(status, MSSQL_SQLCMD_OK);
        assert_eq!(exit_code, 1);
        assert!(!rendered.is_empty());
        assert!(
            !rendered.contains("is not a port number from 1 to 65535"),
            "{rendered}"
        );
        let value: serde_json::Value = serde_json::from_str(&rendered).unwrap();
        assert!(
            value["findings"][0]["text"]
                .as_str()
                .unwrap()
                .starts_with("[!!!"),
            "{rendered}"
        );
        assert!(
            value["errors"][0]["message"]
                .as_str()
                .unwrap()
                .starts_with("[!!!"),
            "{rendered}"
        );
        assert!(crate::i18n::set_locale("en-US"));
    }

    #[cfg(not(target_pointer_width = "64"))]
    #[test]
    fn diagnostics_are_unsupported_in_a_32_bit_build() {
        let (server, user, password) = (utf16("tcp:127.0.0.1,1"), utf16("sa"), utf16("x"));
        let request = diagnostics_request(&server, &user, &password);
        let (status, rendered, exit_code) = run_diagnostics(&request, MSSQL_SQLCMD_REPORT_TEXT);
        assert_eq!(status, MSSQL_SQLCMD_UNSUPPORTED);
        assert_eq!(rendered, "");
        assert_eq!(exit_code, 5);
    }

    #[test]
    fn diagnostics_reject_bad_arguments_before_connecting() {
        let (server, user, password) = (utf16("tcp:127.0.0.1,1"), utf16("sa"), utf16("x"));
        let empty: [u16; 0] = [];
        let mut cases = Vec::new();
        let mut bad = diagnostics_request(&server, &user, &password);
        bad.authentication = 7;
        cases.push((bad, MSSQL_SQLCMD_REPORT_TEXT, MSSQL_SQLCMD_INVALID_ARGUMENT));
        let mut bad = diagnostics_request(&server, &user, &password);
        bad.encrypt = -1;
        cases.push((bad, MSSQL_SQLCMD_REPORT_TEXT, MSSQL_SQLCMD_INVALID_ARGUMENT));
        let good = diagnostics_request(&server, &user, &password);
        cases.push((good, 9, MSSQL_SQLCMD_INVALID_ARGUMENT));
        let mut bad = diagnostics_request(&server, &user, &password);
        bad.depth = 5;
        cases.push((bad, MSSQL_SQLCMD_REPORT_TEXT, MSSQL_SQLCMD_INVALID_ARGUMENT));
        let mut bad = diagnostics_request(&server, &user, &password);
        bad.server = text(&empty);
        cases.push((bad, MSSQL_SQLCMD_REPORT_TEXT, MSSQL_SQLCMD_NULL_ARGUMENT));
        let mut bad = diagnostics_request(&server, &user, &password);
        bad.server = NULL_TEXT;
        cases.push((bad, MSSQL_SQLCMD_REPORT_TEXT, MSSQL_SQLCMD_NULL_ARGUMENT));
        for (request, format, expected) in cases {
            let (status, rendered, exit_code) = run_diagnostics(&request, format);
            assert_eq!(status, expected);
            assert_eq!(rendered, "");
            assert_eq!(exit_code, 5);
        }

        let version = utf16("v");
        let request = diagnostics_request(&server, &user, &password);
        let (mut out, mut out_len, mut exit_code) = (std::ptr::null(), 0, 0);
        // SAFETY: null pointers are what is being tested; the rest are valid.
        unsafe {
            assert_eq!(
                mssql_sqlcmd_diagnostics_run(
                    std::ptr::null(),
                    text(&version),
                    0,
                    &mut out,
                    &mut out_len,
                    &mut exit_code
                ),
                MSSQL_SQLCMD_NULL_ARGUMENT
            );
            assert_eq!(exit_code, 5);
            // One null output: the others are still cleared.
            (out_len, exit_code) = (42, 0);
            assert_eq!(
                mssql_sqlcmd_diagnostics_run(
                    &request,
                    text(&version),
                    0,
                    std::ptr::null_mut(),
                    &mut out_len,
                    &mut exit_code
                ),
                MSSQL_SQLCMD_NULL_ARGUMENT
            );
            assert_eq!((out_len, exit_code), (0, 5));
            let stale = std::ptr::NonNull::<u16>::dangling().as_ptr().cast_const();
            (out, exit_code) = (stale, 0);
            assert_eq!(
                mssql_sqlcmd_diagnostics_run(
                    &request,
                    text(&version),
                    0,
                    &mut out,
                    std::ptr::null_mut(),
                    &mut exit_code
                ),
                MSSQL_SQLCMD_NULL_ARGUMENT
            );
            assert!(out.is_null());
            assert_eq!(exit_code, 5);
            (out, out_len) = (stale, 42);
            assert_eq!(
                mssql_sqlcmd_diagnostics_run(
                    &request,
                    text(&version),
                    0,
                    &mut out,
                    &mut out_len,
                    std::ptr::null_mut()
                ),
                MSSQL_SQLCMD_NULL_ARGUMENT
            );
            assert!(out.is_null());
            assert_eq!(out_len, 0);
            assert_eq!(
                mssql_sqlcmd_diagnostics_run(
                    &request,
                    NULL_TEXT,
                    0,
                    &mut out,
                    &mut out_len,
                    &mut exit_code
                ),
                MSSQL_SQLCMD_NULL_ARGUMENT
            );
        }
    }

    #[test]
    fn set_locale_reports_status() {
        let german = utf16("1031");
        // SAFETY: test text points to readable UTF-16 data.
        assert_eq!(
            unsafe { mssql_sqlcmd_set_locale(text(&german)) },
            MSSQL_SQLCMD_OK
        );
        assert_eq!(crate::i18n::locale(), "de-DE");

        // SAFETY: null with zero length is the documented empty-locale reset.
        assert_eq!(
            unsafe { mssql_sqlcmd_set_locale(NULL_TEXT) },
            MSSQL_SQLCMD_INVALID_ARGUMENT
        );
        assert_eq!(crate::i18n::locale(), "en-US");

        // SAFETY: test text points to readable UTF-16 data.
        assert_eq!(
            unsafe { mssql_sqlcmd_set_locale(text(&german)) },
            MSSQL_SQLCMD_OK
        );

        let invalid = utf16("not-a-locale");
        // SAFETY: test text points to readable UTF-16 data.
        assert_eq!(
            unsafe { mssql_sqlcmd_set_locale(text(&invalid)) },
            MSSQL_SQLCMD_INVALID_ARGUMENT
        );
        assert_eq!(crate::i18n::locale(), "en-US");

        let null_with_len = MssqlSqlcmdText {
            data: std::ptr::null(),
            len: 1,
        };
        // SAFETY: null data is passed to validate the FFI error path.
        assert_eq!(
            unsafe { mssql_sqlcmd_set_locale(null_with_len) },
            MSSQL_SQLCMD_NULL_ARGUMENT
        );
    }

    /// The header declares exactly the functions this module exports, and its
    /// constants have the values of the Rust ones.
    #[test]
    fn the_header_matches_the_exports() {
        let header = include_str!("../include/mssql_sqlcmd.h");
        let source = include_str!("ffi.rs");
        let mut declared: Vec<&str> = header
            .split(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
            .filter(|word| word.starts_with("mssql_sqlcmd_"))
            .collect();
        declared.sort_unstable();
        declared.dedup();
        let mut exported: Vec<&str> = source
            .lines()
            .filter_map(|line| line.trim().split_once("extern \"C\" fn "))
            .map(|(_, rest)| rest.split('(').next().unwrap_or_default())
            .collect();
        exported.sort_unstable();
        assert_eq!(declared, exported);

        let constants = [
            ("MSSQL_SQLCMD_OK", MSSQL_SQLCMD_OK),
            ("MSSQL_SQLCMD_NULL_ARGUMENT", MSSQL_SQLCMD_NULL_ARGUMENT),
            ("MSSQL_SQLCMD_INVALID_STATE", MSSQL_SQLCMD_INVALID_STATE),
            ("MSSQL_SQLCMD_INTERNAL_ERROR", MSSQL_SQLCMD_INTERNAL_ERROR),
            (
                "MSSQL_SQLCMD_INVALID_ARGUMENT",
                MSSQL_SQLCMD_INVALID_ARGUMENT,
            ),
            ("MSSQL_SQLCMD_UNSUPPORTED", MSSQL_SQLCMD_UNSUPPORTED),
            ("MSSQL_SQLCMD_RUN_FINISHED", MSSQL_SQLCMD_RUN_FINISHED),
            ("MSSQL_SQLCMD_RUN_CANCELED", MSSQL_SQLCMD_RUN_CANCELED),
            (
                "MSSQL_SQLCMD_RUN_INVALID_INVOCATION",
                MSSQL_SQLCMD_RUN_INVALID_INVOCATION,
            ),
            (
                "MSSQL_SQLCMD_AUTH_SQL_PASSWORD",
                MSSQL_SQLCMD_AUTH_SQL_PASSWORD,
            ),
            ("MSSQL_SQLCMD_AUTH_INTEGRATED", MSSQL_SQLCMD_AUTH_INTEGRATED),
            (
                "MSSQL_SQLCMD_ENCRYPT_OPTIONAL",
                MSSQL_SQLCMD_ENCRYPT_OPTIONAL,
            ),
            (
                "MSSQL_SQLCMD_ENCRYPT_MANDATORY",
                MSSQL_SQLCMD_ENCRYPT_MANDATORY,
            ),
            ("MSSQL_SQLCMD_ENCRYPT_STRICT", MSSQL_SQLCMD_ENCRYPT_STRICT),
            ("MSSQL_SQLCMD_REPORT_TEXT", MSSQL_SQLCMD_REPORT_TEXT),
            ("MSSQL_SQLCMD_REPORT_JSON", MSSQL_SQLCMD_REPORT_JSON),
            ("MSSQL_SQLCMD_DEPTH_DEFAULT", MSSQL_SQLCMD_DEPTH_DEFAULT),
            (
                "MSSQL_SQLCMD_DEPTH_CONNECTION_INPUT",
                MSSQL_SQLCMD_DEPTH_CONNECTION_INPUT,
            ),
            (
                "MSSQL_SQLCMD_DEPTH_ENDPOINT_RESOLUTION",
                MSSQL_SQLCMD_DEPTH_ENDPOINT_RESOLUTION,
            ),
            (
                "MSSQL_SQLCMD_DEPTH_NETWORK_REACHABILITY",
                MSSQL_SQLCMD_DEPTH_NETWORK_REACHABILITY,
            ),
            (
                "MSSQL_SQLCMD_DEPTH_CONNECTION_ATTEMPT",
                MSSQL_SQLCMD_DEPTH_CONNECTION_ATTEMPT,
            ),
            (
                "MSSQL_SQLCMD_DEPTH_SESSION_VALIDATION",
                MSSQL_SQLCMD_DEPTH_SESSION_VALIDATION,
            ),
        ];
        let defines: Vec<(&str, i32)> = header
            .lines()
            .filter_map(|line| line.strip_prefix("#define MSSQL_SQLCMD_"))
            .filter_map(|rest| {
                let mut words = rest.split_whitespace();
                let name = words.next()?;
                let value = words.next()?.parse().ok()?;
                Some((name, value))
            })
            .collect();
        assert_eq!(defines.len(), constants.len(), "{defines:?}");
        for (name, value) in constants {
            let short = name.trim_start_matches("MSSQL_SQLCMD_");
            assert!(
                defines.contains(&(short, value)),
                "{name} = {value} in the header"
            );
        }
    }
    #[test]
    fn cancel_is_safe_to_call_with_no_diagnosis_running() {
        // Hold the run lock, so this cannot cancel another test's diagnosis.
        #[cfg(target_pointer_width = "64")]
        let _only = crate::diagnostics::RUNNING
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let expected = if cfg!(target_pointer_width = "64") {
            MSSQL_SQLCMD_OK
        } else {
            MSSQL_SQLCMD_UNSUPPORTED
        };
        assert_eq!(mssql_sqlcmd_diagnostics_cancel(), expected);
    }
}
