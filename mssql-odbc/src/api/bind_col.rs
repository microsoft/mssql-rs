// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Implementation of SQLBindCol: the application side of the columnar fetch
//! path.
//!
//! Binding only records where a column's value should land; nothing is written
//! until `SQLFetchScroll` fills the rowset. That separation is why validation
//! here is deliberately shallow — ODBC allows binding before the statement is
//! executed, so there is no column metadata yet to check the ordinal or the
//! source type against. Anything metadata-dependent is reported per row by the
//! fetch instead.

use tracing::{debug, error};

use crate::api::odbc_types::{
    SQL_C_DEFAULT, SQL_ERROR, SQL_INVALID_HANDLE, SQL_SUCCESS, SqlHandle, SqlLen, SqlPointer,
    SqlReturn, SqlSmallInt, SqlUSmallInt,
};
use crate::api::sqlstate::{
    ERR_FUNCTION_SEQUENCE, ERR_INVALID_C_DATA_TYPE, ERR_INVALID_DESCRIPTOR_INDEX,
    ERR_INVALID_STRING_OR_BUFFER_LENGTH, SQLSTATE_HY000, post_diag,
};
use crate::api::type_rules::{canonical_c_type, is_valid_c_type};
use crate::error::{free_errors, post_sql_error};
use crate::handles::stmt::{ColumnBinding, STMT_STATE_FETCH_IN_PROGRESS};
use crate::handles::{DescHandle, HandleType, StmtHandle, handle_from_raw};

/// Implements SQLBindCol.
///
/// # Safety
/// `statement_handle` must be a valid `StmtHandle` or null. The buffers must
/// stay valid until the column is unbound or the statement is freed.
pub(crate) unsafe fn sql_bind_col(
    statement_handle: SqlHandle,
    column_number: SqlUSmallInt,
    target_type: SqlSmallInt,
    target_value_ptr: SqlPointer,
    buffer_length: SqlLen,
    strlen_or_ind_ptr: *mut SqlLen,
) -> SqlReturn {
    debug!(
        ?statement_handle,
        column_number, target_type, buffer_length, "SQLBindCol called"
    );
    crate::ffi_entry!("SQLBindCol", unsafe {
        sql_bind_col_impl(
            statement_handle,
            column_number,
            target_type,
            target_value_ptr,
            buffer_length,
            strlen_or_ind_ptr,
        )
    })
}

unsafe fn sql_bind_col_impl(
    statement_handle: SqlHandle,
    column_number: SqlUSmallInt,
    target_type: SqlSmallInt,
    target_value_ptr: SqlPointer,
    buffer_length: SqlLen,
    strlen_or_ind_ptr: *mut SqlLen,
) -> SqlReturn {
    if statement_handle.is_null() {
        error!("SQLBindCol: statement_handle is null");
        return SQL_INVALID_HANDLE;
    }
    let stmt = unsafe { handle_from_raw::<StmtHandle>(statement_handle) };
    debug_assert_eq!(stmt.object_type, HandleType::Stmt);
    sql_bind_col_safe(
        stmt,
        column_number,
        target_type,
        target_value_ptr,
        buffer_length,
        strlen_or_ind_ptr,
    )
}

fn sql_bind_col_safe(
    stmt: &StmtHandle,
    column_number: SqlUSmallInt,
    target_type: SqlSmallInt,
    target_value_ptr: SqlPointer,
    buffer_length: SqlLen,
    strlen_or_ind_ptr: *mut SqlLen,
) -> SqlReturn {
    // Validated under the STMT lock, exactly as before, but the actual write
    // now lands on the effective ARD's own DescState (AB#47437: the ARD is
    // the single source of truth `SQLBindCol` and `SQLSetDescFieldW` share,
    // not a separate table) — which needs its own lock. The STMT lock is
    // dropped before that one is taken: this crate never holds a STMT lock
    // while acquiring a DESC lock (see ".github/instructions/mssql-odbc.instructions.md",
    // "Locking rules" — DESC is a DBC sibling of STMT, not its child), since
    // `free_desc` already walks DBC→STMT in the other direction to reset a
    // freed descriptor's associations, and holding both here in the opposite
    // order would be a classic ABBA deadlock.
    let ard = {
        let Ok(mut stmt_state) = stmt.inner.lock() else {
            error!("SQLBindCol: stmt mutex poisoned");
            return SQL_ERROR;
        };
        free_errors(&mut stmt_state);

        // A fetch in flight is reading through the ARD it snapshotted, so
        // rebinding now could free a buffer mid-read.
        if stmt_state.has_state(STMT_STATE_FETCH_IN_PROGRESS) {
            error!("SQLBindCol: a fetch is in progress on this statement");
            post_diag(&mut stmt_state, ERR_FUNCTION_SEQUENCE);
            return SQL_ERROR;
        }

        // Column 0 is the bookmark column. Bookmarks need SQL_ATTR_USE_BOOKMARKS,
        // which a forward-only cursor does not offer, so the ordinal is simply out
        // of range here.
        if column_number == 0 {
            error!("SQLBindCol: column 0 is the bookmark column, which is not supported");
            post_diag(&mut stmt_state, ERR_INVALID_DESCRIPTOR_INDEX);
            return SQL_ERROR;
        }

        // A descriptor record number is an SQLSMALLINT: reject an ordinal
        // that can't be represented as one before it ever reaches
        // `bind_ard_column`, which would otherwise grow the ARD to
        // `column_number`'s full width and then silently truncate the write
        // to record 32767 via `as`/`unwrap_or(SqlSmallInt::MAX)` — binding
        // the wrong column while still reporting success.
        if column_number > SqlUSmallInt::try_from(SqlSmallInt::MAX).unwrap_or(SqlUSmallInt::MAX) {
            error!(
                column_number,
                "SQLBindCol: column number exceeds the maximum descriptor record number"
            );
            post_diag(&mut stmt_state, ERR_INVALID_DESCRIPTOR_INDEX);
            return SQL_ERROR;
        }

        // A null TargetValuePtr unbinds the column whatever the indicator says.
        // msodbcsql never inspects the indicator here (`sqlcdesc.cpp` UnbindParam)
        // and has no indicator-only binding, so keeping one bound would both consume
        // the column from the row cursor and run validation msodbcsql never reaches.
        if target_value_ptr.is_null() {
            let ard = stmt_state.effective_ard(stmt);
            drop(stmt_state);
            let Ok(()) = unbind_ard_column(ard, column_number) else {
                error!("SQLBindCol: ard mutex poisoned; unbind failed");
                if let Ok(mut stmt_state) = stmt.inner.lock() {
                    post_sql_error(
                        &mut stmt_state,
                        SQLSTATE_HY000,
                        0,
                        "Internal error unbinding column",
                    );
                }
                return SQL_ERROR;
            };
            debug!(column_number, "SQLBindCol: column unbound");
            return SQL_SUCCESS;
        }

        if buffer_length < 0 {
            error!(buffer_length, "SQLBindCol: negative buffer length");
            post_diag(&mut stmt_state, ERR_INVALID_STRING_OR_BUFFER_LENGTH);
            return SQL_ERROR;
        }

        // Divergence: msodbcsql accepts SQL_C_DEFAULT here and resolves it at fetch
        // time from the IRD (`sqlcfunc.cpp` BindOffset -> Sql2CDefault). Deferring
        // needs the column's SQL type threaded into the fill loop, which the binding
        // does not carry today, so this is refused for now and tracked separately.
        if target_type == SQL_C_DEFAULT {
            error!("SQLBindCol: SQL_C_DEFAULT is not supported as a bound target");
            post_diag(&mut stmt_state, ERR_INVALID_C_DATA_TYPE);
            return SQL_ERROR;
        }

        // Same gate as SQLBindParameter: fold the deprecated 2.x date/time
        // spellings first so one form per type reaches storage and delivery. This
        // only decides whether the identifier names a real ODBC type; whether the
        // fetch can actually deliver it is a per-row question.
        let canonical_type = canonical_c_type(target_type);
        if !is_valid_c_type(canonical_type) {
            error!(target_type, "SQLBindCol: invalid target C type");
            post_diag(&mut stmt_state, ERR_INVALID_C_DATA_TYPE);
            return SQL_ERROR;
        }

        stmt_state.effective_ard(stmt)
    };

    let binding = ColumnBinding {
        column_number,
        target_type: canonical_c_type(target_type),
        target_value_ptr,
        buffer_length,
        strlen_or_ind_ptr,
        // SQLBindCol's one StrLen_or_Ind argument feeds both descriptor
        // fields at once (matches msodbcsql's `pIndValue = pcbValue =
        // pcbValue`, sqlcdesc.cpp) — SQL_DESC_INDICATOR_PTR and
        // SQL_DESC_OCTET_LENGTH_PTR only diverge when set independently via
        // SQLSetDescFieldW/SQLSetDescRec.
        octet_length_ptr: strlen_or_ind_ptr,
    };
    let Ok(()) = bind_ard_column(ard, binding) else {
        error!("SQLBindCol: ard mutex poisoned or missing record after growth");
        if let Ok(mut stmt_state) = stmt.inner.lock() {
            post_sql_error(
                &mut stmt_state,
                SQLSTATE_HY000,
                0,
                "Internal error binding column",
            );
        }
        return SQL_ERROR;
    };
    debug!(column_number, target_type, "SQLBindCol: column bound");
    SQL_SUCCESS
}

/// Writes `binding` into `ard`'s record at `binding.column_number`, growing
/// the record list first if that ordinal doesn't exist yet. `Err(())` if
/// `ard` is no longer a live descriptor (freed by a concurrent
/// `SQLFreeHandle(SQL_HANDLE_DESC)` on the explicit descriptor between
/// `effective_ard` resolving it and this call locking it — a narrow race this
/// check does not fully close, but converts from a raw-pointer dereference of
/// freed memory into a clean `SQL_ERROR` in the overwhelming majority of
/// timings), a poisoned ARD mutex, or a missing record after growth — the
/// caller decides how to report that against the statement, since bind
/// errors are always posted to the STMT handle, never the descriptor.
/// `binding.column_number` must already fit `SqlSmallInt` (`sql_bind_col_safe`
/// rejects an out-of-range ordinal before this is ever called), so the
/// conversion here is not expected to fail in practice — but this still
/// reports it as an error rather than panicking or silently truncating to the
/// wrong record.
fn bind_ard_column(ard: SqlHandle, binding: ColumnBinding) -> Result<(), ()> {
    if crate::handles::live_type(ard) != Some(HandleType::Desc) {
        return Err(());
    }
    let desc = unsafe { handle_from_raw::<DescHandle>(ard) };
    let Ok(mut desc_state) = desc.inner.lock() else {
        return Err(());
    };
    let record_number = SqlSmallInt::try_from(binding.column_number).map_err(|_| ())?;
    let target_count = desc_state
        .records
        .len()
        .max(usize::from(binding.column_number));
    desc_state.set_record_count(target_count, desc.kind);
    let record = desc_state.record_mut(record_number).ok_or(())?;
    binding.write_to_record(record);
    Ok(())
}

/// Unbinds `column_number` on `ard` by nulling its record's `SQL_DESC_DATA_PTR`
/// — this driver's "unbound" signal (`ColumnBinding::from_record`). A no-op,
/// not an error, if no record exists yet at that ordinal (never bound): that
/// is not something `SQLBindCol` can meaningfully fail on the way a genuine
/// bind can. `Err(())` if `ard` is no longer a live descriptor (see
/// `bind_ard_column`'s identical concurrent-free note) or its mutex is
/// poisoned — the caller decides how to report that against the statement,
/// since bind/unbind errors are always posted to the STMT handle, never a
/// descriptor: reporting `SQL_SUCCESS` here would tell the application an
/// unbind happened when it didn't, and a stale bound column would keep
/// writing through a possibly-freed application buffer on the next fetch.
fn unbind_ard_column(ard: SqlHandle, column_number: SqlUSmallInt) -> Result<(), ()> {
    if crate::handles::live_type(ard) != Some(HandleType::Desc) {
        return Err(());
    }
    let desc = unsafe { handle_from_raw::<DescHandle>(ard) };
    let Ok(mut desc_state) = desc.inner.lock() else {
        return Err(());
    };
    let Ok(record_number) = SqlSmallInt::try_from(column_number) else {
        return Ok(());
    };
    if let Some(record) = desc_state.record_mut(record_number) {
        record.data_ptr = std::ptr::null_mut();
    }
    Ok(())
}

/// Implements `SQLFreeStmt(SQL_UNBIND)`: drops every column binding.
///
/// mssql-python calls this before every fetch, so the columnar path depends on
/// it even when the application never unbinds a column itself.
///
/// Per spec (`SQLFreeStmt`'s `SQL_UNBIND` option) this sets `SQL_DESC_COUNT`
/// on the ARD to 0 — a real truncation, not just nulling every record's
/// `SQL_DESC_DATA_PTR` — unlike a single-column `SQLBindCol(..., NULL, ...)`
/// unbind, which the spec does not say shrinks the record list at all.
///
/// # Safety
/// `statement_handle` must be a valid `StmtHandle` or null.
pub(crate) unsafe fn sql_free_stmt_unbind(statement_handle: SqlHandle) -> SqlReturn {
    debug!(?statement_handle, "SQLFreeStmt(SQL_UNBIND) called");
    crate::ffi_entry!("SQLFreeStmt(SQL_UNBIND)", unsafe {
        if statement_handle.is_null() {
            error!("SQLFreeStmt(SQL_UNBIND): statement_handle is null");
            return SQL_INVALID_HANDLE;
        }
        let stmt = handle_from_raw::<StmtHandle>(statement_handle);
        debug_assert_eq!(stmt.object_type, HandleType::Stmt);
        sql_free_stmt_unbind_safe(stmt)
    })
}

fn sql_free_stmt_unbind_safe(stmt: &StmtHandle) -> SqlReturn {
    let ard = {
        let Ok(mut stmt_state) = stmt.inner.lock() else {
            error!("SQLFreeStmt(SQL_UNBIND): stmt mutex poisoned");
            return SQL_ERROR;
        };
        free_errors(&mut stmt_state);
        if stmt_state.has_state(STMT_STATE_FETCH_IN_PROGRESS) {
            error!("SQLFreeStmt(SQL_UNBIND): a fetch is in progress on this statement");
            post_diag(&mut stmt_state, ERR_FUNCTION_SEQUENCE);
            return SQL_ERROR;
        }
        stmt_state.effective_ard(stmt)
    };

    if crate::handles::live_type(ard) != Some(HandleType::Desc) {
        error!("SQLFreeStmt(SQL_UNBIND): ard freed concurrently; unbind failed");
        if let Ok(mut stmt_state) = stmt.inner.lock() {
            post_sql_error(
                &mut stmt_state,
                SQLSTATE_HY000,
                0,
                "Internal error unbinding columns",
            );
        }
        return SQL_ERROR;
    }
    let desc = unsafe { handle_from_raw::<DescHandle>(ard) };
    let Ok(mut desc_state) = desc.inner.lock() else {
        error!("SQLFreeStmt(SQL_UNBIND): ard mutex poisoned; unbind failed");
        if let Ok(mut stmt_state) = stmt.inner.lock() {
            post_sql_error(
                &mut stmt_state,
                SQLSTATE_HY000,
                0,
                "Internal error unbinding columns",
            );
        }
        return SQL_ERROR;
    };
    desc_state.set_record_count(0, desc.kind);
    debug!("SQLFreeStmt(SQL_UNBIND): all column bindings released");
    SQL_SUCCESS
}

#[cfg(test)]
mod tests {
    use std::ptr;

    use super::*;
    use crate::api::odbc_types::{
        SQL_C_CHAR, SQL_C_DATE, SQL_C_DEFAULT, SQL_C_INTERVAL_YEAR, SQL_C_NUMERIC, SQL_C_SLONG,
        SQL_C_TIME, SQL_C_TIMESTAMP, SQL_C_TYPE_DATE, SQL_C_TYPE_TIME, SQL_C_TYPE_TIMESTAMP,
    };
    use crate::handles::stmt::STMT_STATE_FETCH_IN_PROGRESS;
    use crate::test_support::TestHandles;

    fn bindings_len(h: &TestHandles) -> usize {
        let ard = unsafe { handle_from_raw::<DescHandle>(h.ard()) };
        let state = ard.inner.lock().unwrap();
        ColumnBinding::all_from_ard_state(&state).len()
    }

    /// Every currently-bound column on `h`'s implicit ARD, in column order —
    /// the same view `SQLFetchScroll` derives fresh from the descriptor.
    fn bindings(h: &TestHandles) -> Vec<ColumnBinding> {
        let ard = unsafe { handle_from_raw::<DescHandle>(h.ard()) };
        let state = ard.inner.lock().unwrap();
        ColumnBinding::all_from_ard_state(&state)
    }

    /// `SQL_DESC_COUNT` on `h`'s implicit ARD — distinct from `bindings_len`,
    /// which only counts records with a live `SQL_DESC_DATA_PTR`. A single
    /// `SQLBindCol(..., NULL, ...)` unbind clears a record without shrinking
    /// this; only `SQLFreeStmt(SQL_UNBIND)` sets it back to 0 (spec).
    fn record_count(h: &TestHandles) -> usize {
        let ard = unsafe { handle_from_raw::<DescHandle>(h.ard()) };
        ard.inner.lock().unwrap().records.len()
    }

    fn last_state(h: &TestHandles) -> [u8; 5] {
        let stmt = unsafe { handle_from_raw::<StmtHandle>(h.stmt) };
        let s = stmt.inner.lock().unwrap();
        s.diag_records.last().unwrap().sql_state
    }

    #[test]
    fn null_handle_returns_invalid_handle() {
        let mut buf = 0i32;
        let rc = unsafe {
            sql_bind_col(
                ptr::null_mut(),
                1,
                SQL_C_SLONG,
                &mut buf as *mut i32 as SqlPointer,
                0,
                ptr::null_mut(),
            )
        };
        assert_eq!(rc, SQL_INVALID_HANDLE);
    }

    #[test]
    fn binding_a_column_records_it() {
        let h = TestHandles::with_env_dbc_stmt();
        let mut buf = [0i32; 4];
        let mut ind = [0 as SqlLen; 4];
        let rc = unsafe {
            sql_bind_col(
                h.stmt,
                2,
                SQL_C_SLONG,
                buf.as_mut_ptr() as SqlPointer,
                0,
                ind.as_mut_ptr(),
            )
        };
        assert_eq!(rc, SQL_SUCCESS);
        let bound = bindings(&h);
        assert_eq!(bound.len(), 1);
        assert_eq!(bound[0].column_number, 2);
        assert_eq!(bound[0].target_type, SQL_C_SLONG);
    }

    /// Binding is legal before the statement is executed, so there is no
    /// metadata to validate the ordinal against and no cursor requirement.
    #[test]
    fn binding_before_execute_is_allowed() {
        let h = TestHandles::with_env_dbc_stmt();
        let mut buf = [0i32; 1];
        let rc = unsafe {
            sql_bind_col(
                h.stmt,
                99,
                SQL_C_SLONG,
                buf.as_mut_ptr() as SqlPointer,
                0,
                ptr::null_mut(),
            )
        };
        assert_eq!(
            rc, SQL_SUCCESS,
            "an out-of-range ordinal is a fetch-time concern"
        );
        assert_eq!(bindings_len(&h), 1);
    }

    /// Both pointers null unbinds the column.
    #[test]
    fn binding_with_both_pointers_null_unbinds() {
        let h = TestHandles::with_env_dbc_stmt();
        let mut buf = [0i32; 1];
        unsafe {
            sql_bind_col(
                h.stmt,
                1,
                SQL_C_SLONG,
                buf.as_mut_ptr() as SqlPointer,
                0,
                ptr::null_mut(),
            )
        };
        assert_eq!(bindings_len(&h), 1);

        let rc =
            unsafe { sql_bind_col(h.stmt, 1, SQL_C_SLONG, ptr::null_mut(), 0, ptr::null_mut()) };
        assert_eq!(rc, SQL_SUCCESS);
        assert_eq!(bindings_len(&h), 0);
    }

    /// A null data pointer unbinds whatever the indicator says. Keeping such a
    /// binding alive would consume the column from the row cursor and subject it
    /// to validation msodbcsql never reaches, since it unbinds first.
    #[test]
    fn a_null_data_pointer_unbinds_whatever_the_indicator_says() {
        let h = TestHandles::with_env_dbc_stmt();
        let mut buf = [0i32; 1];
        let mut ind = [0 as SqlLen; 1];
        let rc = unsafe {
            sql_bind_col(
                h.stmt,
                1,
                SQL_C_SLONG,
                buf.as_mut_ptr() as SqlPointer,
                4,
                ind.as_mut_ptr(),
            )
        };
        assert_eq!(rc, SQL_SUCCESS);
        assert_eq!(bindings_len(&h), 1);

        let rc =
            unsafe { sql_bind_col(h.stmt, 1, SQL_C_SLONG, ptr::null_mut(), 0, ind.as_mut_ptr()) };
        assert_eq!(rc, SQL_SUCCESS);
        assert_eq!(
            bindings_len(&h),
            0,
            "the live indicator must not keep it bound"
        );
    }

    /// Unbinding happens before any argument validation, so a combination that
    /// would otherwise be rejected still unbinds rather than erroring.
    #[test]
    fn unbinding_skips_the_argument_validation() {
        let h = TestHandles::with_env_dbc_stmt();
        let mut ind = [0 as SqlLen; 1];
        let rc = unsafe {
            sql_bind_col(
                h.stmt,
                1,
                SQL_C_DEFAULT,
                ptr::null_mut(),
                -1,
                ind.as_mut_ptr(),
            )
        };
        assert_eq!(rc, SQL_SUCCESS);
    }

    /// Column 0 is the bookmark column, which a forward-only cursor does not
    /// offer.
    #[test]
    fn binding_the_bookmark_column_is_rejected() {
        let h = TestHandles::with_env_dbc_stmt();
        let mut buf = [0i32; 1];
        let rc = unsafe {
            sql_bind_col(
                h.stmt,
                0,
                SQL_C_SLONG,
                buf.as_mut_ptr() as SqlPointer,
                0,
                ptr::null_mut(),
            )
        };
        assert_eq!(rc, SQL_ERROR);
        assert_eq!(last_state(&h), *b"07009");
    }

    #[test]
    fn a_negative_buffer_length_is_rejected() {
        let h = TestHandles::with_env_dbc_stmt();
        let mut buf = [0u8; 8];
        let rc = unsafe {
            sql_bind_col(
                h.stmt,
                1,
                SQL_C_CHAR,
                buf.as_mut_ptr() as SqlPointer,
                -1,
                ptr::null_mut(),
            )
        };
        assert_eq!(rc, SQL_ERROR);
        assert_eq!(last_state(&h), *b"HY090");
    }

    /// An unknown target type is rejected at bind time rather than surfacing as
    /// a per-row failure on every row of the first fetch.
    #[test]
    fn an_unsupported_target_type_is_rejected() {
        let h = TestHandles::with_env_dbc_stmt();
        let mut buf = [0u8; 8];
        for target in [SQL_C_DEFAULT, 12345] {
            let rc = unsafe {
                sql_bind_col(
                    h.stmt,
                    1,
                    target,
                    buf.as_mut_ptr() as SqlPointer,
                    8,
                    ptr::null_mut(),
                )
            };
            assert_eq!(rc, SQL_ERROR, "target {target}");
            assert_eq!(last_state(&h), *b"HY003");
        }
        assert_eq!(bindings_len(&h), 0);
    }

    /// SQL_UNBIND drops every binding; mssql-python calls it before each fetch.
    /// Per spec this also resets `SQL_DESC_COUNT` on the ARD to 0 — a real
    /// truncation, not just a data-pointer clear.
    #[test]
    fn free_stmt_unbind_clears_every_binding() {
        let h = TestHandles::with_env_dbc_stmt();
        let mut buf = [0i32; 4];
        for col in 1..=3 {
            unsafe {
                sql_bind_col(
                    h.stmt,
                    col,
                    SQL_C_SLONG,
                    buf.as_mut_ptr() as SqlPointer,
                    0,
                    ptr::null_mut(),
                )
            };
        }
        assert_eq!(bindings_len(&h), 3);
        assert_eq!(record_count(&h), 3);

        let rc = unsafe { sql_free_stmt_unbind(h.stmt) };
        assert_eq!(rc, SQL_SUCCESS);
        assert_eq!(bindings_len(&h), 0);
        assert_eq!(
            record_count(&h),
            0,
            "SQL_UNBIND must reset SQL_DESC_COUNT to 0"
        );
        // Unbinding again is a no-op rather than an error.
        assert_eq!(unsafe { sql_free_stmt_unbind(h.stmt) }, SQL_SUCCESS);
    }

    /// A fetch writes through the buffers it snapshotted after releasing the
    /// statement lock, so rebinding mid-fetch could free one under it. Both
    /// mutating entry points refuse rather than race.
    /// The deprecated 2.x date/time spellings are still legal for a 3.x
    /// application, and SQLBindParameter already accepts them; the two paths
    /// share `type_rules` so they cannot drift apart on this.
    #[test]
    fn deprecated_2x_date_types_are_accepted_and_canonicalized() {
        for (passed, canonical) in [
            (SQL_C_DATE, SQL_C_TYPE_DATE),
            (SQL_C_TIME, SQL_C_TYPE_TIME),
            (SQL_C_TIMESTAMP, SQL_C_TYPE_TIMESTAMP),
        ] {
            let h = TestHandles::with_env_dbc_stmt();
            let mut buf = [0u8; 32];
            let rc = unsafe {
                sql_bind_col(
                    h.stmt,
                    1,
                    passed,
                    buf.as_mut_ptr() as SqlPointer,
                    buf.len() as SqlLen,
                    ptr::null_mut(),
                )
            };
            assert_eq!(rc, SQL_SUCCESS, "binding {passed} must be accepted");

            // Storing the canonical form keeps element_stride and deliver_bound
            // on one spelling per type.
            assert_eq!(bindings(&h)[0].target_type, canonical);
        }
    }

    /// A C type that names a real ODBC type but that the fetch cannot deliver
    /// belongs to the per-row path (07006 / HYC00), not to this HY003 gate.
    #[test]
    fn valid_but_undeliverable_c_types_pass_the_bind_gate() {
        for c_type in [SQL_C_NUMERIC, SQL_C_INTERVAL_YEAR] {
            let h = TestHandles::with_env_dbc_stmt();
            let mut buf = [0u8; 32];
            let rc = unsafe {
                sql_bind_col(
                    h.stmt,
                    1,
                    c_type,
                    buf.as_mut_ptr() as SqlPointer,
                    buf.len() as SqlLen,
                    ptr::null_mut(),
                )
            };
            assert_eq!(rc, SQL_SUCCESS, "c_type {c_type} must pass the bind gate");
        }
    }

    #[test]
    fn binding_is_refused_while_a_fetch_is_in_progress() {
        let h = TestHandles::with_env_dbc_stmt();
        {
            let stmt = unsafe { handle_from_raw::<StmtHandle>(h.stmt) };
            let mut s = stmt.inner.lock().unwrap();
            s.set_state(STMT_STATE_FETCH_IN_PROGRESS);
        }
        let mut buf = [0i32; 1];
        let rc = unsafe {
            sql_bind_col(
                h.stmt,
                1,
                SQL_C_SLONG,
                buf.as_mut_ptr() as SqlPointer,
                0,
                ptr::null_mut(),
            )
        };
        assert_eq!(rc, SQL_ERROR);
        assert_eq!(last_state(&h), *b"HY010");

        assert_eq!(unsafe { sql_free_stmt_unbind(h.stmt) }, SQL_ERROR);
        assert_eq!(last_state(&h), *b"HY010");
    }

    #[test]
    fn free_stmt_unbind_rejects_a_null_handle() {
        assert_eq!(
            unsafe { sql_free_stmt_unbind(ptr::null_mut()) },
            SQL_INVALID_HANDLE
        );
    }

    /// After `SQLSetStmtAttrW` reassociates the ARD, `SQLBindCol` must write
    /// through to the *new* explicit descriptor, not the implicit one it
    /// replaced. `set_stmt_attr.rs`'s own tests only check the association
    /// state (`SQLGetStmtAttrW` reads it back); this is the actual
    /// bind-through-reassociation path AB#47437 exists to guarantee.
    #[test]
    fn bind_col_writes_through_a_reassociated_ard() {
        let mut h = TestHandles::with_env_dbc_stmt();
        let explicit_ard = h.alloc_explicit_desc();
        assert_eq!(
            unsafe {
                crate::api::set_stmt_attr::sql_set_stmt_attr_w(
                    h.stmt,
                    crate::api::odbc_types::SQL_ATTR_APP_ROW_DESC,
                    explicit_ard as SqlPointer,
                    0,
                )
            },
            SQL_SUCCESS
        );

        let mut buf = [0i32; 1];
        let rc = unsafe {
            sql_bind_col(
                h.stmt,
                1,
                SQL_C_SLONG,
                buf.as_mut_ptr() as SqlPointer,
                0,
                ptr::null_mut(),
            )
        };
        assert_eq!(rc, SQL_SUCCESS);

        let explicit = unsafe { handle_from_raw::<DescHandle>(explicit_ard) };
        assert_eq!(
            explicit.inner.lock().unwrap().records.len(),
            1,
            "the bind must land on the reassociated descriptor"
        );
        let implicit = unsafe { handle_from_raw::<DescHandle>(h.ard()) };
        assert_eq!(
            implicit.inner.lock().unwrap().records.len(),
            0,
            "the implicit ARD it replaced must be untouched"
        );
    }

    /// After `SQLSetStmtAttrW` reassociates the ARD, `SQLFreeStmt(SQL_UNBIND)`
    /// must clear bindings on the *new* explicit descriptor, not the
    /// implicit one it replaced — the unbind-side counterpart of
    /// `bind_col_writes_through_a_reassociated_ard` above.
    #[test]
    fn free_stmt_unbind_clears_a_reassociated_ard() {
        let mut h = TestHandles::with_env_dbc_stmt();
        let explicit_ard = h.alloc_explicit_desc();
        assert_eq!(
            unsafe {
                crate::api::set_stmt_attr::sql_set_stmt_attr_w(
                    h.stmt,
                    crate::api::odbc_types::SQL_ATTR_APP_ROW_DESC,
                    explicit_ard as SqlPointer,
                    0,
                )
            },
            SQL_SUCCESS
        );

        let mut buf = [0i32; 1];
        assert_eq!(
            unsafe {
                sql_bind_col(
                    h.stmt,
                    1,
                    SQL_C_SLONG,
                    buf.as_mut_ptr() as SqlPointer,
                    0,
                    ptr::null_mut(),
                )
            },
            SQL_SUCCESS
        );
        let explicit = unsafe { handle_from_raw::<DescHandle>(explicit_ard) };
        assert_eq!(explicit.inner.lock().unwrap().records.len(), 1);

        assert_eq!(unsafe { sql_free_stmt_unbind(h.stmt) }, SQL_SUCCESS);
        assert_eq!(
            explicit.inner.lock().unwrap().records.len(),
            0,
            "unbind must clear the reassociated descriptor, not the implicit one"
        );
    }

    /// Freeing the explicit descriptor currently associated as the ARD
    /// resets the statement back to its implicit ARD (`free_desc`'s existing
    /// association-reset logic, `free_handle.rs`) — and a *subsequent*
    /// `SQLBindCol` must write through to that implicit descriptor rather
    /// than erroring or touching the now-freed one.
    #[test]
    fn bind_col_falls_back_to_the_implicit_ard_after_the_explicit_one_is_freed() {
        let mut h = TestHandles::with_env_dbc_stmt();
        let explicit_ard = h.alloc_explicit_desc();
        unsafe {
            crate::api::set_stmt_attr::sql_set_stmt_attr_w(
                h.stmt,
                crate::api::odbc_types::SQL_ATTR_APP_ROW_DESC,
                explicit_ard as SqlPointer,
                0,
            )
        };
        assert_eq!(h.free_explicit_desc(explicit_ard), SQL_SUCCESS);

        let mut buf = [0i32; 1];
        let rc = unsafe {
            sql_bind_col(
                h.stmt,
                1,
                SQL_C_SLONG,
                buf.as_mut_ptr() as SqlPointer,
                0,
                ptr::null_mut(),
            )
        };
        assert_eq!(
            rc, SQL_SUCCESS,
            "must fall back to the implicit ARD, not error or touch freed memory"
        );
        assert_eq!(bindings_len(&h), 1);
    }

    /// The narrow race `bind_ard_column`'s liveness check guards against:
    /// `effective_ard` resolves an explicit descriptor under the STMT lock,
    /// which is dropped before the descriptor is actually locked and
    /// written. If a concurrent `SQLFreeHandle(SQL_HANDLE_DESC)` completes in
    /// that window, the stale pointer must fail cleanly (`Err`), not
    /// dereference freed memory. Calling the helper directly with an
    /// already-freed handle reproduces the state that window leaves behind.
    #[test]
    fn bind_ard_column_fails_cleanly_on_a_freed_descriptor() {
        let mut h = TestHandles::with_env_dbc_stmt();
        let explicit_ard = h.alloc_explicit_desc();
        assert_eq!(h.free_explicit_desc(explicit_ard), SQL_SUCCESS);

        let binding = ColumnBinding {
            column_number: 1,
            target_type: SQL_C_SLONG,
            target_value_ptr: 0x1 as SqlPointer,
            buffer_length: 4,
            strlen_or_ind_ptr: ptr::null_mut(),
            octet_length_ptr: ptr::null_mut(),
        };
        assert!(bind_ard_column(explicit_ard, binding).is_err());
    }

    /// Same race, same guard, for `unbind_ard_column`.
    #[test]
    fn unbind_ard_column_fails_cleanly_on_a_freed_descriptor() {
        let mut h = TestHandles::with_env_dbc_stmt();
        let explicit_ard = h.alloc_explicit_desc();
        assert_eq!(h.free_explicit_desc(explicit_ard), SQL_SUCCESS);
        assert!(unbind_ard_column(explicit_ard, 1).is_err());
    }
}
