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

#[test]
#[ignore = "requires ADBC_TEST_SERVER/DATABASE/USER/PASSWORD"]
fn live_bulk_ingestion_modes_and_atomic_failures() {
    use adbc_core::options::OptionStatement;
    let mut connection = connection();
    let mut statement = connection.new_statement().unwrap();
    let batch = RecordBatch::try_from_iter([
        (
            "label",
            Arc::new(StringArray::from(vec![Some("a"), Some("🦀"), None])) as ArrayRef,
        ),
        ("n", Arc::new(Int32Array::from(vec![1, 2, 3])) as ArrayRef),
    ])
    .unwrap();
    statement
        .set_option(OptionStatement::TargetTable, "#adbc_ingest".into())
        .unwrap();
    statement
        .set_option(OptionStatement::Temporary, true.into())
        .unwrap();
    statement.bind(batch.clone()).unwrap();
    assert_eq!(statement.execute_update().unwrap(), Some(3));
    statement
        .set_option(
            OptionStatement::IngestMode,
            "adbc.ingest.mode.append".into(),
        )
        .unwrap();
    assert_eq!(statement.execute_update().unwrap(), Some(3));
    statement
        .set_option(
            OptionStatement::IngestMode,
            "adbc.ingest.mode.create_append".into(),
        )
        .unwrap();
    assert_eq!(statement.execute_update().unwrap(), Some(3));
    statement
        .set_option(
            OptionStatement::IngestMode,
            "adbc.ingest.mode.replace".into(),
        )
        .unwrap();
    assert_eq!(statement.execute_update().unwrap(), Some(3));
    statement
        .set_sql_query("SELECT COUNT(*) AS n FROM #adbc_ingest")
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
        3
    );
    statement
        .set_sql_query("CREATE TABLE #adbc_short(n int NOT NULL, label nvarchar(2))")
        .unwrap();
    statement.execute_update().unwrap();
    statement
        .set_option(OptionStatement::TargetTable, "#adbc_short".into())
        .unwrap();
    statement
        .set_option(
            OptionStatement::IngestMode,
            "adbc.ingest.mode.append".into(),
        )
        .unwrap();
    statement
        .bind(
            RecordBatch::try_from_iter([
                (
                    "label",
                    Arc::new(StringArray::from(vec!["ok", "too long"])) as ArrayRef,
                ),
                ("n", Arc::new(Int32Array::from(vec![1, 2])) as ArrayRef),
            ])
            .unwrap(),
        )
        .unwrap();
    assert!(statement.execute_update().is_err());
    statement
        .set_sql_query("SELECT COUNT(*) AS n FROM #adbc_short")
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
    statement
        .set_sql_query("DROP TABLE #adbc_short; DROP TABLE #adbc_ingest")
        .unwrap();
    statement.execute_update().unwrap();
}

#[test]
#[ignore = "requires ADBC_TEST_SERVER/DATABASE/USER/PASSWORD"]
fn live_cancel_timeout_and_error_recovery() {
    use adbc_core::error::Status;
    let mut connection = connection();
    let mut statement = connection.new_statement().unwrap();
    statement
        .set_sql_query("WAITFOR DELAY '00:00:10'; SELECT 1")
        .unwrap();
    let cancel = statement.get_cancel_handle();
    let worker = std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(300));
        cancel.try_cancel().unwrap();
    });
    let error = statement.execute().err().expect("query must be cancelled");
    worker.join().unwrap();
    assert_eq!(error.status, Status::Cancelled);
    statement
        .set_option(OptionStatement::from(options::QUERY_TIMEOUT), 1_i64.into())
        .unwrap();
    let error = statement.execute().err().expect("query must time out");
    assert_eq!(error.status, Status::Timeout);
    statement
        .set_sql_query("SELECT CAST(1 AS int) AS n; RAISERROR('adbc late error',16,1)")
        .unwrap();
    assert!(
        statement
            .execute()
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .is_err()
    );
    statement
        .set_sql_query("SELECT CAST(42 AS int) AS n")
        .unwrap();
    assert_eq!(
        statement
            .execute()
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap()[0]
            .num_rows(),
        1
    );
}

struct LiveTable {
    connection: MssqlConnection,
    name: String,
}

impl Drop for LiveTable {
    fn drop(&mut self) {
        let cleanup = (|| {
            let mut statement = self.connection.new_statement()?;
            statement.set_sql_query(format!(
                "DROP TABLE IF EXISTS [dbo].{}",
                mssql_tds::escape_identifier(&self.name)
            ))?;
            statement.execute_update()
        })();
        if let Err(error) = cleanup {
            eprintln!("owned live-test table cleanup failed: {error}");
        }
    }
}

#[test]
#[ignore = "requires ADBC_TEST_SERVER/DATABASE/USER/PASSWORD"]
fn live_nested_columns_and_constraints() {
    use adbc_core::options::ObjectDepth;
    use arrow_array::{ListArray, StructArray};
    let mut table = LiveTable {
        connection: connection(),
        name: format!("adbc_test_{}", uuid::Uuid::new_v4().simple()),
    };
    let mut statement = table.connection.new_statement().unwrap();
    statement.set_sql_query(format!("CREATE TABLE dbo.[{}](n int PRIMARY KEY, parent int REFERENCES dbo.[{}](n), label nvarchar(10) UNIQUE, CHECK(n>0))", table.name, table.name)).unwrap();
    statement.execute_update().unwrap();
    let schema = table
        .connection
        .get_table_schema(None, Some("dbo"), &table.name)
        .unwrap();
    assert_eq!(schema.fields().len(), 3);
    let batches = table
        .connection
        .get_objects(
            ObjectDepth::All,
            None,
            Some("dbo"),
            Some(&table.name),
            None,
            None,
        )
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    let schemas = batches[0]
        .column(1)
        .as_any()
        .downcast_ref::<ListArray>()
        .unwrap()
        .value(0);
    let schemas = schemas.as_any().downcast_ref::<StructArray>().unwrap();
    let tables = schemas
        .column_by_name("db_schema_tables")
        .unwrap()
        .as_any()
        .downcast_ref::<ListArray>()
        .unwrap()
        .value(0);
    let tables = tables.as_any().downcast_ref::<StructArray>().unwrap();
    let columns = tables
        .column_by_name("table_columns")
        .unwrap()
        .as_any()
        .downcast_ref::<ListArray>()
        .unwrap()
        .value(0);
    assert_eq!(columns.len(), 3);
    let constraints = tables
        .column_by_name("table_constraints")
        .unwrap()
        .as_any()
        .downcast_ref::<ListArray>()
        .unwrap()
        .value(0);
    assert_eq!(constraints.len(), 4);
}

#[test]
#[ignore = "requires ADBC_TEST_SERVER/DATABASE/USER/PASSWORD"]
fn live_wide_parameter_and_bulk_type_roundtrip() {
    use arrow_array::*;
    let mut connection = connection();
    let mut statement = connection.new_statement().unwrap();
    let decimal = Decimal128Array::from(vec![Some(-1234567), None])
        .with_precision_and_scale(38, 7)
        .unwrap();
    let uuid = FixedSizeBinaryArray::try_from_sparse_iter_with_size(
        [Some(&[1_u8; 16][..]), None].into_iter(),
        16,
    )
    .unwrap();
    let batch = RecordBatch::try_from_iter(vec![
        (
            "bit",
            Arc::new(BooleanArray::from(vec![Some(true), None])) as ArrayRef,
        ),
        ("tiny", Arc::new(UInt8Array::from(vec![Some(255), None]))),
        (
            "small",
            Arc::new(Int16Array::from(vec![Some(-32768), None])),
        ),
        (
            "int",
            Arc::new(Int32Array::from(vec![Some(i32::MIN), None])),
        ),
        (
            "big",
            Arc::new(Int64Array::from(vec![Some(i64::MAX), None])),
        ),
        ("real", Arc::new(Float32Array::from(vec![Some(1.25), None]))),
        (
            "float",
            Arc::new(Float64Array::from(vec![Some(-1.25), None])),
        ),
        ("decimal", Arc::new(decimal)),
        (
            "text",
            Arc::new(StringArray::from(vec![Some("a\u{03b2}"), None])),
        ),
        (
            "binary",
            Arc::new(BinaryArray::from(vec![Some(&[0_u8, 255][..]), None])),
        ),
        ("uuid", Arc::new(uuid)),
        (
            "date",
            Arc::new(Date32Array::from(vec![Some(-719162), None])),
        ),
        (
            "time",
            Arc::new(Time64NanosecondArray::from(vec![
                Some(86_399_999_999_900),
                None,
            ])),
        ),
        (
            "datetime",
            Arc::new(TimestampMicrosecondArray::from(vec![Some(-1), None])),
        ),
        (
            "offset",
            Arc::new(TimestampMicrosecondArray::from(vec![Some(-1), None]).with_timezone("UTC")),
        ),
    ])
    .unwrap();
    let query = batch
        .schema()
        .fields()
        .iter()
        .enumerate()
        .map(|(index, field)| {
            format!(
                "@p{} AS {}",
                index + 1,
                mssql_tds::escape_identifier(field.name())
            )
        })
        .collect::<Vec<_>>()
        .join(",");
    statement.set_sql_query(format!("SELECT {query}")).unwrap();
    statement.prepare().unwrap();
    statement.bind(batch.clone()).unwrap();
    let query = statement
        .execute()
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    assert_eq!(query.len(), 1);
    assert_eq!(query[0].num_rows(), 2);
    statement
        .set_option(OptionStatement::TargetTable, "#adbc_wide".into())
        .unwrap();
    statement
        .set_option(OptionStatement::Temporary, true.into())
        .unwrap();
    assert_eq!(statement.execute_update().unwrap(), Some(2));
    statement
        .set_sql_query(
            "SELECT * FROM #adbc_wide ORDER BY CASE WHEN [int] IS NULL THEN 1 ELSE 0 END",
        )
        .unwrap();
    let bulk = statement
        .execute()
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    assert_eq!(bulk[0].num_rows(), 2);
    for (index, column) in bulk[0].columns().iter().enumerate() {
        assert_eq!(column.to_data(), query[0].column(index).to_data());
    }
    statement.set_sql_query("DROP TABLE #adbc_wide").unwrap();
    statement.execute_update().unwrap();
}

#[test]
#[ignore = "requires ADBC_TEST_SERVER/DATABASE/USER/PASSWORD"]
fn live_bound_stream_and_replace_rollback() {
    use arrow_array::{Decimal128Array, RecordBatchIterator};
    let mut connection = connection();
    let mut statement = connection.new_statement().unwrap();
    let batch = RecordBatch::try_from_iter([(
        "d",
        Arc::new(
            Decimal128Array::from(vec![Some(7), None])
                .with_precision_and_scale(38, 7)
                .unwrap(),
        ) as ArrayRef,
    )])
    .unwrap();
    statement.set_sql_query("SELECT @p1 AS d").unwrap();
    statement
        .bind_stream(Box::new(RecordBatchIterator::new(
            [
                Ok(RecordBatch::new_empty(batch.schema())),
                Ok(batch.clone()),
                Ok(batch.clone()),
            ],
            batch.schema(),
        )))
        .unwrap();
    assert_eq!(
        statement
            .execute()
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap()
            .iter()
            .map(RecordBatch::num_rows)
            .sum::<usize>(),
        4
    );
    assert!(statement.execute().is_err());
    statement
        .set_option(OptionStatement::TargetTable, "#adbc_replace".into())
        .unwrap();
    statement
        .set_option(OptionStatement::Temporary, true.into())
        .unwrap();
    statement.bind(batch.clone()).unwrap();
    statement.execute_update().unwrap();
    statement
        .set_option(
            OptionStatement::IngestMode,
            "adbc.ingest.mode.replace".into(),
        )
        .unwrap();
    statement
        .bind_stream(Box::new(RecordBatchIterator::new(
            [
                Ok(batch.clone()),
                Ok(batch.clone()),
                Err(arrow_schema::ArrowError::ComputeError(
                    "injected input failure".into(),
                )),
            ],
            batch.schema(),
        )))
        .unwrap();
    assert!(statement.execute_update().is_err());
    statement
        .set_sql_query("SELECT COUNT(*) AS n FROM #adbc_replace")
        .unwrap();
    let result = statement
        .execute()
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    assert_eq!(
        result[0]
            .column(0)
            .as_any()
            .downcast_ref::<Int32Array>()
            .unwrap()
            .value(0),
        2
    );
    statement.set_sql_query("DROP TABLE #adbc_replace").unwrap();
    statement.execute_update().unwrap();
}
