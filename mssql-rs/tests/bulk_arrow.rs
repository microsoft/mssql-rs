// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.
// Portions derived from saurabh500/mssql-tiberius-bridge (MIT).

//! Integration test for `BulkInsert::send_arrow` (issue #85).
//!
//! Skipped when the shared live-test password is unavailable.

#![cfg(feature = "arrow")]

pub mod common;

use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

use arrow_array::{
    BooleanArray, Date64Array, Decimal128Array, Float64Array, Int32Array, RecordBatch, StringArray,
    Time64NanosecondArray, TimestampMicrosecondArray,
};
use arrow_schema::{DataType, Field, Schema, TimeUnit};
use mssql_rs::Client;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn bulk_insert_send_arrow_mixed_types() {
    let cfg = common::live_config_or_exit();
    let mut client = Client::connect(&cfg).await.expect("connect failed");

    client
        .execute(
            "CREATE TABLE #DriverBulkArrow (
                id INT NOT NULL,
                name NVARCHAR(100) NOT NULL,
                price DECIMAL(10, 2) NOT NULL,
                rating FLOAT NOT NULL,
                active BIT NOT NULL
            )",
            &[],
        )
        .await
        .expect("create table failed");

    let n = 1000_i32;
    let ids: Int32Array = (0..n).collect();
    let names = StringArray::from((0..n).map(|i| format!("user-{i}")).collect::<Vec<_>>());
    let prices =
        Decimal128Array::from((0..n).map(|i| i128::from(i) * 100 + 99).collect::<Vec<_>>())
            .with_precision_and_scale(10, 2)
            .expect("valid decimal precision and scale");
    let ratings = Float64Array::from((0..n).map(|i| f64::from(i) / 10.0).collect::<Vec<_>>());
    let actives = BooleanArray::from((0..n).map(|i| i % 2 == 0).collect::<Vec<_>>());

    let schema = Arc::new(Schema::new(vec![
        Field::new("id", DataType::Int32, false),
        Field::new("name", DataType::Utf8, false),
        Field::new("price", DataType::Decimal128(10, 2), false),
        Field::new("rating", DataType::Float64, false),
        Field::new("active", DataType::Boolean, false),
    ]));
    let batch = RecordBatch::try_new(
        schema,
        vec![
            Arc::new(ids),
            Arc::new(names),
            Arc::new(prices),
            Arc::new(ratings),
            Arc::new(actives),
        ],
    )
    .expect("RecordBatch::try_new failed");

    let result = client
        .bulk_insert("#DriverBulkArrow")
        .batch_size(500)
        .table_lock(true)
        .send_arrow(batch)
        .await
        .expect("send_arrow failed");

    assert_eq!(
        result.rows_affected,
        u64::try_from(n).expect("row count must be nonnegative")
    );

    let count_rows = client
        .simple_query("SELECT COUNT(*) AS n FROM #DriverBulkArrow")
        .await
        .expect("select count failed")
        .into_first_result();
    let count: i32 = count_rows
        .first()
        .expect("expected row at index 0")
        .get("n")
        .expect("missing count");
    assert_eq!(count, n);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn bulk_insert_send_arrow_batches() {
    let cfg = common::live_config_or_exit();
    let mut client = Client::connect(&cfg).await.expect("connect failed");

    client
        .execute(
            "CREATE TABLE #DriverBulkArrowBatches (id INT NOT NULL, name NVARCHAR(100) NOT NULL)",
            &[],
        )
        .await
        .expect("create table failed");

    let schema = Arc::new(Schema::new(vec![
        Field::new("id", DataType::Int32, false),
        Field::new("name", DataType::Utf8, false),
    ]));

    let batches: Vec<RecordBatch> = (0..3)
        .map(|b| {
            let ids: Int32Array = (b * 100..(b + 1) * 100).collect();
            let names = StringArray::from(
                (b * 100..(b + 1) * 100)
                    .map(|i| format!("u{i}"))
                    .collect::<Vec<_>>(),
            );
            RecordBatch::try_new(schema.clone(), vec![Arc::new(ids), Arc::new(names)])
                .expect("build record batch matching schema")
        })
        .collect();

    let result = client
        .bulk_insert("#DriverBulkArrowBatches")
        .send_arrow_batches(batches)
        .await
        .expect("send_arrow_batches failed");

    assert_eq!(result.rows_affected, 300);
}

#[tokio::test]
async fn bulk_arrow_stops_consuming_batches_after_row_error() {
    let cfg = common::live_config_or_exit();
    let mut client = Client::connect(&cfg).await.expect("connect failed");
    client
        .execute("CREATE TABLE #DriverArrowLazy (n INT NULL)", &[])
        .await
        .expect("create table failed");
    let batch = RecordBatch::try_from_iter([("n", Arc::new(Date64Array::from(vec![0])) as _)])
        .expect("valid batch");
    let consumed = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&consumed);
    let batches = [batch.clone(), batch].into_iter().inspect(move |_| {
        counter.fetch_add(1, Ordering::SeqCst);
    });
    let error = client
        .bulk_insert("#DriverArrowLazy")
        .send_arrow_batches(batches)
        .await
        .expect_err("Date64 must be rejected");
    assert!(error.to_string().contains("unsupported"));
    assert_eq!(consumed.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn bulk_insert_arrow_null_and_temporal_values() {
    let cfg = common::live_config_or_exit();
    let mut client = Client::connect(&cfg).await.expect("connect failed");
    client
        .execute(
            "CREATE TABLE #DriverArrowValues (n INT NULL, s NVARCHAR(20) NULL, t TIME(7) NULL, d DATETIME2(7) NULL)",
            &[],
        )
        .await
        .expect("create table failed");
    let wide = chrono::NaiveDate::from_ymd_opt(2500, 1, 1)
        .expect("valid date")
        .and_hms_opt(0, 0, 0)
        .expect("valid time")
        .and_utc()
        .timestamp_micros();
    let schema = Arc::new(Schema::new(vec![
        Field::new("n", DataType::Int32, true),
        Field::new("s", DataType::Utf8, true),
        Field::new("t", DataType::Time64(TimeUnit::Nanosecond), true),
        Field::new("d", DataType::Timestamp(TimeUnit::Microsecond, None), true),
    ]));
    let batch = RecordBatch::try_new(
        schema,
        vec![
            Arc::new(Int32Array::from(vec![None, Some(0)])),
            Arc::new(StringArray::from(vec![None, Some("")])),
            Arc::new(Time64NanosecondArray::from(vec![None, Some(123_456_789)])),
            Arc::new(TimestampMicrosecondArray::from(vec![
                None,
                Some(wide + 123_456),
            ])),
        ],
    )
    .expect("valid batch");
    client
        .bulk_insert("#DriverArrowValues")
        .keep_nulls(true)
        .send_arrow(batch)
        .await
        .expect("bulk load failed");
    let rows = client
        .simple_query("SELECT n, s, t, d FROM #DriverArrowValues ORDER BY CASE WHEN n IS NULL THEN 0 ELSE 1 END")
        .await
        .expect("query failed")
        .into_first_result();
    assert_eq!(rows.len(), 2);
    let null_row = rows.first().expect("expected null row");
    let value_row = rows.get(1).expect("expected non-null row");
    assert_eq!(null_row.get::<Option<i32>, _>("n"), Some(None));
    assert_eq!(null_row.get::<Option<&str>, _>("s"), Some(None));
    assert_eq!(
        null_row.get::<Option<chrono::NaiveTime>, _>("t"),
        Some(None)
    );
    assert_eq!(
        null_row.get::<Option<chrono::NaiveDateTime>, _>("d"),
        Some(None)
    );
    assert_eq!(value_row.get::<i32, _>("n"), Some(0));
    assert_eq!(value_row.get::<&str, _>("s"), Some(""));
    assert_eq!(
        value_row.get::<chrono::NaiveTime, _>("t"),
        chrono::NaiveTime::from_hms_nano_opt(0, 0, 0, 123_456_700)
    );
    assert_eq!(
        value_row.get::<chrono::NaiveDateTime, _>("d"),
        chrono::NaiveDate::from_ymd_opt(2500, 1, 1)
            .and_then(|date| date.and_hms_micro_opt(0, 0, 0, 123_456))
    );
}
