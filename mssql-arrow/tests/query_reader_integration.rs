// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#[cfg(test)]
mod common;

mod query_reader_integration {
    use crate::common::{begin_connection, build_tcp_datasource, init_tracing};

    use mssql_arrow::ArrowQueryReader;

    #[ctor::ctor]
    fn init() {
        init_tracing();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn read_diverse_types() {
        let ds = build_tcp_datasource();
        let mut client = begin_connection(&ds).await;

        client
            .execute(
                "SELECT
                    CAST(42 AS INT) AS int_col,
                    CAST(99999999 AS BIGINT) AS bigint_col,
                    CAST(3.14 AS FLOAT) AS float_col,
                    CAST(N'hello' AS NVARCHAR(50)) AS str_col,
                    CAST(123.4500 AS DECIMAL(18,4)) AS dec_col,
                    CAST('2024-01-15' AS DATE) AS date_col,
                    CAST('2024-01-15 10:30:00' AS DATETIME2) AS dt2_col"
                    .to_string(),
                None,
                None,
            )
            .await
            .expect("execute query");

        let batches = ArrowQueryReader::read_result_set(&mut client, 1000)
            .await
            .expect("read result set");

        assert_eq!(batches.len(), 1);
        let batch = &batches[0];
        assert_eq!(batch.num_rows(), 1);
        assert_eq!(batch.num_columns(), 7);

        client.close_query().await.expect("close");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn read_multiple_rows_with_batching() {
        let ds = build_tcp_datasource();
        let mut client = begin_connection(&ds).await;

        // Generate 25 rows, batch_size=10 → should produce 3 batches (10+10+5)
        client
            .execute(
                "SELECT TOP 25 ROW_NUMBER() OVER (ORDER BY (SELECT NULL)) AS row_num
                 FROM sys.objects"
                    .to_string(),
                None,
                None,
            )
            .await
            .expect("execute query");

        let batches = ArrowQueryReader::read_result_set(&mut client, 10)
            .await
            .expect("read result set");

        assert!(
            batches.len() >= 2,
            "expected at least 2 batches, got {}",
            batches.len()
        );

        let total_rows: usize = batches.iter().map(|b| b.num_rows()).sum();
        assert_eq!(total_rows, 25);

        client.close_query().await.expect("close");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn read_null_handling() {
        let ds = build_tcp_datasource();
        let mut client = begin_connection(&ds).await;

        client
            .execute(
                "SELECT
                    CAST(NULL AS INT) AS null_int,
                    CAST(NULL AS NVARCHAR(50)) AS null_str,
                    CAST(NULL AS FLOAT) AS null_flt,
                    CAST(1 AS INT) AS non_null_int"
                    .to_string(),
                None,
                None,
            )
            .await
            .expect("execute query");

        let batches = ArrowQueryReader::read_result_set(&mut client, 1000)
            .await
            .expect("read result set");

        assert_eq!(batches.len(), 1);
        let batch = &batches[0];
        assert_eq!(batch.num_rows(), 1);

        // Verify nulls
        assert!(batch.column(0).is_null(0));
        assert!(batch.column(1).is_null(0));
        assert!(batch.column(2).is_null(0));
        assert!(!batch.column(3).is_null(0));

        client.close_query().await.expect("close");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn read_empty_result_set() {
        let ds = build_tcp_datasource();
        let mut client = begin_connection(&ds).await;

        client
            .execute(
                "CREATE TABLE #ArrowEmpty (id INT); SELECT * FROM #ArrowEmpty".to_string(),
                None,
                None,
            )
            .await
            .expect("execute query");

        let batches = ArrowQueryReader::read_result_set(&mut client, 1000)
            .await
            .expect("read result set");

        // Empty result set produces no batches
        assert!(batches.is_empty());

        client.close_query().await.expect("close");
    }
}
