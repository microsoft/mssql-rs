// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#[cfg(test)]
mod common;

mod bulk_copy_bigint_tests {
    use crate::common::{begin_connection, build_tcp_datasource, init_tracing};
    use async_trait::async_trait;
    use mssql_tds::connection::bulk_copy::{BulkCopy, BulkLoadRow};
    use mssql_tds::connection::tds_client::ResultSet;
    use mssql_tds::core::TdsResult;
    use mssql_tds::datatypes::column_values::ColumnValues;

    #[ctor::ctor]
    fn init() {
        init_tracing();
    }

    // Define a simple test data structure for BIGINT
    #[derive(Debug, Clone)]
    struct TestBigIntUser {
        id: i64,
        value1: i64,
        value2: i64,
        value3: i64,
    }

    #[async_trait]
    impl BulkLoadRow for TestBigIntUser {
        async fn write_to_packet(
            &self,
            writer: &mut mssql_tds::message::bulk_load::StreamingBulkLoadWriter<'_>,
            column_index: &mut usize,
        ) -> TdsResult<()> {
            writer
                .write_column_value(*column_index, &ColumnValues::BigInt(self.id))
                .await?;
            *column_index += 1;
            writer
                .write_column_value(*column_index, &ColumnValues::BigInt(self.value1))
                .await?;
            *column_index += 1;
            writer
                .write_column_value(*column_index, &ColumnValues::BigInt(self.value2))
                .await?;
            *column_index += 1;
            writer
                .write_column_value(*column_index, &ColumnValues::BigInt(self.value3))
                .await?;
            *column_index += 1;
            Ok(())
        }
    }

    #[async_trait]
    impl BulkLoadRow for &TestBigIntUser {
        async fn write_to_packet(
            &self,
            writer: &mut mssql_tds::message::bulk_load::StreamingBulkLoadWriter<'_>,
            column_index: &mut usize,
        ) -> TdsResult<()> {
            writer
                .write_column_value(*column_index, &ColumnValues::BigInt(self.id))
                .await?;
            *column_index += 1;
            writer
                .write_column_value(*column_index, &ColumnValues::BigInt(self.value1))
                .await?;
            *column_index += 1;
            writer
                .write_column_value(*column_index, &ColumnValues::BigInt(self.value2))
                .await?;
            *column_index += 1;
            writer
                .write_column_value(*column_index, &ColumnValues::BigInt(self.value3))
                .await?;
            *column_index += 1;
            Ok(())
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_bulk_copy_bigint_simple_insert() {
        let mut client = begin_connection(&build_tcp_datasource()).await;

        // Create temp table (automatically cleaned up)
        client
            .execute(
                "CREATE TABLE #BulkCopyTestBigInt (
                    id BIGINT NOT NULL,
                    value1 BIGINT NOT NULL,
                    value2 BIGINT NOT NULL,
                    value3 BIGINT NOT NULL
                )"
                .to_string(),
                (),
            )
            .await
            .expect("Failed to create test table");

        // Close the query to free up the connection
        client.close_query().await.expect("Failed to close query");

        // Prepare test data with typical BIGINT values
        let test_data = vec![
            TestBigIntUser {
                id: 1,
                value1: 1000000000000,
                value2: 2000000000000,
                value3: 3000000000000,
            },
            TestBigIntUser {
                id: 2,
                value1: 1000000000001,
                value2: 2000000000001,
                value3: 3000000000001,
            },
            TestBigIntUser {
                id: 3,
                value1: 1000000000002,
                value2: 2000000000002,
                value3: 3000000000002,
            },
        ];

        // Execute bulk copy
        let result = {
            let bulk_copy = BulkCopy::new(&mut client, "#BulkCopyTestBigInt");
            bulk_copy
                .batch_size(1000)
                .write_to_server_zerocopy(&test_data)
                .await
                .expect("Bulk copy failed")
        };

        println!("Bulk copy result: {result:?}");

        // Check actual row count in database before assertion
        client
            .execute(
                "SELECT COUNT(*) as cnt FROM #BulkCopyTestBigInt".to_string(),
                (),
            )
            .await
            .expect("Failed to count rows");

        if let Some(resultset) = client.get_current_resultset()
            && let Some(row) = resultset.next_row().await.expect("Failed to read count")
        {
            println!("DEBUG: Actual rows in database: {:?}", row[0]);
        }
        client
            .close_query()
            .await
            .expect("Failed to close count query");

        assert_eq!(result.rows_affected, 3, "Expected 3 rows to be inserted");

        // Verify the data was inserted
        client
            .execute(
                "SELECT id, value1, value2, value3 FROM #BulkCopyTestBigInt ORDER BY id"
                    .to_string(),
                (),
            )
            .await
            .expect("Failed to select data");

        let mut row_count = 0;
        if let Some(resultset) = client.get_current_resultset() {
            while let Some(row) = resultset.next_row().await.expect("Failed to read row") {
                row_count += 1;
                match row_count {
                    1 => {
                        assert_eq!(row[0], ColumnValues::BigInt(1));
                        assert_eq!(row[1], ColumnValues::BigInt(1000000000000));
                        assert_eq!(row[2], ColumnValues::BigInt(2000000000000));
                        assert_eq!(row[3], ColumnValues::BigInt(3000000000000));
                    }
                    2 => {
                        assert_eq!(row[0], ColumnValues::BigInt(2));
                        assert_eq!(row[1], ColumnValues::BigInt(1000000000001));
                        assert_eq!(row[2], ColumnValues::BigInt(2000000000001));
                        assert_eq!(row[3], ColumnValues::BigInt(3000000000001));
                    }
                    3 => {
                        assert_eq!(row[0], ColumnValues::BigInt(3));
                        assert_eq!(row[1], ColumnValues::BigInt(1000000000002));
                        assert_eq!(row[2], ColumnValues::BigInt(2000000000002));
                        assert_eq!(row[3], ColumnValues::BigInt(3000000000002));
                    }
                    _ => panic!("Unexpected row"),
                }
            }
        }

        assert_eq!(row_count, 3, "Expected 3 rows to be returned");

        // Temp table will be automatically dropped when connection closes
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_bulk_copy_bigint_large_batch() {
        let mut client = begin_connection(&build_tcp_datasource()).await;

        // Create test table
        client
            .execute(
                "CREATE TABLE #BulkCopyLargeBigInt (
                    id BIGINT NOT NULL,
                    value1 BIGINT NOT NULL,
                    value2 BIGINT NOT NULL,
                    value3 BIGINT NOT NULL
                )"
                .to_string(),
                (),
            )
            .await
            .expect("Failed to create test table");

        client.close_query().await.expect("Failed to close query");

        // Generate 100 rows (reduced from 1000 for faster testing)
        let test_data: Vec<TestBigIntUser> = (1..=100)
            .map(|i| TestBigIntUser {
                id: i,
                value1: i * 10000000000,
                value2: i * 20000000000,
                value3: i * 30000000000,
            })
            .collect();

        // Execute bulk copy - use default batch size (all in one batch)
        let result = {
            let mut bulk_copy = BulkCopy::new(&mut client, "#BulkCopyLargeBigInt");
            bulk_copy
                .write_to_server_zerocopy(&test_data)
                .await
                .expect("Bulk copy failed")
        };

        println!("Bulk copy result: {result:?}");
        assert_eq!(
            result.rows_affected, 100,
            "Expected 100 rows to be inserted"
        );
        assert!(result.rows_per_second > 0.0, "Expected positive throughput");

        // Verify count
        client
            .execute("SELECT COUNT(*) FROM #BulkCopyLargeBigInt".to_string(), ())
            .await
            .expect("Failed to select count");

        if let Some(resultset) = client.get_current_resultset()
            && let Some(row) = resultset.next_row().await.expect("Failed to read row")
        {
            assert_eq!(row[0], ColumnValues::Int(100));
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_bulk_copy_bigint_with_nulls() {
        let mut client = begin_connection(&build_tcp_datasource()).await;

        // Create test table with nullable columns
        client
            .execute(
                "CREATE TABLE #BulkCopyNullsBigInt (
                    id BIGINT NOT NULL,
                    value1 BIGINT NULL,
                    value2 BIGINT NULL
                )"
                .to_string(),
                (),
            )
            .await
            .expect("Failed to create test table");

        client.close_query().await.expect("Failed to close query");

        // Define structure with nullable fields
        #[derive(Debug, Clone)]
        struct NullableBigIntUser {
            id: i64,
            value1: Option<i64>,
            value2: Option<i64>,
        }

        #[async_trait]
        impl BulkLoadRow for NullableBigIntUser {
            async fn write_to_packet(
                &self,
                writer: &mut mssql_tds::message::bulk_load::StreamingBulkLoadWriter<'_>,
                column_index: &mut usize,
            ) -> TdsResult<()> {
                writer
                    .write_column_value(*column_index, &ColumnValues::BigInt(self.id))
                    .await?;
                *column_index += 1;
                let value1 = self
                    .value1
                    .map(ColumnValues::BigInt)
                    .unwrap_or(ColumnValues::Null);
                writer.write_column_value(*column_index, &value1).await?;
                *column_index += 1;
                let value2 = self
                    .value2
                    .map(ColumnValues::BigInt)
                    .unwrap_or(ColumnValues::Null);
                writer.write_column_value(*column_index, &value2).await?;
                *column_index += 1;
                Ok(())
            }
        }

        #[async_trait]
        impl BulkLoadRow for &NullableBigIntUser {
            async fn write_to_packet(
                &self,
                writer: &mut mssql_tds::message::bulk_load::StreamingBulkLoadWriter<'_>,
                column_index: &mut usize,
            ) -> TdsResult<()> {
                writer
                    .write_column_value(*column_index, &ColumnValues::BigInt(self.id))
                    .await?;
                *column_index += 1;
                let value1 = self
                    .value1
                    .map(ColumnValues::BigInt)
                    .unwrap_or(ColumnValues::Null);
                writer.write_column_value(*column_index, &value1).await?;
                *column_index += 1;
                let value2 = self
                    .value2
                    .map(ColumnValues::BigInt)
                    .unwrap_or(ColumnValues::Null);
                writer.write_column_value(*column_index, &value2).await?;
                *column_index += 1;
                Ok(())
            }
        }

        let test_data = vec![
            NullableBigIntUser {
                id: 1,
                value1: Some(9000000000000),
                value2: Some(8000000000000),
            },
            NullableBigIntUser {
                id: 2,
                value1: None,
                value2: Some(8000000000001),
            },
            NullableBigIntUser {
                id: 3,
                value1: Some(9000000000002),
                value2: None,
            },
            NullableBigIntUser {
                id: 4,
                value1: None,
                value2: None,
            },
        ];

        let result = {
            let mut bulk_copy = BulkCopy::new(&mut client, "#BulkCopyNullsBigInt");
            bulk_copy
                .write_to_server_zerocopy(&test_data)
                .await
                .expect("Bulk copy failed")
        };

        assert_eq!(result.rows_affected, 4);

        // Verify the data
        client
            .execute(
                "SELECT id, value1, value2 FROM #BulkCopyNullsBigInt ORDER BY id".to_string(),
                (),
            )
            .await
            .expect("Failed to select data");

        let mut row_count = 0;
        if let Some(resultset) = client.get_current_resultset() {
            while let Some(row) = resultset.next_row().await.expect("Failed to read row") {
                row_count += 1;
                match row_count {
                    1 => {
                        assert_eq!(row[0], ColumnValues::BigInt(1));
                        assert_eq!(row[1], ColumnValues::BigInt(9000000000000));
                        assert_eq!(row[2], ColumnValues::BigInt(8000000000000));
                    }
                    2 => {
                        assert_eq!(row[0], ColumnValues::BigInt(2));
                        assert_eq!(row[1], ColumnValues::Null);
                        assert_eq!(row[2], ColumnValues::BigInt(8000000000001));
                    }
                    3 => {
                        assert_eq!(row[0], ColumnValues::BigInt(3));
                        assert_eq!(row[1], ColumnValues::BigInt(9000000000002));
                        assert_eq!(row[2], ColumnValues::Null);
                    }
                    4 => {
                        assert_eq!(row[0], ColumnValues::BigInt(4));
                        assert_eq!(row[1], ColumnValues::Null);
                        assert_eq!(row[2], ColumnValues::Null);
                    }
                    _ => panic!("Unexpected row"),
                }
            }
        }

        assert_eq!(row_count, 4);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_bulk_copy_bigint_empty_dataset() {
        let mut client = begin_connection(&build_tcp_datasource()).await;

        client
            .execute(
                "CREATE TABLE #BulkCopyEmptyBigInt (
                    id BIGINT NOT NULL,
                    value1 BIGINT NOT NULL
                )"
                .to_string(),
                (),
            )
            .await
            .expect("Failed to create test table");

        client.close_query().await.expect("Failed to close query");

        let test_data: Vec<TestBigIntUser> = vec![];

        let result = {
            let mut bulk_copy = BulkCopy::new(&mut client, "#BulkCopyEmptyBigInt");
            bulk_copy
                .write_to_server_zerocopy(&test_data)
                .await
                .expect("Bulk copy should handle empty dataset")
        };

        assert_eq!(result.rows_affected, 0);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_bulk_copy_bigint_null_to_non_nullable_column() {
        let mut client = begin_connection(&build_tcp_datasource()).await;

        // Create test table where all columns are non-nullable
        client
            .execute(
                "CREATE TABLE #BulkCopyNonNullableBigInt (
                    id BIGINT NOT NULL,
                    value1 BIGINT NOT NULL,
                    value2 BIGINT NOT NULL
                )"
                .to_string(),
                (),
            )
            .await
            .expect("Failed to create test table");

        client.close_query().await.expect("Failed to close query");

        // Define structure that can hold nullable values
        #[derive(Debug, Clone)]
        struct NullableBigIntUser {
            id: i64,
            value1: Option<i64>,
            value2: Option<i64>,
        }

        #[async_trait]
        impl BulkLoadRow for NullableBigIntUser {
            async fn write_to_packet(
                &self,
                writer: &mut mssql_tds::message::bulk_load::StreamingBulkLoadWriter<'_>,
                column_index: &mut usize,
            ) -> TdsResult<()> {
                writer
                    .write_column_value(*column_index, &ColumnValues::BigInt(self.id))
                    .await?;
                *column_index += 1;
                let value1 = self
                    .value1
                    .map(ColumnValues::BigInt)
                    .unwrap_or(ColumnValues::Null);
                writer.write_column_value(*column_index, &value1).await?;
                *column_index += 1;
                let value2 = self
                    .value2
                    .map(ColumnValues::BigInt)
                    .unwrap_or(ColumnValues::Null);
                writer.write_column_value(*column_index, &value2).await?;
                *column_index += 1;
                Ok(())
            }
        }

        #[async_trait]
        impl BulkLoadRow for &NullableBigIntUser {
            async fn write_to_packet(
                &self,
                writer: &mut mssql_tds::message::bulk_load::StreamingBulkLoadWriter<'_>,
                column_index: &mut usize,
            ) -> TdsResult<()> {
                writer
                    .write_column_value(*column_index, &ColumnValues::BigInt(self.id))
                    .await?;
                *column_index += 1;
                let value1 = self
                    .value1
                    .map(ColumnValues::BigInt)
                    .unwrap_or(ColumnValues::Null);
                writer.write_column_value(*column_index, &value1).await?;
                *column_index += 1;
                let value2 = self
                    .value2
                    .map(ColumnValues::BigInt)
                    .unwrap_or(ColumnValues::Null);
                writer.write_column_value(*column_index, &value2).await?;
                *column_index += 1;
                Ok(())
            }
        }

        // Create test data with NULL in non-nullable column
        let test_data = vec![
            NullableBigIntUser {
                id: 1,
                value1: Some(7000000000000),
                value2: Some(6000000000000),
            },
            NullableBigIntUser {
                id: 2,
                value1: None, // NULL to non-nullable column - should fail on server
                value2: Some(6000000000001),
            },
        ];

        let result = {
            let mut bulk_copy = BulkCopy::new(&mut client, "#BulkCopyNonNullableBigInt");
            bulk_copy.write_to_server_zerocopy(&test_data).await
        };

        // Server should reject this with error 515 (cannot insert NULL into non-nullable column)
        assert!(
            result.is_err(),
            "Expected error when inserting NULL into non-nullable column"
        );

        let error = result.unwrap_err();
        let error_msg = format!("{:?}", error);

        // Verify it's the expected SQL Server error (error 515)
        assert!(
            error_msg.contains("515") || error_msg.contains("NULL") || error_msg.contains("null"),
            "Expected error about NULL constraint violation, got: {}",
            error_msg
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_bulk_copy_bigint_extreme_values() {
        let mut client = begin_connection(&build_tcp_datasource()).await;

        // Create temp table
        client
            .execute(
                "CREATE TABLE #BulkCopyTestBigIntExtreme (
                    id BIGINT NOT NULL,
                    min_value BIGINT NOT NULL,
                    max_value BIGINT NOT NULL,
                    zero_value BIGINT NOT NULL
                )"
                .to_string(),
                (),
            )
            .await
            .expect("Failed to create test table");

        client.close_query().await.expect("Failed to close query");

        // Test with extreme BIGINT values
        let test_data = vec![
            TestBigIntUser {
                id: 1,
                value1: i64::MIN, // -9,223,372,036,854,775,808
                value2: i64::MAX, // 9,223,372,036,854,775,807
                value3: 0,
            },
            TestBigIntUser {
                id: 2,
                value1: -1,
                value2: 1,
                value3: 0,
            },
        ];

        // Execute bulk copy
        let result = {
            let mut bulk_copy = BulkCopy::new(&mut client, "#BulkCopyTestBigIntExtreme");
            bulk_copy
                .write_to_server_zerocopy(&test_data)
                .await
                .expect("Bulk copy failed")
        };

        assert_eq!(result.rows_affected, 2);

        // Verify the extreme values were inserted correctly
        client
            .execute("SELECT id, min_value, max_value, zero_value FROM #BulkCopyTestBigIntExtreme ORDER BY id".to_string(), ())
            .await
            .expect("Failed to select data");

        let mut row_count = 0;
        if let Some(resultset) = client.get_current_resultset() {
            while let Some(row) = resultset.next_row().await.expect("Failed to read row") {
                row_count += 1;
                match row_count {
                    1 => {
                        assert_eq!(row[0], ColumnValues::BigInt(1));
                        assert_eq!(row[1], ColumnValues::BigInt(i64::MIN));
                        assert_eq!(row[2], ColumnValues::BigInt(i64::MAX));
                        assert_eq!(row[3], ColumnValues::BigInt(0));
                    }
                    2 => {
                        assert_eq!(row[0], ColumnValues::BigInt(2));
                        assert_eq!(row[1], ColumnValues::BigInt(-1));
                        assert_eq!(row[2], ColumnValues::BigInt(1));
                        assert_eq!(row[3], ColumnValues::BigInt(0));
                    }
                    _ => panic!("Unexpected row"),
                }
            }
        }

        assert_eq!(row_count, 2);
    }
}
