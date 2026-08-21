// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! `SQL_ATTR_CURRENT_CATALOG` — the connection's current database.
//!
//! Before connecting the attribute records the database to log in to; the
//! connection string's `Database=` keyword wins if both are supplied, matching
//! msodbcsql. Once connected, setting it issues a `USE` batch and reading it
//! reports the database the server says is current, so a database changed by raw
//! T-SQL (or by a `USE` inside a stored procedure) is still reported correctly.
//!
//! Behavior mirrors msodbcsql's `SQLSetConnectAttrW` arm (`sqlcmisc.cpp:1829`),
//! its `ChangeDatabase` helper (`sqlcconn.cpp:4970`), and the `fCopyStrToBuffer`
//! get path (`sqlcmisc.cpp:3176`).

use tracing::{debug, error};

use super::odbc_types::{
    DEFAULT_CATALOG, SQL_ERROR, SQL_NTS, SQL_SUCCESS, SQL_SUCCESS_WITH_INFO, SYSNAMELEN,
    SqlInteger, SqlPointer, SqlReturn, SqlWChar,
};
use super::sqlstate::{
    ERR_INVALID_ATTRIBUTE_VALUE, ERR_INVALID_STRING_OR_BUFFER_LENGTH, SQLSTATE_HY024, post_diag,
    post_tds_error_as, post_tds_info_messages,
};
use super::txn::{claim_dbc_client, close_all_cursors, exec_batch, release_dbc_client};
use super::util::{read_utf16_attr, write_wide_attr};
use crate::error::free_errors;
use crate::handles::DbcHandle;
use crate::handles::dbc::ConnectionState;

const SET_OP: &str = "SQLSetConnectAttrW(SQL_ATTR_CURRENT_CATALOG)";

/// Renders `name` as a bracket-quoted T-SQL identifier, doubling any embedded
/// `]`.
///
/// msodbcsql brackets the name unconditionally rather than honoring
/// `QUOTED_IDENTIFIER` (`BuildUseDBOrSetLanguage`, `sqlcfunc.cpp:1933-1972`),
/// which is also what makes the whole attribute value a single identifier: a
/// caller passing `tempdb]; DROP TABLE x--` gets a "database does not exist"
/// error for that literal name instead of two statements.
fn quote_catalog(name: &str) -> String {
    let mut quoted = String::with_capacity(name.len() + 2);
    quoted.push('[');
    for ch in name.chars() {
        if ch == ']' {
            quoted.push(']');
        }
        quoted.push(ch);
    }
    quoted.push(']');
    quoted
}

/// What [`set_current_catalog`] decided to do once it had read and validated the
/// requested name.
enum CatalogAction {
    /// Nothing to do: the connection is already on this database, the caller
    /// asked for `(Default)`, or there is no connection yet and the name was
    /// simply recorded for the next one.
    Done,
    /// Run `USE <quoted>` on the live connection.
    Switch(String),
}

/// Applies `SQL_ATTR_CURRENT_CATALOG`.
///
/// Locks in three phases so the DBC mutex is never held across the `USE` round
/// trip: validate and decide, execute, then record the result.
///
/// # Safety
/// `value_ptr` must be null, or point to `string_length` bytes, or to a
/// NUL-terminated wide string when `string_length == SQL_NTS`.
pub(super) unsafe fn set_current_catalog(
    dbc: &DbcHandle,
    value_ptr: SqlPointer,
    string_length: SqlInteger,
) -> SqlReturn {
    let name = {
        let Ok(mut state) = dbc.inner.lock() else {
            error!("{SET_OP}: dbc mutex poisoned");
            return SQL_ERROR;
        };
        free_errors(&mut state);

        // `SQL_NTS` is the only negative length ODBC defines for a character
        // attribute; anything else negative is a caller bug (msodbcsql
        // `sqlcmisc.cpp:1414`).
        if string_length < 0 && string_length != SqlInteger::from(SQL_NTS) {
            error!(string_length, "{SET_OP}: invalid string length");
            post_diag(&mut state, ERR_INVALID_STRING_OR_BUFFER_LENGTH);
            return SQL_ERROR;
        }
        if value_ptr.is_null() {
            error!("{SET_OP}: value pointer is null");
            post_diag(&mut state, ERR_INVALID_ATTRIBUTE_VALUE);
            return SQL_ERROR;
        }

        // SAFETY: `value_ptr` is non-null and, per the ODBC contract for a
        // character attribute, points to `string_length` bytes or to a
        // NUL-terminated string when `string_length == SQL_NTS`.
        let name = unsafe { read_utf16_attr(value_ptr as *const SqlWChar, string_length) };

        // msodbcsql measures the limit in UTF-16 code units (`wcslen`), so a
        // name of astral characters hits it in half as many `char`s.
        if name.is_empty() || name.encode_utf16().count() > SYSNAMELEN {
            error!(len = name.len(), "{SET_OP}: invalid database name length");
            post_diag(&mut state, ERR_INVALID_ATTRIBUTE_VALUE);
            return SQL_ERROR;
        }
        name
    };

    match decide(dbc, &name) {
        Err(ret) => ret,
        Ok(CatalogAction::Done) => SQL_SUCCESS,
        Ok(CatalogAction::Switch(sql)) => switch_catalog(dbc, name, sql),
    }
}

/// Second phase: with the name validated, work out whether a round trip is
/// needed at all.
fn decide(dbc: &DbcHandle, name: &str) -> Result<CatalogAction, SqlReturn> {
    let Ok(mut state) = dbc.inner.lock() else {
        error!("{SET_OP}: dbc mutex poisoned");
        return Err(SQL_ERROR);
    };

    if state.connection_state != ConnectionState::Connected {
        state.current_catalog = Some(name.to_string());
        debug!(name, "{SET_OP}: stored for next connect");
        return Ok(CatalogAction::Done);
    }

    // A `USE` cannot be sent while a cursor is streaming, and msodbcsql refuses
    // outright (`CheckBusy` → `IDS_24_000`, `sqlcmisc.cpp:1839`) rather than
    // closing cursors the way the transaction attributes do.
    if state.active_stmt.is_some() {
        error!("{SET_OP}: a cursor is open on this connection");
        post_diag(&mut state, super::sqlstate::ERR_INVALID_CURSOR_STATE);
        return Err(SQL_ERROR);
    }

    // Neither the database already in use nor the `(Default)` sentinel costs a
    // round trip (`sqlcconn.cpp:4990`). Comparing case-insensitively matters:
    // SQL Server database names are usually case-insensitive, so `MASTER` and
    // `master` must not produce a needless `USE`.
    let unchanged = state
        .client
        .as_ref()
        .is_some_and(|c| c.database().eq_ignore_ascii_case(name));
    if unchanged || name.eq_ignore_ascii_case(DEFAULT_CATALOG) {
        debug!(name, "{SET_OP}: already current");
        return Ok(CatalogAction::Done);
    }

    Ok(CatalogAction::Switch(format!(
        "USE {}",
        quote_catalog(name)
    )))
}

/// Third phase: run the `USE` with the DBC mutex released.
fn switch_catalog(dbc: &DbcHandle, name: String, sql: String) -> SqlReturn {
    if close_all_cursors(dbc) == SQL_ERROR {
        error!("{SET_OP}: could not close open cursors");
        return SQL_ERROR;
    }
    let mut client = match claim_dbc_client(dbc, SET_OP) {
        Ok(client) => client,
        Err(ret) => return ret,
    };
    debug!(%sql, "{SET_OP}: changing database");
    let result = exec_batch(dbc, &mut client, &sql);
    // SQL Server announces the switch with message 5701 ("Changed database
    // context to ..."), which msodbcsql surfaces; draining before the client
    // goes back keeps that record.
    let info_messages = client.take_info_messages();
    release_dbc_client(dbc, client);

    let Ok(mut state) = dbc.inner.lock() else {
        error!("{SET_OP}: dbc mutex poisoned");
        return SQL_ERROR;
    };
    if let Err(e) = result {
        error!(%e, "{SET_OP}: could not change database");
        // msodbcsql overwrites the state a failed `USE` would otherwise carry
        // with `HY024` (`sqlcmisc.cpp:1873-1875`), so "database does not exist"
        // reaches the application as HY024 + native 911 rather than 08004.
        post_tds_error_as(&mut state, &e, SQLSTATE_HY024);
        post_tds_info_messages(&mut state, &info_messages);
        return SQL_ERROR;
    }

    state.current_catalog = Some(name);
    if post_tds_info_messages(&mut state, &info_messages) {
        return SQL_SUCCESS_WITH_INFO;
    }
    SQL_SUCCESS
}

/// Reports `SQL_ATTR_CURRENT_CATALOG`.
///
/// Answers from the TDS client's ENVCHANGE-tracked database when connected —
/// msodbcsql reads its own `conninfo.DataBase` (`sqlcmisc.cpp:3176`), which the
/// same token stream maintains — and falls back to the pre-connect value
/// otherwise.
///
/// # Safety
/// `value_ptr` must be null or writable for `buffer_length` bytes, and
/// `string_length_ptr` must be null or writable for one `SQLINTEGER`.
pub(super) unsafe fn get_current_catalog(
    dbc: &DbcHandle,
    value_ptr: SqlPointer,
    buffer_length: SqlInteger,
    string_length_ptr: *mut SqlInteger,
) -> SqlReturn {
    let Ok(mut state) = dbc.inner.lock() else {
        error!("SQLGetConnectAttrW(SQL_ATTR_CURRENT_CATALOG): dbc mutex poisoned");
        return SQL_ERROR;
    };

    let live = state
        .client
        .as_ref()
        .map(|client| client.database())
        // An unnamed database means the client has nothing better than the
        // pre-connect value to offer.
        .filter(|db| !db.is_empty())
        .map(str::to_string);
    // An empty string is the ODBC answer for "no database chosen yet", which is
    // what a fresh handle reports.
    let catalog = live
        .or_else(|| state.current_catalog.clone())
        .unwrap_or_default();

    // SAFETY: forwarded from the FFI boundary, where the caller guarantees
    // `value_ptr` is writable for `buffer_length` bytes and `string_length_ptr`
    // for one `SQLINTEGER`; both may be null.
    unsafe {
        write_wide_attr(
            &mut *state,
            value_ptr as *mut SqlWChar,
            buffer_length,
            string_length_ptr,
            &catalog,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::free_handle::sql_free_handle;
    use crate::api::get_connect_attr::sql_get_connect_attr_w;
    use crate::api::odbc_types::{
        SQL_ATTR_CURRENT_CATALOG, SQL_HANDLE_STMT, SqlHandle, SqlSmallInt,
    };
    use crate::api::set_connect_attr::sql_set_connect_attr_w;
    use crate::handles::handle_from_raw;
    use crate::test_support::TestHandles;

    /// UTF-16 buffer for a name, as an ODBC caller would pass it.
    fn wide(s: &str) -> Vec<SqlWChar> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }

    /// `SQLSetConnectAttrW(SQL_ATTR_CURRENT_CATALOG)` with an explicit length.
    fn set_catalog_len(dbc: SqlHandle, name: &str, len: SqlInteger) -> SqlReturn {
        let buf = wide(name);
        unsafe {
            sql_set_connect_attr_w(
                dbc,
                SQL_ATTR_CURRENT_CATALOG,
                buf.as_ptr() as SqlPointer,
                len,
            )
        }
    }

    /// The same, letting the driver find the NUL terminator.
    fn set_catalog(dbc: SqlHandle, name: &str) -> SqlReturn {
        set_catalog_len(dbc, name, SqlInteger::from(SQL_NTS))
    }

    /// Reads the attribute into a buffer of `bytes` bytes, returning the return
    /// code, the decoded string, and the reported length.
    fn get_catalog(dbc: SqlHandle, bytes: usize) -> (SqlReturn, String, SqlInteger) {
        let mut buf = vec![0u16; bytes.div_ceil(2).max(1)];
        let mut len: SqlInteger = -1;
        let rc = unsafe {
            sql_get_connect_attr_w(
                dbc,
                SQL_ATTR_CURRENT_CATALOG,
                buf.as_mut_ptr() as SqlPointer,
                bytes as SqlInteger,
                &mut len,
            )
        };
        let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
        (rc, String::from_utf16_lossy(&buf[..end]), len)
    }

    fn sqlstate(dbc: SqlHandle) -> String {
        let dbc = unsafe { handle_from_raw::<DbcHandle>(dbc) };
        let state = dbc.inner.lock().unwrap();
        String::from_utf8_lossy(&state.diag_records[0].sql_state).into_owned()
    }

    #[test]
    fn quote_wraps_in_brackets() {
        assert_eq!(quote_catalog("master"), "[master]");
    }

    #[test]
    fn quote_doubles_closing_bracket() {
        assert_eq!(quote_catalog("odbc]probe"), "[odbc]]probe]");
    }

    #[test]
    fn quote_neutralizes_statement_injection() {
        // The whole value stays one identifier, so the server reports "database
        // does not exist" rather than running a second statement.
        assert_eq!(
            quote_catalog("tempdb]; DROP TABLE x--"),
            "[tempdb]]; DROP TABLE x--]"
        );
    }

    #[test]
    fn quote_leaves_other_punctuation_alone() {
        assert_eq!(quote_catalog("my db-1.0"), "[my db-1.0]");
        assert_eq!(quote_catalog("[weird]"), "[[weird]]]");
    }

    #[test]
    fn quote_handles_empty_and_non_ascii() {
        assert_eq!(quote_catalog(""), "[]");
        assert_eq!(quote_catalog("données"), "[données]");
    }

    #[test]
    fn set_before_connect_is_stored_and_read_back() {
        let h = TestHandles::with_env_dbc();
        assert_eq!(set_catalog(h.dbc, "master"), SQL_SUCCESS);
        let (rc, value, len) = get_catalog(h.dbc, 64);
        assert_eq!(rc, SQL_SUCCESS);
        assert_eq!(value, "master");
        // Byte count, not character count — 6 UTF-16 units.
        assert_eq!(len, 12);
    }

    #[test]
    fn set_measures_an_explicit_length_in_bytes_not_characters() {
        let h = TestHandles::with_env_dbc();
        // ODBC gives a character attribute's `StringLength` in bytes, and the
        // Driver Manager resolves `SQL_NTS` to one before the driver is called.
        // Reading it as a character count would append whatever follows the
        // terminator, which the server then rejects as an invalid name.
        assert_eq!(set_catalog_len(h.dbc, "masterdb", 12), SQL_SUCCESS);
        assert_eq!(get_catalog(h.dbc, 64).1, "master");
    }

    #[test]
    fn set_rounds_an_odd_byte_length_down_to_whole_units() {
        let h = TestHandles::with_env_dbc();
        // 13 bytes cannot describe whole `SQLWCHAR`s; six characters is the
        // most that fits.
        assert_eq!(set_catalog_len(h.dbc, "masterdb", 13), SQL_SUCCESS);
        assert_eq!(get_catalog(h.dbc, 64).1, "master");
    }

    #[test]
    fn set_rejects_a_negative_length_that_is_not_sql_nts() {
        let h = TestHandles::with_env_dbc();
        assert_eq!(set_catalog_len(h.dbc, "master", -7), SQL_ERROR);
        assert_eq!(sqlstate(h.dbc), "HY090");
    }

    #[test]
    fn set_rejects_a_null_pointer() {
        let h = TestHandles::with_env_dbc();
        let rc = unsafe {
            sql_set_connect_attr_w(
                h.dbc,
                SQL_ATTR_CURRENT_CATALOG,
                std::ptr::null_mut(),
                SqlInteger::from(SQL_NTS),
            )
        };
        assert_eq!(rc, SQL_ERROR);
        assert_eq!(sqlstate(h.dbc), "HY024");
    }

    #[test]
    fn set_rejects_an_empty_name() {
        let h = TestHandles::with_env_dbc();
        assert_eq!(set_catalog(h.dbc, ""), SQL_ERROR);
        assert_eq!(sqlstate(h.dbc), "HY024");
    }

    #[test]
    fn set_accepts_a_name_at_the_length_limit() {
        let h = TestHandles::with_env_dbc();
        let name = "d".repeat(SYSNAMELEN);
        assert_eq!(set_catalog(h.dbc, &name), SQL_SUCCESS);
        assert_eq!(get_catalog(h.dbc, 1024).1, name);
    }

    #[test]
    fn set_rejects_a_name_past_the_length_limit() {
        let h = TestHandles::with_env_dbc();
        assert_eq!(set_catalog(h.dbc, &"d".repeat(SYSNAMELEN + 1)), SQL_ERROR);
        assert_eq!(sqlstate(h.dbc), "HY024");
    }

    #[test]
    fn length_limit_counts_utf16_units_not_chars() {
        let h = TestHandles::with_env_dbc();
        // Each astral character is a surrogate pair, so 65 of them are 130
        // UTF-16 units — over the limit despite being only 65 `char`s.
        assert_eq!(set_catalog(h.dbc, &"\u{1F600}".repeat(65)), SQL_ERROR);
        assert_eq!(sqlstate(h.dbc), "HY024");
        assert_eq!(set_catalog(h.dbc, &"\u{1F600}".repeat(64)), SQL_SUCCESS);
    }

    #[test]
    fn get_on_a_fresh_handle_reports_an_empty_catalog() {
        let h = TestHandles::with_env_dbc();
        let (rc, value, len) = get_catalog(h.dbc, 64);
        assert_eq!(rc, SQL_SUCCESS);
        assert_eq!(value, "");
        assert_eq!(len, 0);
    }

    #[test]
    fn get_truncates_but_reports_the_full_length() {
        let h = TestHandles::with_env_dbc();
        assert_eq!(set_catalog(h.dbc, "master"), SQL_SUCCESS);
        // Six bytes hold two characters plus the terminator.
        let (rc, value, len) = get_catalog(h.dbc, 6);
        assert_eq!(rc, SQL_SUCCESS_WITH_INFO);
        assert_eq!(value, "ma");
        // The length a correctly sized second call would need, not what fit.
        assert_eq!(len, 12);
        assert_eq!(sqlstate(h.dbc), "01004");
    }

    #[test]
    fn get_with_a_null_buffer_is_a_length_query() {
        let h = TestHandles::with_env_dbc();
        assert_eq!(set_catalog(h.dbc, "tempdb"), SQL_SUCCESS);
        let mut len: SqlInteger = -1;
        let rc = unsafe {
            sql_get_connect_attr_w(
                h.dbc,
                SQL_ATTR_CURRENT_CATALOG,
                std::ptr::null_mut(),
                0,
                &mut len,
            )
        };
        assert_eq!(rc, SQL_SUCCESS);
        assert_eq!(len, 12);
    }

    #[test]
    fn get_tolerates_a_null_length_pointer() {
        let h = TestHandles::with_env_dbc();
        assert_eq!(set_catalog(h.dbc, "tempdb"), SQL_SUCCESS);
        let mut buf = [0u16; 32];
        let rc = unsafe {
            sql_get_connect_attr_w(
                h.dbc,
                SQL_ATTR_CURRENT_CATALOG,
                buf.as_mut_ptr() as SqlPointer,
                64,
                std::ptr::null_mut(),
            )
        };
        assert_eq!(rc, SQL_SUCCESS);
    }

    #[test]
    fn set_is_rejected_while_a_cursor_is_open() {
        // msodbcsql refuses the switch outright rather than closing the cursor,
        // so a fetch loop cannot be silently reset out from under the caller.
        let mut h = TestHandles::with_env_dbc();
        let stmt = h.alloc_extra_stmt();
        h.mark_dbc_connected();
        {
            let dbc = unsafe { handle_from_raw::<DbcHandle>(h.dbc) };
            dbc.inner.lock().unwrap().active_stmt = Some(stmt);
        }
        assert_eq!(set_catalog(h.dbc, "master"), SQL_ERROR);
        assert_eq!(sqlstate(h.dbc), "24000");
        // The stored value is untouched by the rejected set.
        assert_eq!(get_catalog(h.dbc, 64).1, "");
        unsafe { sql_free_handle(SQL_HANDLE_STMT, stmt) };
        let dbc = unsafe { handle_from_raw::<DbcHandle>(h.dbc) };
        dbc.inner.lock().unwrap().active_stmt = None;
    }

    #[test]
    fn default_sentinel_is_a_no_op_when_connected() {
        // "(Default)" means "whatever the login chose", so it never sends `USE`
        // — proven here by it succeeding with no TDS client available at all.
        let h = TestHandles::with_env_dbc();
        h.mark_dbc_connected();
        assert_eq!(set_catalog(h.dbc, DEFAULT_CATALOG), SQL_SUCCESS);
    }

    #[test]
    fn attribute_helper_matches_the_narrow_one_for_sql_nts() {
        // `SQLSetConnectAttrW` carries a SQLINTEGER byte length while the rest
        // of the driver uses a SQLSMALLINT character count; the two must decode
        // identically when the length is SQL_NTS.
        let buf = wide("master");
        let ptr = buf.as_ptr();
        assert_eq!(
            unsafe { read_utf16_attr(ptr, SqlInteger::from(SQL_NTS)) },
            unsafe { crate::api::util::read_utf16(ptr, SQL_NTS as SqlSmallInt) }
        );
    }

    /// Panics while holding the DBC lock, leaving the mutex poisoned.
    fn poison_dbc(dbc: SqlHandle) {
        let handle = unsafe { handle_from_raw::<DbcHandle>(dbc) };
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _guard = handle.inner.lock().unwrap();
            panic!("poison the dbc lock");
        }));
    }

    #[test]
    fn set_fails_cleanly_on_a_poisoned_lock() {
        let h = TestHandles::with_env_dbc();
        poison_dbc(h.dbc);
        assert_eq!(set_catalog(h.dbc, "master"), SQL_ERROR);
    }

    #[test]
    fn get_fails_cleanly_on_a_poisoned_lock() {
        let h = TestHandles::with_env_dbc();
        poison_dbc(h.dbc);
        assert_eq!(get_catalog(h.dbc, 64).0, SQL_ERROR);
    }

    #[test]
    fn decide_fails_cleanly_on_a_poisoned_lock() {
        // `set_current_catalog` bails at its own lock, so the second phase's
        // guard is only reachable directly.
        let h = TestHandles::with_env_dbc();
        poison_dbc(h.dbc);
        let dbc = unsafe { handle_from_raw::<DbcHandle>(h.dbc) };
        assert!(matches!(decide(dbc, "master"), Err(SQL_ERROR)));
    }
}
