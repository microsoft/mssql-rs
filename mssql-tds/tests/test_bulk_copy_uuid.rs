// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#[cfg(test)]
mod common;

mod bulk_copy_uuid_tests {
    use crate::common::{begin_connection, build_tcp_datasource, init_tracing};
    use async_trait::async_trait;
    use mssql_tds::connection::bulk_copy::{BulkCopy, BulkLoadRow};
    use mssql_tds::connection::tds_client::ResultSet;
    use mssql_tds::core::TdsResult;
    use mssql_tds::datatypes::column_values::ColumnValues;
    use uuid::Uuid;

    #[ctor::ctor]
    fn init() {
        init_tracing();
    }

    #[derive(Debug, Clone)]
    struct UuidRow {
        id: i32,
        uuid_col: Option<Uuid>,
    }

    #[async_trait]
    impl BulkLoadRow for UuidRow {
        async fn write_to_packet(
            &self,
            writer: &mut mssql_tds::message::bulk_load::StreamingBulkLoadWriter<'_>,
            column_index: &mut usize,
        ) -> TdsResult<()> {
            writer
                .write_column_value(*column_index, &ColumnValues::Int(self.id))
                .await?;
            *column_index += 1;
            let uuid_val = self
                .uuid_col
                .map(ColumnValues::Uuid)
                .unwrap_or(ColumnValues::Null);
            writer.write_column_value(*column_index, &uuid_val).await?;
            *column_index += 1;
            Ok(())
        }
    }

    #[async_trait]
    impl BulkLoadRow for &UuidRow {
        async fn write_to_packet(
            &self,
            writer: &mut mssql_tds::message::bulk_load::StreamingBulkLoadWriter<'_>,
            column_index: &mut usize,
        ) -> TdsResult<()> {
            writer
                .write_column_value(*column_index, &ColumnValues::Int(self.id))
                .await?;
            *column_index += 1;
            let uuid_val = self
                .uuid_col
                .map(ColumnValues::Uuid)
                .unwrap_or(ColumnValues::Null);
            writer.write_column_value(*column_index, &uuid_val).await?;
            *column_index += 1;
            Ok(())
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_bulk_copy_uuid_basic() {
        let mut client = begin_connection(&build_tcp_datasource()).await;

        // Create temp table (automatically cleaned up)
        client
            .execute(
                "CREATE TABLE #BulkCopyUuidTest (id INT NOT NULL, uuid_col UNIQUEIDENTIFIER NULL)"
                    .to_string(),
                (),
            )
            .await
            .unwrap();

        let test_uuid1 = Uuid::try_parse("6F9619FF-8B86-D011-B42D-00C04FC964FF").unwrap();
        let test_uuid2 = Uuid::try_parse("A0EEBC99-9C0B-4EF8-BB6D-6BB9BD380A11").unwrap();
        let test_uuid3 = Uuid::try_parse("00000000-0000-0000-0000-000000000000").unwrap(); // NIL UUID
        let test_uuid4 = Uuid::new_v4(); // Random UUID

        let rows = vec![
            UuidRow {
                id: 1,
                uuid_col: Some(test_uuid1),
            },
            UuidRow {
                id: 2,
                uuid_col: Some(test_uuid2),
            },
            UuidRow {
                id: 3,
                uuid_col: None, // NULL
            },
            UuidRow {
                id: 4,
                uuid_col: Some(test_uuid3), // NIL UUID
            },
            UuidRow {
                id: 5,
                uuid_col: Some(test_uuid4),
            },
        ];

        // Execute bulk copy
        let result = {
            let bulk_copy = BulkCopy::new(&mut client, "#BulkCopyUuidTest");
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
                "SELECT id, uuid_col FROM #BulkCopyUuidTest ORDER BY id".to_string(),
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
                        assert_eq!(row[0], ColumnValues::Int(1));
                        assert_eq!(row[1], ColumnValues::Uuid(test_uuid1));
                    }
                    2 => {
                        assert_eq!(row[0], ColumnValues::Int(2));
                        assert_eq!(row[1], ColumnValues::Uuid(test_uuid2));
                    }
                    3 => {
                        assert_eq!(row[0], ColumnValues::Int(3));
                        assert_eq!(row[1], ColumnValues::Null);
                    }
                    4 => {
                        assert_eq!(row[0], ColumnValues::Int(4));
                        assert_eq!(row[1], ColumnValues::Uuid(test_uuid3));
                    }
                    5 => {
                        assert_eq!(row[0], ColumnValues::Int(5));
                        assert_eq!(row[1], ColumnValues::Uuid(test_uuid4));
                    }
                    _ => panic!("Unexpected row count: {}", row_count),
                }
            }
        }
        assert_eq!(row_count, 5, "Expected 5 rows in result set");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_bulk_copy_multiple_uuid_columns() {
        let mut client = begin_connection(&build_tcp_datasource()).await;

        // Create temp table with multiple UUID columns
        client
            .execute("CREATE TABLE #BulkCopyMultiUuidTest (id INT NOT NULL, uuid1 UNIQUEIDENTIFIER, uuid2 UNIQUEIDENTIFIER, uuid3 UNIQUEIDENTIFIER NULL)"
                    .to_string(), ())
            .await
            .unwrap();

        #[derive(Debug, Clone)]
        struct MultiUuidRow {
            id: i32,
            uuid1: Uuid,
            uuid2: Uuid,
            uuid3: Option<Uuid>,
        }

        #[async_trait]
        impl BulkLoadRow for MultiUuidRow {
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
                    .write_column_value(*column_index, &ColumnValues::Uuid(self.uuid1))
                    .await?;
                *column_index += 1;
                writer
                    .write_column_value(*column_index, &ColumnValues::Uuid(self.uuid2))
                    .await?;
                *column_index += 1;
                let uuid3_val = self
                    .uuid3
                    .map(ColumnValues::Uuid)
                    .unwrap_or(ColumnValues::Null);
                writer.write_column_value(*column_index, &uuid3_val).await?;
                *column_index += 1;
                Ok(())
            }
        }

        #[async_trait]
        impl BulkLoadRow for &MultiUuidRow {
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
                    .write_column_value(*column_index, &ColumnValues::Uuid(self.uuid1))
                    .await?;
                *column_index += 1;
                writer
                    .write_column_value(*column_index, &ColumnValues::Uuid(self.uuid2))
                    .await?;
                *column_index += 1;
                let uuid3_val = self
                    .uuid3
                    .map(ColumnValues::Uuid)
                    .unwrap_or(ColumnValues::Null);
                writer.write_column_value(*column_index, &uuid3_val).await?;
                *column_index += 1;
                Ok(())
            }
        }

        let uuid1 = Uuid::try_parse("11111111-1111-1111-1111-111111111111").unwrap();
        let uuid2 = Uuid::try_parse("22222222-2222-2222-2222-222222222222").unwrap();
        let uuid3 = Uuid::try_parse("33333333-3333-3333-3333-333333333333").unwrap();

        let rows = vec![
            MultiUuidRow {
                id: 1,
                uuid1,
                uuid2,
                uuid3: Some(uuid3),
            },
            MultiUuidRow {
                id: 2,
                uuid1: Uuid::new_v4(),
                uuid2: Uuid::new_v4(),
                uuid3: None,
            },
        ];

        // Execute bulk copy
        let result = {
            let bulk_copy = BulkCopy::new(&mut client, "#BulkCopyMultiUuidTest");
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
                "SELECT id, uuid1, uuid2, uuid3 FROM #BulkCopyMultiUuidTest ORDER BY id"
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
                        assert_eq!(row[0], ColumnValues::Int(1));
                        assert_eq!(row[1], ColumnValues::Uuid(uuid1));
                        assert_eq!(row[2], ColumnValues::Uuid(uuid2));
                        assert_eq!(row[3], ColumnValues::Uuid(uuid3));
                    }
                    2 => {
                        assert_eq!(row[0], ColumnValues::Int(2));
                        assert!(matches!(row[1], ColumnValues::Uuid(_)));
                        assert!(matches!(row[2], ColumnValues::Uuid(_)));
                        assert_eq!(row[3], ColumnValues::Null);
                    }
                    _ => panic!("Unexpected row count: {}", row_count),
                }
            }
        }
        assert_eq!(row_count, 2, "Expected 2 rows in result set");
    }
}
