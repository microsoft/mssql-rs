// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#[cfg(test)]
mod common;

mod type_roundtrip {
    use crate::common::{begin_connection, build_tcp_datasource, init_tracing};

    use arrow_array::{
        BinaryArray, BooleanArray, Date32Array, Decimal128Array, FixedSizeBinaryArray,
        Float32Array, Float64Array, Int16Array, Int32Array, Int64Array, RecordBatch, StringArray,
        Time64MicrosecondArray, TimestampMicrosecondArray, UInt8Array,
    };
    use arrow_schema::{DataType, Field, Schema, TimeUnit};

    use mssql_arrow::{ArrowBulkCopy, ArrowQueryReader};
    use std::sync::Arc;

    #[ctor::ctor]
    fn init() {
        init_tracing();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn roundtrip_integer_types() {
        let ds = build_tcp_datasource();
        let mut client = begin_connection(&ds).await;

        client
            .execute(
                "CREATE TABLE #RtInts (
                    tiny TINYINT NOT NULL,
                    small SMALLINT NOT NULL,
                    regular INT NOT NULL,
                    big BIGINT NOT NULL
                )"
                .to_string(),
                None,
                None,
            )
            .await
            .expect("create table");
        client.close_query().await.expect("close");

        let schema = Arc::new(Schema::new(vec![
            Field::new("tiny", DataType::UInt8, false),
            Field::new("small", DataType::Int16, false),
            Field::new("regular", DataType::Int32, false),
            Field::new("big", DataType::Int64, false),
        ]));

        let batch = RecordBatch::try_new(
            schema,
            vec![
                Arc::new(UInt8Array::from(vec![0, 127, 255])),
                Arc::new(Int16Array::from(vec![-32768, 0, 32767])),
                Arc::new(Int32Array::from(vec![i32::MIN, 0, i32::MAX])),
                Arc::new(Int64Array::from(vec![i64::MIN, 0, i64::MAX])),
            ],
        )
        .unwrap();

        ArrowBulkCopy::new(&mut client, "#RtInts")
            .write_batch(&batch)
            .await
            .expect("bulk insert");

        client
            .execute(
                "SELECT tiny, small, regular, big FROM #RtInts ORDER BY regular".to_string(),
                None,
                None,
            )
            .await
            .expect("select");

        let batches = ArrowQueryReader::read_result_set(&mut client, 100)
            .await
            .expect("read");

        assert_eq!(batches.len(), 1);
        assert_eq!(batches[0].num_rows(), 3);
        client.close_query().await.expect("close");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn roundtrip_float_types() {
        let ds = build_tcp_datasource();
        let mut client = begin_connection(&ds).await;

        client
            .execute(
                "CREATE TABLE #RtFloats (
                    real_col REAL NOT NULL,
                    float_col FLOAT NOT NULL
                )"
                .to_string(),
                None,
                None,
            )
            .await
            .expect("create table");
        client.close_query().await.expect("close");

        let schema = Arc::new(Schema::new(vec![
            Field::new("real_col", DataType::Float32, false),
            Field::new("float_col", DataType::Float64, false),
        ]));

        let batch = RecordBatch::try_new(
            schema,
            vec![
                Arc::new(Float32Array::from(vec![0.0_f32, 1.5, -99.9])),
                Arc::new(Float64Array::from(vec![0.0_f64, 123456.789, -1e10])),
            ],
        )
        .unwrap();

        ArrowBulkCopy::new(&mut client, "#RtFloats")
            .write_batch(&batch)
            .await
            .expect("bulk insert");

        client
            .execute(
                "SELECT real_col, float_col FROM #RtFloats".to_string(),
                None,
                None,
            )
            .await
            .expect("select");

        let batches = ArrowQueryReader::read_result_set(&mut client, 100)
            .await
            .expect("read");

        assert_eq!(batches.len(), 1);
        assert_eq!(batches[0].num_rows(), 3);
        client.close_query().await.expect("close");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn roundtrip_string_and_binary() {
        let ds = build_tcp_datasource();
        let mut client = begin_connection(&ds).await;

        client
            .execute(
                "CREATE TABLE #RtStrBin (
                    str_col NVARCHAR(200) NOT NULL,
                    bin_col VARBINARY(100) NOT NULL
                )"
                .to_string(),
                None,
                None,
            )
            .await
            .expect("create table");
        client.close_query().await.expect("close");

        let schema = Arc::new(Schema::new(vec![
            Field::new("str_col", DataType::Utf8, false),
            Field::new("bin_col", DataType::Binary, false),
        ]));

        let batch = RecordBatch::try_new(
            schema,
            vec![
                Arc::new(StringArray::from(vec!["hello", "world", "日本語"])),
                Arc::new(BinaryArray::from(vec![
                    &[0x01, 0x02][..],
                    &[0xFF, 0xFE][..],
                    &[0x00][..],
                ])),
            ],
        )
        .unwrap();

        ArrowBulkCopy::new(&mut client, "#RtStrBin")
            .write_batch(&batch)
            .await
            .expect("bulk insert");

        client
            .execute(
                "SELECT str_col, bin_col FROM #RtStrBin".to_string(),
                None,
                None,
            )
            .await
            .expect("select");

        let batches = ArrowQueryReader::read_result_set(&mut client, 100)
            .await
            .expect("read");

        assert_eq!(batches.len(), 1);
        assert_eq!(batches[0].num_rows(), 3);
        client.close_query().await.expect("close");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn roundtrip_decimal() {
        let ds = build_tcp_datasource();
        let mut client = begin_connection(&ds).await;

        client
            .execute(
                "CREATE TABLE #RtDecimal (dec_col DECIMAL(18,4) NOT NULL)".to_string(),
                None,
                None,
            )
            .await
            .expect("create table");
        client.close_query().await.expect("close");

        let schema = Arc::new(Schema::new(vec![Field::new(
            "dec_col",
            DataType::Decimal128(18, 4),
            false,
        )]));

        // Values represent unscaled: 123456 → 12.3456, 0, -999_9999 → -999.9999
        let batch = RecordBatch::try_new(
            schema,
            vec![Arc::new(
                Decimal128Array::from(vec![123_456_i128, 0, -999_9999])
                    .with_precision_and_scale(18, 4)
                    .unwrap(),
            )],
        )
        .unwrap();

        ArrowBulkCopy::new(&mut client, "#RtDecimal")
            .write_batch(&batch)
            .await
            .expect("bulk insert");

        client
            .execute("SELECT dec_col FROM #RtDecimal".to_string(), None, None)
            .await
            .expect("select");

        let batches = ArrowQueryReader::read_result_set(&mut client, 100)
            .await
            .expect("read");

        assert_eq!(batches.len(), 1);
        assert_eq!(batches[0].num_rows(), 3);
        client.close_query().await.expect("close");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn roundtrip_bool() {
        let ds = build_tcp_datasource();
        let mut client = begin_connection(&ds).await;

        client
            .execute(
                "CREATE TABLE #RtBool (bit_col BIT NOT NULL)".to_string(),
                None,
                None,
            )
            .await
            .expect("create table");
        client.close_query().await.expect("close");

        let schema = Arc::new(Schema::new(vec![Field::new(
            "bit_col",
            DataType::Boolean,
            false,
        )]));

        let batch = RecordBatch::try_new(
            schema,
            vec![Arc::new(BooleanArray::from(vec![true, false, true]))],
        )
        .unwrap();

        ArrowBulkCopy::new(&mut client, "#RtBool")
            .write_batch(&batch)
            .await
            .expect("bulk insert");

        client
            .execute("SELECT bit_col FROM #RtBool".to_string(), None, None)
            .await
            .expect("select");

        let batches = ArrowQueryReader::read_result_set(&mut client, 100)
            .await
            .expect("read");

        assert_eq!(batches.len(), 1);
        assert_eq!(batches[0].num_rows(), 3);
        client.close_query().await.expect("close");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn roundtrip_date_and_time() {
        let ds = build_tcp_datasource();
        let mut client = begin_connection(&ds).await;

        client
            .execute(
                "CREATE TABLE #RtDateTime (
                    date_col DATE NOT NULL,
                    time_col TIME NOT NULL,
                    dt2_col DATETIME2 NOT NULL
                )"
                .to_string(),
                None,
                None,
            )
            .await
            .expect("create table");
        client.close_query().await.expect("close");

        // Date32: days since Unix epoch (1970-01-01)
        // 2024-01-15 = 19737 days since epoch
        let date_vals = vec![0_i32, 19737, -365]; // 1970-01-01, 2024-01-15, 1969-01-01

        // Time: microseconds since midnight
        let time_vals = vec![0_i64, 38_400_000_000, 86_399_000_000]; // midnight, 10:40, 23:59:59

        // Timestamp: microseconds since Unix epoch
        let ts_vals = vec![0_i64, 1_705_312_200_000_000, -86_400_000_000]; // epoch, 2024-01-15 10:30, day before epoch

        let schema = Arc::new(Schema::new(vec![
            Field::new("date_col", DataType::Date32, false),
            Field::new("time_col", DataType::Time64(TimeUnit::Microsecond), false),
            Field::new(
                "dt2_col",
                DataType::Timestamp(TimeUnit::Microsecond, None),
                false,
            ),
        ]));

        let batch = RecordBatch::try_new(
            schema,
            vec![
                Arc::new(Date32Array::from(date_vals)),
                Arc::new(Time64MicrosecondArray::from(time_vals)),
                Arc::new(TimestampMicrosecondArray::from(ts_vals)),
            ],
        )
        .unwrap();

        ArrowBulkCopy::new(&mut client, "#RtDateTime")
            .write_batch(&batch)
            .await
            .expect("bulk insert");

        client
            .execute(
                "SELECT date_col, time_col, dt2_col FROM #RtDateTime".to_string(),
                None,
                None,
            )
            .await
            .expect("select");

        let batches = ArrowQueryReader::read_result_set(&mut client, 100)
            .await
            .expect("read");

        assert_eq!(batches.len(), 1);
        assert_eq!(batches[0].num_rows(), 3);
        client.close_query().await.expect("close");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn roundtrip_uuid() {
        let ds = build_tcp_datasource();
        let mut client = begin_connection(&ds).await;

        client
            .execute(
                "CREATE TABLE #RtUuid (uid UNIQUEIDENTIFIER NOT NULL)".to_string(),
                None,
                None,
            )
            .await
            .expect("create table");
        client.close_query().await.expect("close");

        let schema = Arc::new(Schema::new(vec![Field::new(
            "uid",
            DataType::FixedSizeBinary(16),
            false,
        )]));

        let uuid1 = uuid::Uuid::new_v4();
        let uuid2 = uuid::Uuid::nil();

        let batch = RecordBatch::try_new(
            schema,
            vec![Arc::new(
                FixedSizeBinaryArray::try_from_iter(
                    vec![uuid1.as_bytes().as_slice(), uuid2.as_bytes().as_slice()].into_iter(),
                )
                .unwrap(),
            )],
        )
        .unwrap();

        ArrowBulkCopy::new(&mut client, "#RtUuid")
            .write_batch(&batch)
            .await
            .expect("bulk insert");

        client
            .execute("SELECT uid FROM #RtUuid".to_string(), None, None)
            .await
            .expect("select");

        let batches = ArrowQueryReader::read_result_set(&mut client, 100)
            .await
            .expect("read");

        assert_eq!(batches.len(), 1);
        assert_eq!(batches[0].num_rows(), 2);
        client.close_query().await.expect("close");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn roundtrip_nullable_all_types() {
        let ds = build_tcp_datasource();
        let mut client = begin_connection(&ds).await;

        client
            .execute(
                "CREATE TABLE #RtNullable (
                    int_col INT NULL,
                    str_col NVARCHAR(100) NULL,
                    flt_col FLOAT NULL,
                    dec_col DECIMAL(18,4) NULL
                )"
                .to_string(),
                None,
                None,
            )
            .await
            .expect("create table");
        client.close_query().await.expect("close");

        let schema = Arc::new(Schema::new(vec![
            Field::new("int_col", DataType::Int32, true),
            Field::new("str_col", DataType::Utf8, true),
            Field::new("flt_col", DataType::Float64, true),
            Field::new("dec_col", DataType::Decimal128(18, 4), true),
        ]));

        let batch = RecordBatch::try_new(
            schema,
            vec![
                Arc::new(Int32Array::from(vec![Some(1), None, Some(3)])),
                Arc::new(StringArray::from(vec![None, Some("mid"), None])),
                Arc::new(Float64Array::from(vec![Some(1.1), Some(2.2), None])),
                Arc::new(
                    Decimal128Array::from(vec![None, None, Some(42_0000_i128)])
                        .with_precision_and_scale(18, 4)
                        .unwrap(),
                ),
            ],
        )
        .unwrap();

        ArrowBulkCopy::new(&mut client, "#RtNullable")
            .write_batch(&batch)
            .await
            .expect("bulk insert");

        client
            .execute(
                "SELECT int_col, str_col, flt_col, dec_col FROM #RtNullable".to_string(),
                None,
                None,
            )
            .await
            .expect("select");

        let batches = ArrowQueryReader::read_result_set(&mut client, 100)
            .await
            .expect("read");

        assert_eq!(batches.len(), 1);
        let batch = &batches[0];
        assert_eq!(batch.num_rows(), 3);

        // Verify specific null positions
        assert!(!batch.column(0).is_null(0)); // int_col row 0 = Some(1)
        assert!(batch.column(0).is_null(1)); // int_col row 1 = None

        client.close_query().await.expect("close");
    }
}
