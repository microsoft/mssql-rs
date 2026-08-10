// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Differential tests for the synchronous, reactor-free `TdsSyncClient`.
//!
//! Every sync fetch path is checked byte-identical against an all-async oracle
//! run over the same mock result set, including the residual-byte straddle gate:
//! flipping async->sync and sync->async mid-result-set (where buffered bytes
//! straddle a packet, not a clean boundary) must reproduce the oracle exactly,
//! and a clean-boundary flip (negative control) must also pass.
//!
//! The mock server runs on its own OS thread with its own runtime, so the sync
//! client's blocking reads on the test thread never starve it — the same
//! decoupling a real (remote) SQL peer provides.

#[cfg(test)]
mod sync_client_tests {
    use std::sync::mpsc;
    use std::thread::JoinHandle;

    use mssql_mock_tds::query_response::{ColumnDefinition, ColumnValue, Row, SqlDataType};
    use mssql_mock_tds::{MockTdsServer, QueryResponse};
    use mssql_tds::connection::client_context::ClientContext;
    use mssql_tds::connection::tds_client::{ResultSet, TdsClient};
    use mssql_tds::connection::tds_sync_client::{SyncConversion, TdsSyncClient};
    use mssql_tds::connection_provider::tds_connection_provider::TdsConnectionProvider;
    use mssql_tds::core::{EncryptionOptions, EncryptionSetting};
    use mssql_tds::datatypes::column_values::ColumnValues;
    use mssql_tds::error::SqlInfoMessage;
    use tokio::sync::oneshot;

    const QUERY: &str = "SELECT ROWS";

    /// A mock server bound on its own thread + runtime; shut down on drop.
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

    /// Builds the (Int, NVarChar) result set both the oracle and the sync client
    /// fetch. Varying string lengths make row byte-sizes non-uniform, so a
    /// mid-result-set flip lands at an arbitrary offset inside the single
    /// response packet (a genuine straddle, not a clean packet boundary).
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

    /// Same (Int, NVarChar) result set, but the server emits an ERROR token after
    /// `after_rows` rows, then a terminal DONE — exercising the fetch-time
    /// error/drain path on both edges.
    fn make_error_response(row_count: usize, after_rows: usize) -> QueryResponse {
        use mssql_mock_tds::query_response::{InfoMessage, MidStreamError};
        make_response(row_count).with_error_after(MidStreamError {
            after_rows,
            number: 50_000,
            state: 1,
            severity: 16,
            message: "mid-stream boom".to_string(),
            // INFO emitted during the drain (after ERROR, before terminal DONE)
            // so both drains must capture it byte-identically.
            drain_info: vec![InfoMessage::new(50_001, 10, "post-error drain notice")],
        })
    }

    fn start_server(response: QueryResponse) -> TestServer {
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
                    registry.lock().await.register(QUERY, response);
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

    /// The all-async oracle: every row fetched via `next_row().await`.
    async fn async_oracle(addr: std::net::SocketAddr) -> Vec<Vec<ColumnValues>> {
        let mut client = connect(addr).await;
        client
            .execute(QUERY.to_string(), ())
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

    async fn execute_then_sync(addr: std::net::SocketAddr) -> TdsSyncClient {
        let mut client = connect(addr).await;
        client
            .execute(QUERY.to_string(), ())
            .await
            .expect("executes before flip");
        match client.into_sync() {
            SyncConversion::Converted(sync) => sync,
            SyncConversion::NotEligible(_) => panic!("raw TCP transport must be sync-eligible"),
            SyncConversion::Failed(err) => panic!("into_sync failed: {err:?}"),
        }
    }

    /// Sync `next_row` reproduces the async oracle byte-identically.
    #[tokio::test]
    async fn differential_next_row_matches_async_oracle() {
        let server = start_server(make_response(100));
        let expected = Box::pin(async_oracle(server.addr)).await;

        let mut sync = Box::pin(execute_then_sync(server.addr)).await;
        let mut actual = Vec::new();
        while let Some(row) = sync.next_row().expect("sync next_row") {
            actual.push(row);
        }

        assert_eq!(actual, expected);
        assert!(!actual.is_empty());
    }

    /// Sync `fetch_rows_batch` (with spare-vec recycling) reproduces the oracle.
    #[tokio::test]
    async fn differential_fetch_rows_batch_matches_async_oracle() {
        let server = start_server(make_response(100));
        let expected = Box::pin(async_oracle(server.addr)).await;

        let mut sync = Box::pin(execute_then_sync(server.addr)).await;
        let mut actual: Vec<Vec<ColumnValues>> = Vec::new();
        // Seed a small pool so the first batch exercises the spare-recycling
        // (`spare.pop()`) path; later batches fall back to fresh allocation.
        let mut spare: Vec<Vec<ColumnValues>> = vec![Vec::with_capacity(2), Vec::with_capacity(2)];
        loop {
            let before = actual.len();
            let fetched = sync
                .fetch_rows_batch(&mut actual, std::mem::take(&mut spare), 32)
                .expect("fetch_rows_batch");
            assert_eq!(actual.len() - before, fetched);
            if fetched == 0 {
                break;
            }
        }

        assert_eq!(actual, expected);
    }

    /// Straddle gate, async->sync: read some rows async (leaving residual mid
    /// packet), flip, finish sync. The concatenation must match the oracle.
    #[tokio::test]
    async fn straddle_interleave_async_to_sync() {
        let server = start_server(make_response(100));
        let expected = Box::pin(async_oracle(server.addr)).await;

        let mut client = Box::pin(connect(server.addr)).await;
        client
            .execute(QUERY.to_string(), ())
            .await
            .expect("executes");

        let mut actual = Vec::new();
        assert!(client.on_rows());
        for _ in 0..30 {
            let row = client
                .next_row()
                .await
                .expect("async next_row")
                .expect("row present before flip");
            actual.push(row);
        }

        let mut sync = match client.into_sync() {
            SyncConversion::Converted(sync) => sync,
            other => panic!(
                "expected Converted, got a different variant: {}",
                variant(&other)
            ),
        };
        while let Some(row) = sync.next_row().expect("sync next_row after flip") {
            actual.push(row);
        }

        assert_eq!(actual, expected);
    }

    /// Straddle gate, sync->async: flip to sync, read some rows, revert to async,
    /// finish async. Both residual handoffs must preserve the byte stream.
    #[tokio::test]
    async fn straddle_interleave_sync_to_async() {
        let server = start_server(make_response(100));
        let expected = Box::pin(async_oracle(server.addr)).await;

        let mut sync = Box::pin(execute_then_sync(server.addr)).await;
        let mut actual = Vec::new();
        for _ in 0..30 {
            let row = sync
                .next_row()
                .expect("sync next_row")
                .expect("row present before revert");
            actual.push(row);
        }

        let mut client = sync.into_async().expect("into_async reverts");
        while let Some(row) = client
            .next_row()
            .await
            .expect("async next_row after revert")
        {
            actual.push(row);
        }

        assert_eq!(actual, expected);
    }

    /// Clean-boundary negative control: flip before reading any row, so the
    /// residual begins at a clean token boundary (no straddle). Must also match.
    #[tokio::test]
    async fn clean_boundary_flip_matches_async_oracle() {
        let server = start_server(make_response(100));
        let expected = Box::pin(async_oracle(server.addr)).await;

        let mut sync = Box::pin(execute_then_sync(server.addr)).await;
        let mut actual = Vec::new();
        while let Some(row) = sync.next_row().expect("sync next_row") {
            actual.push(row);
        }

        assert_eq!(actual, expected);
    }

    /// Reversibility: after a full sync fetch, `into_async` yields a working
    /// async client that can run further control-plane work.
    #[tokio::test]
    async fn into_async_returns_a_working_client() {
        let server = start_server(make_response(50));
        let _ = Box::pin(async_oracle(server.addr)).await;

        let mut sync = Box::pin(execute_then_sync(server.addr)).await;
        while sync.next_row().expect("sync next_row").is_some() {}

        let mut client = sync.into_async().expect("into_async reverts");
        client
            .close_query()
            .await
            .expect("close_query on reverted client");
        client
            .close_connection()
            .await
            .expect("close_connection on reverted client");
    }

    /// The publicly-observable connection state a fetch-time ERROR must leave
    /// behind once its batch is drained to terminal DONE. Both shells project
    /// this from the same two fields cleared by the shared `finalize_row_error`
    /// (`current_metadata` -> `None`, `current_result_set_has_been_read_till_end`
    /// -> `true`), so the sync drain must reproduce it byte-identically.
    #[derive(Debug, PartialEq, Eq)]
    struct TerminalState {
        metadata_empty: bool,
        maybe_has_unread_rows: bool,
    }

    /// The all-async error oracle: fetch rows until the mid-stream ERROR, then
    /// capture the surfaced error's `Debug` form plus the post-drain terminal
    /// state for a byte-identical compare.
    async fn async_oracle_until_error(
        addr: std::net::SocketAddr,
    ) -> (
        Vec<Vec<ColumnValues>>,
        String,
        TerminalState,
        Vec<SqlInfoMessage>,
    ) {
        let mut client = connect(addr).await;
        client
            .execute(QUERY.to_string(), ())
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
        let terminal = TerminalState {
            metadata_empty: client.get_metadata().is_empty(),
            maybe_has_unread_rows: client.maybe_has_unread_rows(),
        };
        let info = client.take_info_messages();
        (rows, err, terminal, info)
    }

    /// Sync analog: fetch via `next_row` until the ERROR surfaces (driving the
    /// sync blocking drain), returning the rows, the error's `Debug` form, the
    /// post-drain terminal state, and the info captured during the drain.
    fn sync_fetch_until_error(
        sync: &mut TdsSyncClient,
    ) -> (
        Vec<Vec<ColumnValues>>,
        String,
        TerminalState,
        Vec<SqlInfoMessage>,
    ) {
        let mut rows = Vec::new();
        let err = loop {
            match sync.next_row() {
                Ok(Some(row)) => rows.push(row),
                Ok(None) => panic!("expected a mid-stream error, got a clean end"),
                Err(e) => break format!("{e:?}"),
            }
        };
        let terminal = TerminalState {
            metadata_empty: sync.get_metadata().is_empty(),
            maybe_has_unread_rows: sync.maybe_has_unread_rows(),
        };
        let info = sync.take_info_messages();
        (rows, err, terminal, info)
    }

    /// Differential error path (all sync): rows-before-error and the surfaced
    /// error match the async oracle exactly, driving the sync ERROR drain.
    #[tokio::test]
    async fn differential_error_mid_fetch_matches_async_oracle() {
        let server = start_server(make_error_response(100, 40));
        let (expected_rows, expected_err, expected_terminal, expected_info) =
            Box::pin(async_oracle_until_error(server.addr)).await;

        let mut sync = Box::pin(execute_then_sync(server.addr)).await;
        let (actual_rows, actual_err, actual_terminal, actual_info) =
            sync_fetch_until_error(&mut sync);

        assert_eq!(actual_rows, expected_rows);
        assert_eq!(actual_err, expected_err);
        assert_eq!(actual_rows.len(), 40);
        assert_eq!(actual_terminal, expected_terminal);
        // The INFO emitted during the drain must be captured byte-identically by
        // the sync blocking drain and the async `drain_stream`.
        assert_eq!(actual_info, expected_info);
        assert_eq!(actual_info.len(), 1);
        // Pin the absolute terminal state, not just sync/async parity: the drain
        // reached terminal DONE, so metadata is cleared and no rows remain.
        assert!(expected_terminal.metadata_empty);
        assert!(!expected_terminal.maybe_has_unread_rows);
    }

    /// Differential error path via `fetch_rows_batch`: the batched loop pushes the
    /// rows-before-error, then propagates the drained ERROR, leaving the same
    /// terminal state and captured INFO as the async oracle.
    #[tokio::test]
    async fn differential_fetch_rows_batch_error_matches_async_oracle() {
        let server = start_server(make_error_response(100, 40));
        let (expected_rows, expected_err, expected_terminal, expected_info) =
            Box::pin(async_oracle_until_error(server.addr)).await;

        let mut sync = Box::pin(execute_then_sync(server.addr)).await;
        let mut actual: Vec<Vec<ColumnValues>> = Vec::new();
        let mut spare: Vec<Vec<ColumnValues>> = vec![Vec::with_capacity(2), Vec::with_capacity(2)];
        let actual_err = loop {
            match sync.fetch_rows_batch(&mut actual, std::mem::take(&mut spare), 32) {
                Ok(0) => panic!("expected a mid-stream error, got a clean end"),
                Ok(_) => continue,
                Err(e) => break format!("{e:?}"),
            }
        };
        let actual_terminal = TerminalState {
            metadata_empty: sync.get_metadata().is_empty(),
            maybe_has_unread_rows: sync.maybe_has_unread_rows(),
        };
        let actual_info = sync.take_info_messages();

        assert_eq!(actual, expected_rows);
        assert_eq!(actual.len(), 40);
        assert_eq!(actual_err, expected_err);
        assert_eq!(actual_terminal, expected_terminal);
        assert_eq!(actual_info, expected_info);
        assert!(actual_terminal.metadata_empty);
        assert!(!actual_terminal.maybe_has_unread_rows);
    }

    /// Straddle error, async->sync: read rows async, flip mid-result-set, then the
    /// ERROR surfaces on the sync edge (sync drain over transferred residual).
    #[tokio::test]
    async fn straddle_error_async_to_sync_matches_oracle() {
        let server = start_server(make_error_response(100, 40));
        let (expected_rows, expected_err, expected_terminal, expected_info) =
            Box::pin(async_oracle_until_error(server.addr)).await;

        let mut client = Box::pin(connect(server.addr)).await;
        client
            .execute(QUERY.to_string(), ())
            .await
            .expect("executes");
        let mut actual = Vec::new();
        assert!(client.on_rows());
        for _ in 0..30 {
            let row = client
                .next_row()
                .await
                .expect("async next_row")
                .expect("row present before flip");
            actual.push(row);
        }

        let mut sync = match client.into_sync() {
            SyncConversion::Converted(sync) => sync,
            other => panic!("expected Converted, got {}", variant(&other)),
        };
        let (rest, actual_err, actual_terminal, actual_info) = sync_fetch_until_error(&mut sync);
        actual.extend(rest);

        assert_eq!(actual, expected_rows);
        assert_eq!(actual_err, expected_err);
        assert_eq!(actual_terminal, expected_terminal);
        assert_eq!(actual_info, expected_info);
        assert!(actual_terminal.metadata_empty);
        assert!(!actual_terminal.maybe_has_unread_rows);
    }

    /// Straddle error, sync->async: read rows sync, revert mid-result-set, then the
    /// ERROR surfaces on the async edge (async drain over transferred residual).
    #[tokio::test]
    async fn straddle_error_sync_to_async_matches_oracle() {
        let server = start_server(make_error_response(100, 40));
        let (expected_rows, expected_err, expected_terminal, expected_info) =
            Box::pin(async_oracle_until_error(server.addr)).await;

        let mut sync = Box::pin(execute_then_sync(server.addr)).await;
        let mut actual = Vec::new();
        for _ in 0..30 {
            let row = sync
                .next_row()
                .expect("sync next_row")
                .expect("row present before revert");
            actual.push(row);
        }

        let mut client = sync.into_async().expect("into_async reverts");
        let actual_err = loop {
            match client.next_row().await {
                Ok(Some(row)) => actual.push(row),
                Ok(None) => panic!("expected a mid-stream error, got a clean end"),
                Err(e) => break format!("{e:?}"),
            }
        };
        let actual_terminal = TerminalState {
            metadata_empty: client.get_metadata().is_empty(),
            maybe_has_unread_rows: client.maybe_has_unread_rows(),
        };
        let actual_info = client.take_info_messages();

        assert_eq!(actual, expected_rows);
        assert_eq!(actual_err, expected_err);
        assert_eq!(actual_terminal, expected_terminal);
        assert_eq!(actual_info, expected_info);
        assert!(actual_terminal.metadata_empty);
        assert!(!actual_terminal.maybe_has_unread_rows);
    }

    fn variant(conversion: &SyncConversion) -> &'static str {
        match conversion {
            SyncConversion::Converted(_) => "Converted",
            SyncConversion::NotEligible(_) => "NotEligible",
            SyncConversion::Failed(_) => "Failed",
        }
    }
}
