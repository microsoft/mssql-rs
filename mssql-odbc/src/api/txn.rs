// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Connection-scoped transaction plumbing shared by `SQLEndTran`,
//! `SQLSetConnectAttr`, `SQLDisconnect`, and the execution path.
//!
//! The driver follows msodbcsql's Yukon+ (SQL Server 2005 and later) model: all
//! transaction control travels as TDS transaction-manager requests rather than
//! `SET IMPLICIT_TRANSACTIONS` batches (`sqlcconn.cpp:3692`). Isolation level is
//! the exception — msodbcsql sends it as a `SET TRANSACTION ISOLATION LEVEL`
//! batch (`sqlcmisc.cpp:1760`), so the transaction-manager request carries
//! `NoChange` and inherits the session setting.

use mssql_tds::connection::tds_client::TdsClient;
use mssql_tds::message::transaction_management::TransactionIsolationLevel;
use tracing::{debug, error};

use super::close_cursor::close_cursor_for_connection_op;
use super::odbc_types::{
    SQL_AUTOCOMMIT_OFF, SQL_AUTOCOMMIT_ON, SQL_ERROR, SQL_RESET_CONNECTION_YES, SQL_SUCCESS,
    SQL_SUCCESS_WITH_INFO, SQL_TXN_READ_COMMITTED, SQL_TXN_READ_UNCOMMITTED,
    SQL_TXN_REPEATABLE_READ, SQL_TXN_SERIALIZABLE, SQL_TXN_SS_SNAPSHOT, SqlReturn,
};
use super::sqlstate::{
    ERR_ATTRIBUTE_CANNOT_BE_SET_NOW, ERR_CONNECTION_BUSY, ERR_CONNECTION_DOES_NOT_EXIST,
    ERR_INVALID_ATTRIBUTE_VALUE, ERR_NO_ACTIVE_TDS_CLIENT, ERR_OPTIONAL_FEATURE_NOT_IMPLEMENTED,
    SQLSTATE_08S01, SQLSTATE_01000, SQLSTATE_08007, SQLSTATE_HY000, WARN_TRANSACTION_COMMITTED,
    post_diag, post_tds_error,
};
use crate::error::{HasDiagnostics, free_errors, post_sql_error};
use crate::handles::DbcHandle;
use crate::handles::dbc::ConnectionState;
use crate::handles::{StmtHandle, handle_from_raw};

/// Maps an ODBC `SQL_TXN_*` bit to the T-SQL clause msodbcsql emits for it
/// (`sqlcstr.cpp:56-60`). `None` for any value outside the accepted set, which
/// the caller reports as `HYC00`.
pub(super) fn txn_isolation_to_tsql(level: u32) -> Option<&'static str> {
    match level {
        SQL_TXN_READ_UNCOMMITTED => Some("SET TRANSACTION ISOLATION LEVEL READ UNCOMMITTED"),
        SQL_TXN_READ_COMMITTED => Some("SET TRANSACTION ISOLATION LEVEL READ COMMITTED"),
        SQL_TXN_REPEATABLE_READ => Some("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ"),
        SQL_TXN_SERIALIZABLE => Some("SET TRANSACTION ISOLATION LEVEL SERIALIZABLE"),
        SQL_TXN_SS_SNAPSHOT => Some("SET TRANSACTION ISOLATION LEVEL SNAPSHOT"),
        _ => None,
    }
}

/// Reports a cursor that could not be closed ahead of a connection-scoped
/// transaction operation. The per-statement diagnostic already went to its own
/// STMT handle, which the application is not looking at here, so restate it on
/// the DBC.
fn fail_cursor_close(dbc: &DbcHandle, op: &str) -> SqlReturn {
    error!("{op}: could not close open cursors");
    let Ok(mut state) = dbc.inner.lock() else {
        return SQL_ERROR;
    };
    post_sql_error(
        &mut state,
        SQLSTATE_HY000,
        0,
        "An open cursor on this connection could not be closed, so the \
         transaction request could not be sent.",
    );
    SQL_ERROR
}

/// Claims the connection's TDS client for a connection-scoped operation that no
/// statement owns (commit, rollback, isolation change, autocommit change).
///
/// Unlike [`super::exec_common::claim_connection`] there is no statement handle
/// to attribute the claim to, so `active_stmt` is left alone; the caller must
/// have closed every cursor first (see [`close_all_cursors`]) so the connection
/// is genuinely idle. Returns `Err` with the diagnostic already posted to the
/// DBC.
pub(super) fn claim_dbc_client(dbc: &DbcHandle, op: &str) -> Result<TdsClient, SqlReturn> {
    let Ok(mut state) = dbc.inner.lock() else {
        error!("{op}: dbc mutex poisoned");
        return Err(SQL_ERROR);
    };
    if state.connection_state != ConnectionState::Connected {
        error!("{op}: DBC is not connected");
        post_diag(&mut state, ERR_CONNECTION_DOES_NOT_EXIST);
        return Err(SQL_ERROR);
    }
    if state.active_stmt.is_some() {
        error!("{op}: connection is busy with results for another statement");
        post_diag(&mut state, ERR_CONNECTION_BUSY);
        return Err(SQL_ERROR);
    }
    let Some(client) = state.client.take() else {
        error!("{op}: no active TDS client");
        post_diag(&mut state, ERR_NO_ACTIVE_TDS_CLIENT);
        return Err(SQL_ERROR);
    };
    Ok(client)
}

/// Returns a client claimed by [`claim_dbc_client`].
///
/// If the DBC mutex is poisoned the client cannot be stored and is dropped, so
/// the connection is marked disconnected rather than left as `Connected` with
/// `client == None`. That combination otherwise reads as "claimed, not known
/// dead" to `SQL_ATTR_CONNECTION_DEAD`, and a pool would keep handing out a DBC
/// on which every operation fails with `ERR_NO_ACTIVE_TDS_CLIENT`.
pub(super) fn release_dbc_client(dbc: &DbcHandle, client: TdsClient) {
    match dbc.inner.lock() {
        Ok(mut state) => state.client = Some(client),
        Err(mut poisoned) => {
            error!("release_dbc_client: dbc mutex poisoned; marking DBC disconnected");
            // The lock guard is still reachable through the poison error, so the
            // state can be corrected even though the mutex is poisoned.
            poisoned.get_mut().connection_state = ConnectionState::Disconnected;
        }
    }
}

/// Runs `sql` as a language batch on an already-claimed client and drains the
/// response. Draining matters: the TDS layer refuses a transaction-manager
/// request while a batch is still open.
pub(super) fn exec_batch(
    dbc: &DbcHandle,
    client: &mut TdsClient,
    sql: &str,
) -> Result<(), mssql_tds::error::Error> {
    dbc.runtime.block_on(async {
        client.execute(sql.to_string(), ()).await?;
        client.close_query().await
    })
}

/// Closes every open cursor on the connection, mirroring msodbcsql's
/// `SQLFreeStmt(SQL_CLOSE)` sweep in `CommitAbortTran` (`sqlctran.cpp:302-323`)
/// and honoring the `SQL_CB_CLOSE` this driver advertises.
///
/// Routes through [`close_cursor_for_connection_op`] rather than the public
/// `SQLFreeStmt(SQL_CLOSE)` purely to keep the sweep cheap — statements with no
/// open cursor cost a single lock. The observable effect is the same as
/// msodbcsql's, statement diagnostics included.
///
/// Returns `SQL_ERROR` if any cursor could not be closed. Callers must not
/// proceed in that case: a statement whose result stream did not drain leaves
/// the connection mid-batch, and the transaction-manager request that follows
/// would fail with a usage error.
#[must_use]
pub(super) fn close_all_cursors(dbc: &DbcHandle) -> SqlReturn {
    let statements = match dbc.inner.lock() {
        Ok(state) => state.statements.clone(),
        Err(_) => {
            error!("close_all_cursors: dbc mutex poisoned");
            return SQL_ERROR;
        }
    };
    let mut worst = SQL_SUCCESS;
    for stmt_ptr in statements {
        // SAFETY: every pointer in `statements` came from
        // `handle_to_raw::<StmtHandle>` and is owned by this DBC.
        // A concurrent `SQLFreeHandle(SQL_HANDLE_STMT)` could still free it
        // between the clone above and this call — the same handle-lifetime gap
        // `SQLDisconnect` documents (see the TODO in `disconnect.rs`), which
        // refcounted handles will close for the whole driver at once.
        let stmt = unsafe { handle_from_raw::<StmtHandle>(stmt_ptr) };
        if close_cursor_for_connection_op(stmt, stmt_ptr) == SQL_ERROR {
            error!(?stmt_ptr, "close_all_cursors: could not close cursor");
            worst = SQL_ERROR;
        }
    }
    worst
}

/// Commits or rolls back the connection's transaction — the shared core of
/// `SQLEndTran` and the autocommit OFF→ON transition.
///
/// Reproduces `CommitAbortTran` (`sqlctran.cpp:276-375`): with no user
/// transaction started there is no transaction-manager request and the result is
/// a **silent success**, never a warning or error.
pub(super) fn end_transaction(dbc: &DbcHandle, commit: bool, op: &str) -> SqlReturn {
    let started = {
        let Ok(mut state) = dbc.inner.lock() else {
            error!("{op}: dbc mutex poisoned");
            return SQL_ERROR;
        };
        if state.connection_state != ConnectionState::Connected {
            error!("{op}: DBC is not connected");
            post_diag(&mut state, ERR_CONNECTION_DOES_NOT_EXIST);
            return SQL_ERROR;
        }
        state.local_tran_started
    };

    // The sweep runs on every successful `SQLEndTran`, including the
    // no-transaction no-op, because this driver advertises `SQL_CB_CLOSE`
    // unconditionally and the Driver Manager marks every statement on the
    // connection cursor-closed on the strength of that. The combination is
    // reachable in plain autocommit mode: nothing there ever sets
    // `local_tran_started`, and no other path sweeps, so a cursor left open
    // across `SQLEndTran` would keep `active_stmt` claimed behind the DM's back
    // and lock out every other statement on the connection.
    //
    // msodbcsql returns before its own sweep here (`sqlctran.cpp:293` precedes
    // `302-323`) and is wedged by the same sequence, but it advertises
    // `SQL_CB_PRESERVE`, so the DM never closes cursors on its behalf. Both
    // drivers are internally consistent; they just report different truths about
    // themselves.
    if close_all_cursors(dbc) == SQL_ERROR {
        return fail_cursor_close(dbc, op);
    }

    // msodbcsql `sqlctran.cpp:293`: nothing started, so no TM request.
    if !started {
        debug!("{op}: no transaction started — no server request needed");
        return SQL_SUCCESS;
    }

    let mut client = match claim_dbc_client(dbc, op) {
        Ok(c) => c,
        Err(ret) => return ret,
    };

    // The server transaction can already be gone (XACT_ABORT, deadlock victim,
    // or a raw T-SQL ROLLBACK), in which case the flag is stale and the TM
    // request would fail. msodbcsql guards the same way via FIsLocalTranActive.
    let result = if client.has_active_transaction() {
        dbc.runtime.block_on(async {
            if commit {
                client.commit_transaction(None, None).await
            } else {
                client.rollback_transaction(None, None).await
            }
        })
    } else {
        debug!("{op}: no server-side transaction active — clearing stale flag");
        Ok(())
    };

    release_dbc_client(dbc, client);

    let Ok(mut state) = dbc.inner.lock() else {
        error!("{op}: dbc mutex poisoned");
        return SQL_ERROR;
    };
    // Cleared unconditionally, matching msodbcsql's `Return:` label, so a failed
    // commit cannot strand the connection permanently un-disconnectable.
    state.local_tran_started = false;

    if let Err(e) = result {
        error!(%e, "{op}: transaction-manager request failed");
        // 08007 is ODBC's specific state for "connection failure during
        // transaction": the commit or rollback did not reach the server, so its
        // outcome is unknown to the application. Neither 08007 nor HY000 puts
        // the connection into the suspended state, so this is a strictly more
        // precise diagnostic, not a behavioural change.
        post_tds_error(&mut state, &e, SQLSTATE_08007);
        return SQL_ERROR;
    }

    debug!(
        "{op}: transaction {} complete",
        if commit { "commit" } else { "rollback" }
    );
    SQL_SUCCESS
}

/// Begins a transaction on an already-claimed client when the connection is in
/// manual-commit mode and the server has none active, then marks the connection
/// as holding user work. Called immediately before every statement execution;
/// on failure the caller unwinds through `fail_with_tds`.
///
/// Mirrors msodbcsql's `CheckOptions` (`sqlccmd.cpp:10572-10585`). Running it
/// per statement — rather than only at the autocommit switch — is what recovers
/// from a transaction the server aborted or the application rolled back with
/// raw T-SQL.
///
/// The transaction-manager request carries `NoChange` so it inherits the
/// session isolation level already applied by `SET TRANSACTION ISOLATION LEVEL`.
///
/// This runs before every statement, so it is a hot path: both steady states
/// (autocommit on, and manual-commit with a transaction already open and
/// recorded) cost exactly one lock and no network round trip. The DBC lock is
/// never held across the `begin_transaction` await.
pub(super) fn begin_transaction_if_manual(
    dbc: &DbcHandle,
    client: &mut TdsClient,
    op: &str,
) -> Result<(), mssql_tds::error::Error> {
    let (autocommit, already_recorded) = match dbc.inner.lock() {
        Ok(state) => (state.autocommit, state.local_tran_started),
        Err(_) => {
            error!("{op}: dbc mutex poisoned reading autocommit");
            return Err(mssql_tds::error::Error::ImplementationError(
                "connection state is poisoned".to_string(),
            ));
        }
    };
    if autocommit {
        return Ok(());
    }

    if client.has_active_transaction() {
        // Steady state: transaction already open and already recorded, so there
        // is nothing to write back and no reason to retake the lock.
        if already_recorded {
            return Ok(());
        }
    } else {
        debug!("{op}: manual-commit mode with no active transaction — beginning one");
        dbc.runtime
            .block_on(client.begin_transaction(TransactionIsolationLevel::NoChange, None))?;
    }

    // The transaction is open on the server at this point; failing to record it
    // would leak it past commit/rollback and past disconnect.
    let Ok(mut state) = dbc.inner.lock() else {
        error!("{op}: dbc mutex poisoned recording transaction state");
        return Err(mssql_tds::error::Error::ImplementationError(
            "connection state is poisoned".to_string(),
        ));
    };
    state.local_tran_started = true;
    Ok(())
}

/// Applies `SQL_ATTR_AUTOCOMMIT`.
///
/// Mirrors msodbcsql's `SetCommitModeOption` (`sqlcconn.cpp:3596-3779`) on its
/// Yukon+ branch: the mode change is carried by transaction-manager requests,
/// never by `SET IMPLICIT_TRANSACTIONS`.
pub(super) fn set_autocommit(dbc: &DbcHandle, value: u64) -> SqlReturn {
    const OP: &str = "SQLSetConnectAttrW(SQL_ATTR_AUTOCOMMIT)";

    let enable = if value == u64::from(SQL_AUTOCOMMIT_ON) {
        true
    } else if value == u64::from(SQL_AUTOCOMMIT_OFF) {
        false
    } else {
        if let Ok(mut state) = dbc.inner.lock() {
            free_errors(&mut state);
            error!(value, "{OP}: invalid value");
            post_diag(&mut state, ERR_INVALID_ATTRIBUTE_VALUE);
        }
        return SQL_ERROR;
    };

    let connected = {
        let Ok(mut state) = dbc.inner.lock() else {
            error!("{OP}: dbc mutex poisoned");
            return SQL_ERROR;
        };
        free_errors(&mut state);
        // msodbcsql `sqlcmisc.cpp:1720`: re-setting the current mode is free.
        if state.autocommit == enable {
            debug!(enable, "{OP}: already in this mode");
            return SQL_SUCCESS;
        }
        // Before connect there is no session to change; the value is applied by
        // `apply_post_connect_txn_settings` once the login completes.
        if state.connection_state != ConnectionState::Connected {
            state.autocommit = enable;
            debug!(enable, "{OP}: stored for next connect");
            return SQL_SUCCESS;
        }
        true
    };
    debug_assert!(connected);

    if enable {
        switch_to_autocommit(dbc, OP)
    } else {
        switch_to_manual_commit(dbc, OP)
    }
}

/// Applies the reset performed while a pooled connection is acquired for reuse.
///
/// Mirrors msodbcsql's `SQL_COPT_SS_RESET_CONNECTION` handler
/// (`sqlcmisc.cpp:2373-2461`): reject any value but `SQL_RESET_CONNECTION_YES`
/// with HY024 (D7), roll back a live local transaction first (D4), then arm the
/// session reset. Pool checkout does not preserve transactions, so this never
/// uses RESETCONNECTIONSKIPTRAN.
///
/// The reset is armed without I/O. RESETCONNECTION is a TDS packet-header bit
/// (MS-TDS 2.2.3.1.2), and the server resets the session before executing the
/// request that carries it.
///
/// The two properties that would otherwise be lost to deferral are kept without
/// the round trip:
/// - **No stale client-side state.** `prepare_reset_connection` drops the
///   session-bound caches at arm time, so a borrower can never be served from a
///   prepared handle the reset is about to invalidate.
/// - **Fail at checkout.** `pending_reset_ack` suppresses the same-value
///   short circuit in `set_txn_isolation`, so the pool's checkout
///   `SET TRANSACTION ISOLATION LEVEL READ COMMITTED` (D9/#343) always emits and
///   carries the armed bit. A failed reset therefore surfaces inside the pool's
///   `reset()` — before the connection is handed out — and that handler verifies
///   the server acknowledged it rather than assuming the batch succeeding
///   implies it.
///
/// A consumer that never issues the checkout isolation SET still gets a correct
/// reset — the bit rides its first request, which is reset-before-execute — but
/// loses the early discard, since nothing verifies the acknowledgement.
///
/// This is *not* msodbcsql's behavior. msodbcsql arms the bit and then
/// immediately sends its own re-sync batch on the driver statement
/// (`sqlcmisc.cpp:2410-2446`), whose `BuildServerSideConnectOptions`
/// (`sqlcfunc.cpp:2007+`) re-emits `SET TRANSACTION ISOLATION LEVEL` and other
/// non-default session settings. It therefore pays a round trip here that this
/// driver does not, and it re-applies settings that this driver leaves to the
/// consumer's checkout SET. Today the only session setting emitted post-login is
/// the isolation level, so the two converge; as more are added (QUOTED_IDENTIFIER,
/// ANSI_NPW, CONCAT_NULL), `apply_post_connect_txn_settings` is the analogue that
/// would need to participate in the reset.
pub(super) fn reset_connection(dbc: &DbcHandle, value: u64) -> SqlReturn {
    const OP: &str = "SQLSetConnectAttrW(SQL_ATTR_RESET_CONNECTION)";

    {
        let Ok(mut state) = dbc.inner.lock() else {
            error!("{OP}: dbc mutex poisoned");
            return SQL_ERROR;
        };
        free_errors(&mut state);
        if value != u64::from(SQL_RESET_CONNECTION_YES) {
            error!(value, "{OP}: invalid value");
            post_diag(&mut state, ERR_INVALID_ATTRIBUTE_VALUE);
            return SQL_ERROR;
        }
    }

    // D4: read the transaction flag *before* claiming the client. Every other
    // claim site in this file does the same, and the ordering matters: if this
    // lock were taken after the claim and turned out to be poisoned, the early
    // return would drop the owned client, leaving `state.client == None` while
    // `connection_state` is still `Connected` — which the cached liveness read
    // interprets as "busy but healthy", so the pool would keep handing out a
    // connection that can never serve a request again.
    let started = match dbc.inner.lock() {
        Ok(state) => state.local_tran_started,
        Err(_) => {
            error!("{OP}: dbc mutex poisoned");
            return SQL_ERROR;
        }
    };

    // Sweep cursors before claiming, like every other connection-scoped
    // operation in this file. `claim_dbc_client` requires an idle connection,
    // and check-in with an undrained result set is ordinary: an application that
    // closes its connection without closing a cursor would otherwise make the
    // connection non-poolable, since the reset would fail and the pool would
    // discard a perfectly recyclable connection. msodbcsql reaches the same
    // outcome differently — it runs the reset on its own driver statement and
    // never contends with user cursors — but with a single `TdsClient` and no
    // MARS, sweeping is how this driver gets there.
    if close_all_cursors(dbc) == SQL_ERROR {
        return fail_cursor_close(dbc, OP);
    }

    // Claim the idle client: this rejects a busy connection (open cursor /
    // `active_stmt`) with ERR_CONNECTION_BUSY and a disconnected DBC with
    // ERR_CONNECTION_DOES_NOT_EXIST (08003, D7). From here until
    // `release_dbc_client` the DBC mutex is never taken again.
    let mut client = match claim_dbc_client(dbc, OP) {
        Ok(c) => c,
        Err(ret) => return ret,
    };

    // Close known user work before reuse instead of relying on the subsequent
    // reset to discard it. This also makes a rollback failure observable here,
    // so the pool discards the connection rather than reusing an uncertain
    // session. The ODBC flag can be stale, hence the descriptor guard.
    let result = if started && client.has_active_transaction() {
        debug!("{OP}: rolling back live local transaction before reset");
        dbc.runtime
            .block_on(client.rollback_transaction(None, None))
    } else {
        Ok(())
    };

    // Arm the reset. This costs no I/O: the bit rides the next request, and the
    // session-bound caches are dropped here so nothing stale can be used before
    // then.
    if result.is_ok() {
        client.prepare_reset_connection(false);
    }

    // A failed rollback leaves the session in an unknown state; poison the
    // client so a subsequent SQL_ATTR_CONNECTION_DEAD read reports it dead and
    // the pool discards it, then restore it so SQLDisconnect can still tear it
    // down.
    if result.is_err() {
        client.mark_connection_dead();
    }
    release_dbc_client(dbc, client);

    let Ok(mut state) = dbc.inner.lock() else {
        error!("{OP}: dbc mutex poisoned");
        return SQL_ERROR;
    };
    if let Err(e) = result {
        // The rollback failed, so the reset was never armed and the server may
        // still hold the transaction. Leave `local_tran_started` alone: claiming
        // no transaction is open would be a lie the next operation acts on.
        error!(%e, "{OP}: connection reset failed");
        state.pending_reset_ack = false;
        post_tds_error(&mut state, &e, SQLSTATE_08S01);
        return SQL_ERROR;
    }
    // The reset is armed; the server discards the transaction before it
    // processes the carrying request, so the driver's view catches up here.
    state.local_tran_started = false;
    // The checkout isolation SET must now emit so it carries the armed bit.
    state.pending_reset_ack = true;
    debug!("{OP}: reset armed; it will be carried by the next request");
    SQL_SUCCESS
}

/// Manual-commit → autocommit. Any transaction holding user work is **committed**
/// with a `01000` warning; a driver-begun piggyback transaction is rolled back
/// silently (`sqlcconn.cpp:3692-3741`).
fn switch_to_autocommit(dbc: &DbcHandle, op: &str) -> SqlReturn {
    let had_user_txn = match dbc.inner.lock() {
        Ok(state) => state.local_tran_started,
        Err(_) => {
            error!("{op}: dbc mutex poisoned");
            return SQL_ERROR;
        }
    };

    if had_user_txn {
        let ret = end_transaction(dbc, true, op);
        if ret != SQL_SUCCESS {
            return ret;
        }
        let Ok(mut state) = dbc.inner.lock() else {
            error!("{op}: dbc mutex poisoned");
            return SQL_ERROR;
        };
        state.autocommit = true;
        post_diag(&mut state, WARN_TRANSACTION_COMMITTED);
        return SQL_SUCCESS_WITH_INFO;
    }

    if close_all_cursors(dbc) == SQL_ERROR {
        return fail_cursor_close(dbc, op);
    }
    let mut client = match claim_dbc_client(dbc, op) {
        Ok(c) => c,
        Err(ret) => return ret,
    };
    let result = if client.has_active_transaction() {
        debug!("{op}: rolling back driver-begun transaction");
        dbc.runtime
            .block_on(client.rollback_transaction(None, None))
    } else {
        Ok(())
    };
    release_dbc_client(dbc, client);

    let Ok(mut state) = dbc.inner.lock() else {
        error!("{op}: dbc mutex poisoned");
        return SQL_ERROR;
    };
    if let Err(e) = result {
        error!(%e, "{op}: rollback of driver-begun transaction failed");
        post_tds_error(&mut state, &e, SQLSTATE_HY000);
        return SQL_ERROR;
    }
    state.autocommit = true;
    state.local_tran_started = false;
    SQL_SUCCESS
}

/// Autocommit → manual-commit. Opens a transaction immediately so `@@TRANCOUNT`
/// reflects the new mode without waiting for a statement (`sqlcconn.cpp:3692`).
/// The transaction carries no user work yet, so `local_tran_started` stays false.
///
/// The eager begin is deliberate parity with msodbcsql rather than an
/// optimization, and it is not free: it costs a round trip at the switch and
/// leaves an empty transaction pinning the log and version store until the
/// application commits, rolls back, or disconnects. On a pooled connection that
/// can be the life of the pool entry. `set_txn_isolation` and
/// `rollback_before_disconnect` dispose of an open transaction with
/// `local_tran_started == false` for this reason; `end_transaction` leaves it
/// in place, since msodbcsql issues no transaction-manager request when nothing
/// was started.
fn switch_to_manual_commit(dbc: &DbcHandle, op: &str) -> SqlReturn {
    if close_all_cursors(dbc) == SQL_ERROR {
        return fail_cursor_close(dbc, op);
    }
    let mut client = match claim_dbc_client(dbc, op) {
        Ok(c) => c,
        Err(ret) => return ret,
    };
    let result = if client.has_active_transaction() {
        Ok(())
    } else {
        dbc.runtime
            .block_on(client.begin_transaction(TransactionIsolationLevel::NoChange, None))
    };
    release_dbc_client(dbc, client);

    let Ok(mut state) = dbc.inner.lock() else {
        error!("{op}: dbc mutex poisoned");
        return SQL_ERROR;
    };
    if let Err(e) = result {
        error!(%e, "{op}: could not begin transaction");
        post_tds_error(&mut state, &e, SQLSTATE_HY000);
        return SQL_ERROR;
    }
    state.autocommit = false;
    state.local_tran_started = false;
    SQL_SUCCESS
}

/// Applies `SQL_ATTR_TXN_ISOLATION` (msodbcsql `sqlcmisc.cpp:1754-1827`), and
/// its vendor spelling `SQL_COPT_SS_TXN_ISOLATION`.
pub(super) fn set_txn_isolation(dbc: &DbcHandle, value: u64) -> SqlReturn {
    const OP: &str = "SQLSetConnectAttrW(SQL_ATTR_TXN_ISOLATION)";

    let (level, tsql) = {
        let Ok(mut state) = dbc.inner.lock() else {
            error!("{OP}: dbc mutex poisoned");
            return SQL_ERROR;
        };
        free_errors(&mut state);

        let Some((level, tsql)) = u32::try_from(value)
            .ok()
            .and_then(|level| txn_isolation_to_tsql(level).map(|tsql| (level, tsql)))
        else {
            error!(value, "{OP}: unsupported isolation level");
            post_diag(&mut state, ERR_OPTIONAL_FEATURE_NOT_IMPLEMENTED);
            return SQL_ERROR;
        };
        // Setting the level already in effect is a no-op, matching the
        // same-value short-circuit `SetCommitModeOption` uses for autocommit
        // (`sqlcmisc.cpp:1720`). Checked before the open-transaction rejection
        // below so that asking for no change never fails: an application that
        // explicitly selects the default READ COMMITTED at startup pays neither
        // an error inside a transaction nor a cursor sweep and round trip
        // outside one.
        //
        // This cache tracks only isolation set through this attribute. Raw T-SQL
        // can therefore make SQLGetConnectAttr report a stale value. Pool reuse
        // is still safe: a pending reset suppresses this short circuit, forcing
        // the checkout SET to restore READ COMMITTED on the server.
        //
        // The short circuit is suppressed while a pooling reset is armed: this
        // SET is the request that carries the RESETCONNECTION bit, so skipping
        // it would defer the reset to the borrower's first query and give up
        // fail-at-checkout. That also closes the cross-borrower isolation leak:
        // `sp_reset_connection` does not restore the isolation level (D9), so
        // forcing this batch to emit is what returns the session to READ
        // COMMITTED even when the previous borrower changed it via raw T-SQL.
        if level == state.txn_isolation && !state.pending_reset_ack {
            debug!(value, "{OP}: already at this isolation level");
            return SQL_SUCCESS;
        }
        // Changing isolation mid-transaction would silently apply to the next
        // one instead of this one, so msodbcsql refuses it outright.
        if state.local_tran_started {
            error!("{OP}: a transaction is open");
            post_diag(&mut state, ERR_ATTRIBUTE_CANNOT_BE_SET_NOW);
            return SQL_ERROR;
        }
        if state.connection_state != ConnectionState::Connected {
            state.txn_isolation = level;
            debug!(value, "{OP}: stored for next connect");
            return SQL_SUCCESS;
        }
        (level, tsql)
    };

    if close_all_cursors(dbc) == SQL_ERROR {
        return fail_cursor_close(dbc, OP);
    }
    let mut client = match claim_dbc_client(dbc, OP) {
        Ok(c) => c,
        Err(ret) => return ret,
    };

    // `local_tran_started` was false above, so any transaction open here was
    // begun by the driver at the autocommit switch and carries no user work.
    // SQL Server rejects SET TRANSACTION ISOLATION LEVEL SNAPSHOT inside an
    // active transaction, so close the empty one, apply the change, and reopen
    // it so manual-commit mode is left with a transaction pending, the state
    // the autocommit switch establishes. Rolling back loses nothing because
    // there is nothing in it.
    let reopen = client.has_active_transaction();
    let mut result = if reopen {
        debug!("{OP}: rolling back empty driver-begun transaction to apply isolation");
        dbc.runtime
            .block_on(client.rollback_transaction(None, None))
    } else {
        Ok(())
    };
    if result.is_ok() {
        result = exec_batch(dbc, &mut client, tsql);
    }
    if result.is_ok() && reopen {
        result = dbc
            .runtime
            .block_on(client.begin_transaction(TransactionIsolationLevel::NoChange, None));
    }
    // If a pooling reset was armed, this batch carried its RESETCONNECTION bit.
    // Confirm the server actually acknowledged it: the batch succeeding is not
    // evidence on its own, and an unacknowledged reset means the session was
    // never returned to its login defaults.
    let reset_unacknowledged = result.is_ok() && client.reset_pending();
    if reset_unacknowledged {
        client.mark_connection_dead();
    }
    release_dbc_client(dbc, client);

    let Ok(mut state) = dbc.inner.lock() else {
        error!("{OP}: dbc mutex poisoned");
        return SQL_ERROR;
    };
    if let Err(e) = result {
        error!(%e, "{OP}: could not apply isolation level");
        state.pending_reset_ack = false;
        post_tds_error(&mut state, &e, SQLSTATE_HY000);
        return SQL_ERROR;
    }
    if reset_unacknowledged {
        error!("{OP}: the armed connection reset was not acknowledged by the server");
        state.pending_reset_ack = false;
        post_sql_error(
            &mut state,
            SQLSTATE_08S01,
            0,
            "The connection reset was not acknowledged by the server",
        );
        return SQL_ERROR;
    }
    state.pending_reset_ack = false;
    // `value` was validated into `level` above.
    state.txn_isolation = level;
    debug!(tsql, "{OP}: isolation level applied");
    SQL_SUCCESS
}

/// Rolls back a driver-begun transaction that carries no user work, so the
/// session is not left holding locks when the socket drops. Best-effort: every
/// failure is logged and swallowed, and no diagnostic is posted, because the
/// caller is already tearing the connection down.
pub(super) fn rollback_before_disconnect(dbc: &DbcHandle) {
    const OP: &str = "SQLDisconnect(rollback)";

    // A cursor that will not close leaves the connection mid-batch, so the
    // rollback below cannot be sent. Disconnecting anyway is still correct:
    // the server rolls the transaction back when the socket closes.
    if close_all_cursors(dbc) == SQL_ERROR {
        error!("{OP}: could not close all cursors; the server will roll back on disconnect");
        return;
    }

    let client = match dbc.inner.lock() {
        Ok(mut state) => state.client.take(),
        Err(_) => {
            error!("{OP}: dbc mutex poisoned");
            return;
        }
    };
    let Some(mut client) = client else {
        return;
    };
    if client.has_active_transaction()
        && let Err(e) = dbc
            .runtime
            .block_on(client.rollback_transaction(None, None))
    {
        error!(%e, "{OP}: rollback failed; the server will roll back on disconnect");
    }
    release_dbc_client(dbc, client);
}

/// Applies transaction attributes that were set before `SQLDriverConnect` and so
/// could not reach the server. Called once the login completes.
///
/// A failure does not fail the connection — the session is usable — but it must
/// not pass unnoticed either: an unapplied isolation level would leave
/// `SQLGetConnectAttr` reporting a level the server is not actually running at.
/// Returns `SQL_SUCCESS_WITH_INFO` with a diagnostic posted in that case, which
/// the caller promotes into the `SQLDriverConnect` result.
#[must_use]
pub(super) fn apply_post_connect_txn_settings(dbc: &DbcHandle) -> SqlReturn {
    const OP: &str = "SQLDriverConnectW(transaction settings)";

    let (autocommit, isolation, diag_len) = match dbc.inner.lock() {
        Ok(state) => (
            state.autocommit,
            state.txn_isolation,
            state.diag_records().len(),
        ),
        Err(_) => {
            error!("{OP}: dbc mutex poisoned");
            return SQL_SUCCESS;
        }
    };
    if autocommit && isolation == SQL_TXN_READ_COMMITTED {
        return SQL_SUCCESS;
    }

    let mut client = match claim_dbc_client(dbc, OP) {
        Ok(c) => c,
        Err(_) => {
            // Unreachable right after a successful login, but the claim posts
            // its own error record before failing. The connect itself
            // succeeded, so returning SQL_SUCCESS with that record still on the
            // handle would show an application a connect failure that never
            // happened. Truncating rather than clearing keeps the server's
            // login INFO messages, which a SQL_SUCCESS_WITH_INFO result needs.
            if let Ok(mut state) = dbc.inner.lock() {
                state.diag_records_mut().truncate(diag_len);
            }
            return SQL_SUCCESS;
        }
    };

    let mut failure: Option<String> = None;

    if isolation != SQL_TXN_READ_COMMITTED
        && let Some(tsql) = txn_isolation_to_tsql(isolation)
        && let Err(e) = exec_batch(dbc, &mut client, tsql)
    {
        error!(%e, "{OP}: could not apply pre-connect isolation level");
        failure = Some(format!(
            "The transaction isolation level set before connecting could not be \
             applied to the session: {e}"
        ));
    }

    if !autocommit
        && !client.has_active_transaction()
        && let Err(e) = dbc
            .runtime
            .block_on(client.begin_transaction(TransactionIsolationLevel::NoChange, None))
    {
        error!(%e, "{OP}: could not begin transaction for manual-commit mode");
        failure.get_or_insert_with(|| {
            format!("The manual-commit transaction could not be started: {e}")
        });
    }

    release_dbc_client(dbc, client);

    let Some(message) = failure else {
        return SQL_SUCCESS;
    };
    let Ok(mut state) = dbc.inner.lock() else {
        error!("{OP}: dbc mutex poisoned");
        return SQL_SUCCESS;
    };
    post_sql_error(&mut state, SQLSTATE_01000, 0, &message);
    SQL_SUCCESS_WITH_INFO
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn isolation_maps_to_expected_tsql() {
        assert_eq!(
            txn_isolation_to_tsql(SQL_TXN_READ_UNCOMMITTED),
            Some("SET TRANSACTION ISOLATION LEVEL READ UNCOMMITTED")
        );
        assert_eq!(
            txn_isolation_to_tsql(SQL_TXN_READ_COMMITTED),
            Some("SET TRANSACTION ISOLATION LEVEL READ COMMITTED")
        );
        assert_eq!(
            txn_isolation_to_tsql(SQL_TXN_REPEATABLE_READ),
            Some("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ")
        );
        assert_eq!(
            txn_isolation_to_tsql(SQL_TXN_SERIALIZABLE),
            Some("SET TRANSACTION ISOLATION LEVEL SERIALIZABLE")
        );
        assert_eq!(
            txn_isolation_to_tsql(SQL_TXN_SS_SNAPSHOT),
            Some("SET TRANSACTION ISOLATION LEVEL SNAPSHOT")
        );
    }

    #[test]
    fn isolation_rejects_unsupported_values() {
        // 0 (unset), 3 and 0x10 are not single accepted bits; 0x03 in particular
        // is a two-bit combination msodbcsql also rejects.
        for level in [0, 3, 5, 0x10, 0x40, u32::MAX] {
            assert_eq!(txn_isolation_to_tsql(level), None, "level {level:#x}");
        }
    }

    #[test]
    fn closing_cursors_clears_statement_diagnostics() {
        // msodbcsql's sweep calls SQLFreeStmt(SQL_CLOSE) on every statement
        // (sqlctran.cpp:302-323), and that entry point frees the statement's
        // errors before it inspects the cursor state (sqlccmd.cpp:379-380). The
        // records go even on a statement that never opened a cursor.
        use crate::test_support::TestHandles;
        use crate::{error::HasDiagnostics, handles::StmtHandle};

        let h = TestHandles::with_env_dbc_stmt();
        let stmt = unsafe { handle_from_raw::<StmtHandle>(h.stmt) };
        {
            let mut stmt_state = stmt.inner.lock().unwrap();
            post_sql_error(&mut stmt_state, SQLSTATE_HY000, 0, "statement failed");
            assert_eq!(stmt_state.diag_records().len(), 1);
        }

        let dbc = unsafe { handle_from_raw::<DbcHandle>(h.dbc) };
        assert_eq!(close_all_cursors(dbc), SQL_SUCCESS);

        assert!(
            stmt.inner.lock().unwrap().diag_records().is_empty(),
            "the cursor sweep must clear diagnostics on child statements, as msodbcsql does"
        );
    }

    #[test]
    fn end_tran_sweeps_cursors_even_with_no_transaction_started() {
        // This driver advertises SQL_CB_CLOSE unconditionally, so the Driver
        // Manager marks every statement cursor-closed after a successful
        // SQLEndTran — including the autocommit no-op, where
        // `local_tran_started` is never true. If the sweep were skipped there,
        // the driver and the DM would disagree about the cursor and the
        // application would be wedged. Clearing the statement's diagnostics is
        // the observable side effect of the sweep having run.
        use crate::test_support::TestHandles;
        use crate::{error::HasDiagnostics, handles::StmtHandle};

        let h = TestHandles::with_env_dbc_stmt();
        h.mark_dbc_connected();
        let stmt = unsafe { handle_from_raw::<StmtHandle>(h.stmt) };
        {
            let mut stmt_state = stmt.inner.lock().unwrap();
            post_sql_error(&mut stmt_state, SQLSTATE_HY000, 0, "statement failed");
        }

        let dbc = unsafe { handle_from_raw::<DbcHandle>(h.dbc) };
        assert!(!dbc.inner.lock().unwrap().local_tran_started);
        assert_eq!(end_transaction(dbc, true, "test"), SQL_SUCCESS);

        assert!(
            stmt.inner.lock().unwrap().diag_records().is_empty(),
            "the no-transaction path must still sweep cursors"
        );
    }

    #[test]
    fn reset_connection_rejects_non_yes_value() {
        // D7: only SQL_RESET_CONNECTION_YES(1) is valid; anything else is HY024.
        // Value validation runs before the connection is claimed.
        use crate::api::odbc_types::{SQL_ATTR_RESET_CONNECTION, SQL_COPT_SS_RESET_CONNECTION};
        use crate::api::set_connect_attr::sql_set_connect_attr_w;
        use crate::api::sqlstate::SQLSTATE_HY024;
        use crate::error::HasDiagnostics;
        use crate::test_support::TestHandles;

        let h = TestHandles::with_env_dbc();
        let dbc = unsafe { handle_from_raw::<DbcHandle>(h.dbc) };
        for attribute in [SQL_ATTR_RESET_CONNECTION, SQL_COPT_SS_RESET_CONNECTION] {
            let ret = unsafe { sql_set_connect_attr_w(h.dbc, attribute, 2usize as _, 0) };
            assert_eq!(ret, SQL_ERROR);
            assert_eq!(
                dbc.inner.lock().unwrap().diag_records()[0].sql_state,
                SQLSTATE_HY024
            );
        }
    }

    #[test]
    fn reset_connection_on_disconnected_dbc_is_08003() {
        // D7: reset on a connection that does not exist surfaces 08003, the
        // diagnostic `claim_dbc_client` posts for the disconnected case.
        use crate::api::odbc_types::{SQL_ATTR_RESET_CONNECTION, SQL_RESET_CONNECTION_YES};
        use crate::api::set_connect_attr::sql_set_connect_attr_w;
        use crate::api::sqlstate::SQLSTATE_08003;
        use crate::error::HasDiagnostics;
        use crate::test_support::TestHandles;

        let h = TestHandles::with_env_dbc();
        let ret = unsafe {
            sql_set_connect_attr_w(
                h.dbc,
                SQL_ATTR_RESET_CONNECTION,
                SQL_RESET_CONNECTION_YES as usize as _,
                0,
            )
        };
        assert_eq!(ret, SQL_ERROR);
        let dbc = unsafe { handle_from_raw::<DbcHandle>(h.dbc) };
        assert_eq!(
            dbc.inner.lock().unwrap().diag_records()[0].sql_state,
            SQLSTATE_08003
        );
    }

    #[test]
    fn reset_connection_rejects_busy_connection() {
        // An open cursor / in-progress result set pins `active_stmt`; the reset
        // must not touch a busy connection.
        use std::ffi::c_void;

        use crate::error::HasDiagnostics;
        use crate::test_support::TestHandles;
        use mssql_tds::test_client_support::tds_client_from_tokens;

        let h = TestHandles::with_env_dbc();
        h.mark_dbc_connected();
        let dbc = unsafe { handle_from_raw::<DbcHandle>(h.dbc) };
        {
            let mut state = dbc.inner.lock().unwrap();
            state.client = Some(tds_client_from_tokens(vec![]));
            state.active_stmt = Some(std::ptr::dangling_mut::<c_void>());
        }

        assert_eq!(reset_connection(dbc, 1), SQL_ERROR);
        let state = dbc.inner.lock().unwrap();
        assert_eq!(state.diag_records()[0].sql_state, SQLSTATE_HY000);
        // The busy connection was rejected without taking the client.
        assert!(state.client.is_some());
    }

    #[test]
    fn reset_connection_arms_and_clears_local_tran() {
        // A successful reset drives the acknowledging round trip, leaves the idle
        // client in place, and clears the driver-side transaction flag (D4).
        use crate::error::HasDiagnostics;
        use crate::test_support::TestHandles;
        use mssql_tds::test_client_support::{
            done_no_more, env_change_reset_connection, tds_client_from_tokens,
        };

        let h = TestHandles::with_env_dbc();
        h.mark_dbc_connected();
        let dbc = unsafe { handle_from_raw::<DbcHandle>(h.dbc) };
        {
            let mut state = dbc.inner.lock().unwrap();
            state.client = Some(tds_client_from_tokens(vec![
                env_change_reset_connection(),
                done_no_more(),
            ]));
            state.local_tran_started = true;
        }

        assert_eq!(reset_connection(dbc, 1), SQL_SUCCESS);
        let state = dbc.inner.lock().unwrap();
        assert!(!state.local_tran_started);
        let client = state.client.as_ref().expect("client restored after reset");
        assert!(
            !client.is_connection_dead(),
            "a reset that was acknowledged must leave the connection reusable"
        );
        assert!(state.diag_records().is_empty());
    }

    #[test]
    fn reset_connection_rolls_back_live_transaction_first() {
        // D4: when the borrower left a live local transaction, the reset rolls it
        // back before resetting so the next borrower cannot inherit it. This is
        // the defense-in-depth branch mssql-python's own SQLEndTran normally
        // pre-empts, so it needs its own coverage: script a BeginTransaction
        // ENVCHANGE so `has_active_transaction()` is true, then assert the client
        // ends with no active transaction and an acknowledged reset.
        use crate::error::HasDiagnostics;
        use crate::test_support::TestHandles;
        use mssql_tds::test_client_support::{
            done_no_more, env_change_reset_connection, env_change_rollback_transaction,
            tds_client_from_tokens_in_transaction,
        };

        let h = TestHandles::with_env_dbc();
        h.mark_dbc_connected();
        let dbc = unsafe { handle_from_raw::<DbcHandle>(h.dbc) };
        {
            let mut state = dbc.inner.lock().unwrap();
            let client = tds_client_from_tokens_in_transaction(
                vec![
                    // Rollback leg: the TM request the reset issues first.
                    env_change_rollback_transaction(),
                    done_no_more(),
                    // Reset leg: the acknowledging ENVCHANGE for the reset itself.
                    env_change_reset_connection(),
                    done_no_more(),
                ],
                0xDEAD_BEEF,
            );
            assert!(
                client.has_active_transaction(),
                "precondition: the client must start with a live transaction"
            );
            state.client = Some(client);
            state.local_tran_started = true;
        }

        assert_eq!(reset_connection(dbc, 1), SQL_SUCCESS);
        let state = dbc.inner.lock().unwrap();
        assert!(!state.local_tran_started);
        let client = state.client.as_ref().expect("client restored after reset");
        assert!(
            !client.has_active_transaction(),
            "the live transaction must be rolled back before the reset"
        );
        assert!(!client.is_connection_dead());
        assert!(state.diag_records().is_empty());
    }

    #[test]
    fn armed_reset_clears_the_transaction_descriptor_so_checkout_sends_no_rollback() {
        // Regression: a full RESETCONNECTION discards the server-side
        // transaction, so the driver must stop believing one is open. The
        // reachable case is mssql-python's default autocommit=False: the
        // bookkeeping transaction from `switch_to_manual_commit` leaves a live
        // descriptor with `local_tran_started == false`, so neither check-in
        // `SQLEndTran` nor the reset's own D4 rollback sends a TM request. If the
        // descriptor survived, `set_txn_isolation`'s `reopen` branch would send a
        // rollback carrying the reset bit; the server honors the bit first,
        // discards the transaction, and answers 3903 — failing every checkout.
        use crate::test_support::TestHandles;
        use mssql_tds::test_client_support::{
            done_no_more, env_change_reset_connection, tds_client_from_tokens_in_transaction,
        };

        let h = TestHandles::with_env_dbc();
        h.mark_dbc_connected();
        let dbc = unsafe { handle_from_raw::<DbcHandle>(h.dbc) };
        {
            let mut state = dbc.inner.lock().unwrap();
            // Only the isolation SET should reach the wire: one batch, then the
            // reset acknowledgement. A stray rollback would consume these first
            // and strand the SET.
            let client = tds_client_from_tokens_in_transaction(
                vec![env_change_reset_connection(), done_no_more()],
                0xDEAD_BEEF,
            );
            assert!(client.has_active_transaction());
            state.client = Some(client);
            // The driver-begun transaction carries no user work.
            state.local_tran_started = false;
        }

        assert_eq!(reset_connection(dbc, 1), SQL_SUCCESS);
        {
            let state = dbc.inner.lock().unwrap();
            let client = state.client.as_ref().expect("client restored after arming");
            assert!(
                !client.has_active_transaction(),
                "arming a full reset must drop the transaction the reset discards"
            );
        }

        // The pool's checkout re-apply: it must send only the SET.
        assert_eq!(
            set_txn_isolation(dbc, u64::from(SQL_TXN_READ_COMMITTED)),
            SQL_SUCCESS
        );
        let state = dbc.inner.lock().unwrap();
        let client = state.client.as_ref().expect("client restored");
        assert!(
            !client.reset_pending(),
            "the SET must have carried the bit and collected the acknowledgement"
        );
        assert!(!client.is_connection_dead());
    }

    #[test]
    fn failed_rollback_leaves_local_tran_started_set() {
        // The rollback failing means the reset was never armed and the server may
        // still hold the transaction. Clearing `local_tran_started` would record
        // that none is open — a lie the next operation would act on, e.g.
        // SQLDisconnect skipping its 25000 guard.
        use crate::api::sqlstate::SQLSTATE_08S01;
        use crate::error::HasDiagnostics;
        use crate::test_support::TestHandles;
        use mssql_tds::test_client_support::tds_client_from_tokens_in_transaction;

        let h = TestHandles::with_env_dbc();
        h.mark_dbc_connected();
        let dbc = unsafe { handle_from_raw::<DbcHandle>(h.dbc) };
        {
            let mut state = dbc.inner.lock().unwrap();
            // No tokens: the rollback round trip runs dry and fails.
            state.client = Some(tds_client_from_tokens_in_transaction(vec![], 0xDEAD_BEEF));
            state.local_tran_started = true;
        }

        assert_eq!(reset_connection(dbc, 1), SQL_ERROR);
        let state = dbc.inner.lock().unwrap();
        assert_eq!(state.diag_records()[0].sql_state, SQLSTATE_08S01);
        assert!(
            state.local_tran_started,
            "a failed rollback must not clear the driver's transaction flag"
        );
        assert!(
            !state.pending_reset_ack,
            "no reset was armed, so nothing should be waiting on an acknowledgement"
        );
    }

    #[test]
    fn reset_connection_sweeps_an_open_cursor_instead_of_rejecting() {
        // Check-in with an undrained result set is ordinary (the application
        // closed its connection without closing the cursor). Rejecting it would
        // make the connection non-poolable, so the reset sweeps cursors first
        // like every other connection-scoped operation.
        use crate::error::HasDiagnostics;
        use crate::handles::StmtHandle;
        use crate::handles::stmt::STMT_STATE_CURSOR_OPEN;
        use crate::test_support::TestHandles;
        use mssql_tds::test_client_support::{
            done_no_more, env_change_reset_connection, tds_client_from_tokens,
        };

        let h = TestHandles::with_env_dbc_stmt();
        h.mark_dbc_connected();
        let dbc = unsafe { handle_from_raw::<DbcHandle>(h.dbc) };
        {
            // A statement holds an open cursor, so the connection looks busy to
            // `claim_dbc_client` until the sweep runs.
            let stmt = unsafe { handle_from_raw::<StmtHandle>(h.stmt) };
            stmt.inner.lock().unwrap().set_state(STMT_STATE_CURSOR_OPEN);

            let mut state = dbc.inner.lock().unwrap();
            state.client = Some(tds_client_from_tokens(vec![
                // Drained by the cursor sweep.
                done_no_more(),
                // Collected later by the request that carries the reset bit.
                env_change_reset_connection(),
                done_no_more(),
            ]));
            state.active_stmt = Some(h.stmt);
        }

        assert_eq!(reset_connection(dbc, 1), SQL_SUCCESS);
        let state = dbc.inner.lock().unwrap();
        assert!(state.diag_records().is_empty());
        assert!(
            state.active_stmt.is_none(),
            "the sweep must release the cursor claim"
        );
        assert!(state.pending_reset_ack);
    }

    #[test]
    fn reset_connection_arms_without_any_io() {
        // Arming performs no round trip, so it succeeds even against a client
        // with no queued tokens — that is the whole point of the no-RTT design.
        // It leaves the ack outstanding so the checkout isolation SET cannot
        // short-circuit, and drops session-bound state immediately.
        use crate::error::HasDiagnostics;
        use crate::test_support::TestHandles;
        use mssql_tds::test_client_support::tds_client_from_tokens;

        let h = TestHandles::with_env_dbc();
        h.mark_dbc_connected();
        let dbc = unsafe { handle_from_raw::<DbcHandle>(h.dbc) };
        {
            let mut state = dbc.inner.lock().unwrap();
            state.client = Some(tds_client_from_tokens(vec![]));
        }

        assert_eq!(reset_connection(dbc, 1), SQL_SUCCESS);
        let state = dbc.inner.lock().unwrap();
        assert!(state.diag_records().is_empty());
        assert!(
            state.pending_reset_ack,
            "the armed reset must force the checkout isolation SET to emit"
        );
        let client = state.client.as_ref().expect("client restored after arming");
        assert!(
            client.reset_pending(),
            "the acknowledgement is outstanding until the carrying request completes"
        );
        assert!(
            !client.is_connection_dead(),
            "arming must not poison a healthy connection"
        );
    }

    #[test]
    fn unacknowledged_reset_poisons_client_on_the_carrying_request() {
        // The carrying request is where a failed reset now surfaces. If the
        // checkout isolation SET completes but the server never sent the
        // ResetConnection ENVCHANGE, the session was never reset: surface 08S01
        // so the pool discards the connection and poison the client so a later
        // SQL_ATTR_CONNECTION_DEAD read reports dead, while restoring it so
        // SQLDisconnect can still tear it down.
        use crate::api::sqlstate::SQLSTATE_08S01;
        use crate::error::HasDiagnostics;
        use crate::test_support::TestHandles;
        use mssql_tds::test_client_support::{done_no_more, tds_client_from_tokens};

        let h = TestHandles::with_env_dbc();
        h.mark_dbc_connected();
        let dbc = unsafe { handle_from_raw::<DbcHandle>(h.dbc) };
        {
            let mut state = dbc.inner.lock().unwrap();
            // The batch completes, but with no ResetConnection acknowledgement.
            state.client = Some(tds_client_from_tokens(vec![done_no_more()]));
        }

        assert_eq!(reset_connection(dbc, 1), SQL_SUCCESS);
        // The pool's checkout re-apply of READ COMMITTED carries the armed bit.
        assert_eq!(
            set_txn_isolation(dbc, u64::from(SQL_TXN_READ_COMMITTED)),
            SQL_ERROR
        );

        let state = dbc.inner.lock().unwrap();
        assert_eq!(state.diag_records()[0].sql_state, SQLSTATE_08S01);
        assert!(
            !state.pending_reset_ack,
            "the pending flag must not survive a failed checkout"
        );
        let client = state.client.as_ref().expect("client restored for teardown");
        assert!(
            client.is_connection_dead(),
            "an unacknowledged reset must poison the client so the pool discards it"
        );
    }

    #[test]
    fn armed_reset_suppresses_the_isolation_short_circuit() {
        // The same-value short circuit would normally skip the checkout SET when
        // the borrower never changed isolation (D6). While a reset is armed that
        // SET is the request carrying the bit, so it must emit anyway — proven
        // here by the reset being acknowledged, which only happens on a real
        // round trip.
        use crate::test_support::TestHandles;
        use mssql_tds::test_client_support::{
            done_no_more, env_change_reset_connection, tds_client_from_tokens,
        };

        let h = TestHandles::with_env_dbc();
        h.mark_dbc_connected();
        let dbc = unsafe { handle_from_raw::<DbcHandle>(h.dbc) };
        {
            let mut state = dbc.inner.lock().unwrap();
            state.client = Some(tds_client_from_tokens(vec![
                env_change_reset_connection(),
                done_no_more(),
            ]));
            // The borrower never changed isolation: the cache already holds the
            // value the pool is about to re-apply.
            state.txn_isolation = SQL_TXN_READ_COMMITTED;
        }

        assert_eq!(reset_connection(dbc, 1), SQL_SUCCESS);
        assert_eq!(
            set_txn_isolation(dbc, u64::from(SQL_TXN_READ_COMMITTED)),
            SQL_SUCCESS
        );

        let state = dbc.inner.lock().unwrap();
        assert!(!state.pending_reset_ack);
        let client = state.client.as_ref().expect("client restored");
        assert!(
            !client.reset_pending(),
            "the SET must have emitted and collected the reset acknowledgement"
        );
        assert!(!client.is_connection_dead());
    }

    #[test]
    fn reset_then_checkout_isolation_reapplies_read_committed() {
        // D9/B4: a borrower raised isolation to SERIALIZABLE via the attribute.
        // At checkout the pool resets (acked eagerly here) and re-applies READ
        // COMMITTED, which `sp_reset_connection` does not restore. The isolation
        // SET differs from the cached SERIALIZABLE, so it emits a real batch and
        // the cached level lands back at READ COMMITTED.
        use crate::api::odbc_types::SQL_TXN_READ_COMMITTED as READ_COMMITTED;
        use crate::error::HasDiagnostics;
        use crate::test_support::TestHandles;
        use mssql_tds::test_client_support::{
            done_no_more, env_change_reset_connection, tds_client_from_tokens,
        };

        let h = TestHandles::with_env_dbc();
        h.mark_dbc_connected();
        let dbc = unsafe { handle_from_raw::<DbcHandle>(h.dbc) };
        {
            let mut state = dbc.inner.lock().unwrap();
            state.client = Some(tds_client_from_tokens(vec![
                // SET TRANSACTION ISOLATION LEVEL SERIALIZABLE
                done_no_more(),
                // reset round trip
                env_change_reset_connection(),
                done_no_more(),
                // SET TRANSACTION ISOLATION LEVEL READ COMMITTED
                done_no_more(),
            ]));
        }

        assert_eq!(
            set_txn_isolation(dbc, u64::from(SQL_TXN_SERIALIZABLE)),
            SQL_SUCCESS
        );
        assert_eq!(reset_connection(dbc, 1), SQL_SUCCESS);
        assert_eq!(
            set_txn_isolation(dbc, u64::from(READ_COMMITTED)),
            SQL_SUCCESS
        );

        let state = dbc.inner.lock().unwrap();
        assert_eq!(
            state.txn_isolation, READ_COMMITTED,
            "checkout must re-apply READ COMMITTED after the reset"
        );
        assert!(
            !state.client.as_ref().unwrap().is_connection_dead(),
            "the connection stays reusable across reset + isolation re-apply"
        );
        assert!(state.diag_records().is_empty());
    }

    #[test]
    fn checkout_cycle_reuses_one_physical_connection() {
        // B5: the mssql-python checkout lifecycle on a single physical client —
        // acquire (reset, eagerly acked) → setautocommit(False) (begin-txn) →
        // check-in → next acquire (reset again). One scripted client services the
        // whole sequence, proving the same physical connection is reused and stays
        // reusable rather than being reconnected underneath.
        use crate::error::HasDiagnostics;
        use crate::test_support::TestHandles;
        use mssql_tds::test_client_support::{
            done_no_more, env_change_reset_connection, tds_client_from_tokens,
        };

        let h = TestHandles::with_env_dbc();
        h.mark_dbc_connected();
        let dbc = unsafe { handle_from_raw::<DbcHandle>(h.dbc) };
        {
            let mut state = dbc.inner.lock().unwrap();
            state.client = Some(tds_client_from_tokens(vec![
                // acquire: reset round trip
                env_change_reset_connection(),
                done_no_more(),
                // setautocommit(False): begin transaction
                done_no_more(),
                // setautocommit(True) on the way back: no active txn, no I/O
                // next acquire: reset round trip
                env_change_reset_connection(),
                done_no_more(),
            ]));
        }

        assert_eq!(reset_connection(dbc, 1), SQL_SUCCESS);
        assert_eq!(
            set_autocommit(dbc, u64::from(SQL_AUTOCOMMIT_OFF)),
            SQL_SUCCESS
        );
        assert!(!dbc.inner.lock().unwrap().autocommit);
        assert_eq!(
            set_autocommit(dbc, u64::from(SQL_AUTOCOMMIT_ON)),
            SQL_SUCCESS
        );
        assert!(dbc.inner.lock().unwrap().autocommit);
        assert_eq!(reset_connection(dbc, 1), SQL_SUCCESS);

        let state = dbc.inner.lock().unwrap();
        let client = state
            .client
            .as_ref()
            .expect("same client reused across cycle");
        assert!(!client.is_connection_dead());
        assert!(state.diag_records().is_empty());
    }
}
