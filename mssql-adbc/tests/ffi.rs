// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#![cfg(feature = "ffi")]

use adbc_core::options::{AdbcVersion, OptionDatabase};
use adbc_core::{Connection, Database, Driver, Statement};
use adbc_driver_manager::ManagedDriver;
use adbc_driver_mssql::{AdbcDriverMssqlInit, options};
use mssql_mock_tds::{
    ColumnDefinition, ColumnValue, MockTdsServer, QueryResponse, Row, SqlDataType,
};

const INIT: adbc_ffi::FFI_AdbcDriverInitFunc = AdbcDriverMssqlInit;

#[test]
fn c_driver_initialization_and_release() {
    let mut driver = ManagedDriver::load_static(&INIT, AdbcVersion::V110).unwrap();
    let database = driver.new_database().unwrap();
    assert!(database.new_connection().is_err());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn c_abi_stream_owns_handles_after_release() {
    let server = MockTdsServer::new("127.0.0.1:0").await.unwrap();
    server.query_registry().lock().await.register(
        "SELECT ffi_stream",
        QueryResponse::new(
            vec![ColumnDefinition::new("n", SqlDataType::Int)],
            vec![Row::new(vec![ColumnValue::Int(42)])],
        ),
    );
    let address = server.local_addr();
    let task = tokio::spawn(async move { server.run().await.unwrap() });
    let mut driver = ManagedDriver::load_static(&INIT, AdbcVersion::V110).unwrap();
    let database = driver
        .new_database_with_opts([
            (
                OptionDatabase::from(options::DATA_SOURCE),
                format!("tcp:{},{}", address.ip(), address.port()).into(),
            ),
            (OptionDatabase::Username, "test".into()),
            (OptionDatabase::Password, "test".into()),
            (OptionDatabase::from(options::ENCRYPT), "false".into()),
        ])
        .unwrap();
    let mut connection = database.new_connection().unwrap();
    let mut statement = connection.new_statement().unwrap();
    statement.set_sql_query("SELECT ffi_stream").unwrap();
    let reader = statement.execute().unwrap();
    drop(statement);
    drop(connection);
    drop(database);
    drop(driver);
    let batches = reader.collect::<Result<Vec<_>, _>>().unwrap();
    assert_eq!(batches[0].num_rows(), 1);
    assert_eq!(batches[0].schema().field(0).name(), "n");
    task.abort();
}

#[test]
#[ignore = "set ADBC_TEST_LIBRARY to a built shared library"]
fn shared_library_loads_and_unloads() {
    let path = std::env::var("ADBC_TEST_LIBRARY").expect("ADBC_TEST_LIBRARY");
    let mut driver = ManagedDriver::load_dynamic_from_filename(
        path,
        Some(b"AdbcDriverMssqlInit"),
        AdbcVersion::V110,
    )
    .unwrap();
    let database = driver.new_database().unwrap();
    drop(database);
    drop(driver);
}
