// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Integration tests for the reactor-free sync fetch edge, driven end-to-end
//! through the real ODBC entry points (`SQLExecDirectW` / `SQLFetch` /
//! `SQLMoreResults`) against a live `mssql-mock-tds` peer over TCP.
//!
//! The mock speaks TDS over a raw `TcpStream`, so the connection is
//! **sync-eligible**: `finish_execute` flips it to `TdsSyncClient` and `SQLFetch`
//! serves rows off the blocking socket with no tokio reactor. Every sync fetch
//! is checked byte-identical against an all-async oracle run over the same mock
//! result set — the differential parity the whole rewire rests on.
//!
//! The scripted-token unit tests elsewhere in the crate can only exercise the
//! **async fallback** arm (their transport reports `NotEligible`); these tests
//! are the crate's only coverage of the live sync arm, so they live in-crate
//! (the driver is a `cdylib`; an external test crate could neither link it nor
//! reach the `pub(crate)` entry points and handle state they drive).

use std::sync::mpsc;
use std::thread::JoinHandle;

use mssql_mock_tds::query_response::{
    ColumnDefinition, ColumnValue, MidStreamError, Row, SqlDataType,
};
use mssql_mock_tds::{MockTdsServer, QueryResponse};
use mssql_tds::connection::client_context::ClientContext;
use mssql_tds::connection::tds_client::{ResultSet, StatementResult, TdsClient};
use mssql_tds::connection_provider::tds_connection_provider::TdsConnectionProvider;
use mssql_tds::core::{EncryptionOptions, EncryptionSetting};
use mssql_tds::datatypes::column_values::ColumnValues;
use tokio::sync::oneshot;

use crate::api::exec_direct::sql_exec_direct_w;
use crate::api::fetch::sql_fetch;
use crate::api::more_results::sql_more_results;
use crate::api::odbc_types::{
    SQL_ERROR, SQL_NO_DATA, SQL_NTS, SQL_SUCCESS, SQL_SUCCESS_WITH_INFO, SqlHandle, SqlReturn,
};
use crate::handles::dbc::{DbcClient, DbcHandle};
use crate::handles::{StmtHandle, handle_from_raw};
use crate::test_support::TestHandles;

/// The mock's native error code for the injected mid-stream failure.
const MOCK_ERROR_NUMBER: i32 = 50_000;

/// A mock server bound on its own thread + runtime; shut down on drop. The
/// dedicated thread means the sync client's blocking reads on the test thread
/// never starve the server — the same decoupling a real remote peer provides.
struct TestServer {
    addr: std::net::SocketAddr,
    shutdown: Option<oneshot::Sender<()>>,
    thread: Option<JoinHandle<()>>,
}

impl Drop for TestServer {
    fn drop(&mut self) {
        if let Some(tx) = self.shutdown.take() {
            let _ = tx.send(());
        }
        if let Some(handle) = self.thread.take() {
            let _ = handle.join();
        }
    }
}

fn start_server(query: &str, response: QueryResponse) -> TestServer {
    let query = query.to_string();
    let (addr_tx, addr_rx) = mpsc::channel();
    let (shutdown_tx, shutdown_rx) = oneshot::channel();
    let thread = std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("server runtime builds");
        rt.block_on(async move {
            let server = MockTdsServer::new("127.0.0.1:0")
                .await
                .expect("mock server binds");
            let addr = server.local_addr();
            {
                let registry = server.query_registry();
                registry.lock().await.register(query, response);
            }
            addr_tx.send(addr).expect("addr channel open");
            let _ = server.run_with_shutdown(shutdown_rx).await;
        });
    });
    let addr = addr_rx.recv().expect("server reports its address");
    TestServer {
        addr,
        shutdown: Some(shutdown_tx),
        thread: Some(thread),
    }
}

/// Connects a real async client to the mock over plaintext raw TCP (encryption
/// off), so the resulting connection is sync-eligible.
async fn connect(addr: std::net::SocketAddr) -> TdsClient {
    let datasource = format!("tcp:{},{}", addr.ip(), addr.port());
    let mut context = ClientContext::default();
    context.user_name = "sa".to_string();
    context.password = "test-password".to_string();
    context.database = "master".to_string();
    context.encryption_options = EncryptionOptions {
        mode: EncryptionSetting::PreferOff,
        trust_server_certificate: true,
        host_name_in_cert: None,
        server_certificate: None,
    };
    TdsConnectionProvider {}
        .create_client(context, &datasource, None)
        .await
        .expect("client connects to mock server")
}

/// A `(id INT, label NVARCHAR)` result set. Varying string lengths make row
/// byte-sizes non-uniform, matching the L4 sync harness.
fn make_response(row_count: usize) -> QueryResponse {
    let columns = vec![
        ColumnDefinition::new("id", SqlDataType::Int),
        ColumnDefinition::new("label", SqlDataType::NVarChar),
    ];
    let rows = (0..row_count)
        .map(|i| {
            Row::new(vec![
                ColumnValue::Int(i as i32),
                ColumnValue::NVarChar(format!("row-{i}-{}", "x".repeat(i % 7))),
            ])
        })
        .collect();
    QueryResponse::new(columns, rows)
}

/// Same shape, but the server emits an ERROR token after `after_rows` rows then
/// a terminal DONE — exercising the fetch-time error/drain path.
fn make_error_response(row_count: usize, after_rows: usize) -> QueryResponse {
    make_response(row_count).with_error_after(MidStreamError {
        after_rows,
        number: MOCK_ERROR_NUMBER as u32,
        state: 1,
        severity: 16,
        message: "mid-stream boom".to_string(),
        drain_info: Vec::new(),
    })
}

/// Connects to `addr` on the DBC's runtime and stores the client as the active
/// async connection, leaving the connection idle (claimable by execute).
fn attach_client(h: &TestHandles, addr: std::net::SocketAddr) {
    h.mark_dbc_connected();
    let dbc = unsafe { handle_from_raw::<DbcHandle>(h.dbc) };
    let client = dbc.runtime.block_on(connect(addr));
    let mut ds = dbc.inner.lock().unwrap();
    ds.client = Some(DbcClient::Async(client));
    ds.active_stmt = None;
}

/// Runs `SQLExecDirectW` with `query` (UTF-16, NUL-terminated) and returns its
/// return code.
fn exec_direct(stmt_handle: SqlHandle, query: &str) -> SqlReturn {
    let sql: Vec<u16> = query.encode_utf16().chain(std::iter::once(0)).collect();
    unsafe { sql_exec_direct_w(stmt_handle, sql.as_ptr(), SQL_NTS) }
}

/// True when the DBC currently holds the reactor-free sync fetch client.
fn dbc_is_sync(h: &TestHandles) -> bool {
    let dbc = unsafe { handle_from_raw::<DbcHandle>(h.dbc) };
    let ds = dbc.inner.lock().unwrap();
    matches!(ds.client, Some(DbcClient::Sync(_)))
}

/// Drains the current result set via `SQLFetch`, cloning each `current_row`.
/// Asserts every fetch is a clean success until `SQL_NO_DATA`.
fn fetch_all_rows(stmt_handle: SqlHandle, stmt: &StmtHandle) -> Vec<Vec<ColumnValues>> {
    let mut rows = Vec::new();
    loop {
        let rc = unsafe { sql_fetch(stmt_handle) };
        if rc == SQL_NO_DATA {
            break;
        }
        assert!(
            rc == SQL_SUCCESS || rc == SQL_SUCCESS_WITH_INFO,
            "unexpected SQLFetch return: {rc}"
        );
        let row = stmt
            .inner
            .lock()
            .unwrap()
            .current_row
            .clone()
            .expect("current_row set on success");
        rows.push(row);
    }
    rows
}

/// The all-async oracle for a single-set query: every row via `next_row().await`.
async fn async_oracle(addr: std::net::SocketAddr, query: &str) -> Vec<Vec<ColumnValues>> {
    let mut client = connect(addr).await;
    client
        .execute(query.to_string(), ())
        .await
        .expect("oracle executes");
    let mut rows = Vec::new();
    if client.on_rows() {
        while let Some(row) = client.next_row().await.expect("oracle next_row") {
            rows.push(row);
        }
    }
    client.close_query().await.expect("oracle closes query");
    rows
}

/// The all-async oracle for a multi-result-set batch: collects each set's rows,
/// walking `advance()` across the boundaries.
async fn async_oracle_sets(addr: std::net::SocketAddr, query: &str) -> Vec<Vec<Vec<ColumnValues>>> {
    let mut client = connect(addr).await;
    let mut result = client
        .execute(query.to_string(), ())
        .await
        .expect("oracle executes");
    let mut sets = Vec::new();
    loop {
        match result {
            StatementResult::Rows => {
                let mut rows = Vec::new();
                while let Some(row) = client.next_row().await.expect("oracle next_row") {
                    rows.push(row);
                }
                sets.push(rows);
            }
            StatementResult::NoRows { .. } => sets.push(Vec::new()),
            StatementResult::End => break,
        }
        result = client.advance().await.expect("oracle advance");
    }
    client.close_query().await.expect("oracle closes query");
    sets
}

/// The all-async error oracle: fetch rows until the mid-stream ERROR, returning
/// the rows read and the surfaced error's native code.
async fn async_oracle_until_error(
    addr: std::net::SocketAddr,
    query: &str,
) -> (Vec<Vec<ColumnValues>>, String) {
    let mut client = connect(addr).await;
    client
        .execute(query.to_string(), ())
        .await
        .expect("oracle executes");
    let mut rows = Vec::new();
    assert!(client.on_rows());
    let err = loop {
        match client.next_row().await {
            Ok(Some(row)) => rows.push(row),
            Ok(None) => panic!("expected a mid-stream error, got a clean end"),
            Err(e) => break format!("{e:?}"),
        }
    };
    (rows, err)
}

/// (a) Differential: a full sync `SQLFetch` drain at the default `max_rows == 1`
/// reproduces the async oracle byte-identically, and the DBC actually flipped to
/// the sync edge at execute time.
#[test]
fn sync_fetch_matches_async_oracle_at_max_rows_1() {
    const QUERY: &str = "SELECT SYNC ROWS";
    let server = start_server(QUERY, make_response(100));

    let h = TestHandles::with_env_dbc_stmt();
    let dbc = unsafe { handle_from_raw::<DbcHandle>(h.dbc) };
    let expected = dbc.runtime.block_on(async_oracle(server.addr, QUERY));
    assert!(!expected.is_empty());

    attach_client(&h, server.addr);
    let rc = exec_direct(h.stmt, QUERY);
    assert_eq!(rc, SQL_SUCCESS, "SQLExecDirectW should position on rows");
    assert!(
        dbc_is_sync(&h),
        "raw-TCP connection must flip to the sync fetch edge at execute time"
    );

    let stmt = unsafe { handle_from_raw::<StmtHandle>(h.stmt) };
    let actual = fetch_all_rows(h.stmt, stmt);
    assert_eq!(actual, expected);
}

/// (b) At `max_rows == 64` the buffered sync fetch reproduces the oracle's
/// row-set AND surfaces the mid-stream error after the last good row — the rows
/// and the error must not shift even though INFO timing may.
#[test]
fn sync_fetch_batched_preserves_rows_and_error() {
    const QUERY: &str = "SELECT SYNC ERR ROWS";
    // 50 rows, error after 37 — the error lands mid-batch at max_rows == 64, so
    // `fetch_rows_batch` returns the 37 rows in `out` alongside the error.
    let server = start_server(QUERY, make_error_response(50, 37));

    let h = TestHandles::with_env_dbc_stmt();
    let dbc = unsafe { handle_from_raw::<DbcHandle>(h.dbc) };
    let (expected_rows, oracle_err) = dbc
        .runtime
        .block_on(async_oracle_until_error(server.addr, QUERY));
    assert_eq!(expected_rows.len(), 37);
    assert!(oracle_err.contains("mid-stream boom") || oracle_err.contains("50000"));

    attach_client(&h, server.addr);
    let rc = exec_direct(h.stmt, QUERY);
    assert_eq!(rc, SQL_SUCCESS);
    assert!(dbc_is_sync(&h));

    let stmt = unsafe { handle_from_raw::<StmtHandle>(h.stmt) };
    // Batch-ready: raise the per-statement prefetch so rows and error arrive in
    // one refill.
    stmt.inner.lock().unwrap().max_rows = 64;

    let mut actual = Vec::new();
    let final_rc = loop {
        let rc = unsafe { sql_fetch(h.stmt) };
        if rc == SQL_ERROR || rc == SQL_NO_DATA {
            break rc;
        }
        assert!(rc == SQL_SUCCESS || rc == SQL_SUCCESS_WITH_INFO);
        let row = stmt.inner.lock().unwrap().current_row.clone().unwrap();
        actual.push(row);
    };

    assert_eq!(final_rc, SQL_ERROR, "the mid-stream error must surface");
    assert_eq!(
        actual, expected_rows,
        "rows before the error must be preserved"
    );
    let ss = stmt.inner.lock().unwrap();
    assert!(
        ss.diag_records
            .iter()
            .any(|d| d.native_error == MOCK_ERROR_NUMBER),
        "the server error must be surfaced as a diagnostic"
    );
}

/// (c) `SQLMoreResults` interleave: sync-fetch the first set, cross the boundary
/// (revert to async, `advance()`, re-flip to sync), sync-fetch the second set.
/// Both sets must match the async oracle.
#[test]
fn sync_fetch_interleaves_across_more_results() {
    const QUERY: &str = "SELECT SYNC TWO SETS";
    let response = make_response(40).with_additional_result_set(make_response(25));
    let server = start_server(QUERY, response);

    let h = TestHandles::with_env_dbc_stmt();
    let dbc = unsafe { handle_from_raw::<DbcHandle>(h.dbc) };
    let expected = dbc.runtime.block_on(async_oracle_sets(server.addr, QUERY));
    assert_eq!(expected.len(), 2);
    assert_eq!(expected[0].len(), 40);
    assert_eq!(expected[1].len(), 25);

    attach_client(&h, server.addr);
    let rc = exec_direct(h.stmt, QUERY);
    assert_eq!(rc, SQL_SUCCESS);
    assert!(dbc_is_sync(&h), "first set flips to sync");

    let stmt = unsafe { handle_from_raw::<StmtHandle>(h.stmt) };
    let first = fetch_all_rows(h.stmt, stmt);
    assert_eq!(first, expected[0]);

    // Cross the result-set boundary: revert to async, advance, re-flip to sync.
    let more = unsafe { sql_more_results(h.stmt) };
    assert_eq!(
        more, SQL_SUCCESS,
        "should advance onto the second result set"
    );
    assert!(
        dbc_is_sync(&h),
        "second row-returning set must re-flip to sync"
    );

    let second = fetch_all_rows(h.stmt, stmt);
    assert_eq!(second, expected[1]);

    // The batch is exhausted: one more advance reports no further results.
    let done = unsafe { sql_more_results(h.stmt) };
    assert_eq!(done, SQL_NO_DATA);
}

/// (d) No-regression: an ineligible (scripted-transport) client hitting the flip
/// seam is returned to the async edge unchanged — `flip_to_fetch_edge` takes the
/// `NotEligible` arm and leaves the DBC holding `DbcClient::Async`, so `SQLFetch`
/// falls back to `block_on`. The whole scripted-token suite only ever exercises
/// this arm; this pins the flip seam's fallback explicitly.
#[test]
fn ineligible_transport_stays_on_async_edge() {
    use crate::api::exec_common::flip_to_fetch_edge;
    use mssql_tds::test_client_support::{
        col_metadata_empty, done_no_more, tds_client_from_tokens,
    };

    let h = TestHandles::with_env_dbc_stmt();
    h.mark_dbc_connected();
    let dbc = unsafe { handle_from_raw::<DbcHandle>(h.dbc) };
    let stmt = unsafe { handle_from_raw::<StmtHandle>(h.stmt) };

    // The scripted transport reports `NotEligible` from `into_sync`.
    let client = tds_client_from_tokens(vec![col_metadata_empty(), done_no_more()]);
    let flip = flip_to_fetch_edge(dbc, stmt, h.stmt, client);
    assert!(
        flip.is_ok(),
        "an ineligible transport must not fail the flip"
    );

    let ds = dbc.inner.lock().unwrap();
    assert!(
        matches!(ds.client, Some(DbcClient::Async(_))),
        "an ineligible transport must remain on the async edge"
    );
}
