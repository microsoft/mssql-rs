// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#[cfg(test)]
mod common;

mod bulk_copy_integration_tests {
    use crate::common::{begin_connection, build_tcp_datasource, init_tracing};
    use async_trait::async_trait;
    use mssql_tds::connection::bulk_copy::{BulkCopy, BulkLoadRow};
    use mssql_tds::connection::tds_client::ResultSet;
    use mssql_tds::core::TdsResult;
    use mssql_tds::datatypes::bulk_copy_metadata::SqlDbType;
    use mssql_tds::datatypes::column_values::ColumnValues;

    #[ctor::ctor]
    fn init() {
        init_tracing();
    }

    // Define a simple test data structure
    #[derive(Debug, Clone)]
    struct TestUser {
        id: i32,
        value1: i32,
        value2: i32,
        value3: i32,
    }

    #[async_trait]
    impl BulkLoadRow for TestUser {
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
                .write_column_value(*column_index, &ColumnValues::Int(self.value1))
                .await?;
            *column_index += 1;
            writer
                .write_column_value(*column_index, &ColumnValues::Int(self.value2))
                .await?;
            *column_index += 1;
            writer
                .write_column_value(*column_index, &ColumnValues::Int(self.value3))
                .await?;
            *column_index += 1;
            Ok(())
        }
    }

    #[async_trait]
    impl BulkLoadRow for &TestUser {
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
                .write_column_value(*column_index, &ColumnValues::Int(self.value1))
                .await?;
            *column_index += 1;
            writer
                .write_column_value(*column_index, &ColumnValues::Int(self.value2))
                .await?;
            *column_index += 1;
            writer
                .write_column_value(*column_index, &ColumnValues::Int(self.value3))
                .await?;
            *column_index += 1;
            Ok(())
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_bulk_copy_with_server_metadata() {
        let mut client = begin_connection(&build_tcp_datasource()).await;

        // Create temp table (automatically cleaned up)
        client
            .execute(
                "CREATE TABLE #BulkCopyMetadataTest (
                    id INT NOT NULL,
                    value1 INT NOT NULL,
                    value2 INT NOT NULL,
                    value3 INT NOT NULL
                )"
                .to_string(),
                (),
            )
            .await
            .expect("Failed to create test table");

        // Close the query to free up the connection
        client.close_query().await.expect("Failed to close query");

        // Prepare test data
        let test_data = vec![
            TestUser {
                id: 1,
                value1: 100,
                value2: 200,
                value3: 300,
            },
            TestUser {
                id: 2,
                value1: 101,
                value2: 201,
                value3: 301,
            },
            TestUser {
                id: 3,
                value1: 102,
                value2: 202,
                value3: 302,
            },
        ];

        // Execute bulk copy using public API (without explicit column mappings - should use ordinal mapping)
        let result = {
            let bulk_copy = BulkCopy::new(&mut client, "#BulkCopyMetadataTest");
            bulk_copy
                .batch_size(1000)
                .write_to_server_zerocopy(&test_data)
                .await
                .expect("Bulk copy failed")
        };

        println!(
            "DEBUG: Bulk copy completed, rows_affected: {}",
            result.rows_affected
        );

        // Check actual row count in database before assertion
        client
            .execute(
                "SELECT COUNT(*) as cnt FROM #BulkCopyMetadataTest".to_string(),
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
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_retrieve_destination_metadata_basic() {
        let mut client = begin_connection(&build_tcp_datasource()).await;

        let table_name = "#MetadataTest";

        // Drop table if it exists from previous run
        let drop_sql = format!(
            "IF OBJECT_ID('tempdb..{}') IS NOT NULL DROP TABLE {}",
            table_name, table_name
        );
        client.execute(drop_sql, ()).await.ok();
        client.close_query().await.ok();

        // Create temp table with various data types
        let create_sql = format!(
            "CREATE TABLE {} (
                    id INT NOT NULL,
                    name NVARCHAR(100) NOT NULL,
                    age TINYINT NULL,
                    salary DECIMAL(18, 2) NULL,
                    active BIT NOT NULL
                )",
            table_name
        );

        client
            .execute(create_sql, ())
            .await
            .expect("Failed to create test table");

        client.close_query().await.expect("Failed to close query");

        // Retrieve metadata
        let mut bulk_copy = BulkCopy::new(&mut client, table_name);
        let metadata = bulk_copy
            .retrieve_destination_metadata()
            .await
            .expect("Failed to retrieve metadata");

        // Verify we got 5 columns
        assert_eq!(metadata.len(), 5, "Expected 5 columns");

        // Verify column names and ordinals
        assert_eq!(metadata[0].column_name, "id");
        assert_eq!(metadata[1].column_name, "name");
        assert_eq!(metadata[2].column_name, "age");
        assert_eq!(metadata[3].column_name, "salary");
        assert_eq!(metadata[4].column_name, "active");

        // Verify nullable flags
        assert!(!metadata[0].is_nullable, "id should not be nullable");
        assert!(!metadata[1].is_nullable, "name should not be nullable");
        assert!(metadata[2].is_nullable, "age should be nullable");
        assert!(metadata[3].is_nullable, "salary should be nullable");
        assert!(!metadata[4].is_nullable, "active should not be nullable");

        // Verify types
        assert_eq!(metadata[0].sql_type, SqlDbType::Int);
        assert_eq!(metadata[1].sql_type, SqlDbType::NVarChar);
        assert_eq!(metadata[2].sql_type, SqlDbType::TinyInt);
        assert_eq!(metadata[3].sql_type, SqlDbType::Decimal);
        assert_eq!(metadata[4].sql_type, SqlDbType::Bit);

        // Verify precision and scale for DECIMAL column
        assert_eq!(metadata[3].precision, 18);
        assert_eq!(metadata[3].scale, 2);

        println!("Metadata test passed: Retrieved {} columns", metadata.len());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_retrieve_destination_metadata_with_identity() {
        let mut client = begin_connection(&build_tcp_datasource()).await;

        // Create temp table with identity column
        client
            .execute(
                "CREATE TABLE #IdentityTest (
                    id INT IDENTITY(1,1) PRIMARY KEY,
                    value NVARCHAR(50) NOT NULL
                )"
                .to_string(),
                (),
            )
            .await
            .expect("Failed to create test table");

        client.close_query().await.expect("Failed to close query");

        // Retrieve metadata
        let mut bulk_copy = BulkCopy::new(&mut client, "#IdentityTest");
        let metadata = bulk_copy
            .retrieve_destination_metadata()
            .await
            .expect("Failed to retrieve metadata");

        // Verify identity column is marked correctly
        assert_eq!(metadata.len(), 2);
        assert!(
            metadata[0].is_identity,
            "id column should be marked as identity"
        );
        assert!(
            !metadata[1].is_identity,
            "value column should not be identity"
        );

        println!("Identity metadata test passed");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_retrieve_destination_metadata_with_computed() {
        let mut client = begin_connection(&build_tcp_datasource()).await;

        // Create temp table with computed column
        client
            .execute(
                "CREATE TABLE #ComputedTest (
                    id INT NOT NULL,
                    value1 INT NOT NULL,
                    value2 INT NOT NULL,
                    total AS (value1 + value2)
                )"
                .to_string(),
                (),
            )
            .await
            .expect("Failed to create test table");

        client.close_query().await.expect("Failed to close query");

        // Retrieve metadata
        let mut bulk_copy = BulkCopy::new(&mut client, "#ComputedTest");
        let metadata = bulk_copy
            .retrieve_destination_metadata()
            .await
            .expect("Failed to retrieve metadata");

        // Verify columns are retrieved (computed columns may be included in metadata)
        // Note: BulkCopyColumnMetadata doesn't track is_computed flag as computed columns
        // are typically skipped during bulk copy operations
        assert_eq!(metadata.len(), 4);
        assert_eq!(metadata[0].column_name, "id");
        assert_eq!(metadata[1].column_name, "value1");
        assert_eq!(metadata[2].column_name, "value2");
        assert_eq!(metadata[3].column_name, "total");

        println!("Computed column metadata test passed");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_retrieve_destination_metadata_caching() {
        let mut client = begin_connection(&build_tcp_datasource()).await;

        // Create temp table
        client
            .execute(
                "CREATE TABLE #CacheTest (
                    id INT NOT NULL,
                    name NVARCHAR(100) NOT NULL
                )"
                .to_string(),
                (),
            )
            .await
            .expect("Failed to create test table");

        client.close_query().await.expect("Failed to close query");

        // Retrieve metadata twice
        let mut bulk_copy = BulkCopy::new(&mut client, "#CacheTest");
        let metadata1 = bulk_copy
            .retrieve_destination_metadata()
            .await
            .expect("Failed to retrieve metadata (first call)");

        let metadata2 = bulk_copy
            .retrieve_destination_metadata()
            .await
            .expect("Failed to retrieve metadata (second call)");

        // Both should return the same metadata
        assert_eq!(metadata1.len(), metadata2.len());
        assert_eq!(metadata1[0].column_name, metadata2[0].column_name);
        assert_eq!(metadata1[1].column_name, metadata2[1].column_name);

        println!("Metadata caching test passed");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_retrieve_destination_metadata_string_types() {
        let mut client = begin_connection(&build_tcp_datasource()).await;

        // Create temp table with various string types
        client
            .execute(
                "CREATE TABLE #StringTest (
                    col_varchar VARCHAR(50),
                    col_nvarchar NVARCHAR(100),
                    col_char CHAR(10),
                    col_nchar NCHAR(20),
                    col_varchar_max VARCHAR(MAX),
                    col_nvarchar_max NVARCHAR(MAX)
                )"
                .to_string(),
                (),
            )
            .await
            .expect("Failed to create test table");

        client.close_query().await.expect("Failed to close query");

        // Retrieve metadata
        let mut bulk_copy = BulkCopy::new(&mut client, "#StringTest");
        let metadata = bulk_copy
            .retrieve_destination_metadata()
            .await
            .expect("Failed to retrieve metadata");

        // Verify types and lengths
        use mssql_tds::datatypes::bulk_copy_metadata::SqlDbType;
        assert_eq!(metadata.len(), 6);

        // VARCHAR(50)
        assert_eq!(metadata[0].sql_type, SqlDbType::VarChar);
        assert_eq!(metadata[0].length, 50);

        // NVARCHAR(100) - uses 2 bytes per char
        assert_eq!(metadata[1].sql_type, SqlDbType::NVarChar);
        assert_eq!(metadata[1].length, 200);

        // CHAR(10)
        assert_eq!(metadata[2].sql_type, SqlDbType::Char);
        assert_eq!(metadata[2].length, 10);

        // NCHAR(20) - uses 2 bytes per char
        assert_eq!(metadata[3].sql_type, SqlDbType::NChar);
        assert_eq!(metadata[3].length, 40);

        // VARCHAR(MAX)
        assert_eq!(metadata[4].sql_type, SqlDbType::VarChar);
        assert_eq!(metadata[4].length, -1);

        // NVARCHAR(MAX)
        assert_eq!(metadata[5].sql_type, SqlDbType::NVarChar);
        assert_eq!(metadata[5].length, -1);

        println!("String types metadata test passed");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_retrieve_destination_metadata_numeric_types() {
        let mut client = begin_connection(&build_tcp_datasource()).await;

        // Create temp table with various numeric types
        client
            .execute(
                "CREATE TABLE #NumericTest (
                    col_tinyint TINYINT,
                    col_smallint SMALLINT,
                    col_int INT,
                    col_bigint BIGINT,
                    col_decimal DECIMAL(10, 3),
                    col_numeric NUMERIC(15, 5),
                    col_money MONEY,
                    col_smallmoney SMALLMONEY,
                    col_real REAL,
                    col_float FLOAT
                )"
                .to_string(),
                (),
            )
            .await
            .expect("Failed to create test table");

        client.close_query().await.expect("Failed to close query");

        // Retrieve metadata
        let mut bulk_copy = BulkCopy::new(&mut client, "#NumericTest");
        let metadata = bulk_copy
            .retrieve_destination_metadata()
            .await
            .expect("Failed to retrieve metadata");

        // Verify types
        use mssql_tds::datatypes::bulk_copy_metadata::SqlDbType;
        assert_eq!(metadata.len(), 10);

        assert_eq!(metadata[0].sql_type, SqlDbType::TinyInt);
        assert_eq!(metadata[1].sql_type, SqlDbType::SmallInt);
        assert_eq!(metadata[2].sql_type, SqlDbType::Int);
        assert_eq!(metadata[3].sql_type, SqlDbType::BigInt);
        assert_eq!(metadata[4].sql_type, SqlDbType::Decimal);
        assert_eq!(metadata[5].sql_type, SqlDbType::Numeric);
        assert_eq!(metadata[6].sql_type, SqlDbType::Money);
        assert_eq!(metadata[7].sql_type, SqlDbType::SmallMoney);
        assert_eq!(metadata[8].sql_type, SqlDbType::Real);
        assert_eq!(metadata[9].sql_type, SqlDbType::Float);

        // Verify precision and scale for DECIMAL and NUMERIC
        assert_eq!(metadata[4].precision, 10);
        assert_eq!(metadata[4].scale, 3);
        assert_eq!(metadata[5].precision, 15);
        assert_eq!(metadata[5].scale, 5);

        println!("Numeric types metadata test passed");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_retrieve_destination_metadata_nonexistent_table() {
        let mut client = begin_connection(&build_tcp_datasource()).await;

        // Try to retrieve metadata for a non-existent table
        let mut bulk_copy = BulkCopy::new(&mut client, "#NonExistentTable");
        let result = bulk_copy.retrieve_destination_metadata().await;

        // Should return an error
        assert!(result.is_err(), "Expected error for non-existent table");

        if let Err(e) = result {
            println!("Expected error occurred: {:?}", e);
        }
    }

    // -----------------------------------------------------------------------
    // NVARCHAR(n) odd-length metadata regression tests
    //
    // Verifies that bulk copy works with NVARCHAR(n) where n is odd.
    // Previously, the COLMETADATA writer sent n (char count) instead of 2n
    // (byte count) as the MaxLength field.  When n is odd, SQL Server
    // rejects it with "Unicode data is odd byte size".
    // -----------------------------------------------------------------------

    #[derive(Debug, Clone)]
    struct NVarCharRow {
        id: i32,
        value: String,
    }

    #[async_trait]
    impl BulkLoadRow for NVarCharRow {
        async fn write_to_packet(
            &self,
            writer: &mut mssql_tds::message::bulk_load::StreamingBulkLoadWriter<'_>,
            column_index: &mut usize,
        ) -> TdsResult<()> {
            use mssql_tds::datatypes::sql_string::SqlString;
            writer
                .write_column_value(*column_index, &ColumnValues::Int(self.id))
                .await?;
            *column_index += 1;
            let sql_string = SqlString::from_utf8_string(self.value.clone());
            writer
                .write_column_value(*column_index, &ColumnValues::String(sql_string))
                .await?;
            *column_index += 1;
            Ok(())
        }
    }

    #[async_trait]
    impl BulkLoadRow for &NVarCharRow {
        async fn write_to_packet(
            &self,
            writer: &mut mssql_tds::message::bulk_load::StreamingBulkLoadWriter<'_>,
            column_index: &mut usize,
        ) -> TdsResult<()> {
            use mssql_tds::datatypes::sql_string::SqlString;
            writer
                .write_column_value(*column_index, &ColumnValues::Int(self.id))
                .await?;
            *column_index += 1;
            let sql_string = SqlString::from_utf8_string(self.value.clone());
            writer
                .write_column_value(*column_index, &ColumnValues::String(sql_string))
                .await?;
            *column_index += 1;
            Ok(())
        }
    }

    /// Regression test: NVARCHAR with odd character lengths (1, 3, 5, 7, 15, 255).
    /// These caused "Unicode data is odd byte size" before the fix because
    /// COLMETADATA MaxLength was sent as char count (odd) instead of byte count (even).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_bulk_copy_nvarchar_odd_lengths() {
        let mut client = begin_connection(&build_tcp_datasource()).await;

        for n in [1, 3, 5, 7, 15, 255] {
            let table_name = format!("#NVarCharOdd_{}", n);

            client
                .execute(
                    format!(
                        "CREATE TABLE {} (id INT NOT NULL, val NVARCHAR({}) NOT NULL)",
                        table_name, n
                    ),
                    (),
                )
                .await
                .expect("Failed to create table");
            client.close_query().await.expect("Failed to close query");

            let test_data = vec![
                NVarCharRow {
                    id: 1,
                    value: "A".to_string(),
                },
                NVarCharRow {
                    id: 2,
                    value: "B".to_string(),
                },
            ];

            let result = {
                let bulk_copy = BulkCopy::new(&mut client, &table_name);
                bulk_copy
                    .batch_size(100)
                    .write_to_server_zerocopy(&test_data)
                    .await
            };

            assert!(
                result.is_ok(),
                "NVARCHAR({}) bulk copy failed: {:?}",
                n,
                result.err()
            );

            // Verify round-trip
            client
                .execute(
                    format!("SELECT id, val FROM {} ORDER BY id", table_name),
                    (),
                )
                .await
                .expect("Failed to query");

            let resultset = client.get_current_resultset().expect("No resultset");
            let row1 = resultset
                .next_row()
                .await
                .expect("Failed to read")
                .expect("No row");
            let row2 = resultset
                .next_row()
                .await
                .expect("Failed to read")
                .expect("No row");

            if let ColumnValues::String(s) = &row1[1] {
                assert_eq!(s.to_utf8_string(), "A", "NVARCHAR({}) row 1 mismatch", n);
            } else {
                panic!("Expected String for NVARCHAR({}) row 1", n);
            }
            if let ColumnValues::String(s) = &row2[1] {
                assert_eq!(s.to_utf8_string(), "B", "NVARCHAR({}) row 2 mismatch", n);
            } else {
                panic!("Expected String for NVARCHAR({}) row 2", n);
            }

            client.close_query().await.expect("Failed to close query");
            println!("NVARCHAR({}) passed", n);
        }
    }

    /// Verify metadata reports correct byte lengths for NVARCHAR with odd char counts.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_retrieve_metadata_nvarchar_odd_lengths() {
        let mut client = begin_connection(&build_tcp_datasource()).await;

        client
            .execute(
                "CREATE TABLE #NVarCharOddMeta (
                    col1 NVARCHAR(1),
                    col3 NVARCHAR(3),
                    col5 NVARCHAR(5),
                    col7 NVARCHAR(7),
                    col50 NVARCHAR(50)
                )"
                .to_string(),
                (),
            )
            .await
            .expect("Failed to create table");
        client.close_query().await.expect("Failed to close query");

        let mut bulk_copy = BulkCopy::new(&mut client, "#NVarCharOddMeta");
        let metadata = bulk_copy
            .retrieve_destination_metadata()
            .await
            .expect("Failed to retrieve metadata");

        assert_eq!(metadata.len(), 5);
        // length is always in bytes = 2 * char_count
        assert_eq!(metadata[0].sql_type, SqlDbType::NVarChar);
        assert_eq!(metadata[0].length, 2, "NVARCHAR(1) should be 2 bytes");
        assert_eq!(metadata[1].length, 6, "NVARCHAR(3) should be 6 bytes");
        assert_eq!(metadata[2].length, 10, "NVARCHAR(5) should be 10 bytes");
        assert_eq!(metadata[3].length, 14, "NVARCHAR(7) should be 14 bytes");
        assert_eq!(metadata[4].length, 100, "NVARCHAR(50) should be 100 bytes");

        println!("NVARCHAR odd-length metadata assertions passed");
    }
}
