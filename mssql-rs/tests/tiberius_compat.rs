// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.
// Portions derived from saurabh500/mssql-tiberius-bridge (MIT).

pub mod common;

use chrono::TimeZone;
use mssql_rs::Client;
use serde_json::json;

async fn connect() -> Client {
    Client::connect(&common::live_config_or_exit())
        .await
        .expect("connect failed")
}

// =============================================================================
// 1. Type round-trips via parameterized queries
// =============================================================================

#[tokio::test]
async fn bool_type() {
    let mut client = connect().await;
    let row = client
        .query("SELECT @P1", &[&true])
        .await
        .expect("query succeeds: SELECT @P1")
        .into_first_result();
    assert_eq!(
        row.first()
            .expect("expected row at index 0")
            .get::<bool, _>(0usize),
        Some(true)
    );

    let row = client
        .query("SELECT @P1", &[&false])
        .await
        .expect("query succeeds: SELECT @P1")
        .into_first_result();
    assert_eq!(
        row.first()
            .expect("expected row at index 0")
            .get::<bool, _>(0usize),
        Some(false)
    );
}

#[tokio::test]
async fn u8_token() {
    let mut client = connect().await;
    let row = client
        .query("SELECT @P1", &[&255u8])
        .await
        .expect("query succeeds: SELECT @P1")
        .into_first_result();
    assert_eq!(
        row.first()
            .expect("expected row at index 0")
            .get::<u8, _>(0usize),
        Some(255u8)
    );
}

#[tokio::test]
async fn i16_token() {
    let mut client = connect().await;
    let row = client
        .query("SELECT @P1", &[&i16::MIN])
        .await
        .expect("query succeeds: SELECT @P1")
        .into_first_result();
    assert_eq!(
        row.first()
            .expect("expected row at index 0")
            .get::<i16, _>(0usize),
        Some(i16::MIN)
    );

    let row = client
        .query("SELECT @P1", &[&i16::MAX])
        .await
        .expect("query succeeds: SELECT @P1")
        .into_first_result();
    assert_eq!(
        row.first()
            .expect("expected row at index 0")
            .get::<i16, _>(0usize),
        Some(i16::MAX)
    );
}

#[tokio::test]
async fn i32_token() {
    let mut client = connect().await;
    let row = client
        .query("SELECT @P1", &[&i32::MIN])
        .await
        .expect("query succeeds: SELECT @P1")
        .into_first_result();
    assert_eq!(
        row.first()
            .expect("expected row at index 0")
            .get::<i32, _>(0usize),
        Some(i32::MIN)
    );

    let row = client
        .query("SELECT @P1", &[&i32::MAX])
        .await
        .expect("query succeeds: SELECT @P1")
        .into_first_result();
    assert_eq!(
        row.first()
            .expect("expected row at index 0")
            .get::<i32, _>(0usize),
        Some(i32::MAX)
    );
}

#[tokio::test]
async fn i64_token() {
    let mut client = connect().await;
    let row = client
        .query("SELECT @P1", &[&i64::MIN])
        .await
        .expect("query succeeds: SELECT @P1")
        .into_first_result();
    assert_eq!(
        row.first()
            .expect("expected row at index 0")
            .get::<i64, _>(0usize),
        Some(i64::MIN)
    );

    let row = client
        .query("SELECT @P1", &[&i64::MAX])
        .await
        .expect("query succeeds: SELECT @P1")
        .into_first_result();
    assert_eq!(
        row.first()
            .expect("expected row at index 0")
            .get::<i64, _>(0usize),
        Some(i64::MAX)
    );
}

#[tokio::test]
async fn f32_token() {
    let mut client = connect().await;
    let row = client
        .query("SELECT @P1", &[&1.23f32])
        .await
        .expect("query succeeds: SELECT @P1")
        .into_first_result();
    let val = row
        .first()
        .expect("expected row at index 0")
        .get::<f32, _>(0usize)
        .expect("expected non-NULL column 0usize");
    assert!((val - 1.23f32).abs() < f32::EPSILON);
}

#[tokio::test]
async fn f64_token() {
    let mut client = connect().await;
    let row = client
        .query("SELECT @P1", &[&1.23456789f64])
        .await
        .expect("query succeeds: SELECT @P1")
        .into_first_result();
    let val = row
        .first()
        .expect("expected row at index 0")
        .get::<f64, _>(0usize)
        .expect("expected non-NULL column 0usize");
    assert!((val - 1.23456789f64).abs() < f64::EPSILON);
}

#[tokio::test]
async fn string_roundtrip() {
    let mut client = connect().await;
    let input = "hello world";
    let row = client
        .query("SELECT @P1", &[&input])
        .await
        .expect("query succeeds: SELECT @P1")
        .into_first_result();
    assert_eq!(
        row.first()
            .expect("expected row at index 0")
            .get::<String, _>(0usize),
        Some("hello world".to_string())
    );
}

#[tokio::test]
async fn uuid_roundtrip() {
    let mut client = connect().await;
    let id =
        uuid::Uuid::parse_str("936da01f-9abd-4d9d-80c7-02af85c822a8").expect("valid test UUID");
    let row = client
        .query("SELECT @P1", &[&id])
        .await
        .expect("query succeeds: SELECT @P1")
        .into_first_result();
    assert_eq!(
        row.first()
            .expect("expected row at index 0")
            .get::<uuid::Uuid, _>(0usize),
        Some(id)
    );
}

#[tokio::test]
async fn decimal_roundtrip() {
    let mut client = connect().await;
    let row = client
        .query("SELECT CAST(123.456 AS decimal(10,3))", &[])
        .await
        .expect("query succeeds: SELECT CAST(123.456 AS decimal(10,3))")
        .into_first_result();
    let val = row
        .first()
        .expect("expected row at index 0")
        .get::<rust_decimal::Decimal, _>(0usize)
        .expect("expected non-NULL column 0usize");
    let expected: rust_decimal::Decimal = "123.456".parse().expect("valid test decimal");
    assert_eq!(val, expected);
}

// =============================================================================
// 2. Nullable types
// =============================================================================

#[tokio::test]
async fn nullable_i32_some() {
    let mut client = connect().await;
    let row = client
        .query("SELECT @P1", &[&42i32])
        .await
        .expect("query succeeds: SELECT @P1")
        .into_first_result();
    assert_eq!(
        row.first()
            .expect("expected row at index 0")
            .get::<Option<i32>, _>(0usize),
        Some(Some(42))
    );
}

#[tokio::test]
async fn nullable_i32_none() {
    let mut client = connect().await;
    let row = client
        .query("SELECT CAST(NULL AS int)", &[])
        .await
        .expect("query succeeds: SELECT CAST(NULL AS int)")
        .into_first_result();
    assert_eq!(
        row.first()
            .expect("expected row at index 0")
            .get::<Option<i32>, _>(0usize),
        Some(None)
    );
}

#[tokio::test]
async fn nullable_string_some() {
    let mut client = connect().await;
    let row = client
        .query("SELECT @P1", &[&"hello"])
        .await
        .expect("query succeeds: SELECT @P1")
        .into_first_result();
    assert_eq!(
        row.first()
            .expect("expected row at index 0")
            .get::<Option<String>, _>(0usize),
        Some(Some("hello".to_string()))
    );
}

#[tokio::test]
async fn nullable_string_none() {
    let mut client = connect().await;
    let row = client
        .query("SELECT CAST(NULL AS nvarchar(50))", &[])
        .await
        .expect("query succeeds: SELECT CAST(NULL AS nvarchar(50))")
        .into_first_result();
    assert_eq!(
        row.first()
            .expect("expected row at index 0")
            .get::<Option<String>, _>(0usize),
        Some(None)
    );
}

// =============================================================================
// 3. String encoding tests
// =============================================================================

#[tokio::test]
async fn kanji_nvarchar() {
    let mut client = connect().await;
    let text = "につい765765t";
    client
        .simple_query("CREATE TABLE #kanji_test (val nvarchar(100))")
        .await
        .expect("query succeeds: CREATE TABLE #kanji_test (val nvarchar(100))");
    client
        .execute("INSERT INTO #kanji_test (val) VALUES (@P1)", &[&text])
        .await
        .expect("execute succeeds: INSERT INTO #kanji_test (val) VALUES (@P1)");
    let row = client
        .query("SELECT val FROM #kanji_test", &[])
        .await
        .expect("query succeeds: SELECT val FROM #kanji_test")
        .into_first_result();
    assert_eq!(
        row.first()
            .expect("expected row at index 0")
            .get::<String, _>(0usize),
        Some(text.to_string())
    );
}

#[tokio::test]
async fn finnish_varchar() {
    let mut client = connect().await;
    let text = "Ä is for Ansen";
    client
        .simple_query("CREATE TABLE #finnish_test (val nvarchar(100))")
        .await
        .expect("query succeeds: CREATE TABLE #finnish_test (val nvarchar(100))");
    client
        .execute("INSERT INTO #finnish_test (val) VALUES (@P1)", &[&text])
        .await
        .expect("execute succeeds: INSERT INTO #finnish_test (val) VALUES (@P1)");
    let row = client
        .query("SELECT val FROM #finnish_test", &[])
        .await
        .expect("query succeeds: SELECT val FROM #finnish_test")
        .into_first_result();
    assert_eq!(
        row.first()
            .expect("expected row at index 0")
            .get::<String, _>(0usize),
        Some(text.to_string())
    );
}

#[tokio::test]
async fn empty_string() {
    let mut client = connect().await;
    let row = client
        .query("SELECT CAST('' AS varchar(10))", &[])
        .await
        .expect("query succeeds: SELECT CAST('' AS varchar(10))")
        .into_first_result();
    assert_eq!(
        row.first()
            .expect("expected row at index 0")
            .get::<Option<String>, _>(0usize),
        Some(Some(String::new()))
    );
}

// =============================================================================
// 4. Date/time types
// =============================================================================

#[tokio::test]
async fn naive_date_time() {
    let mut client = connect().await;
    let row = client
        .query("SELECT CAST('2020-04-20 16:20:00' AS datetime2)", &[])
        .await
        .expect("query succeeds: SELECT CAST('2020-04-20 16:20:00' AS datetime2)")
        .into_first_result();
    let val = row
        .first()
        .expect("expected row at index 0")
        .get::<chrono::NaiveDateTime, _>(0usize)
        .expect("expected non-NULL column 0usize");
    let expected = chrono::NaiveDate::from_ymd_opt(2020, 4, 20)
        .expect("valid test date")
        .and_hms_opt(16, 20, 0)
        .expect("valid test time");
    assert_eq!(val, expected);
}

#[tokio::test]
async fn naive_date() {
    let mut client = connect().await;
    let row = client
        .query("SELECT CAST('2020-04-20' AS date)", &[])
        .await
        .expect("query succeeds: SELECT CAST('2020-04-20' AS date)")
        .into_first_result();
    let val = row
        .first()
        .expect("expected row at index 0")
        .get::<chrono::NaiveDate, _>(0usize)
        .expect("expected non-NULL column 0usize");
    assert_eq!(
        val,
        chrono::NaiveDate::from_ymd_opt(2020, 4, 20).expect("valid test date")
    );
}

#[tokio::test]
async fn naive_time() {
    let mut client = connect().await;
    let row = client
        .query("SELECT CAST('16:20:00' AS time)", &[])
        .await
        .expect("query succeeds: SELECT CAST('16:20:00' AS time)")
        .into_first_result();
    let val = row
        .first()
        .expect("expected row at index 0")
        .get::<chrono::NaiveTime, _>(0usize)
        .expect("expected non-NULL column 0usize");
    assert_eq!(
        val,
        chrono::NaiveTime::from_hms_opt(16, 20, 0).expect("valid test time")
    );
}

#[tokio::test]
async fn datetime_offset() {
    let mut client = connect().await;
    let row = client
        .query(
            "SELECT CAST('2020-04-20 16:20:00 +02:00' AS datetimeoffset)",
            &[],
        )
        .await
        .expect("query succeeds: SELECT CAST('2020-04-20 16:20:00 +02:00' AS datetimeoffset)")
        .into_first_result();
    let val = row
        .first()
        .expect("expected row at index 0")
        .get::<chrono::DateTime<chrono::FixedOffset>, _>(0usize)
        .expect("expected non-NULL column 0usize");
    let expected = chrono::FixedOffset::east_opt(2 * 3600)
        .expect("valid test offset")
        .from_local_datetime(
            &chrono::NaiveDate::from_ymd_opt(2020, 4, 20)
                .expect("valid test date")
                .and_hms_opt(16, 20, 0)
                .expect("valid test time"),
        )
        .single()
        .expect("unambiguous test datetime");
    assert_eq!(val, expected);
}

// =============================================================================
// 5. Binary types
// =============================================================================

#[tokio::test]
async fn varbinary_roundtrip() {
    let mut client = connect().await;
    let row = client
        .query("SELECT CAST(0xDEADBEEF AS varbinary(4))", &[])
        .await
        .expect("query succeeds: SELECT CAST(0xDEADBEEF AS varbinary(4))")
        .into_first_result();
    let val = row
        .first()
        .expect("expected row at index 0")
        .get::<Vec<u8>, _>(0usize)
        .expect("expected non-NULL column 0usize");
    assert_eq!(val, vec![0xDE, 0xAD, 0xBE, 0xEF]);
}

#[tokio::test]
async fn varbinary_empty() {
    let mut client = connect().await;
    let row = client
        .query("SELECT CAST(0x AS varbinary(1))", &[])
        .await
        .expect("query succeeds: SELECT CAST(0x AS varbinary(1))")
        .into_first_result();
    let val = row
        .first()
        .expect("expected row at index 0")
        .get::<Vec<u8>, _>(0usize)
        .expect("expected non-NULL column 0usize");
    assert!(val.is_empty());
}

#[tokio::test]
async fn binary_type() {
    let mut client = connect().await;
    let row = client
        .query("SELECT CAST(0x0102030405 AS binary(5))", &[])
        .await
        .expect("query succeeds: SELECT CAST(0x0102030405 AS binary(5))")
        .into_first_result();
    let val = row
        .first()
        .expect("expected row at index 0")
        .get::<Vec<u8>, _>(0usize)
        .expect("expected non-NULL column 0usize");
    assert_eq!(val, vec![0x01, 0x02, 0x03, 0x04, 0x05]);
}

// =============================================================================
// 5b. ToSql round-trips for binary/chrono (issue #13)
// =============================================================================

#[tokio::test]
async fn vec_u8_param_roundtrip() {
    let mut client = connect().await;
    let payload: Vec<u8> = vec![0xDE, 0xAD, 0xBE, 0xEF, 0x00, 0xFF];
    let row = client
        .query("SELECT @P1", &[&payload])
        .await
        .expect("query succeeds: SELECT @P1")
        .into_first_result();
    assert_eq!(
        row.first()
            .expect("expected row at index 0")
            .get::<Vec<u8>, _>(0usize),
        Some(payload)
    );
}

#[tokio::test]
async fn empty_vec_u8_param_roundtrip() {
    let mut client = connect().await;
    let payload: Vec<u8> = vec![];
    let row = client
        .query("SELECT @P1", &[&payload])
        .await
        .expect("query succeeds: SELECT @P1")
        .into_first_result();
    assert_eq!(
        row.first()
            .expect("expected row at index 0")
            .get::<Vec<u8>, _>(0usize),
        Some(payload)
    );
}

#[tokio::test]
async fn naive_date_param_roundtrip() {
    let mut client = connect().await;
    let d = chrono::NaiveDate::from_ymd_opt(2024, 7, 4).expect("valid test date");
    let row = client
        .query("SELECT @P1", &[&d])
        .await
        .expect("query succeeds: SELECT @P1")
        .into_first_result();
    assert_eq!(
        row.first()
            .expect("expected row at index 0")
            .get::<chrono::NaiveDate, _>(0usize),
        Some(d)
    );
}

#[tokio::test]
async fn naive_time_param_roundtrip() {
    let mut client = connect().await;
    let t = chrono::NaiveTime::from_hms_nano_opt(12, 34, 56, 789_000_000).expect("valid test time");
    let row = client
        .query("SELECT @P1", &[&t])
        .await
        .expect("query succeeds: SELECT @P1")
        .into_first_result();
    assert_eq!(
        row.first()
            .expect("expected row at index 0")
            .get::<chrono::NaiveTime, _>(0usize),
        Some(t)
    );
}

#[tokio::test]
async fn naive_date_time_param_roundtrip() {
    let mut client = connect().await;
    let dt = chrono::NaiveDate::from_ymd_opt(1999, 12, 31)
        .expect("valid test date")
        .and_hms_milli_opt(23, 59, 58, 250)
        .expect("valid test time");
    let row = client
        .query("SELECT @P1", &[&dt])
        .await
        .expect("query succeeds: SELECT @P1")
        .into_first_result();
    assert_eq!(
        row.first()
            .expect("expected row at index 0")
            .get::<chrono::NaiveDateTime, _>(0usize),
        Some(dt)
    );
}

#[tokio::test]
async fn datetime_fixed_offset_param_roundtrip() {
    use chrono::TimeZone;
    let mut client = connect().await;
    let dt = chrono::FixedOffset::east_opt(2 * 3600)
        .expect("valid test offset")
        .with_ymd_and_hms(2024, 1, 15, 9, 30, 0)
        .single()
        .expect("valid test time");
    let row = client
        .query("SELECT @P1", &[&dt])
        .await
        .expect("query succeeds: SELECT @P1")
        .into_first_result();
    let got = row
        .first()
        .expect("expected row at index 0")
        .get::<chrono::DateTime<chrono::FixedOffset>, _>(0usize)
        .expect("expected non-NULL column 0usize");
    assert_eq!(got, dt);
}

#[tokio::test]
async fn datetime_utc_param_sends_offset_zero() {
    use chrono::TimeZone;
    let mut client = connect().await;
    let dt_utc = chrono::Utc
        .with_ymd_and_hms(2024, 6, 1, 12, 0, 0)
        .single()
        .expect("valid test datetime");
    let row = client
        .query("SELECT @P1", &[&dt_utc])
        .await
        .expect("query succeeds: SELECT @P1")
        .into_first_result();
    let got = row
        .first()
        .expect("expected row at index 0")
        .get::<chrono::DateTime<chrono::FixedOffset>, _>(0usize)
        .expect("expected non-NULL column 0usize");
    assert_eq!(got.naive_utc(), dt_utc.naive_utc());
    assert_eq!(got.offset().local_minus_utc(), 0);
}

#[tokio::test]
async fn serde_json_value_param_variant_dispatch() {
    let mut client = connect().await;

    let null_value = serde_json::Value::Null;
    let rows = client
        .query("SELECT @P1", &[&null_value])
        .await
        .expect("query succeeds: SELECT @P1")
        .into_first_result();
    assert_eq!(
        rows.first()
            .expect("expected row at index 0")
            .get::<Option<String>, _>(0usize),
        Some(None)
    );

    let bool_value = json!(true);
    let rows = client
        .query("SELECT @P1", &[&bool_value])
        .await
        .expect("query succeeds: SELECT @P1")
        .into_first_result();
    assert_eq!(
        rows.first()
            .expect("expected row at index 0")
            .get::<bool, _>(0usize),
        Some(true)
    );

    let int_value = json!(42);
    let rows = client
        .query("SELECT @P1", &[&int_value])
        .await
        .expect("query succeeds: SELECT @P1")
        .into_first_result();
    assert_eq!(
        rows.first()
            .expect("expected row at index 0")
            .get::<i64, _>(0usize),
        Some(42)
    );

    let float_value = json!(2.5);
    let rows = client
        .query("SELECT @P1", &[&float_value])
        .await
        .expect("query succeeds: SELECT @P1")
        .into_first_result();
    let got_float = rows
        .first()
        .expect("expected row at index 0")
        .get::<f64, _>(0usize)
        .expect("expected non-NULL column 0usize");
    assert!((got_float - 2.5).abs() < f64::EPSILON);

    let string_value = json!("alice");
    let rows = client
        .query("SELECT @P1", &[&string_value])
        .await
        .expect("query succeeds: SELECT @P1")
        .into_first_result();
    assert_eq!(
        rows.first()
            .expect("expected row at index 0")
            .get::<String, _>(0usize),
        Some("alice".to_string())
    );

    let array_value = json!([1, 2, 3]);
    let rows = client
        .query("SELECT @P1", &[&array_value])
        .await
        .expect("query succeeds: SELECT @P1")
        .into_first_result();
    assert_eq!(
        rows.first()
            .expect("expected row at index 0")
            .get::<String, _>(0usize),
        Some(array_value.to_string())
    );

    let object_value = json!({"id": 1, "name": "alice"});
    let rows = client
        .query("SELECT @P1", &[&object_value])
        .await
        .expect("query succeeds: SELECT @P1")
        .into_first_result();
    assert_eq!(
        rows.first()
            .expect("expected row at index 0")
            .get::<String, _>(0usize),
        Some(object_value.to_string())
    );
}

// =============================================================================
// 5c. Connection options — Config::readonly (issue #15)
// =============================================================================

/// `Config::readonly(true)` must surface `ApplicationIntent=ReadOnly` in the
/// Login7 packet. The unit test in `src/config.rs` verifies the
/// `ClientContext.application_intent` field; this test verifies the login is
/// accepted by SQL Server end-to-end. (Bridge #15.)
///
/// Note: standalone SQL Server doesn't expose the negotiated value via
/// `CONNECTIONPROPERTY('client_app_intent')` (always NULL), so a true
/// observable round-trip requires Always On / Azure SQL geo-replicas.
#[tokio::test]
async fn readonly_login_smoke() {
    let mut cfg = common::live_config_or_exit();
    cfg.readonly(true);
    let mut client = Client::connect(&cfg)
        .await
        .expect("login with readonly=true");
    let row = client
        .query("SELECT 1", &[])
        .await
        .expect("query succeeds: SELECT 1")
        .into_first_result();
    assert_eq!(
        row.first()
            .expect("expected row at index 0")
            .get::<i32, _>(0usize),
        Some(1)
    );
}

// =============================================================================
// 6. Multiple rows and result sets
// =============================================================================

#[tokio::test]
async fn multiple_rows() {
    let mut client = connect().await;
    let rows = client
        .query("SELECT value FROM (VALUES (1),(2),(3)) AS t(value)", &[])
        .await
        .expect("query succeeds: SELECT value FROM (VALUES (1),(2),(3)) AS t(value)")
        .into_first_result();
    assert_eq!(rows.len(), 3);
    assert_eq!(
        rows.first()
            .expect("expected row at index 0")
            .get::<i32, _>(0usize),
        Some(1)
    );
    assert_eq!(
        rows.get(1)
            .expect("expected row at index 1")
            .get::<i32, _>(0usize),
        Some(2)
    );
    assert_eq!(
        rows.get(2)
            .expect("expected row at index 2")
            .get::<i32, _>(0usize),
        Some(3)
    );
}

#[tokio::test]
async fn multiple_result_sets() {
    let mut client = connect().await;
    let results = client
        .query("SELECT 1; SELECT 'hello'", &[])
        .await
        .expect("query succeeds: SELECT 1; SELECT 'hello'")
        .into_results();
    assert_eq!(results.len(), 2);
    assert_eq!(
        results
            .first()
            .expect("expected result set at index 0")
            .first()
            .expect("expected row at index 0")
            .get::<i32, _>(0usize),
        Some(1)
    );
    assert_eq!(
        results
            .get(1)
            .expect("expected result set at index 1")
            .first()
            .expect("expected row at index 0")
            .get::<String, _>(0usize),
        Some("hello".to_string())
    );
}

#[tokio::test]
async fn empty_result_set() {
    let mut client = connect().await;
    let rows = client
        .query("SELECT 1 WHERE 1=0", &[])
        .await
        .expect("query succeeds: SELECT 1 WHERE 1=0")
        .into_first_result();
    assert!(rows.is_empty());
}

// =============================================================================
// 7. DML execution
// =============================================================================

#[tokio::test]
async fn execute_insert_update_delete() {
    let mut client = connect().await;
    client
        .simple_query("CREATE TABLE #dml_test (id int, name nvarchar(50))")
        .await
        .expect("query succeeds: CREATE TABLE #dml_test (id int, name nvarchar(50))");

    let result = client
        .execute(
            "INSERT INTO #dml_test (id, name) VALUES (@P1, @P2)",
            &[&1i32, &"test"],
        )
        .await
        .expect("execute succeeds: INSERT INTO #dml_test (id, name) VALUES (@P1, @P2)");
    assert_eq!(result.total(), 1);

    let result = client
        .execute(
            "UPDATE #dml_test SET name = @P1 WHERE id = @P2",
            &[&"updated", &1i32],
        )
        .await
        .expect("execute succeeds: UPDATE #dml_test SET name = @P1 WHERE id = @P2");
    assert_eq!(result.total(), 1);

    let result = client
        .execute("DELETE FROM #dml_test WHERE id = @P1", &[&1i32])
        .await
        .expect("execute succeeds: DELETE FROM #dml_test WHERE id = @P1");
    assert_eq!(result.total(), 1);
}

#[tokio::test]
async fn mixed_statement_results_and_counts() {
    let mut client = connect().await;
    client
        .simple_query("CREATE TABLE #mixed_results (id int)")
        .await
        .expect("query succeeds: CREATE TABLE #mixed_results (id int)");
    let results = client
        .simple_query(
            "INSERT INTO #mixed_results VALUES (1); \
             SELECT id FROM #mixed_results WHERE 1 = 0; \
             PRINT 'between results'; \
             SELECT id FROM #mixed_results; \
             DELETE FROM #mixed_results",
        )
        .await
        .expect("query succeeds: INSERT INTO #mixed_results VALUES (1); SELECT id FROM #mixed_results WHERE 1 = 0; PRINT 'between ...")
        .into_results();
    assert_eq!(results.len(), 2);
    assert!(
        results
            .first()
            .expect("expected result set at index 0")
            .is_empty()
    );
    assert_eq!(
        results
            .get(1)
            .expect("expected result set at index 1")
            .first()
            .expect("expected row at index 0")
            .get::<i32, _>(0usize),
        Some(1)
    );

    let result = client
        .execute(
            "INSERT INTO #mixed_results VALUES (1), (2); \
             PRINT 'between counts'; \
             UPDATE #mixed_results SET id = id + 1; \
             DELETE FROM #mixed_results WHERE id = 99; \
             DELETE FROM #mixed_results",
            &[],
        )
        .await
        .expect("execute succeeds: INSERT INTO #mixed_results VALUES (1), (2); PRINT 'between counts'; UPDATE #mixed_results SET id ...");
    assert_eq!(result.into_iter().collect::<Vec<_>>(), vec![2, 2, 0, 2]);
    client.ping().await.expect("ping connection");
}

#[tokio::test]
async fn simple_query_ddl() {
    let mut client = connect().await;
    client
        .simple_query("CREATE TABLE #ddl_test (id int)")
        .await
        .expect("query succeeds: CREATE TABLE #ddl_test (id int)");
    client
        .simple_query("DROP TABLE #ddl_test")
        .await
        .expect("query succeeds: DROP TABLE #ddl_test");
}

// =============================================================================
// 8. Transactions
// =============================================================================

#[tokio::test]
async fn transaction_commit() {
    let mut client = connect().await;
    client
        .simple_query("CREATE TABLE #tx_commit (id int)")
        .await
        .expect("query succeeds: CREATE TABLE #tx_commit (id int)");
    client
        .simple_query("BEGIN TRAN")
        .await
        .expect("query succeeds: BEGIN TRAN");
    client
        .simple_query("INSERT INTO #tx_commit VALUES (1)")
        .await
        .expect("query succeeds: INSERT INTO #tx_commit VALUES (1)");
    client
        .simple_query("COMMIT")
        .await
        .expect("query succeeds: COMMIT");

    let rows = client
        .query("SELECT id FROM #tx_commit", &[])
        .await
        .expect("query succeeds: SELECT id FROM #tx_commit")
        .into_first_result();
    assert_eq!(rows.len(), 1);
    assert_eq!(
        rows.first()
            .expect("expected row at index 0")
            .get::<i32, _>(0usize),
        Some(1)
    );
}

#[tokio::test]
async fn transaction_rollback() {
    let mut client = connect().await;
    client
        .simple_query("CREATE TABLE #tx_rollback (id int)")
        .await
        .expect("query succeeds: CREATE TABLE #tx_rollback (id int)");
    client
        .simple_query("BEGIN TRAN")
        .await
        .expect("query succeeds: BEGIN TRAN");
    client
        .simple_query("INSERT INTO #tx_rollback VALUES (1)")
        .await
        .expect("query succeeds: INSERT INTO #tx_rollback VALUES (1)");
    client
        .simple_query("ROLLBACK")
        .await
        .expect("query succeeds: ROLLBACK");

    let rows = client
        .query("SELECT id FROM #tx_rollback", &[])
        .await
        .expect("query succeeds: SELECT id FROM #tx_rollback")
        .into_first_result();
    assert!(rows.is_empty());
}

// =============================================================================
// 9. Numeric/Decimal edge cases
// =============================================================================

#[tokio::test]
async fn numeric_large() {
    let mut client = connect().await;
    let row = client
        .query(
            "SELECT CAST(99999999999999999999.999999 AS numeric(38,6))",
            &[],
        )
        .await
        .expect("query succeeds: SELECT CAST(99999999999999999999.999999 AS numeric(38,6))")
        .into_first_result();
    let val = row
        .first()
        .expect("expected row at index 0")
        .get::<rust_decimal::Decimal, _>(0usize)
        .expect("expected non-NULL column 0usize");
    let expected: rust_decimal::Decimal = "99999999999999999999.999999"
        .parse()
        .expect("valid test decimal");
    assert_eq!(val, expected);
}

#[tokio::test]
async fn money_type() {
    let mut client = connect().await;
    let row = client
        .query("SELECT CAST(1234.5678 AS money)", &[])
        .await
        .expect("query succeeds: SELECT CAST(1234.5678 AS money)")
        .into_first_result();
    let val = row
        .first()
        .expect("expected row at index 0")
        .get::<rust_decimal::Decimal, _>(0usize)
        .expect("expected non-NULL column 0usize");
    let expected: rust_decimal::Decimal = "1234.5678".parse().expect("valid test decimal");
    assert_eq!(val, expected);
}

// =============================================================================
// 10. XML type
// =============================================================================

#[tokio::test]
async fn xml_type() {
    let mut client = connect().await;
    let row = client
        .query("SELECT CAST('<root><item>test</item></root>' AS xml)", &[])
        .await
        .expect("query succeeds: SELECT CAST('<root><item>test</item></root>' AS xml)")
        .into_first_result();
    let val = row
        .first()
        .expect("expected row at index 0")
        .get::<String, _>(0usize)
        .expect("expected non-NULL column 0usize");
    assert_eq!(val, "<root><item>test</item></root>");
}

// =============================================================================
// 11. Column access patterns
// =============================================================================

#[tokio::test]
async fn get_by_name() {
    let mut client = connect().await;
    let rows = client
        .query("SELECT 42 AS answer, 'hello' AS greeting", &[])
        .await
        .expect("query succeeds: SELECT 42 AS answer, 'hello' AS greeting")
        .into_first_result();
    assert_eq!(
        rows.first()
            .expect("expected row at index 0")
            .get::<i32, _>("answer"),
        Some(42)
    );
    assert_eq!(
        rows.first()
            .expect("expected row at index 0")
            .get::<String, _>("greeting"),
        Some("hello".to_string())
    );
}

#[tokio::test]
async fn get_by_index() {
    let mut client = connect().await;
    let rows = client
        .query("SELECT 42 AS answer, 'hello' AS greeting", &[])
        .await
        .expect("query succeeds: SELECT 42 AS answer, 'hello' AS greeting")
        .into_first_result();
    assert_eq!(
        rows.first()
            .expect("expected row at index 0")
            .get::<i32, _>(0usize),
        Some(42)
    );
    assert_eq!(
        rows.first()
            .expect("expected row at index 0")
            .get::<String, _>(1usize),
        Some("hello".to_string())
    );
}

#[tokio::test]
async fn column_metadata() {
    let mut client = connect().await;
    let rows = client
        .query("SELECT 42 AS answer, 'hello' AS greeting", &[])
        .await
        .expect("query succeeds: SELECT 42 AS answer, 'hello' AS greeting")
        .into_first_result();
    let cols = rows.first().expect("expected row at index 0").columns();
    assert_eq!(
        cols.first().expect("expected column at index 0").name(),
        "answer"
    );
    assert_eq!(
        cols.get(1).expect("expected column at index 1").name(),
        "greeting"
    );
}

// --- &str borrowing (tiberius compat) ---

#[tokio::test]
async fn str_borrow_from_row() {
    let mut client = connect().await;
    let rows = client
        .query("SELECT @P1 AS name", &[&"hello world"])
        .await
        .expect("query failed")
        .into_first_result();
    let name: &str = rows
        .first()
        .expect("expected row at index 0")
        .get("name")
        .expect("should borrow &str");
    assert_eq!(name, "hello world");
}

#[tokio::test]
async fn str_borrow_null() {
    let mut client = connect().await;
    let rows = client
        .simple_query("SELECT CAST(NULL AS nvarchar(50)) AS val")
        .await
        .expect("query failed")
        .into_first_result();
    let val: Option<&str> = rows
        .first()
        .expect("expected row at index 0")
        .get("val")
        .expect("option should work");
    assert_eq!(val, None);
}
