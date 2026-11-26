// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use mssql_mock_tds::protocol::{
    build_done_token, build_feature_ext_ack, build_login_ack, build_prelogin_response,
    build_query_result,
};
use mssql_mock_tds::{
    ColumnDefinition, ColumnValue, QueryResponse, Row, SqlDataType, TerminalError,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use super::*;
use crate::api::odbc_types::*;
use crate::api::{SQLColumnsW, SQLFetch, SQLFreeStmt, SQLGetData, SQLGetTypeInfoW};
use crate::test_support::TestHandles;

struct Server {
    runtime: tokio::runtime::Runtime,
    task: tokio::task::JoinHandle<()>,
    address: std::net::SocketAddr,
    requests: Arc<Mutex<Vec<String>>>,
    responses: Arc<Mutex<VecDeque<Vec<u8>>>>,
}

fn packet(body: &[u8]) -> Vec<u8> {
    let length = u16::try_from(body.len() + 8).unwrap().to_be_bytes();
    let mut result = vec![4, 1, length[0], length[1], 0, 0, 1, 0];
    result.extend_from_slice(body);
    result
}

impl Server {
    fn start(vector: bool, json: bool) -> Self {
        Self::start_recoverable(vector, json, vector)
    }

    fn start_recoverable(vector: bool, json: bool, recovered_vector: bool) -> Self {
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let responses = Arc::new(Mutex::new(VecDeque::<Vec<u8>>::new()));
        let (address, task) = runtime.block_on(async {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = listener.local_addr().unwrap();
            let requests = Arc::clone(&requests);
            let responses = Arc::clone(&responses);
            let task = tokio::spawn(async move {
                let mut connection = 0;
                loop {
                    let (mut socket, _) = listener.accept().await.unwrap();
                    let vector = if connection == 0 {
                        vector
                    } else {
                        recovered_vector
                    };
                    connection += 1;
                    let requests = Arc::clone(&requests);
                    let responses = Arc::clone(&responses);
                    tokio::spawn(async move {
                        let mut body = Vec::new();
                        loop {
                            let mut header = [0; 8];
                            if socket.read_exact(&mut header).await.is_err() {
                                return;
                            }
                            let length = usize::from(u16::from_be_bytes([header[2], header[3]]));
                            let start = body.len();
                            body.resize(start + length - 8, 0);
                            socket.read_exact(&mut body[start..]).await.unwrap();
                            if header[1] & 1 == 0 {
                                continue;
                            }
                            let response = match header[0] {
                                0x12 => build_prelogin_response().to_vec(),
                                0x10 => {
                                    let mut login = build_login_ack().to_vec();
                                    let mut features = vec![(0x01, Vec::new())];
                                    if vector {
                                        features.push((0x0E, vec![1]));
                                    }
                                    if json {
                                        features.push((0x0D, vec![1]));
                                    }
                                    login.extend_from_slice(&build_feature_ext_ack(&features));
                                    login.extend_from_slice(&build_done_token(0));
                                    packet(&login)
                                }
                                0x03 => {
                                    let all_headers = usize::try_from(u32::from_le_bytes(
                                        body[..4].try_into().unwrap(),
                                    ))
                                    .unwrap();
                                    let length = usize::from(u16::from_le_bytes(
                                        body[all_headers..all_headers + 2].try_into().unwrap(),
                                    ));
                                    let units = body[all_headers + 2..all_headers + 2 + length * 2]
                                        .as_chunks::<2>()
                                        .0
                                        .iter()
                                        .map(|unit| u16::from_le_bytes([unit[0], unit[1]]))
                                        .collect::<Vec<_>>();
                                    requests
                                        .lock()
                                        .unwrap()
                                        .push(String::from_utf16(&units).unwrap());
                                    responses
                                        .lock()
                                        .unwrap()
                                        .pop_front()
                                        .expect("unexpected catalog RPC")
                                }
                                0x06 => {
                                    mssql_mock_tds::protocol::build_attention_ack_packet().to_vec()
                                }
                                _ => packet(&build_done_token(0)),
                            };
                            socket.write_all(&response).await.unwrap();
                            body.clear();
                        }
                    });
                }
            });
            (address, task)
        });
        Self {
            runtime,
            task,
            address,
            requests,
            responses,
        }
    }

    fn connect(&self, h: &TestHandles) {
        let text = crate::test_support::cs(&format!(
            "Server=tcp:{},{};UID=mock;<PW>=unused;Database=master;Encrypt=no",
            self.address.ip(),
            self.address.port()
        ))
        .encode_utf16()
        .chain(Some(0))
        .collect::<Vec<_>>();
        assert_eq!(
            unsafe {
                crate::api::SQLDriverConnectW(
                    h.dbc,
                    std::ptr::null_mut(),
                    text.as_ptr(),
                    SQL_NTS,
                    std::ptr::null_mut(),
                    0,
                    std::ptr::null_mut(),
                    SQL_DRIVER_NOPROMPT,
                )
            },
            SQL_SUCCESS
        );
    }

    fn queue(&self, response: QueryResponse) {
        self.responses
            .lock()
            .unwrap()
            .push_back(build_query_result(&response).to_vec());
    }

    fn queue_database_change(&self, database: &str, response: QueryResponse) {
        let units = database.encode_utf16().collect::<Vec<_>>();
        let mut change = vec![1, u8::try_from(units.len()).unwrap()];
        change.extend(units.iter().flat_map(|unit| unit.to_le_bytes()));
        change.push(0);
        let mut body = vec![0xE3];
        body.extend_from_slice(&u16::try_from(change.len()).unwrap().to_le_bytes());
        body.extend_from_slice(&change);
        body.extend_from_slice(&build_query_result(&response)[8..]);
        self.responses.lock().unwrap().push_back(packet(&body));
    }

    fn calls(&self) -> Vec<String> {
        self.requests.lock().unwrap().clone()
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.task.abort();
        self.runtime.block_on(async {
            let _ = (&mut self.task).await;
        });
    }
}

fn failure(number: u32) -> QueryResponse {
    QueryResponse::error_only(TerminalError::new(number, 16, "catalog error"))
}

fn empty(count: usize) -> QueryResponse {
    QueryResponse::new(
        (0..count)
            .map(|i| ColumnDefinition::new(format!("c{i}"), SqlDataType::Int))
            .collect(),
        Vec::new(),
    )
}

fn columns(h: &TestHandles, catalog: Option<&str>) -> SqlReturn {
    let text = catalog.map(|value| value.encode_utf16().chain(Some(0)).collect::<Vec<_>>());
    unsafe {
        SQLColumnsW(
            h.stmt,
            text.as_ref().map_or(std::ptr::null(), |text| text.as_ptr()),
            if catalog.is_some() { SQL_NTS } else { 0 },
            std::ptr::null(),
            0,
            std::ptr::null(),
            0,
            std::ptr::null(),
            0,
        )
    }
}

fn close(h: &TestHandles) {
    assert_eq!(unsafe { SQLFreeStmt(h.stmt, SQL_CLOSE) }, SQL_SUCCESS);
}

#[test]
fn vector_ack_selects_modern_catalogs_independently_of_json_ack() {
    for (vector, json) in [(false, false), (false, true), (true, false), (true, true)] {
        let server = Server::start(vector, json);
        let h = TestHandles::with_env_dbc_stmt();
        server.connect(&h);
        server.queue(empty(18));
        assert_eq!(columns(&h, None), SQL_SUCCESS);
        assert_eq!(unsafe { SQLFetch(h.stmt) }, SQL_NO_DATA);
        close(&h);
        server.queue(empty(19));
        assert_eq!(unsafe { SQLGetTypeInfoW(h.stmt, SQL_SS_JSON) }, SQL_SUCCESS);
        assert_eq!(unsafe { SQLFetch(h.stmt) }, SQL_NO_DATA);
        let suffix = if vector { "170" } else { "100" };
        assert_eq!(
            server.calls(),
            [
                format!("[sys].sp_columns_{suffix}"),
                format!("[sys].sp_datatype_info_{suffix}"),
            ]
        );
    }
}

#[test]
fn missing_columns_are_cached_per_exact_database_and_independently_of_type_info() {
    let server = Server::start(true, true);
    let h = TestHandles::with_env_dbc_stmt();
    server.connect(&h);
    server.queue(failure(2812));
    server.queue(empty(18));
    assert_eq!(columns(&h, Some("db]a")), SQL_SUCCESS);
    close(&h);
    for database in ["db_b", "db]a"] {
        server.queue(empty(18));
        assert_eq!(columns(&h, Some(database)), SQL_SUCCESS);
        close(&h);
    }
    server.queue(empty(19));
    assert_eq!(
        unsafe { SQLGetTypeInfoW(h.stmt, SQL_ALL_TYPES) },
        SQL_SUCCESS
    );
    assert_eq!(
        server.calls(),
        [
            "[db]]a].sys.sp_columns_170",
            "[db]]a].sys.sp_columns_100",
            "[db_b].sys.sp_columns_170",
            "[db]]a].sys.sp_columns_100",
            "[sys].sp_datatype_info_170",
        ]
    );
}

#[test]
fn permission_errors_do_not_fallback_or_poison_the_cache() {
    let server = Server::start(true, true);
    let h = TestHandles::with_env_dbc_stmt();
    server.connect(&h);
    for _ in 0..2 {
        server.queue(failure(229));
        assert_eq!(unsafe { SQLGetTypeInfoW(h.stmt, SQL_SS_JSON) }, SQL_ERROR);
        let state =
            unsafe { crate::handles::handle_from_raw::<crate::handles::StmtHandle>(h.stmt) }
                .inner
                .lock()
                .unwrap();
        assert_eq!(state.diag_records.last().unwrap().native_error, 229);
    }
    assert_eq!(server.calls(), ["[sys].sp_datatype_info_170"; 2]);
}

#[test]
fn empty_legacy_type_info_is_not_replaced_with_a_synthetic_json_row() {
    let server = Server::start(true, true);
    let h = TestHandles::with_env_dbc_stmt();
    server.connect(&h);
    server.queue(failure(2812));
    server.queue(empty(19));
    assert_eq!(unsafe { SQLGetTypeInfoW(h.stmt, SQL_SS_JSON) }, SQL_SUCCESS);
    assert_eq!(unsafe { SQLFetch(h.stmt) }, SQL_NO_DATA);
    close(&h);
    server.queue(empty(19));
    assert_eq!(unsafe { SQLGetTypeInfoW(h.stmt, SQL_SS_JSON) }, SQL_SUCCESS);
    assert_eq!(unsafe { SQLFetch(h.stmt) }, SQL_NO_DATA);
    assert_eq!(
        server.calls(),
        [
            "[sys].sp_datatype_info_170",
            "[sys].sp_datatype_info_100",
            "[sys].sp_datatype_info_100",
        ]
    );
}

#[test]
fn modern_type_info_preserves_json_size_and_case_sensitivity() {
    let server = Server::start(true, true);
    let h = TestHandles::with_env_dbc_stmt();
    server.connect(&h);
    let mut response = empty(19);
    response.columns[0] = ColumnDefinition::new("TYPE_NAME", SqlDataType::NVarChar);
    let mut values = vec![ColumnValue::Int(0); 19];
    values[0] = ColumnValue::NVarChar("json".into());
    values[1] = ColumnValue::Int(i32::from(SQL_SS_JSON));
    values[2] = ColumnValue::Int(i32::MAX);
    values[7] = ColumnValue::Int(1);
    response.rows.push(Row::new(values));
    server.queue(response);
    assert_eq!(unsafe { SQLGetTypeInfoW(h.stmt, SQL_SS_JSON) }, SQL_SUCCESS);
    assert_eq!(unsafe { SQLFetch(h.stmt) }, SQL_SUCCESS);
    for (ordinal, expected) in [(2, i32::from(SQL_SS_JSON)), (3, i32::MAX), (8, 1)] {
        let mut value = 0i32;
        let mut indicator = 0;
        assert_eq!(
            unsafe {
                SQLGetData(
                    h.stmt,
                    ordinal,
                    SQL_C_SLONG,
                    (&mut value as *mut i32).cast(),
                    4,
                    &mut indicator,
                )
            },
            SQL_SUCCESS
        );
        assert_eq!(value, expected);
        assert_eq!(indicator, 4);
    }
    assert_eq!(unsafe { SQLFetch(h.stmt) }, SQL_NO_DATA);
}

#[test]
fn recovery_cache_forgets_old_sessions_but_preserves_database_entries_within_a_session() {
    let mut cache = CatalogProcedureCache::default();
    let first = uuid::Uuid::new_v4();
    let next = uuid::Uuid::new_v4();
    assert!(!cache.select(first, "master", Procedure::Columns));
    cache.remember(first, "master".into(), Procedure::Columns);
    assert!(!cache.select(first, "db_b", Procedure::Columns));
    assert!(cache.select(first, "master", Procedure::Columns));
    assert!(!cache.select(first, "master", Procedure::TypeInfo));
    assert!(!cache.select(next, "master", Procedure::Columns));
    cache.remember(first, "master".into(), Procedure::Columns);
    assert!(!cache.select(next, "master", Procedure::Columns));
}

#[test]
fn session_recovery_reselects_catalogs_before_sending_on_the_new_session() {
    for recovered_vector in [false, true] {
        let server = Server::start_recoverable(true, true, recovered_vector);
        let h = TestHandles::with_env_dbc_stmt();
        server.connect(&h);
        server.queue(failure(2812));
        server.queue(empty(19));
        assert_eq!(unsafe { SQLGetTypeInfoW(h.stmt, SQL_SS_JSON) }, SQL_SUCCESS);
        assert_eq!(unsafe { SQLFetch(h.stmt) }, SQL_NO_DATA);
        close(&h);
        let dbc = unsafe { crate::handles::handle_from_raw::<DbcHandle>(h.dbc) };
        dbc.inner
            .lock()
            .unwrap()
            .wire
            .client_mut_for_test()
            .unwrap()
            .mark_connection_dead();
        server.queue(empty(19));
        assert_eq!(unsafe { SQLGetTypeInfoW(h.stmt, SQL_SS_JSON) }, SQL_SUCCESS);
        assert_eq!(unsafe { SQLFetch(h.stmt) }, SQL_NO_DATA);
        assert_eq!(
            server.calls(),
            [
                "[sys].sp_datatype_info_170",
                "[sys].sp_datatype_info_100",
                if recovered_vector {
                    "[sys].sp_datatype_info_170"
                } else {
                    "[sys].sp_datatype_info_100"
                },
            ]
        );
    }
}

#[test]
fn reconnecting_the_same_odbc_handle_forgets_missing_catalog_procedures() {
    let server = Server::start(true, true);
    let mut h = TestHandles::with_env_dbc();
    server.connect(&h);
    let stmt = h.alloc_extra_stmt();
    server.queue(failure(2812));
    server.queue(empty(19));
    assert_eq!(unsafe { SQLGetTypeInfoW(stmt, SQL_SS_JSON) }, SQL_SUCCESS);
    assert_eq!(unsafe { SQLFetch(stmt) }, SQL_NO_DATA);
    assert_eq!(h.free_extra_stmt(stmt), SQL_SUCCESS);
    assert_eq!(unsafe { crate::api::SQLDisconnect(h.dbc) }, SQL_SUCCESS);
    server.connect(&h);
    let stmt = h.alloc_extra_stmt();
    server.queue(empty(19));
    assert_eq!(unsafe { SQLGetTypeInfoW(stmt, SQL_SS_JSON) }, SQL_SUCCESS);
    assert_eq!(unsafe { SQLFetch(stmt) }, SQL_NO_DATA);
    assert_eq!(
        server.calls(),
        [
            "[sys].sp_datatype_info_170",
            "[sys].sp_datatype_info_100",
            "[sys].sp_datatype_info_170",
        ]
    );
}

#[test]
fn an_empty_modern_catalog_is_not_cached_as_missing() {
    let server = Server::start(true, true);
    let h = TestHandles::with_env_dbc_stmt();
    server.connect(&h);
    for _ in 0..2 {
        server.queue(empty(19));
        assert_eq!(unsafe { SQLGetTypeInfoW(h.stmt, SQL_SS_JSON) }, SQL_SUCCESS);
        assert_eq!(unsafe { SQLFetch(h.stmt) }, SQL_NO_DATA);
        close(&h);
    }

    assert_eq!(server.calls(), ["[sys].sp_datatype_info_170"; 2]);
}

#[test]
fn database_envchange_selects_and_restores_the_effective_database_cache() {
    let server = Server::start(true, true);
    let h = TestHandles::with_env_dbc_stmt();
    server.connect(&h);
    server.queue(failure(2812));
    server.queue(empty(18));
    assert_eq!(columns(&h, None), SQL_SUCCESS);
    close(&h);
    for database in ["db_b", "master"] {
        server.queue_database_change(database, empty(18));
        assert_eq!(columns(&h, None), SQL_SUCCESS);
        close(&h);
    }
    server.queue(empty(18));
    assert_eq!(columns(&h, None), SQL_SUCCESS);
    assert_eq!(
        server.calls(),
        [
            "[sys].sp_columns_170",
            "[sys].sp_columns_100",
            "[sys].sp_columns_100",
            "[sys].sp_columns_170",
            "[sys].sp_columns_100",
        ]
    );
}

#[test]
fn missing_modern_with_database_change_still_retries_legacy() {
    let server = Server::start(true, true);
    let h = TestHandles::with_env_dbc_stmt();
    server.connect(&h);
    server.queue_database_change("db_b", failure(2812));
    server.queue(empty(18));
    assert_eq!(columns(&h, None), SQL_SUCCESS);
    close(&h);
    server.queue(empty(18));
    assert_eq!(columns(&h, None), SQL_SUCCESS);
    assert_eq!(
        server.calls(),
        [
            "[sys].sp_columns_170",
            "[sys].sp_columns_100",
            "[sys].sp_columns_170",
        ]
    );
}
