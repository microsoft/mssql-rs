// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.
// Portions derived from saurabh500/mssql-tiberius-bridge (MIT).

//! Integration tests requiring a live SQL Server instance. Connection settings
//! come from the shared test environment described in `tests/common/mod.rs`.

pub mod common;

use mssql_rs::{Client, TdsManager};

#[tokio::test]
async fn long_native_string_and_json_parameters_roundtrip() {
    let cfg = common::live_config_or_exit();
    let mut client = Client::connect(&cfg).await.expect("connect failed");
    let text = "😀".repeat(2001);
    let json = serde_json::json!([text]);
    let expected_json = json.to_string();
    let rows = client
        .query(
            "SELECT @P1 AS text_value, @P2 AS json_value",
            &[&text, &json],
        )
        .await
        .expect("query failed")
        .into_first_result();
    let row = rows.first().expect("expected result row");
    assert_eq!(row.get::<String, _>("text_value"), Some(text));
    assert_eq!(row.get::<String, _>("json_value"), Some(expected_json));
}

#[tokio::test]
async fn connect_and_select_one() {
    let cfg = common::live_config_or_exit();
    let mut client = Client::connect(&cfg).await.expect("connect failed");

    let rows = client
        .simple_query("SELECT 1 AS value")
        .await
        .expect("query failed")
        .into_first_result();

    assert_eq!(rows.len(), 1);
    let val: i32 = rows
        .first()
        .expect("expected row at index 0")
        .get("value")
        .expect("column 'value' not found");
    assert_eq!(val, 1);
}

#[tokio::test]
async fn decimal_column_exposes_precision_and_scale() {
    let cfg = common::live_config_or_exit();
    let mut client = Client::connect(&cfg).await.expect("connect failed");
    let rows = client
        .simple_query("SELECT CAST(1 AS decimal(18, 4)) AS amount")
        .await
        .expect("decimal query failed")
        .into_first_result();
    let column = rows
        .first()
        .expect("expected decimal row")
        .columns()
        .first()
        .expect("expected decimal column");
    assert_eq!(column.precision(), Some(18));
    assert_eq!(column.scale(), Some(4));
}

#[tokio::test]
async fn image_column_exposes_source_table() {
    let cfg = common::live_config_or_exit();
    let mut client = Client::connect(&cfg).await.expect("connect failed");
    client
        .simple_query(
            "CREATE TABLE #driver_metadata_source (value IMAGE NULL); \
             INSERT INTO #driver_metadata_source (value) VALUES (0x01);",
        )
        .await
        .expect("create metadata source");
    let rows = client
        .simple_query("SELECT value FROM #driver_metadata_source")
        .await
        .expect("select metadata source")
        .into_first_result();
    let source = rows
        .first()
        .expect("expected image row")
        .columns()
        .first()
        .expect("expected image column")
        .multi_part_name()
        .expect("expected source table name");
    assert!(source.table_name.starts_with("#driver_metadata_source"));
}

#[tokio::test]
async fn select_multiple_types() {
    let cfg = common::live_config_or_exit();
    let mut client = Client::connect(&cfg).await.expect("connect failed");

    let rows = client
        .simple_query(
            "SELECT \
                CAST(42 AS int) AS int_col, \
                CAST(1 AS bit) AS bit_col, \
                CAST(3.14 AS float) AS float_col, \
                CAST('hello' AS nvarchar(50)) AS str_col, \
                NEWID() AS guid_col",
        )
        .await
        .expect("query failed")
        .into_first_result();

    assert_eq!(rows.len(), 1);
    assert_eq!(
        rows.first()
            .expect("expected row at index 0")
            .get::<i32, _>("int_col"),
        Some(42)
    );
    assert_eq!(
        rows.first()
            .expect("expected row at index 0")
            .get::<bool, _>("bit_col"),
        Some(true)
    );
    assert!(
        rows.first()
            .expect("expected row at index 0")
            .get::<f64, _>("float_col")
            .is_some()
    );
    assert!(
        rows.first()
            .expect("expected row at index 0")
            .get::<String, _>("str_col")
            .is_some()
    );
    assert!(
        rows.first()
            .expect("expected row at index 0")
            .get::<uuid::Uuid, _>("guid_col")
            .is_some()
    );
}

#[tokio::test]
async fn parameterized_query() {
    let cfg = common::live_config_or_exit();
    let mut client = Client::connect(&cfg).await.expect("connect failed");

    let rows = client
        .query("SELECT @P1 AS a, @P2 AS b", &[&42i32, &"world"])
        .await
        .expect("query failed")
        .into_first_result();

    assert_eq!(rows.len(), 1);
    assert_eq!(
        rows.first()
            .expect("expected row at index 0")
            .get::<i32, _>("a"),
        Some(42)
    );
    assert_eq!(
        rows.first()
            .expect("expected row at index 0")
            .get::<String, _>("b"),
        Some("world".into())
    );
}

#[tokio::test]
async fn null_handling() {
    let cfg = common::live_config_or_exit();
    let mut client = Client::connect(&cfg).await.expect("connect failed");

    let rows = client
        .simple_query("SELECT CAST(NULL AS int) AS nullable_col")
        .await
        .expect("query failed")
        .into_first_result();

    assert_eq!(rows.len(), 1);
    assert_eq!(
        rows.first()
            .expect("expected row at index 0")
            .get::<i32, _>("nullable_col"),
        None
    );
    assert_eq!(
        rows.first()
            .expect("expected row at index 0")
            .get::<Option<i32>, _>("nullable_col"),
        Some(None)
    );
}

#[tokio::test]
async fn multiple_rows() {
    let cfg = common::live_config_or_exit();
    let mut client = Client::connect(&cfg).await.expect("connect failed");

    let rows = client
        .simple_query("SELECT name FROM sys.databases WHERE database_id <= 4 ORDER BY database_id")
        .await
        .expect("query failed")
        .into_first_result();

    // System DBs: master, tempdb, model, msdb
    assert!(rows.len() >= 4);
    assert_eq!(
        rows.first()
            .expect("expected row at index 0")
            .get::<String, _>("name"),
        Some("master".into())
    );
}

#[tokio::test]
async fn get_by_index() {
    let cfg = common::live_config_or_exit();
    let mut client = Client::connect(&cfg).await.expect("connect failed");

    let rows = client
        .simple_query("SELECT 99 AS val")
        .await
        .expect("query failed")
        .into_first_result();

    assert_eq!(
        rows.first()
            .expect("expected row at index 0")
            .get::<i32, _>(0usize),
        Some(99)
    );
}

#[tokio::test]
async fn datetime_types() {
    let cfg = common::live_config_or_exit();
    let mut client = Client::connect(&cfg).await.expect("connect failed");

    let rows = client
        .simple_query(
            "SELECT \
                CAST('2026-05-08' AS date) AS d, \
                CAST('13:30:00' AS time) AS t, \
                CAST('2026-05-08 13:30:00' AS datetime2) AS dt2",
        )
        .await
        .expect("query failed")
        .into_first_result();

    assert_eq!(rows.len(), 1);
    let d: chrono::NaiveDate = rows
        .first()
        .expect("expected row at index 0")
        .get("d")
        .expect("date");
    assert_eq!(
        d,
        chrono::NaiveDate::from_ymd_opt(2026, 5, 8).expect("valid test date")
    );

    let t: chrono::NaiveTime = rows
        .first()
        .expect("expected row at index 0")
        .get("t")
        .expect("time");
    assert_eq!(
        t,
        chrono::NaiveTime::from_hms_opt(13, 30, 0).expect("valid test time")
    );

    let dt: chrono::NaiveDateTime = rows
        .first()
        .expect("expected row at index 0")
        .get("dt2")
        .expect("datetime2");
    assert_eq!(
        dt.date(),
        chrono::NaiveDate::from_ymd_opt(2026, 5, 8).expect("valid test date")
    );
}

#[tokio::test]
async fn decimal_type() {
    let cfg = common::live_config_or_exit();
    let mut client = Client::connect(&cfg).await.expect("connect failed");

    let rows = client
        .simple_query("SELECT CAST(123.45 AS decimal(10,2)) AS dec_col")
        .await
        .expect("query failed")
        .into_first_result();

    let d: rust_decimal::Decimal = rows
        .first()
        .expect("expected row at index 0")
        .get("dec_col")
        .expect("decimal");
    assert_eq!(d.to_string(), "123.45");
}

#[tokio::test]
async fn client_ping() {
    let cfg = common::live_config_or_exit();
    let mut client = Client::connect(&cfg).await.expect("connect failed");

    client.ping().await.expect("ping failed");

    let rows = client
        .simple_query("SELECT 1 AS value")
        .await
        .expect("query after ping failed")
        .into_first_result();
    assert_eq!(
        rows.first()
            .expect("expected row at index 0")
            .get::<i32, _>("value"),
        Some(1)
    );
}

#[tokio::test]
async fn connection_pool() {
    let cfg = common::live_config_or_exit();
    let pool = TdsManager::create_pool(cfg, 4).expect("pool creation failed");

    let mut conn = Box::pin(pool.get()).await.expect("pool checkout failed");

    let rows = conn
        .simple_query("SELECT 1 AS value")
        .await
        .expect("query failed")
        .into_first_result();
    assert_eq!(
        rows.first()
            .expect("expected row at index 0")
            .get::<i32, _>("value"),
        Some(1)
    );
}

#[tokio::test]
async fn binary_data() {
    let cfg = common::live_config_or_exit();
    let mut client = Client::connect(&cfg).await.expect("connect failed");

    let rows = client
        .simple_query("SELECT CAST(0xDEADBEEF AS varbinary(4)) AS bin_col")
        .await
        .expect("query failed")
        .into_first_result();

    let bytes: Vec<u8> = rows
        .first()
        .expect("expected row at index 0")
        .get("bin_col")
        .expect("binary");
    assert_eq!(bytes, vec![0xDE, 0xAD, 0xBE, 0xEF]);
}

#[tokio::test]
async fn decimal_parameter_roundtrip() {
    use rust_decimal::Decimal;

    let cfg = common::live_config_or_exit();
    let mut client = Client::connect(&cfg).await.expect("connect failed");

    // Create a temporary table
    let decimal_val = Decimal::new(12345, 2); // 123.45
    client
        .simple_query("CREATE TABLE #temp_decimal_test (id INT, amount NUMERIC(10, 4))")
        .await
        .expect("create table failed");

    // Insert with decimal parameter
    client
        .query(
            "INSERT INTO #temp_decimal_test (id, amount) VALUES (@P1, @P2)",
            &[&1i32, &decimal_val],
        )
        .await
        .expect("insert failed");

    // Read back the decimal value
    let rows = client
        .simple_query("SELECT amount FROM #temp_decimal_test WHERE id = 1")
        .await
        .expect("select failed")
        .into_first_result();

    assert_eq!(rows.len(), 1);
    let read_val: Option<Decimal> = rows.first().expect("expected row at index 0").get("amount");
    assert_eq!(read_val, Some(decimal_val));

    // Clean up
    client
        .simple_query("DROP TABLE #temp_decimal_test")
        .await
        .expect("drop table failed");
}
