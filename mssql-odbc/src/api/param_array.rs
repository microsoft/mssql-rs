// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Parameter-array execution: one ODBC execute consuming every parameter set
//! selected by `SQL_ATTR_PARAMSET_SIZE` (AB#47820).
//!
//! # Behaviour measured from msodbcsql
//!
//! All of the following was read out of the shipping C++ driver rather than
//! inferred from the ODBC specification, because several of the rules are
//! driver choices the spec leaves open:
//!
//! - **Rows are not stopped at the first server error.** `sqlctokn.cpp:2341`
//!   states it outright: "we don't stop on ERROR token in parameter array
//!   execution". Every parameter set is attempted.
//! - **A parameter status array downgrades the overall return code.** Same
//!   comment: with `SQL_ATTR_PARAM_STATUS_PTR` set, a failed row yields
//!   `SQL_SUCCESS_WITH_INFO` because the caller can see `SQL_PARAM_ERROR` in
//!   the array; without it the driver must return `SQL_ERROR` or the failure
//!   would be invisible.
//! - **Per-row status values** come from `sqlctokn.cpp:2352-2371`: server error
//!   -> `SQL_PARAM_ERROR`, server info -> `SQL_PARAM_SUCCESS_WITH_INFO`,
//!   otherwise `SQL_PARAM_SUCCESS`.
//! - **`SQL_PARAM_IGNORE`** rows are skipped without being sent
//!   (`sqlccmd.cpp:3218`) and recorded as `SQL_PARAM_UNUSED`
//!   (`sqlctokn.cpp:2399`).
//! - **`SQL_ATTR_PARAMS_PROCESSED_PTR`** is written with the 1-based ordinal of
//!   the row that just completed (`sqlccmd.cpp:3216`, `sqlctokn.cpp:2388`), so
//!   after a full run it equals the paramset size.
//! - **Row counts are summed** across parameter sets, starting from a
//!   `SQL_NO_ROWCOUNT_TOTAL` (-1) sentinel (`sqlctokn.cpp:2246`,
//!   `sqlsrv.h:284`), which is what `SQLRowCount` then reports.
//! - **A leading `SQL_PARAM_IGNORE` still reports `SQL_PARAM_UNUSED` at its own
//!   index**, and the processed count still reaches the paramset size. The
//!   decisive commit is msodbcsql `63a70fb07` (PR 7183, "Fixes for
//!   SQL_ATTR_PARAMS_PROCESSED_PTR - handle SQL_PARAM_IGNORE and fix incorrect
//!   increment incase of failure row."), which moved the
//!   `*pRowsProcessed = iRow` write ahead of both the `iRow++` and the
//!   ignore-skip loop in `OnDone` so the pointer reports the set that just
//!   completed; it also added the `TestRowWiseParamArraysPaspAfterIgnore` and
//!   `RegressionsODBC` sibling tests. Reading the pre-fix code (PR 6629, PR
//!   6882) alone gives the opposite answer, and retail 18.6.2.1 predates all
//!   three.
//! - **The processed count includes the failing set.** Same PR: it reports the
//!   failing row 1-based rather than the next row or the full array.
//!
//! # Deliberate divergences
//!
//! - msodbcsql serialises every parameter set into **one** TDS batch; this
//!   driver issues one prepared RPC per row (AB#47820 scope). Observable
//!   difference: a client-side conversion failure in row *N* aborts before
//!   msodbcsql has sent anything, whereas here rows `0..N` have already run.
//! - Rows never reached after an abort keep whatever the application already
//!   had in its status array. msodbcsql behaves the same way (it only ever
//!   writes `SQL_PARAM_UNUSED` for explicitly ignored rows), even though the
//!   ODBC specification describes `SQL_PARAM_UNUSED` for this case too.
//! - Row-returning statements and data-at-execution parameters are refused with
//!   `HYC00`; msodbcsql supports both. See `docs/attributes_plan.md`.
//! - A row-returning statement is only detectable **after** its first set has
//!   run: there is no column metadata before the prepare. So
//!   `INSERT ... OUTPUT` with an array executes set 0 (durably, under
//!   autocommit) and only then reports `HYC00`. Data-at-execution, by contrast,
//!   is caught while converting set 0 and sends nothing.

use std::time::Instant;

use tracing::{debug, error};

use mssql_tds::connection::tds_client::{ResultSet, StatementResult, TdsClient};
use mssql_tds::error::Error as TdsError;
use mssql_tds::message::parameters::rpc_parameters::RpcParameter;

use super::exec_common::{
    ParamsWithDae, build_named_params_for_row, deduct_query_timeout, return_client_idle,
};
use super::ird::populate_ird;
use super::sqlstate::{
    ERR_OPTIONAL_FEATURE_NOT_IMPLEMENTED, SQLSTATE_HY000, post_diag, post_tds_error,
    post_tds_info_messages,
};
use crate::api::odbc_types::{
    SQL_ERROR, SQL_PARAM_ERROR, SQL_PARAM_IGNORE, SQL_PARAM_SUCCESS, SQL_PARAM_SUCCESS_WITH_INFO,
    SQL_PARAM_UNUSED, SQL_SUCCESS, SQL_SUCCESS_WITH_INFO, SqlHandle, SqlReturn, SqlULen,
    SqlUSmallInt,
};
use crate::error::post_sql_error;
use crate::handles::stmt::{
    STMT_STATE_CURSOR_OPEN, STMT_STATE_EXEC_CONTEXT, STMT_STATE_EXEC_STARTED, StmtState,
};
use crate::handles::{DbcHandle, StmtHandle};

/// No parameter set has reported a count yet. msodbcsql's
/// `SQL_NO_ROWCOUNT_TOTAL` (`sqlsrv.h:284`), and the value `SQLRowCount`
/// reports when a batch produced no counts at all.
const NO_ROWCOUNT_TOTAL: i64 = -1;

/// The application-owned parameter-array control block, resolved once per
/// execute.
///
/// ODBC hands these as raw pointers into application memory and reads them at
/// execute time, not at set time, so the values are captured here and every
/// access is unaligned — the ODBC contract never promises alignment.
pub(super) struct ParamArray {
    /// `SQL_ATTR_PARAMSET_SIZE`: number of parameter sets to execute.
    pub(super) paramset_size: usize,
    status_ptr: *mut SqlUSmallInt,
    processed_ptr: *mut SqlULen,
    operation_ptr: *const SqlUSmallInt,
}

impl ParamArray {
    pub(super) fn from_state(stmt_state: &StmtState) -> Self {
        Self {
            paramset_size: stmt_state.paramset_size,
            status_ptr: stmt_state.inert_attrs.param_status_ptr(),
            processed_ptr: stmt_state.inert_attrs.params_processed_ptr(),
            operation_ptr: stmt_state.inert_attrs.param_operation_ptr(),
        }
    }

    /// Whether the application supplied `SQL_ATTR_PARAM_STATUS_PTR`. This alone
    /// decides whether a failed row is reported as `SQL_ERROR` or downgraded to
    /// `SQL_SUCCESS_WITH_INFO` — see the module docs.
    fn reports_row_status(&self) -> bool {
        !self.status_ptr.is_null()
    }

    /// Whether the application asked for this row to be skipped.
    ///
    /// # Safety
    /// When set, `SQL_ATTR_PARAM_OPERATION_PTR` must address `paramset_size`
    /// readable `SQLUSMALLINT`s, per `SQLSetStmtAttr`.
    fn is_ignored(&self, row: usize) -> bool {
        if self.operation_ptr.is_null() {
            return false;
        }
        unsafe { self.operation_ptr.add(row).read_unaligned() == SQL_PARAM_IGNORE }
    }

    /// # Safety
    /// When set, `SQL_ATTR_PARAM_STATUS_PTR` must address `paramset_size`
    /// writable `SQLUSMALLINT`s, per `SQLSetStmtAttr`.
    fn set_status(&self, row: usize, status: SqlUSmallInt) {
        if self.status_ptr.is_null() {
            return;
        }
        unsafe { self.status_ptr.add(row).write_unaligned(status) };
    }

    /// # Safety
    /// When set, `SQL_ATTR_PARAMS_PROCESSED_PTR` must address one writable
    /// `SQLULEN`, per `SQLSetStmtAttr`.
    fn publish_processed(&self, count: usize) {
        if self.processed_ptr.is_null() {
            return;
        }
        unsafe { self.processed_ptr.write_unaligned(count) };
    }
}

/// What one parameter set did.
enum RowOutcome {
    /// The row ran; `info` records whether the server sent messages with it.
    Ran { info: bool },
    /// The row failed server-side. The connection is still on a token boundary,
    /// so the remaining rows are still attempted (msodbcsql parity).
    ServerError,
    /// The execution cannot continue: a transport failure, an exhausted
    /// timeout, a client-side conversion error, or an unsupported shape.
    Abort,
}

/// Running totals across the parameter sets of one execute.
pub(super) struct ArrayOutcome {
    /// Summed affected rows, or [`NO_ROWCOUNT_TOTAL`] when no set reported one.
    pub(super) rows_affected: i64,
    /// Parameter sets attempted, including skipped ones — the value published
    /// through `SQL_ATTR_PARAMS_PROCESSED_PTR`.
    pub(super) rows_processed: usize,
    any_error: bool,
    any_info: bool,
    aborted: bool,
    reports_row_status: bool,
}

impl ArrayOutcome {
    /// The ODBC return code for the whole execute.
    ///
    /// An abort is always `SQL_ERROR`: it is a client-side or transport failure
    /// that left parameter sets unexecuted, which no status array explains. A
    /// per-row server error is downgraded to `SQL_SUCCESS_WITH_INFO` only when
    /// the application can read `SQL_PARAM_ERROR` back out of its status array
    /// (`sqlctokn.cpp:2341-2350`).
    pub(super) fn return_code(&self) -> SqlReturn {
        if self.aborted {
            SQL_ERROR
        } else if self.any_error {
            if self.reports_row_status {
                SQL_SUCCESS_WITH_INFO
            } else {
                SQL_ERROR
            }
        } else if self.any_info {
            SQL_SUCCESS_WITH_INFO
        } else {
            SQL_SUCCESS
        }
    }

    fn add_count(&mut self, count: i64) {
        if self.rows_affected == NO_ROWCOUNT_TOTAL {
            self.rows_affected = count;
        } else {
            self.rows_affected += count;
        }
    }
}

/// Everything one parameter-array execute needs besides the connection and the
/// per-row send closure.
///
/// `budget`/`started` are the single `SQL_ATTR_QUERY_TIMEOUT` allowance shared
/// by every set, so a long array cannot outlive the timeout the application
/// asked for by restarting it per row.
pub(super) struct ArrayExec<'a> {
    pub(super) dbc: &'a DbcHandle,
    pub(super) stmt: &'a StmtHandle,
    pub(super) array: &'a ParamArray,
    pub(super) marker_count: usize,
    pub(super) budget: u32,
    pub(super) started: Instant,
    pub(super) op: &'a str,
}

/// Executes every parameter set through `run_row`, which sends exactly one set
/// and returns where the batch is positioned.
///
/// # Safety
/// Every bound parameter's buffers must be readable at each of the
/// `paramset_size` row positions the configured strides select, and the three
/// parameter-array pointers must satisfy their `SQLSetStmtAttr` contracts.
pub(super) unsafe fn execute_rows<F>(
    ctx: &ArrayExec<'_>,
    client: &mut TdsClient,
    mut run_row: F,
) -> ArrayOutcome
where
    F: FnMut(&mut TdsClient, Vec<RpcParameter>, u32) -> Result<StatementResult, TdsError>,
{
    let ArrayExec {
        dbc,
        stmt,
        array,
        marker_count,
        budget,
        started,
        op,
    } = *ctx;
    let mut outcome = ArrayOutcome {
        rows_affected: NO_ROWCOUNT_TOTAL,
        rows_processed: 0,
        any_error: false,
        any_info: false,
        aborted: false,
        reports_row_status: array.reports_row_status(),
    };

    for row in 0..array.paramset_size {
        // Read per row at execute time: the application may rewrite the
        // operation array between executes without rebinding.
        if array.is_ignored(row) {
            array.set_status(row, SQL_PARAM_UNUSED);
            outcome.rows_processed = row + 1;
            array.publish_processed(outcome.rows_processed);
            continue;
        }

        let params = match unsafe { build_row_params(stmt, marker_count, row, op) } {
            Ok(params) => params,
            Err(()) => {
                // The failing set still counts as processed: msodbcsql's
                // Variation_76 expects the counter to reach the ordinal of the
                // set that failed, and ODBC defines it as "including error
                // sets".
                array.set_status(row, SQL_PARAM_ERROR);
                outcome.rows_processed = row + 1;
                array.publish_processed(outcome.rows_processed);
                outcome.any_error = true;
                outcome.aborted = true;
                break;
            }
        };

        // Charged against the cumulative elapsed time of the whole execute, so
        // every row shares one budget rather than restarting it.
        let remaining = match deduct_query_timeout(budget, started.elapsed()) {
            Ok(remaining) => remaining,
            Err(()) => {
                error!("{op}: query timeout expired before parameter set {row}");
                if let Ok(mut stmt_state) = stmt.inner.lock() {
                    post_tds_error(
                        &mut stmt_state,
                        &super::exec_common::query_timeout_expired_error(),
                        SQLSTATE_HY000,
                    );
                }
                array.set_status(row, SQL_PARAM_ERROR);
                outcome.rows_processed = row + 1;
                array.publish_processed(outcome.rows_processed);
                outcome.any_error = true;
                outcome.aborted = true;
                break;
            }
        };

        let row_outcome =
            unsafe { run_one_row(dbc, stmt, client, &mut run_row, params, remaining, op) };

        outcome.rows_processed = row + 1;
        array.publish_processed(outcome.rows_processed);

        match row_outcome {
            RowOutcome::Ran { info } => {
                let counts = client.take_dml_result_counts();
                for count in counts {
                    outcome.add_count(count);
                }
                if info {
                    outcome.any_info = true;
                    array.set_status(row, SQL_PARAM_SUCCESS_WITH_INFO);
                } else {
                    array.set_status(row, SQL_PARAM_SUCCESS);
                }
            }
            RowOutcome::ServerError => {
                array.set_status(row, SQL_PARAM_ERROR);
                outcome.any_error = true;
            }
            RowOutcome::Abort => {
                array.set_status(row, SQL_PARAM_ERROR);
                outcome.any_error = true;
                outcome.aborted = true;
                break;
            }
        }
    }

    outcome
}

/// Builds one row's parameter list under the STMT lock, never holding it across
/// I/O. Diagnostics for a failure are posted by `build_named_params_for_row`.
///
/// # Safety
/// As [`execute_rows`]: this dereferences the application's bound parameter
/// buffers at row `row`.
unsafe fn build_row_params(
    stmt: &StmtHandle,
    marker_count: usize,
    row: usize,
    op: &str,
) -> Result<Vec<RpcParameter>, ()> {
    let Ok(mut stmt_state) = stmt.inner.lock() else {
        error!("{op}: stmt mutex poisoned building parameter set {row}");
        return Err(());
    };
    let ParamsWithDae { params, dae_params } =
        unsafe { build_named_params_for_row(&mut stmt_state, marker_count, row, op) }
            .map_err(|_| ())?;

    // Streaming a value needs SQLParamData/SQLPutData to drive one row at a
    // time, which cannot be reconciled with executing the whole array inside a
    // single ODBC call. msodbcsql supports the combination; refusing it keeps
    // this path from silently sending a placeholder with no data behind it.
    if !dae_params.is_empty() {
        error!("{op}: data-at-execution is not supported with parameter arrays");
        post_diag(&mut stmt_state, ERR_OPTIONAL_FEATURE_NOT_IMPLEMENTED);
        return Err(());
    }
    Ok(params)
}

/// Sends one parameter set and leaves the connection idle and ready for the
/// next one.
///
/// # Safety
/// As [`execute_rows`]: `params` was built from the application's bound
/// buffers and the statement's parameter-array pointers must still be valid.
unsafe fn run_one_row<F>(
    dbc: &DbcHandle,
    stmt: &StmtHandle,
    client: &mut TdsClient,
    run_row: &mut F,
    params: Vec<RpcParameter>,
    remaining: u32,
    op: &str,
) -> RowOutcome
where
    F: FnMut(&mut TdsClient, Vec<RpcParameter>, u32) -> Result<StatementResult, TdsError>,
{
    let result = match run_row(client, params, remaining) {
        Ok(result) => result,
        Err(e) => {
            // A server-reported error leaves the reader on a token boundary and
            // the batch closed, so the next parameter set can still be sent —
            // the same property `catalog.rs` relies on to retry an unqualified
            // catalog call. Anything else (transport, protocol, timeout) has
            // desynchronised or retired the connection.
            let recoverable =
                matches!(e, TdsError::SqlServerError { .. }) && !client.is_connection_dead();
            let info_messages = client.take_info_messages();
            if let Ok(mut stmt_state) = stmt.inner.lock() {
                post_tds_error(&mut stmt_state, &e, SQLSTATE_HY000);
                post_tds_info_messages(&mut stmt_state, &info_messages);
            }
            if recoverable {
                // `catalog.rs` re-executes after a server error but still
                // advances the batch if one is left open; do the same rather
                // than assume every ERROR token closed it. Queued counts are
                // dropped here too, so a failed set cannot roll its counts into
                // a later set's total or ride the pooled client to an unrelated
                // statement.
                //
                // A drain that itself fails leaves the reader at an unknown
                // offset, so the next set must not be sent into it — same rule
                // as the success path below.
                if client.has_open_batch()
                    && let Err(drain) = dbc.runtime.block_on(client.close_query())
                {
                    error!(%drain, "{op}: draining a failed parameter set failed");
                    return RowOutcome::Abort;
                }
                let _ = client.take_dml_result_counts();
                debug!(%e, "{op}: parameter set failed server-side, continuing");
                return RowOutcome::ServerError;
            }
            error!(%e, "{op}: parameter set failed unrecoverably");
            return RowOutcome::Abort;
        }
    };

    if matches!(result, StatementResult::Rows) {
        // Executing the next row would have to discard this row set, and
        // keeping it would strand the connection mid-cursor for the remaining
        // rows. msodbcsql surfaces one result set per row via SQLMoreResults
        // because it sends the whole array as a single batch.
        error!("{op}: row-returning statements are not supported with parameter arrays");
        let _ = dbc.runtime.block_on(client.close_query());
        if let Ok(mut stmt_state) = stmt.inner.lock() {
            post_diag(&mut stmt_state, ERR_OPTIONAL_FEATURE_NOT_IMPLEMENTED);
        }
        return RowOutcome::Abort;
    }

    // Leave the connection idle so the next parameter set can be sent. This
    // also collects the trailing DONE counts that `take_dml_result_counts`
    // reports to the caller.
    if let Err(e) = dbc.runtime.block_on(client.close_query()) {
        error!(%e, "{op}: draining a parameter set failed");
        if let Ok(mut stmt_state) = stmt.inner.lock() {
            post_tds_error(&mut stmt_state, &e, SQLSTATE_HY000);
        }
        return RowOutcome::Abort;
    }

    let info_messages = client.take_info_messages();
    let info = !info_messages.is_empty();
    if info && let Ok(mut stmt_state) = stmt.inner.lock() {
        post_tds_info_messages(&mut stmt_state, &info_messages);
    }
    RowOutcome::Ran { info }
}

/// Publishes the processed count for a scalar (single parameter set) execute.
///
/// msodbcsql writes this whether or not parameter arrays are in play: the
/// non-bulk branch sets `iRowEnd = 1` (`sqlccmd.cpp:3203`) and the row loop
/// then writes `*pRowsProcessed = 1` (`:3212`), with four more explicit `= 1`
/// sites (`sqlccmd.cpp:1688`, `:3493`, `:6700`, `deprecate.cpp:2114`) covering
/// the paths that skip that loop. It is written before the set is converted or
/// sent, so a set that then fails is still counted.
///
/// Without this, `executemany(sql, [one_row])` — which sets `PARAMSET_SIZE = 1`
/// and a processed pointer — would read back whatever was in its own buffer.
pub(super) fn publish_scalar_processed(stmt_state: &StmtState) {
    ParamArray::from_state(stmt_state).publish_processed(1);
}

/// Publishes a completed parameter-array execute onto the statement.
///
/// Every parameter set was drained to idle by [`execute_rows`], so there is no
/// cursor to leave open and nothing pending on the wire: the statement is left
/// exactly as a no-result-set execute leaves it, with the summed affected-row
/// count available to `SQLRowCount` and the connection returned to the pool.
pub(super) fn finish(
    dbc: &DbcHandle,
    stmt: &StmtHandle,
    statement_handle: SqlHandle,
    client: TdsClient,
    outcome: &ArrayOutcome,
    op: &str,
) -> SqlReturn {
    let metadata = client.get_metadata().clone();
    let ird_ok = populate_ird(stmt, &metadata).is_ok();

    let Ok(mut stmt_state) = stmt.inner.lock() else {
        error!("{op}: stmt mutex poisoned finishing parameter-array execute");
        return_client_idle(dbc, statement_handle, client);
        return SQL_ERROR;
    };
    stmt_state.begin_batch(metadata);
    stmt_state.row_count = outcome.rows_affected;
    stmt_state.pending_row_counts.clear();
    stmt_state.clear_exhaustion_state();
    stmt_state.set_state(STMT_STATE_EXEC_CONTEXT);
    stmt_state.clear_state(STMT_STATE_CURSOR_OPEN | STMT_STATE_EXEC_STARTED);
    drop(stmt_state);

    return_client_idle(dbc, statement_handle, client);

    if !ird_ok {
        if let Ok(mut stmt_state) = stmt.inner.lock() {
            post_sql_error(
                &mut stmt_state,
                SQLSTATE_HY000,
                0,
                "Internal error refreshing result-set metadata",
            );
        }
        return SQL_ERROR;
    }
    outcome.return_code()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn array(
        paramset_size: usize,
        status: *mut SqlUSmallInt,
        processed: *mut SqlULen,
        operation: *const SqlUSmallInt,
    ) -> ParamArray {
        ParamArray {
            paramset_size,
            status_ptr: status,
            processed_ptr: processed,
            operation_ptr: operation,
        }
    }

    fn outcome(reports_row_status: bool) -> ArrayOutcome {
        ArrayOutcome {
            rows_affected: NO_ROWCOUNT_TOTAL,
            rows_processed: 0,
            any_error: false,
            any_info: false,
            aborted: false,
            reports_row_status,
        }
    }

    /// The sentinel is replaced by the first count rather than added to, so a
    /// single-row batch reports its own count and not `count - 1`.
    #[test]
    fn the_first_count_replaces_the_sentinel_and_later_counts_add() {
        let mut o = outcome(false);
        assert_eq!(o.rows_affected, -1);
        o.add_count(3);
        assert_eq!(o.rows_affected, 3);
        o.add_count(4);
        assert_eq!(o.rows_affected, 7);
    }

    /// A batch where no statement carried a count keeps the sentinel, which is
    /// what `SQLRowCount` reports as "not available".
    #[test]
    fn no_counted_row_leaves_the_sentinel() {
        assert_eq!(outcome(false).rows_affected, -1);
    }

    /// Zero-count rows are still counts: they must promote the sentinel to 0,
    /// not leave it at -1.
    #[test]
    fn a_zero_count_promotes_the_sentinel() {
        let mut o = outcome(false);
        o.add_count(0);
        assert_eq!(o.rows_affected, 0);
        o.add_count(0);
        assert_eq!(o.rows_affected, 0);
    }

    /// The measured msodbcsql downgrade rule: the status array is the only
    /// thing that makes a row failure visible, so it alone decides whether the
    /// call reports SQL_ERROR.
    #[test]
    fn a_row_error_is_downgraded_only_when_a_status_array_can_report_it() {
        let mut with_array = outcome(true);
        with_array.any_error = true;
        assert_eq!(with_array.return_code(), SQL_SUCCESS_WITH_INFO);

        let mut without_array = outcome(false);
        without_array.any_error = true;
        assert_eq!(without_array.return_code(), SQL_ERROR);
    }

    /// An abort left parameter sets unexecuted, which no status array explains,
    /// so it outranks the downgrade.
    #[test]
    fn an_abort_reports_sql_error_even_with_a_status_array() {
        let mut o = outcome(true);
        o.any_error = true;
        o.aborted = true;
        assert_eq!(o.return_code(), SQL_ERROR);
    }

    #[test]
    fn info_without_error_is_success_with_info() {
        let mut o = outcome(false);
        o.any_info = true;
        assert_eq!(o.return_code(), SQL_SUCCESS_WITH_INFO);
        assert_eq!(outcome(false).return_code(), SQL_SUCCESS);
    }

    /// A null operation array means every row proceeds; ODBC only skips rows
    /// the application explicitly marked.
    #[test]
    fn a_null_operation_array_ignores_nothing() {
        let a = array(
            4,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null(),
        );
        assert!(!a.is_ignored(0));
        assert!(!a.is_ignored(3));
        assert!(!a.reports_row_status());
    }

    /// Only `SQL_PARAM_IGNORE` skips. `SQL_PARAM_PROCEED` (0) and any other
    /// value proceed, so a zeroed array behaves as "run everything".
    #[test]
    fn only_the_ignore_value_skips_a_row() {
        let ops: [SqlUSmallInt; 4] = [0, SQL_PARAM_IGNORE, 2, 9];
        let a = array(4, std::ptr::null_mut(), std::ptr::null_mut(), ops.as_ptr());
        assert!(!a.is_ignored(0));
        assert!(a.is_ignored(1));
        assert!(!a.is_ignored(2));
        assert!(!a.is_ignored(3));
    }

    #[test]
    fn status_and_processed_writes_land_at_the_right_slots() {
        let mut status: [SqlUSmallInt; 3] = [99, 99, 99];
        let mut processed: SqlULen = 0;
        let a = array(3, status.as_mut_ptr(), &raw mut processed, std::ptr::null());
        assert!(a.reports_row_status());

        a.set_status(1, SQL_PARAM_ERROR);
        a.publish_processed(2);

        assert_eq!(status, [99, SQL_PARAM_ERROR, 99]);
        assert_eq!(processed, 2);
    }

    /// Every pointer is optional; a run with none of them set must not fault.
    #[test]
    fn writes_through_null_pointers_are_dropped() {
        let a = array(
            2,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null(),
        );
        a.set_status(0, SQL_PARAM_SUCCESS);
        a.publish_processed(2);
    }

    /// ODBC never promises an aligned application buffer, so both arrays are
    /// written unaligned.
    #[test]
    fn misaligned_status_and_processed_pointers_are_written() {
        let mut storage = [0u8; 64];
        let status = storage.as_mut_ptr().wrapping_add(1).cast::<SqlUSmallInt>();
        let processed = storage.as_mut_ptr().wrapping_add(19).cast::<SqlULen>();
        let a = array(2, status, processed, std::ptr::null());

        a.set_status(1, SQL_PARAM_SUCCESS_WITH_INFO);
        a.publish_processed(2);

        assert_eq!(
            unsafe { status.add(1).read_unaligned() },
            SQL_PARAM_SUCCESS_WITH_INFO
        );
        assert_eq!(unsafe { processed.read_unaligned() }, 2);
    }

    /// A misaligned operation array is read the same way.
    #[test]
    fn a_misaligned_operation_array_is_read_unaligned() {
        let mut storage = [0u8; 32];
        let ops = storage.as_mut_ptr().wrapping_add(1).cast::<SqlUSmallInt>();
        unsafe { ops.add(2).write_unaligned(SQL_PARAM_IGNORE) };
        let a = array(3, std::ptr::null_mut(), std::ptr::null_mut(), ops);

        assert!(!a.is_ignored(0));
        assert!(!a.is_ignored(1));
        assert!(a.is_ignored(2));
    }

    /// The whole control block must come off the statement attributes the
    /// application actually set, through the real `SQLSetStmtAttr` surface —
    /// a plan built from the wrong attribute ids would silently execute one
    /// row and report nothing.
    #[test]
    fn from_state_reads_every_parameter_array_attribute() {
        use crate::api::odbc_types::{
            SQL_ATTR_PARAM_OPERATION_PTR, SQL_ATTR_PARAM_STATUS_PTR, SQL_ATTR_PARAMS_PROCESSED_PTR,
            SQL_SUCCESS,
        };
        use crate::api::set_stmt_attr::sql_set_stmt_attr_w;
        use crate::handles::{StmtHandle, handle_from_raw};
        use crate::test_support::TestHandles;

        let h = TestHandles::with_env_dbc_stmt();
        let stmt = unsafe { handle_from_raw::<StmtHandle>(h.stmt) };

        let mut status: [SqlUSmallInt; 2] = [0, 0];
        let mut processed: SqlULen = 0;
        let mut operation: [SqlUSmallInt; 2] = [SQL_PARAM_IGNORE, 0];

        for (attribute, value) in [
            (SQL_ATTR_PARAM_STATUS_PTR, status.as_mut_ptr() as SqlULen),
            (
                SQL_ATTR_PARAMS_PROCESSED_PTR,
                (&raw mut processed) as SqlULen,
            ),
            (
                SQL_ATTR_PARAM_OPERATION_PTR,
                operation.as_mut_ptr() as SqlULen,
            ),
        ] {
            assert_eq!(
                unsafe { sql_set_stmt_attr_w(h.stmt, attribute, value as *mut _, 0) },
                SQL_SUCCESS
            );
        }
        stmt.inner.lock().unwrap().paramset_size = 2;

        let plan = ParamArray::from_state(&stmt.inner.lock().unwrap());

        assert_eq!(plan.paramset_size, 2);
        assert!(plan.reports_row_status());
        assert!(plan.is_ignored(0), "operation array must be wired through");
        assert!(!plan.is_ignored(1));

        plan.set_status(1, SQL_PARAM_ERROR);
        plan.publish_processed(2);
        assert_eq!(status[1], SQL_PARAM_ERROR);
        assert_eq!(processed, 2);
    }

    /// The default statement has none of the three pointers, so an array run
    /// on it must report nothing and skip nothing.
    #[test]
    fn from_state_defaults_to_no_pointers() {
        use crate::handles::{StmtHandle, handle_from_raw};
        use crate::test_support::TestHandles;

        let h = TestHandles::with_env_dbc_stmt();
        let stmt = unsafe { handle_from_raw::<StmtHandle>(h.stmt) };
        let plan = ParamArray::from_state(&stmt.inner.lock().unwrap());

        assert_eq!(plan.paramset_size, 1);
        assert!(!plan.reports_row_status());
        assert!(!plan.is_ignored(0));
    }
}
