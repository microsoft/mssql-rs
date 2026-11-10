// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use adbc_core::options::{OptionDatabase, OptionStatement};
use adbc_core::{Connection, Database, Driver, Optionable, Statement};
use adbc_driver_mssql::{MssqlConnection, MssqlDriver, options};
use arrow_array::{Int32Array, StringArray};

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
