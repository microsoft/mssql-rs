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
    SQL_AUTOCOMMIT_OFF, SQL_AUTOCOMMIT_ON, SQL_ERROR, SQL_SUCCESS, SQL_SUCCESS_WITH_INFO,
    SQL_TXN_READ_COMMITTED, SQL_TXN_READ_UNCOMMITTED, SQL_TXN_REPEATABLE_READ,
    SQL_TXN_SERIALIZABLE, SQL_TXN_SS_SNAPSHOT, SqlReturn,
};
use super::sqlstate::{
    ERR_ATTRIBUTE_CANNOT_BE_SET_NOW, ERR_CONNECTION_BUSY, ERR_CONNECTION_DOES_NOT_EXIST,
    ERR_INVALID_ATTRIBUTE_VALUE, ERR_NO_ACTIVE_TDS_CLIENT, ERR_OPTIONAL_FEATURE_NOT_IMPLEMENTED,
    SQLSTATE_01000, SQLSTATE_08007, SQLSTATE_HY000, WARN_TRANSACTION_COMMITTED, post_diag,
    post_tds_error,
};
use crate::error::{free_errors, post_sql_error};
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
pub(super) fn release_dbc_client(dbc: &DbcHandle, client: TdsClient) {
    if let Ok(mut state) = dbc.inner.lock() {
        state.client = Some(client);
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
/// `SQLFreeStmt(SQL_CLOSE)`: this is a connection-scoped operation, so it must
/// not clear the diagnostics belonging to child statement handles. Statements
/// with no open cursor cost a single lock, which is the common case.
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
/// transaction started this is a **silent success**, never a warning or error.
pub(super) fn end_transaction(dbc: &DbcHandle, commit: bool, op: &str) -> SqlReturn {
    {
        let Ok(mut state) = dbc.inner.lock() else {
            error!("{op}: dbc mutex poisoned");
            return SQL_ERROR;
        };
        if state.connection_state != ConnectionState::Connected {
            error!("{op}: DBC is not connected");
            post_diag(&mut state, ERR_CONNECTION_DOES_NOT_EXIST);
            return SQL_ERROR;
        }
        // msodbcsql `sqlctran.cpp:293`: nothing started, nothing to do.
        if !state.local_tran_started {
            debug!("{op}: no transaction started — no-op");
            return SQL_SUCCESS;
        }
    }

    if close_all_cursors(dbc) == SQL_ERROR {
        return fail_cursor_close(dbc, op);
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
/// as holding user work.
///
/// Called immediately before every statement execution, mirroring msodbcsql's
/// `CheckOptions` (`sqlccmd.cpp:10572-10585`). Running it per statement — rather
/// than only at the autocommit switch — is what recovers from a transaction the
/// server aborted or the application rolled back with raw T-SQL.
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
/// can be the life of the pool entry. `sql_end_tran`, `set_txn_isolation` and
/// `rollback_before_disconnect` all treat an open transaction with
/// `local_tran_started == false` as empty and disposable for this reason.
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

    let tsql = {
        let Ok(mut state) = dbc.inner.lock() else {
            error!("{OP}: dbc mutex poisoned");
            return SQL_ERROR;
        };
        free_errors(&mut state);

        let level = u32::try_from(value).ok();
        let Some(tsql) = level.and_then(txn_isolation_to_tsql) else {
            error!(value, "{OP}: unsupported isolation level");
            post_diag(&mut state, ERR_OPTIONAL_FEATURE_NOT_IMPLEMENTED);
            return SQL_ERROR;
        };
        // Changing isolation mid-transaction would silently apply to the next
        // one instead of this one, so msodbcsql refuses it outright.
        if state.local_tran_started {
            error!("{OP}: a transaction is open");
            post_diag(&mut state, ERR_ATTRIBUTE_CANNOT_BE_SET_NOW);
            return SQL_ERROR;
        }
        // Setting the level already in effect is a no-op, matching the
        // same-value short-circuit `SetCommitModeOption` uses for autocommit
        // (`sqlcmisc.cpp:1720`). Without this, an application that explicitly
        // selects the default READ COMMITTED at startup would pay a cursor
        // sweep and a round trip for nothing.
        if level == Some(state.txn_isolation) {
            debug!(value, "{OP}: already at this isolation level");
            return SQL_SUCCESS;
        }
        if state.connection_state != ConnectionState::Connected {
            state.txn_isolation = level.unwrap_or(SQL_TXN_READ_COMMITTED);
            debug!(value, "{OP}: stored for next connect");
            return SQL_SUCCESS;
        }
        tsql
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
    // it to preserve the manual-commit invariant that a transaction is always
    // pending. Rolling back loses nothing because there is nothing in it.
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
    release_dbc_client(dbc, client);

    let Ok(mut state) = dbc.inner.lock() else {
        error!("{OP}: dbc mutex poisoned");
        return SQL_ERROR;
    };
    if let Err(e) = result {
        error!(%e, "{OP}: could not apply isolation level");
        post_tds_error(&mut state, &e, SQLSTATE_HY000);
        return SQL_ERROR;
    }
    // `value` already round-tripped through `u32::try_from` above.
    state.txn_isolation = value as u32;
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

    let (autocommit, isolation) = match dbc.inner.lock() {
        Ok(state) => (state.autocommit, state.txn_isolation),
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
        Err(_) => return SQL_SUCCESS,
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
    fn closing_cursors_preserves_statement_diagnostics() {
        // ODBC clears diagnostics on the handle the function was called on. A
        // connection-scoped operation must therefore leave child statement
        // diagnostics alone, or an application that rolls back after a failed
        // statement can no longer read why the statement failed.
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

        assert_eq!(
            stmt.inner.lock().unwrap().diag_records().len(),
            1,
            "the cursor sweep must not clear diagnostics on child statements"
        );
    }
}
