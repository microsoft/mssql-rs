// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#[cfg(test)]
mod common;

mod bulk_copy_vector_tests {
    #[cfg(windows)]
    use crate::common::build_named_pipe_datasource;
    use crate::common::{begin_connection, build_tcp_datasource, init_tracing};
    use async_trait::async_trait;
    use mssql_tds::connection::bulk_copy::{BulkCopy, BulkLoadRow};
    use mssql_tds::connection::tds_client::ResultSet;
    use mssql_tds::core::TdsResult;
    use mssql_tds::datatypes::column_values::ColumnValues;
    use mssql_tds::datatypes::sql_vector::SqlVector;
    use mssql_tds::datatypes::sqldatatypes::VectorBaseType;

    #[ctor::ctor]
    fn init() {
        init_tracing();
    }

    #[derive(Debug, Clone)]
    struct VectorRow {
        id: i32,
        vector_col: Option<Vec<f32>>,
    }

    #[async_trait]
    impl BulkLoadRow for VectorRow {
        async fn write_to_packet(
            &self,
            writer: &mut mssql_tds::message::bulk_load::StreamingBulkLoadWriter<'_>,
            column_index: &mut usize,
        ) -> TdsResult<()> {
            writer
                .write_column_value(*column_index, &ColumnValues::Int(self.id))
                .await?;
            *column_index += 1;
            let vector_val = if let Some(vec_data) = &self.vector_col {
                ColumnValues::Vector(SqlVector::try_from_f32(vec_data.clone())?)
            } else {
                ColumnValues::Null
            };
            writer
                .write_column_value(*column_index, &vector_val)
                .await?;
            *column_index += 1;
            Ok(())
        }
    }

    #[async_trait]
    impl BulkLoadRow for &VectorRow {
        async fn write_to_packet(
            &self,
            writer: &mut mssql_tds::message::bulk_load::StreamingBulkLoadWriter<'_>,
            column_index: &mut usize,
        ) -> TdsResult<()> {
            writer
                .write_column_value(*column_index, &ColumnValues::Int(self.id))
                .await?;
            *column_index += 1;
            let vector_val = if let Some(vec_data) = &self.vector_col {
                ColumnValues::Vector(SqlVector::try_from_f32(vec_data.clone())?)
            } else {
                ColumnValues::Null
            };
            writer
                .write_column_value(*column_index, &vector_val)
                .await?;
            *column_index += 1;
            Ok(())
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_bulk_copy_vector_basic() {
        let mut client = begin_connection(&build_tcp_datasource()).await;

        // Create temp table with VECTOR(3) column
        client
            .execute(
                "CREATE TABLE #BulkCopyVectorTest (id INT NOT NULL, vector_col VECTOR(3) NULL)"
                    .to_string(),
                (),
            )
            .await
            .unwrap();

        let test_vec1 = vec![1.0, 2.0, 3.0];
        let test_vec2 = vec![4.0, 5.0, 6.0];
        let test_vec3 = vec![0.0, 0.0, 0.0]; // Zero vector
        let test_vec4 = vec![-1.5, 2.5, -3.5]; // Negative values

        let rows = vec![
            VectorRow {
                id: 1,
                vector_col: Some(test_vec1.clone()),
            },
            VectorRow {
                id: 2,
                vector_col: Some(test_vec2.clone()),
            },
            VectorRow {
                id: 3,
                vector_col: None, // NULL
            },
            VectorRow {
                id: 4,
                vector_col: Some(test_vec3.clone()),
            },
            VectorRow {
                id: 5,
                vector_col: Some(test_vec4.clone()),
            },
        ];

        // Execute bulk copy
        let result = {
            let bulk_copy = BulkCopy::new(&mut client, "#BulkCopyVectorTest");
            bulk_copy
                .batch_size(1000)
                .write_to_server_zerocopy(&rows)
                .await
                .expect("Bulk copy failed")
        };
        assert_eq!(result.rows_affected, 5, "Expected 5 rows to be inserted");

        // Verify the data
        client
            .execute(
                "SELECT id, vector_col FROM #BulkCopyVectorTest ORDER BY id".to_string(),
                (),
            )
            .await
            .expect("Failed to select data");

        let mut row_count = 0;
        if client.on_rows() {
            while let Some(row) = client.next_row().await.expect("Failed to read row") {
                row_count += 1;
                match row_count {
                    1 => {
                        assert_eq!(row[0], ColumnValues::Int(1));
                        if let ColumnValues::Vector(vec) = &row[1] {
                            let vals = vec.as_f32().expect("Expected f32 vector");
                            assert_eq!(vals, test_vec1.as_slice());
                        } else {
                            panic!("Expected Vector, got {:?}", row[1]);
                        }
                    }
                    2 => {
                        assert_eq!(row[0], ColumnValues::Int(2));
                        if let ColumnValues::Vector(vec) = &row[1] {
                            let vals = vec.as_f32().expect("Expected f32 vector");
                            assert_eq!(vals, test_vec2.as_slice());
                        } else {
                            panic!("Expected Vector, got {:?}", row[1]);
                        }
                    }
                    3 => {
                        assert_eq!(row[0], ColumnValues::Int(3));
                        assert_eq!(row[1], ColumnValues::Null);
                    }
                    4 => {
                        assert_eq!(row[0], ColumnValues::Int(4));
                        if let ColumnValues::Vector(vec) = &row[1] {
                            let vals = vec.as_f32().expect("Expected f32 vector");
                            assert_eq!(vals, test_vec3.as_slice());
                        } else {
                            panic!("Expected Vector, got {:?}", row[1]);
                        }
                    }
                    5 => {
                        assert_eq!(row[0], ColumnValues::Int(5));
                        if let ColumnValues::Vector(vec) = &row[1] {
                            let vals = vec.as_f32().expect("Expected f32 vector");
                            assert_eq!(vals, test_vec4.as_slice());
                        } else {
                            panic!("Expected Vector, got {:?}", row[1]);
                        }
                    }
                    _ => panic!("Unexpected row count: {}", row_count),
                }
            }
        }
        assert_eq!(row_count, 5, "Expected 5 rows in result set");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_bulk_copy_multiple_vector_columns() {
        let mut client = begin_connection(&build_tcp_datasource()).await;

        // Create temp table with multiple VECTOR columns
        client
            .execute("CREATE TABLE #BulkCopyMultiVectorTest (id INT NOT NULL, vec1 VECTOR(2), vec2 VECTOR(3), vec3 VECTOR(4) NULL)"
                    .to_string(), ())
            .await
            .unwrap();

        #[derive(Debug, Clone)]
        struct MultiVectorRow {
            id: i32,
            vec1: Vec<f32>,
            vec2: Vec<f32>,
            vec3: Option<Vec<f32>>,
        }

        #[async_trait]
        impl BulkLoadRow for MultiVectorRow {
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
                    .write_column_value(
                        *column_index,
                        &ColumnValues::Vector(SqlVector::try_from_f32(self.vec1.clone())?),
                    )
                    .await?;
                *column_index += 1;
                writer
                    .write_column_value(
                        *column_index,
                        &ColumnValues::Vector(SqlVector::try_from_f32(self.vec2.clone())?),
                    )
                    .await?;
                *column_index += 1;
                let vec3_val = if let Some(vec_data) = &self.vec3 {
                    ColumnValues::Vector(SqlVector::try_from_f32(vec_data.clone())?)
                } else {
                    ColumnValues::Null
                };
                writer.write_column_value(*column_index, &vec3_val).await?;
                *column_index += 1;
                Ok(())
            }
        }

        #[async_trait]
        impl BulkLoadRow for &MultiVectorRow {
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
                    .write_column_value(
                        *column_index,
                        &ColumnValues::Vector(SqlVector::try_from_f32(self.vec1.clone())?),
                    )
                    .await?;
                *column_index += 1;
                writer
                    .write_column_value(
                        *column_index,
                        &ColumnValues::Vector(SqlVector::try_from_f32(self.vec2.clone())?),
                    )
                    .await?;
                *column_index += 1;
                let vec3_val = if let Some(vec_data) = &self.vec3 {
                    ColumnValues::Vector(SqlVector::try_from_f32(vec_data.clone())?)
                } else {
                    ColumnValues::Null
                };
                writer.write_column_value(*column_index, &vec3_val).await?;
                *column_index += 1;
                Ok(())
            }
        }

        let test_vec1_row1 = vec![1.0, 2.0];
        let test_vec2_row1 = vec![3.0, 4.0, 5.0];
        let test_vec3_row1 = vec![6.0, 7.0, 8.0, 9.0];
        let test_vec1_row2 = vec![10.0, 11.0];
        let test_vec2_row2 = vec![12.0, 13.0, 14.0];

        let rows = vec![
            MultiVectorRow {
                id: 1,
                vec1: test_vec1_row1.clone(),
                vec2: test_vec2_row1.clone(),
                vec3: Some(test_vec3_row1.clone()),
            },
            MultiVectorRow {
                id: 2,
                vec1: test_vec1_row2.clone(),
                vec2: test_vec2_row2.clone(),
                vec3: None,
            },
        ];

        // Execute bulk copy
        let result = {
            let bulk_copy = BulkCopy::new(&mut client, "#BulkCopyMultiVectorTest");
            bulk_copy
                .batch_size(1000)
                .write_to_server_zerocopy(&rows)
                .await
                .expect("Bulk copy failed")
        };
        assert_eq!(result.rows_affected, 2, "Expected 2 rows to be inserted");

        // Verify the data
        client
            .execute(
                "SELECT id, vec1, vec2, vec3 FROM #BulkCopyMultiVectorTest ORDER BY id".to_string(),
                (),
            )
            .await
            .expect("Failed to select data");

        let mut row_count = 0;
        if client.on_rows() {
            while let Some(row) = client.next_row().await.expect("Failed to read row") {
                row_count += 1;
                match row_count {
                    1 => {
                        assert_eq!(row[0], ColumnValues::Int(1));
                        if let ColumnValues::Vector(vec) = &row[1] {
                            assert_eq!(vec.as_f32().unwrap(), test_vec1_row1.as_slice());
                        } else {
                            panic!("Expected Vector for vec1");
                        }
                        if let ColumnValues::Vector(vec) = &row[2] {
                            assert_eq!(vec.as_f32().unwrap(), test_vec2_row1.as_slice());
                        } else {
                            panic!("Expected Vector for vec2");
                        }
                        if let ColumnValues::Vector(vec) = &row[3] {
                            assert_eq!(vec.as_f32().unwrap(), test_vec3_row1.as_slice());
                        } else {
                            panic!("Expected Vector for vec3");
                        }
                    }
                    2 => {
                        assert_eq!(row[0], ColumnValues::Int(2));
                        if let ColumnValues::Vector(vec) = &row[1] {
                            assert_eq!(vec.as_f32().unwrap(), test_vec1_row2.as_slice());
                        } else {
                            panic!("Expected Vector for vec1");
                        }
                        if let ColumnValues::Vector(vec) = &row[2] {
                            assert_eq!(vec.as_f32().unwrap(), test_vec2_row2.as_slice());
                        } else {
                            panic!("Expected Vector for vec2");
                        }
                        assert_eq!(row[3], ColumnValues::Null);
                    }
                    _ => panic!("Unexpected row count: {}", row_count),
                }
            }
        }
        assert_eq!(row_count, 2, "Expected 2 rows in result set");
    }

    /// Helper function to test bulk copy with large vectors (1998 dimensions).
    /// This tests multi-packet TDS responses since each vector is ~8KB.
    async fn bulk_copy_vector_large_dimensions_impl(datasource: &str, table_name: &str) {
        let mut client = begin_connection(datasource).await;

        // Create temp table with VECTOR(1998) - maximum supported dimensions
        client
            .execute(
                format!(
                    "CREATE TABLE {} (id INT NOT NULL, embedding VECTOR(1998))",
                    table_name
                ),
                (),
            )
            .await
            .unwrap();

        // Generate 1998-dimensional vectors (~8KB each, spans multiple TDS packets)
        let vec1: Vec<f32> = (0..1998).map(|i| i as f32 * 0.001).collect();
        let vec2: Vec<f32> = (0..1998).map(|i| (1998 - i) as f32 * 0.001).collect();

        let rows = vec![
            VectorRow {
                id: 1,
                vector_col: Some(vec1.clone()),
            },
            VectorRow {
                id: 2,
                vector_col: Some(vec2.clone()),
            },
        ];

        // Execute bulk copy
        let result = {
            let bulk_copy = BulkCopy::new(&mut client, table_name);
            bulk_copy
                .batch_size(1000)
                .write_to_server_zerocopy(&rows)
                .await
                .expect("Bulk copy failed")
        };
        assert_eq!(result.rows_affected, 2, "Expected 2 rows to be inserted");

        // Verify the data
        client
            .execute(
                format!("SELECT id, embedding FROM {} ORDER BY id", table_name),
                (),
            )
            .await
            .expect("Failed to select data");

        let mut row_count = 0;
        if client.on_rows() {
            while let Some(row) = client.next_row().await.expect("Failed to read row") {
                row_count += 1;
                match row_count {
                    1 => {
                        assert_eq!(row[0], ColumnValues::Int(1));
                        if let ColumnValues::Vector(vec) = &row[1] {
                            let vals = vec.as_f32().expect("Expected f32 vector");
                            assert_eq!(vals.len(), 1998);
                            // Spot check a few values
                            assert!((vals[0] - 0.0).abs() < 1e-6);
                            assert!((vals[100] - 0.1).abs() < 1e-6);
                            assert!((vals[1997] - 1.997).abs() < 1e-6);
                        } else {
                            panic!("Expected Vector, got {:?}", row[1]);
                        }
                    }
                    2 => {
                        assert_eq!(row[0], ColumnValues::Int(2));
                        if let ColumnValues::Vector(vec) = &row[1] {
                            let vals = vec.as_f32().expect("Expected f32 vector");
                            assert_eq!(vals.len(), 1998);
                            // Spot check a few values
                            assert!((vals[0] - 1.998).abs() < 1e-6);
                            assert!((vals[1997] - 0.001).abs() < 1e-6);
                        } else {
                            panic!("Expected Vector, got {:?}", row[1]);
                        }
                    }
                    _ => panic!("Unexpected row count: {}", row_count),
                }
            }
        }
        assert_eq!(row_count, 2, "Expected 2 rows in result set");
    }

    /// Test bulk copy with large vectors (1998 dimensions) over TCP.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_bulk_copy_vector_large_dimensions_tcp() {
        bulk_copy_vector_large_dimensions_impl(&build_tcp_datasource(), "#BulkCopyLargeVectorTest")
            .await;
    }

    /// Test bulk copy with large vectors (1998 dimensions) over Named Pipes.
    /// Named Pipes have different read semantics (message mode) that can cause
    /// issues with multi-packet reads if not handled correctly.
    #[cfg(windows)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_bulk_copy_vector_large_dimensions_named_pipe() {
        bulk_copy_vector_large_dimensions_impl(
            &build_named_pipe_datasource(),
            "#BulkCopyLargeVectorNPTest",
        )
        .await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_bulk_copy_vector_dimension_mismatch() {
        let mut client = begin_connection(&build_tcp_datasource()).await;

        // Create temp table with VECTOR(3) column
        client
            .execute(
                "CREATE TABLE #BulkCopyVectorMismatchTest (id INT NOT NULL, vector_col VECTOR(3))"
                    .to_string(),
                (),
            )
            .await
            .unwrap();

        // Try to insert vector with wrong dimensions (2 instead of 3)
        let rows_too_short = vec![VectorRow {
            id: 1,
            vector_col: Some(vec![1.0, 2.0]), // Only 2 dimensions, table expects 3
        }];

        let result = {
            let bulk_copy = BulkCopy::new(&mut client, "#BulkCopyVectorMismatchTest");
            bulk_copy
                .batch_size(1000)
                .write_to_server_zerocopy(&rows_too_short)
                .await
        };

        assert!(
            result.is_err(),
            "Expected bulk copy to fail with dimension mismatch (2 vs 3)"
        );

        // Try to insert vector with wrong dimensions (4 instead of 3)
        let rows_too_long = vec![VectorRow {
            id: 2,
            vector_col: Some(vec![1.0, 2.0, 3.0, 4.0]), // 4 dimensions, table expects 3
        }];

        let result = {
            let bulk_copy = BulkCopy::new(&mut client, "#BulkCopyVectorMismatchTest");
            bulk_copy
                .batch_size(1000)
                .write_to_server_zerocopy(&rows_too_long)
                .await
        };

        assert!(
            result.is_err(),
            "Expected bulk copy to fail with dimension mismatch (4 vs 3)"
        );
    }

    #[derive(Debug, Clone)]
    struct Vector16Row {
        id: i32,
        vector_col: Option<Vec<f32>>,
    }

    #[async_trait]
    impl BulkLoadRow for Vector16Row {
        async fn write_to_packet(
            &self,
            writer: &mut mssql_tds::message::bulk_load::StreamingBulkLoadWriter<'_>,
            column_index: &mut usize,
        ) -> TdsResult<()> {
            writer
                .write_column_value(*column_index, &ColumnValues::Int(self.id))
                .await?;
            *column_index += 1;
            let vector_val = if let Some(vec_data) = &self.vector_col {
                ColumnValues::Vector(SqlVector::try_from_f16(vec_data.clone())?)
            } else {
                ColumnValues::Null
            };
            writer
                .write_column_value(*column_index, &vector_val)
                .await?;
            *column_index += 1;
            Ok(())
        }
    }

    #[async_trait]
    impl BulkLoadRow for &Vector16Row {
        async fn write_to_packet(
            &self,
            writer: &mut mssql_tds::message::bulk_load::StreamingBulkLoadWriter<'_>,
            column_index: &mut usize,
        ) -> TdsResult<()> {
            writer
                .write_column_value(*column_index, &ColumnValues::Int(self.id))
                .await?;
            *column_index += 1;
            let vector_val = if let Some(vec_data) = &self.vector_col {
                ColumnValues::Vector(SqlVector::try_from_f16(vec_data.clone())?)
            } else {
                ColumnValues::Null
            };
            writer
                .write_column_value(*column_index, &vector_val)
                .await?;
            *column_index += 1;
            Ok(())
        }
    }

    /// Bulk copy into a `VECTOR(n, float16)` column.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_bulk_copy_vector16_basic() {
        let mut client = begin_connection(&build_tcp_datasource()).await;

        client
            .execute(
                "CREATE TABLE #BulkCopyVector16Test (id INT NOT NULL, vector_col VECTOR(3, float16) NULL)"
                    .to_string(),
                None,
                None,
            )
            .await
            .unwrap();

        // All values are exactly representable in IEEE 754 half-precision.
        let test_vec1 = vec![1.0, 2.0, 3.0];
        let test_vec2 = vec![0.0, 0.0, 0.0];
        let test_vec3 = vec![-1.5, 2.5, -3.5];

        let rows = vec![
            Vector16Row {
                id: 1,
                vector_col: Some(test_vec1.clone()),
            },
            Vector16Row {
                id: 2,
                vector_col: None,
            },
            Vector16Row {
                id: 3,
                vector_col: Some(test_vec2.clone()),
            },
            Vector16Row {
                id: 4,
                vector_col: Some(test_vec3.clone()),
            },
        ];

        let result = {
            let bulk_copy = BulkCopy::new(&mut client, "#BulkCopyVector16Test");
            bulk_copy
                .batch_size(1000)
                .write_to_server_zerocopy(&rows)
                .await
                .expect("Bulk copy failed")
        };
        assert_eq!(result.rows_affected, 4, "Expected 4 rows to be inserted");

        client
            .execute(
                "SELECT id, vector_col FROM #BulkCopyVector16Test ORDER BY id".to_string(),
                None,
                None,
            )
            .await
            .expect("Failed to select data");

        let mut row_count = 0;
        if let Some(resultset) = client.get_current_resultset() {
            while let Some(row) = resultset.next_row().await.expect("Failed to read row") {
                row_count += 1;
                let expected = match row_count {
                    1 => Some(&test_vec1),
                    2 => None,
                    3 => Some(&test_vec2),
                    4 => Some(&test_vec3),
                    _ => panic!("Unexpected row count: {}", row_count),
                };
                assert_eq!(row[0], ColumnValues::Int(row_count));
                match (&row[1], expected) {
                    (ColumnValues::Null, None) => {}
                    (ColumnValues::Vector(vec), Some(exp)) => {
                        assert_eq!(vec.base_type(), VectorBaseType::Float16);
                        assert_eq!(vec.as_f32().expect("Expected f32 values"), exp.as_slice());
                    }
                    _ => panic!("Unexpected value for row {}: {:?}", row_count, row[1]),
                }
            }
        }
        assert_eq!(row_count, 4, "Expected 4 rows in result set");
    }

    /// Bulk copy float16 vectors at the maximum dimension count (3996).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_bulk_copy_vector16_max_dimensions() {
        let mut client = begin_connection(&build_tcp_datasource()).await;

        let dims = VectorBaseType::Float16.max_dimensions();

        client
            .execute(
                format!(
                    "CREATE TABLE #BulkCopyVector16Large (id INT NOT NULL, embedding VECTOR({}, float16))",
                    dims
                ),
                None,
                None,
            )
            .await
            .unwrap();

        // Keep values within f16's exactly-representable integer range.
        let vec1: Vec<f32> = (0..dims).map(|i| (i % 2048) as f32).collect();
        let vec2: Vec<f32> = (0..dims).map(|i| -((i % 2048) as f32)).collect();

        let rows = vec![
            Vector16Row {
                id: 1,
                vector_col: Some(vec1.clone()),
            },
            Vector16Row {
                id: 2,
                vector_col: Some(vec2.clone()),
            },
        ];

        let result = {
            let bulk_copy = BulkCopy::new(&mut client, "#BulkCopyVector16Large");
            bulk_copy
                .batch_size(1000)
                .write_to_server_zerocopy(&rows)
                .await
                .expect("Bulk copy failed")
        };
        assert_eq!(result.rows_affected, 2, "Expected 2 rows to be inserted");

        client
            .execute(
                "SELECT id, embedding FROM #BulkCopyVector16Large ORDER BY id".to_string(),
                None,
                None,
            )
            .await
            .expect("Failed to select data");

        let mut row_count = 0;
        if let Some(resultset) = client.get_current_resultset() {
            while let Some(row) = resultset.next_row().await.expect("Failed to read row") {
                row_count += 1;
                let expected = match row_count {
                    1 => &vec1,
                    2 => &vec2,
                    _ => panic!("Unexpected row count: {}", row_count),
                };
                assert_eq!(row[0], ColumnValues::Int(row_count));
                if let ColumnValues::Vector(vec) = &row[1] {
                    assert_eq!(vec.base_type(), VectorBaseType::Float16);
                    assert_eq!(vec.dimension_count(), dims);
                    assert_eq!(
                        vec.as_f32().expect("Expected f32 values"),
                        expected.as_slice()
                    );
                } else {
                    panic!("Expected Vector, got {:?}", row[1]);
                }
            }
        }
        assert_eq!(row_count, 2, "Expected 2 rows in result set");
    }

    /// Bulk copy float16 values that are not exactly representable as IEEE 754
    /// halves, verifying the server round-trips the narrowed values.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_bulk_copy_vector16_precision_loss() {
        let mut client = begin_connection(&build_tcp_datasource()).await;

        client
            .execute(
                "CREATE TABLE #BulkCopyVector16Precision (id INT NOT NULL, vector_col VECTOR(4, float16))"
                    .to_string(),
                None,
                None,
            )
            .await
            .unwrap();

        let sent = vec![0.1f32, 1.0 / 3.0, -0.1, 65504.0];
        let expected: Vec<f32> = sent
            .iter()
            .map(|v| half::f16::from_f32(*v).to_f32())
            .collect();

        let rows = vec![Vector16Row {
            id: 1,
            vector_col: Some(sent.clone()),
        }];

        let result = {
            let bulk_copy = BulkCopy::new(&mut client, "#BulkCopyVector16Precision");
            bulk_copy
                .batch_size(1000)
                .write_to_server_zerocopy(&rows)
                .await
                .expect("Bulk copy failed")
        };
        assert_eq!(result.rows_affected, 1, "Expected 1 row to be inserted");

        client
            .execute(
                "SELECT vector_col FROM #BulkCopyVector16Precision".to_string(),
                None,
                None,
            )
            .await
            .expect("Failed to select data");

        let mut row_count = 0;
        if let Some(resultset) = client.get_current_resultset() {
            while let Some(row) = resultset.next_row().await.expect("Failed to read row") {
                row_count += 1;
                if let ColumnValues::Vector(vec) = &row[0] {
                    assert_eq!(vec.base_type(), VectorBaseType::Float16);
                    assert_eq!(
                        vec.as_f32().expect("Expected f32 values"),
                        expected.as_slice()
                    );
                } else {
                    panic!("Expected Vector, got {:?}", row[0]);
                }
            }
        }
        assert_eq!(row_count, 1, "Expected 1 row in result set");
    }

    /// Bulk copy into a table holding both float32 and float16 vector columns,
    /// exercising per-column base types in the bulk load column metadata.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_bulk_copy_mixed_vector_base_types() {
        let mut client = begin_connection(&build_tcp_datasource()).await;

        client
            .execute(
                "CREATE TABLE #BulkCopyMixedVectorTest (id INT NOT NULL, vec32 VECTOR(3), vec16 VECTOR(3, float16) NULL)"
                    .to_string(),
                None,
                None,
            )
            .await
            .unwrap();

        #[derive(Debug, Clone)]
        struct MixedVectorRow {
            id: i32,
            vec32: Vec<f32>,
            vec16: Option<Vec<f32>>,
        }

        #[async_trait]
        impl BulkLoadRow for MixedVectorRow {
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
                    .write_column_value(
                        *column_index,
                        &ColumnValues::Vector(SqlVector::try_from_f32(self.vec32.clone())?),
                    )
                    .await?;
                *column_index += 1;
                let vec16_val = if let Some(vec_data) = &self.vec16 {
                    ColumnValues::Vector(SqlVector::try_from_f16(vec_data.clone())?)
                } else {
                    ColumnValues::Null
                };
                writer.write_column_value(*column_index, &vec16_val).await?;
                *column_index += 1;
                Ok(())
            }
        }

        #[async_trait]
        impl BulkLoadRow for &MixedVectorRow {
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
                    .write_column_value(
                        *column_index,
                        &ColumnValues::Vector(SqlVector::try_from_f32(self.vec32.clone())?),
                    )
                    .await?;
                *column_index += 1;
                let vec16_val = if let Some(vec_data) = &self.vec16 {
                    ColumnValues::Vector(SqlVector::try_from_f16(vec_data.clone())?)
                } else {
                    ColumnValues::Null
                };
                writer.write_column_value(*column_index, &vec16_val).await?;
                *column_index += 1;
                Ok(())
            }
        }

        let vec32_row1 = vec![1.5, 2.5, 3.5];
        let vec16_row1 = vec![4.0, 5.0, 6.0];
        let vec32_row2 = vec![-1.5, 0.0, 7.25];

        let rows = vec![
            MixedVectorRow {
                id: 1,
                vec32: vec32_row1.clone(),
                vec16: Some(vec16_row1.clone()),
            },
            MixedVectorRow {
                id: 2,
                vec32: vec32_row2.clone(),
                vec16: None,
            },
        ];

        let result = {
            let bulk_copy = BulkCopy::new(&mut client, "#BulkCopyMixedVectorTest");
            bulk_copy
                .batch_size(1000)
                .write_to_server_zerocopy(&rows)
                .await
                .expect("Bulk copy failed")
        };
        assert_eq!(result.rows_affected, 2, "Expected 2 rows to be inserted");

        client
            .execute(
                "SELECT id, vec32, vec16 FROM #BulkCopyMixedVectorTest ORDER BY id".to_string(),
                None,
                None,
            )
            .await
            .expect("Failed to select data");

        let mut row_count = 0;
        if let Some(resultset) = client.get_current_resultset() {
            while let Some(row) = resultset.next_row().await.expect("Failed to read row") {
                row_count += 1;
                assert_eq!(row[0], ColumnValues::Int(row_count));

                let expected32 = match row_count {
                    1 => &vec32_row1,
                    2 => &vec32_row2,
                    _ => panic!("Unexpected row count: {}", row_count),
                };
                if let ColumnValues::Vector(vec) = &row[1] {
                    assert_eq!(vec.base_type(), VectorBaseType::Float32);
                    assert_eq!(
                        vec.as_f32().expect("Expected f32 values"),
                        expected32.as_slice()
                    );
                } else {
                    panic!("Expected Vector, got {:?}", row[1]);
                }

                match (row_count, &row[2]) {
                    (1, ColumnValues::Vector(vec)) => {
                        assert_eq!(vec.base_type(), VectorBaseType::Float16);
                        assert_eq!(
                            vec.as_f32().expect("Expected f32 values"),
                            vec16_row1.as_slice()
                        );
                    }
                    (2, ColumnValues::Null) => {}
                    _ => panic!("Unexpected value for row {}: {:?}", row_count, row[2]),
                }
            }
        }
        assert_eq!(row_count, 2, "Expected 2 rows in result set");
    }

    /// A float16 vector whose dimension count does not match the target column
    /// must be rejected.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_bulk_copy_vector16_dimension_mismatch() {
        let mut client = begin_connection(&build_tcp_datasource()).await;

        client
            .execute(
                "CREATE TABLE #BulkCopyVector16MismatchTest (id INT NOT NULL, vector_col VECTOR(3, float16))"
                    .to_string(),
                None,
                None,
            )
            .await
            .unwrap();

        for (id, values) in [(1, vec![1.0, 2.0]), (2, vec![1.0, 2.0, 3.0, 4.0])] {
            let dims = values.len();
            let rows = vec![Vector16Row {
                id,
                vector_col: Some(values),
            }];

            let result = {
                let bulk_copy = BulkCopy::new(&mut client, "#BulkCopyVector16MismatchTest");
                bulk_copy
                    .batch_size(1000)
                    .write_to_server_zerocopy(&rows)
                    .await
            };

            assert!(
                result.is_err(),
                "Expected bulk copy to fail with dimension mismatch ({} vs 3)",
                dims
            );
        }
    }
}
