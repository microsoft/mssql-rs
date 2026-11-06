// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.
// Portions derived from saurabh500/mssql-tiberius-bridge (MIT).

//! Integration tests for [`QueryResult::into_row_stream`].
//!
//! Mirrors tiberius' `into_row_stream_should_work` test from
//! `tiberius/tests/query.rs`. Verifies the streaming API contract that
//! windmill's S3 export path depends on (`.into_row_stream().map(...).next()`).

pub mod common;

use futures_util::StreamExt;
use futures_util::TryStreamExt;
use mssql_rs::{Client, Row};

async fn connect() -> Client {
    Client::connect(&common::live_config_or_exit())
        .await
        .expect("connect to test SQL Server")
}

#[tokio::test]
async fn into_row_stream_yields_all_rows() {
    let mut client = connect().await;
    let stream = client
        .simple_query("SELECT 1 AS n UNION ALL SELECT 2 UNION ALL SELECT 3")
        .await
        .expect("query succeeds: SELECT 1 AS n UNION ALL SELECT 2 UNION ALL SELECT 3")
        .into_row_stream();

    let rows: Vec<Row> = stream.try_collect().await.expect("collect query rows");
    assert_eq!(rows.len(), 3);
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
    assert_eq!(
        rows.get(2)
            .expect("expected row at index 2")
            .get::<i32, _>("n"),
        Some(3)
    );
}

#[tokio::test]
async fn into_row_stream_supports_map_next() {
    // Mirrors the windmill S3 export pattern:
    //   stream.map(|row| row_to_json(row?)).next().await
    let mut client = connect().await;
    let mut stream = client
        .simple_query("SELECT 'hello' AS s UNION ALL SELECT 'world'")
        .await
        .expect("query succeeds: SELECT 'hello' AS s UNION ALL SELECT 'world'")
        .into_row_stream()
        .map(|row| {
            row.map(|r| {
                r.get::<&str, _>("s")
                    .expect("expected non-NULL column s")
                    .to_string()
            })
        });

    let mut got = Vec::new();
    while let Some(item) = stream.next().await {
        got.push(item.expect("read stream row"));
    }
    assert_eq!(got, vec!["hello".to_string(), "world".to_string()]);
}

#[tokio::test]
async fn into_row_stream_dropped_early_does_not_panic() {
    // Mirrors tiberius'
    // `drop_stream_before_handling_all_results_should_not_cause_weird_things`.
    // Because the bridge buffers rows up-front, dropping the stream simply
    // drops the in-memory Vec — no wire state to corrupt. The client must
    // remain usable for subsequent queries.
    let mut client = connect().await;
    {
        let mut stream = client
            .simple_query("SELECT 1 AS n UNION ALL SELECT 2 UNION ALL SELECT 3")
            .await
            .expect("query succeeds: SELECT 1 AS n UNION ALL SELECT 2 UNION ALL SELECT 3")
            .into_row_stream();
        // Pull only the first row, then drop.
        stream
            .next()
            .await
            .expect("expected first stream row")
            .expect("read first stream row");
    }

    // Client should still work for further queries.
    let rows = client
        .simple_query("SELECT 42 AS n")
        .await
        .expect("query succeeds: SELECT 42 AS n");
    let rows = rows.into_first_result();
    assert_eq!(
        rows.first()
            .expect("expected row at index 0")
            .get::<i32, _>("n"),
        Some(42)
    );
}

#[tokio::test]
async fn into_row_stream_empty_result_set() {
    let mut client = connect().await;
    let stream = client
        .simple_query("SELECT 1 AS n WHERE 1 = 0")
        .await
        .expect("query succeeds: SELECT 1 AS n WHERE 1 = 0")
        .into_row_stream();
    let rows: Vec<Row> = stream.try_collect().await.expect("collect query rows");
    assert_eq!(rows.len(), 0);
}

#[tokio::test]
async fn into_row_stream_flattens_multiple_result_sets() {
    let mut client = connect().await;
    let stream = client
        .simple_query("SELECT 1 AS n; SELECT 2 AS n")
        .await
        .expect("query succeeds: SELECT 1 AS n; SELECT 2 AS n")
        .into_row_stream();
    let rows: Vec<Row> = stream.try_collect().await.expect("collect query rows");
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
}

// =============================================================================
// True wire-level streaming: Client::query_streamed / simple_query_streamed
// =============================================================================

#[tokio::test]
async fn query_streamed_yields_all_rows_in_order() {
    let mut client = connect().await;
    let stream = client.simple_query_streamed(
        "SELECT 1 AS n UNION ALL SELECT 2 UNION ALL SELECT 3 UNION ALL SELECT 4",
    );
    let rows: Vec<Row> = stream.try_collect().await.expect("collect query rows");
    assert_eq!(rows.len(), 4);
    for (i, row) in rows.iter().enumerate() {
        assert_eq!(
            row.get::<i32, _>("n"),
            Some(i32::try_from(i).expect("row index fits i32") + 1)
        );
    }
}

#[tokio::test]
async fn query_streamed_with_params_roundtrips() {
    let mut client = connect().await;
    let stream = client.query_streamed("SELECT @P1 AS a, @P2 AS b", &[&7i32, &"hi"]);
    let rows: Vec<Row> = stream.try_collect().await.expect("collect query rows");
    assert_eq!(rows.len(), 1);
    assert_eq!(
        rows.first()
            .expect("expected row at index 0")
            .get::<i32, _>("a"),
        Some(7)
    );
    assert_eq!(
        rows.first()
            .expect("expected row at index 0")
            .get::<&str, _>("b"),
        Some("hi")
    );
}

#[tokio::test]
async fn query_streamed_empty_result_set() {
    let mut client = connect().await;
    let stream = client.simple_query_streamed("SELECT 1 AS n WHERE 1 = 0");
    let rows: Vec<Row> = stream.try_collect().await.expect("collect query rows");
    assert!(rows.is_empty());
}

#[tokio::test]
async fn query_streamed_flattens_multiple_result_sets() {
    let mut client = connect().await;
    let stream = client.simple_query_streamed("SELECT 1 AS n; SELECT 2 AS n; SELECT 3 AS n");
    let rows: Vec<Row> = stream.try_collect().await.expect("collect query rows");
    assert_eq!(rows.len(), 3);
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
    assert_eq!(
        rows.get(2)
            .expect("expected row at index 2")
            .get::<i32, _>("n"),
        Some(3)
    );
}

#[tokio::test]
async fn query_streamed_skips_no_row_statements() {
    let mut client = connect().await;
    client
        .simple_query("CREATE TABLE #stream_results (id int)")
        .await
        .expect("query succeeds: CREATE TABLE #stream_results (id int)");
    let rows: Vec<Row> = client
        .query_streamed(
            "INSERT INTO #stream_results VALUES (@P1); \
             SELECT id FROM #stream_results WHERE 1 = 0; \
             PRINT 'between results'; \
             SELECT id FROM #stream_results; \
             DELETE FROM #stream_results; \
             SELECT @P1 + 1 AS id",
            &[&7i32],
        )
        .try_collect()
        .await
        .expect("collect query rows");
    assert_eq!(rows.len(), 2);
    assert_eq!(
        rows.first()
            .expect("expected row at index 0")
            .get::<i32, _>("id"),
        Some(7)
    );
    assert_eq!(
        rows.get(1)
            .expect("expected row at index 1")
            .get::<i32, _>("id"),
        Some(8)
    );
    client.ping().await.expect("ping connection");
}

#[tokio::test]
async fn query_streamed_pulls_lazily_one_at_a_time() {
    // Pull only the first row, then drop. Because we yield row-by-row,
    // the first .next() must not have to materialize all rows. We can't
    // cheaply observe wire-level laziness directly, but we can verify
    // partial consumption returns the right element ordering.
    let mut client = connect().await;
    {
        let mut stream =
            client.simple_query_streamed("SELECT 10 AS n UNION ALL SELECT 20 UNION ALL SELECT 30");
        let first = stream
            .next()
            .await
            .expect("expected stream row")
            .expect("read stream row");
        assert_eq!(first.get::<i32, _>("n"), Some(10));
        // Drop stream mid-result-set.
    }
    // Client must remain usable for a follow-up query (this exercises
    // mssql-tds' state recovery when an unread result set is abandoned).
    let rs = client
        .simple_query("SELECT 99 AS n")
        .await
        .expect("query succeeds: SELECT 99 AS n");
    let rows = rs.into_first_result();
    assert_eq!(
        rows.first()
            .expect("expected row at index 0")
            .get::<i32, _>("n"),
        Some(99)
    );
}

#[tokio::test]
async fn query_streamed_supports_map_next() {
    // Mirrors windmill's S3 export pattern.
    let mut client = connect().await;
    let stream = client
        .simple_query_streamed("SELECT 'a' AS s UNION ALL SELECT 'b'")
        .map(|row| {
            row.map(|r| {
                r.get::<&str, _>("s")
                    .expect("expected non-NULL column s")
                    .to_string()
            })
        });
    let got: Vec<String> = stream.try_collect().await.expect("collect query rows");
    assert_eq!(got, vec!["a".to_string(), "b".to_string()]);
}
