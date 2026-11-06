// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.
// Portions derived from saurabh500/mssql-tiberius-bridge (MIT).

//! Live pooling regressions using the shared live-test environment.

pub mod common;

use std::time::Duration;

use deadpool::managed::{Object, Timeouts};
use futures_util::StreamExt;
use mssql_rs::{Client, Error, Pool, RecyclingMethod, TdsManager};

async fn scalar(client: &mut Client, sql: &str) -> i32 {
    client
        .simple_query(sql)
        .await
        .expect("scalar query failed")
        .into_first_result()
        .first()
        .expect("expected scalar query row")
        .get(0)
        .expect("expected an int scalar")
}

async fn start_waiting_batch(client: &mut Client) {
    client
        .inner_mut()
        .execute(
            "SELECT 1; RAISERROR('flush before wait', 0, 1) WITH NOWAIT; \
             WAITFOR DELAY '00:00:05'"
                .into(),
            (),
        )
        .await
        .expect("execute succeeds: SELECT 1; RAISERROR('flush before wait', 0, 1) WITH NOWAIT; WAITFOR DELAY '00:00:05'");
}

#[tokio::test]
async fn native_pool_reset_isolates_borrowers_on_the_same_connection() {
    let Some(config) = common::live_config() else {
        return;
    };
    let pool = TdsManager::create_pool(config, 1).expect("build connection pool");
    let mut conn = Box::pin(pool.get()).await.expect("pool checkout failed");
    let original = conn
        .simple_query("SELECT @@SPID AS spid, DB_NAME() AS db, @@LOCK_TIMEOUT AS lock_timeout")
        .await
        .expect("query succeeds: SELECT @@SPID AS spid, DB_NAME() AS db, @@LOCK_TIMEOUT AS lock_timeout")
        .into_first_result();
    let spid = original
        .first()
        .expect("expected row at index 0")
        .get::<i16, _>("spid")
        .expect("expected non-NULL column spid");
    let database = original
        .first()
        .expect("expected row at index 0")
        .get::<&str, _>("db")
        .expect("expected non-NULL column db")
        .to_string();
    let lock_timeout = original
        .first()
        .expect("expected row at index 0")
        .get::<i32, _>("lock_timeout")
        .expect("expected non-NULL column lock_timeout");
    let statement = conn
        .prepare("SELECT 42", &[])
        .await
        .expect("prepare statement");

    conn.simple_query(
        "USE tempdb; \
         CREATE TABLE #bridge_pool_reset (value int); \
         SET LOCK_TIMEOUT 1234; \
         SET TRANSACTION ISOLATION LEVEL SERIALIZABLE; \
         BEGIN TRANSACTION; \
         INSERT INTO #bridge_pool_reset VALUES (42)",
    )
    .await
    .expect("query succeeds: USE tempdb; CREATE TABLE #bridge_pool_reset (value int); SET LOCK_TIMEOUT 1234; SET TRANSACTION I...");
    assert_eq!(scalar(&mut conn, "SELECT @@TRANCOUNT").await, 1);
    drop(conn);

    let mut conn = Box::pin(pool.get()).await.expect("pool checkout failed");
    assert_eq!(Object::metrics(&conn).recycle_count, 1);
    let rows = conn
        .simple_query(
            "SELECT @@SPID AS spid, DB_NAME() AS db, @@LOCK_TIMEOUT AS lock_timeout, \
             @@TRANCOUNT AS tran_count, \
             CASE WHEN OBJECT_ID('tempdb..#bridge_pool_reset') IS NULL THEN 1 ELSE 0 END AS cleared, \
             CAST(transaction_isolation_level AS int) AS isolation_level \
             FROM sys.dm_exec_sessions WHERE session_id = @@SPID",
        )
        .await
        .expect("query succeeds: SELECT @@SPID AS spid, DB_NAME() AS db, @@LOCK_TIMEOUT AS lock_timeout, @@TRANCOUNT AS tran_count...")
        .into_first_result();
    assert_eq!(
        rows.first()
            .expect("expected row at index 0")
            .get::<i16, _>("spid"),
        Some(spid)
    );
    assert_eq!(
        rows.first()
            .expect("expected row at index 0")
            .get::<&str, _>("db"),
        Some(database.as_str())
    );
    assert_eq!(
        rows.first()
            .expect("expected row at index 0")
            .get::<i32, _>("lock_timeout"),
        Some(lock_timeout)
    );
    assert_eq!(
        rows.first()
            .expect("expected row at index 0")
            .get::<i32, _>("tran_count"),
        Some(0)
    );
    assert_eq!(
        rows.first()
            .expect("expected row at index 0")
            .get::<i32, _>("cleared"),
        Some(1)
    );
    assert_eq!(
        rows.first()
            .expect("expected row at index 0")
            .get::<i32, _>("isolation_level"),
        Some(2)
    );
    assert!(matches!(
        statement.query(&mut conn, &[]).await,
        Err(Error::InvalidPreparedStatement)
    ));
}

#[tokio::test]
async fn repeated_pool_reuse_does_not_leak_validation_result_sets() {
    let Some(config) = common::live_config() else {
        return;
    };
    let pool = TdsManager::create_pool(config, 1).expect("build connection pool");
    for cycle in 0..256 {
        let mut conn = Box::pin(pool.get()).await.expect("pool checkout failed");
        assert_eq!(Object::metrics(&conn).recycle_count, cycle);
        let value = i32::try_from(cycle).expect("test value fits target integer") + 1;
        for step in 0..3 {
            let expected = value * 10 + step;
            let results = conn
                .query(
                    "SELECT @P1 AS expected_value, \
                     CAST('application' AS nvarchar(16)) AS result_source",
                    &[&expected],
                )
                .await
                .expect("query succeeds: SELECT @P1 AS expected_value, CAST('application' AS nvarchar(16)) AS result_source")
                .into_results();
            assert_eq!(
                results.len(),
                1,
                "unexpected result set on checkout {cycle}"
            );
            assert_eq!(
                results
                    .first()
                    .expect("expected result set at index 0")
                    .len(),
                1
            );
            let row = results
                .first()
                .expect("expected result set at index 0")
                .first()
                .expect("expected row at index 0");
            assert_eq!(row.columns().len(), 2);
            assert_eq!(row.get::<i32, _>("expected_value"), Some(expected));
            assert_eq!(row.get::<&str, _>("result_source"), Some("application"));
        }

        // A legitimate unnamed integer result must not be filtered out (#104).
        let results = conn
            .query("SELECT @P1; SELECT @P1 + 1 AS next_value", &[&value])
            .await
            .expect("query succeeds: SELECT @P1; SELECT @P1 + 1 AS next_value")
            .into_results();
        assert_eq!(results.len(), 2);
        assert_eq!(
            results
                .first()
                .expect("expected result set at index 0")
                .len(),
            1
        );
        assert_eq!(
            results
                .first()
                .expect("expected result set at index 0")
                .first()
                .expect("expected row at index 0")
                .columns()
                .first()
                .expect("expected unnamed column")
                .name(),
            ""
        );
        assert_eq!(
            results
                .first()
                .expect("expected result set at index 0")
                .first()
                .expect("expected row at index 0")
                .get::<i32, _>(0),
            Some(value)
        );
        assert_eq!(
            results
                .get(1)
                .expect("expected result set at index 1")
                .len(),
            1
        );
        assert_eq!(
            results
                .get(1)
                .expect("expected result set at index 1")
                .first()
                .expect("expected row at index 0")
                .get::<i32, _>("next_value"),
            Some(value + 1)
        );

        if cycle % 8 == 0 {
            let mut stream = conn.simple_query_streamed("SELECT 7 UNION ALL SELECT 8; SELECT 9");
            stream
                .next()
                .await
                .expect("expected stream row")
                .expect("read stream row");
        }
    }
}

#[tokio::test]
async fn reset_rejects_stale_prepared_handles_without_unpreparing_new_ones() {
    let Some(config) = common::live_config() else {
        return;
    };
    let mut client = Client::connect(&config)
        .await
        .expect("connect to SQL Server");
    let old = client
        .prepare("SELECT @P1", &[&0i32])
        .await
        .expect("prepare statement");
    client.reset_session().await.expect("reset session");

    assert!(matches!(
        old.query(&mut client, &[&1i32]).await,
        Err(Error::InvalidPreparedStatement)
    ));
    assert!(matches!(
        old.execute(&mut client, &[&1i32]).await,
        Err(Error::InvalidPreparedStatement)
    ));
    let fresh = client
        .prepare("SELECT @P1 + 1", &[&0i32])
        .await
        .expect("prepare statement");
    assert!(matches!(
        old.close(&mut client).await,
        Err(Error::InvalidPreparedStatement)
    ));
    let rows = fresh
        .query(&mut client, &[&41i32])
        .await
        .expect(
            "query succeeds for reset rejects stale prepared handles without unpreparing new ones",
        )
        .into_first_result();
    assert_eq!(
        rows.first()
            .expect("expected row at index 0")
            .get::<i32, _>(0),
        Some(42)
    );
    fresh
        .close(&mut client)
        .await
        .expect("close prepared statement");
}

#[tokio::test]
async fn explicit_ping_recycling_preserves_legacy_session_state() {
    let Some(config) = common::live_config() else {
        return;
    };
    let manager = TdsManager::new(config).with_recycling_method(RecyclingMethod::Ping);
    let pool = Pool::builder(manager)
        .max_size(1)
        .build()
        .expect("build connection pool");
    let mut conn = Box::pin(pool.get()).await.expect("pool checkout failed");
    let statement = conn
        .prepare("SELECT 42", &[])
        .await
        .expect("prepare statement");
    conn.simple_query(
        "CREATE TABLE #bridge_pool_legacy (value int); \
         SET LOCK_TIMEOUT 1234; BEGIN TRANSACTION; \
         INSERT INTO #bridge_pool_legacy VALUES (7)",
    )
    .await
    .expect("query succeeds: CREATE TABLE #bridge_pool_legacy (value int); SET LOCK_TIMEOUT 1234; BEGIN TRANSACTION; INSERT IN...");
    drop(conn);

    let mut conn = Box::pin(pool.get()).await.expect("pool checkout failed");
    assert_eq!(Object::metrics(&conn).recycle_count, 1);
    assert_eq!(scalar(&mut conn, "SELECT @@LOCK_TIMEOUT").await, 1234);
    assert_eq!(scalar(&mut conn, "SELECT @@TRANCOUNT").await, 1);
    assert_eq!(
        scalar(&mut conn, "SELECT value FROM #bridge_pool_legacy").await,
        7
    );
    let rows = statement
        .query(&mut conn, &[])
        .await
        .expect("query succeeds for explicit ping recycling preserves legacy session state")
        .into_first_result();
    assert_eq!(
        rows.first()
            .expect("expected row at index 0")
            .get::<i32, _>(0),
        Some(42)
    );
    conn.simple_query("ROLLBACK TRANSACTION")
        .await
        .expect("query succeeds: ROLLBACK TRANSACTION");
    statement
        .close(&mut conn)
        .await
        .expect("close prepared statement");
}

#[tokio::test]
async fn both_recycling_methods_discard_known_dead_connections() {
    let Some(config) = common::live_config() else {
        return;
    };
    for method in [RecyclingMethod::Reset, RecyclingMethod::Ping] {
        let manager = TdsManager::new(config.clone()).with_recycling_method(method);
        let pool = Pool::builder(manager)
            .max_size(1)
            .build()
            .expect("build connection pool");
        let mut conn = Box::pin(pool.get()).await.expect("pool checkout failed");
        assert!(!conn.is_connection_dead());
        conn.inner_mut()
            .close_connection()
            .await
            .expect("close connection");
        assert!(conn.is_connection_dead());
        assert!(matches!(
            conn.reset_session().await,
            Err(Error::Tds(mssql_tds::error::Error::ConnectionClosed(_)))
        ));
        drop(conn);

        let mut replacement = Box::pin(pool.get()).await.expect("pool checkout failed");
        assert_eq!(Object::metrics(&replacement).recycle_count, 0);
        assert_eq!(scalar(&mut replacement, "SELECT 42").await, 42);
    }
}

#[tokio::test]
async fn reset_drains_an_abandoned_stream_before_arming_the_reset() {
    let Some(config) = common::live_config() else {
        return;
    };
    let mut client = Client::connect(&config)
        .await
        .expect("connect to SQL Server");
    let default_timeout = scalar(&mut client, "SELECT @@LOCK_TIMEOUT").await;
    let mut stream = client
        .simple_query_streamed("SELECT 1 UNION ALL SELECT 2; SET LOCK_TIMEOUT 1234; SELECT 3");
    stream
        .next()
        .await
        .expect("expected stream row")
        .expect("read stream row");
    drop(stream);

    client.reset_session().await.expect("reset session");
    assert_eq!(
        scalar(&mut client, "SELECT @@LOCK_TIMEOUT").await,
        default_timeout
    );
    assert!(!client.inner_mut().reset_pending());
}

#[tokio::test]
async fn a_failed_reset_retires_the_connection() {
    let Some(config) = common::live_config() else {
        return;
    };
    let mut client = Client::connect(&config)
        .await
        .expect("connect to SQL Server");
    client
        .inner_mut()
        .execute("SELECT 1; THROW 50000, 'reset drain failure', 1".into(), ())
        .await
        .expect("execute succeeds: SELECT 1; THROW 50000, 'reset drain failure', 1");
    client
        .reset_session()
        .await
        .expect_err("reset must fail while draining SQL error");
    assert!(client.is_connection_dead());
}

#[tokio::test]
async fn cancelling_a_reset_retires_the_connection() {
    let Some(config) = common::live_config() else {
        return;
    };
    let mut client = Client::connect(&config)
        .await
        .expect("connect to SQL Server");
    start_waiting_batch(&mut client).await;
    tokio::time::timeout(Duration::from_millis(50), client.reset_session())
        .await
        .expect_err("reset must time out while draining waiting batch");
    assert!(client.is_connection_dead());
}

#[tokio::test]
async fn pool_replaces_a_connection_when_reset_fails() {
    let Some(config) = common::live_config() else {
        return;
    };
    let pool = TdsManager::create_pool(config, 1).expect("build connection pool");
    let mut conn = Box::pin(pool.get()).await.expect("pool checkout failed");
    conn.inner_mut()
        .execute(
            "SELECT 1; THROW 50000, 'recycle drain failure', 1".into(),
            (),
        )
        .await
        .expect("execute succeeds: SELECT 1; THROW 50000, 'recycle drain failure', 1");
    drop(conn);

    let mut replacement = Box::pin(pool.get()).await.expect("pool checkout failed");
    assert_eq!(Object::metrics(&replacement).recycle_count, 0);
    assert_eq!(scalar(&mut replacement, "SELECT 42").await, 42);
}

#[tokio::test]
async fn pool_replaces_a_connection_when_reset_times_out() {
    let Some(config) = common::live_config() else {
        return;
    };
    let pool = Pool::builder(TdsManager::new(config))
        .max_size(1)
        .runtime(deadpool::Runtime::Tokio1)
        .timeouts(Timeouts {
            recycle: Some(Duration::from_millis(50)),
            ..Timeouts::default()
        })
        .build()
        .expect("build connection pool");
    let mut conn = Box::pin(pool.get()).await.expect("pool checkout failed");
    start_waiting_batch(&mut conn).await;
    drop(conn);

    let mut replacement = tokio::time::timeout(Duration::from_secs(10), Box::pin(pool.get()))
        .await
        .expect("checkout must complete after discarding the timed-out connection")
        .expect("checkout replacement connection");
    assert_eq!(Object::metrics(&replacement).recycle_count, 0);
    assert_eq!(scalar(&mut replacement, "SELECT 42").await, 42);
}

#[tokio::test]
async fn convenience_pool_supports_checkout_timeouts() {
    let Some(config) = common::live_config() else {
        return;
    };
    let pool = TdsManager::create_pool(config, 1).expect("build connection pool");
    let conn = Box::pin(pool.get()).await.expect("pool checkout failed");
    assert!(matches!(
        Box::pin(pool.timeout_get(&Timeouts::wait_millis(10))).await,
        Err(deadpool::managed::PoolError::Timeout(
            deadpool::managed::TimeoutType::Wait
        ))
    ));
    drop(conn);
    let mut conn = Box::pin(pool.get()).await.expect("pool checkout failed");
    assert_eq!(scalar(&mut conn, "SELECT 42").await, 42);
}
