// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Implementation of SQLExecDirectW — execute a SQL statement directly.

use tracing::{debug, error};

use std::time::Instant;

use mssql_tds::connection::tds_client::{ExecuteOptions, StreamedParamStatus};

use super::exec_common::{
    ParamsWithDae, build_named_params, claim_connection, deduct_query_timeout, fail_with_tds,
    finish_execute, flush_pending_unprepare, park_dae_client, query_timeout_expired_error,
    snapshot_bound_params,
};
use super::sqlstate::*;
use super::txn::begin_transaction_if_manual;
use super::util::{read_utf16, rewrite_param_markers};
use crate::api::odbc_types::{
    SQL_ERROR, SQL_INVALID_HANDLE, SqlHandle, SqlReturn, SqlSmallInt, SqlWChar,
};
use crate::error::free_errors;
use crate::error::post_sql_error;
use crate::handles::stmt::{
    STMT_STATE_CURSOR_OPEN, STMT_STATE_EXEC_CONTEXT, STMT_STATE_EXEC_STARTED, STMT_STATE_PREPARED,
};
use crate::handles::{HandleType, StmtHandle, handle_from_raw};

/// Implementation of `SQLExecDirectW`.
///
/// Executes a SQL statement directly on the connection associated with `statement_handle`.
/// `SQLFreeStmt(SQL_CLOSE)` to drain the wire and release the connection.
///
/// # Safety
/// - `statement_handle` must be a valid `StmtHandle` allocated by `SQLAllocHandle`.
/// - `statement_text` must point to a valid UTF-16 buffer readable for `text_length` characters.
///   If `text_length` is `SQL_NTS`, the string must be NUL-terminated.
/// - For each non-data-at-execution parameter, the currently bound value,
///   indicator, and octet-length buffers must remain readable according to the
///   bound C type and lengths. When `SQL_ATTR_PARAM_BIND_OFFSET_PTR` is
///   non-null, these readable extents begin at each bound base plus the
///   pointed-to signed byte offset, which may be negative, so every allocation
///   must cover that displaced range. The offset pointer itself must remain
///   readable for one `SqlLen`.
pub(crate) unsafe fn sql_exec_direct_w(
    statement_handle: SqlHandle,
    statement_text: *const SqlWChar,
    text_length: SqlSmallInt,
) -> SqlReturn {
    debug!(
        ?statement_handle,
        ?statement_text,
        text_length,
        "SQLExecDirectW called",
    );

    crate::ffi_entry!("SQLExecDirectW", unsafe {
        sql_exec_direct_w_impl(statement_handle, statement_text, text_length)
    })
}

/// # Safety
/// `statement_handle` must be null or point to a live `StmtHandle`.
/// `statement_text` must be readable for `text_length` UTF-16 code units, or
/// through a NUL terminator when `text_length` is `SQL_NTS`.
/// For each non-data-at-execution parameter, the currently bound value,
/// indicator, and octet-length buffers must remain readable according to the
/// bound C type and lengths. When `SQL_ATTR_PARAM_BIND_OFFSET_PTR` is non-null,
/// these readable extents begin at each bound base plus the pointed-to signed
/// byte offset, which may be negative, so every allocation must cover that
/// displaced range. The offset pointer itself must remain readable for one
/// `SqlLen`.
unsafe fn sql_exec_direct_w_impl(
    statement_handle: SqlHandle,
    statement_text: *const SqlWChar,
    text_length: SqlSmallInt,
) -> SqlReturn {
    if statement_handle.is_null() {
        error!("SQLExecDirectW: statement_handle is null");
        return SQL_INVALID_HANDLE;
    }

    let stmt = unsafe { handle_from_raw::<StmtHandle>(statement_handle) };
    debug_assert_eq!(
        stmt.object_type,
        HandleType::Stmt,
        "SQLExecDirectW: handle is not a STMT"
    );

    // The DM rejects null statement_text before calling the driver; see SQLExecDirect spec.
    debug_assert!(
        !statement_text.is_null(),
        "SQLExecDirectW: statement_text is null — DM should have rejected this"
    );

    let sql = unsafe { read_utf16(statement_text, text_length) };
    sql_exec_direct_w_safe(statement_handle, stmt, sql)
}

fn sql_exec_direct_w_safe(
    statement_handle: SqlHandle,
    stmt: &StmtHandle,
    sql: String,
) -> SqlReturn {
    debug!(sql = %sql, "SQLExecDirectW: executing");

    let dbc = stmt.parent_dbc();

    // Snapshotted before the STMT lock below is taken — this crate never
    // holds a STMT lock while acquiring a DESC lock (see bind_col.rs's
    // rationale). Not applied to `stmt_state.bound_params` until the
    // early-return checks below have passed, so a rejected re-entry during
    // an active DAE sequence can't clobber that sequence's own snapshot.
    let Ok(bound_params) = snapshot_bound_params(stmt) else {
        error!("SQLExecDirectW: failed to snapshot parameter bindings");
        if let Ok(mut stmt_state) = stmt.inner.lock() {
            // Cleared first so this diagnostic lands as record 1, not
            // appended after whatever a previous call left behind.
            free_errors(&mut stmt_state);
            post_sql_error(
                &mut stmt_state,
                SQLSTATE_HY000,
                0,
                "Internal error reading parameter bindings",
            );
        }
        return SQL_ERROR;
    };

    // Check STMT state, gather parameter values, and reset prior context.
    let (named_params, rewritten_sql, marker_count, query_timeout) = {
        let Ok(mut stmt_state) = stmt.inner.lock() else {
            error!("SQLExecDirectW: stmt mutex poisoned");
            return SQL_ERROR;
        };
        free_errors(&mut stmt_state);
        // A statement awaiting data-at-execution input is in the ODBC "Need
        // Data" state, where anything but SQLPutData/SQLParamData/SQLCancel is a
        // sequence error rather than the cursor error a merely-busy statement
        // gets.
        if stmt_state.needs_data() {
            error!("SQLExecDirectW: statement is awaiting data-at-execution input");
            post_diag(&mut stmt_state, ERR_FUNCTION_SEQUENCE);
            return SQL_ERROR;
        }
        if stmt_state.has_state(STMT_STATE_EXEC_STARTED | STMT_STATE_CURSOR_OPEN) {
            error!("SQLExecDirectW: statement has an active execute or open cursor");
            post_diag(&mut stmt_state, ERR_INVALID_CURSOR_STATE);
            return SQL_ERROR;
        }
        stmt_state.bound_params = bound_params;
        // Rewrite markers and read the bound parameter buffers before mutating
        // any state, so a binding error (07002 / HYC00) leaves the statement
        // unchanged.
        let (rewritten_sql, marker_count) = rewrite_param_markers(&sql);
        let named_params =
            match unsafe { build_named_params(&mut stmt_state, marker_count, "SQLExecDirectW") } {
                Ok(params) => params,
                Err(rc) => return rc,
            };
        // A new execute invalidates prior metadata/context immediately, so a
        // later execute failure cannot expose stale SQLNumResultCols/DescribeCol state.
        stmt_state.clear_state(STMT_STATE_EXEC_CONTEXT);
        stmt_state.column_metadata.clear();
        stmt_state.reset_row_stream();
        stmt_state.row_count = -1;
        stmt_state.pending_row_counts.clear();
        // Superseding a prepared plan orphans its server handle; release it
        // (deferred) once we hold the client below.
        stmt_state.orphan_prepared_handle();
        stmt_state.prepared = None;
        stmt_state.parameter_metadata.clear();
        stmt_state.clear_state(STMT_STATE_PREPARED);
        stmt_state.set_state(STMT_STATE_EXEC_STARTED);
        (
            named_params,
            rewritten_sql,
            marker_count,
            stmt_state.query_timeout,
        )
    };

    let ParamsWithDae { params, dae_params } = named_params;

    let mut client = match claim_connection(dbc, stmt, statement_handle, "SQLExecDirectW") {
        Ok(client) => client,
        Err(rc) => return rc,
    };
    let budget = query_timeout;
    let started = Instant::now();

    // Release any handle orphaned by the reset above before running the batch.
    // Bounded by the full budget: nothing has run yet to charge against it.
    flush_pending_unprepare(dbc, stmt, &mut client, "SQLExecDirectW", query_timeout);

    // `query_timeout` (SQL_ATTR_QUERY_TIMEOUT) bounds every wire operation this
    // call makes, not just the final execute — matching msodbcsql's
    // `DropPrepHandle` / `CheckOptions`, which charge the same deducted budget
    // to the deferred `sp_unprepare` and the implicit transaction begin. Each
    // step's remaining allowance is `budget` minus the *cumulative* elapsed
    // time since this call began (`started` is fixed, never re-seeded), so
    // every step's cost is charged exactly once against the original budget,
    // and sub-second remainders accumulate across steps instead of each being
    // floored away independently — matching msodbcsql's own millisecond-
    // granularity deduction (`dwQueryTimeoutInMS` in `DropPrepHandle`). An
    // already-exhausted budget fails immediately with HYT00 rather than
    // sending the next step unbounded.
    let query_timeout = match deduct_query_timeout(budget, started.elapsed()) {
        Ok(remaining) => remaining,
        Err(()) => {
            return fail_with_tds(
                dbc,
                stmt,
                statement_handle,
                client,
                &query_timeout_expired_error(),
            );
        }
    };

    if let Err(e) = begin_transaction_if_manual(dbc, &mut client, "SQLExecDirectW", query_timeout) {
        return fail_with_tds(dbc, stmt, statement_handle, client, &e);
    }

    let query_timeout = match deduct_query_timeout(budget, started.elapsed()) {
        Ok(remaining) => remaining,
        Err(()) => {
            return fail_with_tds(
                dbc,
                stmt,
                statement_handle,
                client,
                &query_timeout_expired_error(),
            );
        }
    };

    // Data-at-execution parameters park the half-written RPC on the statement
    // and hand control to SQLParamData / SQLPutData. There is no prepared plan
    // to restore afterwards, so `None` is passed for it.
    if !dae_params.is_empty() {
        let begin_result = dbc.runtime.block_on(client.begin_sp_executesql(
            rewritten_sql,
            params,
            ExecuteOptions::new().timeout_secs(query_timeout),
        ));
        return match begin_result {
            // Defensive: staging only reports DAE parameters when at least one
            // placeholder is present, so the TDS layer should not complete here.
            Ok(StreamedParamStatus::Complete(_)) => {
                error!(
                    dae_param_count = dae_params.len(),
                    "SQLExecDirectW: begin_sp_executesql completed despite data-at-execution parameters"
                );
                finish_execute(dbc, stmt, statement_handle, client, "SQLExecDirectW")
            }
            Ok(StreamedParamStatus::NeedData { .. }) => {
                park_dae_client(stmt, client, None, None, dae_params, "SQLExecDirectW")
            }
            Err(e) => {
                error!(%e, "SQLExecDirectW: begin_sp_executesql failed");
                fail_with_tds(dbc, stmt, statement_handle, client, &e)
            }
        };
    }

    // Parameterized text runs via sp_executesql (direct execution, no cached
    // handle); unparameterized text runs as a plain SQL batch. Neither DBC nor
    // STMT lock is held during I/O. `query_timeout` (already deducted above)
    // bounds either call; `0` means unlimited, matching the ODBC default.
    let exec_result: Result<(), mssql_tds::error::Error> = if marker_count > 0 {
        dbc.runtime
            .block_on(client.execute_sp_executesql(
                rewritten_sql,
                params,
                ExecuteOptions::new().timeout_secs(query_timeout),
            ))
            .map(|_| ())
    } else {
        // Statement-wise navigation: position on the batch's first statement
        // (msodbcsql parity) so no-row statements (PRINT / RAISERROR / DML) are
        // individually navigable via SQLMoreResults. finish_execute inspects the
        // resulting client state.
        dbc.runtime
            .block_on(client.execute(sql, ExecuteOptions::new().timeout_secs(query_timeout)))
            .map(|_| ())
    };
    if let Err(e) = exec_result {
        error!(%e, "SQLExecDirectW: execution failed");
        return fail_with_tds(dbc, stmt, statement_handle, client, &e);
    }

    finish_execute(dbc, stmt, statement_handle, client, "SQLExecDirectW")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::odbc_types::{SQL_NTS, SQL_NULL_HANDLE};
    use crate::handles::DescHandle;
    use crate::test_support::TestHandles;

    #[test]
    fn null_handle_returns_invalid_handle() {
        let sql: Vec<u16> = "SELECT 1"
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect();
        let ret = unsafe { sql_exec_direct_w(SQL_NULL_HANDLE, sql.as_ptr(), SQL_NTS) };
        assert_eq!(ret, SQL_INVALID_HANDLE);
    }

    #[test]
    fn null_statement_text_returns_error() {
        let h = TestHandles::with_env_dbc_stmt();

        let ret = unsafe { sql_exec_direct_w(h.stmt, std::ptr::null(), SQL_NTS) };
        assert_eq!(ret, SQL_ERROR);
    }

    #[test]
    fn disconnected_dbc_returns_error() {
        let h = TestHandles::with_env_dbc_stmt();

        let sql: Vec<u16> = "SELECT 1"
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect();
        let ret = unsafe { sql_exec_direct_w(h.stmt, sql.as_ptr(), SQL_NTS) };
        // DBC is not connected
        assert_eq!(ret, SQL_ERROR);
    }

    /// A statement awaiting `SQLPutData` is in the Need Data state, where the
    /// spec calls anything but SQLPutData/SQLParamData/SQLCancel a sequence
    /// error. Without the dedicated guard this falls through to the
    /// cursor-state check and reports 24000 instead.
    #[test]
    fn exec_direct_during_need_data_posts_hy010() {
        let h = TestHandles::with_env_dbc_stmt();
        let stmt = unsafe { handle_from_raw::<StmtHandle>(h.stmt) };
        {
            let mut state = stmt.inner.lock().unwrap();
            state.set_state(STMT_STATE_EXEC_STARTED);
            state.dae = Some(crate::handles::stmt::DaeState::for_test(Vec::new(), None));
        }

        let sql: Vec<u16> = "SELECT 1"
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect();
        assert_eq!(
            unsafe { sql_exec_direct_w(h.stmt, sql.as_ptr(), SQL_NTS) },
            SQL_ERROR
        );

        let state = stmt.inner.lock().unwrap();
        assert_eq!(state.diag_records[0].sql_state, SQLSTATE_HY010);
    }

    #[test]
    fn exec_direct_clears_stale_pending_row_counts_even_on_failure() {
        let h = TestHandles::with_env_dbc_stmt();
        let stmt = unsafe { handle_from_raw::<StmtHandle>(h.stmt) };
        // Simulate a prior pure-DML batch that left per-statement counts queued.
        {
            let mut state = stmt.inner.lock().unwrap();
            state.row_count = 3;
            state.pending_row_counts = std::collections::VecDeque::from(vec![2, 1]);
        }

        let sql: Vec<u16> = "SELECT 1"
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect();
        // Fails (not connected), but the stale row-count state must be cleared
        // at execute start so a later SQLMoreResults can't surface it.
        assert_eq!(
            unsafe { sql_exec_direct_w(h.stmt, sql.as_ptr(), SQL_NTS) },
            SQL_ERROR
        );

        let state = stmt.inner.lock().unwrap();
        assert_eq!(state.row_count, -1);
        assert!(state.pending_row_counts.is_empty());
    }

    #[test]
    fn exec_direct_clears_stale_prepared_plan() {
        use mssql_tds::connection::tds_client::PreparedStatement;

        use crate::handles::stmt::PreparedPlan;

        let h = TestHandles::with_env_dbc_stmt();
        let stmt = unsafe { handle_from_raw::<StmtHandle>(h.stmt) };
        {
            let mut state = stmt.inner.lock().unwrap();
            state.prepared = Some(PreparedPlan {
                stmt: PreparedStatement::materialized_for_test(
                    "SELECT 1",
                    mssql_tds::connection::tds_client::StatementId::from_raw_for_test(42),
                ),
                marker_count: 0,
            });
            state.set_state(STMT_STATE_PREPARED);
        }

        let sql: Vec<u16> = "SELECT 2"
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect();
        // Fails (not connected), but the prepared plan must already be reset.
        assert_eq!(
            unsafe { sql_exec_direct_w(h.stmt, sql.as_ptr(), SQL_NTS) },
            SQL_ERROR
        );

        let state = stmt.inner.lock().unwrap();
        assert!(state.prepared.is_none());
        assert!(!state.has_state(STMT_STATE_PREPARED));
        // The superseded handle is queued for sp_unprepare. The flush never ran
        // here because the connection claim failed, so it remains pending.
        let orphaned = state
            .pending_unprepare
            .expect("superseded handle queued for release");
        assert_eq!(
            orphaned,
            mssql_tds::connection::tds_client::StatementId::from_raw_for_test(42)
        );
    }

    #[test]
    fn unbound_parameter_marker_returns_07002() {
        let h = TestHandles::with_env_dbc_stmt();
        // SQL has one marker but no parameter is bound; the failure must be
        // posted before any state mutation and before the connection claim.
        let sql: Vec<u16> = "SELECT ? AS v"
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect();
        let ret = unsafe { sql_exec_direct_w(h.stmt, sql.as_ptr(), SQL_NTS) };
        assert_eq!(ret, SQL_ERROR);
        let stmt = unsafe { handle_from_raw::<StmtHandle>(h.stmt) };
        let state = stmt.inner.lock().unwrap();
        assert_eq!(state.diag_records[0].sql_state, SQLSTATE_07002);
        // A binding error must leave the statement unchanged — no EXEC_STARTED.
        assert!(!state.has_state(STMT_STATE_EXEC_STARTED));
    }

    /// Panics while holding the APD lock, leaving the mutex poisoned —
    /// mirrors `bind_param.rs`'s own `poison_apd` test helper.
    fn poison_apd(apd: crate::api::odbc_types::SqlHandle) {
        let handle = unsafe { handle_from_raw::<DescHandle>(apd) };
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _guard = handle.inner.lock().unwrap();
            panic!("poison the apd lock");
        }));
    }

    /// A `snapshot_bound_params` failure (here, a poisoned APD) must still
    /// post an HY000 diagnostic, and post it as record 1 — not leave
    /// `SQLGetDiagRec` reporting `SQL_NO_DATA`, and not append after a stale
    /// record a previous call left behind (`free_errors` must run first).
    #[test]
    fn snapshot_failure_posts_hy000_as_the_first_diagnostic_record() {
        let h = TestHandles::with_env_dbc_stmt();
        let stmt = unsafe { handle_from_raw::<StmtHandle>(h.stmt) };
        stmt.inner
            .lock()
            .unwrap()
            .diag_records
            .push(crate::error::DiagRecord::new(SQLSTATE_07002, 0, "stale"));
        poison_apd(h.apd());

        let sql: Vec<u16> = "SELECT 1"
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect();
        let ret = unsafe { sql_exec_direct_w(h.stmt, sql.as_ptr(), SQL_NTS) };
        assert_eq!(ret, SQL_ERROR);

        let state = stmt.inner.lock().unwrap();
        assert_eq!(state.diag_records.len(), 1, "stale record must be cleared");
        assert_eq!(state.diag_records[0].sql_state, SQLSTATE_HY000);
        assert!(
            state.diag_records[0]
                .message
                .contains("Internal error reading parameter bindings")
        );
    }

    /// A plain batch whose first statement is a no-row result (DML row count)
    /// followed by more statements leaves the cursor open with zero columns and
    /// the connection busy, so SQLMoreResults can advance past it (msodbcsql
    /// statement-wise parity). Exercises the `finish_execute` no-row branch.
    #[test]
    fn exec_direct_norow_statement_keeps_cursor_open_and_busy() {
        use crate::api::odbc_types::SQL_SUCCESS;
        use crate::handles::dbc::DbcHandle;
        use mssql_tds::test_client_support::{
            col_metadata_empty, done_more_with_count, done_no_more, tds_client_from_tokens,
        };

        let h = TestHandles::with_env_dbc_stmt();
        h.mark_dbc_connected();
        let dbc = unsafe { handle_from_raw::<DbcHandle>(h.dbc) };
        // Batch response: a DML statement (row count + MORE) then a trailing
        // SELECT. The first statement surfaces as a no-row result with the batch
        // still open.
        let client = tds_client_from_tokens(vec![
            done_more_with_count(5),
            col_metadata_empty(),
            done_no_more(),
        ]);
        {
            let mut ds = dbc.inner.lock().unwrap();
            ds.client = Some(client);
            // active_stmt stays None => connection idle and claimable.
        }

        let stmt = unsafe { handle_from_raw::<StmtHandle>(h.stmt) };
        let ret = sql_exec_direct_w_safe(h.stmt, stmt, "UPDATE t SET x = 1; SELECT 1".to_string());
        assert_eq!(ret, SQL_SUCCESS);

        let ss = stmt.inner.lock().unwrap();
        assert!(ss.has_state(STMT_STATE_CURSOR_OPEN));
        assert!(ss.column_metadata.is_empty());
        drop(ss);

        // Connection stays busy on this statement with the client returned.
        let ds = dbc.inner.lock().unwrap();
        assert_eq!(ds.active_stmt, Some(h.stmt));
        assert!(ds.client.is_some());
    }

    /// `SQL_ATTR_QUERY_TIMEOUT` must actually bound the wait for a response,
    /// not just reach `ExecuteOptions` — see mssql-rs#439, where the timeout
    /// was silently dropped on the floor instead of bounding a statement
    /// blocked server-side (e.g. behind another session's row lock).
    ///
    /// Drives the real `SQLExecDirectW` code path (`claim_connection`,
    /// `begin_transaction_if_manual`, the elapsed-time deduction, and the
    /// final `execute`) against a real `TdsClient` connected to a mock TDS
    /// server that holds its response for `RESPONSE_DELAY` — far longer than
    /// the statement's configured timeout. Reverting the timeout wiring back
    /// to `ExecuteOptions::default()` would make this test take the full
    /// `RESPONSE_DELAY` and return `SQL_SUCCESS`/`1222` instead of the prompt
    /// `HYT00` asserted here, so it fails if the plumbing regresses.
    #[test]
    fn exec_direct_query_timeout_bounds_a_longer_server_delay() {
        use crate::handles::dbc::DbcHandle;
        use mssql_mock_tds::{QueryResponse, TerminalError};
        use std::time::{Duration, Instant};

        const RESPONSE_DELAY: Duration = Duration::from_secs(8);
        const STMT_TIMEOUT_SECS: u32 = 1;
        // Comfortably above STMT_TIMEOUT_SECS plus connection/RTT overhead,
        // comfortably below RESPONSE_DELAY — the gap is what proves the
        // statement timeout, not the server delay, ended the wait.
        const BOUND: Duration = Duration::from_secs(5);
        const SELECT_SQL: &str = "SELECT * FROM ##t WHERE id = 1";

        let h = TestHandles::with_env_dbc_stmt();
        let dbc = unsafe { handle_from_raw::<DbcHandle>(h.dbc) };
        let _mock_server = crate::test_support::connect_mock_server(
            dbc,
            SELECT_SQL,
            QueryResponse::error_only(TerminalError::new(
                1222,
                16,
                "Lock request time out period exceeded.",
            ))
            .with_delay(RESPONSE_DELAY),
        );

        let stmt = unsafe { handle_from_raw::<StmtHandle>(h.stmt) };
        stmt.inner.lock().unwrap().query_timeout = STMT_TIMEOUT_SECS;

        let started = Instant::now();
        let ret = sql_exec_direct_w_safe(h.stmt, stmt, SELECT_SQL.to_string());
        let elapsed = started.elapsed();

        assert_eq!(ret, SQL_ERROR);
        assert!(
            elapsed < BOUND,
            "SQLExecDirectW took {elapsed:?} — a {STMT_TIMEOUT_SECS}s SQL_ATTR_QUERY_TIMEOUT \
             must bound the wait well below the server's {RESPONSE_DELAY:?} delay"
        );
        let state = stmt.inner.lock().unwrap();
        assert_eq!(
            state.diag_records[0].sql_state, *b"HYT00",
            "a query-timeout expiry must report HYT00, got {:?}",
            state.diag_records[0].sql_state
        );
    }

    /// `SQL_ATTR_QUERY_TIMEOUT` must also bound the implicit transaction begin
    /// `begin_transaction_if_manual` sends before the statement itself when
    /// the connection is in manual-commit mode — mirroring msodbcsql's
    /// `CheckOptions`/`ExecTMRImmediate` (`sqlccmd.cpp:10572-10585`), which
    /// passes the statement's own query timeout to that TM request. Unlike
    /// `exec_direct_query_timeout_bounds_a_longer_server_delay` above (which
    /// delays the query response), this delays only the server's answer to
    /// the Begin request, via the mock server's reserved
    /// `TM_BEGIN_DELAY_KEY`, so it fails if the timeout wiring into
    /// `begin_transaction_if_manual` regresses even though the query step
    /// itself is untouched.
    #[test]
    fn exec_direct_query_timeout_bounds_a_delayed_implicit_transaction_begin() {
        use crate::handles::dbc::DbcHandle;
        use mssql_mock_tds::QueryResponse;
        use std::time::{Duration, Instant};

        const BEGIN_DELAY: Duration = Duration::from_secs(8);
        const STMT_TIMEOUT_SECS: u32 = 1;
        // Comfortably above STMT_TIMEOUT_SECS plus connection/RTT overhead,
        // comfortably below BEGIN_DELAY — the gap is what proves the
        // statement timeout, not the server delay, ended the wait.
        const BOUND: Duration = Duration::from_secs(5);
        const SELECT_SQL: &str = "SELECT * FROM ##t WHERE id = 1";

        let h = TestHandles::with_env_dbc_stmt();
        let dbc = unsafe { handle_from_raw::<DbcHandle>(h.dbc) };
        let mock_server =
            crate::test_support::connect_mock_server(dbc, SELECT_SQL, QueryResponse::select_one());
        mock_server.set_tm_begin_delay(BEGIN_DELAY);
        // Manual-commit mode with no transaction open yet is what makes
        // `begin_transaction_if_manual` send a real Begin request instead of
        // returning immediately.
        dbc.inner.lock().unwrap().autocommit = false;

        let stmt = unsafe { handle_from_raw::<StmtHandle>(h.stmt) };
        stmt.inner.lock().unwrap().query_timeout = STMT_TIMEOUT_SECS;

        let started = Instant::now();
        let ret = sql_exec_direct_w_safe(h.stmt, stmt, SELECT_SQL.to_string());
        let elapsed = started.elapsed();

        assert_eq!(ret, SQL_ERROR);
        assert!(
            elapsed < BOUND,
            "SQLExecDirectW took {elapsed:?} — a {STMT_TIMEOUT_SECS}s SQL_ATTR_QUERY_TIMEOUT \
             must bound the implicit transaction begin well below the server's \
             {BEGIN_DELAY:?} delay"
        );
        let state = stmt.inner.lock().unwrap();
        assert_eq!(
            state.diag_records[0].sql_state, *b"HYT00",
            "a query-timeout expiry must report HYT00, got {:?}",
            state.diag_records[0].sql_state
        );
    }

    /// SQL Server compiles variable assignment as a SQLSELECT command carrying
    /// DONE_COUNT. `SQLExecDirect` must skip past it and open the following row
    /// set, so an immediate `SQLFetch` succeeds instead of failing with 24000.
    #[test]
    fn exec_direct_skips_assignment_count_and_opens_rowset() {
        use crate::api::odbc_types::SQL_SUCCESS;
        use crate::handles::dbc::DbcHandle;
        use mssql_tds::test_client_support::{
            col_metadata, done_more_select_with_count, done_no_more, int_columns,
            tds_client_from_tokens,
        };

        let h = TestHandles::with_env_dbc_stmt();
        h.mark_dbc_connected();
        let dbc = unsafe { handle_from_raw::<DbcHandle>(h.dbc) };
        let client = tds_client_from_tokens(vec![
            done_more_select_with_count(1),
            col_metadata(int_columns(1)),
            done_no_more(),
        ]);
        dbc.inner.lock().unwrap().client = Some(client);

        let stmt = unsafe { handle_from_raw::<StmtHandle>(h.stmt) };
        let ret = sql_exec_direct_w_safe(
            h.stmt,
            stmt,
            "DECLARE @out int = 1; EXEC dbo.p;".to_string(),
        );

        assert_eq!(ret, SQL_SUCCESS);
        let state = stmt.inner.lock().unwrap();
        assert!(state.has_state(STMT_STATE_CURSOR_OPEN));
        assert_eq!(state.column_metadata.len(), 1);
        assert_eq!(state.row_count, -1);
    }

    /// A no-row statement that also produced a message surfaces its diagnostics
    /// with SQL_SUCCESS_WITH_INFO from the `finish_execute` no-row branch.
    #[test]
    fn exec_direct_norow_statement_with_message_returns_success_with_info() {
        use crate::api::odbc_types::SQL_SUCCESS_WITH_INFO;
        use crate::handles::dbc::DbcHandle;
        use mssql_tds::test_client_support::{
            col_metadata_empty, done_more_with_count, done_no_more, info, tds_client_from_tokens,
        };

        let h = TestHandles::with_env_dbc_stmt();
        h.mark_dbc_connected();
        let dbc = unsafe { handle_from_raw::<DbcHandle>(h.dbc) };
        let client = tds_client_from_tokens(vec![
            info(0, 0, "print in batch"),
            done_more_with_count(1),
            col_metadata_empty(),
            done_no_more(),
        ]);
        {
            let mut ds = dbc.inner.lock().unwrap();
            ds.client = Some(client);
        }

        let stmt = unsafe { handle_from_raw::<StmtHandle>(h.stmt) };
        let ret = sql_exec_direct_w_safe(
            h.stmt,
            stmt,
            "PRINT 'x'; UPDATE t SET x = 1; SELECT 1".to_string(),
        );
        assert_eq!(ret, SQL_SUCCESS_WITH_INFO);
        assert!(stmt.inner.lock().unwrap().has_state(STMT_STATE_CURSOR_OPEN));
    }

    /// A statement handle reused for a new execute after its previous cursor
    /// was fetched to exhaustion (AB#47508's `result_set_exhausted` flag) must
    /// not carry that flag — or a stale `pending_fetch_error` from that same
    /// previous result set — into the new result set. Otherwise the very
    /// first `SQLFetch` on the fresh cursor would wrongly report
    /// `SQL_NO_DATA`, or worse, wrongly fail with an error left over from an
    /// entirely different, already-finished query.
    #[test]
    fn exec_direct_row_returning_clears_a_stale_exhausted_flag() {
        use crate::api::odbc_types::SQL_SUCCESS;
        use crate::handles::dbc::DbcHandle;
        use mssql_tds::error::Error as TdsError;
        use mssql_tds::test_client_support::{col_metadata_empty, tds_client_from_tokens};

        let h = TestHandles::with_env_dbc_stmt();
        h.mark_dbc_connected();
        let dbc = unsafe { handle_from_raw::<DbcHandle>(h.dbc) };
        let client = tds_client_from_tokens(vec![col_metadata_empty()]);
        {
            let mut ds = dbc.inner.lock().unwrap();
            ds.client = Some(client);
        }
        let stmt = unsafe { handle_from_raw::<StmtHandle>(h.stmt) };
        {
            let mut ss = stmt.inner.lock().unwrap();
            ss.result_set_exhausted = true;
            ss.pending_fetch_error = Some(TdsError::ProtocolError("stale".to_string()));
        }

        let ret = sql_exec_direct_w_safe(h.stmt, stmt, "SELECT 1".to_string());

        assert_eq!(ret, SQL_SUCCESS);
        let ss = stmt.inner.lock().unwrap();
        assert!(!ss.result_set_exhausted);
        assert!(ss.pending_fetch_error.is_none());
    }

    /// **Blocking, found in review**: `finish_execute`'s pure-DML branch (no
    /// result set, wire fully drained) was the only one of its three
    /// terminal branches that did not reset `result_set_exhausted`,
    /// `batch_exhausted`, `pending_fetch_error`, and `pending_fetch_info` —
    /// the sibling "statement-wise navigation" and "row-returning" branches
    /// both do. Reusing a statement handle for a pure-DML re-execute after
    /// those were left stale by a previous query (e.g. a zero-row fetch that
    /// exhausted the whole batch and stashed a trailing INFO message) let
    /// that stale state leak forward: `SQLMoreResults`'s `batch_exhausted`
    /// fast path would post the *previous* query's INFO message — and a
    /// stale `pending_fetch_error` would fail — as if they belonged to the
    /// brand new query.
    #[test]
    fn exec_direct_pure_dml_clears_stale_exhausted_and_pending_info() {
        use crate::api::odbc_types::SQL_SUCCESS;
        use crate::handles::dbc::DbcHandle;
        use mssql_tds::error::{Error as TdsError, SqlInfoMessage};
        use mssql_tds::test_client_support::{done_no_more, tds_client_from_tokens};

        let h = TestHandles::with_env_dbc_stmt();
        h.mark_dbc_connected();
        let dbc = unsafe { handle_from_raw::<DbcHandle>(h.dbc) };
        // No COLMETADATA at all, terminal DONE: a pure-DML, last/only
        // statement in the batch — routes to finish_execute's third branch.
        let client = tds_client_from_tokens(vec![done_no_more()]);
        {
            let mut ds = dbc.inner.lock().unwrap();
            ds.client = Some(client);
        }
        let stmt = unsafe { handle_from_raw::<StmtHandle>(h.stmt) };
        {
            let mut ss = stmt.inner.lock().unwrap();
            // As if a previous query's zero-row fetch exhausted the whole
            // batch and stashed a trailing INFO message and error
            // (release_busy_if_row_exhausted), left over on the reused handle.
            ss.result_set_exhausted = true;
            ss.batch_exhausted = true;
            ss.pending_fetch_error = Some(TdsError::ProtocolError("stale".to_string()));
            ss.pending_fetch_info = vec![SqlInfoMessage {
                message: "previous query's PRINT output".to_string(),
                state: 1,
                class: 0,
                number: 0,
                server_name: None,
                proc_name: None,
                line_number: None,
            }];
        }

        let ret = sql_exec_direct_w_safe(h.stmt, stmt, "UPDATE t1 SET x = 1".to_string());

        assert_eq!(ret, SQL_SUCCESS);
        let ss = stmt.inner.lock().unwrap();
        assert!(!ss.result_set_exhausted);
        assert!(
            !ss.batch_exhausted,
            "must not fast-path this new query's SQLMoreResults to SQL_NO_DATA"
        );
        assert!(ss.pending_fetch_error.is_none());
        assert!(
            ss.pending_fetch_info.is_empty(),
            "the previous query's INFO message must not leak onto the new query"
        );
    }
}
