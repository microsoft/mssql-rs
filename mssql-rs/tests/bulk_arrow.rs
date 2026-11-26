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
    BooleanArray, Date64Array, Decimal128Array, Float32Array, Float64Array, Int16Array, Int32Array,
    RecordBatch, StringArray, Time64NanosecondArray, TimestampMicrosecondArray, UInt8Array,
};
use arrow_schema::{DataType, Field, Schema, TimeUnit};
use mssql_rs::{Client, IntoRow};

#[tokio::test]
async fn bulk_adapters_reject_incompatible_scalars() {
    let cfg = common::live_config_or_exit();
    for (arrow, real_destination) in [(true, false), (true, true), (false, false), (false, true)] {
        let mut client = Client::connect(&cfg).await.expect("connect");
        let sql_type = if real_destination { "REAL" } else { "INT" };
        client
            .simple_query(format!("CREATE TABLE #BulkMismatch (v {sql_type})"))
            .await
            .expect("create table");
        let result = if arrow {
            let (field_type, values): (DataType, Arc<dyn arrow_array::Array>) = if real_destination
            {
                (DataType::Int32, Arc::new(Int32Array::from(vec![1])))
            } else {
                (DataType::Float32, Arc::new(Float32Array::from(vec![1.0])))
            };
            let schema = Arc::new(Schema::new(vec![Field::new("v", field_type, false)]));
            let batch = RecordBatch::try_new(schema, vec![values]).expect("batch");
            client
                .bulk_insert("#BulkMismatch")
                .send_arrow(batch)
                .await
                .map(|_| ())
        } else {
            let row = if real_destination {
                1i32.into_row().expect("row")
            } else {
                1.0f32.into_row().expect("row")
            };
            let mut request = client
                .bulk_insert("#BulkMismatch")
                .await
                .expect("bulk request");
            request.send(row).await.expect("buffer row");
            request.finalize().await.map(|_| ())
        };
        assert!(matches!(
            result,
            Err(mssql_rs::Error::Tds(
                mssql_tds::error::Error::TypeConversionError(_)
            ))
        ));
        let rows = client
            .simple_query("SELECT COUNT(*) AS n FROM #BulkMismatch")
            .await
            .expect("query after rejected row")
            .into_first_result();
        assert_eq!(rows.first().expect("count").get::<i32, _>("n"), Some(0));
    }
}

#[tokio::test]
async fn bulk_adapters_preserve_matching_null_and_variant_values() {
    let cfg = common::live_config_or_exit();
    let mut client = Client::connect(&cfg).await.expect("connect");
    client
        .simple_query("CREATE TABLE #BulkScalarControls (n INT, f REAL, v SQL_VARIANT)")
        .await
        .expect("create table");
    let schema = Arc::new(Schema::new(vec![
        Field::new("n", DataType::Int32, true),
        Field::new("f", DataType::Float32, true),
        Field::new("v", DataType::Float32, true),
    ]));
    let batch = RecordBatch::try_new(
        schema,
        vec![
            Arc::new(Int32Array::from(vec![Some(1), None])),
            Arc::new(Float32Array::from(vec![Some(1.0), None])),
            Arc::new(Float32Array::from(vec![Some(1.0), None])),
        ],
    )
    .expect("batch");
    client
        .bulk_insert("#BulkScalarControls")
        .keep_nulls(true)
        .send_arrow(batch)
        .await
        .expect("Arrow insert");
    let mut request = client
        .bulk_insert("#BulkScalarControls")
        .keep_nulls(true)
        .await
        .expect("bulk request");
    request
        .send((1i32, 1.0f32, 1.0f32).into_row().expect("row"))
        .await
        .expect("buffer row");
    request
        .send(
            (None::<i32>, None::<f32>, None::<f32>)
                .into_row()
                .expect("null row"),
        )
        .await
        .expect("buffer null row");
    request.finalize().await.expect("compatibility insert");
    let rows = client
        .simple_query(
            "SELECT COUNT(*) AS n FROM #BulkScalarControls WHERE (n = 1 AND f = 1 AND CONVERT(REAL, v) = 1 AND SQL_VARIANT_PROPERTY(v, 'BaseType') = 'real') OR (n IS NULL AND f IS NULL AND v IS NULL)",
        )
        .await
        .expect("read values")
        .into_first_result();
    assert_eq!(rows.first().expect("count").get::<i32, _>("n"), Some(4));
}

#[tokio::test]
async fn bulk_adapters_widen_integers_without_losing_sign() {
    let cfg = common::live_config_or_exit();
    let mut client = Client::connect(&cfg).await.expect("connect");
    client
        .simple_query("CREATE TABLE #BulkIntegerWidths (a SMALLINT, b INT, c BIGINT)")
        .await
        .expect("create table");
    let schema = Arc::new(Schema::new(vec![
        Field::new("a", DataType::UInt8, false),
        Field::new("b", DataType::Int16, false),
        Field::new("c", DataType::Int32, false),
    ]));
    let batch = RecordBatch::try_new(
        schema,
        vec![
            Arc::new(UInt8Array::from(vec![123])),
            Arc::new(Int16Array::from(vec![-123])),
            Arc::new(Int32Array::from(vec![-123])),
        ],
    )
    .expect("batch");
    client
        .bulk_insert("#BulkIntegerWidths")
        .send_arrow(batch)
        .await
        .expect("Arrow insert");
    let mut request = client
        .bulk_insert("#BulkIntegerWidths")
        .await
        .expect("bulk request");
    request
        .send((123u8, -123i16, -123i32).into_row().expect("row"))
        .await
        .expect("buffer row");
    request.finalize().await.expect("compatibility insert");
    let rows = client
        .simple_query(
            "SELECT COUNT(*) AS n FROM #BulkIntegerWidths WHERE a = 123 AND b = -123 AND c = -123",
        )
        .await
        .expect("read values")
        .into_first_result();
    assert_eq!(rows.first().expect("count").get::<i32, _>("n"), Some(2));
}

#[tokio::test]
async fn bulk_adapter_values_use_destination_scale() {
    let cfg = common::live_config_or_exit();
    let mut client = Client::connect(&cfg).await.expect("connect");
    client
        .execute(
            "CREATE TABLE #BulkScales (t TIME(6), d DATETIME2(6), n DECIMAL(10,4))",
            &[],
        )
        .await
        .expect("create table");

    let decimal = Decimal128Array::from(vec![12345])
        .with_precision_and_scale(10, 2)
        .expect("decimal metadata");
    let schema = Arc::new(Schema::new(vec![
        Field::new("t", DataType::Time64(TimeUnit::Nanosecond), false),
        Field::new("d", DataType::Timestamp(TimeUnit::Microsecond, None), false),
        Field::new("n", DataType::Decimal128(10, 2), false),
    ]));
    let batch = RecordBatch::try_new(
        schema,
        vec![
            Arc::new(Time64NanosecondArray::from(vec![1_000_000_000])),
            Arc::new(TimestampMicrosecondArray::from(vec![1_000_000])),
            Arc::new(decimal),
        ],
    )
    .expect("batch");
    client
        .bulk_insert("#BulkScales")
        .send_arrow(batch)
        .await
        .expect("Arrow insert");
    let time = chrono::NaiveTime::from_hms_opt(0, 0, 1).expect("time");
    let datetime = chrono::NaiveDate::from_ymd_opt(1970, 1, 1)
        .expect("date")
        .and_hms_opt(0, 0, 1)
        .expect("datetime");
    let mut request = client
        .bulk_insert("#BulkScales")
        .await
        .expect("bulk request");
    request
        .send(
            (time, datetime, rust_decimal::Decimal::new(12345, 2))
                .into_row()
                .expect("row"),
        )
        .await
        .expect("compatibility insert");
    request.finalize().await.expect("finish");

    let rows = client
        .simple_query(
            "SELECT COUNT(*) AS n FROM #BulkScales WHERE t = '00:00:01' AND d = '1970-01-01T00:00:01' AND n = 123.4500",
        )
        .await
        .expect("read values")
        .into_first_result();
    assert_eq!(rows.first().expect("count").get::<i32, _>("n"), Some(2));
}

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

fn decimal_batch(values: Vec<Option<i128>>, precision: u8, scale: i8) -> RecordBatch {
    let values = Decimal128Array::from(values)
        .with_precision_and_scale(precision, scale)
        .expect("decimal metadata");
    RecordBatch::try_from_iter([("v", Arc::new(values) as _)]).expect("decimal batch")
}

#[tokio::test]
async fn bulk_arrow_decimal_destination_rounding() {
    let cfg = common::live_config_or_exit();
    let mut client = Client::connect(&cfg).await.expect("connect");
    for sql_type in ["DECIMAL", "NUMERIC"] {
        for (precision, scale, source_scale, values, expected) in [
            (
                10,
                4,
                2,
                vec![Some(12345), Some(-12345), None],
                vec![Some("123.4500"), Some("-123.4500"), None],
            ),
            (
                10,
                2,
                2,
                vec![Some(12345), Some(-12345), Some(0)],
                vec![Some("123.45"), Some("-123.45"), Some("0.00")],
            ),
            (
                10,
                2,
                3,
                vec![
                    Some(123455),
                    Some(-123455),
                    Some(123454),
                    Some(-123456),
                    Some(5),
                    Some(-5),
                    Some(4),
                    Some(-4),
                    None,
                ],
                vec![
                    Some("123.46"),
                    Some("-123.46"),
                    Some("123.45"),
                    Some("-123.46"),
                    Some("0.01"),
                    Some("-0.01"),
                    Some("0.00"),
                    Some("0.00"),
                    None,
                ],
            ),
            (
                10,
                0,
                1,
                vec![Some(15), Some(-15), Some(14), Some(-14), Some(995)],
                vec![Some("2"), Some("-2"), Some("1"), Some("-1"), Some("100")],
            ),
            (
                38,
                0,
                0,
                vec![Some(10_i128.pow(38) - 1), Some(1_i128 << 96)],
                vec![
                    Some("99999999999999999999999999999999999999"),
                    Some("79228162514264337593543950336"),
                ],
            ),
            (
                38,
                37,
                38,
                vec![Some(10_i128.pow(38) - 5), Some(-(10_i128.pow(38) - 5))],
                vec![
                    Some("1.0000000000000000000000000000000000000"),
                    Some("-1.0000000000000000000000000000000000000"),
                ],
            ),
            (
                38,
                38,
                37,
                vec![Some(1), Some(-1)],
                vec![
                    Some("0.00000000000000000000000000000000000010"),
                    Some("-0.00000000000000000000000000000000000010"),
                ],
            ),
        ] {
            client
                .simple_query(format!("CREATE TABLE #ArrowDecimal917 (id INT IDENTITY, v {sql_type}({precision},{scale}))"))
                .await
                .expect("create table");
            let result = client
                .bulk_insert("#ArrowDecimal917")
                .keep_nulls(true)
                .batch_size(2)
                .send_arrow(decimal_batch(values, 38, source_scale))
                .await
                .expect("decimal insert");
            assert_eq!(
                result.rows_affected,
                u64::try_from(expected.len()).expect("row count")
            );
            let rows = client
                .simple_query(
                    "SELECT CONVERT(VARCHAR(80), v) AS v FROM #ArrowDecimal917 ORDER BY id",
                )
                .await
                .expect("numeric readback")
                .into_first_result();
            let actual: Vec<_> = rows.iter().map(|row| row.get::<&str, _>("v")).collect();
            assert_eq!(
                actual, expected,
                "{sql_type}({precision},{scale}), source scale {source_scale}"
            );
            client
                .simple_query("DROP TABLE #ArrowDecimal917")
                .await
                .expect("drop table");
        }
    }
}

#[tokio::test]
async fn bulk_arrow_decimal_overflow_recovers_at_batch_boundary() {
    let cfg = common::live_config_or_exit();
    let mut client = Client::connect(&cfg).await.expect("connect");
    for (precision, scale, source_scale, valid, overflow) in [
        (3, 2, 3, 1230, 9995),
        (3, 0, 1, -10, -9995),
        (3, 2, 2, 100, 1000),
        (38, 38, 0, 0, 1),
    ] {
        client
            .simple_query(format!(
                "CREATE TABLE #ArrowOverflow917 (v DECIMAL({precision},{scale}))"
            ))
            .await
            .expect("create table");
        let consumed = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&consumed);
        let batches = [
            decimal_batch(vec![Some(valid), Some(valid)], 38, source_scale),
            decimal_batch(vec![Some(valid), Some(overflow)], 38, source_scale),
            decimal_batch(vec![Some(valid)], 38, source_scale),
        ]
        .into_iter()
        .inspect(move |_| {
            counter.fetch_add(1, Ordering::SeqCst);
        });
        let error = client
            .bulk_insert("#ArrowOverflow917")
            .batch_size(2)
            .use_internal_transaction(true)
            .send_arrow_batches(batches)
            .await
            .expect_err("destination precision overflow");
        assert!(
            error.to_string().contains("precision") || error.to_string().contains("overflow"),
            "{error}"
        );
        assert_eq!(consumed.load(Ordering::SeqCst), 2);
        let rows = client
            .simple_query("SELECT COUNT(*) AS n FROM #ArrowOverflow917")
            .await
            .expect("connection recovered")
            .into_first_result();
        assert_eq!(rows.first().expect("count").get::<i32, _>("n"), Some(2));
        client
            .bulk_insert("#ArrowOverflow917")
            .send_arrow(decimal_batch(vec![Some(valid)], 38, source_scale))
            .await
            .expect("subsequent bulk insert");
        client
            .simple_query("DROP TABLE #ArrowOverflow917")
            .await
            .expect("drop table");
    }
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
