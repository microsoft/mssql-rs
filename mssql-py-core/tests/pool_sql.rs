// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

// The cdylib's pool remains private; compile its contracts into this native harness.
#[allow(dead_code)]
#[path = "../src/pool"]
mod pool {
    pub(crate) mod config;
    pub(crate) mod connection;
    pub(crate) mod control;
    pub(crate) mod error;
    mod lifecycle;
    pub(crate) mod manager;
    pub(crate) mod session;
}
#[allow(dead_code)]
#[path = "../../mssql-tds/tests/common/mod.rs"]
mod common;

use std::time::Duration;

use mssql_tds::connection::tds_client::TdsClient;
use mssql_tds::datatypes::column_values::ColumnValues;
use mssql_tds::message::transaction_management::TransactionIsolationLevel;
use pool::config::PoolConfig;
use pool::connection::{AuthBinding, ConnectSpec, PoolKey, SqlConnectionFactory};
use pool::control::AcquireControl;
use pool::manager::AsyncPoolManager;
use pool::session::{Isolation, PoolLease};
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

fn spec() -> ConnectSpec {
    let mut context = common::create_context();
    context.data_source = common::build_tcp_datasource();
    context.connect_retry_count = 0;
    ConnectSpec::new(
        context,
        AuthBinding::new(vec![1].into_boxed_slice()).unwrap(),
    )
}

fn manager() -> AsyncPoolManager {
    let budget = Duration::from_secs(30);
    let config = PoolConfig::new(budget, budget, budget)
        .unwrap()
        .with_max_size(1)
        .unwrap();
    let manager = AsyncPoolManager::new(config.clone()).unwrap();
    manager.enable(config).unwrap();
    manager
}

async fn acquire(
    manager: &AsyncPoolManager,
    spec: &ConnectSpec,
    isolation: Isolation,
) -> PoolLease {
    manager
        .acquire(
            PoolKey::new(vec![1].into_boxed_slice()).unwrap(),
            spec,
            &SqlConnectionFactory,
            isolation,
            AcquireControl::from_timeout(
                Instant::now(),
                Duration::from_secs(30),
                CancellationToken::new(),
            )
            .unwrap(),
        )
        .await
        .unwrap()
        .unwrap()
}

async fn scalar(client: &mut TdsClient, sql: &str) -> i32 {
    client.execute(sql.into(), ()).await.unwrap();
    match common::get_scalar_value(client).await.unwrap() {
        Some(ColumnValues::Int(value)) => value,
        Some(ColumnValues::SmallInt(value)) => i32::from(value),
        other => panic!("expected int scalar, got {other:?}"),
    }
}

async fn exec(client: &mut TdsClient, sql: &str) {
    client.execute(sql.into(), ()).await.unwrap();
    client.close_query().await.unwrap();
}

#[tokio::test]
#[ignore = "requires live SQL Server and DB_HOST/DB_USERNAME/SQL_PASSWORD"]
async fn live_fresh_and_reused_sessions_apply_every_isolation_level() {
    let spec = spec();
    for (level, expected) in [
        (TransactionIsolationLevel::ReadUncommitted, 1),
        (TransactionIsolationLevel::ReadCommitted, 2),
        (TransactionIsolationLevel::RepeatableRead, 3),
        (TransactionIsolationLevel::Serializable, 4),
        (TransactionIsolationLevel::Snapshot, 5),
    ] {
        let manager = manager();
        let mut fresh = acquire(&manager, &spec, level.clone().try_into().unwrap()).await;
        let spid = scalar(fresh.client_mut(), "SELECT @@SPID").await;
        let query = "SELECT CAST(transaction_isolation_level AS int) \
                     FROM sys.dm_exec_sessions WHERE session_id = @@SPID";
        assert_eq!(scalar(fresh.client_mut(), query).await, expected);
        exec(
            fresh.client_mut(),
            "SET TRANSACTION ISOLATION LEVEL READ UNCOMMITTED",
        )
        .await;
        fresh.return_quiescent().unwrap();
        let mut reused = acquire(&manager, &spec, level.try_into().unwrap()).await;
        assert_eq!(scalar(reused.client_mut(), "SELECT @@SPID").await, spid);
        assert_eq!(scalar(reused.client_mut(), query).await, expected);
        assert!(!reused.client_mut().reset_pending());
        assert!(!reused.client_mut().has_open_batch());
        drop(reused);
        manager.shutdown().await.unwrap();
    }
}

#[tokio::test]
#[ignore = "requires live SQL Server and DB_HOST/DB_USERNAME/SQL_PASSWORD"]
async fn live_checkout_resets_session_state_and_discards_dirty_candidates() {
    let manager = manager();
    let spec = spec();
    let mut first = acquire(
        &manager,
        &spec,
        TransactionIsolationLevel::ReadCommitted.try_into().unwrap(),
    )
    .await;
    let spid = scalar(first.client_mut(), "SELECT @@SPID").await;
    let lock_timeout = scalar(first.client_mut(), "SELECT @@LOCK_TIMEOUT").await;
    exec(
        first.client_mut(),
        "SET LOCK_TIMEOUT 1234; CREATE TABLE #pool_probe (n int)",
    )
    .await;
    first.return_quiescent().unwrap();
    let mut reused = acquire(
        &manager,
        &spec,
        TransactionIsolationLevel::Serializable.try_into().unwrap(),
    )
    .await;
    assert_eq!(scalar(reused.client_mut(), "SELECT @@SPID").await, spid);
    assert_eq!(
        scalar(reused.client_mut(), "SELECT @@LOCK_TIMEOUT").await,
        lock_timeout
    );
    assert_eq!(
        scalar(
            reused.client_mut(),
            "SELECT CASE WHEN OBJECT_ID('tempdb..#pool_probe') IS NULL THEN 0 ELSE 1 END"
        )
        .await,
        0
    );
    exec(reused.client_mut(), "BEGIN TRANSACTION").await;
    assert!(reused.client_mut().has_active_transaction());
    let discarded_id = reused.client_mut().client_connection_id();
    // Fault injection, not a production return path: checkout must reject this client.
    reused.return_quiescent().unwrap();
    let mut replacement = acquire(
        &manager,
        &spec,
        TransactionIsolationLevel::ReadCommitted.try_into().unwrap(),
    )
    .await;
    assert_ne!(
        replacement.client_mut().client_connection_id(),
        discarded_id
    );
    assert_eq!(
        scalar(replacement.client_mut(), "SELECT @@TRANCOUNT").await,
        0
    );
    assert_eq!(
        scalar(
            replacement.client_mut(),
            "SELECT CAST(transaction_isolation_level AS int) \
         FROM sys.dm_exec_sessions WHERE session_id = @@SPID"
        )
        .await,
        2
    );
    drop(replacement);
    manager.shutdown().await.unwrap();
}
