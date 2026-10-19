// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Implementation of SQLCloseCursor and the SQL_CLOSE path of SQLFreeStmt.
//!
//! Both operations close the cursor on a statement handle and release the
//! connection-level "busy" claim, allowing other statements on the same DBC
//! to execute.

use std::sync::MutexGuard;
use tracing::{debug, error};

use super::sqlstate::*;
use crate::api::exec_common::post_claim_error;
use crate::api::odbc_types::{
    SQL_ERROR, SQL_INVALID_HANDLE, SQL_SUCCESS, SQL_SUCCESS_WITH_INFO, SqlHandle, SqlReturn,
};
use crate::error::free_errors;
use crate::handles::stmt::{STMT_STATE_CURSOR_OPEN, STMT_STATE_EXEC_CONTEXT, StmtState};
use crate::handles::wire::{ClaimError, ClaimPolicy, LeaseReturn, WireOwner};
use crate::handles::{HandleType, StmtHandle, handle_from_raw, process_is_shutting_down};

/// Closes the cursor on `statement_handle` and discards any pending rows.
///
/// Mirrors msodbcsql's `SQLCloseCursor` → `SQLFreeStmt(SQL_CLOSE)` path.
/// Returns `SQL_ERROR` (SQLSTATE 24000) if no cursor is open, matching
/// msodbcsql's behaviour.
///
/// # Safety
/// `statement_handle` must be a valid `StmtHandle` or null.
pub(crate) unsafe fn sql_close_cursor(statement_handle: SqlHandle) -> SqlReturn {
    crate::ffi_entry!(
        "SQLCloseCursor",
        statement_handle,
        debug!(?statement_handle, "SQLCloseCursor called"),
        unsafe { sql_close_cursor_impl(statement_handle) }
    )
}

/// Implements the `SQL_CLOSE` option of `SQLFreeStmt` — closes the cursor
/// without dropping the statement handle or its bound parameters.
///
/// Unlike `SQLCloseCursor`, `SQLFreeStmt(SQL_CLOSE)` succeeds even when no
/// cursor is open (it is a no-op in that case).
///
/// # Safety
/// `statement_handle` must be a valid `StmtHandle` or null.
pub(crate) unsafe fn sql_free_stmt_close(statement_handle: SqlHandle) -> SqlReturn {
    crate::ffi_entry!(
        "SQLFreeStmt(SQL_CLOSE)",
        statement_handle,
        debug!(?statement_handle, "SQLFreeStmt(SQL_CLOSE) called"),
        unsafe { sql_free_stmt_close_impl(statement_handle) }
    )
}

/// # Safety
/// `statement_handle` must be null or point to a live `StmtHandle`.
unsafe fn sql_close_cursor_impl(statement_handle: SqlHandle) -> SqlReturn {
    if statement_handle.is_null() {
        error!("SQLCloseCursor: statement_handle is null");
        return SQL_INVALID_HANDLE;
    }
    let stmt = unsafe { handle_from_raw::<StmtHandle>(statement_handle) };
    debug_assert_eq!(stmt.object_type, HandleType::Stmt);
    sql_close_cursor_safe(statement_handle, stmt)
}

fn sql_close_cursor_safe(statement_handle: SqlHandle, stmt: &StmtHandle) -> SqlReturn {
    let _operation = match stmt.begin_operation() {
        Ok(operation) => operation,
        Err(rc) => return rc,
    };
    let Ok(mut stmt_state) = stmt.inner.lock() else {
        error!("SQLCloseCursor: stmt mutex poisoned");
        return SQL_ERROR;
    };
    free_errors(&mut stmt_state);
    if !stmt_state.has_state(STMT_STATE_CURSOR_OPEN) {
        error!("SQLCloseCursor: no cursor is open — SQLSTATE 24000");
        post_diag(&mut stmt_state, ERR_INVALID_CURSOR_STATE);
        return SQL_ERROR;
    }

    // A prior fetch's read-ahead peek can have discovered this cursor's
    // batch ends in a SQL Server error and deferred it here (see AB#47508's
    // release_busy_if_row_exhausted / StmtState::pending_fetch_error) rather
    // than losing it under that fetch's own SQL_SUCCESS return. The peek
    // already closed the batch on the wire, so the drain below can no
    // longer discover it there — take it now and surface it below instead
    // of silently closing as if the batch had never errored.
    let pending_fetch_error = stmt_state.pending_fetch_error.take();
    reset_cursor_state(&mut stmt_state);
    drop(stmt_state);

    let outcome = drain_and_release(stmt, statement_handle);
    if let Some(e) = pending_fetch_error {
        if let Ok(mut stmt_state) = stmt.inner.lock() {
            post_tds_error(&mut stmt_state, &e, SQLSTATE_HY000);
        }
        return SQL_ERROR;
    }

    match outcome {
        DrainOutcome::Failed => {
            error!("SQLCloseCursor: failed to drain TDS stream on close");
            SQL_ERROR
        }
        DrainOutcome::InfoPosted => {
            debug!("SQLCloseCursor: cursor closed");
            SQL_SUCCESS_WITH_INFO
        }
        DrainOutcome::Clean => {
            debug!("SQLCloseCursor: cursor closed");
            SQL_SUCCESS
        }
    }
}

/// # Safety
/// `statement_handle` must be null or point to a live `StmtHandle`.
unsafe fn sql_free_stmt_close_impl(statement_handle: SqlHandle) -> SqlReturn {
    if statement_handle.is_null() {
        error!("SQLFreeStmt(SQL_CLOSE): statement_handle is null");
        return SQL_INVALID_HANDLE;
    }
    let stmt = unsafe { handle_from_raw::<StmtHandle>(statement_handle) };
    debug_assert_eq!(stmt.object_type, HandleType::Stmt);
    sql_free_stmt_close_safe(statement_handle, stmt)
}

fn sql_free_stmt_close_safe(statement_handle: SqlHandle, stmt: &StmtHandle) -> SqlReturn {
    let _operation = match stmt.begin_operation() {
        Ok(operation) => operation,
        Err(rc) => return rc,
    };
    let Ok(stmt_state) = stmt.inner.lock() else {
        error!("SQLFreeStmt(SQL_CLOSE): stmt mutex poisoned");
        return SQL_ERROR;
    };
    free_stmt_close_locked(statement_handle, stmt, stmt_state, false)
}

/// The caller retains an operation or cancellation guard through the drain.
/// SQLCancel must not begin another operation while holding its cancellation fence.
pub(super) fn free_stmt_close_locked(
    statement_handle: SqlHandle,
    stmt: &StmtHandle,
    mut stmt_state: MutexGuard<'_, StmtState>,
    cancel_pending: bool,
) -> SqlReturn {
    free_errors(&mut stmt_state);
    // No-op if cursor is already closed.
    if !stmt_state.has_state(STMT_STATE_CURSOR_OPEN) {
        return SQL_SUCCESS;
    }

    // An exhausted response has already released the client to other DBC calls.
    // Parameter arrays still need their completion status published on close.
    let needs_drain = !stmt_state.batch_exhausted || stmt_state.parameter_array.is_some();
    // See the identical comment in sql_close_cursor_safe.
    let pending_fetch_error = stmt_state.pending_fetch_error.take();
    reset_cursor_state(&mut stmt_state);
    drop(stmt_state);

    let outcome = if needs_drain {
        drain_and_release_inner(
            stmt,
            statement_handle,
            process_is_shutting_down(),
            cancel_pending,
        )
    } else {
        DrainOutcome::Clean
    };
    if let Some(e) = pending_fetch_error {
        if let Ok(mut stmt_state) = stmt.inner.lock() {
            post_tds_error(&mut stmt_state, &e, SQLSTATE_HY000);
        }
        return SQL_ERROR;
    }

    match outcome {
        DrainOutcome::Failed => {
            error!("SQLFreeStmt(SQL_CLOSE): failed to drain TDS stream on close");
            SQL_ERROR
        }
        DrainOutcome::InfoPosted => {
            debug!("SQLFreeStmt(SQL_CLOSE): cursor closed");
            SQL_SUCCESS_WITH_INFO
        }
        DrainOutcome::Clean => {
            debug!("SQLFreeStmt(SQL_CLOSE): cursor closed");
            SQL_SUCCESS
        }
    }
}

/// Closes the cursor on a statement as part of a *connection*-scoped operation
/// (`SQLEndTran`, an autocommit switch, an isolation change, disconnect).
///
/// Reproduces what msodbcsql's sweep does to each statement. `CommitAbortTran`
/// calls `SQLFreeStmt(lpstmt, SQL_CLOSE)` on every statement it visits
/// (`sqlctran.cpp:302-323`), and that entry point calls `FreeErrors(lpstmt)`
/// before it looks at either the option or the cursor state
/// (`sqlccmd.cpp:379-380`). Statement diagnostics are therefore discarded even
/// on a statement that never opened a cursor — the failed-statement case — so
/// [`free_errors`] runs here unconditionally, ahead of the cursor check.
///
/// Kept separate from the public `SQLFreeStmt(SQL_CLOSE)` path only for cost:
/// statements with no open cursor return straight after the diagnostics reset,
/// with no FFI entry and no drain.
///
/// A pending fetch error (see the comment in `sql_close_cursor_safe`) is
/// still posted to the statement's own diagnostics here, but — unlike the
/// direct `SQLCloseCursor`/`SQLFreeStmt(SQL_CLOSE)` paths — does not fail
/// this function's own return: `SQL_ERROR` here specifically tells the
/// caller the stream failed to drain, so sending a transaction-manager
/// request next is unsafe, which an already-closed batch's stale diagnostic
/// does not make true.
pub(super) fn close_cursor_for_connection_op(stmt: &StmtHandle, handle: SqlHandle) -> SqlReturn {
    let _operation = match stmt.begin_operation() {
        Ok(operation) => operation,
        Err(rc) => return rc,
    };
    let pending_fetch_error = {
        let Ok(mut stmt_state) = stmt.inner.lock() else {
            error!("close_cursor_for_connection_op: stmt mutex poisoned");
            return SQL_ERROR;
        };
        free_errors(&mut stmt_state);
        if !stmt_state.has_state(STMT_STATE_CURSOR_OPEN) {
            return SQL_SUCCESS;
        }
        let pending_fetch_error = stmt_state.pending_fetch_error.take();
        reset_cursor_state(&mut stmt_state);
        pending_fetch_error
    };

    let outcome = drain_and_release(stmt, handle);
    if let Some(e) = pending_fetch_error
        && let Ok(mut stmt_state) = stmt.inner.lock()
    {
        post_tds_error(&mut stmt_state, &e, SQLSTATE_HY000);
    }

    match outcome {
        DrainOutcome::Failed => {
            error!("close_cursor_for_connection_op: failed to drain TDS stream");
            SQL_ERROR
        }
        DrainOutcome::InfoPosted | DrainOutcome::Clean => SQL_SUCCESS,
    }
}

/// Resets cursor state on the statement (cursor is no longer open, metadata cleared).
pub(super) fn reset_cursor_state(stmt_state: &mut crate::handles::stmt::StmtState) {
    stmt_state.clear_state(STMT_STATE_CURSOR_OPEN | STMT_STATE_EXEC_CONTEXT);
    stmt_state.reset_row_stream();
    stmt_state.clear_result_metadata();
    stmt_state.pending_row_counts.clear();
    stmt_state.clear_exhaustion_state();
}

/// Outcome of draining the TDS stream and releasing the connection on cursor close.
pub(super) enum DrainOutcome {
    /// Drain completed cleanly; no server INFO messages were posted.
    Clean,
    /// Drain completed and one or more server INFO messages were posted.
    InfoPosted,
    /// Draining the TDS stream failed (I/O error or a lost/poisoned client).
    /// A diagnostic is posted where possible; the connection may be broken.
    Failed,
}

/// Takes the TDS client from the DBC, drains any pending tokens, and releases the wire.
/// The wire stays reserved for this statement until the drain finishes so concurrent
/// threads see the connection as busy (HY000) throughout — not just until `client` is taken.
/// No locks are held during the network I/O.
///
/// Returns a [`DrainOutcome`] so callers can distinguish a clean close from one
/// that surfaced server INFO messages, and — importantly — from a drain failure,
/// which must not be reported to the app as success.
pub(super) fn drain_and_release(stmt: &StmtHandle, statement_handle: SqlHandle) -> DrainOutcome {
    drain_and_release_inner(stmt, statement_handle, process_is_shutting_down(), false)
}

/// The body of [`drain_and_release`], with the loader's shutdown flag passed in
/// rather than read. The skip arm below is unreachable in a live process, so
/// threading the flag is the only way a test can exercise it — without that,
/// deleting the arm leaves the suite green and restores the AB#47510 hang.
fn drain_and_release_inner(
    stmt: &StmtHandle,
    statement_handle: SqlHandle,
    process_is_shutting_down: bool,
    cancel_pending: bool,
) -> DrainOutcome {
    let dbc = stmt.parent_dbc();

    let (mut client, cancel_timeout) = {
        let Ok(mut dbc_state) = dbc.inner.lock() else {
            error!("drain_and_release: dbc mutex poisoned");
            return DrainOutcome::Failed;
        };
        // A fetch may have already released the wire early once this
        // statement's own result set was exhausted (AB#47508), independently
        // of `STMT_STATE_CURSOR_OPEN`, which callers here check instead. If
        // another owner has since taken the connection, this statement has
        // nothing left on the wire to drain — taking the client here would
        // steal and corrupt whatever that owner is doing. Claiming an idle wire
        // reserves it for the drain, so a concurrent execute sees it busy
        // rather than unowned with no client (AB#49225).
        match dbc_state.wire.claim(
            WireOwner::Statement(statement_handle),
            ClaimPolicy::IdleOrOwn,
        ) {
            Ok(lease) => (lease, dbc_state.cancel_timeout_secs()),
            Err(ClaimError::Busy(owner)) if owner != WireOwner::Statement(statement_handle) => {
                return DrainOutcome::Clean;
            }
            Err(claim_error) => {
                error!(
                    ?claim_error,
                    "drain_and_release: connection is not available"
                );
                post_claim_error(dbc_state, stmt, claim_error);
                return DrainOutcome::Failed;
            }
        }
    };

    // The drain is a server round-trip, so it needs the scheduler's worker to
    // drive the socket. During `DLL_PROCESS_DETACH` the OS has already
    // terminated that worker and `block_on` would park this thread forever
    // (AB#47510). Reached whenever a host frees a statement with an open cursor
    // from an `onexit` handler. Skipping costs nothing the exit does not
    // already cost: the undrained rows die with the connection, and the client
    // is still returned below so the DBC is left consistent for whatever
    // teardown runs after this.
    if process_is_shutting_down {
        debug!("drain_and_release: process is exiting — skipping the drain round-trip");
        dbc.return_lease(client, LeaseReturn::Release);
        return DrainOutcome::Clean;
    }

    let close_result = if cancel_pending && client.has_open_batch() {
        dbc.runtime.block_on(client.cancel_query(cancel_timeout))
    } else {
        dbc.runtime.block_on(client.close_query())
    };
    if let Err(e) = close_result {
        error!(%e, "drain_and_release: failed to drain TDS stream — connection may be broken");
        if !dbc.return_lease(client, LeaseReturn::Release) {
            return DrainOutcome::Failed;
        }
        // Surface the failure as a diagnostic so the app is not told the close
        // succeeded when the stream did not drain cleanly.
        if let Ok(mut stmt_state) = stmt.inner.lock() {
            post_tds_error(&mut stmt_state, &e, SQLSTATE_HY000);
        }
        return DrainOutcome::Failed;
    }

    let array_update = super::execute::take_parameter_array_update(stmt, &mut client);
    let info_messages = client.take_info_messages();

    // Drain complete: return client and release busy claim before publishing
    // anything derived from a session that may have been disconnected.
    if !dbc.return_lease(client, LeaseReturn::Release) {
        return DrainOutcome::Failed;
    }

    let array_rc = match array_update {
        Ok(update) => super::execute::apply_parameter_array_update(stmt, update),
        Err(rc) => rc,
    };
    let has_server_info = match stmt.inner.lock() {
        Ok(mut stmt_state) => post_tds_info_messages(&mut stmt_state, &info_messages),
        Err(_) => {
            error!("drain_and_release: stmt mutex poisoned while posting info messages");
            return DrainOutcome::Failed;
        }
    };

    if array_rc == SQL_ERROR {
        DrainOutcome::Failed
    } else if has_server_info || array_rc == SQL_SUCCESS_WITH_INFO {
        DrainOutcome::InfoPosted
    } else {
        DrainOutcome::Clean
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::odbc_types::SQL_NULL_HANDLE;
    use crate::test_support::TestHandles;
    use crate::test_support::assert_last_diag;

    #[test]
    fn exhausted_parameter_array_publishes_completion_on_close_or_cancel() {
        use crate::api::odbc_types::*;
        use mssql_tds::connection::tds_client::PreparedStatement;
        use mssql_tds::test_client_support::{
            done_in_proc_more, done_proc_no_more, tds_client_from_int_rows_with_trailing_tokens,
        };

        for cancel in [false, true] {
            let h = TestHandles::with_env_dbc_stmt();
            let stmt = unsafe { handle_from_raw::<StmtHandle>(h.stmt) };
            let dbc = stmt.parent_dbc();
            h.mark_dbc_connected();
            let mut client = tds_client_from_int_rows_with_trailing_tokens(
                vec![vec![2]],
                vec![done_in_proc_more(), done_proc_no_more()],
            );
            let id = client.register_prepared_handle_for_test(42);
            dbc.inner.lock().unwrap().wire.put_client_for_test(client);
            let sql: Vec<u16> = "SELECT ?".encode_utf16().collect();
            let mut values = [1i32, 2];
            let mut indicators = [0isize; 2];
            let mut statuses = [SQL_PARAM_UNUSED; 2];
            let mut operations = [SQL_PARAM_IGNORE, SQL_PARAM_PROCEED];
            let mut processed = 0usize;
            assert_eq!(SQL_SUCCESS, unsafe {
                crate::api::SQLPrepareW(h.stmt, sql.as_ptr(), i16::try_from(sql.len()).unwrap())
            });
            assert_eq!(SQL_SUCCESS, unsafe {
                crate::api::SQLBindParameter(
                    h.stmt,
                    1,
                    SQL_PARAM_INPUT,
                    SQL_C_LONG,
                    SQL_INTEGER,
                    0,
                    0,
                    values.as_mut_ptr().cast(),
                    4,
                    indicators.as_mut_ptr(),
                )
            });
            for (attribute, value) in [
                (SQL_ATTR_PARAMSET_SIZE, 2usize as SqlPointer),
                (SQL_ATTR_PARAM_STATUS_PTR, statuses.as_mut_ptr().cast()),
                (SQL_ATTR_PARAM_OPERATION_PTR, operations.as_mut_ptr().cast()),
                (
                    SQL_ATTR_PARAMS_PROCESSED_PTR,
                    (&mut processed as *mut usize).cast(),
                ),
            ] {
                assert_eq!(SQL_SUCCESS, unsafe {
                    crate::api::SQLSetStmtAttrW(h.stmt, attribute, value, 0)
                });
            }
            stmt.inner.lock().unwrap().prepared.as_mut().unwrap().stmt =
                PreparedStatement::materialized_for_test("SELECT @P1", id);
            assert_eq!(
                SQL_SUCCESS,
                unsafe { crate::api::SQLExecute(h.stmt) },
                "{:?}",
                stmt.inner.lock().unwrap().diag_records
            );
            assert_eq!(SQL_SUCCESS, unsafe { crate::api::SQLFetch(h.stmt) });
            assert_eq!(SQL_NO_DATA, unsafe { crate::api::SQLFetch(h.stmt) });
            // Model wire exhaustion before the ODBC parameter-array completion
            // has been published. SQLMoreResults would publish it immediately.
            let mut client = dbc
                .inner
                .lock()
                .unwrap()
                .wire
                .clear_client_for_test()
                .unwrap();
            dbc.runtime.block_on(client.advance()).unwrap();
            assert!(!client.has_open_batch());
            {
                let mut state = dbc.inner.lock().unwrap();
                state.wire.put_client_for_test(client);
                state.wire.set_owner_for_test(None);
            }
            stmt.inner.lock().unwrap().batch_exhausted = true;
            {
                let state = stmt.inner.lock().unwrap();
                assert!(state.batch_exhausted);
                assert!(state.parameter_array.is_some());
            }
            processed = 0;
            assert_eq!(
                if cancel {
                    SQL_SUCCESS_WITH_INFO
                } else {
                    SQL_SUCCESS
                },
                unsafe {
                    if cancel {
                        crate::api::SQLCancel(h.stmt)
                    } else {
                        crate::api::SQLFreeStmt(h.stmt, SQL_CLOSE)
                    }
                }
            );
            assert_eq!(processed, 2);
            assert_eq!(statuses, [SQL_PARAM_UNUSED, SQL_PARAM_SUCCESS]);
            assert!(stmt.inner.lock().unwrap().parameter_array.is_none());
        }
    }

    #[test]
    fn close_cursor_null_handle() {
        let ret = unsafe { sql_close_cursor(SQL_NULL_HANDLE) };
        assert_eq!(ret, SQL_INVALID_HANDLE);
    }

    #[test]
    fn close_cursor_no_cursor_open_returns_error() {
        let h = TestHandles::with_env_dbc_stmt();
        // No execute has been called — cursor_open is false.
        let ret = unsafe { sql_close_cursor(h.stmt) };
        assert_eq!(ret, SQL_ERROR);
    }

    #[test]
    fn free_stmt_close_null_handle() {
        let ret = unsafe { sql_free_stmt_close(SQL_NULL_HANDLE) };
        assert_eq!(ret, SQL_INVALID_HANDLE);
    }

    #[test]
    fn free_stmt_close_no_cursor_is_noop() {
        let h = TestHandles::with_env_dbc_stmt();
        // No execute has been called — cursor_open is false; SQL_CLOSE is a no-op.
        let ret = unsafe { sql_free_stmt_close(h.stmt) };
        assert_eq!(ret, SQL_SUCCESS);
    }

    /// A prior fetch's read-ahead peek can have discovered a trailing SQL
    /// Server error (see AB#47508's `release_busy_if_row_exhausted`), stashed
    /// as `pending_fetch_error` because that fetch had already committed to
    /// `SQL_SUCCESS`. Before this deferral existed, a direct `SQLCloseCursor`
    /// on that same statement would still see the error from its own drain —
    /// the peek's read-ahead is what stops the drain from finding it there —
    /// so `SQLCloseCursor` must surface it itself instead of silently
    /// reporting a clean close.
    #[test]
    fn close_cursor_surfaces_a_pending_fetch_error() {
        use mssql_tds::error::Error as TdsError;

        let h = TestHandles::with_env_dbc_stmt();
        let stmt = unsafe { handle_from_raw::<StmtHandle>(h.stmt) };
        {
            let mut ss = stmt.inner.lock().unwrap();
            ss.set_state(STMT_STATE_CURSOR_OPEN);
            ss.pending_fetch_error = Some(TdsError::ProtocolError(
                "simulated trailing SQL Server error".to_string(),
            ));
        }

        let ret = unsafe { sql_close_cursor(h.stmt) };

        assert_eq!(ret, SQL_ERROR);
        let ss = stmt.inner.lock().unwrap();
        assert!(
            ss.diag_records
                .iter()
                .any(|d| d.message.contains("simulated trailing SQL Server error")),
            "the deferred error must be posted, not silently dropped by the close"
        );
        assert!(ss.pending_fetch_error.is_none());
    }

    /// Same requirement as `close_cursor_surfaces_a_pending_fetch_error`, for
    /// the `SQLFreeStmt(SQL_CLOSE)` entry point.
    #[test]
    fn free_stmt_close_surfaces_a_pending_fetch_error() {
        use mssql_tds::error::Error as TdsError;

        let h = TestHandles::with_env_dbc_stmt();
        let stmt = unsafe { handle_from_raw::<StmtHandle>(h.stmt) };
        {
            let mut ss = stmt.inner.lock().unwrap();
            ss.set_state(STMT_STATE_CURSOR_OPEN);
            ss.pending_fetch_error = Some(TdsError::ProtocolError(
                "simulated trailing SQL Server error".to_string(),
            ));
        }

        let ret = unsafe { sql_free_stmt_close(h.stmt) };

        assert_eq!(ret, SQL_ERROR);
        let ss = stmt.inner.lock().unwrap();
        assert!(
            ss.diag_records
                .iter()
                .any(|d| d.message.contains("simulated trailing SQL Server error")),
            "the deferred error must be posted, not silently dropped by the close"
        );
        assert!(ss.pending_fetch_error.is_none());
    }

    /// The connection-scoped sweep (`SQLEndTran`/autocommit/isolation change)
    /// must NOT fail its own return over a pending fetch error on one
    /// statement: `SQL_ERROR` from this function specifically means "the
    /// stream failed to drain, unsafe to send a TM request next" — a stale
    /// diagnostic on an already-closed batch does not make that true. The
    /// diagnostic still posts to the statement's own records so it remains
    /// visible if the application inspects that handle later.
    #[test]
    fn close_cursor_for_connection_op_posts_but_does_not_fail_on_a_pending_fetch_error() {
        use crate::handles::dbc::DbcHandle;
        use mssql_tds::error::Error as TdsError;
        use mssql_tds::test_client_support::tds_client_from_tokens;

        let h = TestHandles::with_env_dbc_stmt();
        let stmt = unsafe { handle_from_raw::<StmtHandle>(h.stmt) };
        {
            let mut ss = stmt.inner.lock().unwrap();
            ss.set_state(STMT_STATE_CURSOR_OPEN);
            ss.pending_fetch_error = Some(TdsError::ProtocolError(
                "simulated trailing SQL Server error".to_string(),
            ));
        }
        h.mark_dbc_connected();
        // A fresh, never-executed client: has_open_batch() is false, so
        // drain_and_release's own close_query() is a real no-op — isolating
        // the assertion to "does a pending fetch error alone force
        // SQL_ERROR", independent of drain success/failure.
        let dbc = unsafe { handle_from_raw::<DbcHandle>(h.dbc) };
        dbc.inner
            .lock()
            .unwrap()
            .wire
            .put_client_for_test(tds_client_from_tokens(vec![]));

        let ret = close_cursor_for_connection_op(stmt, h.stmt);

        assert_eq!(ret, SQL_SUCCESS);
        let ss = stmt.inner.lock().unwrap();
        assert!(
            ss.diag_records
                .iter()
                .any(|d| d.message.contains("simulated trailing SQL Server error")),
            "the pending error must still be posted to this statement's diagnostics"
        );
        assert!(ss.pending_fetch_error.is_none());
    }

    /// Statement A's cursor stays `STMT_STATE_CURSOR_OPEN` after its own fetch
    /// released `active_stmt` early (AB#47508) — a different statement B may
    /// have since claimed the connection and be mid-fetch on its own live,
    /// still-open result set. Closing A's cursor in that window must not touch
    /// B's client at all: taking it here would drain/corrupt B's pending rows.
    #[test]
    fn drain_and_release_does_not_touch_a_different_statements_client() {
        use crate::handles::dbc::DbcHandle;
        use mssql_tds::test_client_support::{col_metadata_empty, tds_client_from_tokens};

        let mut h = TestHandles::with_env_dbc_stmt();
        let stmt_b = h.alloc_extra_stmt();
        h.mark_dbc_connected();

        let dbc = unsafe { handle_from_raw::<DbcHandle>(h.dbc) };
        // B's live client, positioned on an open result set (has_open_batch) so
        // `close_query()` would genuinely try to drain it if this stole the
        // client — with an empty token queue left, that drain would error.
        let mut client_b = tds_client_from_tokens(vec![col_metadata_empty()]);
        dbc.runtime
            .block_on(client_b.execute("SELECT 1;".to_string(), ()))
            .unwrap();
        {
            let mut ds = dbc.inner.lock().unwrap();
            ds.wire.put_client_for_test(client_b);
            ds.wire.set_owner_for_test(Some(stmt_b));
        }
        {
            let stmt_a = unsafe { handle_from_raw::<StmtHandle>(h.stmt) };
            stmt_a
                .inner
                .lock()
                .unwrap()
                .set_state(STMT_STATE_CURSOR_OPEN);
        }

        let outcome = drain_and_release(unsafe { handle_from_raw::<StmtHandle>(h.stmt) }, h.stmt);

        assert!(matches!(outcome, DrainOutcome::Clean));
        let ds = dbc.inner.lock().unwrap();
        assert_eq!(
            ds.wire.statement_owner(),
            Some(stmt_b),
            "B's claim on the connection must be left untouched"
        );
        assert!(
            ds.wire.client().is_some_and(|c| c.has_open_batch()),
            "B's result set must still be open — not drained by A's close"
        );
    }

    /// An early-released statement has nothing to drain while a
    /// connection-scoped operation holds the wire.
    #[test]
    fn drain_and_release_leaves_a_connection_operations_lease_alone() {
        use crate::handles::dbc::DbcHandle;
        use crate::test_support::connection_lease_for_test;
        use mssql_tds::test_client_support::tds_client_from_tokens;

        let h = TestHandles::with_env_dbc_stmt();
        h.mark_dbc_connected();
        let dbc = unsafe { handle_from_raw::<DbcHandle>(h.dbc) };
        let lease = connection_lease_for_test(dbc, tds_client_from_tokens(Vec::new()));

        let stmt = unsafe { handle_from_raw::<StmtHandle>(h.stmt) };
        assert!(matches!(
            drain_and_release(stmt, h.stmt),
            DrainOutcome::Clean
        ));
        assert!(stmt.inner.lock().unwrap().diag_records.is_empty());
        assert_eq!(
            dbc.inner.lock().unwrap().wire.owner(),
            Some(WireOwner::Connection)
        );
        dbc.return_lease(lease, LeaseReturn::Release);
    }

    /// The AB#47510 guard on the cursor drain. Once the loader has terminated
    /// the runtime's worker, `block_on(close_query())` can never complete, so
    /// the drain must be skipped entirely. The flag is threaded in because a
    /// live test process always reads it as `false`; without that seam this arm
    /// is dead code under test and deleting it leaves the suite green.
    ///
    /// The client is left holding an open batch, which is what proves no drain
    /// was attempted — a real `close_query()` would have consumed it.
    #[test]
    fn drain_and_release_skips_the_round_trip_while_the_process_is_exiting() {
        use crate::handles::dbc::DbcHandle;
        use mssql_tds::test_client_support::{col_metadata_empty, tds_client_from_tokens};

        let h = TestHandles::with_env_dbc_stmt();
        h.mark_dbc_connected();
        let dbc = unsafe { handle_from_raw::<DbcHandle>(h.dbc) };

        // Positioned on an open result set with an empty token queue behind it,
        // so an actual drain would error rather than silently succeed.
        let mut client = tds_client_from_tokens(vec![col_metadata_empty()]);
        dbc.runtime
            .block_on(client.execute("SELECT 1;".to_string(), ()))
            .unwrap();
        {
            let mut ds = dbc.inner.lock().unwrap();
            ds.wire.put_client_for_test(client);
            ds.wire.set_owner_for_test(Some(h.stmt));
        }

        let outcome = drain_and_release_inner(
            unsafe { handle_from_raw::<StmtHandle>(h.stmt) },
            h.stmt,
            true,
            false,
        );

        assert!(
            matches!(outcome, DrainOutcome::Clean),
            "skipping during shutdown must report a clean close, not a failure"
        );
        let ds = dbc.inner.lock().unwrap();
        assert!(
            ds.wire.client().is_some_and(|c| c.has_open_batch()),
            "the batch must be left undrained — a skipped drain performs no I/O"
        );
        assert_eq!(
            ds.wire.statement_owner(),
            None,
            "the connection must still be released, or it stays busy forever"
        );
    }

    /// The same call with the flag false takes the ordinary path and does drain
    /// the batch. Without this pair, a guard that fired unconditionally would
    /// also pass the test above.
    #[test]
    fn drain_and_release_still_drains_while_the_process_is_alive() {
        use crate::handles::dbc::DbcHandle;
        use mssql_tds::test_client_support::{
            col_metadata_empty, done_no_more, tds_client_from_tokens,
        };

        let h = TestHandles::with_env_dbc_stmt();
        h.mark_dbc_connected();
        let dbc = unsafe { handle_from_raw::<DbcHandle>(h.dbc) };

        let mut client = tds_client_from_tokens(vec![col_metadata_empty(), done_no_more()]);
        dbc.runtime
            .block_on(client.execute("SELECT 1;".to_string(), ()))
            .unwrap();
        {
            let mut ds = dbc.inner.lock().unwrap();
            ds.wire.put_client_for_test(client);
            ds.wire.set_owner_for_test(Some(h.stmt));
        }

        let _ = drain_and_release_inner(
            unsafe { handle_from_raw::<StmtHandle>(h.stmt) },
            h.stmt,
            false,
            false,
        );

        let ds = dbc.inner.lock().unwrap();
        assert!(
            ds.wire.client().is_some_and(|c| !c.has_open_batch()),
            "the live-process path must actually drain the batch"
        );
    }

    /// AB#49225: closing an early-released cursor (AB#47508) checks the client
    /// out; an execute in that window must get the busy error. Holding A's
    /// statement lock parks the close right after its checkout.
    #[test]
    fn closing_an_early_released_cursor_reports_busy_to_a_concurrent_execute() {
        use std::time::{Duration, Instant};

        use crate::api::exec_common::{claim_connection, return_client_idle};
        use crate::api::odbc_types::{SQL_ERROR, SqlHandle};
        use crate::handles::dbc::DbcHandle;
        use mssql_tds::test_client_support::tds_client_from_tokens;

        let mut h = TestHandles::with_env_dbc_stmt();
        let stmt_b = h.alloc_extra_stmt();
        h.mark_dbc_connected();
        let dbc = unsafe { handle_from_raw::<DbcHandle>(h.dbc) };
        // Early-released: client parked, wire unreserved.
        dbc.inner
            .lock()
            .unwrap()
            .wire
            .put_client_for_test(tds_client_from_tokens(Vec::new()));

        let stmt_a = unsafe { handle_from_raw::<StmtHandle>(h.stmt) };
        let parked = stmt_a.inner.lock().unwrap();

        // Raw handles are not `Send`; move the address instead.
        let stmt_a_addr = h.stmt as usize;
        let closer = std::thread::spawn(move || {
            let handle = stmt_a_addr as SqlHandle;
            let stmt_a = unsafe { handle_from_raw::<StmtHandle>(handle) };
            matches!(drain_and_release(stmt_a, handle), DrainOutcome::Clean)
        });

        let deadline = Instant::now() + Duration::from_secs(10);
        while !dbc.inner.lock().unwrap().wire.is_checked_out() {
            assert!(
                Instant::now() < deadline,
                "the close never checked the client out"
            );
            std::thread::yield_now();
        }
        assert_eq!(
            dbc.inner.lock().unwrap().wire.statement_owner(),
            Some(h.stmt),
            "the close must reserve the wire for its own statement"
        );

        let other = unsafe { handle_from_raw::<StmtHandle>(stmt_b) };
        let claim = claim_connection(dbc, other, stmt_b, "SQLExecDirectW");
        assert!(matches!(claim, Err(SQL_ERROR)));
        {
            let ss = other.inner.lock().unwrap();
            assert_eq!(ss.diag_records.len(), 1);
            assert_last_diag(&ss.diag_records, ERR_CONNECTION_BUSY);
        }

        drop(parked);
        assert!(
            closer.join().unwrap(),
            "the close itself must still succeed"
        );
        {
            let ds = dbc.inner.lock().unwrap();
            assert_eq!(ds.wire.owner(), None, "the close must release the wire");
            assert!(ds.wire.client().is_some());
        }
        let lease = claim_connection(dbc, other, stmt_b, "SQLExecDirectW")
            .expect("the other statement can execute once the close finishes");
        return_client_idle(dbc, stmt_b, lease);
    }
}
