// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

mod common;

use async_trait::async_trait;
use common::{begin_connection, build_tcp_datasource, init_tracing};
use mssql_tds::connection::bulk_copy::{BulkCopy, BulkLoadRow};
use mssql_tds::connection::tds_client::{ResultSet, TdsClient};
use mssql_tds::core::TdsResult;
use mssql_tds::datatypes::bulk_copy_metadata::BulkCopyColumnMetadata;
use mssql_tds::datatypes::column_values::ColumnValues;
use mssql_tds::datatypes::sql_string::SqlString;
use mssql_tds::error::Error;
use mssql_tds::message::bulk_load::StreamingBulkLoadWriter;
use mssql_tds::test_client_support::{MetadataRetriever, escape_identifier};

#[ctor::ctor]
fn init() {
    init_tracing();
}

struct TestRow(Vec<ColumnValues>);

#[async_trait]
impl BulkLoadRow for TestRow {
    async fn write_to_packet(
        &self,
        writer: &mut StreamingBulkLoadWriter<'_>,
        column_index: &mut usize,
    ) -> TdsResult<()> {
        for value in &self.0 {
            writer.write_column_value(*column_index, value).await?;
            *column_index += 1;
        }
        Ok(())
    }
}

struct CachedMetadata(Vec<BulkCopyColumnMetadata>);

#[async_trait]
impl MetadataRetriever for CachedMetadata {
    async fn retrieve_metadata(
        &mut self,
        _client: &mut TdsClient,
        _table_name: &str,
        _timeout_sec: u32,
    ) -> TdsResult<Vec<BulkCopyColumnMetadata>> {
        Ok(self.0.clone())
    }
}

async fn execute_statement(client: &mut TdsClient, sql: String) -> TdsResult<()> {
    client.execute(sql, ()).await?;
    client.close_query().await
}

async fn assert_int_query(client: &mut TdsClient, sql: String, expected: i32) -> TdsResult<()> {
    client.execute(sql, ()).await?;
    let row = client
        .next_row()
        .await?
        .expect("Expected an integer result");
    assert_eq!(row[0], ColumnValues::Int(expected));
    assert!(client.next_row().await?.is_none());
    client.close_query().await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_bulk_copy_identifier_payloads_default_and_cached_metadata() -> TdsResult<()> {
    let mut client = begin_connection(&build_tcp_datasource()).await;
    let table_payload = "#Target ([id] int); SELECT 4242 AS audit_probe;--";
    for (table_name, destination, column_name) in [
        (
            "#BulkIdentifiers",
            "#BulkIdentifiers",
            "id] int); SELECT 4242 AS audit_probe;--",
        ),
        (table_payload, table_payload, "id"),
        ("#O'Brien].Table", "[#O'Brien]].Table]", "O'Brien].Column"),
        ("#Double\"Quote", "\"#Double\"\"Quote\"", " column "),
        ("#\u{6d4b}\u{8bd5}", "[#\u{6d4b}\u{8bd5}]", "\u{5217}]"),
    ] {
        let quoted_table = escape_identifier(table_name);
        let quoted_column = escape_identifier(column_name);
        execute_statement(
            &mut client,
            format!("CREATE TABLE {quoted_table} ({quoted_column} INT NOT NULL)"),
        )
        .await?;

        let metadata = BulkCopy::new(&mut client, destination)
            .retrieve_destination_metadata()
            .await?;
        assert_eq!(metadata.len(), 1);
        assert_eq!(metadata[0].column_name, column_name);

        let result = BulkCopy::new(&mut client, destination)
            .write_to_server_zerocopy([TestRow(vec![ColumnValues::Int(7)])])
            .await?;
        assert_eq!(result.rows_affected, 1);
        let result =
            BulkCopy::with_retriever(&mut client, destination, Box::new(CachedMetadata(metadata)))
                .write_to_server_zerocopy([TestRow(vec![ColumnValues::Int(8)])])
                .await?;
        assert_eq!(result.rows_affected, 1);
        assert_int_query(
            &mut client,
            format!("SELECT COUNT(*) FROM {quoted_table} WHERE {quoted_column} IN (7, 8)"),
            2,
        )
        .await?;
        assert_int_query(
            &mut client,
            format!("SELECT SUM({quoted_column}) FROM {quoted_table}"),
            15,
        )
        .await?;
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_bulk_copy_omitted_schema_and_temp_names() -> TdsResult<()> {
    let mut client = begin_connection(&build_tcp_datasource()).await;
    execute_statement(
        &mut client,
        "CREATE TABLE #BulkOmitted (id INT NOT NULL)".to_string(),
    )
    .await?;
    let metadata = BulkCopy::new(&mut client, "#BulkOmitted")
        .retrieve_destination_metadata()
        .await?;
    let destinations = [
        "#BulkOmitted",
        "..#BulkOmitted",
        "tempdb..#BulkOmitted",
        "[tempdb]..[#BulkOmitted]",
        "[tempdb].[dbo].[#BulkOmitted]",
    ];
    for destination in destinations {
        let result = BulkCopy::new(&mut client, destination)
            .write_to_server_zerocopy([TestRow(vec![ColumnValues::Int(7)])])
            .await?;
        assert_eq!(result.rows_affected, 1);
        let result = BulkCopy::with_retriever(
            &mut client,
            destination,
            Box::new(CachedMetadata(metadata.clone())),
        )
        .write_to_server_zerocopy([TestRow(vec![ColumnValues::Int(8)])])
        .await?;
        assert_eq!(result.rows_affected, 1);
    }
    assert_int_query(
        &mut client,
        "SELECT COUNT(*) FROM #BulkOmitted".to_string(),
        10,
    )
    .await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_metadata_catalog_literal_does_not_execute_payload() -> TdsResult<()> {
    let mut client = begin_connection(&build_tcp_datasource()).await;
    execute_statement(
        &mut client,
        "CREATE TABLE #BulkInjectionProbe (id INT NOT NULL)".to_string(),
    )
    .await?;

    let catalog = "x'; INSERT INTO #BulkInjectionProbe VALUES (4242); RETURN;--";
    let result = BulkCopy::new(&mut client, format!("{}.dbo.t", escape_identifier(catalog)))
        .retrieve_destination_metadata()
        .await;
    assert!(
        result.is_err(),
        "The payload catalog must not resolve to a table"
    );
    client.close_query().await?;
    execute_statement(&mut client, "SET FMTONLY OFF".to_string()).await?;
    assert_int_query(
        &mut client,
        "SELECT COUNT(*) FROM #BulkInjectionProbe".to_string(),
        0,
    )
    .await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_bulk_copy_custom_metadata_rejects_invalid_targets() -> TdsResult<()> {
    let mut client = begin_connection(&build_tcp_datasource()).await;
    execute_statement(
        &mut client,
        "CREATE TABLE #BulkTargetValidation (id INT NOT NULL)".to_string(),
    )
    .await?;
    let metadata = BulkCopy::new(&mut client, "#BulkTargetValidation")
        .retrieve_destination_metadata()
        .await?;

    for destination in [
        "",
        " ",
        ".",
        "tempdb..",
        "dbo.",
        "[]",
        "[t",
        "[t]extra",
        "a.b.c.d.e",
    ] {
        let result = BulkCopy::with_retriever(
            &mut client,
            destination,
            Box::new(CachedMetadata(metadata.clone())),
        )
        .write_to_server_zerocopy([TestRow(vec![ColumnValues::Int(7)])])
        .await;
        assert!(matches!(result, Err(Error::UsageError(_))), "{destination}");
    }
    let result = BulkCopy::with_retriever(
        &mut client,
        "#BulkTargetValidation",
        Box::new(CachedMetadata(metadata)),
    )
    .write_to_server_zerocopy([TestRow(vec![ColumnValues::Int(7)])])
    .await?;
    assert_eq!(result.rows_affected, 1);
    assert_int_query(
        &mut client,
        "SELECT COUNT(*) FROM #BulkTargetValidation".to_string(),
        1,
    )
    .await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_bulk_copy_custom_metadata_rejects_invalid_collations() -> TdsResult<()> {
    let mut client = begin_connection(&build_tcp_datasource()).await;
    execute_statement(
        &mut client,
        "CREATE TABLE #BulkCollationValidation (value NVARCHAR(20) COLLATE Latin1_General_100_CI_AS NOT NULL)".to_string(),
    ).await?;
    let metadata = BulkCopy::new(&mut client, "#BulkCollationValidation")
        .retrieve_destination_metadata()
        .await?;
    assert_eq!(
        metadata[0].collation_name.as_deref(),
        Some("Latin1_General_100_CI_AS")
    );

    for collation in [
        "",
        "Latin1_General_100_CI_AS); SELECT 4242 AS audit_probe;--",
        "Latin1_General_100_CI_AS/*",
        "'Latin1_General_100_CI_AS'",
    ] {
        let mut invalid_metadata = metadata.clone();
        invalid_metadata[0].collation_name = Some(collation.to_string());
        let result = BulkCopy::with_retriever(
            &mut client,
            "#BulkCollationValidation",
            Box::new(CachedMetadata(invalid_metadata)),
        )
        .write_to_server_zerocopy([TestRow(vec![ColumnValues::Null])])
        .await;
        assert!(
            matches!(
                result,
                Err(Error::UsageError(message)) if message.starts_with("Invalid collation name:")
            ),
            "{collation}"
        );
    }
    assert_int_query(
        &mut client,
        "SELECT COUNT(*) FROM #BulkCollationValidation".to_string(),
        0,
    )
    .await?;
    let result = BulkCopy::with_retriever(
        &mut client,
        "#BulkCollationValidation",
        Box::new(CachedMetadata(metadata)),
    )
    .write_to_server_zerocopy([TestRow(vec![ColumnValues::String(
        SqlString::from_utf8_string("valid".to_string()),
    )])])
    .await?;
    assert_eq!(result.rows_affected, 1);
    assert_int_query(
        &mut client,
        "SELECT COUNT(*) FROM #BulkCollationValidation WHERE value = N'valid'".to_string(),
        1,
    )
    .await
}
