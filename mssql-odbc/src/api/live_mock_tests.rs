// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Live reproduction tests for AB#47530: `mssql-python`'s `fetchone` /
//! `fetchmany` / `fetchall` reported zero rows for a genuinely non-empty
//! result set.
//!
//! `crate::test_support`'s scripted `TdsClient` (`mssql_tds::test_client_support`)
//! never carries real row bytes on the wire — only `ColMetadata`/`Done`
//! control tokens — so it cannot exercise the fill loop's actual
//! `next_row_cursor` / `read_row_column` path against real `ROW` tokens. These
//! tests instead run the driver against `mssql-mock-tds`, a real TCP-level TDS
//! server, so the whole `SQLExecDirect` -> `SQLFetch` / `SQLBindCol` +
//! `SQLFetchScroll` path is driven end to end exactly as `mssql-python`'s
//! `ddbc_bindings.cpp` drives it: one statement handle reused across several
//! executions, with `SQLFreeStmt(SQL_CLOSE)` between them (mirroring
//! `Cursor._soft_reset_cursor`) and rows deliberately left undrained, matching
//! `test_004_cursor.py`'s module-scoped `cursor` fixture.

use std::net::SocketAddr;

use mssql_mock_tds::{
    ColumnDefinition, ColumnValue, MockTdsServer, QueryResponse, Row, SqlDataType,
};
use mssql_tds::connection::client_context::ClientContext;
use mssql_tds::connection_provider::tds_connection_provider::TdsConnectionProvider;
use mssql_tds::core::{EncryptionOptions, EncryptionSetting};
use tokio::sync::oneshot;

use super::bind_col::{sql_bind_col, sql_free_stmt_unbind};
use super::close_cursor::sql_free_stmt_close;
use super::exec_direct::sql_exec_direct_w;
use super::fetch::sql_fetch;
use super::fetch_scroll::sql_fetch_scroll;
use super::get_data::sql_get_data;
use super::odbc_types::{
    SQL_ATTR_ROW_ARRAY_SIZE, SQL_ATTR_ROW_STATUS_PTR, SQL_ATTR_ROWS_FETCHED_PTR, SQL_C_SLONG,
    SQL_FETCH_NEXT, SQL_NTS, SQL_NULL_DATA, SQL_SUCCESS, SQL_SUCCESS_WITH_INFO, SqlLen, SqlPointer,
    SqlReturn, SqlULen, SqlUSmallInt,
};
use super::set_stmt_attr::sql_set_stmt_attr_w;
use crate::handles::dbc::ConnectionState;
use crate::handles::stmt::{PreparedPlan, STMT_STATE_PREPARED};
use crate::handles::{DbcHandle, StmtHandle, handle_from_raw};
use crate::test_support::TestHandles;

const QUERY: &str = "SELECT n FROM repro_all_data_types";
const ROW_VALUES: [i32; 4] = [10, 20, 30, 40];

/// Starts a mock TDS server with `QUERY` registered to return four rows of a
/// single `INT` column, mirroring `#pytest_all_data_types`'s four persisted
/// rows in the `mssql-python` suite.
async fn start_server() -> (MockTdsServer, SocketAddr) {
    let server = MockTdsServer::new("127.0.0.1:0")
        .await
        .expect("mock server bind");
    let addr = server.local_addr();
    let registry = server.query_registry();
    {
        let mut reg = registry.lock().await;
        reg.register(
            QUERY,
            QueryResponse::new(
                vec![ColumnDefinition::new("n", SqlDataType::Int)],
                ROW_VALUES
                    .iter()
                    .map(|v| Row::new(vec![ColumnValue::Int(*v)]))
                    .collect(),
            ),
        );
    }
    (server, addr)
}

/// Connects a real `TdsClient` to `addr` and installs it on `h`'s DBC,
/// bypassing `SQLDriverConnectW`'s connection-string parsing — irrelevant to
/// this repro — the same way the fetch-path unit tests bypass it for the
/// scripted client.
fn connect(h: &TestHandles, addr: SocketAddr) {
    let dbc = unsafe { handle_from_raw::<DbcHandle>(h.dbc) };
    let mut context = ClientContext::default();
    context.user_name = "sa".to_string();
    context.password = "does-not-matter".to_string();
    context.database = "master".to_string();
    context.encryption_options = EncryptionOptions {
        mode: EncryptionSetting::PreferOff,
        trust_server_certificate: true,
        host_name_in_cert: None,
        server_certificate: None,
    };
    let datasource = format!("tcp:{},{}", addr.ip(), addr.port());
    let provider = TdsConnectionProvider::new();
    let client = dbc
        .runtime
        .block_on(provider.create_client(context, &datasource, None))
        .expect("connect to mock server");

    let mut state = dbc.inner.lock().unwrap();
    state.connection_state = ConnectionState::Connected;
    state.client = Some(client);
}

fn utf16_nts(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Runs `QUERY` on `h.stmt` and asserts it opened a result set.
fn exec(h: &TestHandles) {
    let sql = utf16_nts(QUERY);
    let ret = unsafe { sql_exec_direct_w(h.stmt, sql.as_ptr(), SQL_NTS) };
    assert!(
        ret == SQL_SUCCESS || ret == SQL_SUCCESS_WITH_INFO,
        "SQLExecDirectW failed: {ret}"
    );
}

/// Mimics `mssql-python`'s `FetchOne_wrap`: unbind, plain `SQLFetch`, no
/// `SQLBindCol`.
fn fetch_one_unbound(h: &TestHandles) -> SqlReturn {
    assert_eq!(unsafe { sql_free_stmt_unbind(h.stmt) }, SQL_SUCCESS);
    unsafe { sql_fetch(h.stmt) }
}

/// Mimics `mssql-python`'s block-fetch path (`FetchMany_wrap` /
/// `FetchBatchData`): bind one `SQL_C_SLONG` column, set
/// `SQL_ATTR_ROW_ARRAY_SIZE` / `ROWS_FETCHED_PTR` / `ROW_STATUS_PTR`, then a
/// single `SQLFetchScroll`.
fn fetch_block(h: &TestHandles, array_size: usize) -> (SqlReturn, SqlULen, Vec<i32>) {
    assert_eq!(unsafe { sql_free_stmt_unbind(h.stmt) }, SQL_SUCCESS);

    let mut buf = vec![0i32; array_size];
    let mut ind = vec![0 as SqlLen; array_size];
    let mut status = vec![0xFFFFu16 as SqlUSmallInt; array_size];
    let mut rows_fetched: SqlULen = 0;

    unsafe {
        assert_eq!(
            sql_bind_col(
                h.stmt,
                1,
                SQL_C_SLONG,
                buf.as_mut_ptr() as SqlPointer,
                0,
                ind.as_mut_ptr(),
            ),
            SQL_SUCCESS
        );
        assert_eq!(
            sql_set_stmt_attr_w(h.stmt, SQL_ATTR_ROW_ARRAY_SIZE, array_size as SqlPointer, 0),
            SQL_SUCCESS
        );
        assert_eq!(
            sql_set_stmt_attr_w(
                h.stmt,
                SQL_ATTR_ROWS_FETCHED_PTR,
                (&mut rows_fetched as *mut SqlULen).cast(),
                0,
            ),
            SQL_SUCCESS
        );
        assert_eq!(
            sql_set_stmt_attr_w(
                h.stmt,
                SQL_ATTR_ROW_STATUS_PTR,
                status.as_mut_ptr().cast(),
                0,
            ),
            SQL_SUCCESS
        );
    }

    let ret = unsafe { sql_fetch_scroll(h.stmt, SQL_FETCH_NEXT, 0) };
    (ret, rows_fetched, buf)
}

/// Full reproduction of AB#47530: one statement handle executes the same
/// query several times in a row — exactly the module-scoped `cursor` fixture
/// `mssql-python`'s `test_004_cursor.py` reuses across `test_fetchone`,
/// `test_fetchmany`, `test_fetchmany_with_arraysize`, etc. — with
/// `SQLFreeStmt(SQL_CLOSE)` between executions and rows deliberately left
/// undrained, since none of those tests fetch to end-of-set before the next
/// `execute()`.
#[test]
fn fetch_returns_real_rows_across_reexecutions() {
    let h = TestHandles::with_env_dbc_stmt();
    let (server, addr) = {
        let dbc = unsafe { handle_from_raw::<DbcHandle>(h.dbc) };
        dbc.runtime.block_on(start_server())
    };
    let (shutdown_tx, shutdown_rx) = oneshot::channel();
    {
        let dbc = unsafe { handle_from_raw::<DbcHandle>(h.dbc) };
        dbc.runtime
            .spawn(async move { server.run_with_shutdown(shutdown_rx).await });
    }
    // Give the server a moment to start accepting connections.
    std::thread::sleep(std::time::Duration::from_millis(100));

    connect(&h, addr);

    // --- test_fetchone: execute, fetch exactly one row, leave 3 undrained ---
    exec(&h);
    let ret = fetch_one_unbound(&h);
    assert_eq!(ret, SQL_SUCCESS, "test_fetchone-style fetch returned {ret}");
    let mut ind: SqlLen = -1;
    let mut value: i32 = -1;
    let get_ret = unsafe {
        sql_get_data(
            h.stmt,
            1,
            SQL_C_SLONG,
            (&mut value as *mut i32).cast(),
            4,
            &mut ind,
        )
    };
    assert_eq!(
        get_ret, SQL_SUCCESS,
        "SQLGetData after test_fetchone-style fetch"
    );
    assert_eq!(value, ROW_VALUES[0]);
    assert_ne!(ind, SQL_NULL_DATA);

    // --- test_fetchone_lob: soft-reset (SQLFreeStmt SQL_CLOSE), re-execute
    // the SAME query on the SAME statement, fetch one row again ---
    assert_eq!(unsafe { sql_free_stmt_close(h.stmt) }, SQL_SUCCESS);
    exec(&h);
    let ret = fetch_one_unbound(&h);
    assert_eq!(
        ret, SQL_SUCCESS,
        "test_fetchone_lob-style fetch got {ret} instead of a real row (AB#47530)"
    );

    // --- test_fetchmany(2): soft-reset, re-execute, block-fetch 2 rows ---
    assert_eq!(unsafe { sql_free_stmt_close(h.stmt) }, SQL_SUCCESS);
    exec(&h);
    let (ret, rows_fetched, values) = fetch_block(&h, 2);
    assert_eq!(
        ret, SQL_SUCCESS,
        "test_fetchmany-style fetch got {ret} instead of rows (AB#47530)"
    );
    assert_eq!(rows_fetched, 2, "fetchmany(2) should report 2 rows fetched");
    assert_eq!(&values[..2], &ROW_VALUES[..2]);

    // --- test_fetchmany_with_arraysize(3): soft-reset, re-execute, block-fetch 3 rows ---
    assert_eq!(unsafe { sql_free_stmt_close(h.stmt) }, SQL_SUCCESS);
    exec(&h);
    let (ret, rows_fetched, values) = fetch_block(&h, 3);
    assert_eq!(
        ret, SQL_SUCCESS,
        "test_fetchmany_with_arraysize-style fetch got {ret} instead of rows (AB#47530)"
    );
    assert_eq!(rows_fetched, 3);
    assert_eq!(&values[..3], &ROW_VALUES[..3]);

    // --- test_fetchall: soft-reset, re-execute, block-fetch all 4 rows ---
    assert_eq!(unsafe { sql_free_stmt_close(h.stmt) }, SQL_SUCCESS);
    exec(&h);
    let (ret, rows_fetched, values) = fetch_block(&h, ROW_VALUES.len());
    assert_eq!(
        ret, SQL_SUCCESS,
        "test_fetchall-style fetch got {ret} instead of rows (AB#47530)"
    );
    assert_eq!(rows_fetched, ROW_VALUES.len() as SqlULen);
    assert_eq!(values, ROW_VALUES);

    let _ = shutdown_tx.send(());
}

/// Reproduction variant covering the part of the real `test_004_cursor.py`
/// sequence the first test does not: `test_insert_args` /
/// `test_parametrized_insert` execute a **parameterized** `INSERT` first
/// (`SQLPrepare` + `SQLBindParameter` + `SQLExecute`), materializing a
/// prepared plan on the shared statement handle, before the later
/// parameterless `SELECT`s run via plain `SQLExecDirect`. `SQLExecDirect`
/// supersedes that plan: it orphans the handle and releases it with
/// `sp_unprepare` (`flush_pending_unprepare`) ahead of the new batch. A bug in
/// that hand-off — the `sp_unprepare` round trip not being fully drained
/// before the next batch is read — would misalign the wire and could plausibly
/// make the following `SELECT`'s own `ColMetadata` invisible, exactly
/// reproducing AB#47530's symptom. Simulates the materialized plan directly
/// via `PreparedStatement::materialized_for_test` (test-util) rather than a
/// real `SQLPrepare`, since the mock server's RPC handling does not implement
/// `sp_prepare`'s output-parameter contract.
#[test]
fn exec_direct_after_superseded_prepare_still_fetches_rows() {
    let h = TestHandles::with_env_dbc_stmt();
    let (server, addr) = {
        let dbc = unsafe { handle_from_raw::<DbcHandle>(h.dbc) };
        dbc.runtime.block_on(start_server())
    };
    let (shutdown_tx, shutdown_rx) = oneshot::channel();
    {
        let dbc = unsafe { handle_from_raw::<DbcHandle>(h.dbc) };
        dbc.runtime
            .spawn(async move { server.run_with_shutdown(shutdown_rx).await });
    }
    std::thread::sleep(std::time::Duration::from_millis(100));

    connect(&h, addr);

    // Simulate the state left behind by a prior `SQLPrepare` + `SQLExecute`
    // of a parameterized INSERT (test_insert_args-style), without actually
    // running one against the mock server.
    {
        let stmt = unsafe { handle_from_raw::<StmtHandle>(h.stmt) };
        let mut state = stmt.inner.lock().unwrap();
        state.prepared = Some(PreparedPlan {
            stmt: mssql_tds::connection::tds_client::PreparedStatement::materialized_for_test(
                "INSERT INTO t VALUES (?)",
                mssql_tds::connection::tds_client::StatementId::from_raw_for_test(999),
            ),
            marker_count: 1,
        });
        state.set_state(STMT_STATE_PREPARED);
    }

    // SQLExecDirect on a plain, parameterless SELECT: must orphan + sp_unprepare
    // the simulated plan, then still open and deliver the real result set.
    exec(&h);
    let ret = fetch_one_unbound(&h);
    assert_eq!(
        ret, SQL_SUCCESS,
        "SQLExecDirect after superseding a prepared plan got {ret} instead of a real row (AB#47530)"
    );
    let mut ind: SqlLen = -1;
    let mut value: i32 = -1;
    let get_ret = unsafe {
        sql_get_data(
            h.stmt,
            1,
            SQL_C_SLONG,
            (&mut value as *mut i32).cast(),
            4,
            &mut ind,
        )
    };
    assert_eq!(get_ret, SQL_SUCCESS);
    assert_eq!(value, ROW_VALUES[0]);

    let _ = shutdown_tx.send(());
}
