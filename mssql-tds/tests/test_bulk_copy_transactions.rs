// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Integration tests for BulkCopy BatchSize and UseInternalTransaction options.
//!
//! These tests verify the transaction behavior during bulk copy operations,
//! matching the behavior documented in the .NET SqlBulkCopy implementation.
//!
//! Test Categories:
//! 1. UseInternalTransaction = true with no existing transaction (should work)
//! 2. UseInternalTransaction = true with existing transaction (should fail)
//! 3. UseInternalTransaction = false with existing transaction (should participate)
//! 4. UseInternalTransaction = false with no transaction (autocommit behavior)
//! 5. BatchSize variations with transaction behavior
//!
//! Reference: .NET SqlBulkCopy tests in Transaction.cs, Transaction1.cs, Transaction4.cs

#[cfg(test)]
mod common;

#[cfg(test)]
mod bulk_copy_transaction_tests {
    use crate::common::{begin_connection, build_tcp_datasource, init_tracing};
    use async_trait::async_trait;
    use mssql_tds::connection::bulk_copy::{BulkCopy, BulkLoadRow};
    use mssql_tds::connection::tds_client::ResultSet;
    use mssql_tds::core::TdsResult;
    use mssql_tds::datatypes::column_values::ColumnValues;
    use mssql_tds::message::transaction_management::TransactionIsolationLevel;

    #[ctor::ctor]
    fn init() {
        init_tracing();
    }

    // =========================================================================
    // Test Data Structures
    // =========================================================================

    /// Test row structure for bulk copy operations
    #[derive(Debug, Clone)]
    struct TestRow {
        id: i32,
        value: i32,
    }

    /// Implementation for owned TestRow - delegates to reference implementation
    #[async_trait]
    impl BulkLoadRow for TestRow {
        async fn write_to_packet(
            &self,
            writer: &mut mssql_tds::message::bulk_load::StreamingBulkLoadWriter<'_>,
            column_index: &mut usize,
        ) -> TdsResult<()> {
            (&self).write_to_packet(writer, column_index).await
        }
    }

    /// Primary implementation for TestRow reference
    #[async_trait]
    impl BulkLoadRow for &TestRow {
        async fn write_to_packet(
            &self,
            writer: &mut mssql_tds::message::bulk_load::StreamingBulkLoadWriter<'_>,
            column_index: &mut usize,
        ) -> TdsResult<()> {
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

    // =========================================================================
    // Test: Default behavior (UseInternalTransaction = false)
    // =========================================================================

    /// Test that UseInternalTransaction defaults to false (matching .NET)
    /// and bulk copy succeeds without explicit transaction.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_bulk_copy_default_no_internal_transaction() {
        let mut client = begin_connection(&build_tcp_datasource()).await;

        // Create temp table
        client
            .execute(
                "CREATE TABLE #BulkTxnDefault (id INT NOT NULL, value INT NOT NULL)".to_string(),
                (),
            )
            .await
            .expect("Failed to create test table");
        client.close_query().await.expect("Failed to close query");

        let test_data = vec![
            TestRow { id: 1, value: 100 },
            TestRow { id: 2, value: 200 },
            TestRow { id: 3, value: 300 },
        ];

        // Default BulkCopy (use_internal_transaction = false by default)
        let result = {
            let mut bulk_copy = BulkCopy::new(&mut client, "#BulkTxnDefault");
            bulk_copy
                .write_to_server_zerocopy(&test_data)
                .await
                .expect("Bulk copy should succeed with default settings")
        };

        assert_eq!(result.rows_affected, 3, "Expected 3 rows to be inserted");

        // Verify data was committed (autocommit behavior)
        client
            .execute("SELECT COUNT(*) FROM #BulkTxnDefault".to_string(), ())
            .await
            .expect("Failed to count rows");

        if let Some(resultset) = client.get_current_resultset()
            && let Some(row) = resultset.next_row().await.expect("Failed to read row")
        {
            assert_eq!(row[0], ColumnValues::Int(3), "Expected 3 rows committed");
        }
        client.close_query().await.expect("Failed to close query");
    }

    // =========================================================================
    // Test: UseInternalTransaction = true with no existing transaction
    // =========================================================================

    /// Test that UseInternalTransaction = true works when no transaction exists.
    /// Each batch should be wrapped in its own transaction.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_bulk_copy_internal_transaction_no_existing() {
        let mut client = begin_connection(&build_tcp_datasource()).await;

        // Create temp table
        client
            .execute(
                "CREATE TABLE #BulkTxnInternal (id INT NOT NULL, value INT NOT NULL)".to_string(),
                (),
            )
            .await
            .expect("Failed to create test table");
        client.close_query().await.expect("Failed to close query");

        let test_data = vec![
            TestRow { id: 1, value: 100 },
            TestRow { id: 2, value: 200 },
            TestRow { id: 3, value: 300 },
        ];

        // BulkCopy with internal transaction enabled
        let result = {
            let bulk_copy = BulkCopy::new(&mut client, "#BulkTxnInternal");
            bulk_copy
                .use_internal_transaction(true)
                .write_to_server_zerocopy(&test_data)
                .await
                .expect("Bulk copy should succeed with internal transaction")
        };

        assert_eq!(result.rows_affected, 3, "Expected 3 rows to be inserted");

        // Verify data was committed
        client
            .execute("SELECT COUNT(*) FROM #BulkTxnInternal".to_string(), ())
            .await
            .expect("Failed to count rows");

        if let Some(resultset) = client.get_current_resultset()
            && let Some(row) = resultset.next_row().await.expect("Failed to read row")
        {
            assert_eq!(row[0], ColumnValues::Int(3), "Expected 3 rows committed");
        }
        client.close_query().await.expect("Failed to close query");
    }

    // =========================================================================
    // Test: UseInternalTransaction = true with existing API transaction (FAIL)
    // Mirrors .NET Transaction.cs - InvalidOperationException expected
    // =========================================================================

    /// Test that UseInternalTransaction = true fails when connection has active transaction.
    /// This mirrors .NET SqlBulkCopy Transaction.cs behavior.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_bulk_copy_internal_transaction_with_existing_api_transaction_fails() {
        let mut client = begin_connection(&build_tcp_datasource()).await;

        // Create temp table
        client
            .execute(
                "CREATE TABLE #BulkTxnConflict (id INT NOT NULL, value INT NOT NULL)".to_string(),
                (),
            )
            .await
            .expect("Failed to create test table");
        client.close_query().await.expect("Failed to close query");

        // Begin a transaction using the API
        client
            .begin_transaction(TransactionIsolationLevel::ReadCommitted, None)
            .await
            .expect("Failed to begin transaction");

        let test_data = vec![TestRow { id: 1, value: 100 }, TestRow { id: 2, value: 200 }];

        // Attempt bulk copy with internal transaction - should fail
        let result = {
            let bulk_copy = BulkCopy::new(&mut client, "#BulkTxnConflict");
            bulk_copy
                .use_internal_transaction(true)
                .write_to_server_zerocopy(&test_data)
                .await
        };

        // Should fail with error about conflicting transaction
        assert!(
            result.is_err(),
            "Expected error when using internal transaction with existing API transaction"
        );

        let error = result.unwrap_err();
        let error_msg = format!("{:?}", error);
        assert!(
            error_msg.contains("UseInternalTransaction")
                || error_msg.contains("active transaction")
                || error_msg.contains("internal transaction"),
            "Expected error about conflicting transaction, got: {}",
            error_msg
        );

        // Rollback the transaction to clean up
        client
            .rollback_transaction(None, None)
            .await
            .expect("Failed to rollback transaction");
    }

    // =========================================================================
    // Test: UseInternalTransaction = true with existing SQL transaction (FAIL)
    // Mirrors .NET Transaction1.cs - InvalidOperationException expected
    // =========================================================================

    /// Test that UseInternalTransaction = true fails when SQL BEGIN TRANSACTION was executed.
    /// This mirrors .NET SqlBulkCopy Transaction1.cs behavior.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_bulk_copy_internal_transaction_with_existing_sql_transaction_fails() {
        let mut client = begin_connection(&build_tcp_datasource()).await;

        // Create temp table
        client
            .execute(
                "CREATE TABLE #BulkTxnSqlConflict (id INT NOT NULL, value INT NOT NULL)"
                    .to_string(),
                (),
            )
            .await
            .expect("Failed to create test table");
        client.close_query().await.expect("Failed to close query");

        // Begin a transaction using SQL command (not API)
        client
            .execute("BEGIN TRANSACTION".to_string(), ())
            .await
            .expect("Failed to begin SQL transaction");
        client.close_query().await.expect("Failed to close query");

        let test_data = vec![TestRow { id: 1, value: 100 }, TestRow { id: 2, value: 200 }];

        // Attempt bulk copy with internal transaction - should fail
        let result = {
            let bulk_copy = BulkCopy::new(&mut client, "#BulkTxnSqlConflict");
            bulk_copy
                .use_internal_transaction(true)
                .write_to_server_zerocopy(&test_data)
                .await
        };

        // Should fail with error about conflicting transaction
        assert!(
            result.is_err(),
            "Expected error when using internal transaction with existing SQL transaction"
        );

        // Rollback the SQL transaction to clean up
        let _ = client.execute("ROLLBACK TRANSACTION".to_string(), ()).await;
        let _ = client.close_query().await;
    }

    // =========================================================================
    // Test: UseInternalTransaction = false with existing transaction (participates)
    // =========================================================================

    /// Test that UseInternalTransaction = false allows bulk copy to participate
    /// in an existing transaction. The data should be visible within the transaction
    /// but can be rolled back.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_bulk_copy_participates_in_external_transaction() {
        let mut client = begin_connection(&build_tcp_datasource()).await;

        // Create temp table
        client
            .execute(
                "CREATE TABLE #BulkTxnExternal (id INT NOT NULL, value INT NOT NULL)".to_string(),
                (),
            )
            .await
            .expect("Failed to create test table");
        client.close_query().await.expect("Failed to close query");

        // Begin a transaction
        client
            .begin_transaction(TransactionIsolationLevel::ReadCommitted, None)
            .await
            .expect("Failed to begin transaction");

        let test_data = vec![
            TestRow { id: 1, value: 100 },
            TestRow { id: 2, value: 200 },
            TestRow { id: 3, value: 300 },
        ];

        // Bulk copy with use_internal_transaction = false (participate in external)
        let result = {
            let bulk_copy = BulkCopy::new(&mut client, "#BulkTxnExternal");
            bulk_copy
                .use_internal_transaction(false)
                .write_to_server_zerocopy(&test_data)
                .await
                .expect("Bulk copy should succeed participating in external transaction")
        };

        assert_eq!(result.rows_affected, 3, "Expected 3 rows to be inserted");

        // Verify data is visible within the transaction
        client
            .execute("SELECT COUNT(*) FROM #BulkTxnExternal".to_string(), ())
            .await
            .expect("Failed to count rows");

        if let Some(resultset) = client.get_current_resultset()
            && let Some(row) = resultset.next_row().await.expect("Failed to read row")
        {
            assert_eq!(
                row[0],
                ColumnValues::Int(3),
                "Expected 3 rows visible in transaction"
            );
        }
        client.close_query().await.expect("Failed to close query");

        // Rollback the transaction
        client
            .rollback_transaction(None, None)
            .await
            .expect("Failed to rollback transaction");

        // Verify data was rolled back
        client
            .execute("SELECT COUNT(*) FROM #BulkTxnExternal".to_string(), ())
            .await
            .expect("Failed to count rows after rollback");

        if let Some(resultset) = client.get_current_resultset()
            && let Some(row) = resultset.next_row().await.expect("Failed to read row")
        {
            assert_eq!(
                row[0],
                ColumnValues::Int(0),
                "Expected 0 rows after rollback"
            );
        }
        client.close_query().await.expect("Failed to close query");
    }

    // =========================================================================
    // Test: BatchSize with internal transaction (multiple batch commits)
    // =========================================================================

    /// Test that with BatchSize and UseInternalTransaction = true,
    /// each batch is committed separately.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_bulk_copy_batch_size_with_internal_transaction() {
        let mut client = begin_connection(&build_tcp_datasource()).await;

        // Create temp table
        client
            .execute(
                "CREATE TABLE #BulkTxnBatch (id INT NOT NULL, value INT NOT NULL)".to_string(),
                (),
            )
            .await
            .expect("Failed to create test table");
        client.close_query().await.expect("Failed to close query");

        // Create 10 rows to be split into batches of 3
        let test_data: Vec<TestRow> = (1..=10)
            .map(|i| TestRow {
                id: i,
                value: i * 100,
            })
            .collect();

        // Bulk copy with batch_size=3 and internal transaction
        // This should create 4 batches: 3, 3, 3, 1
        let result = {
            let bulk_copy = BulkCopy::new(&mut client, "#BulkTxnBatch");
            bulk_copy
                .batch_size(3)
                .use_internal_transaction(true)
                .write_to_server_zerocopy(&test_data)
                .await
                .expect("Bulk copy with batch_size and internal transaction should succeed")
        };

        assert_eq!(result.rows_affected, 10, "Expected 10 rows to be inserted");

        // Verify all data was committed
        client
            .execute("SELECT COUNT(*) FROM #BulkTxnBatch".to_string(), ())
            .await
            .expect("Failed to count rows");

        if let Some(resultset) = client.get_current_resultset()
            && let Some(row) = resultset.next_row().await.expect("Failed to read row")
        {
            assert_eq!(row[0], ColumnValues::Int(10), "Expected 10 rows committed");
        }
        client.close_query().await.expect("Failed to close query");
    }

    // =========================================================================
    // Test: BatchSize = 0 means single batch (all rows)
    // =========================================================================

    /// Test that BatchSize = 0 (default) processes all rows in a single batch.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_bulk_copy_batch_size_zero_means_single_batch() {
        let mut client = begin_connection(&build_tcp_datasource()).await;

        // Create temp table
        client
            .execute(
                "CREATE TABLE #BulkTxnSingleBatch (id INT NOT NULL, value INT NOT NULL)"
                    .to_string(),
                (),
            )
            .await
            .expect("Failed to create test table");
        client.close_query().await.expect("Failed to close query");

        // Create 100 rows
        let test_data: Vec<TestRow> = (1..=100)
            .map(|i| TestRow {
                id: i,
                value: i * 10,
            })
            .collect();

        // Bulk copy with batch_size=0 (default - single batch)
        let result = {
            let bulk_copy = BulkCopy::new(&mut client, "#BulkTxnSingleBatch");
            bulk_copy
                .batch_size(0) // Explicit zero = single batch
                .write_to_server_zerocopy(&test_data)
                .await
                .expect("Bulk copy with batch_size=0 should succeed")
        };

        assert_eq!(
            result.rows_affected, 100,
            "Expected 100 rows to be inserted"
        );

        // Verify all data was inserted
        client
            .execute("SELECT COUNT(*) FROM #BulkTxnSingleBatch".to_string(), ())
            .await
            .expect("Failed to count rows");

        if let Some(resultset) = client.get_current_resultset()
            && let Some(row) = resultset.next_row().await.expect("Failed to read row")
        {
            assert_eq!(row[0], ColumnValues::Int(100), "Expected 100 rows");
        }
        client.close_query().await.expect("Failed to close query");
    }

    // =========================================================================
    // Test: External transaction commit persists bulk copy data
    // =========================================================================

    /// Test that bulk copy data is persisted when external transaction commits.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_bulk_copy_external_transaction_commit() {
        let mut client = begin_connection(&build_tcp_datasource()).await;

        // Create temp table
        client
            .execute(
                "CREATE TABLE #BulkTxnCommit (id INT NOT NULL, value INT NOT NULL)".to_string(),
                (),
            )
            .await
            .expect("Failed to create test table");
        client.close_query().await.expect("Failed to close query");

        // Begin a transaction
        client
            .begin_transaction(TransactionIsolationLevel::ReadCommitted, None)
            .await
            .expect("Failed to begin transaction");

        let test_data = vec![TestRow { id: 1, value: 100 }, TestRow { id: 2, value: 200 }];

        // Bulk copy participating in external transaction
        let result = {
            let bulk_copy = BulkCopy::new(&mut client, "#BulkTxnCommit");
            bulk_copy
                .use_internal_transaction(false)
                .write_to_server_zerocopy(&test_data)
                .await
                .expect("Bulk copy should succeed")
        };

        assert_eq!(result.rows_affected, 2, "Expected 2 rows");

        // Commit the transaction
        client
            .commit_transaction(None, None)
            .await
            .expect("Failed to commit transaction");

        // Verify data persists after commit
        client
            .execute("SELECT COUNT(*) FROM #BulkTxnCommit".to_string(), ())
            .await
            .expect("Failed to count rows");

        if let Some(resultset) = client.get_current_resultset()
            && let Some(row) = resultset.next_row().await.expect("Failed to read row")
        {
            assert_eq!(row[0], ColumnValues::Int(2), "Expected 2 rows after commit");
        }
        client.close_query().await.expect("Failed to close query");
    }

    // =========================================================================
    // Test: Verify has_active_transaction detection works correctly
    // =========================================================================

    /// Test that has_active_transaction correctly detects transaction state.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_has_active_transaction_detection() {
        let mut client = begin_connection(&build_tcp_datasource()).await;

        // Initially no transaction
        assert!(
            !client.has_active_transaction(),
            "Should have no active transaction initially"
        );

        // Begin transaction via API
        client
            .begin_transaction(TransactionIsolationLevel::ReadCommitted, None)
            .await
            .expect("Failed to begin transaction");

        assert!(
            client.has_active_transaction(),
            "Should have active transaction after begin"
        );

        // Commit
        client
            .commit_transaction(None, None)
            .await
            .expect("Failed to commit");

        assert!(
            !client.has_active_transaction(),
            "Should have no active transaction after commit"
        );

        // Begin and rollback
        client
            .begin_transaction(TransactionIsolationLevel::ReadCommitted, None)
            .await
            .expect("Failed to begin transaction");

        assert!(
            client.has_active_transaction(),
            "Should have active transaction"
        );

        client
            .rollback_transaction(None, None)
            .await
            .expect("Failed to rollback");

        assert!(
            !client.has_active_transaction(),
            "Should have no active transaction after rollback"
        );
    }

    // =========================================================================
    // Test: Multiple batch sizes
    // =========================================================================

    /// Test various batch sizes to ensure batching works correctly.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_bulk_copy_various_batch_sizes() {
        let mut client = begin_connection(&build_tcp_datasource()).await;

        // Test batch sizes: 1, 5, 10, 50
        for batch_size in [1, 5, 10, 50] {
            let table_name = format!("#BulkTxnBatch{}", batch_size);

            // Create temp table
            client
                .execute(
                    format!(
                        "CREATE TABLE {} (id INT NOT NULL, value INT NOT NULL)",
                        table_name
                    ),
                    (),
                )
                .await
                .expect("Failed to create test table");
            client.close_query().await.expect("Failed to close query");

            // Create 25 rows
            let test_data: Vec<TestRow> = (1..=25)
                .map(|i| TestRow {
                    id: i,
                    value: i * 10,
                })
                .collect();

            let result = {
                let bulk_copy = BulkCopy::new(&mut client, &table_name);
                bulk_copy
                    .batch_size(batch_size)
                    .write_to_server_zerocopy(&test_data)
                    .await
                    .unwrap_or_else(|_| {
                        panic!("Bulk copy with batch_size={} should succeed", batch_size)
                    })
            };

            assert_eq!(
                result.rows_affected, 25,
                "Expected 25 rows for batch_size={}",
                batch_size
            );

            // Verify count
            client
                .execute(format!("SELECT COUNT(*) FROM {}", table_name), ())
                .await
                .expect("Failed to count rows");

            if let Some(resultset) = client.get_current_resultset()
                && let Some(row) = resultset.next_row().await.expect("Failed to read row")
            {
                assert_eq!(
                    row[0],
                    ColumnValues::Int(25),
                    "Expected 25 rows for batch_size={}",
                    batch_size
                );
            }
            client.close_query().await.expect("Failed to close query");
        }
    }

    // =========================================================================
    // Test: BatchSize with external transaction - all-or-nothing rollback
    // =========================================================================

    /// Test that with BatchSize > 0 and external transaction,
    /// a rollback removes ALL batches (all-or-nothing behavior).
    /// This is different from UseInternalTransaction where each batch commits separately.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_batch_size_with_external_transaction_rollback_all() {
        let mut client = begin_connection(&build_tcp_datasource()).await;

        // Create temp table
        client
            .execute(
                "CREATE TABLE #BulkTxnAllOrNothing (id INT NOT NULL, value INT NOT NULL)"
                    .to_string(),
                (),
            )
            .await
            .expect("Failed to create test table");
        client.close_query().await.expect("Failed to close query");

        // Begin external transaction
        client
            .begin_transaction(TransactionIsolationLevel::ReadCommitted, None)
            .await
            .expect("Failed to begin transaction");

        // Create 12 rows, batch_size=4 means 3 batches
        let test_data: Vec<TestRow> = (1..=12)
            .map(|i| TestRow {
                id: i,
                value: i * 100,
            })
            .collect();

        // Bulk copy with batch_size=4, participating in external transaction
        let result = {
            let bulk_copy = BulkCopy::new(&mut client, "#BulkTxnAllOrNothing");
            bulk_copy
                .batch_size(4)
                .use_internal_transaction(false) // Participate in external
                .write_to_server_zerocopy(&test_data)
                .await
                .expect("Bulk copy should succeed")
        };

        assert_eq!(result.rows_affected, 12, "Expected 12 rows inserted");

        // Verify data exists within transaction
        client
            .execute("SELECT COUNT(*) FROM #BulkTxnAllOrNothing".to_string(), ())
            .await
            .expect("Failed to count rows");

        if let Some(resultset) = client.get_current_resultset()
            && let Some(row) = resultset.next_row().await.expect("Failed to read row")
        {
            assert_eq!(
                row[0],
                ColumnValues::Int(12),
                "Expected 12 rows within transaction"
            );
        }
        client.close_query().await.expect("Failed to close query");

        // Rollback the external transaction - ALL batches should be rolled back
        client
            .rollback_transaction(None, None)
            .await
            .expect("Failed to rollback");

        // Verify ALL data is gone (all 3 batches rolled back together)
        client
            .execute("SELECT COUNT(*) FROM #BulkTxnAllOrNothing".to_string(), ())
            .await
            .expect("Failed to count rows after rollback");

        if let Some(resultset) = client.get_current_resultset()
            && let Some(row) = resultset.next_row().await.expect("Failed to read row")
        {
            assert_eq!(
                row[0],
                ColumnValues::Int(0),
                "Expected 0 rows after rollback - all batches should be rolled back together"
            );
        }
        client.close_query().await.expect("Failed to close query");
    }

    // =========================================================================
    // Test: Internal transaction with batch_size=1 (extreme case)
    // =========================================================================

    /// Test that batch_size=1 with internal transaction works correctly,
    /// creating a transaction for each individual row.
    /// Note: This is inefficient but should work correctly.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_batch_size_one_with_internal_transaction() {
        let mut client = begin_connection(&build_tcp_datasource()).await;

        // Create temp table
        client
            .execute(
                "CREATE TABLE #BulkTxnSingleRow (id INT NOT NULL, value INT NOT NULL)".to_string(),
                (),
            )
            .await
            .expect("Failed to create test table");
        client.close_query().await.expect("Failed to close query");

        // Create 5 rows - each will be in its own transaction
        let test_data: Vec<TestRow> = (1..=5)
            .map(|i| TestRow {
                id: i,
                value: i * 100,
            })
            .collect();

        // Bulk copy with batch_size=1 and internal transaction
        // This creates 5 separate transactions!
        let result = {
            let bulk_copy = BulkCopy::new(&mut client, "#BulkTxnSingleRow");
            bulk_copy
                .batch_size(1)
                .use_internal_transaction(true)
                .write_to_server_zerocopy(&test_data)
                .await
                .expect("Bulk copy with batch_size=1 should succeed")
        };

        assert_eq!(result.rows_affected, 5, "Expected 5 rows inserted");

        // Verify all data was committed
        client
            .execute("SELECT COUNT(*) FROM #BulkTxnSingleRow".to_string(), ())
            .await
            .expect("Failed to count rows");

        if let Some(resultset) = client.get_current_resultset()
            && let Some(row) = resultset.next_row().await.expect("Failed to read row")
        {
            assert_eq!(row[0], ColumnValues::Int(5), "Expected 5 rows committed");
        }
        client.close_query().await.expect("Failed to close query");
    }

    // =========================================================================
    // Test: Verify transaction state is clean after bulk copy
    // =========================================================================

    /// Test that after bulk copy with internal transaction, no transaction is left active.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_internal_transaction_leaves_no_active_transaction() {
        let mut client = begin_connection(&build_tcp_datasource()).await;

        // Create temp table
        client
            .execute(
                "CREATE TABLE #BulkTxnCleanup (id INT NOT NULL, value INT NOT NULL)".to_string(),
                (),
            )
            .await
            .expect("Failed to create test table");
        client.close_query().await.expect("Failed to close query");

        // Verify no active transaction before
        assert!(
            !client.has_active_transaction(),
            "Should have no active transaction before bulk copy"
        );

        let test_data = vec![TestRow { id: 1, value: 100 }, TestRow { id: 2, value: 200 }];

        // Bulk copy with internal transaction
        {
            let bulk_copy = BulkCopy::new(&mut client, "#BulkTxnCleanup");
            bulk_copy
                .use_internal_transaction(true)
                .write_to_server_zerocopy(&test_data)
                .await
                .expect("Bulk copy should succeed");
        }

        // Verify no active transaction after
        assert!(
            !client.has_active_transaction(),
            "Should have no active transaction after bulk copy with internal transaction"
        );

        // Connection should be usable for normal operations
        client
            .execute("SELECT 1".to_string(), ())
            .await
            .expect("Connection should be usable after bulk copy");
        client.close_query().await.expect("Failed to close query");
    }

    // =========================================================================
    // Test: Multiple sequential bulk copies with internal transaction
    // =========================================================================

    /// Test that multiple sequential bulk copies with internal transaction work correctly.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_multiple_sequential_bulk_copies_with_internal_transaction() {
        let mut client = begin_connection(&build_tcp_datasource()).await;

        // Create temp table
        client
            .execute(
                "CREATE TABLE #BulkTxnSequential (id INT NOT NULL, value INT NOT NULL)".to_string(),
                (),
            )
            .await
            .expect("Failed to create test table");
        client.close_query().await.expect("Failed to close query");

        // First bulk copy
        let data1 = vec![TestRow { id: 1, value: 100 }, TestRow { id: 2, value: 200 }];

        {
            let bulk_copy = BulkCopy::new(&mut client, "#BulkTxnSequential");
            bulk_copy
                .use_internal_transaction(true)
                .write_to_server_zerocopy(&data1)
                .await
                .expect("First bulk copy should succeed");
        }

        // Second bulk copy
        let data2 = vec![TestRow { id: 3, value: 300 }, TestRow { id: 4, value: 400 }];

        {
            let bulk_copy = BulkCopy::new(&mut client, "#BulkTxnSequential");
            bulk_copy
                .use_internal_transaction(true)
                .write_to_server_zerocopy(&data2)
                .await
                .expect("Second bulk copy should succeed");
        }

        // Third bulk copy
        let data3 = vec![TestRow { id: 5, value: 500 }];

        {
            let bulk_copy = BulkCopy::new(&mut client, "#BulkTxnSequential");
            bulk_copy
                .use_internal_transaction(true)
                .write_to_server_zerocopy(&data3)
                .await
                .expect("Third bulk copy should succeed");
        }

        // Verify all data was committed
        client
            .execute("SELECT COUNT(*) FROM #BulkTxnSequential".to_string(), ())
            .await
            .expect("Failed to count rows");

        if let Some(resultset) = client.get_current_resultset()
            && let Some(row) = resultset.next_row().await.expect("Failed to read row")
        {
            assert_eq!(
                row[0],
                ColumnValues::Int(5),
                "Expected 5 total rows from 3 sequential bulk copies"
            );
        }
        client.close_query().await.expect("Failed to close query");
    }

    // =========================================================================
    // Test: Autocommit behavior - BatchSize > 0, UseInternalTransaction = false
    // WITHOUT external transaction - each batch auto-commits after DONE token
    // =========================================================================

    /// Test autocommit behavior: BatchSize > 0 with UseInternalTransaction = false
    /// and NO external transaction. Each batch is auto-committed by SQL Server
    /// after the DONE token is sent.
    ///
    /// This tests the scenario from the wiki specification:
    /// ```text
    /// WITHOUT UseInternalTransaction (autocommit):
    /// │  Batch 1    │     │  Batch 2     │     │  Batch 3    │
    /// │ rows 1-1000 │     │ 1001-2000    │     │ 2001-2500   │
    /// │ DONE → AC   │ ──► │ DONE → AC    │ ──► │ ERROR!      │
    /// │  ✓ saved    │     │  ✓ saved     │     │  ✗ lost     │
    /// Result: 2000 rows committed (autocommit), 500 rows lost
    /// ```
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_autocommit_batch_size_no_internal_transaction_no_external_transaction() {
        let mut client = begin_connection(&build_tcp_datasource()).await;

        // Create temp table
        client
            .execute(
                "CREATE TABLE #BulkAutocommit (id INT NOT NULL, value INT NOT NULL)".to_string(),
                (),
            )
            .await
            .expect("Failed to create test table");
        client.close_query().await.expect("Failed to close query");

        // Verify no active transaction before
        assert!(
            !client.has_active_transaction(),
            "Should have no active transaction initially"
        );

        // Create 12 rows, batch_size=4 means 3 batches
        // Each batch will be auto-committed after its DONE token
        let test_data: Vec<TestRow> = (1..=12)
            .map(|i| TestRow {
                id: i,
                value: i * 100,
            })
            .collect();

        // Bulk copy with batch_size=4, NO internal transaction, NO external transaction
        // This means SQL Server autocommit mode applies - each batch commits on DONE
        let result = {
            let bulk_copy = BulkCopy::new(&mut client, "#BulkAutocommit");
            bulk_copy
                .batch_size(4)
                .use_internal_transaction(false) // No internal transaction
                // No external transaction either - pure autocommit mode
                .write_to_server_zerocopy(&test_data)
                .await
                .expect("Bulk copy should succeed in autocommit mode")
        };

        assert_eq!(result.rows_affected, 12, "Expected 12 rows inserted");

        // Verify no transaction is active (autocommit, not in a transaction)
        assert!(
            !client.has_active_transaction(),
            "Should have no active transaction after autocommit bulk copy"
        );

        // Verify all data is committed (cannot be rolled back - autocommit)
        client
            .execute("SELECT COUNT(*) FROM #BulkAutocommit".to_string(), ())
            .await
            .expect("Failed to count rows");

        if let Some(resultset) = client.get_current_resultset()
            && let Some(row) = resultset.next_row().await.expect("Failed to read row")
        {
            assert_eq!(
                row[0],
                ColumnValues::Int(12),
                "All 12 rows should be committed via autocommit"
            );
        }
        client.close_query().await.expect("Failed to close query");

        // Key verification: Start a new transaction and try to rollback
        // This should NOT affect the autocommitted rows
        client
            .begin_transaction(TransactionIsolationLevel::ReadCommitted, None)
            .await
            .expect("Failed to begin transaction");

        // Insert an additional row in this transaction
        client
            .execute(
                "INSERT INTO #BulkAutocommit (id, value) VALUES (99, 9900)".to_string(),
                (),
            )
            .await
            .expect("Failed to insert");
        client.close_query().await.expect("Failed to close query");

        // Now rollback this transaction
        client
            .rollback_transaction(None, None)
            .await
            .expect("Failed to rollback");

        // Verify: The original 12 autocommitted rows should still be there
        // Only the row we inserted in the transaction (99) should be rolled back
        client
            .execute("SELECT COUNT(*) FROM #BulkAutocommit".to_string(), ())
            .await
            .expect("Failed to count rows after rollback");

        if let Some(resultset) = client.get_current_resultset()
            && let Some(row) = resultset.next_row().await.expect("Failed to read row")
        {
            assert_eq!(
                row[0],
                ColumnValues::Int(12),
                "Autocommitted rows should persist - cannot be rolled back"
            );
        }
        client.close_query().await.expect("Failed to close query");
    }

    // =========================================================================
    // Test: Verify DONE token is sent per batch by checking row counts
    // =========================================================================

    /// Test that each batch results in a separate operation by verifying
    /// intermediate state through batch counting.
    ///
    /// This indirectly verifies that DONE tokens are being sent per batch,
    /// as SQL Server only commits/acknowledges rows after receiving DONE.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_batch_done_token_per_batch() {
        let mut client = begin_connection(&build_tcp_datasource()).await;

        // Create a table (not temp) to allow multiple connections to see the same data
        // Note: Using temp table here but verifying batch-by-batch behavior through row counts
        client
            .execute(
                "CREATE TABLE #BulkDoneToken (id INT NOT NULL, value INT NOT NULL)".to_string(),
                (),
            )
            .await
            .expect("Failed to create test table");
        client.close_query().await.expect("Failed to close query");

        // Create 15 rows with batch_size=5 (exactly 3 batches)
        let test_data: Vec<TestRow> = (1..=15)
            .map(|i| TestRow {
                id: i,
                value: i * 10,
            })
            .collect();

        // Bulk copy with batch_size=5
        let result = {
            let bulk_copy = BulkCopy::new(&mut client, "#BulkDoneToken");
            bulk_copy
                .batch_size(5)
                .write_to_server_zerocopy(&test_data)
                .await
                .expect("Bulk copy should succeed")
        };

        // The result should reflect total rows from all batches
        // Each batch sends DONE token, and rows_affected accumulates all batch counts
        assert_eq!(
            result.rows_affected, 15,
            "Expected 15 rows total from 3 batches of 5"
        );

        // Verify all rows are in the table
        client
            .execute("SELECT COUNT(*) FROM #BulkDoneToken".to_string(), ())
            .await
            .expect("Failed to count rows");

        if let Some(resultset) = client.get_current_resultset()
            && let Some(row) = resultset.next_row().await.expect("Failed to read row")
        {
            assert_eq!(row[0], ColumnValues::Int(15), "Expected 15 rows in table");
        }
        client.close_query().await.expect("Failed to close query");

        // Verify each row has correct data (this ensures DONE was processed per batch)
        client
            .execute(
                "SELECT id, value FROM #BulkDoneToken ORDER BY id".to_string(),
                (),
            )
            .await
            .expect("Failed to select rows");

        let mut row_count = 0;
        if let Some(resultset) = client.get_current_resultset() {
            while let Some(row) = resultset.next_row().await.expect("Failed to read row") {
                row_count += 1;
                let expected_id = row_count;
                let expected_value = row_count * 10;

                assert_eq!(
                    row[0],
                    ColumnValues::Int(expected_id),
                    "Expected id={} for row {}",
                    expected_id,
                    row_count
                );
                assert_eq!(
                    row[1],
                    ColumnValues::Int(expected_value),
                    "Expected value={} for row {}",
                    expected_value,
                    row_count
                );
            }
        }
        client.close_query().await.expect("Failed to close query");

        assert_eq!(row_count, 15, "Should have read all 15 rows");
    }

    // =========================================================================
    // Test: Verify multiple batches are individually processed
    // =========================================================================

    /// Test that verifies batch processing by using a larger dataset
    /// and confirming all batches are processed correctly.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_multiple_batches_processed_correctly() {
        let mut client = begin_connection(&build_tcp_datasource()).await;

        // Create temp table
        client
            .execute(
                "CREATE TABLE #BulkMultiBatch (id INT NOT NULL, value INT NOT NULL)".to_string(),
                (),
            )
            .await
            .expect("Failed to create test table");
        client.close_query().await.expect("Failed to close query");

        // Create 100 rows with batch_size=7 (14 full batches + 1 partial batch of 2)
        // Total: 15 batches
        let test_data: Vec<TestRow> = (1..=100)
            .map(|i| TestRow {
                id: i,
                value: i * 5,
            })
            .collect();

        // Bulk copy with batch_size=7
        let result = {
            let bulk_copy = BulkCopy::new(&mut client, "#BulkMultiBatch");
            bulk_copy
                .batch_size(7)
                .use_internal_transaction(true) // Use internal transaction for clean commits
                .write_to_server_zerocopy(&test_data)
                .await
                .expect("Bulk copy should succeed")
        };

        assert_eq!(
            result.rows_affected, 100,
            "Expected 100 rows from 15 batches"
        );

        // Verify row count
        client
            .execute("SELECT COUNT(*) FROM #BulkMultiBatch".to_string(), ())
            .await
            .expect("Failed to count rows");

        if let Some(resultset) = client.get_current_resultset()
            && let Some(row) = resultset.next_row().await.expect("Failed to read row")
        {
            assert_eq!(row[0], ColumnValues::Int(100), "Expected 100 rows");
        }
        client.close_query().await.expect("Failed to close query");

        // Verify min and max IDs to ensure all batches were processed
        client
            .execute(
                "SELECT MIN(id), MAX(id) FROM #BulkMultiBatch".to_string(),
                (),
            )
            .await
            .expect("Failed to get min/max");

        if let Some(resultset) = client.get_current_resultset()
            && let Some(row) = resultset.next_row().await.expect("Failed to read row")
        {
            assert_eq!(row[0], ColumnValues::Int(1), "Min ID should be 1");
            assert_eq!(row[1], ColumnValues::Int(100), "Max ID should be 100");
        }
        client.close_query().await.expect("Failed to close query");
    }
}
