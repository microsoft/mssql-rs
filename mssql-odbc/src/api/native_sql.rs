// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Implementation of SQLNativeSqlW — show the SQL the driver would send.

use tracing::{debug, error};

use crate::api::escape::translate_escapes;
use crate::api::odbc_types::{
    SQL_ERROR, SQL_INVALID_HANDLE, SQL_SUCCESS, SQL_SUCCESS_WITH_INFO, SqlHandle, SqlInteger,
    SqlReturn, SqlWChar,
};
use crate::api::sqlstate::{SQLSTATE_HY090, WARN_STRING_TRUNCATION, post_diag};
use crate::api::util::{copy_with_nul, read_utf16_long, write_if_some};
use crate::error::{free_errors, post_sql_error};
use crate::handles::{DbcHandle, HandleType, handle_from_raw};

/// Returns the statement as the driver would send it, with ODBC escape
/// sequences translated.
///
/// Escape translation is unconditional here: `SQL_ATTR_NOSCAN` is a *statement*
/// attribute and this is a connection-level call, so there is no statement to
/// read it from. msodbcsql behaves the same way — `DoSubstitutions` forces
/// `SQL_NOSCAN_OFF` when it is invoked without a statement handle
/// (`sqlcmisc.cpp:4559`).
///
/// Parameter markers are *not* rewritten. That is phase 2, which only the
/// execution path runs; msodbcsql likewise answers `{? = call sp_who(?)}` with
/// ` EXEC ?=sp_who ?  `.
///
/// # Safety
/// - `connection_handle` must be a valid DBC handle from `SQLAllocHandle`.
/// - `in_statement_text` must be readable for `text_length1` UTF-16 code units,
///   or through a NUL terminator when `text_length1` is `SQL_NTS`.
/// - `out_statement_text`, when non-null, must be writable for `buffer_length`
///   UTF-16 code units.
/// - `text_length2_ptr`, when non-null, must be writable for one `SQLINTEGER`.
pub(crate) unsafe fn sql_native_sql_w(
    connection_handle: SqlHandle,
    in_statement_text: *const SqlWChar,
    text_length1: SqlInteger,
    out_statement_text: *mut SqlWChar,
    buffer_length: SqlInteger,
    text_length2_ptr: *mut SqlInteger,
) -> SqlReturn {
    debug!(
        ?connection_handle,
        text_length1, buffer_length, "SQLNativeSqlW called",
    );

    if connection_handle.is_null() {
        error!("SQLNativeSqlW: connection_handle is null");
        return SQL_INVALID_HANDLE;
    }
    let dbc = unsafe { handle_from_raw::<DbcHandle>(connection_handle) };
    debug_assert_eq!(
        dbc.object_type,
        HandleType::Dbc,
        "SQLNativeSqlW: handle is not a DBC"
    );

    // The DM rejects a null input pointer before the driver sees it.
    debug_assert!(
        !in_statement_text.is_null(),
        "SQLNativeSqlW: in_statement_text is null — DM should have rejected this"
    );

    let sql = unsafe { read_utf16_long(in_statement_text, text_length1) };

    let Ok(mut state) = dbc.inner.lock() else {
        error!("SQLNativeSqlW: dbc mutex poisoned");
        return SQL_ERROR;
    };
    free_errors(&mut state);

    if buffer_length < 0 {
        error!(buffer_length, "SQLNativeSqlW: negative buffer length");
        post_sql_error(
            &mut state,
            SQLSTATE_HY090,
            0,
            "Invalid string or buffer length",
        );
        return SQL_ERROR;
    }

    let translated = match translate_escapes(&sql) {
        Ok(t) => t.sql,
        Err(e) => {
            error!(error = %e, "SQLNativeSqlW: escape translation failed");
            post_sql_error(&mut state, e.state(), 0, e.message());
            return SQL_ERROR;
        }
    };

    // Unlike SQLGetInfo, SQLNativeSql counts characters, not bytes.
    let utf16: Vec<SqlWChar> = translated.encode_utf16().collect();
    unsafe {
        write_if_some(
            text_length2_ptr,
            SqlInteger::try_from(utf16.len()).unwrap_or(SqlInteger::MAX),
        );
    }

    if out_statement_text.is_null() {
        return SQL_SUCCESS;
    }

    let truncated = unsafe { copy_with_nul(out_statement_text, buffer_length as usize, &utf16) };
    if truncated {
        post_diag(&mut state, WARN_STRING_TRUNCATION);
        return SQL_SUCCESS_WITH_INFO;
    }
    SQL_SUCCESS
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::odbc_types::{SQL_NTS, SQL_NULL_HANDLE};
    use crate::test_support::TestHandles;

    fn wide(s: &str) -> Vec<SqlWChar> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }

    fn native_sql(dbc: SqlHandle, sql: &str, cap: usize) -> (SqlReturn, String, SqlInteger) {
        let input = wide(sql);
        let mut out = vec![0u16; cap];
        let mut len: SqlInteger = -1;
        let rc = unsafe {
            sql_native_sql_w(
                dbc,
                input.as_ptr(),
                SQL_NTS as SqlInteger,
                out.as_mut_ptr(),
                cap as SqlInteger,
                &mut len,
            )
        };
        let text = String::from_utf16_lossy(&out[..out.iter().position(|&c| c == 0).unwrap_or(0)]);
        (rc, text, len)
    }

    #[test]
    fn null_handle_returns_invalid_handle() {
        let sql = wide("SELECT 1");
        let mut out = [0u16; 32];
        let mut len: SqlInteger = 0;
        let rc = unsafe {
            sql_native_sql_w(
                SQL_NULL_HANDLE,
                sql.as_ptr(),
                SQL_NTS as SqlInteger,
                out.as_mut_ptr(),
                32,
                &mut len,
            )
        };
        assert_eq!(rc, SQL_INVALID_HANDLE);
    }

    /// A disconnected DBC is fine: translation is pure text work and never
    /// touches the wire.
    #[test]
    fn translation_needs_no_connection() {
        let h = TestHandles::with_env_dbc();
        let (rc, text, len) = native_sql(h.dbc, "{call sp_who}", 64);
        assert_eq!(rc, SQL_SUCCESS);
        assert_eq!(text, " EXEC sp_who  ");
        assert_eq!(len, text.encode_utf16().count() as SqlInteger);
    }

    #[test]
    fn markers_are_preserved() {
        let h = TestHandles::with_env_dbc();
        let (rc, text, _) = native_sql(h.dbc, "{? = call sp_who(?)}", 64);
        assert_eq!(rc, SQL_SUCCESS);
        assert_eq!(text, " EXEC ?=sp_who ?  ");
    }

    #[test]
    fn native_escapes_are_unchanged() {
        let h = TestHandles::with_env_dbc();
        let (rc, text, _) = native_sql(h.dbc, "SELECT {fn UCASE('abc')}", 64);
        assert_eq!(rc, SQL_SUCCESS);
        assert_eq!(text, "SELECT {fn UCASE('abc')}");
    }

    #[test]
    fn a_malformed_escape_is_an_error() {
        let h = TestHandles::with_env_dbc();
        let (rc, _, _) = native_sql(h.dbc, "SELECT {bogus 1}", 64);
        assert_eq!(rc, SQL_ERROR);
    }

    /// A short buffer truncates and warns; the reported length is still the
    /// full one, in characters.
    #[test]
    fn short_buffer_truncates_with_01004() {
        let h = TestHandles::with_env_dbc();
        let (rc, text, len) = native_sql(h.dbc, "SELECT {fn UCASE('abc')}", 8);
        assert_eq!(rc, SQL_SUCCESS_WITH_INFO);
        assert_eq!(text, "SELECT ");
        assert_eq!(len, 24);
    }

    /// A null output buffer is the length-query form.
    #[test]
    fn null_output_buffer_reports_the_length() {
        let h = TestHandles::with_env_dbc();
        let input = wide("SELECT 1");
        let mut len: SqlInteger = -1;
        let rc = unsafe {
            sql_native_sql_w(
                h.dbc,
                input.as_ptr(),
                SQL_NTS as SqlInteger,
                std::ptr::null_mut(),
                0,
                &mut len,
            )
        };
        assert_eq!(rc, SQL_SUCCESS);
        assert_eq!(len, 8);
    }

    #[test]
    fn negative_buffer_length_is_rejected() {
        let h = TestHandles::with_env_dbc();
        let input = wide("SELECT 1");
        let mut out = [0u16; 32];
        let mut len: SqlInteger = 0;
        let rc = unsafe {
            sql_native_sql_w(
                h.dbc,
                input.as_ptr(),
                SQL_NTS as SqlInteger,
                out.as_mut_ptr(),
                -1,
                &mut len,
            )
        };
        assert_eq!(rc, SQL_ERROR);
    }
}
