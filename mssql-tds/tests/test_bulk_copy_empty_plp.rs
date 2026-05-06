// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#[cfg(test)]
mod common;

mod bulk_copy_empty_plp_tests {
    use crate::common::{begin_connection, build_tcp_datasource, init_tracing};
    use async_trait::async_trait;
    use mssql_tds::connection::bulk_copy::{BulkCopy, BulkLoadRow};
    use mssql_tds::connection::tds_client::{ResultSet, ResultSetClient};
    use mssql_tds::core::TdsResult;
    use mssql_tds::datatypes::column_values::ColumnValues;
    use mssql_tds::datatypes::sql_string::SqlString;

    #[ctor::ctor]
    fn init() {
        init_tracing();
    }

    /// A row with id, NVARCHAR(MAX), VARCHAR(MAX), VARBINARY(MAX) and a
    /// trailing INT column. The trailing column is critical: the original
    /// bug for empty MAX values surfaced as "premature end of message" or
    /// data corruption on the *next* column because a zero-length PLP chunk
    /// header was misread as the PLP terminator, leaving 4 spurious bytes
    /// in the row buffer.
    #[derive(Debug, Clone)]
    struct MaxRow {
        id: i32,
        nvc_max: Option<String>,
        vc_max: Option<String>,
        vb_max: Option<Vec<u8>>,
        tail: i32,
    }

    async fn write_one(
        row: &MaxRow,
        writer: &mut mssql_tds::message::bulk_load::StreamingBulkLoadWriter<'_>,
        column_index: &mut usize,
    ) -> TdsResult<()> {
        writer
            .write_column_value(*column_index, &ColumnValues::Int(row.id))
            .await?;
        *column_index += 1;
        let nvc = match &row.nvc_max {
            Some(s) => ColumnValues::String(SqlString::from_utf8_string(s.clone())),
            None => ColumnValues::Null,
        };
        writer.write_column_value(*column_index, &nvc).await?;
        *column_index += 1;
        let vc = match &row.vc_max {
            Some(s) => ColumnValues::String(SqlString::from_utf8_string(s.clone())),
            None => ColumnValues::Null,
        };
        writer.write_column_value(*column_index, &vc).await?;
        *column_index += 1;
        let vb = match &row.vb_max {
            Some(b) => ColumnValues::Bytes(b.clone()),
            None => ColumnValues::Null,
        };
        writer.write_column_value(*column_index, &vb).await?;
        *column_index += 1;
        writer
            .write_column_value(*column_index, &ColumnValues::Int(row.tail))
            .await?;
        *column_index += 1;
        Ok(())
    }

    #[async_trait]
    impl BulkLoadRow for MaxRow {
        async fn write_to_packet(
            &self,
            writer: &mut mssql_tds::message::bulk_load::StreamingBulkLoadWriter<'_>,
            column_index: &mut usize,
        ) -> TdsResult<()> {
            write_one(self, writer, column_index).await
        }
    }

    #[async_trait]
    impl BulkLoadRow for &MaxRow {
        async fn write_to_packet(
            &self,
            writer: &mut mssql_tds::message::bulk_load::StreamingBulkLoadWriter<'_>,
            column_index: &mut usize,
        ) -> TdsResult<()> {
            write_one(self, writer, column_index).await
        }
    }

    /// Regression test for microsoft/mssql-python#547.
    ///
    /// Empty values into NVARCHAR(MAX) / VARCHAR(MAX) / VARBINARY(MAX)
    /// previously desynced the PLP stream because the serializer wrote a
    /// 4-byte zero-length chunk header (which IS the PLP terminator per
    /// MS-TDS) plus an explicit terminator. The server consumed the first
    /// 4 zero bytes as the terminator and then misread the next 4 bytes
    /// as part of the following column, surfacing as SQL error 4804
    /// ("premature end of message") or as data corruption.
    ///
    /// This test covers all three PLP types in a single row and includes a
    /// trailing INT column to detect any residual stream misalignment.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_bulk_copy_empty_plp_max_values() {
        let mut client = begin_connection(&build_tcp_datasource()).await;

        client
            .execute(
                "CREATE TABLE #BulkCopyEmptyPlp (
                    id INT NOT NULL,
                    nvc_max NVARCHAR(MAX) NULL,
                    vc_max VARCHAR(MAX) NULL,
                    vb_max VARBINARY(MAX) NULL,
                    tail INT NOT NULL
                )"
                .to_string(),
                None,
                None,
            )
            .await
            .expect("Failed to create temp table");
        client.close_query().await.expect("Failed to close query");

        let rows = vec![
            MaxRow {
                id: 1,
                nvc_max: Some("hello".to_string()),
                vc_max: Some("hello".to_string()),
                vb_max: Some(vec![0xDE, 0xAD, 0xBE, 0xEF]),
                tail: 100,
            },
            MaxRow {
                id: 2,
                nvc_max: Some(String::new()),
                vc_max: Some(String::new()),
                vb_max: Some(Vec::new()),
                tail: 200,
            },
            MaxRow {
                id: 3,
                nvc_max: None,
                vc_max: None,
                vb_max: None,
                tail: 300,
            },
            MaxRow {
                id: 4,
                nvc_max: Some("world".to_string()),
                vc_max: Some("world".to_string()),
                vb_max: Some(vec![0x01, 0x02, 0x03]),
                tail: 400,
            },
        ];

        let result = {
            let bulk_copy = BulkCopy::new(&mut client, "#BulkCopyEmptyPlp");
            bulk_copy
                .batch_size(100)
                .write_to_server_zerocopy(&rows)
                .await
                .expect("Bulk copy of empty PLP values failed")
        };
        assert_eq!(result.rows_affected, 4);

        client
            .execute(
                "SELECT id, nvc_max, vc_max, vb_max, tail FROM #BulkCopyEmptyPlp ORDER BY id"
                    .to_string(),
                None,
                None,
            )
            .await
            .expect("Failed to query temp table");

        let resultset = client.get_current_resultset().expect("No resultset");

        // Row 1: non-empty values
        let r1 = resultset.next_row().await.unwrap().unwrap();
        assert_eq!(r1[0], ColumnValues::Int(1));
        match &r1[1] {
            ColumnValues::String(s) => assert_eq!(s.to_utf8_string(), "hello"),
            other => panic!("nvc row 1: {other:?}"),
        }
        match &r1[2] {
            ColumnValues::String(s) => assert_eq!(s.to_utf8_string(), "hello"),
            other => panic!("vc row 1: {other:?}"),
        }
        match &r1[3] {
            ColumnValues::Bytes(b) => assert_eq!(b, &vec![0xDE, 0xAD, 0xBE, 0xEF]),
            other => panic!("vb row 1: {other:?}"),
        }
        assert_eq!(r1[4], ColumnValues::Int(100));

        // Row 2: empty values — must round-trip as empty (not NULL) and
        // must not corrupt the trailing INT column.
        let r2 = resultset.next_row().await.unwrap().unwrap();
        assert_eq!(r2[0], ColumnValues::Int(2));
        match &r2[1] {
            ColumnValues::String(s) => assert_eq!(s.to_utf8_string(), ""),
            other => panic!("nvc row 2 expected empty string, got {other:?}"),
        }
        match &r2[2] {
            ColumnValues::String(s) => assert_eq!(s.to_utf8_string(), ""),
            other => panic!("vc row 2 expected empty string, got {other:?}"),
        }
        match &r2[3] {
            // Empty VARBINARY(MAX) round-trips as an empty byte vec.
            ColumnValues::Bytes(b) => assert!(b.is_empty(), "vb row 2 expected empty"),
            other => panic!("vb row 2 expected empty bytes, got {other:?}"),
        }
        assert_eq!(r2[4], ColumnValues::Int(200));

        // Row 3: NULL values
        let r3 = resultset.next_row().await.unwrap().unwrap();
        assert_eq!(r3[0], ColumnValues::Int(3));
        assert!(matches!(r3[1], ColumnValues::Null));
        assert!(matches!(r3[2], ColumnValues::Null));
        assert!(matches!(r3[3], ColumnValues::Null));
        assert_eq!(r3[4], ColumnValues::Int(300));

        // Row 4: non-empty after empties — confirms stream alignment held
        let r4 = resultset.next_row().await.unwrap().unwrap();
        assert_eq!(r4[0], ColumnValues::Int(4));
        match &r4[1] {
            ColumnValues::String(s) => assert_eq!(s.to_utf8_string(), "world"),
            other => panic!("nvc row 4: {other:?}"),
        }
        assert_eq!(r4[4], ColumnValues::Int(400));

        client.close_query().await.expect("Failed to close query");
    }
}
