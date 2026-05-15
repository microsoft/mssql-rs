// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Integration tests for SQL Server Vector data type
//! These tests require SQL Server 2025 or later with Vector support enabled.
//!
//! Note: These tests will fail if connecting to a SQL Server version that doesn't support
//! the Vector data type (prior to SQL Server 2025). In the future, these tests should be
//! conditionally enabled based on server version detection.

#[cfg(test)]
mod common;

#[cfg(test)]
mod vector_integration_tests {
    use crate::common::{begin_connection, build_tcp_datasource, init_tracing};
    use mssql_tds::connection::tds_client::{ResultSet, ResultSetClient};
    use mssql_tds::datatypes::column_values::ColumnValues;
    use mssql_tds::datatypes::sqldatatypes::VectorBaseType;

    #[ctor::ctor]
    fn init() {
        init_tracing();
    }

    /// Test basic vector deserialization with a simple 3-dimensional vector
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_vector_basic_deserialization() {
        let mut client = begin_connection(&build_tcp_datasource()).await;

        // Query a simple 3-dimensional vector
        let query = "SELECT CAST('[1.0, 2.0, 3.0]' AS VECTOR(3)) AS VectorColumn";

        client.execute(query.to_string(), None, None).await.unwrap();

        // Get the result set
        if let Some(resultset) = client.get_current_resultset() {
            // Verify metadata
            let columns = resultset.get_metadata();
            assert_eq!(columns.len(), 1);
            assert_eq!(columns[0].column_name, "VectorColumn");

            // Read the row
            let mut row_count = 0;
            while let Some(row) = resultset.next_row().await.unwrap() {
                row_count += 1;
                assert_eq!(row.len(), 1);

                // Verify the vector data
                match &row[0] {
                    ColumnValues::Vector(vector) => {
                        assert_eq!(vector.dimension_count(), 3);
                        assert_eq!(vector.base_type(), VectorBaseType::Float32);
                        let values = vector.as_f32().expect("Should be Float32 vector");
                        assert_eq!(values.len(), 3);
                        assert!((values[0] - 1.0).abs() < 0.001);
                        assert!((values[1] - 2.0).abs() < 0.001);
                        assert!((values[2] - 3.0).abs() < 0.001);
                    }
                    _ => panic!("Expected Vector column value, got: {:?}", row[0]),
                }
            }
            assert_eq!(row_count, 1, "Expected 1 row");
        } else {
            panic!("Expected a result set");
        }
    }

    /// Test basic vector16 deserialization with a simple 3-dimensional vector
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[ignore = "Not supported in pipeline currently so skipping"]
    async fn test_vector16_basic_deserialization() {
        let mut client = begin_connection(&build_tcp_datasource()).await;

        // Enable preview features before testing
        // TODO: disable once v16 is available in sql server
        client
            .execute("USE UserDatabase;".to_string(), None, None)
            .await
            .unwrap();
        client
            .execute(
                "ALTER DATABASE SCOPED CONFIGURATION SET PREVIEW_FEATURES = ON;".to_string(),
                None,
                None,
            )
            .await
            .unwrap();

        // Query a simple 3-dimensional float16 vector
        let query = "SELECT CAST('[1.0, 2.0, 3.0]' AS VECTOR(3, float16)) AS VectorColumn";
        client.execute(query.to_string(), None, None).await.unwrap();

        if let Some(resultset) = client.get_current_resultset() {
            // Verify metadata
            let columns = resultset.get_metadata();
            assert_eq!(columns.len(), 1);
            assert_eq!(columns[0].column_name, "VectorColumn");

            // Read the row
            let mut row_count = 0;
            while let Some(row) = resultset.next_row().await.unwrap() {
                row_count += 1;
                assert_eq!(row.len(), 1);

                // Verify the vector data
                match &row[0] {
                    ColumnValues::Vector(vector) => {
                        assert_eq!(vector.dimension_count(), 3);
                        assert_eq!(vector.base_type(), VectorBaseType::Float16);
                        let values = vector.as_f32().expect("Should unpack to f32 vector slice");
                        assert_eq!(values.len(), 3);
                        assert!((values[0] - 1.0).abs() < 0.001);
                        assert!((values[1] - 2.0).abs() < 0.001);
                        assert!((values[2] - 3.0).abs() < 0.001);
                    }
                    _ => panic!("Expected Vector column value, got: {:?}", row[0]),
                }
            }
            assert_eq!(row_count, 1, "Expected 1 row");
        } else {
            panic!("Expected a result set");
        }
    }

    /// Test vector column metadata via get_metadata(): type, length, scale, flags
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_vector_metadata_fields() {
        use mssql_tds::datatypes::sqldatatypes::{TdsDataType, VECTOR_HEADER_SIZE, VectorBaseType};

        let mut client = begin_connection(&build_tcp_datasource()).await;

        let query = "SELECT
            CAST('[1.0, 2.0, 3.0]' AS VECTOR(3)) AS NonNullVec,
            CAST(NULL AS VECTOR(3)) AS NullVec";

        client.execute(query.to_string(), None, None).await.unwrap();

        let resultset = client
            .get_current_resultset()
            .expect("Expected a result set");

        let metadata = resultset.get_metadata();
        assert_eq!(metadata.len(), 2);

        // Expected metadata values for VECTOR(3), Float32 base type
        let expected_length = VECTOR_HEADER_SIZE + 3 * VectorBaseType::Float32.element_size_bytes();
        let expected_scale = VectorBaseType::Float32 as u8; // SCALE carries base type byte

        // Column 0: NonNullVec
        let col0 = &metadata[0];
        assert_eq!(col0.column_name, "NonNullVec");
        assert_eq!(col0.data_type, TdsDataType::Vector);
        assert_eq!(col0.type_info.length, expected_length);
        assert_eq!(col0.get_scale(), Some(expected_scale));
        assert!(!col0.is_plp());
        // Expression columns are generally nullable in SQL Server metadata
        assert!(col0.is_nullable());

        // Column 1: NullVec
        let col1 = &metadata[1];
        assert_eq!(col1.column_name, "NullVec");
        assert_eq!(col1.data_type, TdsDataType::Vector);
        assert_eq!(col1.type_info.length, expected_length);
        assert_eq!(col1.get_scale(), Some(expected_scale));
        assert!(!col1.is_plp());
        assert!(col1.is_nullable());
    }

    /// Test vector16 column metadata via get_metadata(): type, length, scale, flags
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[ignore = "Not supported in pipeline currently so skipping"]
    async fn test_vector16_metadata_fields() {
        use mssql_tds::datatypes::sqldatatypes::{TdsDataType, VECTOR_HEADER_SIZE, VectorBaseType};

        let mut client = begin_connection(&build_tcp_datasource()).await;

        // TODO: disable once v16 is available in sql server
        client
            .execute("USE UserDatabase;".to_string(), None, None)
            .await
            .unwrap();
        client
            .execute(
                "ALTER DATABASE SCOPED CONFIGURATION SET PREVIEW_FEATURES = ON;".to_string(),
                None,
                None,
            )
            .await
            .unwrap();

        let query = "SELECT
            CAST('[1.0, 2.0, 3.0]' AS VECTOR(3, float16)) AS NonNullVec,
            CAST(NULL AS VECTOR(3, float16)) AS NullVec";

        client.execute(query.to_string(), None, None).await.unwrap();

        let resultset = client
            .get_current_resultset()
            .expect("Expected a result set");

        let metadata = resultset.get_metadata();
        assert_eq!(metadata.len(), 2);

        // Expected metadata values for VECTOR(3, float16), Float16 base type
        let expected_length = VECTOR_HEADER_SIZE + 3 * VectorBaseType::Float16.element_size_bytes();
        let expected_scale = VectorBaseType::Float16 as u8; // SCALE carries base type byte

        // Column 0: NonNullVec
        let col0 = &metadata[0];
        assert_eq!(col0.column_name, "NonNullVec");
        assert_eq!(col0.data_type, TdsDataType::Vector);
        assert_eq!(col0.type_info.length, expected_length);
        assert_eq!(col0.get_scale(), Some(expected_scale));
        assert!(!col0.is_plp());
        // Expression columns are generally nullable in SQL Server metadata
        assert!(col0.is_nullable());

        // Column 1: NullVec
        let col1 = &metadata[1];
        assert_eq!(col1.column_name, "NullVec");
        assert_eq!(col1.data_type, TdsDataType::Vector);
        assert_eq!(col1.type_info.length, expected_length);
        assert_eq!(col1.get_scale(), Some(expected_scale));
        assert!(!col1.is_plp());
        assert!(col1.is_nullable());
    }

    /// Test vector with single dimension
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_vector_single_dimension() {
        let mut client = begin_connection(&build_tcp_datasource()).await;

        let query = "SELECT CAST('[42.5]' AS VECTOR(1)) AS SingleVector";

        client.execute(query.to_string(), None, None).await.unwrap();

        if let Some(resultset) = client.get_current_resultset() {
            let mut row_count = 0;
            while let Some(row) = resultset.next_row().await.unwrap() {
                row_count += 1;
                match &row[0] {
                    ColumnValues::Vector(vector) => {
                        assert_eq!(vector.dimension_count(), 1);
                        assert_eq!(vector.base_type(), VectorBaseType::Float32);
                        let values = vector.as_f32().unwrap();
                        assert!((values[0] - 42.5).abs() < 0.001);
                    }
                    _ => panic!("Expected Vector column value"),
                }
            }
            assert_eq!(row_count, 1, "Expected exactly 1 row");
        }
    }

    /// Test vector with maximum dimensions (1998)
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_vector_max_dimensions() {
        let mut client = begin_connection(&build_tcp_datasource()).await;

        // Create a vector with 1998 dimensions (max supported)
        let vector_values: Vec<String> = (0..1998).map(|i| format!("{}.0", i)).collect();
        let vector_literal = format!("[{}]", vector_values.join(", "));
        let query = format!(
            "SELECT CAST('{}' AS VECTOR(1998)) AS MaxVector",
            vector_literal
        );

        client.execute(query, None, None).await.unwrap();

        if let Some(resultset) = client.get_current_resultset() {
            let mut row_count = 0;
            while let Some(row) = resultset.next_row().await.unwrap() {
                row_count += 1;
                match &row[0] {
                    ColumnValues::Vector(vector) => {
                        assert_eq!(vector.dimension_count(), 1998);
                        assert_eq!(vector.base_type(), VectorBaseType::Float32);
                        let values = vector.as_f32().unwrap();
                        assert_eq!(values.len(), 1998);

                        // Spot check a few values
                        assert!((values[0] - 0.0).abs() < 0.001);
                        assert!((values[100] - 100.0).abs() < 0.001);
                        assert!((values[1997] - 1997.0).abs() < 0.001);
                    }
                    _ => panic!("Expected Vector column value"),
                }
            }
            assert_eq!(row_count, 1, "Expected exactly 1 row");
        }
    }

    /// Test vector16 with maximum dimensions (3996)
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[ignore = "Not supported in pipeline currently so skipping"]
    async fn test_vector16_max_dimensions() {
        let mut client = begin_connection(&build_tcp_datasource()).await;

        // TODO: disable once v16 is available in sql server
        client
            .execute("USE UserDatabase;".to_string(), None, None)
            .await
            .unwrap();
        client
            .execute(
                "ALTER DATABASE SCOPED CONFIGURATION SET PREVIEW_FEATURES = ON;".to_string(),
                None,
                None,
            )
            .await
            .unwrap();

        // Create a vector with 3996 dimensions (max supported for float16)
        let vector_values: Vec<String> = (0..3996).map(|i| format!("{}.0", i)).collect();
        let vector_literal = format!("[{}]", vector_values.join(", "));
        let query = format!(
            "SELECT CAST('{}' AS VECTOR(3996, float16)) AS MaxVector",
            vector_literal
        );

        client.execute(query, None, None).await.unwrap();

        if let Some(resultset) = client.get_current_resultset() {
            let mut row_count = 0;
            while let Some(row) = resultset.next_row().await.unwrap() {
                row_count += 1;
                match &row[0] {
                    ColumnValues::Vector(vector) => {
                        assert_eq!(vector.dimension_count(), 3996);
                        assert_eq!(vector.base_type(), VectorBaseType::Float16);
                        let values = vector.as_f32().unwrap();
                        assert_eq!(values.len(), 3996);

                        // Spot check a few values
                        assert!((values[0] - 0.0).abs() < 0.001);
                        assert!((values[100] - 100.0).abs() < 0.001);
                        // Float16 rounds 3995.0 to 3996.0 due to 11-bit precision limit above 2048.
                        assert!((values[3995] - 3995.0).abs() <= 2.0);
                    }
                    _ => panic!("Expected Vector column value"),
                }
            }
            assert_eq!(row_count, 1, "Expected exactly 1 row");
        }
    }

    /// Test NULL vector value
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_vector_null_value() {
        let mut client = begin_connection(&build_tcp_datasource()).await;

        let query = "SELECT CAST(NULL AS VECTOR(3)) AS NullVector";

        client.execute(query.to_string(), None, None).await.unwrap();

        if let Some(resultset) = client.get_current_resultset() {
            let mut row_count = 0;
            while let Some(row) = resultset.next_row().await.unwrap() {
                row_count += 1;
                match &row[0] {
                    ColumnValues::Null => {
                        // Expected - NULL vector should be ColumnValues::Null
                    }
                    _ => panic!("Expected Null column value, got: {:?}", row[0]),
                }
            }
            assert_eq!(row_count, 1, "Expected exactly 1 row");
        }
    }

    /// Test vector with mixed positive/negative/zero values
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_vector_mixed_values() {
        let mut client = begin_connection(&build_tcp_datasource()).await;

        let query = "SELECT CAST('[0.0, -1.0, 1.0, -100.5, 100.5]' AS VECTOR(5)) AS MixedVector";

        client.execute(query.to_string(), None, None).await.unwrap();

        if let Some(resultset) = client.get_current_resultset() {
            let mut row_count = 0;
            while let Some(row) = resultset.next_row().await.unwrap() {
                row_count += 1;
                match &row[0] {
                    ColumnValues::Vector(vector) => {
                        assert_eq!(vector.dimension_count(), 5);
                        assert_eq!(vector.base_type(), VectorBaseType::Float32);
                        let values = vector.as_f32().unwrap();
                        assert!((values[0] - 0.0).abs() < 0.001);
                        assert!((values[1] - (-1.0)).abs() < 0.001);
                        assert!((values[2] - 1.0).abs() < 0.001);
                        assert!((values[3] - (-100.5)).abs() < 0.001);
                        assert!((values[4] - 100.5).abs() < 0.001);
                    }
                    _ => panic!("Expected Vector column value"),
                }
            }
            assert_eq!(row_count, 1, "Expected exactly 1 row");
        }
    }

    /// Test multiple vector columns in a single query
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_multiple_vector_columns() {
        let mut client = begin_connection(&build_tcp_datasource()).await;

        let query = "SELECT 
            CAST('[1.0, 2.0]' AS VECTOR(2)) AS Vec1,
            CAST('[3.0, 4.0, 5.0]' AS VECTOR(3)) AS Vec2,
            42 AS IntCol";

        client.execute(query.to_string(), None, None).await.unwrap();

        if let Some(resultset) = client.get_current_resultset() {
            let columns = resultset.get_metadata();
            assert_eq!(columns.len(), 3);

            let mut row_count = 0;
            while let Some(row) = resultset.next_row().await.unwrap() {
                row_count += 1;
                assert_eq!(row.len(), 3);

                // First vector
                match &row[0] {
                    ColumnValues::Vector(vector) => {
                        assert_eq!(vector.dimension_count(), 2);
                        assert_eq!(vector.base_type(), VectorBaseType::Float32);
                        let values = vector.as_f32().unwrap();
                        assert!((values[0] - 1.0).abs() < 0.001);
                        assert!((values[1] - 2.0).abs() < 0.001);
                    }
                    _ => panic!("Expected Vector in first column"),
                }

                // Second vector
                match &row[1] {
                    ColumnValues::Vector(vector) => {
                        assert_eq!(vector.dimension_count(), 3);
                        assert_eq!(vector.base_type(), VectorBaseType::Float32);
                        let values = vector.as_f32().unwrap();
                        assert!((values[0] - 3.0).abs() < 0.001);
                        assert!((values[1] - 4.0).abs() < 0.001);
                        assert!((values[2] - 5.0).abs() < 0.001);
                    }
                    _ => panic!("Expected Vector in second column"),
                }

                // Integer column
                match &row[2] {
                    ColumnValues::Int(value) => {
                        assert_eq!(*value, 42);
                    }
                    _ => panic!("Expected Int in third column"),
                }
            }
            assert_eq!(row_count, 1, "Expected exactly 1 row");
        }
    }

    /// Test vector in a table with multiple rows
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_vector_multiple_rows() {
        let mut client = begin_connection(&build_tcp_datasource()).await;

        // Create temp table with vector column
        let setup = "
            CREATE TABLE #VectorTest (
                Id INT,
                Embedding VECTOR(3)
            );
            INSERT INTO #VectorTest VALUES (1, CAST('[1.0, 2.0, 3.0]' AS VECTOR(3)));
            INSERT INTO #VectorTest VALUES (2, CAST('[4.0, 5.0, 6.0]' AS VECTOR(3)));
            INSERT INTO #VectorTest VALUES (3, NULL);
        ";

        client.execute(setup.to_string(), None, None).await.unwrap();

        // Consume setup results
        while client.move_to_next().await.unwrap() {}

        // Query the data
        client
            .execute(
                "SELECT Id, Embedding FROM #VectorTest ORDER BY Id".to_string(),
                None,
                None,
            )
            .await
            .unwrap();

        let expected_values = [Some(vec![1.0, 2.0, 3.0]), Some(vec![4.0, 5.0, 6.0]), None];

        let mut row_index = 0;
        if let Some(resultset) = client.get_current_resultset() {
            while let Some(row) = resultset.next_row().await.unwrap() {
                assert_eq!(row.len(), 2);

                // Check Id
                match &row[0] {
                    ColumnValues::Int(id) => {
                        assert_eq!(*id, (row_index + 1) as i32);
                    }
                    _ => panic!("Expected Int for Id"),
                }

                // Check Vector/Null
                match (&row[1], &expected_values[row_index]) {
                    (ColumnValues::Vector(vector), Some(expected)) => {
                        assert_eq!(vector.dimension_count(), expected.len() as u16);
                        assert_eq!(vector.base_type(), VectorBaseType::Float32);
                        let values = vector.as_f32().unwrap();
                        for (i, &expected_val) in expected.iter().enumerate() {
                            assert!((values[i] - expected_val).abs() < 0.001);
                        }
                    }
                    (ColumnValues::Null, None) => {
                        // Expected NULL
                    }
                    (actual, expected) => {
                        panic!(
                            "Mismatch at row {}: expected {:?}, got {:?}",
                            row_index, expected, actual
                        );
                    }
                }

                row_index += 1;
            }
        }
        assert_eq!(row_index, 3, "Expected 3 rows");
    }

    /// Test vector16 NULL value
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[ignore = "Not supported in pipeline currently so skipping"]
    async fn test_vector16_null_value() {
        let mut client = begin_connection(&build_tcp_datasource()).await;

        // TODO: disable once v16 is available in sql server
        client
            .execute("USE UserDatabase;".to_string(), None, None)
            .await
            .unwrap();
        client
            .execute(
                "ALTER DATABASE SCOPED CONFIGURATION SET PREVIEW_FEATURES = ON;".to_string(),
                None,
                None,
            )
            .await
            .unwrap();

        let query = "SELECT CAST(NULL AS VECTOR(3, float16)) AS NullVector";

        client.execute(query.to_string(), None, None).await.unwrap();

        if let Some(resultset) = client.get_current_resultset() {
            let mut row_count = 0;
            while let Some(row) = resultset.next_row().await.unwrap() {
                row_count += 1;
                match &row[0] {
                    ColumnValues::Null => {
                        // Expected - NULL vector should be ColumnValues::Null
                    }
                    _ => panic!("Expected Null column value, got: {:?}", row[0]),
                }
            }
            assert_eq!(row_count, 1, "Expected exactly 1 row");
        }
    }

    /// Test both float32 and float16 vectors in a table with multiple rows
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[ignore = "Not supported in pipeline currently so skipping"]
    async fn test_vector_mixed_types_multiple_rows() {
        let mut client = begin_connection(&build_tcp_datasource()).await;

        // TODO: disable once v16 is available in sql server
        client
            .execute("USE UserDatabase;".to_string(), None, None)
            .await
            .unwrap();
        client
            .execute(
                "ALTER DATABASE SCOPED CONFIGURATION SET PREVIEW_FEATURES = ON;".to_string(),
                None,
                None,
            )
            .await
            .unwrap();

        // Create temp table with both vector column types
        let setup = "
            CREATE TABLE #MixedVectorTest (
                Id INT,
                Vec32 VECTOR(2),
                Vec16 VECTOR(2, float16)
            );
            INSERT INTO #MixedVectorTest VALUES (1, CAST('[1.0, 2.0]' AS VECTOR(2)), CAST('[1.0, 2.0]' AS VECTOR(2, float16)));
            INSERT INTO #MixedVectorTest VALUES (2, CAST('[4.0, 5.0]' AS VECTOR(2)), NULL);
            INSERT INTO #MixedVectorTest VALUES (3, NULL, CAST('[3.0, 4.0]' AS VECTOR(2, float16)));
        ";

        client.execute(setup.to_string(), None, None).await.unwrap();

        // Consume setup results
        while client.move_to_next().await.unwrap() {}

        // Query the data
        client
            .execute(
                "SELECT Id, Vec32, Vec16 FROM #MixedVectorTest ORDER BY Id".to_string(),
                None,
                None,
            )
            .await
            .unwrap();

        let expected_v32 = [Some(vec![1.0, 2.0]), Some(vec![4.0, 5.0]), None];
        let expected_v16 = [Some(vec![1.0, 2.0]), None, Some(vec![3.0, 4.0])];

        let mut row_index = 0;
        if let Some(resultset) = client.get_current_resultset() {
            while let Some(row) = resultset.next_row().await.unwrap() {
                assert_eq!(row.len(), 3);

                // Check Id
                match &row[0] {
                    ColumnValues::Int(id) => {
                        assert_eq!(*id, (row_index + 1) as i32);
                    }
                    _ => panic!("Expected Int for Id"),
                }

                // Check Vec32
                match (&row[1], &expected_v32[row_index]) {
                    (ColumnValues::Vector(vector), Some(expected)) => {
                        assert_eq!(vector.dimension_count(), expected.len() as u16);
                        assert_eq!(vector.base_type(), VectorBaseType::Float32);
                        let values = vector.as_f32().unwrap();
                        for (i, &expected_val) in expected.iter().enumerate() {
                            assert!((values[i] - expected_val).abs() < 0.001);
                        }
                    }
                    (ColumnValues::Null, None) => {}
                    (actual, expected) => panic!(
                        "Mismatch Vec32 at row {}: expected {:?}, got {:?}",
                        row_index, expected, actual
                    ),
                }

                // Check Vec16
                match (&row[2], &expected_v16[row_index]) {
                    (ColumnValues::Vector(vector), Some(expected)) => {
                        assert_eq!(vector.dimension_count(), expected.len() as u16);
                        assert_eq!(vector.base_type(), VectorBaseType::Float16);
                        let values = vector.as_f32().unwrap();
                        for (i, &expected_val) in expected.iter().enumerate() {
                            assert!((values[i] - expected_val).abs() < 0.001);
                        }
                    }
                    (ColumnValues::Null, None) => {}
                    (actual, expected) => panic!(
                        "Mismatch Vec16 at row {}: expected {:?}, got {:?}",
                        row_index, expected, actual
                    ),
                }

                row_index += 1;
            }
        }
        assert_eq!(row_index, 3, "Expected 3 rows");
    }

    /// Test vector with very small float values (near zero)
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_vector_small_values() {
        let mut client = begin_connection(&build_tcp_datasource()).await;

        let query = "SELECT CAST('[0.0001, 0.0002, 0.0003]' AS VECTOR(3)) AS SmallVector";

        client.execute(query.to_string(), None, None).await.unwrap();

        if let Some(resultset) = client.get_current_resultset() {
            let mut row_count = 0;
            while let Some(row) = resultset.next_row().await.unwrap() {
                row_count += 1;
                match &row[0] {
                    ColumnValues::Vector(vector) => {
                        assert_eq!(vector.dimension_count(), 3);
                        assert_eq!(vector.base_type(), VectorBaseType::Float32);
                        let values = vector.as_f32().unwrap();
                        assert!((values[0] - 0.0001).abs() < 0.00001);
                        assert!((values[1] - 0.0002).abs() < 0.00001);
                        assert!((values[2] - 0.0003).abs() < 0.00001);
                    }
                    _ => panic!("Expected Vector column value"),
                }
            }
            assert_eq!(row_count, 1, "Expected exactly 1 row");
        }
    }

    /// Test vector with large float values
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_vector_large_values() {
        let mut client = begin_connection(&build_tcp_datasource()).await;

        let query = "SELECT CAST('[123456.79, -987654.3, 0.0]' AS VECTOR(3)) AS LargeVector";

        client.execute(query.to_string(), None, None).await.unwrap();

        if let Some(resultset) = client.get_current_resultset() {
            let mut row_count = 0;
            while let Some(row) = resultset.next_row().await.unwrap() {
                row_count += 1;
                match &row[0] {
                    ColumnValues::Vector(vector) => {
                        assert_eq!(vector.dimension_count(), 3);
                        assert_eq!(vector.base_type(), VectorBaseType::Float32);
                        let values = vector.as_f32().unwrap();
                        assert!((values[0] - 123_456.79).abs() < 0.01);
                        assert!((values[1] - (-987_654.3)).abs() < 0.01);
                        assert!((values[2] - 0.0).abs() < 0.001);
                    }
                    _ => panic!("Expected Vector column value"),
                }
            }
            assert_eq!(row_count, 1, "Expected exactly 1 row");
        }
    }

    /// Test sending Vector as a query parameter
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_vector_parameter_basic() {
        use mssql_tds::datatypes::sql_vector::SqlVector;
        use mssql_tds::datatypes::sqltypes::SqlType;
        use mssql_tds::message::parameters::rpc_parameters::{RpcParameter, StatusFlags};

        let mut client = begin_connection(&build_tcp_datasource()).await;

        // Create a vector to send as parameter
        let values = vec![1.0f32, 2.0f32, 3.0f32];
        let vector = SqlVector::try_from_f32(values).unwrap();
        let param = RpcParameter::new(
            Some("@p1".to_string()),
            StatusFlags::NONE,
            SqlType::Vector(Some(vector), 3, VectorBaseType::Float32),
        );

        // Use the parameter in a query
        let query = "SELECT @p1 AS ReturnedVector";
        let params = vec![param];

        client
            .execute_sp_executesql(query.to_string(), params, None, None)
            .await
            .unwrap();

        if let Some(resultset) = client.get_current_resultset() {
            let mut row_count = 0;
            while let Some(row) = resultset.next_row().await.unwrap() {
                row_count += 1;
                match &row[0] {
                    ColumnValues::Vector(returned_vector) => {
                        assert_eq!(returned_vector.dimension_count(), 3);
                        assert_eq!(returned_vector.base_type(), VectorBaseType::Float32);
                        let returned_values = returned_vector.as_f32().unwrap();
                        assert!((returned_values[0] - 1.0).abs() < 0.001);
                        assert!((returned_values[1] - 2.0).abs() < 0.001);
                        assert!((returned_values[2] - 3.0).abs() < 0.001);
                    }
                    _ => panic!("Expected Vector column value, got: {:?}", row[0]),
                }
            }
            assert_eq!(row_count, 1, "Expected exactly 1 row");
        } else {
            panic!("Expected a result set");
        }
    }

    /// Test NULL Vector parameter
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_vector_parameter_null() {
        use mssql_tds::datatypes::sqltypes::SqlType;
        use mssql_tds::message::parameters::rpc_parameters::{RpcParameter, StatusFlags};

        let mut client = begin_connection(&build_tcp_datasource()).await;

        // Create NULL vector parameter
        let param = RpcParameter::new(
            Some("@p1".to_string()),
            StatusFlags::NONE,
            SqlType::Vector(None, 10, VectorBaseType::Float32),
        );

        let query = "SELECT @p1 AS NullVector, CASE WHEN @p1 IS NULL THEN 1 ELSE 0 END AS IsNull";
        let params = vec![param];

        client
            .execute_sp_executesql(query.to_string(), params, None, None)
            .await
            .unwrap();

        if let Some(resultset) = client.get_current_resultset() {
            let mut row_count = 0;
            while let Some(row) = resultset.next_row().await.unwrap() {
                row_count += 1;
                assert_eq!(row.len(), 2);

                // First column should be NULL
                match &row[0] {
                    ColumnValues::Null => {}
                    _ => panic!("Expected NULL, got: {:?}", row[0]),
                }

                // Second column should be 1 (indicating NULL)
                match &row[1] {
                    ColumnValues::Int(val) => {
                        assert_eq!(*val, 1, "Expected IsNull to be 1");
                    }
                    _ => panic!("Expected Int(1), got: {:?}", row[1]),
                }
            }
            assert_eq!(row_count, 1, "Expected exactly 1 row");
        } else {
            panic!("Expected a result set");
        }
    }

    /// Test Vector parameter in WHERE clause
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_vector_parameter_in_where_clause() {
        use mssql_tds::datatypes::sql_vector::SqlVector;
        use mssql_tds::datatypes::sqltypes::SqlType;
        use mssql_tds::message::parameters::rpc_parameters::{RpcParameter, StatusFlags};

        let mut client = begin_connection(&build_tcp_datasource()).await;

        // Create a temporary table with vectors
        let create_table = "
            CREATE TABLE #VectorTest (
                id INT,
                vec VECTOR(3)
            );
            INSERT INTO #VectorTest VALUES (1, CAST('[1.0, 2.0, 3.0]' AS VECTOR(3)));
            INSERT INTO #VectorTest VALUES (2, CAST('[4.0, 5.0, 6.0]' AS VECTOR(3)));
            INSERT INTO #VectorTest VALUES (3, CAST('[1.0, 2.0, 3.0]' AS VECTOR(3)));
        ";

        client
            .execute(create_table.to_string(), None, None)
            .await
            .unwrap();

        // Consume any result sets from the setup
        while client.get_current_resultset().is_some() {
            client.move_to_next().await.unwrap();
        }

        // Now query with Vector parameter
        let values = vec![1.0f32, 2.0f32, 3.0f32];
        let vector = SqlVector::try_from_f32(values).unwrap();
        let param = RpcParameter::new(
            Some("@p1".to_string()),
            StatusFlags::NONE,
            SqlType::Vector(Some(vector), 3, VectorBaseType::Float32),
        );

        // Use a small epsilon to account for floating-point precision in distance computation
        let query = "SELECT id FROM #VectorTest WHERE VECTOR_DISTANCE('cosine', vec, @p1) < 1e-6";
        let params = vec![param];

        client
            .execute_sp_executesql(query.to_string(), params, None, None)
            .await
            .unwrap();

        if let Some(resultset) = client.get_current_resultset() {
            let mut row_count = 0;
            let mut found_ids = vec![];

            while let Some(row) = resultset.next_row().await.unwrap() {
                row_count += 1;
                match &row[0] {
                    ColumnValues::Int(id) => {
                        found_ids.push(*id);
                    }
                    _ => panic!("Expected Int column, got: {:?}", row[0]),
                }
            }

            // Should find rows 1 and 3 (both have [1.0, 2.0, 3.0]) within epsilon
            assert_eq!(
                row_count, 2,
                "Expected 2 matching rows within epsilon tolerance"
            );
            assert!(found_ids.contains(&1), "Expected to find id 1");
            assert!(found_ids.contains(&3), "Expected to find id 3");
        } else {
            panic!("Expected a result set");
        }
    }

    /// Test Vector parameter with large dimensions
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_vector_parameter_large_dimensions() {
        use mssql_tds::datatypes::sql_vector::SqlVector;
        use mssql_tds::datatypes::sqltypes::SqlType;
        use mssql_tds::message::parameters::rpc_parameters::{RpcParameter, StatusFlags};

        let mut client = begin_connection(&build_tcp_datasource()).await;

        // Create a 100-dimensional vector
        let values: Vec<f32> = (0..100).map(|i| i as f32 * 0.5).collect();
        let vector = SqlVector::try_from_f32(values.clone()).unwrap();
        let param = RpcParameter::new(
            Some("@p1".to_string()),
            StatusFlags::NONE,
            SqlType::Vector(Some(vector), 100, VectorBaseType::Float32),
        );

        let query = "SELECT @p1 AS LargeVector";
        let params = vec![param];

        client
            .execute_sp_executesql(query.to_string(), params, None, None)
            .await
            .unwrap();

        if let Some(resultset) = client.get_current_resultset() {
            let mut row_count = 0;
            while let Some(row) = resultset.next_row().await.unwrap() {
                row_count += 1;
                match &row[0] {
                    ColumnValues::Vector(returned_vector) => {
                        assert_eq!(returned_vector.dimension_count(), 100);
                        assert_eq!(returned_vector.base_type(), VectorBaseType::Float32);
                        let returned_values = returned_vector.as_f32().unwrap();
                        for (i, &value) in returned_values.iter().enumerate() {
                            assert!((value - (i as f32 * 0.5)).abs() < 0.001);
                        }
                    }
                    _ => panic!("Expected Vector column value"),
                }
            }
            assert_eq!(row_count, 1, "Expected exactly 1 row");
        } else {
            panic!("Expected a result set");
        }
    }

    /// Test multiple Vector parameters
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_multiple_vector_parameters() {
        use mssql_tds::datatypes::sql_vector::SqlVector;
        use mssql_tds::datatypes::sqltypes::SqlType;
        use mssql_tds::message::parameters::rpc_parameters::{RpcParameter, StatusFlags};

        let mut client = begin_connection(&build_tcp_datasource()).await;

        // Create two vectors
        let vec1 = SqlVector::try_from_f32(vec![1.0f32, 2.0f32, 3.0f32]).unwrap();
        let vec2 = SqlVector::try_from_f32(vec![4.0f32, 5.0f32, 6.0f32]).unwrap();

        let params = vec![
            RpcParameter::new(
                Some("@p1".to_string()),
                StatusFlags::NONE,
                SqlType::Vector(Some(vec1), 3, VectorBaseType::Float32),
            ),
            RpcParameter::new(
                Some("@p2".to_string()),
                StatusFlags::NONE,
                SqlType::Vector(Some(vec2), 3, VectorBaseType::Float32),
            ),
        ];

        let query = "SELECT @p1 AS Vec1, @p2 AS Vec2";

        client
            .execute_sp_executesql(query.to_string(), params, None, None)
            .await
            .unwrap();

        if let Some(resultset) = client.get_current_resultset() {
            let mut row_count = 0;
            while let Some(row) = resultset.next_row().await.unwrap() {
                row_count += 1;
                assert_eq!(row.len(), 2);

                // Verify first vector
                match &row[0] {
                    ColumnValues::Vector(v) => {
                        assert_eq!(v.dimension_count(), 3);
                        assert_eq!(v.base_type(), VectorBaseType::Float32);
                        let vals = v.as_f32().unwrap();
                        assert_eq!(vals, &[1.0, 2.0, 3.0]);
                    }
                    _ => panic!("Expected Vector for first column"),
                }

                // Verify second vector
                match &row[1] {
                    ColumnValues::Vector(v) => {
                        assert_eq!(v.dimension_count(), 3);
                        assert_eq!(v.base_type(), VectorBaseType::Float32);
                        let vals = v.as_f32().unwrap();
                        assert_eq!(vals, &[4.0, 5.0, 6.0]);
                    }
                    _ => panic!("Expected Vector for second column"),
                }
            }
            assert_eq!(row_count, 1, "Expected exactly 1 row");
        } else {
            panic!("Expected a result set");
        }
    }

    /// Test Vector as output parameter from stored procedure
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_vector_output_parameter() {
        use mssql_tds::datatypes::sqltypes::SqlType;
        use mssql_tds::message::parameters::rpc_parameters::{RpcParameter, StatusFlags};

        let mut client = begin_connection(&build_tcp_datasource()).await;

        // Create a stored procedure that returns a vector as output parameter
        let create_proc = "
            CREATE PROCEDURE #TestVectorOutput
                @OutputVector VECTOR(3) OUTPUT
            AS
            BEGIN
                SET @OutputVector = CAST('[7.0, 8.0, 9.0]' AS VECTOR(3));
            END
        ";

        client
            .execute(create_proc.to_string(), None, None)
            .await
            .unwrap();

        // Consume result sets from CREATE PROCEDURE
        while client.get_current_resultset().is_some() {
            client.move_to_next().await.unwrap();
        }

        // Call the procedure with output parameter
        let output_param = RpcParameter::new(
            Some("@OutputVector".to_string()),
            StatusFlags::BY_REF_VALUE, // Mark as output parameter
            SqlType::Vector(None, 3, VectorBaseType::Float32),
        );

        client
            .execute_stored_procedure(
                "#TestVectorOutput".to_string(),
                None,
                Some(vec![output_param]),
                None,
                None,
            )
            .await
            .unwrap();

        // Get output parameter value
        let output_params = client.retrieve_output_params().unwrap().unwrap();
        assert_eq!(output_params.len(), 1, "Expected 1 output parameter");

        match &output_params[0].value {
            ColumnValues::Vector(vector) => {
                assert_eq!(vector.dimension_count(), 3);
                let values = vector.as_f32().unwrap();
                assert!((values[0] - 7.0).abs() < 0.001);
                assert!((values[1] - 8.0).abs() < 0.001);
                assert!((values[2] - 9.0).abs() < 0.001);
            }
            _ => panic!(
                "Expected Vector output parameter, got: {:?}",
                output_params[0].value
            ),
        }
    }

    /// Test Vector as both input and output parameter
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_vector_input_output_parameter() {
        use mssql_tds::datatypes::sql_vector::SqlVector;
        use mssql_tds::datatypes::sqltypes::SqlType;
        use mssql_tds::message::parameters::rpc_parameters::{RpcParameter, StatusFlags};

        let mut client = begin_connection(&build_tcp_datasource()).await;

        // Create a stored procedure that takes vector input and returns modified vector as output
        let create_proc = "
            CREATE PROCEDURE #TestVectorInOut
                @InputVector VECTOR(3),
                @OutputVector VECTOR(3) OUTPUT
            AS
            BEGIN
                -- Just return the input vector as output for this test
                SET @OutputVector = @InputVector;
            END
        ";

        client
            .execute(create_proc.to_string(), None, None)
            .await
            .unwrap();

        // Consume result sets from CREATE PROCEDURE
        while client.get_current_resultset().is_some() {
            client.move_to_next().await.unwrap();
        }

        // Prepare input and output parameters
        let input_vector = SqlVector::try_from_f32(vec![10.5f32, 20.5f32, 30.5f32]).unwrap();
        let params = vec![
            RpcParameter::new(
                Some("@InputVector".to_string()),
                StatusFlags::NONE,
                SqlType::Vector(Some(input_vector), 3, VectorBaseType::Float32),
            ),
            RpcParameter::new(
                Some("@OutputVector".to_string()),
                StatusFlags::BY_REF_VALUE,
                SqlType::Vector(None, 3, VectorBaseType::Float32),
            ),
        ];

        client
            .execute_stored_procedure(
                "#TestVectorInOut".to_string(),
                None,
                Some(params),
                None,
                None,
            )
            .await
            .unwrap();

        // Get output parameter value
        let output_params = client.retrieve_output_params().unwrap().unwrap();
        assert_eq!(output_params.len(), 1, "Expected 1 output parameter");

        match &output_params[0].value {
            ColumnValues::Vector(vector) => {
                assert_eq!(vector.dimension_count(), 3);
                let values = vector.as_f32().unwrap();
                assert!((values[0] - 10.5).abs() < 0.001);
                assert!((values[1] - 20.5).abs() < 0.001);
                assert!((values[2] - 30.5).abs() < 0.001);
            }
            _ => panic!(
                "Expected Vector output parameter, got: {:?}",
                output_params[0].value
            ),
        }
    }

    /// Test NULL Vector as output parameter
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_vector_output_parameter_null() {
        use mssql_tds::datatypes::sqltypes::SqlType;
        use mssql_tds::message::parameters::rpc_parameters::{RpcParameter, StatusFlags};

        let mut client = begin_connection(&build_tcp_datasource()).await;

        // Create a stored procedure that returns NULL vector as output
        let create_proc = "
            CREATE PROCEDURE #TestVectorOutputNull
                @OutputVector VECTOR(5) OUTPUT
            AS
            BEGIN
                SET @OutputVector = NULL;
            END
        ";

        client
            .execute(create_proc.to_string(), None, None)
            .await
            .unwrap();

        // Consume result sets from CREATE PROCEDURE
        while client.get_current_resultset().is_some() {
            client.move_to_next().await.unwrap();
        }

        // Call the procedure with output parameter
        let output_param = RpcParameter::new(
            Some("@OutputVector".to_string()),
            StatusFlags::BY_REF_VALUE,
            SqlType::Vector(None, 5, VectorBaseType::Float32),
        );

        client
            .execute_stored_procedure(
                "#TestVectorOutputNull".to_string(),
                None,
                Some(vec![output_param]),
                None,
                None,
            )
            .await
            .unwrap();

        // Get output parameter value - should be NULL
        let output_params = client.retrieve_output_params().unwrap().unwrap();
        assert_eq!(output_params.len(), 1, "Expected 1 output parameter");

        match &output_params[0].value {
            ColumnValues::Null => {
                // Expected - NULL vector output
            }
            _ => panic!(
                "Expected NULL output parameter, got: {:?}",
                output_params[0].value
            ),
        }
    }

    /// Test Vector output parameter with large dimensions
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_vector_output_parameter_large_dimensions() {
        use mssql_tds::datatypes::sqltypes::SqlType;
        use mssql_tds::message::parameters::rpc_parameters::{RpcParameter, StatusFlags};

        let mut client = begin_connection(&build_tcp_datasource()).await;

        // Create a stored procedure that returns a large-dimensional vector
        let vector_literal = (0..100)
            .map(|i| format!("{}.0", i))
            .collect::<Vec<_>>()
            .join(", ");
        let create_proc = format!(
            "
            CREATE PROCEDURE #TestVectorOutputLarge
                @OutputVector VECTOR(100) OUTPUT
            AS
            BEGIN
                SET @OutputVector = CAST('[{}]' AS VECTOR(100));
            END
            ",
            vector_literal
        );

        client.execute(create_proc, None, None).await.unwrap();

        // Consume result sets from CREATE PROCEDURE
        while client.get_current_resultset().is_some() {
            client.move_to_next().await.unwrap();
        }

        // Call the procedure with output parameter
        let output_param = RpcParameter::new(
            Some("@OutputVector".to_string()),
            StatusFlags::BY_REF_VALUE,
            SqlType::Vector(None, 100, VectorBaseType::Float32),
        );

        client
            .execute_stored_procedure(
                "#TestVectorOutputLarge".to_string(),
                None,
                Some(vec![output_param]),
                None,
                None,
            )
            .await
            .unwrap();

        // Get output parameter value
        let output_params = client.retrieve_output_params().unwrap().unwrap();
        assert_eq!(output_params.len(), 1, "Expected 1 output parameter");

        match &output_params[0].value {
            ColumnValues::Vector(vector) => {
                assert_eq!(vector.dimension_count(), 100);
                let values = vector.as_f32().unwrap();

                // Spot check some values
                assert!((values[0] - 0.0).abs() < 0.001);
                assert!((values[50] - 50.0).abs() < 0.001);
                assert!((values[99] - 99.0).abs() < 0.001);
            }
            _ => panic!("Expected Vector output parameter"),
        }
    }
}
