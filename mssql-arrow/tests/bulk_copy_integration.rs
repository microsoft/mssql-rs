// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#[cfg(test)]
mod common;

mod bulk_copy_integration {
    use crate::common::{begin_connection, build_tcp_datasource, init_tracing};

    use arrow_array::{
        Decimal128Array, Float64Array, Int32Array, Int64Array, RecordBatch, StringArray,
    };
    use arrow_schema::{DataType, Field, Schema};
    use mssql_tds::connection::tds_client::{ResultSet, ResultSetClient};
    use mssql_tds::datatypes::column_values::ColumnValues;
    use std::sync::Arc;

    use mssql_arrow::ArrowBulkCopy;

    #[ctor::ctor]
    fn init() {
        init_tracing();
    }

    fn create_five_col_batch(row_count: usize) -> RecordBatch {
        let ids: Vec<i32> = (1..=row_count as i32).collect();
        let bigints: Vec<i64> = ids.iter().map(|&i| i as i64 * 1_000_000).collect();
        let floats: Vec<f64> = ids.iter().map(|&i| i as f64 * 1.5).collect();
        let strings: Vec<String> = ids.iter().map(|i| format!("row_{i}")).collect();
        let decimals: Vec<i128> = ids.iter().map(|&i| i as i128 * 10_000).collect();

        let schema = Arc::new(Schema::new(vec![
            Field::new("id", DataType::Int32, false),
            Field::new("big_val", DataType::Int64, false),
            Field::new("flt_val", DataType::Float64, false),
            Field::new("str_val", DataType::Utf8, false),
            Field::new("dec_val", DataType::Decimal128(18, 4), false),
        ]));

        RecordBatch::try_new(
            schema,
            vec![
                Arc::new(Int32Array::from(ids)),
                Arc::new(Int64Array::from(bigints)),
                Arc::new(Float64Array::from(floats)),
                Arc::new(StringArray::from(strings)),
                Arc::new(
                    Decimal128Array::from(decimals)
                        .with_precision_and_scale(18, 4)
                        .unwrap(),
                ),
            ],
        )
        .unwrap()
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn bulk_insert_five_columns() {
        let ds = build_tcp_datasource();
        let mut client = begin_connection(&ds).await;

        client
            .execute(
                "CREATE TABLE #ArrowBulk5Col (
                    id INT NOT NULL,
                    big_val BIGINT NOT NULL,
                    flt_val FLOAT NOT NULL,
                    str_val NVARCHAR(200) NOT NULL,
                    dec_val DECIMAL(18,4) NOT NULL
                )"
                .to_string(),
                None,
                None,
            )
            .await
            .expect("create table");
        client.close_query().await.expect("close query");

        let batch = create_five_col_batch(100);
        let result = ArrowBulkCopy::new(&mut client, "#ArrowBulk5Col")
            .write_batch(&batch)
            .await
            .expect("bulk insert");

        assert_eq!(result.rows_affected, 100);

        // Verify row count
        client
            .execute(
                "SELECT COUNT(*) FROM #ArrowBulk5Col".to_string(),
                None,
                None,
            )
            .await
            .expect("select count");

        if let Some(rs) = client.get_current_resultset()
            && let Some(row) = rs.next_row().await.expect("next row")
        {
            assert_eq!(row[0], ColumnValues::Int(100));
        }
        client.close_query().await.expect("close");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn bulk_insert_with_nulls() {
        let ds = build_tcp_datasource();
        let mut client = begin_connection(&ds).await;

        client
            .execute(
                "CREATE TABLE #ArrowBulkNulls (
                    id INT NOT NULL,
                    nullable_str NVARCHAR(100) NULL,
                    nullable_int INT NULL
                )"
                .to_string(),
                None,
                None,
            )
            .await
            .expect("create table");
        client.close_query().await.expect("close query");

        let schema = Arc::new(Schema::new(vec![
            Field::new("id", DataType::Int32, false),
            Field::new("nullable_str", DataType::Utf8, true),
            Field::new("nullable_int", DataType::Int32, true),
        ]));

        let batch = RecordBatch::try_new(
            schema,
            vec![
                Arc::new(Int32Array::from(vec![1, 2, 3])),
                Arc::new(StringArray::from(vec![Some("hello"), None, Some("world")])),
                Arc::new(Int32Array::from(vec![Some(42), Some(99), None])),
            ],
        )
        .unwrap();

        let result = ArrowBulkCopy::new(&mut client, "#ArrowBulkNulls")
            .write_batch(&batch)
            .await
            .expect("bulk insert");

        assert_eq!(result.rows_affected, 3);

        // Verify nulls
        client
            .execute(
                "SELECT COUNT(*) FROM #ArrowBulkNulls WHERE nullable_str IS NULL".to_string(),
                None,
                None,
            )
            .await
            .expect("query nulls");

        if let Some(rs) = client.get_current_resultset()
            && let Some(row) = rs.next_row().await.expect("next row")
        {
            assert_eq!(row[0], ColumnValues::Int(1));
        }
        client.close_query().await.expect("close");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn bulk_insert_zero_row_batch() {
        let ds = build_tcp_datasource();
        let mut client = begin_connection(&ds).await;

        client
            .execute(
                "CREATE TABLE #ArrowBulkEmpty (id INT NOT NULL)".to_string(),
                None,
                None,
            )
            .await
            .expect("create table");
        client.close_query().await.expect("close query");

        let schema = Arc::new(Schema::new(vec![Field::new("id", DataType::Int32, false)]));
        let batch =
            RecordBatch::try_new(schema, vec![Arc::new(Int32Array::from(Vec::<i32>::new()))])
                .unwrap();

        let result = ArrowBulkCopy::new(&mut client, "#ArrowBulkEmpty")
            .write_batch(&batch)
            .await
            .expect("bulk insert empty");

        assert_eq!(result.rows_affected, 0);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn bulk_insert_multi_batch_streaming() {
        let ds = build_tcp_datasource();
        let mut client = begin_connection(&ds).await;

        client
            .execute(
                "CREATE TABLE #ArrowBulkStream (
                    id INT NOT NULL,
                    big_val BIGINT NOT NULL,
                    flt_val FLOAT NOT NULL,
                    str_val NVARCHAR(200) NOT NULL,
                    dec_val DECIMAL(18,4) NOT NULL
                )"
                .to_string(),
                None,
                None,
            )
            .await
            .expect("create table");
        client.close_query().await.expect("close query");

        let batches: Vec<RecordBatch> = (0..5).map(|_| create_five_col_batch(1000)).collect();

        let result = ArrowBulkCopy::new(&mut client, "#ArrowBulkStream")
            .write_batches(&batches)
            .await
            .expect("bulk insert stream");

        assert_eq!(result.rows_affected, 5000);

        // Verify total row count
        client
            .execute(
                "SELECT COUNT(*) FROM #ArrowBulkStream".to_string(),
                None,
                None,
            )
            .await
            .expect("count query");

        if let Some(rs) = client.get_current_resultset()
            && let Some(row) = rs.next_row().await.expect("next row")
        {
            assert_eq!(row[0], ColumnValues::Int(5000));
        }
        client.close_query().await.expect("close");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn bulk_insert_with_bulk_copy_options() {
        let ds = build_tcp_datasource();
        let mut client = begin_connection(&ds).await;

        client
            .execute(
                "CREATE TABLE #ArrowBulkOpts (
                    id INT NOT NULL,
                    val NVARCHAR(50) NOT NULL
                )"
                .to_string(),
                None,
                None,
            )
            .await
            .expect("create table");
        client.close_query().await.expect("close query");

        let schema = Arc::new(Schema::new(vec![
            Field::new("id", DataType::Int32, false),
            Field::new("val", DataType::Utf8, false),
        ]));

        let batch = RecordBatch::try_new(
            schema,
            vec![
                Arc::new(Int32Array::from(vec![1, 2])),
                Arc::new(StringArray::from(vec!["a", "b"])),
            ],
        )
        .unwrap();

        let result = ArrowBulkCopy::new(&mut client, "#ArrowBulkOpts")
            .batch_size(1)
            .table_lock(true)
            .fire_triggers(false)
            .write_batch(&batch)
            .await
            .expect("bulk insert with options");

        assert_eq!(result.rows_affected, 2);
    }
}
