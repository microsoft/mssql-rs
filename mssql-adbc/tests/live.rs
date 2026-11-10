// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use adbc_core::options::{OptionDatabase, OptionStatement};
use adbc_core::{Connection, Database, Driver, Optionable, Statement};
use adbc_driver_mssql::{MssqlConnection, MssqlDriver, options};
use arrow_array::{ArrayRef, Int32Array, RecordBatch, StringArray};
use std::sync::Arc;

fn connection() -> MssqlConnection {
    dotenv::dotenv().ok();
    let get = |name| std::env::var(name).unwrap_or_else(|_| panic!("missing {name}"));
    let db = MssqlDriver
        .new_database_with_opts([
            (
                OptionDatabase::from(options::DATA_SOURCE),
                get("ADBC_TEST_SERVER").into(),
            ),
            (
                OptionDatabase::from(options::DATABASE),
                get("ADBC_TEST_DATABASE").into(),
            ),
            (OptionDatabase::Username, get("ADBC_TEST_USER").into()),
            (OptionDatabase::Password, get("ADBC_TEST_PASSWORD").into()),
        ])
        .unwrap();
    db.new_connection().unwrap()
}

#[test]
#[ignore = "requires ADBC_TEST_SERVER/DATABASE/USER/PASSWORD"]
fn live_streaming_and_types() {
    let mut connection = connection();
    let mut statement = connection.new_statement().unwrap();
    statement
        .set_option(OptionStatement::from(options::BATCH_ROWS), 3_i64.into())
        .unwrap();
    statement.set_sql_query("SELECT DB_NAME() AS db, CAST(42 AS int) AS n, CAST(123.456 AS decimal(38,3)) AS d, CAST('9999-12-31T23:59:59.9999999' AS datetime2(7)) AS dt, CAST('2024-01-02T03:04:05.1234567+05:30' AS datetimeoffset(7)) AS dto, CAST(NULL AS nvarchar(10)) AS null_text").unwrap();
    let batches = statement
        .execute()
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    assert_eq!(batches.len(), 1);
    let batch = &batches[0];
    assert_eq!(
        batch
            .column(0)
            .as_any()
            .downcast_ref::<StringArray>()
            .unwrap()
            .value(0),
        std::env::var("ADBC_TEST_DATABASE").unwrap()
    );
    assert_eq!(
        batch
            .column(1)
            .as_any()
            .downcast_ref::<Int32Array>()
            .unwrap()
            .value(0),
        42
    );
}

#[test]
#[ignore = "requires ADBC_TEST_SERVER/DATABASE/USER/PASSWORD"]
fn live_prepared_binding_and_empty_input() {
    let mut connection = connection();
    let mut statement = connection.new_statement().unwrap();
    statement
        .set_sql_query("SELECT CAST(@p1 AS int) AS n")
        .unwrap();
    statement.prepare().unwrap();
    assert_eq!(statement.get_parameter_schema().unwrap().fields().len(), 1);
    statement
        .bind(
            RecordBatch::try_from_iter([(
                "input",
                Arc::new(Int32Array::from(vec![10, 20, 30])) as ArrayRef,
            )])
            .unwrap(),
        )
        .unwrap();
    for _ in 0..2 {
        let batches = statement
            .execute()
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        let values = batches
            .iter()
            .flat_map(|batch| {
                batch
                    .column(0)
                    .as_any()
                    .downcast_ref::<Int32Array>()
                    .unwrap()
                    .values()
                    .iter()
                    .copied()
            })
            .collect::<Vec<_>>();
        assert_eq!(values, [10, 20, 30]);
    }
    statement
        .bind(
            RecordBatch::try_from_iter([(
                "input",
                Arc::new(Int32Array::from(Vec::<i32>::new())) as ArrayRef,
            )])
            .unwrap(),
        )
        .unwrap();
    let reader = statement.execute().unwrap();
    assert_eq!(reader.schema().fields().len(), 1);
    assert_eq!(reader.count(), 0);
}

#[test]
#[ignore = "requires ADBC_TEST_SERVER/DATABASE/USER/PASSWORD"]
fn live_transactions() {
    use adbc_core::options::OptionConnection;
    let mut connection = connection();
    assert!(connection.commit().is_err());
    let mut statement = connection.new_statement().unwrap();
    statement
        .set_sql_query("CREATE TABLE #adbc_tx(n int NOT NULL)")
        .unwrap();
    statement.execute_update().unwrap();
    connection
        .set_option(OptionConnection::AutoCommit, false.into())
        .unwrap();
    statement
        .set_sql_query("INSERT INTO #adbc_tx VALUES (@p1)")
        .unwrap();
    statement.prepare().unwrap();
    statement
        .bind(
            RecordBatch::try_from_iter([("n", Arc::new(Int32Array::from(vec![1, 2])) as ArrayRef)])
                .unwrap(),
        )
        .unwrap();
    assert_eq!(statement.execute_update().unwrap(), Some(2));
    connection.rollback().unwrap();
    statement
        .set_sql_query("SELECT COUNT(*) AS n FROM #adbc_tx")
        .unwrap();
    let batches = statement
        .execute()
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    assert_eq!(
        batches[0]
            .column(0)
            .as_any()
            .downcast_ref::<Int32Array>()
            .unwrap()
            .value(0),
        0
    );
    connection.commit().unwrap();
    connection
        .set_option(OptionConnection::AutoCommit, true.into())
        .unwrap();
    statement.set_sql_query("DROP TABLE #adbc_tx").unwrap();
    statement.execute_update().unwrap();
}

#[test]
#[ignore = "requires ADBC_TEST_SERVER/DATABASE/USER/PASSWORD"]
fn live_catalog_and_schema_description() {
    use adbc_core::options::ObjectDepth;
    let mut connection = connection();
    let info = connection
        .get_info(None)
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    assert_eq!(
        info[0].schema(),
        adbc_core::schemas::GET_INFO_SCHEMA.clone()
    );
    assert!(info[0].num_rows() >= 5);
    for depth in [
        ObjectDepth::Catalogs,
        ObjectDepth::Schemas,
        ObjectDepth::Tables,
        ObjectDepth::All,
    ] {
        let objects = connection
            .get_objects(
                depth,
                Some("drivers"),
                Some("dbo"),
                Some("adbc_nonexistent_%"),
                None,
                None,
            )
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        assert_eq!(
            objects[0].schema(),
            adbc_core::schemas::GET_OBJECTS_SCHEMA.clone()
        );
    }
    let mut statement = connection.new_statement().unwrap();
    statement
        .set_sql_query("SELECT CAST(NULL AS decimal(38,7)) AS d, CAST(NULL AS datetime2(7)) AS t")
        .unwrap();
    let schema = statement.execute_schema().unwrap();
    assert_eq!(
        schema.field(0).data_type(),
        &arrow_schema::DataType::Decimal128(38, 7)
    );
    assert_eq!(schema.fields().len(), 2);
}
