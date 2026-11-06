// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.
// Portions derived from saurabh500/mssql-tiberius-bridge (MIT).

//! Cancellation regressions for #88. Live tests skip only when the shared
//! live-test password is unavailable; configured connection failures are test
//! failures.

pub mod common;

use std::future::{Future, pending};
use std::pin::pin;
use std::task::{Context, Poll};
use std::time::Duration;

use async_trait::async_trait;
use deadpool::managed::{Manager, Metrics, Object, RecycleError};
use futures_util::{StreamExt, TryStreamExt, poll, task::noop_waker_ref};
use mssql_rs::{
    BulkInsert, BulkLoadRow, Client, Config, Error, Pool, RecyclingMethod, Result, Row, TdsManager,
};
use mssql_tds::core::TdsResult;
use mssql_tds::datatypes::column_values::ColumnValues;
use mssql_tds::message::bulk_load::StreamingBulkLoadWriter;
use tokio::sync::Notify;
use tokio::time::timeout;

const IO_BOUND: Duration = Duration::from_secs(20);
const CANCEL_AFTER: Duration = Duration::from_millis(1);
const WAIT_QUERY: &str = "WAITFOR DELAY '00:00:05'; SELECT 1 AS n";

async fn connect(config: &Config) -> Client {
    timeout(IO_BOUND, Client::connect(config))
        .await
        .expect("connection deadline exceeded")
        .expect("configured SQL Server connection failed")
}

async fn live_client() -> Option<Client> {
    Some(connect(&common::live_config()?).await)
}

fn poll_once<T>(future: impl Future<Output = T>) -> Poll<T> {
    pin!(future).poll(&mut Context::from_waker(noop_waker_ref()))
}

fn assert_rejected<T>(future: impl Future<Output = Result<T>>) {
    assert!(
        matches!(
            poll_once(future),
            Poll::Ready(Err(Error::Tds(mssql_tds::error::Error::ConnectionClosed(
                _
            ))))
        ),
        "known-dead I/O must return ConnectionClosed on the first poll"
    );
}

fn assert_retired(client: &mut Client) {
    assert!(client.is_connection_dead());
    assert_rejected(client.simple_query("SELECT 1 AS n"));
    assert_rejected(client.ping());
    assert_rejected(client.reset_session());
}

async fn cancel_after_poll<T>(future: impl Future<Output = T>) {
    let mut future = Box::pin(future);
    assert!(
        poll!(future.as_mut()).is_pending(),
        "repro must reach a pending operation before starting the timeout"
    );
    timeout(CANCEL_AFTER, future)
        .await
        .map(|_| ())
        .expect_err("repro must time out, not complete");
}

async fn assert_select_one(client: &mut Client) {
    let rows = timeout(IO_BOUND, client.simple_query("SELECT 1 AS n"))
        .await
        .expect("follow-up SELECT timed out")
        .expect("follow-up SELECT failed")
        .into_first_result();
    assert_eq!(
        rows.first()
            .expect("expected row at index 0")
            .get::<i32, _>("n"),
        Some(1)
    );
    assert!(!client.is_connection_dead());
}

async fn delayed_results(
    client: &mut Client,
) -> impl futures_core::Stream<Item = Result<Row>> + Unpin + '_ {
    // NOWAIT flushes the row's final packet, which SQL Server may otherwise
    // buffer until the WAITFOR finishes even when the row spans several packets.
    let mut stream = client.simple_query_streamed(
        "SELECT REPLICATE(CAST('x' AS varchar(max)), 16000) AS payload; \
         RAISERROR ('flush first result', 0, 1) WITH NOWAIT; \
         WAITFOR DELAY '00:00:05'; SELECT 'tail' AS payload",
    );
    let first = timeout(Duration::from_secs(3), stream.next())
        .await
        .expect("first result must arrive before WAITFOR completes")
        .expect("missing first row")
        .expect("first row failed");
    assert_eq!(
        first
            .get::<&str, _>("payload")
            .expect("expected non-NULL column payload")
            .len(),
        16000
    );
    stream
}

#[tokio::test]
async fn simple_query_timeout_retires_connection() {
    let Some(mut client) = live_client().await else {
        return;
    };
    cancel_after_poll(client.simple_query(WAIT_QUERY)).await;
    assert_retired(&mut client);
}

#[tokio::test]
async fn parameterized_query_timeout_retires_connection() {
    let Some(mut client) = live_client().await else {
        return;
    };
    cancel_after_poll(client.query("WAITFOR DELAY '00:00:05'; SELECT @P1 AS n", &[&1i32])).await;
    assert_retired(&mut client);
}

#[tokio::test]
async fn empty_parameter_query_timeout_retires_connection() {
    let Some(mut client) = live_client().await else {
        return;
    };
    cancel_after_poll(client.query(WAIT_QUERY, &[])).await;
    assert_retired(&mut client);
}

#[tokio::test]
async fn execute_timeout_retires_connection() {
    let Some(config) = common::live_config() else {
        return;
    };
    for parameterized in [false, true] {
        let mut client = connect(&config).await;
        if parameterized {
            cancel_after_poll(
                client.execute("WAITFOR DELAY '00:00:05'; SELECT @P1 AS n", &[&1i32]),
            )
            .await;
        } else {
            cancel_after_poll(client.execute(WAIT_QUERY, &[])).await;
        }
        assert_retired(&mut client);
    }
}

#[tokio::test]
async fn prepared_query_and_execute_timeout_retire_connection() {
    let Some(config) = common::live_config() else {
        return;
    };
    for execute in [false, true] {
        let mut client = connect(&config).await;
        let statement = timeout(
            IO_BOUND,
            client.prepare("WAITFOR DELAY '00:00:05'; SELECT @P1 AS n", &[&0i32]),
        )
        .await
        .expect("prepare timed out")
        .expect("prepare failed");
        if execute {
            cancel_after_poll(statement.execute(&mut client, &[&1i32])).await;
        } else {
            cancel_after_poll(statement.query(&mut client, &[&1i32])).await;
        }
        assert_retired(&mut client);
    }
}

#[tokio::test]
async fn streamed_initialization_timeout_retires_connection() {
    let Some(config) = common::live_config() else {
        return;
    };
    for parameterized in [false, true] {
        let mut client = connect(&config).await;
        let mut stream = if parameterized {
            client.query_streamed("WAITFOR DELAY '00:00:05'; SELECT @P1 AS n", &[&1i32])
        } else {
            client.simple_query_streamed(WAIT_QUERY)
        };
        cancel_after_poll(stream.next()).await;
        drop(stream);
        assert_retired(&mut client);
    }
}

#[tokio::test]
async fn cancelling_only_next_retains_resumable_stream() {
    let Some(mut client) = live_client().await else {
        return;
    };
    let mut stream =
        client.simple_query_streamed("WAITFOR DELAY '00:00:01'; SELECT 1 AS n; SELECT 2 AS n");
    cancel_after_poll(stream.next()).await;
    let rows: Vec<_> = timeout(IO_BOUND, stream.try_collect())
        .await
        .expect("retained stream did not resume")
        .expect("retained stream failed");
    assert_eq!(rows.len(), 2);
    assert_eq!(
        rows.first()
            .expect("expected row at index 0")
            .get::<i32, _>("n"),
        Some(1)
    );
    assert_eq!(
        rows.get(1)
            .expect("expected row at index 1")
            .get::<i32, _>("n"),
        Some(2)
    );
    assert_select_one(&mut client).await;
}

#[tokio::test]
async fn stream_result_advancement_timeout_retires_connection() {
    let Some(mut client) = live_client().await else {
        return;
    };
    let mut stream = delayed_results(&mut client).await;
    cancel_after_poll(stream.next()).await;
    drop(stream);
    assert_retired(&mut client);
}

#[tokio::test]
async fn ping_does_not_drain_pending_prior_results() {
    let Some(mut client) = live_client().await else {
        return;
    };
    drop(delayed_results(&mut client).await);
    assert!(!client.is_connection_dead());
    assert!(matches!(poll_once(client.ping()), Poll::Ready(Ok(()))));
    assert!(!client.is_connection_dead());
    cancel_after_poll(client.simple_query("SELECT 1 AS n")).await;
    assert_retired(&mut client);
}

#[tokio::test]
async fn cancelling_prepare_during_prior_result_cleanup_retires_connection() {
    let Some(mut client) = live_client().await else {
        return;
    };
    drop(delayed_results(&mut client).await);
    cancel_after_poll(client.prepare("SELECT @P1", &[&0i32])).await;
    assert_retired(&mut client);
}

#[tokio::test]
async fn cancelling_unprepare_during_prior_result_cleanup_retires_connection() {
    let Some(mut client) = live_client().await else {
        return;
    };
    let statement = timeout(IO_BOUND, client.prepare("SELECT 1", &[]))
        .await
        .expect("prepare timed out")
        .expect("prepare failed");
    drop(delayed_results(&mut client).await);
    cancel_after_poll(statement.close(&mut client)).await;
    assert_retired(&mut client);
}

#[tokio::test]
async fn cancelled_reset_rejects_dead_state_before_stale_prepared_handles() {
    let Some(mut client) = live_client().await else {
        return;
    };
    let statement = timeout(IO_BOUND, client.prepare("SELECT @P1", &[&0i32]))
        .await
        .expect("prepare timed out")
        .expect("prepare failed");
    drop(delayed_results(&mut client).await);
    cancel_after_poll(client.reset_session()).await;
    assert_retired(&mut client);
    assert_rejected(statement.query(&mut client, &[&1i32]));
    assert_rejected(statement.execute(&mut client, &[&1i32]));
    assert_rejected(statement.close(&mut client));
}

#[tokio::test]
async fn dropping_stream_between_rows_preserves_reuse() {
    let Some(mut client) = live_client().await else {
        return;
    };
    {
        let mut stream =
            client.simple_query_streamed("SELECT 1 AS n UNION ALL SELECT 2; SELECT 3 AS n");
        let first = timeout(IO_BOUND, stream.next())
            .await
            .expect("first row timed out")
            .expect("missing first row")
            .expect("first row failed");
        assert_eq!(first.get::<i32, _>("n"), Some(1));
    }
    assert_select_one(&mut client).await;
}

#[tokio::test]
async fn dropping_compat_stream_after_metadata_preserves_reuse() {
    let Some(mut client) = live_client().await else {
        return;
    };
    {
        let mut stream =
            client.simple_query_compat("SELECT 1 AS n UNION ALL SELECT 2; SELECT 3 AS n");
        let columns = timeout(IO_BOUND, stream.columns())
            .await
            .expect("metadata timed out")
            .expect("metadata failed")
            .expect("missing metadata");
        assert_eq!(columns.first().expect("metadata column").name(), "n");
    }
    assert_select_one(&mut client).await;
}

#[tokio::test]
async fn compat_columns_sql_error_preserves_reuse() {
    let Some(mut client) = live_client().await else {
        return;
    };
    {
        let mut stream =
            client.simple_query_compat("RAISERROR ('expected compatibility error', 16, 1)");
        let result = timeout(IO_BOUND, stream.columns())
            .await
            .expect("compatibility error timed out");
        assert!(matches!(
            result,
            Err(Error::Tds(mssql_tds::error::Error::SqlServerError { .. }))
        ));
    }
    assert_select_one(&mut client).await;
}

#[tokio::test]
async fn unpolled_operations_and_unused_builders_preserve_reuse() {
    let Some(mut client) = live_client().await else {
        return;
    };
    drop(client.simple_query(WAIT_QUERY));
    drop(client.query(WAIT_QUERY, &[]));
    drop(client.execute(WAIT_QUERY, &[]));
    drop(client.prepare("SELECT @P1", &[&0i32]));
    drop(client.ping());
    drop(client.reset_session());
    drop(client.simple_query_streamed(WAIT_QUERY));
    drop(client.query_streamed("SELECT @P1", &[&1i32]));
    drop(client.simple_query_compat(WAIT_QUERY));
    drop(client.query_compat("SELECT @P1", &[&1i32]));
    drop(client.bulk_insert("#unused"));
    drop(client.bulk_insert_with_columns("#unused", &["n"]));
    drop(
        client
            .bulk_insert("#unused")
            .send(std::iter::empty::<PausedRow<'_>>()),
    );
    assert_select_one(&mut client).await;
}

#[tokio::test]
async fn ordinary_sql_error_preserves_reuse() {
    let Some(mut client) = live_client().await else {
        return;
    };
    let result = timeout(
        IO_BOUND,
        client.simple_query("RAISERROR ('expected cancellation regression error', 16, 1)"),
    )
    .await
    .expect("SQL error response timed out");
    assert!(matches!(
        result,
        Err(Error::Tds(mssql_tds::error::Error::SqlServerError { .. }))
    ));
    assert_select_one(&mut client).await;
}

struct PausedRow<'a> {
    entered: &'a Notify,
}

#[async_trait]
impl BulkLoadRow for PausedRow<'_> {
    async fn write_to_packet(
        &self,
        writer: &mut StreamingBulkLoadWriter<'_>,
        column_index: &mut usize,
    ) -> TdsResult<()> {
        writer
            .write_column_value(*column_index, &ColumnValues::Int(1))
            .await?;
        *column_index += 1;
        self.entered.notify_one();
        pending().await
    }
}

#[tokio::test]
async fn cancelling_bulk_write_marks_native_connection_dead() {
    let Some(config) = common::live_config() else {
        return;
    };
    for raw_constructor in [false, true] {
        let mut client = connect(&config).await;
        timeout(
            IO_BOUND,
            client.simple_query("CREATE TABLE #CancelBulk (n int NOT NULL)"),
        )
        .await
        .expect("bulk setup timed out")
        .expect("bulk setup failed");
        let entered = Notify::new();
        let bulk = if raw_constructor {
            BulkInsert::new(client.inner_mut(), "#CancelBulk")
        } else {
            client.bulk_insert_with_columns("#CancelBulk", &["n"])
        };
        let mut send = Box::pin(bulk.send([PausedRow { entered: &entered }]));
        let entered_row_writer = timeout(IO_BOUND, async {
            tokio::select! {
                _ = entered.notified() => true,
                _ = send.as_mut() => false,
            }
        })
        .await
        .expect("bulk send never entered row writing");
        assert!(
            entered_row_writer,
            "bulk send must remain pending inside write_to_packet"
        );
        drop(send);
        assert_retired(&mut client);
        assert_rejected(
            client
                .bulk_insert("#CancelBulk")
                .send(std::iter::empty::<PausedRow<'_>>()),
        );
    }
}

#[tokio::test]
async fn all_entry_points_reject_native_dead_state_immediately() {
    let Some(mut client) = live_client().await else {
        return;
    };
    let statement = timeout(IO_BOUND, client.prepare("SELECT @P1 AS n", &[&0i32]))
        .await
        .expect("prepare timed out")
        .expect("prepare failed");
    let other = timeout(IO_BOUND, client.prepare("SELECT 2 AS n", &[]))
        .await
        .expect("prepare timed out")
        .expect("prepare failed");
    client.inner_mut().mark_connection_dead();
    assert_retired(&mut client);
    assert_rejected(client.query("SELECT @P1", &[&1i32]));
    assert_rejected(client.query("SELECT 1", &[]));
    assert_rejected(client.execute("SELECT @P1", &[&1i32]));
    assert_rejected(client.execute("SELECT 1", &[]));
    assert_rejected(client.prepare("SELECT 1", &[]));
    assert_rejected(client.query_prepared(&statement, &[&1i32]));
    assert_rejected(client.execute_prepared(&statement, &[&1i32]));
    assert_rejected(statement.query(&mut client, &[&1i32]));
    assert_rejected(statement.execute(&mut client, &[&1i32]));
    assert_rejected(client.unprepare(statement));
    assert_rejected(other.close(&mut client));
    {
        let mut stream = client.simple_query_streamed("SELECT 1");
        assert_rejected(async { stream.next().await.expect("missing stream error") });
    }
    {
        let mut stream = client.query_streamed("SELECT @P1", &[&1i32]);
        assert_rejected(async { stream.next().await.expect("missing stream error") });
    }
    assert_rejected(
        client
            .bulk_insert("#unused")
            .send(std::iter::empty::<PausedRow<'_>>()),
    );
    assert_rejected(
        client
            .bulk_insert_with_columns("#unused", &["n"])
            .send(std::iter::empty::<PausedRow<'_>>()),
    );
    assert_rejected(
        BulkInsert::new(client.inner_mut(), "#unused").send(std::iter::empty::<PausedRow<'_>>()),
    );
    #[cfg(feature = "arrow")]
    {
        use std::sync::Arc;

        use arrow_array::RecordBatch;
        use arrow_schema::Schema;

        let batch = RecordBatch::new_empty(Arc::new(Schema::empty()));
        assert_rejected(client.bulk_insert("#unused").send_arrow(batch.clone()));
        assert_rejected(client.bulk_insert("#unused").send_arrow_batches([batch]));
    }
}

#[tokio::test]
async fn pool_rejects_cancelled_client_and_replaces_its_session() {
    let Some(config) = common::live_config() else {
        return;
    };
    for method in [RecyclingMethod::Reset, RecyclingMethod::Ping] {
        let manager = TdsManager::new(config.clone()).with_recycling_method(method);
        let pool = Pool::builder(manager.clone())
            .max_size(1)
            .build()
            .expect("pool build failed");
        let mut connection = timeout(IO_BOUND, Box::pin(pool.get()))
            .await
            .expect("pool checkout timed out")
            .expect("pool checkout failed");
        timeout(
            IO_BOUND,
            connection.simple_query("CREATE TABLE #CancelledSessionMarker (n int)"),
        )
        .await
        .expect("session marker setup timed out")
        .expect("session marker setup failed");
        cancel_after_poll(connection.simple_query(WAIT_QUERY)).await;
        assert!(connection.is_connection_dead());
        assert!(matches!(
            poll_once(manager.recycle(&mut connection, &Metrics::default())),
            Poll::Ready(Err(RecycleError::Backend(Error::Tds(
                mssql_tds::error::Error::ConnectionClosed(_)
            ))))
        ));
        drop(connection);

        let mut replacement = timeout(IO_BOUND, Box::pin(pool.get()))
            .await
            .expect("pool stalled recycling cancelled client")
            .expect("replacement checkout failed");
        assert_eq!(
            Object::metrics(&replacement).recycle_count,
            0,
            "cancelled client must be replaced, not merely reset"
        );
        let rows = timeout(
            IO_BOUND,
            replacement.simple_query(
                "SELECT CASE WHEN OBJECT_ID('tempdb..#CancelledSessionMarker') \
                 IS NULL THEN 1 ELSE 0 END AS fresh",
            ),
        )
        .await
        .expect("replacement query timed out")
        .expect("replacement query failed")
        .into_first_result();
        assert_eq!(
            rows.first()
                .expect("expected row at index 0")
                .get::<i32, _>("fresh"),
            Some(1)
        );
        assert_select_one(&mut replacement).await;
    }
}
