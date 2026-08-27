// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Implementation of SQLMoreResults.
//!
//! Mirrors msodbcsql's `SQLMoreResults`: close the current
//! rowset's reading state and advance to the next result set in the batch,
//! if any. Returns `SQL_SUCCESS` when a new result set is positioned,
//! `SQL_NO_DATA` when the batch is exhausted, or `SQL_ERROR` on failure.

use tracing::{debug, error};

use mssql_tds::connection::tds_client::{ResultSet, StatementResult};

use super::close_cursor::reset_cursor_state;
use crate::api::odbc_types::{
    SQL_ERROR, SQL_INVALID_HANDLE, SQL_NO_DATA, SQL_SUCCESS, SQL_SUCCESS_WITH_INFO, SqlHandle,
    SqlReturn,
};
use crate::api::sqlstate::{
    ERR_CONNECTION_BUSY, ERR_NO_ACTIVE_TDS_CLIENT, SQLSTATE_HY000, post_diag, post_tds_error,
    post_tds_info_messages,
};
use crate::error::free_errors;
use crate::handles::stmt::STMT_STATE_CURSOR_OPEN;
use crate::handles::{HandleType, StmtHandle, handle_from_raw};

/// Advances to the next result set on a statement.
///
/// # Safety
/// `statement_handle` must be a valid `StmtHandle` or null.
pub(crate) unsafe fn sql_more_results(statement_handle: SqlHandle) -> SqlReturn {
    debug!(?statement_handle, "SQLMoreResults called");
    crate::ffi_entry!("SQLMoreResults", unsafe {
        sql_more_results_impl(statement_handle)
    })
}

unsafe fn sql_more_results_impl(statement_handle: SqlHandle) -> SqlReturn {
    if statement_handle.is_null() {
        error!("SQLMoreResults: statement_handle is null");
        return SQL_INVALID_HANDLE;
    }
    let stmt = unsafe { handle_from_raw::<StmtHandle>(statement_handle) };
    debug_assert_eq!(stmt.object_type, HandleType::Stmt);
    sql_more_results_safe(statement_handle, stmt)
}

fn sql_more_results_safe(statement_handle: SqlHandle, stmt: &StmtHandle) -> SqlReturn {
    // Free any stale diagnostics and observe cursor state.
    let cursor_open = {
        let Ok(mut stmt_state) = stmt.inner.lock() else {
            error!("SQLMoreResults: stmt mutex poisoned");
            return SQL_ERROR;
        };
        free_errors(&mut stmt_state);
        if let Some(e) = stmt_state.pending_fetch_error.take() {
            // A prior fetch's read-ahead peek already discovered this result
            // set ends in a SQL Server error (see AB#47508's
            // release_busy_if_row_exhausted), but that call had already
            // committed to delivering its own row successfully, so the
            // diagnostic was deferred here instead of being lost under that
            // call's own success return.
            reset_cursor_state(&mut stmt_state);
            post_tds_error(&mut stmt_state, &e, SQLSTATE_HY000);
            return SQL_ERROR;
        }
        if stmt_state.batch_exhausted {
            // A prior fetch's read-ahead peek already confirmed the wire has
            // nothing left anywhere in this batch — not just the current
            // result set, which is all `result_set_exhausted` would prove
            // (see AB#47508's release_busy_if_row_exhausted). The answer is
            // already known and needs no connection access at all: report it
            // even if a different statement has since claimed the
            // connection. Matches msodbcsql, whose SQLMoreResults has no busy
            // check of its own (`GetBatchCtxOrRecover` just falls through to
            // `SQL_NO_DATA_FOUND` once the batch context is gone).
            reset_cursor_state(&mut stmt_state);
            debug!("SQLMoreResults: batch already known exhausted; returning SQL_NO_DATA");
            return SQL_NO_DATA;
        }
        // A pure-DML batch queued one row count per statement; step through them
        // in memory (no cursor or connection) before falling back to the wire.
        if let Some(next) = stmt_state.pending_row_counts.pop_front() {
            // A queued count is still a result set, so the batch ordinal has to
            // advance with it: msodbcsql reports 1, 2, 3 across a pure-DML batch
            // exactly as it does across a batch of SELECTs.
            stmt_state.begin_result_set(Vec::new());
            stmt_state.row_count = next;
            debug!("SQLMoreResults: advanced to next DML result set");
            return SQL_SUCCESS;
        }
        stmt_state.has_state(STMT_STATE_CURSOR_OPEN)
    };

    if !cursor_open {
        debug!("SQLMoreResults: no cursor open; no more result sets");
        return SQL_NO_DATA;
    }

    let dbc = stmt.parent_dbc();

    // Take the client; keep active_stmt set so concurrent statements continue
    // to see the connection as busy throughout the advance.
    let mut client = {
        let Ok(mut dbc_state) = dbc.inner.lock() else {
            error!("SQLMoreResults: dbc mutex poisoned");
            return SQL_ERROR;
        };
        if let Some(busy_stmt) = dbc_state.active_stmt
            && busy_stmt != statement_handle
        {
            error!("SQLMoreResults: connection is busy with results for another statement");
            drop(dbc_state);
            if let Ok(mut ss) = stmt.inner.lock() {
                post_diag(&mut ss, ERR_CONNECTION_BUSY);
            }
            return SQL_ERROR;
        }
        let Some(client) = dbc_state.client.take() else {
            error!("SQLMoreResults: no active TDS client");
            drop(dbc_state);
            if let Ok(mut ss) = stmt.inner.lock() {
                post_diag(&mut ss, ERR_NO_ACTIVE_TDS_CLIENT);
            }
            return SQL_ERROR;
        };
        // AB#47508's early release can have left this `None` (the previous
        // result set was fetched to exhaustion without an explicit close),
        // so claim it now, in the same critical section as the take, rather
        // than leaving a window where a concurrent statement sees a claimed
        // client with no owning statement and gets a confusing
        // "no active TDS client" instead of the correct busy diagnostic.
        dbc_state.active_stmt = Some(statement_handle);
        client
    };

    match dbc.runtime.block_on(client.advance()) {
        Ok(StatementResult::Rows) => {
            // Positioned on a new row-returning result set. Refresh metadata,
            // clear row state, keep CURSOR_OPEN and active_stmt set.
            let metadata = client.get_metadata().clone();
            let Ok(mut stmt_state) = stmt.inner.lock() else {
                error!("SQLMoreResults: stmt mutex poisoned advancing result set");
                if let Ok(mut ds) = dbc.inner.lock() {
                    ds.client = Some(client);
                    if ds.active_stmt == Some(statement_handle) {
                        ds.active_stmt = None;
                    }
                }
                return SQL_ERROR;
            };
            stmt_state.begin_result_set(metadata);
            stmt_state.reset_row_stream();
            stmt_state.result_set_exhausted = false;
            stmt_state.batch_exhausted = false;
            // Refresh the count for the newly-positioned result set (-1 for a SELECT).
            stmt_state.row_count = client.last_rows_affected();
            // Drain INFO only after the lock is held.
            let info_messages = client.take_info_messages();
            let has_server_info = post_tds_info_messages(&mut stmt_state, &info_messages);
            drop(stmt_state);
            if let Ok(mut dbc_state) = dbc.inner.lock() {
                dbc_state.client = Some(client);
                // Explicitly (re-)claim: AB#47508's early release can have left
                // this `None` if the previous result set was fetched to
                // exhaustion without an explicit close, so this cannot just
                // assume it is still `Some(statement_handle)`.
                dbc_state.active_stmt = Some(statement_handle);
            }
            debug!("SQLMoreResults: advanced to next result set");
            if has_server_info {
                SQL_SUCCESS_WITH_INFO
            } else {
                SQL_SUCCESS
            }
        }
        Ok(StatementResult::NoRows { .. }) => {
            // Positioned on a no-row statement result (PRINT / low-severity
            // RAISERROR / DDL / DML): zero columns, so it is not fetchable
            // (SQLFetch returns 24000), but it is a navigable result and may
            // carry diagnostic messages. The connection stays busy so a further
            // SQLMoreResults can advance past it. Matches msodbcsql.
            let Ok(mut stmt_state) = stmt.inner.lock() else {
                error!("SQLMoreResults: stmt mutex poisoned on no-row result");
                if let Ok(mut ds) = dbc.inner.lock() {
                    ds.client = Some(client);
                    if ds.active_stmt == Some(statement_handle) {
                        ds.active_stmt = None;
                    }
                }
                return SQL_ERROR;
            };
            stmt_state.begin_result_set(Vec::new());
            // Surface this no-row statement's own affected-row count for
            // SQLRowCount now that we are positioned on it.
            stmt_state.row_count = client.last_rows_affected();
            stmt_state.reset_row_stream();
            // A stale `true` here (inherited from a previous result set this
            // statement fetched to exhaustion) would make SQLFetch report
            // SQL_NO_DATA instead of the 24000 this zero-column result must
            // give — so this arm needs the same reset as the `Rows` arm even
            // though there is nothing to fetch on this result itself.
            stmt_state.result_set_exhausted = false;
            stmt_state.batch_exhausted = false;
            let info_messages = client.take_info_messages();
            let has_server_info = post_tds_info_messages(&mut stmt_state, &info_messages);
            drop(stmt_state);
            if let Ok(mut dbc_state) = dbc.inner.lock() {
                dbc_state.client = Some(client);
                // Explicitly (re-)claim — see the `Rows` arm above.
                dbc_state.active_stmt = Some(statement_handle);
            }
            debug!("SQLMoreResults: advanced to a no-row statement result");
            if has_server_info {
                SQL_SUCCESS_WITH_INFO
            } else {
                SQL_SUCCESS
            }
        }
        Ok(StatementResult::End) => {
            // Batch exhausted. Close cursor state and release the connection.
            let Ok(mut stmt_state) = stmt.inner.lock() else {
                error!("SQLMoreResults: stmt mutex poisoned at batch end");
                if let Ok(mut ds) = dbc.inner.lock() {
                    ds.client = Some(client);
                    if ds.active_stmt == Some(statement_handle) {
                        ds.active_stmt = None;
                    }
                }
                return SQL_ERROR;
            };
            reset_cursor_state(&mut stmt_state);
            // Drain INFO only after the lock is held.
            let info_messages = client.take_info_messages();
            post_tds_info_messages(&mut stmt_state, &info_messages);
            drop(stmt_state);
            // TODO: surface output-param availability here once output
            // params land.
            if let Ok(mut dbc_state) = dbc.inner.lock() {
                dbc_state.client = Some(client);
                if dbc_state.active_stmt == Some(statement_handle) {
                    dbc_state.active_stmt = None;
                }
            }
            debug!("SQLMoreResults: no more result sets");
            SQL_NO_DATA
        }
        Err(e) => {
            error!(%e, "SQLMoreResults: advance failed");
            if let Ok(mut stmt_state) = stmt.inner.lock() {
                // Treat as terminal: clear cursor state and post diagnostic.
                reset_cursor_state(&mut stmt_state);
                post_tds_error(&mut stmt_state, &e, SQLSTATE_HY000);
                let info_messages = client.take_info_messages();
                post_tds_info_messages(&mut stmt_state, &info_messages);
            }
            if let Ok(mut dbc_state) = dbc.inner.lock() {
                dbc_state.client = Some(client);
                if dbc_state.active_stmt == Some(statement_handle) {
                    dbc_state.active_stmt = None;
                }
            }
            SQL_ERROR
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::ptr;

    use super::*;
    use crate::api::odbc_types::{SQL_NO_DATA, SQL_SUCCESS, SQL_SUCCESS_WITH_INFO};
    use crate::api::sqlstate::ERR_NO_ACTIVE_TDS_CLIENT;
    use crate::handles::dbc::DbcHandle;
    use crate::test_support::TestHandles;
    use mssql_tds::error::Error as TdsError;
    use mssql_tds::test_client_support::{
        ScriptedToken, col_metadata_empty, done_more, done_no_more, info, tds_client_from_tokens,
    };

    /// Builds a scripted client, positions it on the batch's first statement,
    /// then injects it into `h`'s DBC as the busy client owning `h.stmt` with an
    /// open cursor — mirroring the state left by a successful `SQLExecDirect`.
    /// Returns the first statement's result so callers can assert on it.
    fn position_first_and_inject(h: &TestHandles, tokens: Vec<ScriptedToken>) -> StatementResult {
        let dbc = unsafe { handle_from_raw::<DbcHandle>(h.dbc) };
        let stmt = unsafe { handle_from_raw::<StmtHandle>(h.stmt) };
        let mut client = tds_client_from_tokens(tokens);
        let first = dbc
            .runtime
            .block_on(client.execute("SELECT 1;".to_string(), ()))
            .unwrap();
        {
            let mut ss = stmt.inner.lock().unwrap();
            ss.set_state(STMT_STATE_CURSOR_OPEN);
        }
        {
            let mut ds = dbc.inner.lock().unwrap();
            ds.client = Some(client);
            ds.active_stmt = Some(h.stmt);
        }
        first
    }

    /// SQLMoreResults advances from one row set to the next, keeping the cursor
    /// open and the connection busy on the same statement.
    #[test]
    fn more_results_advances_to_next_rowset() {
        let h = TestHandles::with_env_dbc_stmt();
        let first = position_first_and_inject(
            &h,
            vec![
                col_metadata_empty(), // stmt1 row set
                done_more(),          // terminates stmt1, more to come
                col_metadata_empty(), // stmt2 row set
            ],
        );
        assert_eq!(first, StatementResult::Rows);

        let ret = unsafe { sql_more_results(h.stmt) };
        assert_eq!(ret, SQL_SUCCESS);

        let dbc = unsafe { handle_from_raw::<DbcHandle>(h.dbc) };
        assert_eq!(dbc.inner.lock().unwrap().active_stmt, Some(h.stmt));
    }

    /// The first result set was fetched to exhaustion, which releases
    /// `active_stmt` early (AB#47508) *without* closing the cursor. Advancing
    /// past it with `SQLMoreResults` must (re-)claim `active_stmt` for this
    /// statement rather than assume it is already set — otherwise the freshly
    /// positioned second rowset is unreachable: the next `SQLFetch` hits
    /// `fill_rowset`'s "already drained" guard (which only checks
    /// `active_stmt`) and wrongly reports `SQL_NO_DATA` despite a real row
    /// waiting on the wire.
    #[test]
    fn more_results_reclaims_active_stmt_after_an_early_release() {
        let h = TestHandles::with_env_dbc_stmt();
        let first = position_first_and_inject(
            &h,
            vec![
                col_metadata_empty(), // stmt1 row set
                done_more(),          // terminates stmt1, more to come
                col_metadata_empty(), // stmt2 row set
            ],
        );
        assert_eq!(first, StatementResult::Rows);

        let dbc = unsafe { handle_from_raw::<DbcHandle>(h.dbc) };
        // Simulate stmt1 having been fetched to exhaustion: active_stmt was
        // already released, but the cursor (and client) are still there.
        dbc.inner.lock().unwrap().active_stmt = None;

        let ret = unsafe { sql_more_results(h.stmt) };
        assert_eq!(ret, SQL_SUCCESS);
        assert_eq!(
            dbc.inner.lock().unwrap().active_stmt,
            Some(h.stmt),
            "must explicitly reclaim active_stmt, not just leave the released None"
        );
    }

    /// A statement whose first result set was fetched to exhaustion (marked
    /// via a prior fetch's peek, AB#47508) must not have that stale flag leak
    /// into the next result set `SQLMoreResults` positions on — otherwise a
    /// perfectly fetchable second rowset would report `SQL_NO_DATA` on the
    /// very first `SQLFetch`.
    #[test]
    fn more_results_clears_a_stale_exhausted_flag_on_the_next_rowset() {
        let h = TestHandles::with_env_dbc_stmt();
        let first = position_first_and_inject(
            &h,
            vec![
                col_metadata_empty(), // stmt1 row set
                done_more(),          // terminates stmt1, more to come
                col_metadata_empty(), // stmt2 row set
            ],
        );
        assert_eq!(first, StatementResult::Rows);
        {
            let stmt = unsafe { handle_from_raw::<StmtHandle>(h.stmt) };
            stmt.inner.lock().unwrap().result_set_exhausted = true;
        }

        let ret = unsafe { sql_more_results(h.stmt) };
        assert_eq!(ret, SQL_SUCCESS);

        let stmt = unsafe { handle_from_raw::<StmtHandle>(h.stmt) };
        assert!(!stmt.inner.lock().unwrap().result_set_exhausted);
    }

    /// SQLMoreResults surfaces a no-row statement result (message-bearing, zero
    /// columns) as SQL_SUCCESS_WITH_INFO with the cursor kept open, then reports
    /// SQL_NO_DATA and releases the connection when the batch is exhausted.
    #[test]
    fn more_results_surfaces_norow_then_end() {
        let h = TestHandles::with_env_dbc_stmt();
        let first = position_first_and_inject(
            &h,
            vec![
                col_metadata_empty(),        // stmt1 row set
                done_more(),                 // terminates stmt1
                info(50000, 10, "raise me"), // stmt2 message
                done_no_more(),              // stmt2 no-row result, last in batch
            ],
        );
        assert_eq!(first, StatementResult::Rows);

        // Advance onto the no-row statement result.
        let ret = unsafe { sql_more_results(h.stmt) };
        assert_eq!(ret, SQL_SUCCESS_WITH_INFO);
        let stmt = unsafe { handle_from_raw::<StmtHandle>(h.stmt) };
        assert!(stmt.inner.lock().unwrap().column_metadata.is_empty());

        // Advance again: batch exhausted -> SQL_NO_DATA, cursor closed, released.
        let ret = unsafe { sql_more_results(h.stmt) };
        assert_eq!(ret, SQL_NO_DATA);
        let dbc = unsafe { handle_from_raw::<DbcHandle>(h.dbc) };
        assert!(dbc.inner.lock().unwrap().active_stmt.is_none());
    }

    /// Same re-claim requirement as
    /// `more_results_reclaims_active_stmt_after_an_early_release`, for the
    /// no-row-result arm: it also only "left `active_stmt` as is" before this
    /// fix, which broke the moment an earlier fetch had already released it.
    #[test]
    fn more_results_reclaims_active_stmt_advancing_to_a_norow_result() {
        let h = TestHandles::with_env_dbc_stmt();
        let first = position_first_and_inject(
            &h,
            vec![
                col_metadata_empty(),        // stmt1 row set
                done_more(),                 // terminates stmt1
                info(50000, 10, "raise me"), // stmt2 message
                done_no_more(),              // stmt2 no-row result, last in batch
            ],
        );
        assert_eq!(first, StatementResult::Rows);

        let dbc = unsafe { handle_from_raw::<DbcHandle>(h.dbc) };
        dbc.inner.lock().unwrap().active_stmt = None;

        let ret = unsafe { sql_more_results(h.stmt) };
        assert_eq!(ret, SQL_SUCCESS_WITH_INFO);
        assert_eq!(
            dbc.inner.lock().unwrap().active_stmt,
            Some(h.stmt),
            "must explicitly reclaim active_stmt for the still-navigable batch"
        );
    }

    /// A prior fetch's read-ahead peek can have discovered a trailing SQL
    /// Server error instead of a clean end of set (see AB#47508's
    /// `release_busy_if_row_exhausted`), deferred via `pending_fetch_error`
    /// since that fetch call had already committed to delivering its own row
    /// successfully. `SQLMoreResults` must drain and report it — not
    /// silently treat the closed batch as `SQL_NO_DATA` — even when called
    /// directly, with no intervening `SQLFetch` ever seeing it.
    #[test]
    fn more_results_surfaces_a_pending_fetch_error() {
        let h = TestHandles::with_env_dbc_stmt();
        let stmt = unsafe { handle_from_raw::<StmtHandle>(h.stmt) };
        {
            let mut ss = stmt.inner.lock().unwrap();
            ss.set_state(STMT_STATE_CURSOR_OPEN);
            ss.result_set_exhausted = true;
            ss.pending_fetch_error = Some(TdsError::ProtocolError(
                "simulated trailing SQL Server error".to_string(),
            ));
        }
        let dbc = unsafe { handle_from_raw::<DbcHandle>(h.dbc) };
        dbc.inner.lock().unwrap().active_stmt = Some(h.stmt);

        let ret = unsafe { sql_more_results(h.stmt) };

        assert_eq!(ret, SQL_ERROR);
        let ss = stmt.inner.lock().unwrap();
        assert!(
            ss.diag_records
                .iter()
                .any(|d| d.message.contains("simulated trailing SQL Server error")),
            "the deferred error must be posted, not silently dropped as SQL_NO_DATA"
        );
        assert!(
            ss.pending_fetch_error.is_none(),
            "must be taken so it cannot leak into a later call"
        );
        assert!(!ss.has_state(STMT_STATE_CURSOR_OPEN));
    }

    /// **The blocking regression this tick fixes**, reproducing the
    /// reviewer's exact probe: statement A executes a single-statement,
    /// single-result-set batch, fetches its only row to exhaustion (which
    /// releases `active_stmt` early per AB#47508 and marks the whole batch
    /// — not just the current result set — as known-done), and statement B
    /// then claims the now-idle connection and executes its own query.
    /// `SQLMoreResults(A)` at that point must report `SQL_NO_DATA` without
    /// touching the connection at all — msodbcsql's `SQLMoreResults` has no
    /// busy check of its own — not `SQL_ERROR`/`HY000` from falling through
    /// to the ordinary busy-check path that only `SQLFetch` had a
    /// `result_set_exhausted` short-circuit for.
    #[test]
    fn more_results_fast_path_reports_no_data_when_batch_already_exhausted() {
        let mut h = TestHandles::with_env_dbc_stmt();
        let stmt_b = h.alloc_extra_stmt();
        let stmt_a = unsafe { handle_from_raw::<StmtHandle>(h.stmt) };
        {
            let mut sa = stmt_a.inner.lock().unwrap();
            sa.set_state(STMT_STATE_CURSOR_OPEN);
            // As if A's own fetch already peeked past its lone row, found
            // the wire fully done (single-statement batch, no MORE), and
            // released the claim — exactly what release_busy_if_row_exhausted
            // does when `release` is true.
            sa.result_set_exhausted = true;
            sa.batch_exhausted = true;
        }

        // B has since claimed the connection and is actively using it.
        let dbc = unsafe { handle_from_raw::<DbcHandle>(h.dbc) };
        dbc.inner.lock().unwrap().active_stmt = Some(stmt_b);
        // No client configured at all: if the fast path reached for the
        // connection this would fail with a different SQLSTATE (busy / no
        // active client) instead of SQL_NO_DATA.

        let ret = unsafe { sql_more_results(h.stmt) };

        assert_eq!(
            ret, SQL_NO_DATA,
            "must match msodbcsql, whose SQLMoreResults has no busy check at all"
        );
        assert_eq!(
            dbc.inner.lock().unwrap().active_stmt,
            Some(stmt_b),
            "B's claim on the connection must be left completely untouched"
        );
        assert!(
            !stmt_a
                .inner
                .lock()
                .unwrap()
                .has_state(STMT_STATE_CURSOR_OPEN)
        );
    }

    /// SQLMoreResults on an open cursor whose connection has no active client
    /// posts the no-active-client diagnostic and returns SQL_ERROR.
    #[test]
    fn more_results_no_active_client_errors() {
        let h = TestHandles::with_env_dbc_stmt();
        let stmt = unsafe { handle_from_raw::<StmtHandle>(h.stmt) };
        {
            let mut ss = stmt.inner.lock().unwrap();
            ss.set_state(STMT_STATE_CURSOR_OPEN);
        }
        let dbc = unsafe { handle_from_raw::<DbcHandle>(h.dbc) };
        dbc.inner.lock().unwrap().active_stmt = Some(h.stmt);

        let ret = unsafe { sql_more_results(h.stmt) };
        assert_eq!(ret, SQL_ERROR);
        assert_eq!(
            stmt.inner.lock().unwrap().diag_records[0].sql_state,
            ERR_NO_ACTIVE_TDS_CLIENT.state
        );
    }

    /// A transport failure while advancing (the drain hits a dead connection)
    /// surfaces as SQL_ERROR with a terminal cursor reset and the connection
    /// released.
    #[test]
    fn more_results_advance_failure_resets_and_errors() {
        let h = TestHandles::with_env_dbc_stmt();
        // Position on a row set, then leave nothing behind it so the drain hits
        // the end of the scripted stream and reports a closed connection.
        let first = position_first_and_inject(&h, vec![col_metadata_empty()]);
        assert_eq!(first, StatementResult::Rows);

        let ret = unsafe { sql_more_results(h.stmt) };
        assert_eq!(ret, SQL_ERROR);

        let stmt = unsafe { handle_from_raw::<StmtHandle>(h.stmt) };
        assert_eq!(
            stmt.inner.lock().unwrap().diag_records[0].sql_state,
            SQLSTATE_HY000
        );
        let dbc = unsafe { handle_from_raw::<DbcHandle>(h.dbc) };
        assert!(dbc.inner.lock().unwrap().active_stmt.is_none());
    }

    #[test]
    fn null_handle_returns_invalid_handle() {
        let rc = unsafe { sql_more_results(ptr::null_mut()) };
        assert_eq!(rc, SQL_INVALID_HANDLE);
    }

    #[test]
    fn no_cursor_and_no_pending_returns_no_data() {
        let h = TestHandles::with_env_dbc_stmt();
        let rc = unsafe { sql_more_results(h.stmt) };
        assert_eq!(rc, SQL_NO_DATA);
    }

    #[test]
    fn pending_dml_counts_stepped_then_exhausted() {
        let h = TestHandles::with_env_dbc_stmt();
        let stmt = unsafe { handle_from_raw::<StmtHandle>(h.stmt) };
        {
            let mut s = stmt.inner.lock().unwrap();
            s.row_count = 3;
            s.pending_row_counts = VecDeque::from(vec![2, 1]);
        }

        // Each SQLMoreResults surfaces the next DML statement's count in memory.
        assert_eq!(unsafe { sql_more_results(h.stmt) }, SQL_SUCCESS);
        assert_eq!(stmt.inner.lock().unwrap().row_count, 2);

        assert_eq!(unsafe { sql_more_results(h.stmt) }, SQL_SUCCESS);
        assert_eq!(stmt.inner.lock().unwrap().row_count, 1);

        // Queue drained and no cursor open -> end of batch.
        assert_eq!(unsafe { sql_more_results(h.stmt) }, SQL_NO_DATA);
    }

    /// A queued DML count is a result set as far as the batch ordinal is
    /// concerned. msodbcsql reports command 1, 2, 3 across a pure-DML batch
    /// exactly as it does across three SELECTs, so stepping the queue without
    /// advancing the ordinal would leave `SQL_SOPT_SS_CURRENT_COMMAND` stuck at
    /// 1 while `SQLMoreResults` walked the batch.
    #[test]
    fn stepping_pending_dml_counts_advances_the_command_ordinal() {
        let h = TestHandles::with_env_dbc_stmt();
        let stmt = unsafe { handle_from_raw::<StmtHandle>(h.stmt) };
        {
            let mut s = stmt.inner.lock().unwrap();
            s.begin_batch(Vec::new());
            s.pending_row_counts = VecDeque::from(vec![2, 1]);
        }
        assert_eq!(stmt.inner.lock().unwrap().current_command, 1);

        assert_eq!(unsafe { sql_more_results(h.stmt) }, SQL_SUCCESS);
        assert_eq!(stmt.inner.lock().unwrap().current_command, 2);

        assert_eq!(unsafe { sql_more_results(h.stmt) }, SQL_SUCCESS);
        assert_eq!(stmt.inner.lock().unwrap().current_command, 3);

        // End of batch holds the final ordinal rather than advancing past it.
        assert_eq!(unsafe { sql_more_results(h.stmt) }, SQL_NO_DATA);
        assert_eq!(stmt.inner.lock().unwrap().current_command, 3);
    }
}
