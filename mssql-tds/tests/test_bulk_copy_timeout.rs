// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#[cfg(test)]
mod common;

mod bulk_copy_timeout_tests {
    use crate::common::{begin_connection, build_tcp_datasource, init_tracing};
    use async_trait::async_trait;
    use mssql_tds::connection::bulk_copy::{BulkCopy, BulkLoadRow};
    use mssql_tds::core::TdsResult;
    use mssql_tds::datatypes::column_values::ColumnValues;
    use std::time::Duration;

    #[ctor::ctor]
    fn init() {
        init_tracing();
    }

    const COLS: usize = 200;

    struct WideRow;

    #[async_trait]
    impl BulkLoadRow for WideRow {
        async fn write_to_packet(
            &self,
            writer: &mut mssql_tds::message::bulk_load::StreamingBulkLoadWriter<'_>,
            column_index: &mut usize,
        ) -> TdsResult<()> {
            for i in 0..COLS {
                writer
                    .write_column_value(*column_index, &ColumnValues::Int(i as i32))
                    .await?;
                *column_index += 1;
            }
            Ok(())
        }
    }

    /// Regression test for issue #513: bulk copy must return a clean timeout
    /// error — not a panic — when the timeout fires while data is being written.
    ///
    /// An infinite iterator with a 1-second timeout guarantees the timeout will
    /// fire while packets are in flight.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn bulk_copy_timeout_returns_error_not_panic() {
        let mut client = begin_connection(&build_tcp_datasource()).await;

        let col_defs: String = (0..COLS)
            .map(|i| format!("[c{i}] INT"))
            .collect::<Vec<_>>()
            .join(", ");
        client
            .execute(
                format!("CREATE TABLE #bcp_timeout_513 ({col_defs})"),
                None,
                None,
            )
            .await
            .expect("Failed to create table");
        client.close_query().await.expect("Failed to close query");

        let infinite_rows = std::iter::repeat_with(|| WideRow);

        let result = BulkCopy::new(&mut client, "#bcp_timeout_513")
            .timeout(Duration::from_secs(1))
            .write_to_server_zerocopy(infinite_rows)
            .await;

        assert!(result.is_err(), "Expected timeout error, got success");
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("Timeout") || err.contains("timeout"),
            "Expected a timeout error, got: {err}"
        );
    }
}
