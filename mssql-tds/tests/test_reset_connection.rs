// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Integration tests for client-side connection reset
//! (`RESETCONNECTION` / `RESETCONNECTIONSKIPTRAN`).
//!
//! These verify the end-to-end behavior against a live SQL Server: that
//! `TdsClient::prepare_reset_connection` causes the server to reset the reused
//! session back to its login defaults before processing the next request, and
//! that the `preserve_transaction` flag controls whether an open transaction
//! survives the reset.

mod common;

#[cfg(test)]
mod reset_connection {
    use crate::common::{
        ExpectedQueryResultType, begin_connection, build_tcp_datasource, get_scalar_value,
        run_query_and_check_results,
    };
    use mssql_tds::connection::tds_client::TdsClient;
    use mssql_tds::datatypes::column_values::ColumnValues;

    /// Execute a statement that yields no result rows (SET / DDL / DML) and
    /// fully drain the response.
    async fn exec_drain(conn: &mut TdsClient, sql: &str) {
        conn.execute(sql.to_string(), ()).await.unwrap();
        // `get_scalar_value` drains every result set and closes the batch; the
        // returned value is irrelevant for non-SELECT statements.
        let _ = get_scalar_value(conn).await.unwrap();
    }

    /// Execute a scalar `SELECT` and return its single `int` value.
    async fn select_int(conn: &mut TdsClient, sql: &str) -> i32 {
        conn.execute(sql.to_string(), ()).await.unwrap();
        match get_scalar_value(conn).await.unwrap() {
            Some(ColumnValues::Int(v)) => v,
            other => panic!("expected a single Int scalar for `{sql}`, got {other:?}"),
        }
    }

    /// Positive: a session setting changed away from its login default is reset
    /// back to that default after `prepare_reset_connection(false)`.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn reset_restores_session_setting() {
        let mut conn = begin_connection(&build_tcp_datasource()).await;

        // Capture the connection's login-default LOCK_TIMEOUT so the assertion
        // does not hard-code a server default.
        let default_lock_timeout = select_int(&mut conn, "SELECT @@LOCK_TIMEOUT").await;

        // Change it to a clearly non-default value and confirm it took effect.
        exec_drain(&mut conn, "SET LOCK_TIMEOUT 5000").await;
        assert_eq!(
            select_int(&mut conn, "SELECT @@LOCK_TIMEOUT").await,
            5000,
            "session setting should have changed before the reset"
        );

        // Reset the connection. The reset is applied before the SELECT runs, so
        // the SELECT observes the restored login default.
        conn.prepare_reset_connection(false);
        assert_eq!(
            select_int(&mut conn, "SELECT @@LOCK_TIMEOUT").await,
            default_lock_timeout,
            "connection reset should restore the login-default session setting"
        );
    }

    /// Negative (control): without a reset, the changed session setting persists
    /// across requests on the same connection.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn no_reset_preserves_session_setting() {
        let mut conn = begin_connection(&build_tcp_datasource()).await;

        exec_drain(&mut conn, "SET LOCK_TIMEOUT 5000").await;

        // No `prepare_reset_connection` call: the setting must survive.
        assert_eq!(
            select_int(&mut conn, "SELECT @@LOCK_TIMEOUT").await,
            5000,
            "session setting must persist when no reset is requested"
        );
    }

    /// Positive: a temp table created on the session is dropped by the reset, so
    /// referencing it afterwards fails.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn reset_drops_temp_table() {
        let mut conn = begin_connection(&build_tcp_datasource()).await;

        exec_drain(&mut conn, "CREATE TABLE #reset_probe(col int)").await;
        exec_drain(&mut conn, "INSERT INTO #reset_probe VALUES(42)").await;

        // Sanity: the temp table is visible before the reset.
        assert_eq!(
            select_int(&mut conn, "SELECT col FROM #reset_probe").await,
            42,
            "temp table should be readable before the reset"
        );

        // After the reset the temp table is gone; the SELECT must error. The
        // server error can surface either from `execute` or while draining.
        conn.prepare_reset_connection(false);
        let exec = conn
            .execute("SELECT col FROM #reset_probe".to_string(), ())
            .await;
        let errored = if exec.is_err() {
            true
        } else {
            get_scalar_value(&mut conn).await.is_err()
        };
        assert!(
            errored,
            "temp table must no longer exist after a connection reset"
        );
    }

    /// Negative (control): without a reset, the temp table remains accessible.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn no_reset_keeps_temp_table() {
        let mut conn = begin_connection(&build_tcp_datasource()).await;

        exec_drain(&mut conn, "CREATE TABLE #reset_probe2(col int)").await;
        exec_drain(&mut conn, "INSERT INTO #reset_probe2 VALUES(7)").await;

        assert_eq!(
            select_int(&mut conn, "SELECT col FROM #reset_probe2").await,
            7,
            "temp table must remain when no reset is requested"
        );
    }

    /// Open transaction + `preserve_transaction = false`: the reset rolls back
    /// the transaction, so `@@TRANCOUNT` is 0 afterwards.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn reset_without_preserve_rolls_back_transaction() {
        let mut conn = begin_connection(&build_tcp_datasource()).await;

        exec_drain(&mut conn, "BEGIN TRANSACTION").await;
        assert_eq!(
            select_int(&mut conn, "SELECT @@TRANCOUNT").await,
            1,
            "transaction should be open before the reset"
        );

        // RESETCONNECTION (no SKIPTRAN) discards the transaction.
        conn.prepare_reset_connection(false);
        assert_eq!(
            select_int(&mut conn, "SELECT @@TRANCOUNT").await,
            0,
            "transaction must be rolled back by a reset without preserve_transaction"
        );
    }

    /// Open transaction + `preserve_transaction = true`: the reset preserves the
    /// transaction (RESETCONNECTIONSKIPTRAN), so `@@TRANCOUNT` stays 1.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn reset_with_preserve_keeps_transaction() {
        let mut conn = begin_connection(&build_tcp_datasource()).await;

        exec_drain(&mut conn, "BEGIN TRANSACTION").await;
        assert_eq!(
            select_int(&mut conn, "SELECT @@TRANCOUNT").await,
            1,
            "transaction should be open before the reset"
        );

        // RESETCONNECTIONSKIPTRAN resets session state but preserves the
        // transaction.
        conn.prepare_reset_connection(true);
        assert_eq!(
            select_int(&mut conn, "SELECT @@TRANCOUNT").await,
            1,
            "transaction must survive a reset with preserve_transaction = true"
        );

        // Clean up the still-open transaction.
        run_query_and_check_results(
            &mut conn,
            "ROLLBACK TRANSACTION".to_string(),
            &[ExpectedQueryResultType::Update(0)],
        )
        .await;
    }

    /// After a reset, the pool-facing session-state getters must fall back to
    /// the login defaults. The server resets the session but does not emit
    /// individual Database/Language ENVCHANGE tokens for the revert, so the
    /// client restores the cached state on the ResetConnection acknowledgement.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn reset_restores_database_getter_to_login_default() {
        let mut conn = begin_connection(&build_tcp_datasource()).await;
        assert_eq!(conn.database(), "master");

        exec_drain(&mut conn, "USE tempdb").await;
        assert_eq!(conn.database(), "tempdb", "USE should update the getter");

        conn.prepare_reset_connection(false);
        // Run a trivial request that carries the reset bit and drain its response.
        let _ = select_int(&mut conn, "SELECT 1").await;

        assert_eq!(
            conn.database(),
            "master",
            "after reset the database getter should fall back to the login default"
        );
    }
}
