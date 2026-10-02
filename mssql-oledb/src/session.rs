// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use crate::ConnectionOptions;
use mssql_tds::connection::client_context::ClientContext;
use mssql_tds::connection::tds_client::TdsClient;
use mssql_tds::connection::tds_client::{ResultSet as TdsResultSet, StatementResult};
use mssql_tds::connection_provider::tds_connection_provider::TdsConnectionProvider;
use mssql_tds::core::TdsResult;
use mssql_tds::datatypes::column_values::ColumnValues;
use mssql_tds::query::metadata::ColumnMetadata;
use std::fmt;
use tokio::runtime::Runtime;

#[derive(Debug)]
pub struct QueryResult {
    pub columns: Vec<ColumnMetadata>,
    pub rows_affected: Option<u64>,
}

#[derive(Debug)]
pub struct MssqlSession {
    runtime: Runtime,
    client: TdsClient,
}

impl MssqlSession {
    pub fn connect(options: &ConnectionOptions) -> Result<Self, SessionError> {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .map_err(|error| SessionError(error.to_string()))?;
        let mut context = ClientContext::default();
        context.database = options.initial_catalog.clone();
        context.user_name = options.user_id.clone();
        context.password = options.password.clone();
        context.encryption_options = options.encryption_options();
        let client = runtime
            .block_on(TdsConnectionProvider::new().create_client(
                context,
                &options.data_source,
                None,
            ))
            .map_err(SessionError::from)?;

        Ok(Self { runtime, client })
    }

    pub fn execute(&mut self, sql: impl Into<String>) -> Result<QueryResult, SessionError> {
        self.runtime
            .block_on(execute(&mut self.client, sql.into()))
            .map_err(SessionError::from)
    }

    pub fn fetch_next_row(&mut self) -> Result<Option<Vec<ColumnValues>>, SessionError> {
        if !self.client.on_rows() {
            return Ok(None);
        }
        self.runtime
            .block_on(TdsResultSet::next_row(&mut self.client))
            .map_err(SessionError::from)
    }

    pub fn next_result(&mut self) -> Result<Option<QueryResult>, SessionError> {
        let result: TdsResult<Option<QueryResult>> = self.runtime.block_on(async {
            if !self.client.advance_to_rows().await? {
                return Ok(None);
            }
            Ok(Some(QueryResult {
                columns: TdsResultSet::get_metadata(&self.client).clone(),
                rows_affected: None,
            }))
        });
        result.map_err(SessionError::from)
    }
}

async fn execute(client: &mut TdsClient, sql: String) -> TdsResult<QueryResult> {
    match client.execute(sql, ()).await? {
        StatementResult::Rows => Ok(QueryResult {
            columns: TdsResultSet::get_metadata(client).clone(),
            rows_affected: None,
        }),
        StatementResult::NoRows { rows_affected } => Ok(QueryResult {
            columns: Vec::new(),
            rows_affected,
        }),
        StatementResult::End => Ok(QueryResult {
            columns: Vec::new(),
            rows_affected: None,
        }),
    }
}

#[derive(Debug)]
pub struct SessionError(String);

impl From<mssql_tds::error::Error> for SessionError {
    fn from(error: mssql_tds::error::Error) -> Self {
        Self(error.to_string())
    }
}

impl fmt::Display for SessionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for SessionError {}
