// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Regression test for microsoft/mssql-rs#439.
//!
//! mssql-odbc shares one Tokio runtime (`EnvHandle::runtime`, built with
//! `worker_threads(1)`) across every connection allocated under one ODBC
//! environment, and drives each blocking `SQLExecute` / `SQLExecDirectW` call
//! via `Runtime::block_on` from a plain, non-async calling thread — never from
//! inside a task already running on that runtime. This test reproduces that
//! exact entry pattern (two connections, one single-worker-thread runtime,
//! sequential `block_on` calls from outside any task) against the mock TDS
//! server to check whether a second connection's blocked statement can still
//! complete once its (deliberately late) response arrives.

use std::sync::Arc;
use std::time::{Duration, Instant};

use mssql_mock_tds::{MockTdsServer, QueryResponse, TerminalError};
use mssql_tds::connection::client_context::ClientContext;
use mssql_tds::connection_provider::tds_connection_provider::TdsConnectionProvider;
use mssql_tds::core::{EncryptionOptions, EncryptionSetting};
use tokio::runtime::Runtime;
use tokio::sync::oneshot;

fn generate_test_password() -> String {
    use rand::Rng;
    const CHARSET: &[u8] =
        b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789!@#$%^&*";
    let mut rng = rand::rng();
    (0..24)
        .map(|_| CHARSET[rng.random_range(0..CHARSET.len())] as char)
        .collect()
}

/// Mirrors `mssql-odbc`'s `EnvHandle::new()`: a multi-thread Tokio runtime
/// pinned to a single worker thread, shared by every connection allocated
/// under one ODBC environment.
fn single_worker_thread_runtime() -> Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()
        .expect("failed to build single-worker-thread runtime")
}

fn make_context() -> ClientContext {
    let mut context = ClientContext::default();
    context.user_name = "sa".to_string();
    context.password = generate_test_password();
    context.database = "master".to_string();
    context.encryption_options = EncryptionOptions {
        mode: EncryptionSetting::PreferOff,
        trust_server_certificate: true,
        host_name_in_cert: None,
        server_certificate: None,
    };
    context
}

#[test]
fn two_connections_share_a_single_worker_thread_runtime_without_hanging() {
    // The mock server runs on its own runtime/thread, exactly like a real SQL
    // Server: a separate process that never competes for the driver's own
    // worker thread.
    let server_runtime = tokio::runtime::Runtime::new().expect("failed to build server runtime");

    const RESPONSE_DELAY: Duration = Duration::from_secs(3);
    const SELECT_SQL: &str = "SELECT * FROM ##test_isolation WHERE id = 1";

    let (server_addr, shutdown_tx, server_handle) = server_runtime.block_on(async {
        let server = MockTdsServer::new("127.0.0.1:0")
            .await
            .expect("failed to start mock server");
        let addr = server.local_addr();
        let registry = server.query_registry();
        registry.lock().await.register(
            SELECT_SQL,
            QueryResponse::error_only(TerminalError::new(
                1222,
                16,
                "Lock request time out period exceeded.",
            ))
            .with_delay(RESPONSE_DELAY),
        );

        let (tx, rx) = oneshot::channel();
        let handle = tokio::spawn(async move { server.run_with_shutdown(rx).await });
        tokio::time::sleep(Duration::from_millis(100)).await;
        (addr, tx, handle)
    });

    // The DBC-shared runtime: exactly `EnvHandle::new()`'s configuration.
    let dbc_runtime = Arc::new(single_worker_thread_runtime());
    let datasource = format!("tcp:{},{}", server_addr.ip(), server_addr.port());
    let provider = TdsConnectionProvider {};

    // conn1: connect, then run a quick statement and leave the connection be —
    // mirrors `BEGIN TRANSACTION` + `UPDATE` completing immediately and
    // leaving the transaction open. Each call is its own `block_on` from this
    // plain test thread, exactly like a separate synchronous SQLExecDirectW
    // call from the ODBC driver (never from inside a task on `dbc_runtime`).
    let mut conn1 = dbc_runtime
        .block_on(provider.create_client(make_context(), &datasource, None))
        .expect("conn1 failed to connect");
    dbc_runtime
        .block_on(conn1.execute("SELECT 1".to_string(), ()))
        .expect("conn1's quick statement failed");

    // conn2: connect, then run the statement the mock server holds for
    // RESPONSE_DELAY before answering with SQL Server's real lock-timeout
    // error (1222).
    let mut conn2 = dbc_runtime
        .block_on(provider.create_client(make_context(), &datasource, None))
        .expect("conn2 failed to connect");

    let started = Instant::now();
    // Wrapped in a bounded `tokio::time::timeout` so a genuine starvation
    // regression fails the test deterministically after 30s instead of
    // hanging the whole suite — `block_on` itself has no deadline, so an
    // un-wrapped await here would never return if the runtime really did
    // starve the blocked statement.
    let result = dbc_runtime.block_on(async {
        tokio::time::timeout(
            Duration::from_secs(30),
            conn2.execute(SELECT_SQL.to_string(), ()),
        )
        .await
    });
    let elapsed = started.elapsed();

    let result = result.unwrap_or_else(|_| {
        panic!(
            "SQLExecute-equivalent on conn2 did not return within 30s — the shared \
             single-worker-thread runtime is starving the blocked statement instead of resuming \
             it once the (delayed) response arrives (mssql-rs#439)"
        )
    });
    result.expect_err("a lock-timed-out statement must surface as an error, not succeed");
    assert!(
        elapsed >= RESPONSE_DELAY,
        "response surfaced before the server's artificial delay even elapsed: {elapsed:?}"
    );

    let _ = shutdown_tx.send(());
    let _ = server_runtime
        .block_on(async { tokio::time::timeout(Duration::from_secs(2), server_handle).await });
}
