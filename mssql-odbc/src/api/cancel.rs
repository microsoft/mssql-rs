// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Implementation of `SQLCancel`.
//!
//! `SQLCancel` has two distinct jobs in ODBC. Against a statement executing on
//! another thread it asks the server to abandon the running command; against a
//! statement in the "Need Data" state it abandons a data-at-execution sequence
//! so the statement can be executed again. Cross-thread cancellation signals
//! first, then waits for the interrupted call to finish settlement and release
//! its client and statement state. Diagnostics belong to the interrupted call.

use std::sync::TryLockError;
use tracing::{debug, error};

use super::exec_common::finish_dae_unwind;
use crate::api::odbc_types::{SQL_ERROR, SQL_INVALID_HANDLE, SQL_SUCCESS, SqlHandle, SqlReturn};
use crate::error::free_errors;
use crate::handles::stmt::STMT_STATE_EXEC_STARTED;
use crate::handles::{HandleType, StmtHandle, handle_from_raw, process_is_shutting_down};

/// Cancels processing on a statement.
///
/// When the statement is awaiting data-at-execution input, the parked request
/// is discarded and the statement returns to its prepared state, which is what
/// the ODBC spec requires: "the application can then call `SQLExecute` or
/// `SQLExecDirect` again".
///
/// # Safety
/// - `statement_handle` must be a valid `STMT` handle allocated by
///   `SQLAllocHandle`.
pub(crate) unsafe fn sql_cancel(statement_handle: SqlHandle) -> SqlReturn {
    debug!(?statement_handle, "SQLCancel called");
    crate::ffi_entry!("SQLCancel", unsafe { sql_cancel_impl(statement_handle) })
}

/// # Safety
/// `statement_handle` must be null or point to a live `StmtHandle`.
unsafe fn sql_cancel_impl(statement_handle: SqlHandle) -> SqlReturn {
    if statement_handle.is_null() {
        error!("SQLCancel: statement_handle is null");
        return SQL_INVALID_HANDLE;
    }

    let stmt = unsafe { handle_from_raw::<StmtHandle>(statement_handle) };
    debug_assert_eq!(
        stmt.object_type,
        HandleType::Stmt,
        "SQLCancel: handle is not a STMT"
    );

    let cancellation = match stmt.cancel_operation() {
        Ok(cancellation) => cancellation,
        Err(rc) => return rc,
    };
    if cancellation.interrupted {
        return SQL_SUCCESS;
    }
    let client = {
        // Calls not participating in cancellation can briefly hold inner.
        // An idle cancel need not wait for them or alter their diagnostics.
        let mut stmt_state = match stmt.inner.try_lock() {
            Ok(state) => state,
            Err(TryLockError::WouldBlock) => return SQL_SUCCESS,
            Err(TryLockError::Poisoned(_)) => {
                error!("SQLCancel: stmt mutex poisoned");
                return SQL_ERROR;
            }
        };
        if stmt_state
            .dae
            .as_ref()
            .is_none_or(|dae| dae.call_in_flight())
        {
            // Never clear or post diagnostics for the interrupted call, even
            // if it finished before we acquired this lock.
            return SQL_SUCCESS;
        }
        free_errors(&mut stmt_state);
        let client = stmt_state.take_dae();
        stmt_state.clear_state(STMT_STATE_EXEC_STARTED);
        client
    };

    debug!("SQLCancel: abandoning data-at-execution sequence");
    finish_dae_unwind(
        stmt.parent_dbc(),
        statement_handle,
        client,
        process_is_shutting_down(),
    );

    SQL_SUCCESS
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::odbc_types::SQL_INVALID_HANDLE;
    use crate::handles::stmt::{DaeParam, DaeState, STMT_STATE_EXEC_STARTED};
    use crate::test_support::TestHandles;

    fn dae_with_one_param(cursor: Option<usize>) -> DaeState {
        DaeState::for_test(
            vec![DaeParam::unbounded(0, std::ptr::null_mut(), None)],
            cursor,
        )
    }

    #[test]
    fn null_handle_returns_invalid_handle() {
        assert_eq!(SQL_INVALID_HANDLE, unsafe {
            sql_cancel(std::ptr::null_mut())
        });
    }

    #[test]
    fn cancel_outside_need_data_is_a_success_noop() {
        let h = TestHandles::with_env_dbc_stmt();
        assert_eq!(SQL_SUCCESS, unsafe { sql_cancel(h.stmt) });
    }

    #[test]
    fn cancel_clears_need_data_and_restores_prepared_plan() {
        let h = TestHandles::with_env_dbc_stmt();
        let stmt = unsafe { handle_from_raw::<StmtHandle>(h.stmt) };
        {
            let mut state = stmt.inner.lock().unwrap();
            state.set_state(STMT_STATE_EXEC_STARTED);
            let mut dae = dae_with_one_param(None);
            dae.progress.bytes_sent = 3;
            state.dae = Some(dae);
        }

        assert_eq!(SQL_SUCCESS, unsafe { sql_cancel(h.stmt) });

        let state = stmt.inner.lock().unwrap();
        assert!(!state.needs_data());
        assert!(!state.has_state(STMT_STATE_EXEC_STARTED));
    }

    #[test]
    fn cancel_during_in_flight_dae_call_preserves_state_and_diagnostics() {
        let h = TestHandles::with_env_dbc_stmt();
        let stmt = unsafe { handle_from_raw::<StmtHandle>(h.stmt) };
        {
            let mut state = stmt.inner.lock().unwrap();
            state.set_state(STMT_STATE_EXEC_STARTED);
            let mut dae = dae_with_one_param(Some(0));
            dae.set_call_in_flight(true);
            state.dae = Some(dae);
            state.diag_records.push(crate::error::DiagRecord::new(
                *b"01000",
                42,
                "owned by the executing call",
            ));
        }

        assert_eq!(SQL_SUCCESS, unsafe { sql_cancel(h.stmt) });

        let state = stmt.inner.lock().unwrap();
        assert_eq!(state.diag_records.len(), 1);
        assert_eq!(state.diag_records[0].sql_state, *b"01000");
        assert_eq!(state.diag_records[0].native_error, 42);
        // The sequence is untouched, so the owning thread can still finish it.
        assert!(state.needs_data());
    }

    #[test]
    fn idle_cancel_never_waits_for_stmt_lock_or_changes_diagnostics() {
        let h = TestHandles::with_env_dbc_stmt();
        let stmt = unsafe { handle_from_raw::<StmtHandle>(h.stmt) };
        let mut state = stmt.inner.lock().unwrap();
        state.diag_records.push(crate::error::DiagRecord::new(
            *b"HY008",
            0,
            "execution cancelled",
        ));
        // Holding inner on this thread would deadlock a blocking SQLCancel.
        assert_eq!(SQL_SUCCESS, unsafe { crate::api::SQLCancel(h.stmt) });
        drop(state);
        assert_eq!(SQL_SUCCESS, unsafe { crate::api::SQLCancel(h.stmt) });
        let state = stmt.inner.lock().unwrap();
        assert_eq!(state.diag_records.len(), 1);
        assert_eq!(state.diag_records[0].sql_state, *b"HY008");
    }

    #[test]
    fn fresh_execution_does_not_inherit_idle_or_previous_cancellation() {
        use mssql_mock_tds::QueryResponse;
        use mssql_tds::connection::tds_client::ExecuteOptions;
        use std::time::Duration;

        let h = TestHandles::with_env_dbc_stmt();
        let stmt = unsafe { handle_from_raw::<StmtHandle>(h.stmt) };
        let dbc = stmt.parent_dbc();
        let _server = crate::test_support::connect_mock_server(
            dbc,
            "SELECT 1",
            QueryResponse::select_one().with_delay(Duration::from_millis(100)),
        );
        assert_eq!(SQL_SUCCESS, unsafe { crate::api::SQLCancel(h.stmt) });
        let previous = stmt.new_execution_cancel().unwrap();
        assert_eq!(SQL_SUCCESS, unsafe { crate::api::SQLCancel(h.stmt) });
        let fresh = stmt.new_execution_cancel().unwrap();
        // A late signal through the previous execution must remain isolated.
        previous.cancel();
        let mut client = dbc.inner.lock().unwrap().client.take().unwrap();
        dbc.runtime.block_on(async {
            client
                .execute("SELECT 1".to_string(), ExecuteOptions::new().cancel(&fresh))
                .await
                .unwrap();
            client.close_query().await.unwrap();
        });
        dbc.inner.lock().unwrap().client = Some(client);
    }

    #[derive(Clone, Copy, Debug)]
    enum CursorPhase {
        Fetch,
        ReadAhead,
        GetData,
        Plp,
        GetDataReadAhead,
        MoreResults,
        Close,
    }

    fn cancel_cursor_phase(phase: CursorPhase, acknowledge: bool) {
        use crate::api::odbc_types::*;
        use crate::handles::stmt::STMT_STATE_CURSOR_OPEN;
        use mssql_mock_tds::protocol::{build_attention_ack_packet, build_query_result};
        use mssql_mock_tds::server::ConnectionProcessor;
        use mssql_mock_tds::{
            ColumnDefinition, ColumnValue, QueryRegistry, QueryResponse, Row, SqlDataType,
        };
        use mssql_tds::connection::client_context::ClientContext;
        use mssql_tds::connection_provider::tds_connection_provider::TdsConnectionProvider;
        use mssql_tds::core::{EncryptionOptions, EncryptionSetting};
        use std::sync::Arc;
        use std::time::{Duration, Instant};
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let h = TestHandles::with_env_dbc_stmt();
        let stmt = unsafe { handle_from_raw::<StmtHandle>(h.stmt) };
        let dbc = stmt.parent_dbc();
        let server_runtime = tokio::runtime::Runtime::new().unwrap();
        let response = if matches!(phase, CursorPhase::Plp | CursorPhase::GetDataReadAhead) {
            QueryResponse::new(
                vec![ColumnDefinition::new("", SqlDataType::VarBinaryMax)],
                vec![Row::new(vec![ColumnValue::VarBinaryMax(vec![vec![1; 32]])])],
            )
        } else if matches!(phase, CursorPhase::GetData) {
            QueryResponse::new(
                vec![
                    ColumnDefinition::new("", SqlDataType::Int),
                    ColumnDefinition::new("", SqlDataType::Int),
                ],
                vec![Row::new(vec![ColumnValue::Int(1), ColumnValue::Int(2)])],
            )
        } else {
            QueryResponse::select_one()
        };
        let full = build_query_result(&response);
        // Hold back part of a value, a PLP read-ahead, or the final DONE.
        let held = match phase {
            CursorPhase::Fetch | CursorPhase::GetData => 13 + 5,
            CursorPhase::Plp => 13 + 4 + 32,
            _ => 13,
        };
        let split = full.len() - held;
        let mut prefix = full[..split].to_vec();
        prefix[1] = 0;
        prefix[2..4].copy_from_slice(&u16::try_from(split).unwrap().to_be_bytes());
        let mut tail = vec![4, 1, 0, 0, 0, 0, 2, 0];
        tail.extend_from_slice(&full[split..]);
        let tail_len = u16::try_from(tail.len()).unwrap();
        tail[2..4].copy_from_slice(&tail_len.to_be_bytes());
        let (attention_tx, attention_rx) = std::sync::mpsc::channel();
        let (settle_tx, settle_rx) = tokio::sync::oneshot::channel();
        let (tail_tx, tail_rx) = std::sync::mpsc::channel();
        let (ack_tx, ack_rx) = tokio::sync::oneshot::channel();
        let (addr, server) = server_runtime.block_on(async {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            let server = tokio::spawn(async move {
                tokio::time::timeout(Duration::from_secs(10), async move {
                    let (mut socket, peer) = listener.accept().await.unwrap();
                    let mut registry = QueryRegistry::new();
                    registry.register("SELECT 1", QueryResponse::select_one());
                    let mut processor = ConnectionProcessor::new(
                        0,
                        peer,
                        Arc::new(tokio::sync::Mutex::new(registry)),
                        None,
                    );
                    let mut first = true;
                    let mut settle_rx = Some(settle_rx);
                    let mut ack_rx = Some(ack_rx);
                    loop {
                        let mut header = [0u8; 8];
                        if let Err(error) = socket.read_exact(&mut header).await {
                            assert_eq!(error.kind(), std::io::ErrorKind::UnexpectedEof);
                            break;
                        }
                        let len = usize::from(u16::from_be_bytes([header[2], header[3]]));
                        let mut body = vec![0; len - 8];
                        socket.read_exact(&mut body).await.unwrap();
                        if header[0] == 0x12 {
                            socket
                                .write_all(
                                    &mssql_mock_tds::protocol::build_prelogin_response_with_fedauth(
                                        false, true,
                                    ),
                                )
                                .await
                                .unwrap();
                        } else if processor.is_authenticated() && header[0] == 1 && first {
                            first = false;
                            socket.write_all(&prefix).await.unwrap();
                        } else if header[0] == 6 {
                            attention_tx.send(()).unwrap();
                            settle_rx.take().unwrap().await.unwrap();
                            if !acknowledge {
                                break;
                            }
                            socket.write_all(&tail).await.unwrap();
                            tail_tx.send(()).unwrap();
                            ack_rx.take().unwrap().await.unwrap();
                            socket
                                .write_all(&build_attention_ack_packet())
                                .await
                                .unwrap();
                        } else {
                            processor.buffer_mut().extend_from_slice(&header);
                            processor.buffer_mut().extend_from_slice(&body);
                            if let Some(reply) =
                                processor.process_packet(&mut socket).await.unwrap()
                            {
                                socket.write_all(&reply).await.unwrap();
                            }
                        }
                    }
                })
                .await
                .unwrap();
            });
            (addr, server)
        });
        let mut context = ClientContext::default();
        context.user_name = "sa".to_string();
        context.password = "unused-by-the-mock-server".to_string();
        context.database = "master".to_string();
        context.encryption_options = EncryptionOptions {
            mode: EncryptionSetting::PreferOff,
            trust_server_certificate: true,
            host_name_in_cert: None,
            server_certificate: None,
        };
        let client = dbc
            .runtime
            .block_on(TdsConnectionProvider {}.create_client(
                context,
                &format!("tcp:{},{}", addr.ip(), addr.port()),
                None,
            ))
            .unwrap();
        {
            let mut state = dbc.inner.lock().unwrap();
            state.client = Some(client);
            state.connection_state = crate::handles::dbc::ConnectionState::Connected;
        }
        let sql: Vec<u16> = "SELECT 1".encode_utf16().collect();
        assert_eq!(
            SQL_SUCCESS,
            unsafe {
                crate::api::SQLExecDirectW(h.stmt, sql.as_ptr(), i16::try_from(sql.len()).unwrap())
            },
            "{:?}",
            stmt.inner.lock().unwrap().diag_records
        );
        let mut bound = 0i32;
        let mut length = 0;
        if matches!(
            phase,
            CursorPhase::Fetch | CursorPhase::ReadAhead | CursorPhase::GetData
        ) {
            assert_eq!(SQL_SUCCESS, unsafe {
                crate::api::SQLBindCol(
                    h.stmt,
                    1,
                    SQL_C_LONG,
                    (&mut bound as *mut i32).cast(),
                    4,
                    &mut length,
                )
            });
        }
        if matches!(
            phase,
            CursorPhase::GetData | CursorPhase::Plp | CursorPhase::GetDataReadAhead
        ) {
            assert_eq!(
                SQL_SUCCESS,
                unsafe { crate::api::SQLFetch(h.stmt) },
                "{phase:?}"
            );
        }
        let raw = h.stmt as usize;
        std::thread::scope(|scope| {
            let operation = scope.spawn(move || unsafe {
                let raw = raw as SqlHandle;
                match phase {
                    CursorPhase::Fetch | CursorPhase::ReadAhead => crate::api::SQLFetch(raw),
                    CursorPhase::MoreResults => crate::api::SQLMoreResults(raw),
                    CursorPhase::Close => crate::api::SQLFreeStmt(raw, SQL_CLOSE),
                    _ => {
                        let mut data = [0u8; 64];
                        let mut length = 0;
                        crate::api::SQLGetData(
                            raw,
                            if matches!(phase, CursorPhase::GetData) {
                                2
                            } else {
                                1
                            },
                            if matches!(phase, CursorPhase::GetData) {
                                SQL_C_LONG
                            } else {
                                SQL_C_BINARY
                            },
                            data.as_mut_ptr().cast(),
                            if matches!(phase, CursorPhase::GetDataReadAhead) {
                                64
                            } else {
                                4
                            },
                            &mut length,
                        )
                    }
                }
            });
            let deadline = Instant::now() + Duration::from_secs(2);
            while dbc.inner.lock().unwrap().client.is_some() {
                assert!(
                    Instant::now() < deadline,
                    "{phase:?} did not enter network I/O"
                );
                std::thread::yield_now();
            }
            let (cancel_tx, cancel_rx) = std::sync::mpsc::channel();
            let cancel = scope.spawn(move || {
                let rc = unsafe { crate::api::SQLCancel(raw as SqlHandle) };
                cancel_tx.send(rc).unwrap();
            });
            attention_rx.recv_timeout(Duration::from_secs(2)).unwrap();
            assert!(cancel_rx.recv_timeout(Duration::from_millis(25)).is_err());
            settle_tx.send(()).unwrap();
            if acknowledge {
                tail_rx.recv_timeout(Duration::from_secs(2)).unwrap();
                // An ordinary final DONE must not release the cancellation fence.
                assert!(cancel_rx.recv_timeout(Duration::from_millis(25)).is_err());
                ack_tx.send(()).unwrap();
            }
            assert_eq!(
                SQL_SUCCESS,
                cancel_rx.recv_timeout(Duration::from_secs(5)).unwrap()
            );
            // Do not join the operation before checking the cancellation fence.
            {
                let state = stmt.inner.lock().unwrap();
                assert!(!state.has_state(STMT_STATE_CURSOR_OPEN), "{phase:?}");
                assert!(state.active_plp.is_none());
                assert!(state.pending_fetch_error.is_none());
                assert_eq!(state.diag_records[0].sql_state, *b"HY008");
            }
            {
                let state = dbc.inner.lock().unwrap();
                assert!(state.active_stmt.is_none());
                assert_eq!(
                    state.client.as_ref().unwrap().is_connection_dead(),
                    !acknowledge
                );
            }
            assert_eq!(SQL_SUCCESS, unsafe {
                crate::api::SQLFreeStmt(h.stmt, SQL_CLOSE)
            });
            assert_eq!(SQL_ERROR, operation.join().unwrap());
            cancel.join().unwrap();
        });
        if acknowledge {
            assert_eq!(SQL_SUCCESS, unsafe {
                crate::api::SQLExecDirectW(h.stmt, sql.as_ptr(), i16::try_from(sql.len()).unwrap())
            });
            assert_eq!(SQL_SUCCESS, unsafe { crate::api::SQLFetch(h.stmt) });
        }
        // The server task is scoped to this mock, not a shared runtime.
        server.abort();
    }

    #[test]
    fn cursor_cancellation_waits_for_ack_and_releases_all_cursor_state() {
        for phase in [
            CursorPhase::Fetch,
            CursorPhase::ReadAhead,
            CursorPhase::GetData,
            CursorPhase::Plp,
            CursorPhase::GetDataReadAhead,
            CursorPhase::MoreResults,
            CursorPhase::Close,
        ] {
            cancel_cursor_phase(phase, true);
        }
    }

    #[test]
    fn failed_cursor_cancellation_settlement_retires_connection() {
        cancel_cursor_phase(CursorPhase::Fetch, false);
    }

    fn cancel_blocked(h: &TestHandles, execute: impl FnOnce(SqlHandle) -> SqlReturn + Send) {
        use std::time::{Duration, Instant};
        let dbc = unsafe { handle_from_raw::<crate::handles::DbcHandle>(h.dbc) };
        let raw = h.stmt as usize;
        std::thread::scope(|scope| {
            let (tx, rx) = std::sync::mpsc::channel();
            let worker = scope.spawn(move || {
                tx.send(execute(raw as SqlHandle)).unwrap();
            });
            let started = Instant::now();
            while dbc.inner.lock().unwrap().client.is_some()
                && started.elapsed() < Duration::from_secs(2)
            {
                std::thread::sleep(Duration::from_millis(1));
            }
            // Let the request reach the mock's delayed-response read, rather
            // than testing only cancellation of a not-yet-sent request.
            std::thread::sleep(Duration::from_millis(200));
            let cancelled = Instant::now();
            assert_eq!(SQL_SUCCESS, unsafe { crate::api::SQLCancel(h.stmt) });
            assert_eq!(SQL_SUCCESS, unsafe { crate::api::SQLCancel(h.stmt) });
            let result = rx.recv_timeout(Duration::from_secs(5));
            worker.join().unwrap();
            assert_eq!(result.unwrap(), SQL_ERROR);
            assert!(cancelled.elapsed() < Duration::from_secs(5));
        });
        let stmt = unsafe { handle_from_raw::<StmtHandle>(h.stmt) };
        {
            let state = stmt.inner.lock().unwrap();
            assert_eq!(state.diag_records[0].sql_state, *b"HY008");
            assert!(!state.has_state(STMT_STATE_EXEC_STARTED));
            assert!(!state.needs_data());
        }
        {
            let state = dbc.inner.lock().unwrap();
            assert!(state.active_stmt.is_none());
            assert!(!state.client.as_ref().unwrap().is_connection_dead());
        }
        assert_eq!(SQL_SUCCESS, unsafe { crate::api::SQLCancel(h.stmt) });
        assert_eq!(
            stmt.inner.lock().unwrap().diag_records[0].sql_state,
            *b"HY008"
        );
        assert_eq!(SQL_SUCCESS, unsafe {
            crate::api::SQLSetStmtAttrW(
                h.stmt,
                crate::api::odbc_types::SQL_ATTR_PARAMSET_SIZE,
                1usize as crate::api::odbc_types::SqlPointer,
                0,
            )
        });
        let sql: Vec<u16> = "SELECT 1".encode_utf16().collect();
        assert_eq!(SQL_SUCCESS, unsafe {
            crate::api::SQLExecDirectW(h.stmt, sql.as_ptr(), i16::try_from(sql.len()).unwrap())
        });
        assert_eq!(SQL_SUCCESS, unsafe { crate::api::SQLFetch(h.stmt) });
        let mut value = 0i32;
        let mut len = 0;
        assert_eq!(SQL_SUCCESS, unsafe {
            crate::api::SQLGetData(
                h.stmt,
                1,
                crate::api::odbc_types::SQL_C_LONG,
                (&mut value as *mut i32).cast(),
                4,
                &mut len,
            )
        });
        assert_eq!(value, 1);
    }

    #[test]
    fn cancel_interrupts_statement_execute_paths_and_connection_is_reusable() {
        use crate::api::odbc_types::*;
        use mssql_mock_tds::QueryResponse;
        use std::time::Duration;

        for path in [
            "direct",
            "parameterized",
            "rpc",
            "prepared",
            "array",
            "type_info",
            "catalog",
            "describe_param",
        ] {
            let h = TestHandles::with_env_dbc_stmt();
            let dbc = unsafe { handle_from_raw::<crate::handles::DbcHandle>(h.dbc) };
            let server = crate::test_support::connect_mock_server(
                dbc,
                "WAITFOR",
                QueryResponse::select_one().with_delay(Duration::from_secs(8)),
            );
            server.register_query(
                "WAITFOR DELAY '00:00:08'; SELECT 2",
                QueryResponse::select_one().with_delay(Duration::from_secs(8)),
            );
            server.register_query("SELECT 1", QueryResponse::select_one());
            server.set_rpc_delay(Duration::from_secs(8));
            let sql: Vec<u16> = match path {
                "rpc" => "{call cancel_test}",
                "parameterized" | "array" => "WAITFOR DELAY '00:00:08'; SELECT ?",
                "describe_param" => "SELECT ?",
                _ => "WAITFOR DELAY '00:00:08'; SELECT 2",
            }
            .encode_utf16()
            .collect();
            let sql_len = i16::try_from(sql.len()).unwrap();
            let mut values = [7i32, 8];
            if matches!(path, "parameterized" | "array") {
                assert_eq!(SQL_SUCCESS, unsafe {
                    crate::api::SQLBindParameter(
                        h.stmt,
                        1,
                        SQL_PARAM_INPUT,
                        SQL_C_LONG,
                        SQL_INTEGER,
                        10,
                        0,
                        values.as_mut_ptr().cast(),
                        4,
                        std::ptr::null_mut(),
                    )
                });
            }
            if path == "array" {
                assert_eq!(SQL_SUCCESS, unsafe {
                    crate::api::SQLSetStmtAttrW(
                        h.stmt,
                        SQL_ATTR_PARAMSET_SIZE,
                        2usize as SqlPointer,
                        0,
                    )
                });
            }
            if matches!(path, "prepared" | "array" | "describe_param") {
                assert_eq!(SQL_SUCCESS, unsafe {
                    crate::api::SQLPrepareW(h.stmt, sql.as_ptr(), sql_len)
                });
            }
            cancel_blocked(&h, move |raw| unsafe {
                match path {
                    "prepared" | "array" => crate::api::SQLExecute(raw),
                    "type_info" => crate::api::SQLGetTypeInfoW(raw, SQL_ALL_TYPES),
                    "catalog" => crate::api::SQLTablesW(
                        raw,
                        std::ptr::null(),
                        0,
                        std::ptr::null(),
                        0,
                        std::ptr::null(),
                        0,
                        std::ptr::null(),
                        0,
                    ),
                    "describe_param" => crate::api::SQLDescribeParam(
                        raw,
                        1,
                        std::ptr::null_mut(),
                        std::ptr::null_mut(),
                        std::ptr::null_mut(),
                        std::ptr::null_mut(),
                    ),
                    _ => crate::api::SQLExecDirectW(raw, sql.as_ptr(), sql_len),
                }
            });
        }
    }

    #[test]
    fn cancel_interrupts_final_param_data_for_streamed_and_deferred_execution() {
        use crate::api::odbc_types::*;
        use mssql_mock_tds::QueryResponse;
        use std::time::Duration;

        for (buffered, prepared, mixed) in [
            (false, false, false),
            (true, false, false),
            (false, true, false),
            (true, true, false),
            (true, false, true),
            (true, true, true),
        ] {
            let h = TestHandles::with_env_dbc_stmt();
            let dbc = unsafe { handle_from_raw::<crate::handles::DbcHandle>(h.dbc) };
            let server = crate::test_support::connect_mock_server(
                dbc,
                "WAITFOR",
                QueryResponse::select_one().with_delay(Duration::from_secs(8)),
            );
            server.register_query("SELECT 1", QueryResponse::select_one());
            let mut indicator = SQL_DATA_AT_EXEC;
            let mut value = b'7';
            assert_eq!(SQL_SUCCESS, unsafe {
                crate::api::SQLBindParameter(
                    h.stmt,
                    1,
                    SQL_PARAM_INPUT,
                    SQL_C_CHAR,
                    if buffered { SQL_INTEGER } else { SQL_VARCHAR },
                    10,
                    0,
                    (&mut value as *mut u8).cast(),
                    1,
                    &mut indicator,
                )
            });
            let mut second_indicator = SQL_DATA_AT_EXEC;
            if mixed {
                assert_eq!(SQL_SUCCESS, unsafe {
                    crate::api::SQLBindParameter(
                        h.stmt,
                        2,
                        SQL_PARAM_INPUT,
                        SQL_C_CHAR,
                        SQL_VARCHAR,
                        10,
                        0,
                        (&mut value as *mut u8).cast(),
                        1,
                        &mut second_indicator,
                    )
                });
            }
            let sql: Vec<u16> = if mixed {
                "WAITFOR DELAY '00:00:08'; SELECT ?, ?"
            } else {
                "WAITFOR DELAY '00:00:08'; SELECT ?"
            }
            .encode_utf16()
            .collect();
            let sql_len = i16::try_from(sql.len()).unwrap();
            if prepared {
                assert_eq!(SQL_SUCCESS, unsafe {
                    crate::api::SQLPrepareW(h.stmt, sql.as_ptr(), sql_len)
                });
                assert_eq!(SQL_NEED_DATA, unsafe { crate::api::SQLExecute(h.stmt) });
            } else {
                assert_eq!(SQL_NEED_DATA, unsafe {
                    crate::api::SQLExecDirectW(h.stmt, sql.as_ptr(), sql_len)
                });
            }
            let mut token = std::ptr::null_mut();
            assert_eq!(SQL_NEED_DATA, unsafe {
                crate::api::SQLParamData(h.stmt, &mut token)
            });
            assert_eq!(SQL_SUCCESS, unsafe {
                crate::api::SQLPutData(h.stmt, (&mut value as *mut u8).cast(), 1)
            });
            if mixed {
                // Collecting the buffered value opens the deferred RPC; the
                // second parameter is then streamed without replacing its token.
                assert_eq!(SQL_NEED_DATA, unsafe {
                    crate::api::SQLParamData(h.stmt, &mut token)
                });
                assert_eq!(SQL_SUCCESS, unsafe {
                    crate::api::SQLPutData(h.stmt, (&mut value as *mut u8).cast(), 1)
                });
            }
            cancel_blocked(&h, |raw| unsafe {
                crate::api::SQLParamData(raw, &mut std::ptr::null_mut())
            });
        }
    }
}
