// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Bulk diagnostics survive internal transaction requests, without a live server.

use async_trait::async_trait;
use mssql_tds::connection::bulk_copy::{BulkCopy, BulkLoadRow};
use mssql_tds::connection::tds_client::TdsClient;
use mssql_tds::core::TdsResult;
use mssql_tds::datatypes::bulk_copy_metadata::{BulkCopyColumnMetadata, SqlDbType, TypeLength};
use mssql_tds::datatypes::column_values::ColumnValues;
use mssql_tds::datatypes::sql_string::SqlString;
use mssql_tds::message::bulk_load::StreamingBulkLoadWriter;
use mssql_tds::test_client_support::{
    MetadataRetriever, ScriptedToken, done_no_more, env_change_begin_transaction,
    env_change_rollback_transaction, sql_error, tds_client_from_tokens,
};
use mssql_tds::token::tokens::SqlCollation;

struct VarcharMetadata;

#[async_trait]
impl MetadataRetriever for VarcharMetadata {
    async fn retrieve_metadata(
        &mut self,
        _client: &mut TdsClient,
        _table_name: &str,
        _timeout_sec: u32,
    ) -> TdsResult<Vec<BulkCopyColumnMetadata>> {
        Ok(vec![
            BulkCopyColumnMetadata::new("value", SqlDbType::VarChar, 0xA7)
                .with_length(100, TypeLength::Variable(100))
                .with_collation(SqlCollation {
                    info: 0x0409,
                    lcid_language_id: 0,
                    col_flags: 0,
                    sort_id: 0,
                })
                .with_collation_name("Latin1_General_CI_AS"),
        ])
    }
}

struct TextRow(&'static str);

#[async_trait]
impl BulkLoadRow for TextRow {
    async fn write_to_packet(
        &self,
        writer: &mut StreamingBulkLoadWriter<'_>,
        column_index: &mut usize,
    ) -> TdsResult<()> {
        writer
            .write_column_value(
                *column_index,
                &ColumnValues::String(SqlString::from_utf8_string(self.0.to_string())),
            )
            .await?;
        *column_index += 1;
        Ok(())
    }
}

async fn copy(client: &mut TdsClient, rows: Vec<TextRow>) -> TdsResult<()> {
    BulkCopy::with_retriever(client, "BulkLoss", Box::new(VarcharMetadata))
        .use_internal_transaction(true)
        .batch_size(1)
        .write_to_server_zerocopy(rows)
        .await
        .map(|_| ())
}

fn begin_batch() -> Vec<ScriptedToken> {
    vec![
        env_change_begin_transaction(1),
        done_no_more(),
        // INSERT BULK preamble completes before row serialization.
        done_no_more(),
    ]
}

#[tokio::test]
async fn conversion_loss_survives_internal_commit_failure() {
    let mut tokens = begin_batch();
    tokens.extend([
        done_no_more(),
        sql_error(50001, 16, "internal commit failed"),
        done_no_more(),
    ]);
    let mut client = tds_client_from_tokens(tokens);

    let error = copy(&mut client, vec![TextRow("日")]).await.unwrap_err();
    assert!(
        error.to_string().contains("internal commit failed"),
        "{error}"
    );
    assert!(client.take_code_page_conversion_loss());
    assert!(!client.take_code_page_conversion_loss());
}

#[tokio::test]
async fn conversion_loss_survives_failed_batch_and_internal_rollback() {
    let mut tokens = begin_batch();
    tokens.extend([
        sql_error(50002, 16, "bulk batch failed"),
        done_no_more(),
        env_change_rollback_transaction(),
        done_no_more(),
    ]);
    let mut client = tds_client_from_tokens(tokens);

    let error = copy(&mut client, vec![TextRow("日")]).await.unwrap_err();
    assert!(error.to_string().contains("bulk batch failed"), "{error}");
    assert!(!client.has_active_transaction(), "rollback must complete");
    assert!(client.take_code_page_conversion_loss());
    assert!(!client.take_code_page_conversion_loss());
}

#[tokio::test]
async fn conversion_loss_survives_failed_batch_and_failed_internal_rollback() {
    let mut tokens = begin_batch();
    tokens.extend([
        sql_error(50002, 16, "bulk batch failed"),
        done_no_more(),
        sql_error(50003, 16, "internal rollback failed"),
        done_no_more(),
    ]);
    let mut client = tds_client_from_tokens(tokens);

    let error = copy(&mut client, vec![TextRow("日")]).await.unwrap_err();
    assert!(error.to_string().contains("bulk batch failed"), "{error}");
    assert!(client.has_active_transaction(), "rollback must fail");
    assert!(client.take_code_page_conversion_loss());
}

#[tokio::test]
async fn clean_batch_does_not_report_loss_after_internal_commit_failure() {
    let mut tokens = begin_batch();
    tokens.extend([
        done_no_more(),
        sql_error(50001, 16, "internal commit failed"),
        done_no_more(),
    ]);
    let mut client = tds_client_from_tokens(tokens);

    let error = copy(&mut client, vec![TextRow("ASCII")]).await.unwrap_err();
    assert!(
        error.to_string().contains("internal commit failed"),
        "{error}"
    );
    assert!(!client.take_code_page_conversion_loss());
}

#[tokio::test]
async fn earlier_batch_conversion_loss_survives_next_internal_begin_failure() {
    let mut tokens = begin_batch();
    tokens.extend([
        done_no_more(),
        done_no_more(),
        sql_error(50004, 16, "next internal begin failed"),
        done_no_more(),
    ]);
    let mut client = tds_client_from_tokens(tokens);

    let error = copy(&mut client, vec![TextRow("日"), TextRow("ASCII")])
        .await
        .unwrap_err();
    assert!(
        error.to_string().contains("next internal begin failed"),
        "{error}"
    );
    assert!(client.take_code_page_conversion_loss());
}
