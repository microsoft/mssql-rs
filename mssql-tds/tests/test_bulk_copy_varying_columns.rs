// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Tests for bulk copy with varying column counts.
//!
//! These tests verify that when rows have different numbers of columns,
//! the bulk copy operation fails appropriately and the connection remains usable.

#[cfg(test)]
mod common;

mod bulk_copy_varying_columns_tests {
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

    #[derive(Debug, Clone)]
    struct VariableColumnRow {
        col1: i32,
        col2: i32,
        col3: Option<i32>,
        col4: Option<i32>,
    }

    #[async_trait]
    impl BulkLoadRow for VariableColumnRow {
        async fn write_to_packet(
            &self,
            writer: &mut mssql_tds::message::bulk_load::StreamingBulkLoadWriter<'_>,
            column_index: &mut usize,
        ) -> TdsResult<()> {
            writer
                .write_column_value(*column_index, &ColumnValues::Int(self.col1))
                .await?;
            *column_index += 1;
            writer
                .write_column_value(*column_index, &ColumnValues::Int(self.col2))
                .await?;
            *column_index += 1;

            if let Some(col3_val) = self.col3 {
                writer
                    .write_column_value(*column_index, &ColumnValues::Int(col3_val))
                    .await?;
                *column_index += 1;
            }

            if let Some(col4_val) = self.col4 {
                writer
                    .write_column_value(*column_index, &ColumnValues::Int(col4_val))
                    .await?;
                *column_index += 1;
            }

            Ok(())
        }
    }

    #[async_trait]
    impl BulkLoadRow for &VariableColumnRow {
        async fn write_to_packet(
            &self,
            writer: &mut mssql_tds::message::bulk_load::StreamingBulkLoadWriter<'_>,
            column_index: &mut usize,
        ) -> TdsResult<()> {
            (*self).write_to_packet(writer, column_index).await
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_bulk_copy_subsequent_row_has_more_columns() {
        let mut client = begin_connection(&build_tcp_datasource()).await;

        client
            .execute(
                "CREATE TABLE #BulkCopyVaryingColumnsMoreTest (
                    col1 INT NOT NULL,
                    col2 INT NOT NULL,
                    col3 INT NOT NULL
                )"
                .to_string(),
                (),
            )
            .await
            .expect("Failed to create temp table");

        let rows = vec![
            VariableColumnRow {
                col1: 1,
                col2: 100,
                col3: Some(30),
                col4: None,
            },
            VariableColumnRow {
                col1: 2,
                col2: 200,
                col3: Some(25),
                col4: Some(999),
            },
        ];

        let bulk_result = {
            let bulk_copy = BulkCopy::new(&mut client, "#BulkCopyVaryingColumnsMoreTest");
            bulk_copy
                .batch_size(1000)
                .write_to_server_zerocopy(&rows)
                .await
        };

        assert!(
            bulk_result.is_err(),
            "Bulk copy with varying column counts should fail"
        );

        let err_msg = bulk_result.unwrap_err().to_string().to_lowercase();
        assert!(
            err_msg.contains("column")
                && (err_msg.contains("first row") || err_msg.contains("expected")),
            "Error message should mention column count and first row, got: {}",
            err_msg
        );

        let select_result = client.execute("SELECT 1 AS test_col".to_string(), ()).await;
        assert!(
            select_result.is_ok(),
            "Connection should still be usable after bulk copy error"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_bulk_copy_subsequent_row_has_fewer_columns() {
        let mut client = begin_connection(&build_tcp_datasource()).await;

        client
            .execute(
                "CREATE TABLE #BulkCopyVaryingColumnsFewerTest (
                    col1 INT NOT NULL,
                    col2 INT NOT NULL,
                    col3 INT NOT NULL
                )"
                .to_string(),
                (),
            )
            .await
            .expect("Failed to create temp table");

        let rows = vec![
            VariableColumnRow {
                col1: 1,
                col2: 100,
                col3: Some(30),
                col4: None,
            },
            VariableColumnRow {
                col1: 2,
                col2: 200,
                col3: None,
                col4: None,
            },
        ];

        let bulk_result = {
            let bulk_copy = BulkCopy::new(&mut client, "#BulkCopyVaryingColumnsFewerTest");
            bulk_copy
                .batch_size(1000)
                .write_to_server_zerocopy(&rows)
                .await
        };

        assert!(
            bulk_result.is_err(),
            "Bulk copy with varying column counts should fail"
        );

        let err_msg = bulk_result.unwrap_err().to_string().to_lowercase();
        assert!(
            err_msg.contains("column")
                && (err_msg.contains("first row") || err_msg.contains("expected")),
            "Error message should mention column count and first row, got: {}",
            err_msg
        );

        let select_result = client.execute("SELECT 1 AS test_col".to_string(), ()).await;
        assert!(
            select_result.is_ok(),
            "Connection should still be usable after bulk copy error"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_bulk_copy_consistent_columns_success() {
        let mut client = begin_connection(&build_tcp_datasource()).await;

        client
            .execute(
                "CREATE TABLE #BulkCopyConsistentTest (
                    col1 INT NOT NULL,
                    col2 INT NOT NULL,
                    col3 INT NOT NULL
                )"
                .to_string(),
                (),
            )
            .await
            .expect("Failed to create temp table");

        let rows = vec![
            VariableColumnRow {
                col1: 1,
                col2: 100,
                col3: Some(30),
                col4: None,
            },
            VariableColumnRow {
                col1: 2,
                col2: 200,
                col3: Some(25),
                col4: None,
            },
            VariableColumnRow {
                col1: 3,
                col2: 300,
                col3: Some(35),
                col4: None,
            },
            VariableColumnRow {
                col1: 4,
                col2: 400,
                col3: Some(28),
                col4: None,
            },
        ];

        let bulk_result = {
            let bulk_copy = BulkCopy::new(&mut client, "#BulkCopyConsistentTest");
            bulk_copy
                .batch_size(1000)
                .write_to_server_zerocopy(&rows)
                .await
        };

        assert!(
            bulk_result.is_ok(),
            "Bulk copy with consistent column counts should succeed"
        );
        assert_eq!(
            bulk_result.unwrap().rows_affected,
            4,
            "Expected 4 rows to be copied successfully"
        );

        client
            .execute(
                "SELECT COUNT(*) FROM #BulkCopyConsistentTest".to_string(),
                (),
            )
            .await
            .expect("Failed to count rows");

        if let Some(resultset) = client.get_current_resultset() {
            let row = resultset
                .next_row()
                .await
                .expect("Failed to read count")
                .expect("Expected count row");
            assert_eq!(row[0], ColumnValues::Int(4), "Expected 4 rows in table");
        }
    }
}
