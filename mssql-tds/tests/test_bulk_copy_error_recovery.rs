// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Tests for bulk copy error recovery behavior.
//!
//! These tests verify that when an error occurs during bulk copy streaming,
//! the connection is properly cleaned up via attention packet and remains usable.

#[cfg(test)]
mod common;

mod bulk_copy_error_recovery_tests {
    use crate::common::{begin_connection, build_tcp_datasource, init_tracing};
    use async_trait::async_trait;
    use mssql_tds::connection::bulk_copy::{BulkCopy, BulkLoadRow};
    use mssql_tds::connection::tds_client::ResultSet;
    use mssql_tds::core::TdsResult;
    use mssql_tds::datatypes::column_values::ColumnValues;
    use mssql_tds::error::Error;

    #[ctor::ctor]
    fn init() {
        init_tracing();
    }

    /// A row that will fail on a specific row number to simulate mid-stream errors.
    #[derive(Debug, Clone)]
    struct FailingRow {
        id: i32,
        value: i32,
        fail_on_id: i32, // If id == fail_on_id, this row will return an error
    }

    #[async_trait]
    impl BulkLoadRow for FailingRow {
        async fn write_to_packet(
            &self,
            writer: &mut mssql_tds::message::bulk_load::StreamingBulkLoadWriter<'_>,
            column_index: &mut usize,
        ) -> TdsResult<()> {
            // Simulate an error on a specific row
            if self.id == self.fail_on_id {
                return Err(Error::UsageError(format!(
                    "Simulated error on row {}",
                    self.id
                )));
            }

            writer
                .write_column_value(*column_index, &ColumnValues::Int(self.id))
                .await?;
            *column_index += 1;
            writer
                .write_column_value(*column_index, &ColumnValues::Int(self.value))
                .await?;
            *column_index += 1;
            Ok(())
        }
    }

    #[async_trait]
    impl BulkLoadRow for &FailingRow {
        async fn write_to_packet(
            &self,
            writer: &mut mssql_tds::message::bulk_load::StreamingBulkLoadWriter<'_>,
            column_index: &mut usize,
        ) -> TdsResult<()> {
            // Simulate an error on a specific row
            if self.id == self.fail_on_id {
                return Err(Error::UsageError(format!(
                    "Simulated error on row {}",
                    self.id
                )));
            }

            writer
                .write_column_value(*column_index, &ColumnValues::Int(self.id))
                .await?;
            *column_index += 1;
            writer
                .write_column_value(*column_index, &ColumnValues::Int(self.value))
                .await?;
            *column_index += 1;
            Ok(())
        }
    }

    /// Test that when a bulk copy fails mid-stream, the connection remains usable.
    ///
    /// This test verifies that:
    /// 1. An error during row streaming causes bulk copy to fail with the correct error
    /// 2. The connection is properly cleaned up via attention packet
    /// 3. Subsequent queries on the same connection succeed
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_bulk_copy_error_recovery_connection_remains_usable() {
        let mut client = begin_connection(&build_tcp_datasource()).await;

        // Create temp table
        client
            .execute(
                "CREATE TABLE #BulkCopyErrorRecoveryTest (
                    id INT NOT NULL,
                    value INT NOT NULL
                )"
                .to_string(),
                (),
            )
            .await
            .expect("Failed to create temp table");

        // Create rows where the 5th row will fail (after some successful writes)
        let rows: Vec<FailingRow> = (1..=10)
            .map(|i| FailingRow {
                id: i,
                value: i * 100,
                fail_on_id: 5, // Fail on row 5
            })
            .collect();

        // Execute bulk copy - this should fail on row 5
        let bulk_result = {
            let bulk_copy = BulkCopy::new(&mut client, "#BulkCopyErrorRecoveryTest");
            bulk_copy
                .batch_size(1000)
                .write_to_server_zerocopy(&rows)
                .await
        };

        // Verify the bulk copy failed with our simulated error
        assert!(bulk_result.is_err(), "Bulk copy should have failed");
        let err = bulk_result.unwrap_err();
        let err_msg = err.to_string();
        assert!(
            err_msg.contains("Simulated error on row 5"),
            "Expected 'Simulated error on row 5' but got: {}",
            err_msg
        );

        // CRITICAL: Verify the connection is still usable after the error
        // If attention packet was not sent, this query would fail with
        // "Connection closed by server while reading TDS packet header"
        let select_result = client
            .execute("SELECT 1 AS test_value".to_string(), ())
            .await;

        assert!(
            select_result.is_ok(),
            "Connection should still be usable after bulk copy error. Got error: {:?}",
            select_result.err()
        );

        // Verify we can read the result and consume the entire result set
        if let Some(resultset) = client.get_current_resultset() {
            let row = resultset
                .next_row()
                .await
                .expect("Failed to read row")
                .expect("Expected a row");
            assert_eq!(row[0], ColumnValues::Int(1));

            // Consume remaining rows to close the result set
            while resultset
                .next_row()
                .await
                .expect("Failed reading")
                .is_some()
            {}
        }

        // Also verify we can do another operation (insert)
        client
            .execute(
                "INSERT INTO #BulkCopyErrorRecoveryTest (id, value) VALUES (100, 1000)".to_string(),
                (),
            )
            .await
            .expect("Should be able to insert after failed bulk copy");

        // Verify the insert worked
        client
            .execute(
                "SELECT COUNT(*) FROM #BulkCopyErrorRecoveryTest WHERE id = 100".to_string(),
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
            assert_eq!(row[0], ColumnValues::Int(1), "Insert should have succeeded");
        }
    }

    /// Test that error recovery works when the error occurs on the very first row.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_bulk_copy_error_recovery_first_row_error() {
        let mut client = begin_connection(&build_tcp_datasource()).await;

        // Create temp table
        client
            .execute(
                "CREATE TABLE #BulkCopyFirstRowErrorTest (
                    id INT NOT NULL,
                    value INT NOT NULL
                )"
                .to_string(),
                (),
            )
            .await
            .expect("Failed to create temp table");

        // Create rows where the first row will fail
        let rows: Vec<FailingRow> = (1..=5)
            .map(|i| FailingRow {
                id: i,
                value: i * 100,
                fail_on_id: 1, // Fail on first row
            })
            .collect();

        // Execute bulk copy - this should fail immediately
        let bulk_result = {
            let bulk_copy = BulkCopy::new(&mut client, "#BulkCopyFirstRowErrorTest");
            bulk_copy
                .batch_size(1000)
                .write_to_server_zerocopy(&rows)
                .await
        };

        // Verify the bulk copy failed
        assert!(bulk_result.is_err(), "Bulk copy should have failed");

        // Verify the connection is still usable
        let select_result = client
            .execute("SELECT 'connection ok' AS status".to_string(), ())
            .await;

        assert!(
            select_result.is_ok(),
            "Connection should still be usable after first-row bulk copy error"
        );
    }

    /// Test that a successful bulk copy can follow a failed one on the same connection.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_bulk_copy_success_after_failure() {
        let mut client = begin_connection(&build_tcp_datasource()).await;

        // Create temp table
        client
            .execute(
                "CREATE TABLE #BulkCopySuccessAfterFailTest (
                    id INT NOT NULL,
                    value INT NOT NULL
                )"
                .to_string(),
                (),
            )
            .await
            .expect("Failed to create temp table");

        // First: Do a failing bulk copy
        let failing_rows: Vec<FailingRow> = (1..=5)
            .map(|i| FailingRow {
                id: i,
                value: i * 100,
                fail_on_id: 3, // Fail on row 3
            })
            .collect();

        let bulk_result = {
            let bulk_copy = BulkCopy::new(&mut client, "#BulkCopySuccessAfterFailTest");
            bulk_copy
                .batch_size(1000)
                .write_to_server_zerocopy(&failing_rows)
                .await
        };
        assert!(bulk_result.is_err(), "First bulk copy should have failed");

        // Second: Do a successful bulk copy on the same connection
        // Use rows that won't fail (fail_on_id is set to a non-existent id)
        let success_rows: Vec<FailingRow> = (10..=15)
            .map(|i| FailingRow {
                id: i,
                value: i * 100,
                fail_on_id: 999, // Won't fail
            })
            .collect();

        let bulk_result = {
            let bulk_copy = BulkCopy::new(&mut client, "#BulkCopySuccessAfterFailTest");
            bulk_copy
                .batch_size(1000)
                .write_to_server_zerocopy(&success_rows)
                .await
        };

        assert!(
            bulk_result.is_ok(),
            "Second bulk copy should succeed after first one failed. Error: {:?}",
            bulk_result.err()
        );
        assert_eq!(bulk_result.unwrap().rows_affected, 6);

        // Verify the data was inserted
        client
            .execute(
                "SELECT COUNT(*) FROM #BulkCopySuccessAfterFailTest".to_string(),
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
            assert_eq!(
                row[0],
                ColumnValues::Int(6),
                "Should have 6 rows from successful bulk copy"
            );
        }
    }
}
