// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Integration tests for cursor RPC methods: cursor_open, cursor_fetch,
//! cursor_close, plus the prepared-cursor lifecycle (cursor_prepexec,
//! cursor_execute, cursor_prepare, cursor_unprepare).
//! These run against a live SQL Server instance (Docker).

mod common;

use common::{begin_connection, build_tcp_datasource};
use mssql_tds::connection::cursor_ops::CursorClient;
use mssql_tds::connection::tds_client::ResultSet;
use mssql_tds::cursor::{
    CursorConcurrency, CursorOperation, CursorOptionCode, CursorOptionValue, CursorScrollOption,
    CursorStatus, FetchDirection, FetchStatus,
};
use mssql_tds::datatypes::column_values::ColumnValues;
use mssql_tds::datatypes::sqltypes::SqlType;
use mssql_tds::message::parameters::rpc_parameters::{RpcParameter, StatusFlags};

/// Set up a temp table with the given number of rows.
async fn setup_temp_table(
    client: &mut mssql_tds::connection::tds_client::TdsClient,
    num_rows: i32,
) {
    client
        .execute(
            "CREATE TABLE #ct (id INT PRIMARY KEY, name NVARCHAR(50), value INT)".to_string(),
            (),
        )
        .await
        .unwrap();
    client.close_query().await.unwrap();

    if num_rows > 0 {
        let mut sql = String::from("INSERT INTO #ct (id, name, value) VALUES ");
        for i in 1..=num_rows {
            if i > 1 {
                sql.push_str(", ");
            }
            sql.push_str(&format!("({i}, 'row_{i}', {0})", i * 10));
        }
        client.execute(sql, ()).await.unwrap();
        client.close_query().await.unwrap();
    }
}

/// Read all available rows from the current result set.
async fn read_all_rows(
    client: &mut mssql_tds::connection::tds_client::TdsClient,
) -> Vec<Vec<ColumnValues>> {
    let mut rows = Vec::new();
    if let Some(rs) = client.get_current_resultset() {
        while let Some(row) = rs.next_row().await.unwrap() {
            rows.push(row);
        }
    }
    client.close_query().await.unwrap();
    rows
}

// --- Basic Lifecycle Tests ---

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cursor_open_forward_only_and_close() {
    let mut client = begin_connection(&build_tcp_datasource()).await;
    setup_temp_table(&mut client, 100).await;

    let resp = client
        .cursor_open(
            "SELECT id, name, value FROM #ct ORDER BY id",
            CursorScrollOption::FORWARD_ONLY,
            CursorConcurrency::READONLY,
            0,
            None,
            None,
        )
        .await
        .unwrap();

    assert_ne!(
        resp.cursor_id, 0,
        "Server should assign a non-zero cursor handle"
    );

    client
        .cursor_close(resp.cursor_id, None, None)
        .await
        .unwrap();
    client.close_connection().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cursor_open_fetch_next_close() {
    let mut client = begin_connection(&build_tcp_datasource()).await;
    setup_temp_table(&mut client, 100).await;

    let resp = client
        .cursor_open(
            "SELECT id, name, value FROM #ct ORDER BY id",
            CursorScrollOption::FORWARD_ONLY,
            CursorConcurrency::READONLY,
            0,
            None,
            None,
        )
        .await
        .unwrap();

    client
        .cursor_fetch(resp.cursor_id, FetchDirection::NEXT, 0, 5, None, None)
        .await
        .unwrap();

    let rows = read_all_rows(&mut client).await;
    assert_eq!(rows.len(), 5, "Expected 5 rows from first fetch");
    assert_eq!(rows[0][0], ColumnValues::Int(1));

    client
        .cursor_close(resp.cursor_id, None, None)
        .await
        .unwrap();
    client.close_connection().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cursor_fetch_all_rows() {
    let mut client = begin_connection(&build_tcp_datasource()).await;
    setup_temp_table(&mut client, 100).await;

    let resp = client
        .cursor_open(
            "SELECT id FROM #ct ORDER BY id",
            CursorScrollOption::FORWARD_ONLY,
            CursorConcurrency::READONLY,
            0,
            None,
            None,
        )
        .await
        .unwrap();

    let mut total_rows = 0;
    loop {
        client
            .cursor_fetch(resp.cursor_id, FetchDirection::NEXT, 0, 20, None, None)
            .await
            .unwrap();

        let rows = read_all_rows(&mut client).await;
        if rows.is_empty() {
            break;
        }
        total_rows += rows.len();
    }

    assert_eq!(total_rows, 100, "Expected all 100 rows fetched");

    client
        .cursor_close(resp.cursor_id, None, None)
        .await
        .unwrap();
    client.close_connection().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cursor_open_static_scroll() {
    let mut client = begin_connection(&build_tcp_datasource()).await;
    setup_temp_table(&mut client, 100).await;

    let resp = client
        .cursor_open(
            "SELECT id, value FROM #ct ORDER BY id",
            CursorScrollOption::STATIC,
            CursorConcurrency::READONLY,
            0,
            None,
            None,
        )
        .await
        .unwrap();

    // FIRST
    client
        .cursor_fetch(resp.cursor_id, FetchDirection::FIRST, 0, 1, None, None)
        .await
        .unwrap();
    let rows = read_all_rows(&mut client).await;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0][0], ColumnValues::Int(1));

    // LAST
    client
        .cursor_fetch(resp.cursor_id, FetchDirection::LAST, 0, 1, None, None)
        .await
        .unwrap();
    let rows = read_all_rows(&mut client).await;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0][0], ColumnValues::Int(100));

    // PREV (from last -> row 99)
    client
        .cursor_fetch(resp.cursor_id, FetchDirection::PREV, 0, 1, None, None)
        .await
        .unwrap();
    let rows = read_all_rows(&mut client).await;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0][0], ColumnValues::Int(99));

    // NEXT (from 99 -> row 100)
    client
        .cursor_fetch(resp.cursor_id, FetchDirection::NEXT, 0, 1, None, None)
        .await
        .unwrap();
    let rows = read_all_rows(&mut client).await;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0][0], ColumnValues::Int(100));

    // ABSOLUTE row 50
    client
        .cursor_fetch(resp.cursor_id, FetchDirection::ABSOLUTE, 50, 1, None, None)
        .await
        .unwrap();
    let rows = read_all_rows(&mut client).await;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0][0], ColumnValues::Int(50));

    // RELATIVE +5 (from 50 -> row 55)
    client
        .cursor_fetch(resp.cursor_id, FetchDirection::RELATIVE, 5, 1, None, None)
        .await
        .unwrap();
    let rows = read_all_rows(&mut client).await;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0][0], ColumnValues::Int(55));

    client
        .cursor_close(resp.cursor_id, None, None)
        .await
        .unwrap();
    client.close_connection().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cursor_open_keyset() {
    let mut client = begin_connection(&build_tcp_datasource()).await;
    setup_temp_table(&mut client, 100).await;

    let resp = client
        .cursor_open(
            "SELECT id, name FROM #ct ORDER BY id",
            CursorScrollOption::KEYSET_DRIVEN,
            CursorConcurrency::READONLY,
            0,
            None,
            None,
        )
        .await
        .unwrap();

    assert_ne!(resp.cursor_id, 0);

    client
        .cursor_fetch(resp.cursor_id, FetchDirection::NEXT, 0, 3, None, None)
        .await
        .unwrap();
    let rows = read_all_rows(&mut client).await;
    assert_eq!(rows.len(), 3);
    assert_eq!(rows[0][0], ColumnValues::Int(1));

    client
        .cursor_close(resp.cursor_id, None, None)
        .await
        .unwrap();
    client.close_connection().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cursor_open_dynamic() {
    let mut client = begin_connection(&build_tcp_datasource()).await;
    setup_temp_table(&mut client, 100).await;

    let resp = client
        .cursor_open(
            "SELECT id, name FROM #ct",
            CursorScrollOption::DYNAMIC,
            CursorConcurrency::READONLY,
            0,
            None,
            None,
        )
        .await
        .unwrap();

    assert_ne!(resp.cursor_id, 0);

    client
        .cursor_fetch(resp.cursor_id, FetchDirection::NEXT, 0, 3, None, None)
        .await
        .unwrap();
    let rows = read_all_rows(&mut client).await;
    assert_eq!(rows.len(), 3);

    client
        .cursor_close(resp.cursor_id, None, None)
        .await
        .unwrap();
    client.close_connection().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cursor_open_fast_forward() {
    let mut client = begin_connection(&build_tcp_datasource()).await;
    setup_temp_table(&mut client, 100).await;

    let resp = client
        .cursor_open(
            "SELECT id FROM #ct ORDER BY id",
            CursorScrollOption::FAST_FORWARD_ONLY,
            CursorConcurrency::READONLY,
            0,
            None,
            None,
        )
        .await
        .unwrap();

    assert_ne!(resp.cursor_id, 0);

    client
        .cursor_fetch(resp.cursor_id, FetchDirection::NEXT, 0, 10, None, None)
        .await
        .unwrap();
    let rows = read_all_rows(&mut client).await;
    assert_eq!(rows.len(), 10);
    assert_eq!(rows[0][0], ColumnValues::Int(1));
    assert_eq!(rows[9][0], ColumnValues::Int(10));

    client
        .cursor_close(resp.cursor_id, None, None)
        .await
        .unwrap();
    client.close_connection().await.unwrap();
}

// --- Negotiation and Edge Case Tests ---

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cursor_type_negotiation_downgrade() {
    let mut client = begin_connection(&build_tcp_datasource()).await;
    setup_temp_table(&mut client, 100).await;

    // DISTINCT queries cannot use KEYSET cursors — server should downgrade.
    // Include STATIC_ACCEPTABLE so the server has a valid fallback.
    let resp = client
        .cursor_open(
            "SELECT DISTINCT name FROM #ct",
            CursorScrollOption::KEYSET_DRIVEN
                | CursorScrollOption::CHECK_ACCEPTED_TYPES
                | CursorScrollOption::STATIC_ACCEPTABLE,
            CursorConcurrency::READONLY,
            0,
            None,
            None,
        )
        .await
        .unwrap();

    assert_ne!(resp.cursor_id, 0, "Cursor should have been opened");

    // The server should have downgraded from KEYSET.
    assert!(
        !resp
            .negotiated_scroll
            .contains(CursorScrollOption::KEYSET_DRIVEN),
        "Expected server to downgrade KEYSET on DISTINCT query, got {:?}",
        resp.negotiated_scroll
    );

    client
        .cursor_close(resp.cursor_id, None, None)
        .await
        .unwrap();
    client.close_connection().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cursor_open_empty_result() {
    let mut client = begin_connection(&build_tcp_datasource()).await;
    setup_temp_table(&mut client, 10).await;

    let resp = client
        .cursor_open(
            "SELECT id FROM #ct WHERE 1 = 0",
            CursorScrollOption::FORWARD_ONLY,
            CursorConcurrency::READONLY,
            0,
            None,
            None,
        )
        .await
        .unwrap();

    if resp.cursor_id != 0 {
        client
            .cursor_fetch(resp.cursor_id, FetchDirection::NEXT, 0, 10, None, None)
            .await
            .unwrap();
        let rows = read_all_rows(&mut client).await;
        assert_eq!(rows.len(), 0, "Empty result set should return no rows");

        client
            .cursor_close(resp.cursor_id, None, None)
            .await
            .unwrap();
    }

    client.close_connection().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cursor_close_invalid_handle() {
    let mut client = begin_connection(&build_tcp_datasource()).await;

    // Closing an invalid handle should return an error, not panic.
    let result = client.cursor_close(999999, None, None).await;
    // The server may return an error or drain might succeed silently.
    // Either outcome is acceptable -- the critical thing is no panic.
    if let Err(e) = &result {
        let msg = format!("{e:?}");
        assert!(
            msg.contains("cursor") || msg.contains("16909") || msg.contains("invalid"),
            "Expected cursor-related error, got: {msg}"
        );
    }

    client.close_connection().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cursor_fetch_past_end() {
    let mut client = begin_connection(&build_tcp_datasource()).await;
    setup_temp_table(&mut client, 3).await;

    let resp = client
        .cursor_open(
            "SELECT id FROM #ct ORDER BY id",
            CursorScrollOption::FORWARD_ONLY,
            CursorConcurrency::READONLY,
            0,
            None,
            None,
        )
        .await
        .unwrap();

    // Fetch all 3 rows
    client
        .cursor_fetch(resp.cursor_id, FetchDirection::NEXT, 0, 10, None, None)
        .await
        .unwrap();
    let rows = read_all_rows(&mut client).await;
    assert_eq!(rows.len(), 3);

    // Fetch again -- should get no rows (past end)
    client
        .cursor_fetch(resp.cursor_id, FetchDirection::NEXT, 0, 10, None, None)
        .await
        .unwrap();
    let rows = read_all_rows(&mut client).await;
    assert_eq!(rows.len(), 0, "Fetch past end should return no rows");

    client
        .cursor_close(resp.cursor_id, None, None)
        .await
        .unwrap();
    client.close_connection().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cursor_open_and_immediate_fetch() {
    let mut client = begin_connection(&build_tcp_datasource()).await;
    setup_temp_table(&mut client, 5).await;

    // Open without AUTO_FETCH, then immediately fetch all rows.
    let resp = client
        .cursor_open(
            "SELECT id FROM #ct ORDER BY id",
            CursorScrollOption::FORWARD_ONLY,
            CursorConcurrency::READONLY,
            0,
            None,
            None,
        )
        .await
        .unwrap();

    assert_ne!(resp.cursor_id, 0);

    client
        .cursor_fetch(resp.cursor_id, FetchDirection::NEXT, 0, 10, None, None)
        .await
        .unwrap();

    let rows = read_all_rows(&mut client).await;
    assert_eq!(rows.len(), 5, "Should return all 5 rows in one fetch");
    assert_eq!(rows[0][0], ColumnValues::Int(1));
    assert_eq!(rows[4][0], ColumnValues::Int(5));

    client
        .cursor_close(resp.cursor_id, None, None)
        .await
        .unwrap();
    client.close_connection().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cursor_auto_close() {
    let mut client = begin_connection(&build_tcp_datasource()).await;
    setup_temp_table(&mut client, 3).await;

    let resp = client
        .cursor_open(
            "SELECT id FROM #ct ORDER BY id",
            CursorScrollOption::FAST_FORWARD_ONLY | CursorScrollOption::AUTO_CLOSE,
            CursorConcurrency::READONLY,
            0,
            None,
            None,
        )
        .await
        .unwrap();

    assert_ne!(resp.cursor_id, 0);

    // First fetch — returns all 3 rows. AUTO_CLOSE detects the cursor
    // is now past end-of-result and closes it server-side.
    client
        .cursor_fetch(resp.cursor_id, FetchDirection::NEXT, 0, 10, None, None)
        .await
        .unwrap();
    let rows = read_all_rows(&mut client).await;
    assert_eq!(rows.len(), 3);

    // Second fetch on the now-closed cursor should fail with an invalid handle.
    let fetch_result = client
        .cursor_fetch(resp.cursor_id, FetchDirection::NEXT, 0, 10, None, None)
        .await;
    assert!(
        fetch_result.is_err(),
        "Fetching from an auto-closed cursor should fail"
    );

    client.close_connection().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cursor_open_rejects_auto_fetch() {
    let mut client = begin_connection(&build_tcp_datasource()).await;

    let result = client
        .cursor_open(
            "SELECT 1",
            CursorScrollOption::FORWARD_ONLY | CursorScrollOption::AUTO_FETCH,
            CursorConcurrency::READONLY,
            1,
            None,
            None,
        )
        .await;

    assert!(result.is_err(), "AUTO_FETCH should be rejected");
    let err_msg = format!("{}", result.unwrap_err());
    assert!(
        err_msg.contains("AUTO_FETCH is not yet supported"),
        "Error should mention AUTO_FETCH: {err_msg}"
    );

    client.close_connection().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cursor_open_with_params_rejects_auto_fetch() {
    let mut client = begin_connection(&build_tcp_datasource()).await;

    let result = client
        .cursor_open_with_params(
            "SELECT @p1",
            vec![
                mssql_tds::message::parameters::rpc_parameters::RpcParameter::new(
                    Some("@p1".to_string()),
                    mssql_tds::message::parameters::rpc_parameters::StatusFlags::NONE,
                    mssql_tds::datatypes::sqltypes::SqlType::Int(Some(42)),
                ),
            ],
            CursorScrollOption::DYNAMIC | CursorScrollOption::AUTO_FETCH,
            CursorConcurrency::READONLY,
            1,
            None,
            None,
        )
        .await;

    assert!(result.is_err(), "AUTO_FETCH should be rejected");
    let err_msg = format!("{}", result.unwrap_err());
    assert!(
        err_msg.contains("AUTO_FETCH is not yet supported"),
        "Error should mention AUTO_FETCH: {err_msg}"
    );

    client.close_connection().await.unwrap();
}

// --- cursor_open_with_params happy-path ---

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cursor_open_with_params_success() {
    use mssql_tds::datatypes::sqltypes::SqlType;
    use mssql_tds::message::parameters::rpc_parameters::{RpcParameter, StatusFlags};

    let mut client = begin_connection(&build_tcp_datasource()).await;
    setup_temp_table(&mut client, 100).await;

    let resp = client
        .cursor_open_with_params(
            "SELECT id, name, value FROM #ct WHERE id >= @min_id AND id <= @max_id ORDER BY id",
            vec![
                RpcParameter::new(
                    Some("@min_id".to_string()),
                    StatusFlags::NONE,
                    SqlType::Int(Some(10)),
                ),
                RpcParameter::new(
                    Some("@max_id".to_string()),
                    StatusFlags::NONE,
                    SqlType::Int(Some(15)),
                ),
            ],
            CursorScrollOption::STATIC,
            CursorConcurrency::READONLY,
            0,
            None,
            None,
        )
        .await
        .unwrap();

    assert_ne!(resp.cursor_id, 0, "Server should assign a cursor handle");

    client
        .cursor_fetch(resp.cursor_id, FetchDirection::NEXT, 0, 100, None, None)
        .await
        .unwrap();
    let rows = read_all_rows(&mut client).await;
    assert_eq!(rows.len(), 6, "Expected rows 10..=15");
    assert_eq!(rows[0][0], ColumnValues::Int(10));
    assert_eq!(rows[5][0], ColumnValues::Int(15));

    client
        .cursor_close(resp.cursor_id, None, None)
        .await
        .unwrap();
    client.close_connection().await.unwrap();
}

// --- Concurrent cursors on one connection ---

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cursor_concurrent_on_single_connection() {
    let mut client = begin_connection(&build_tcp_datasource()).await;
    setup_temp_table(&mut client, 20).await;

    let resp1 = client
        .cursor_open(
            "SELECT id FROM #ct WHERE id <= 10 ORDER BY id",
            CursorScrollOption::STATIC,
            CursorConcurrency::READONLY,
            0,
            None,
            None,
        )
        .await
        .unwrap();
    assert_ne!(resp1.cursor_id, 0);

    // Fetch from first cursor, consume rows
    client
        .cursor_fetch(resp1.cursor_id, FetchDirection::NEXT, 0, 5, None, None)
        .await
        .unwrap();
    let rows1 = read_all_rows(&mut client).await;
    assert_eq!(rows1.len(), 5);
    assert_eq!(rows1[0][0], ColumnValues::Int(1));

    // Open second cursor while first is still open
    let resp2 = client
        .cursor_open(
            "SELECT id FROM #ct WHERE id > 10 ORDER BY id",
            CursorScrollOption::STATIC,
            CursorConcurrency::READONLY,
            0,
            None,
            None,
        )
        .await
        .unwrap();
    assert_ne!(resp2.cursor_id, 0);
    assert_ne!(
        resp1.cursor_id, resp2.cursor_id,
        "Cursors should have different handles"
    );

    // Fetch from second cursor
    client
        .cursor_fetch(resp2.cursor_id, FetchDirection::NEXT, 0, 100, None, None)
        .await
        .unwrap();
    let rows2 = read_all_rows(&mut client).await;
    assert_eq!(rows2.len(), 10);
    assert_eq!(rows2[0][0], ColumnValues::Int(11));

    // Fetch more from first cursor — it should still work
    client
        .cursor_fetch(resp1.cursor_id, FetchDirection::NEXT, 0, 100, None, None)
        .await
        .unwrap();
    let rows1b = read_all_rows(&mut client).await;
    assert_eq!(rows1b.len(), 5, "Remaining 5 rows from first cursor");
    assert_eq!(rows1b[0][0], ColumnValues::Int(6));

    client
        .cursor_close(resp1.cursor_id, None, None)
        .await
        .unwrap();
    client
        .cursor_close(resp2.cursor_id, None, None)
        .await
        .unwrap();
    client.close_connection().await.unwrap();
}

// --- Row count negotiation ---

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cursor_row_count_negotiation() {
    let mut client = begin_connection(&build_tcp_datasource()).await;
    setup_temp_table(&mut client, 50).await;

    let resp = client
        .cursor_open(
            "SELECT id FROM #ct ORDER BY id",
            CursorScrollOption::STATIC,
            CursorConcurrency::READONLY,
            10,
            None,
            None,
        )
        .await
        .unwrap();

    assert_ne!(resp.cursor_id, 0);
    // row_count OUTPUT reflects the total result set size for STATIC cursors.
    assert!(
        resp.row_count >= 0,
        "Server should return a non-negative row_count, got {}",
        resp.row_count
    );
    assert!(
        !resp.negotiated_concurrency.is_empty(),
        "Server should return a valid concurrency, got {:?}",
        resp.negotiated_concurrency
    );

    client
        .cursor_close(resp.cursor_id, None, None)
        .await
        .unwrap();
    client.close_connection().await.unwrap();
}

// --- Failure: invalid SQL ---

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cursor_open_invalid_sql() {
    let mut client = begin_connection(&build_tcp_datasource()).await;

    let result = client
        .cursor_open(
            "SELECT * FROM this_table_does_not_exist_xyz_12345",
            CursorScrollOption::FORWARD_ONLY,
            CursorConcurrency::READONLY,
            0,
            None,
            None,
        )
        .await;

    assert!(
        result.is_err(),
        "Opening a cursor with invalid SQL should fail"
    );
    let err_msg = format!("{:?}", result.unwrap_err());
    assert!(
        err_msg.contains("Invalid object name") || err_msg.contains("208"),
        "Expected invalid object name error, got: {err_msg}"
    );

    // Connection should still be usable after the error
    client.execute("SELECT 1".to_string(), ()).await.unwrap();
    client.close_query().await.unwrap();
    client.close_connection().await.unwrap();
}

// --- Failure: double close ---

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cursor_double_close() {
    let mut client = begin_connection(&build_tcp_datasource()).await;
    setup_temp_table(&mut client, 5).await;

    let resp = client
        .cursor_open(
            "SELECT id FROM #ct ORDER BY id",
            CursorScrollOption::FORWARD_ONLY,
            CursorConcurrency::READONLY,
            0,
            None,
            None,
        )
        .await
        .unwrap();

    client
        .cursor_close(resp.cursor_id, None, None)
        .await
        .unwrap();

    // Second close on the same handle should error
    let result = client.cursor_close(resp.cursor_id, None, None).await;
    assert!(
        result.is_err(),
        "Closing an already-closed cursor should fail"
    );

    // Connection should still be usable
    client.execute("SELECT 1".to_string(), ()).await.unwrap();
    client.close_query().await.unwrap();
    client.close_connection().await.unwrap();
}

// --- Failure: invalid fetch direction on FORWARD_ONLY ---

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cursor_fetch_prev_on_forward_only() {
    let mut client = begin_connection(&build_tcp_datasource()).await;
    setup_temp_table(&mut client, 10).await;

    let resp = client
        .cursor_open(
            "SELECT id FROM #ct ORDER BY id",
            CursorScrollOption::FORWARD_ONLY,
            CursorConcurrency::READONLY,
            0,
            None,
            None,
        )
        .await
        .unwrap();

    // Fetch forward to establish position
    client
        .cursor_fetch(resp.cursor_id, FetchDirection::NEXT, 0, 3, None, None)
        .await
        .unwrap();
    let rows = read_all_rows(&mut client).await;
    assert_eq!(rows.len(), 3);

    // PREV on a FORWARD_ONLY cursor should fail
    let result = client
        .cursor_fetch(resp.cursor_id, FetchDirection::PREV, 0, 1, None, None)
        .await;
    assert!(result.is_err(), "PREV on FORWARD_ONLY cursor should fail");

    client.close_connection().await.unwrap();
}

// --- Prepared cursor lifecycle (PR 3) ---

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cursor_prepexec_and_close() {
    let mut client = begin_connection(&build_tcp_datasource()).await;
    setup_temp_table(&mut client, 50).await;

    let resp = client
        .cursor_prepexec(
            "SELECT id, name, value FROM #ct ORDER BY id",
            vec![],
            CursorScrollOption::FORWARD_ONLY,
            CursorConcurrency::READONLY,
            0,
            None,
            None,
        )
        .await
        .unwrap();

    assert_ne!(
        resp.prepared_handle, 0,
        "Server should assign a prepared handle"
    );
    assert_ne!(
        resp.cursor.cursor_id, 0,
        "Server should assign a cursor handle"
    );
    assert!(
        !resp.cursor.negotiated_concurrency.is_empty(),
        "Server should return a valid concurrency, got {:?}",
        resp.cursor.negotiated_concurrency
    );
    assert!(
        !resp.cursor.negotiated_scroll.is_empty(),
        "Server should return a valid scroll option, got {:?}",
        resp.cursor.negotiated_scroll
    );

    client
        .cursor_fetch(
            resp.cursor.cursor_id,
            FetchDirection::NEXT,
            0,
            10,
            None,
            None,
        )
        .await
        .unwrap();
    let rows = read_all_rows(&mut client).await;
    assert_eq!(rows.len(), 10, "Expected 10 rows from first fetch");
    assert_eq!(rows[0][0], ColumnValues::Int(1));

    client
        .cursor_close(resp.cursor.cursor_id, None, None)
        .await
        .unwrap();
    client
        .cursor_unprepare(resp.prepared_handle, None, None)
        .await
        .unwrap();
    client.close_connection().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cursor_prepexec_then_reexecute() {
    let mut client = begin_connection(&build_tcp_datasource()).await;
    setup_temp_table(&mut client, 30).await;

    // prepexec opens the first cursor and returns a reusable prepared handle.
    let prep = client
        .cursor_prepexec(
            "SELECT id FROM #ct ORDER BY id",
            vec![],
            CursorScrollOption::FORWARD_ONLY,
            CursorConcurrency::READONLY,
            0,
            None,
            None,
        )
        .await
        .unwrap();
    let cursor_a = prep.cursor.cursor_id;

    // Re-execute the same prepared handle -> opens a SECOND, distinct cursor
    // while the first is still open.
    let exec = client
        .cursor_execute(
            prep.prepared_handle,
            vec![],
            CursorScrollOption::FORWARD_ONLY,
            CursorConcurrency::READONLY,
            0,
            None,
            None,
        )
        .await
        .unwrap();
    let cursor_b = exec.cursor_id;

    assert_ne!(cursor_b, 0);
    assert_ne!(
        cursor_a, cursor_b,
        "Each execution should open a distinct cursor handle"
    );

    client
        .cursor_fetch(cursor_a, FetchDirection::NEXT, 0, 100, None, None)
        .await
        .unwrap();
    let rows_a = read_all_rows(&mut client).await;
    assert_eq!(rows_a.len(), 30, "First cursor should see all 30 rows");

    client
        .cursor_fetch(cursor_b, FetchDirection::NEXT, 0, 100, None, None)
        .await
        .unwrap();
    let rows_b = read_all_rows(&mut client).await;
    assert_eq!(
        rows_b.len(),
        30,
        "Re-executed cursor should see all 30 rows"
    );

    client.cursor_close(cursor_a, None, None).await.unwrap();
    client.cursor_close(cursor_b, None, None).await.unwrap();
    client
        .cursor_unprepare(prep.prepared_handle, None, None)
        .await
        .unwrap();
    client.close_connection().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cursor_prepare_execute_unprepare() {
    let mut client = begin_connection(&build_tcp_datasource()).await;
    setup_temp_table(&mut client, 25).await;

    // Prepare only -- no cursor opened yet, no rows returned.
    let prep = client
        .cursor_prepare(
            "SELECT id, value FROM #ct ORDER BY id",
            "",
            CursorScrollOption::FORWARD_ONLY,
            CursorConcurrency::READONLY,
            None,
            None,
        )
        .await
        .unwrap();
    assert_ne!(
        prep.prepared_handle, 0,
        "Server should assign a prepared handle"
    );
    assert!(
        !prep.negotiated_concurrency.is_empty(),
        "Server should return a valid concurrency, got {:?}",
        prep.negotiated_concurrency
    );
    assert!(
        !prep.negotiated_scroll.is_empty(),
        "Server should return a valid scroll option, got {:?}",
        prep.negotiated_scroll
    );

    // Execute the prepared handle to open a cursor.
    let exec = client
        .cursor_execute(
            prep.prepared_handle,
            vec![],
            CursorScrollOption::FORWARD_ONLY,
            CursorConcurrency::READONLY,
            0,
            None,
            None,
        )
        .await
        .unwrap();
    assert_ne!(exec.cursor_id, 0, "Execute should open a cursor");

    client
        .cursor_fetch(exec.cursor_id, FetchDirection::NEXT, 0, 100, None, None)
        .await
        .unwrap();
    let rows = read_all_rows(&mut client).await;
    assert_eq!(rows.len(), 25, "Expected all 25 rows");

    client
        .cursor_close(exec.cursor_id, None, None)
        .await
        .unwrap();
    client
        .cursor_unprepare(prep.prepared_handle, None, None)
        .await
        .unwrap();
    client.close_connection().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cursor_prepexec_with_params() {
    use mssql_tds::datatypes::sqltypes::SqlType;
    use mssql_tds::message::parameters::rpc_parameters::{RpcParameter, StatusFlags};

    let mut client = begin_connection(&build_tcp_datasource()).await;
    setup_temp_table(&mut client, 100).await;

    let resp = client
        .cursor_prepexec(
            "SELECT id, name FROM #ct WHERE id >= @min_id AND id <= @max_id ORDER BY id",
            vec![
                RpcParameter::new(
                    Some("@min_id".to_string()),
                    StatusFlags::NONE,
                    SqlType::Int(Some(20)),
                ),
                RpcParameter::new(
                    Some("@max_id".to_string()),
                    StatusFlags::NONE,
                    SqlType::Int(Some(24)),
                ),
            ],
            CursorScrollOption::STATIC,
            CursorConcurrency::READONLY,
            0,
            None,
            None,
        )
        .await
        .unwrap();

    assert_ne!(resp.prepared_handle, 0);
    assert_ne!(resp.cursor.cursor_id, 0);
    // STATIC cursors build a snapshot, so the server reports a known row count.
    assert!(
        resp.cursor.row_count >= 0,
        "STATIC cursor should report a non-negative row_count, got {}",
        resp.cursor.row_count
    );

    client
        .cursor_fetch(
            resp.cursor.cursor_id,
            FetchDirection::NEXT,
            0,
            100,
            None,
            None,
        )
        .await
        .unwrap();
    let rows = read_all_rows(&mut client).await;
    assert_eq!(rows.len(), 5, "Expected rows 20..=24");
    assert_eq!(rows[0][0], ColumnValues::Int(20));
    assert_eq!(rows[4][0], ColumnValues::Int(24));

    client
        .cursor_close(resp.cursor.cursor_id, None, None)
        .await
        .unwrap();
    client
        .cursor_unprepare(resp.prepared_handle, None, None)
        .await
        .unwrap();
    client.close_connection().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cursor_prepare_execute_with_params() {
    use mssql_tds::datatypes::sqltypes::SqlType;
    use mssql_tds::message::parameters::rpc_parameters::{RpcParameter, StatusFlags};

    let mut client = begin_connection(&build_tcp_datasource()).await;
    setup_temp_table(&mut client, 100).await;

    // Prepare with an explicit parameter declaration; types are fixed here.
    let prep = client
        .cursor_prepare(
            "SELECT id FROM #ct WHERE id = @target",
            "@target INT",
            CursorScrollOption::FORWARD_ONLY,
            CursorConcurrency::READONLY,
            None,
            None,
        )
        .await
        .unwrap();
    assert_ne!(prep.prepared_handle, 0);

    // Execute with @target = 42.
    let exec = client
        .cursor_execute(
            prep.prepared_handle,
            vec![RpcParameter::new(
                Some("@target".to_string()),
                StatusFlags::NONE,
                SqlType::Int(Some(42)),
            )],
            CursorScrollOption::FORWARD_ONLY,
            CursorConcurrency::READONLY,
            0,
            None,
            None,
        )
        .await
        .unwrap();
    client
        .cursor_fetch(exec.cursor_id, FetchDirection::NEXT, 0, 100, None, None)
        .await
        .unwrap();
    let rows = read_all_rows(&mut client).await;
    assert_eq!(rows.len(), 1, "Expected exactly the @target=42 row");
    assert_eq!(rows[0][0], ColumnValues::Int(42));
    client
        .cursor_close(exec.cursor_id, None, None)
        .await
        .unwrap();

    // Re-execute the same prepared handle with a different value @target = 7.
    let exec2 = client
        .cursor_execute(
            prep.prepared_handle,
            vec![RpcParameter::new(
                Some("@target".to_string()),
                StatusFlags::NONE,
                SqlType::Int(Some(7)),
            )],
            CursorScrollOption::FORWARD_ONLY,
            CursorConcurrency::READONLY,
            0,
            None,
            None,
        )
        .await
        .unwrap();
    client
        .cursor_fetch(exec2.cursor_id, FetchDirection::NEXT, 0, 100, None, None)
        .await
        .unwrap();
    let rows2 = read_all_rows(&mut client).await;
    assert_eq!(rows2.len(), 1, "Expected exactly the @target=7 row");
    assert_eq!(rows2[0][0], ColumnValues::Int(7));
    client
        .cursor_close(exec2.cursor_id, None, None)
        .await
        .unwrap();

    client
        .cursor_unprepare(prep.prepared_handle, None, None)
        .await
        .unwrap();
    client.close_connection().await.unwrap();
}

// --- Failure scenarios (PR 3) ---

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cursor_prepexec_rejects_auto_fetch() {
    let mut client = begin_connection(&build_tcp_datasource()).await;

    let result = client
        .cursor_prepexec(
            "SELECT 1",
            vec![],
            CursorScrollOption::FORWARD_ONLY | CursorScrollOption::AUTO_FETCH,
            CursorConcurrency::READONLY,
            0,
            None,
            None,
        )
        .await;

    assert!(result.is_err(), "AUTO_FETCH should be rejected");
    let err_msg = format!("{:?}", result.unwrap_err());
    assert!(
        err_msg.contains("AUTO_FETCH"),
        "Expected AUTO_FETCH usage error, got: {err_msg}"
    );

    client.close_connection().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cursor_execute_rejects_auto_fetch() {
    let mut client = begin_connection(&build_tcp_datasource()).await;

    let result = client
        .cursor_execute(
            1,
            vec![],
            CursorScrollOption::FORWARD_ONLY | CursorScrollOption::AUTO_FETCH,
            CursorConcurrency::READONLY,
            0,
            None,
            None,
        )
        .await;

    assert!(result.is_err(), "AUTO_FETCH should be rejected");
    let err_msg = format!("{:?}", result.unwrap_err());
    assert!(
        err_msg.contains("AUTO_FETCH"),
        "Expected AUTO_FETCH usage error, got: {err_msg}"
    );

    client.close_connection().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cursor_prepexec_invalid_sql() {
    let mut client = begin_connection(&build_tcp_datasource()).await;

    let result = client
        .cursor_prepexec(
            "SELECT * FROM this_table_does_not_exist_xyz_67890",
            vec![],
            CursorScrollOption::FORWARD_ONLY,
            CursorConcurrency::READONLY,
            0,
            None,
            None,
        )
        .await;

    assert!(
        result.is_err(),
        "Preparing a cursor with invalid SQL should fail"
    );
    let err_msg = format!("{:?}", result.unwrap_err());
    assert!(
        err_msg.contains("Invalid object name") || err_msg.contains("208"),
        "Expected invalid object name error, got: {err_msg}"
    );

    // Connection should still be usable after the error.
    client.execute("SELECT 1".to_string(), ()).await.unwrap();
    client.close_query().await.unwrap();
    client.close_connection().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cursor_unprepare_invalid_handle() {
    let mut client = begin_connection(&build_tcp_datasource()).await;

    // Unpreparing a handle that was never created should not panic. The server
    // may return an error or drain silently -- either outcome is acceptable; the
    // critical thing is no panic and a still-usable connection afterwards.
    let _ = client.cursor_unprepare(999999, None, None).await;

    client.execute("SELECT 1".to_string(), ()).await.unwrap();
    client.close_query().await.unwrap();
    client.close_connection().await.unwrap();
}

// --- Positioned operations (sp_cursor) and cursor options (sp_cursoroption) ---

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cursor_positioned_update() {
    let mut client = begin_connection(&build_tcp_datasource()).await;
    setup_temp_table(&mut client, 5).await;

    // Updatable cursor: keyset-driven with optimistic concurrency.
    let resp = client
        .cursor_open(
            "SELECT id, name, value FROM #ct ORDER BY id",
            CursorScrollOption::KEYSET_DRIVEN,
            CursorConcurrency::OPTCC,
            0,
            None,
            None,
        )
        .await
        .unwrap();
    let cursor_id = resp.cursor_id;
    assert_ne!(cursor_id, 0);

    // Fetch the first row into the fetch buffer, then drain the rowset so the
    // connection is free for the positioned operation.
    client
        .cursor_fetch(cursor_id, FetchDirection::NEXT, 0, 1, None, None)
        .await
        .unwrap();
    let rows = read_all_rows(&mut client).await;
    assert_eq!(rows.len(), 1);
    let first_id = match rows[0][0] {
        ColumnValues::Int(v) => v,
        ref other => panic!("expected INT id, got {other:?}"),
    };

    // Positioned UPDATE of row 1 in the fetch buffer: value = 999. The column
    // value is supplied as a named @value parameter.
    client
        .perform_cursor_operation(
            cursor_id,
            CursorOperation::UPDATE,
            1,
            "",
            vec![RpcParameter::new(
                Some("@value".to_string()),
                StatusFlags::NONE,
                SqlType::Int(Some(999)),
            )],
            None,
            None,
        )
        .await
        .unwrap();

    client.cursor_close(cursor_id, None, None).await.unwrap();

    // Verify the row was modified.
    client
        .execute(format!("SELECT value FROM #ct WHERE id = {first_id}"), ())
        .await
        .unwrap();
    let verify = read_all_rows(&mut client).await;
    assert_eq!(verify.len(), 1);
    assert_eq!(verify[0][0], ColumnValues::Int(999));

    client.close_connection().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cursor_positioned_delete() {
    let mut client = begin_connection(&build_tcp_datasource()).await;
    setup_temp_table(&mut client, 5).await;

    let resp = client
        .cursor_open(
            "SELECT id, name, value FROM #ct ORDER BY id",
            CursorScrollOption::KEYSET_DRIVEN,
            CursorConcurrency::OPTCC,
            0,
            None,
            None,
        )
        .await
        .unwrap();
    let cursor_id = resp.cursor_id;

    client
        .cursor_fetch(cursor_id, FetchDirection::NEXT, 0, 1, None, None)
        .await
        .unwrap();
    let rows = read_all_rows(&mut client).await;
    assert_eq!(rows.len(), 1);
    let first_id = match rows[0][0] {
        ColumnValues::Int(v) => v,
        ref other => panic!("expected INT id, got {other:?}"),
    };

    // Positioned DELETE of row 1 in the fetch buffer (no value params).
    client
        .perform_cursor_operation(
            cursor_id,
            CursorOperation::DELETE,
            1,
            "",
            vec![],
            None,
            None,
        )
        .await
        .unwrap();

    client.cursor_close(cursor_id, None, None).await.unwrap();

    // Verify the row is gone.
    client
        .execute(
            format!("SELECT COUNT(*) FROM #ct WHERE id = {first_id}"),
            (),
        )
        .await
        .unwrap();
    let verify = read_all_rows(&mut client).await;
    assert_eq!(verify.len(), 1);
    assert_eq!(verify[0][0], ColumnValues::Int(0));

    client.close_connection().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cursor_option_set_name() {
    let mut client = begin_connection(&build_tcp_datasource()).await;
    setup_temp_table(&mut client, 3).await;

    let resp = client
        .cursor_open(
            "SELECT id, name, value FROM #ct ORDER BY id",
            CursorScrollOption::KEYSET_DRIVEN,
            CursorConcurrency::OPTCC,
            0,
            None,
            None,
        )
        .await
        .unwrap();
    let cursor_id = resp.cursor_id;

    // Assign a Transact-SQL cursor name (used by WHERE CURRENT OF). The string
    // value matches CursorOptionCode::CursorName's expectation.
    client
        .set_cursor_option(
            cursor_id,
            CursorOptionCode::CursorName,
            CursorOptionValue::String("my_named_cursor".to_string()),
            None,
            None,
        )
        .await
        .unwrap();

    client.cursor_close(cursor_id, None, None).await.unwrap();
    client.close_connection().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cursor_option_rejects_type_mismatch() {
    let mut client = begin_connection(&build_tcp_datasource()).await;

    // CursorName expects a string; passing an Int must fail fast with a usage
    // error before any server round-trip.
    let result = client
        .set_cursor_option(
            1,
            CursorOptionCode::CursorName,
            CursorOptionValue::Int(7),
            None,
            None,
        )
        .await;
    assert!(result.is_err(), "type mismatch should be rejected");
    let err_msg = format!("{}", result.unwrap_err());
    assert!(
        err_msg.contains("expects a string value"),
        "error should mention expected type: {err_msg}"
    );

    client.close_connection().await.unwrap();
}

// --- Status surfacing (Gap 2): CursorStatus on open-family responses ---

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cursor_open_reports_succeeded_status() {
    let mut client = begin_connection(&build_tcp_datasource()).await;
    setup_temp_table(&mut client, 5).await;

    let resp = client
        .cursor_open(
            "SELECT id FROM #ct ORDER BY id",
            CursorScrollOption::KEYSET_DRIVEN,
            CursorConcurrency::READONLY,
            0,
            None,
            None,
        )
        .await
        .unwrap();
    // A normal open is neither auto-closed nor asynchronously populated.
    assert_eq!(resp.status, CursorStatus::Succeeded);

    client
        .cursor_close(resp.cursor_id, None, None)
        .await
        .unwrap();
    client.close_connection().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cursor_prepexec_reports_succeeded_status() {
    let mut client = begin_connection(&build_tcp_datasource()).await;
    setup_temp_table(&mut client, 3).await;

    let resp = client
        .cursor_prepexec(
            "SELECT id FROM #ct ORDER BY id",
            vec![],
            CursorScrollOption::FORWARD_ONLY,
            CursorConcurrency::READONLY,
            0,
            None,
            None,
        )
        .await
        .unwrap();
    assert_eq!(resp.cursor.status, CursorStatus::Succeeded);

    client
        .cursor_close(resp.cursor.cursor_id, None, None)
        .await
        .unwrap();
    client
        .cursor_unprepare(resp.prepared_handle, None, None)
        .await
        .unwrap();
    client.close_connection().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cursor_prepare_reports_succeeded_status() {
    let mut client = begin_connection(&build_tcp_datasource()).await;
    setup_temp_table(&mut client, 3).await;

    let resp = client
        .cursor_prepare(
            "SELECT id FROM #ct ORDER BY id",
            "",
            CursorScrollOption::FORWARD_ONLY,
            CursorConcurrency::READONLY,
            None,
            None,
        )
        .await
        .unwrap();
    // Prepare opens no cursor, so the status is always success.
    assert_eq!(resp.status, CursorStatus::Succeeded);

    client
        .cursor_unprepare(resp.prepared_handle, None, None)
        .await
        .unwrap();
    client.close_connection().await.unwrap();
}

// --- Per-row rowstat / FetchStatus (Gap 3) ---

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cursor_rowstat_succeeded() {
    let mut client = begin_connection(&build_tcp_datasource()).await;
    setup_temp_table(&mut client, 5).await;

    let resp = client
        .cursor_open(
            "SELECT id, name, value FROM #ct ORDER BY id",
            CursorScrollOption::KEYSET_DRIVEN,
            CursorConcurrency::READONLY,
            0,
            None,
            None,
        )
        .await
        .unwrap();
    let cursor_id = resp.cursor_id;

    client
        .cursor_fetch(cursor_id, FetchDirection::NEXT, 0, 5, None, None)
        .await
        .unwrap();

    // next_cursor_row splits the hidden trailing rowstat column off each row.
    let mut ids = Vec::new();
    while let Some((row, status)) = client.next_cursor_row().await.unwrap() {
        // Unchanged keyset rows fetch successfully.
        assert!(
            status.contains(FetchStatus::SUCCEEDED),
            "expected SUCCEEDED rowstat, got {status:?}"
        );
        // Only id, name, value remain after the rowstat is stripped.
        assert_eq!(
            row.len(),
            3,
            "rowstat column should be stripped from the row"
        );
        if let ColumnValues::Int(id) = row[0] {
            ids.push(id);
        }
    }
    client.close_query().await.unwrap();
    assert_eq!(ids, vec![1, 2, 3, 4, 5]);

    client.cursor_close(cursor_id, None, None).await.unwrap();
    client.close_connection().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn next_cursor_row_rejects_non_cursor_result() {
    let mut client = begin_connection(&build_tcp_datasource()).await;
    setup_temp_table(&mut client, 2).await;

    // An ordinary query result has no trailing rowstat column, so next_cursor_row
    // must refuse to strip a real data column.
    client
        .execute("SELECT id FROM #ct ORDER BY id".to_string(), ())
        .await
        .unwrap();
    let result = client.next_cursor_row().await;
    assert!(
        result.is_err(),
        "next_cursor_row on a normal result set must error"
    );
    let err_msg = format!("{}", result.unwrap_err());
    assert!(
        err_msg.contains("rowstat"),
        "error should mention rowstat: {err_msg}"
    );
    client.close_query().await.unwrap();
    client.close_connection().await.unwrap();
}
