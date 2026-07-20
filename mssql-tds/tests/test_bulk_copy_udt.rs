// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Integration test for bulk copy of CLR UDT columns (GH-667).
//!
//! Custom (user-defined) CLR UDTs require a deployed assembly, which is not
//! practical to provision in a test run. This test instead exercises the exact
//! same TDS `0xF0` (UDT) wire path using the built-in `geometry` CLR UDT, which
//! ships with SQL Server.
//!
//! Before the fix, bulk copy of any UDT column failed with
//! `Protocol Error: Unsupported TDS type for bulk copy: 0xF0` because the UDT
//! wire token had no COLMETADATA representation. The fix streams UDTs as
//! `varbinary(max)` (their `IBinarySerialize` form), matching pyodbc/python-tds.

mod common;

mod bulk_copy_udt_tests {
    use crate::common::{begin_connection, build_tcp_datasource, init_tracing};
    use async_trait::async_trait;
    use mssql_tds::connection::bulk_copy::{BulkCopy, BulkLoadRow};
    use mssql_tds::connection::tds_client::{ResultSet, ResultSetClient};
    use mssql_tds::core::TdsResult;
    use mssql_tds::datatypes::column_values::ColumnValues;

    #[ctor::ctor]
    fn init() {
        init_tracing();
    }

    #[derive(Debug, Clone)]
    struct UdtRow {
        id: i32,
        /// Serialized CLR UDT bytes (varbinary form), or `None` for SQL NULL.
        udt: Option<Vec<u8>>,
    }

    #[async_trait]
    impl BulkLoadRow for UdtRow {
        async fn write_to_packet(
            &self,
            writer: &mut mssql_tds::message::bulk_load::StreamingBulkLoadWriter<'_>,
            column_index: &mut usize,
        ) -> TdsResult<()> {
            writer
                .write_column_value(*column_index, &ColumnValues::Int(self.id))
                .await?;
            *column_index += 1;
            let udt_val = self
                .udt
                .as_ref()
                .map(|b| ColumnValues::Bytes(b.clone()))
                .unwrap_or(ColumnValues::Null);
            writer.write_column_value(*column_index, &udt_val).await?;
            *column_index += 1;
            Ok(())
        }
    }

    #[async_trait]
    impl BulkLoadRow for &UdtRow {
        async fn write_to_packet(
            &self,
            writer: &mut mssql_tds::message::bulk_load::StreamingBulkLoadWriter<'_>,
            column_index: &mut usize,
        ) -> TdsResult<()> {
            (*self).write_to_packet(writer, column_index).await
        }
    }

    /// End-to-end round-trip: seed a `geometry` column, read the serialized UDT
    /// bytes back, bulk copy them into a second `geometry` column, and verify the
    /// stored bytes match. This exercises the UDT (`0xF0`) bulk-copy path that
    /// previously failed with "Unsupported TDS type for bulk copy: 0xF0".
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_bulk_copy_geometry_udt_column() {
        let mut client = begin_connection(&build_tcp_datasource()).await;

        // Source table with a geometry (built-in CLR UDT) column, seeded via T-SQL.
        client
            .execute(
                "CREATE TABLE #BulkCopyUdtSrc (id INT NOT NULL, g geometry NULL)".to_string(),
                None,
                None,
            )
            .await
            .expect("Failed to create source table");
        client.close_query().await.expect("Failed to close query");

        client
            .execute(
                "INSERT INTO #BulkCopyUdtSrc (id, g) VALUES \
                 (1, geometry::STGeomFromText('POINT(1 2)', 0)), \
                 (2, geometry::STGeomFromText('LINESTRING(0 0, 10 10, 20 25)', 0)), \
                 (3, NULL), \
                 (4, geometry::STGeomFromText('POLYGON((0 0, 0 5, 5 5, 5 0, 0 0))', 0))"
                    .to_string(),
                None,
                None,
            )
            .await
            .expect("Failed to seed source table");
        client.close_query().await.expect("Failed to close query");

        // Read the serialized UDT bytes back from the source.
        client
            .execute(
                "SELECT id, g FROM #BulkCopyUdtSrc ORDER BY id".to_string(),
                None,
                None,
            )
            .await
            .expect("Failed to select source data");

        let mut source_rows: Vec<UdtRow> = Vec::new();
        if let Some(resultset) = client.get_current_resultset() {
            while let Some(row) = resultset.next_row().await.expect("Failed to read row") {
                let id = match &row[0] {
                    ColumnValues::Int(v) => *v,
                    other => panic!("unexpected id type: {:?}", other),
                };
                let udt = match &row[1] {
                    ColumnValues::Bytes(b) => Some(b.clone()),
                    ColumnValues::Null => None,
                    other => panic!("unexpected UDT column type: {:?}", other),
                };
                source_rows.push(UdtRow { id, udt });
            }
        }
        client.close_query().await.expect("Failed to close query");

        assert_eq!(source_rows.len(), 4, "Expected 4 source rows");
        assert!(
            source_rows.iter().filter(|r| r.udt.is_some()).count() == 3,
            "Expected 3 non-null UDT values"
        );
        assert!(
            source_rows.iter().any(|r| r.udt.is_none()),
            "Expected one NULL UDT value"
        );

        // Destination table with the same geometry column.
        client
            .execute(
                "CREATE TABLE #BulkCopyUdtDst (id INT NOT NULL, g geometry NULL)".to_string(),
                None,
                None,
            )
            .await
            .expect("Failed to create destination table");
        client.close_query().await.expect("Failed to close query");

        // Bulk copy the UDT bytes. Pre-fix this failed with
        // "Unsupported TDS type for bulk copy: 0xF0".
        let result = {
            let bulk_copy = BulkCopy::new(&mut client, "#BulkCopyUdtDst");
            bulk_copy
                .batch_size(1000)
                .write_to_server_zerocopy(&source_rows)
                .await
                .expect("Bulk copy of UDT column failed")
        };
        assert_eq!(result.rows_affected, 4, "Expected 4 rows to be inserted");

        // Verify round-trip: destination bytes must equal source bytes.
        client
            .execute(
                "SELECT id, g FROM #BulkCopyUdtDst ORDER BY id".to_string(),
                None,
                None,
            )
            .await
            .expect("Failed to select destination data");

        let mut row_count = 0usize;
        if let Some(resultset) = client.get_current_resultset() {
            while let Some(row) = resultset.next_row().await.expect("Failed to read row") {
                let expected = &source_rows[row_count];
                assert_eq!(row[0], ColumnValues::Int(expected.id));
                match (&row[1], &expected.udt) {
                    (ColumnValues::Bytes(got), Some(exp)) => {
                        assert_eq!(got, exp, "UDT bytes mismatch for id={}", expected.id);
                    }
                    (ColumnValues::Null, None) => {}
                    (got, exp) => panic!(
                        "row id={} mismatch: got={:?} expected_null={}",
                        expected.id,
                        got,
                        exp.is_none()
                    ),
                }
                row_count += 1;
            }
        }
        assert_eq!(row_count, 4, "Expected 4 rows in destination table");
    }
}
