// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use adbc_core::options::{OptionDatabase, OptionStatement};
use adbc_core::{Connection, Database, Driver, Optionable, Statement};
use adbc_driver_mssql::{MssqlDriver, options};
use arrow_array::{Int32Array, RecordBatchReader};
use mssql_mock_tds::{
    ColumnDefinition, ColumnValue, MockTdsServer, QueryResponse, Row, SqlDataType,
};

async fn server() -> (String, tokio::task::JoinHandle<()>) {
    let server = MockTdsServer::new("127.0.0.1:0").await.unwrap();
    server.query_registry().lock().await.register(
        "SELECT batch_rows",
        QueryResponse::new(
            vec![ColumnDefinition::new("value", SqlDataType::Int)],
            (0..11)
                .map(|value| Row::new(vec![ColumnValue::Int(value)]))
                .collect(),
        ),
    );
    server.query_registry().lock().await.register(
        "SELECT empty_rows",
        QueryResponse::new(
            vec![ColumnDefinition::new("value", SqlDataType::Int)],
            vec![],
        ),
    );
    let source = format!("tcp:{}", server.local_addr()).replace(':', ",");
    let source = source.replacen("tcp,", "tcp:", 1);
    let task = tokio::spawn(async move {
        server.run().await.unwrap();
    });
    (source, task)
}

fn connect(source: &str) -> adbc_driver_mssql::MssqlConnection {
    let db = MssqlDriver
        .new_database_with_opts([
            (OptionDatabase::from(options::DATA_SOURCE), source.into()),
            (OptionDatabase::Username, "test".into()),
            (OptionDatabase::Password, "test".into()),
            (OptionDatabase::from(options::ENCRYPT), "false".into()),
        ])
        .unwrap();
    db.new_connection().unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn streams_batches_and_preserves_empty_schema() {
    let (source, task) = server().await;
    let mut connection = connect(&source);
    let mut statement = connection.new_statement().unwrap();
    statement
        .set_option(OptionStatement::from(options::BATCH_ROWS), 4_i64.into())
        .unwrap();
    statement.set_sql_query("SELECT batch_rows").unwrap();
    let reader = statement.execute().unwrap();
    assert_eq!(reader.schema().field(0).name(), "value");
    let batches = reader.collect::<Result<Vec<_>, _>>().unwrap();
    assert_eq!(
        batches
            .iter()
            .map(|batch| batch.num_rows())
            .collect::<Vec<_>>(),
        [4, 4, 3]
    );
    assert_eq!(
        batches[2]
            .column(0)
            .as_any()
            .downcast_ref::<Int32Array>()
            .unwrap()
            .value(2),
        10
    );
    statement.set_sql_query("SELECT empty_rows").unwrap();
    let mut reader = statement.execute().unwrap();
    assert_eq!(reader.schema().fields().len(), 1);
    assert!(reader.next().is_none());
    drop(reader);
    drop(statement);
    drop(connection);
    task.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn another_statement_is_rejected_and_old_reader_is_invalidated() {
    let (source, task) = server().await;
    let mut connection = connect(&source);
    let mut first = connection.new_statement().unwrap();
    let mut second = connection.new_statement().unwrap();
    first.set_sql_query("SELECT batch_rows").unwrap();
    second.set_sql_query("SELECT 1").unwrap();
    let mut old = first.execute().unwrap();
    assert!(second.execute().is_err());
    first.set_sql_query("SELECT 1").unwrap();
    assert!(old.next().unwrap().is_err());
    drop(old);
    assert_eq!(
        first.execute().unwrap().next().unwrap().unwrap().num_rows(),
        1
    );
    drop(second);
    drop(first);
    drop(connection);
    task.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dropping_a_partial_stream_releases_the_connection() {
    let (source, task) = server().await;
    let mut connection = connect(&source);
    let mut statement = connection.new_statement().unwrap();
    statement
        .set_option(OptionStatement::from(options::BATCH_ROWS), 1_i64.into())
        .unwrap();
    statement.set_sql_query("SELECT batch_rows").unwrap();
    let mut reader = statement.execute().unwrap();
    reader.next().unwrap().unwrap();
    drop(reader);
    statement.set_sql_query("SELECT 1").unwrap();
    assert_eq!(statement.execute().unwrap().count(), 1);
    drop(statement);
    drop(connection);
    task.abort();
}
