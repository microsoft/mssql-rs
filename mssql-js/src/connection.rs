// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use std::{fmt::Debug, sync::Arc};

use mssql_tds::{
    connection::tds_client::{ResultSet, StatementResult, TdsClient},
    message::{
        parameters::rpc_parameters::{RpcParameter, StatusFlags},
        transaction_management::TransactionIsolationLevel,
    },
};
use napi::bindgen_prelude::{BigInt, Buffer, Either14, Null};
use tokio::sync::Mutex;
use tracing::instrument;

use crate::{
    binary_row_writer::BinaryRowWriter,
    datatypes::datetime::{
        NapiF64, NapiSqlDateTime, NapiSqlDateTime2, NapiSqlDateTimeOffset, NapiSqlTime,
    },
    ffidatatypes::{
        CollationMetadata, NapiDecimalParts, NapiSqlMoney, OutputParams, Parameter,
        ParameterDirection,
    },
};

/// The ordering is super important here as it defines the order in which napi will try to convert from one number
/// to another.
pub(crate) type RowDataType = Either14<
    NapiF64,               // A
    i32,                   // B
    BigInt,                // C
    bool,                  // D
    Buffer,                // E
    Null,                  // F
    NapiSqlDateTime,       // G
    u32,                   // H
    NapiSqlTime,           // I
    NapiSqlDateTime2,      // J
    NapiSqlDateTimeOffset, // K
    NapiSqlMoney,          // L
    NapiDecimalParts,      // M
    String,                // N
>;

#[napi(object)]
pub struct ChunkResult {
    pub data: Buffer,
    pub has_more: bool,
}

#[napi]
pub struct Connection {
    pub(crate) tds_client: Arc<Mutex<TdsClient>>,
    pub(crate) collation: Option<CollationMetadata>,
}

impl Debug for Connection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Connection").finish()
    }
}

#[napi]
impl Connection {
    #[napi]
    pub fn get_collation(&self) -> Option<CollationMetadata> {
        self.collation.clone()
    }

    #[napi]
    pub async fn execute(&self, query: String) -> napi::Result<()> {
        let mut client = self.tds_client.lock().await;
        let result = client.execute(query, ()).await;
        match result {
            Ok(_) => Ok(()),
            Err(e) => Err(napi::Error::from_reason(format!(
                "Failed to execute query: {e}"
            ))),
        }
    }

    #[napi]
    pub async fn execute_with_params(
        &self,
        query: String,
        params: Vec<Parameter>,
    ) -> napi::Result<()> {
        let mut client = self.tds_client.lock().await;

        let rpc_params: Result<Vec<RpcParameter>, napi::Error> = params
            .into_iter()
            .map(|p| {
                let param_name = p.name.clone();
                let param_value = match p.try_into() {
                    Ok(value) => value,
                    Err(e) => {
                        return Err(napi::Error::from_reason(format!(
                            "Parameter conversion failed: {e}"
                        )));
                    }
                };
                Ok(RpcParameter::new(
                    Some(param_name),
                    StatusFlags::NONE,
                    param_value,
                ))
            })
            .collect();

        let rpc_params = rpc_params?;
        let result = client.execute_sp_executesql(query, rpc_params, ()).await;

        match result {
            Ok(_) => Ok(()),
            Err(e) => Err(napi::Error::from_reason(format!(
                "Failed to execute query with parameters: {e}"
            ))),
        }
    }

    /// Executes a stored procedure with named parameters.
    /// The execution executes and positions the client to the first result set.
    #[napi]
    pub async fn execute_proc(
        &self,
        stored_proc_name: String,
        named_params: Vec<Parameter>,
    ) -> napi::Result<()> {
        let mut client = self.tds_client.lock().await;

        let named_params: Result<Vec<RpcParameter>, napi::Error> = named_params
            .into_iter()
            .map(|p| {
                let options = match &p.direction {
                    ParameterDirection::Input => StatusFlags::NONE,
                    ParameterDirection::Output => StatusFlags::BY_REF_VALUE,
                };
                let param_name = p.name.clone();
                let param_value = match p.try_into() {
                    Ok(value) => value,
                    Err(e) => {
                        return Err(napi::Error::from_reason(format!(
                            "Parameter conversion failed: {e}"
                        )));
                    }
                };
                Ok(RpcParameter::new(Some(param_name), options, param_value))
            })
            .collect();

        let named_params = named_params?;

        let result = client
            .execute_stored_procedure(stored_proc_name, None, Some(named_params), ())
            .await;

        match result {
            Ok(_) => Ok(()),
            Err(e) => Err(napi::Error::from_reason(format!(
                "Failed to execute stored proc: {e}"
            ))),
        }
    }

    /// Executes a query and returns all result sets as binary `Buffer`s,
    /// one per result set, avoiding per-row N-API boundary crossings.
    ///
    /// The buffer layout is decoded on the JS side by `decode.ts`.
    #[napi]
    pub async fn query_raw(&self, query: String) -> napi::Result<Vec<Buffer>> {
        let mut client = self.tds_client.lock().await;

        // Statement-wise execute, then collapse to row-returning result sets.
        let first = client
            .execute(query, ())
            .await
            .map_err(|e| napi::Error::from_reason(format!("Failed to execute query: {e}")))?;
        let mut has_rows = matches!(first, StatementResult::Rows);
        if !has_rows {
            has_rows = client.advance_to_rows().await.map_err(|e| {
                napi::Error::from_reason(format!("Failed to advance to next result set: {e}"))
            })?;
        }

        let mut buffers: Vec<Buffer> = Vec::new();

        while has_rows {
            let metadata = client.get_metadata().clone();
            let col_count = metadata.len() as u16;

            let mut writer = BinaryRowWriter::new(col_count);

            let col_names: Vec<String> = metadata.iter().map(|m| m.column_name.clone()).collect();
            let col_name_indices = writer.intern_column_names(&col_names);
            let col_type_ids: Vec<u8> = metadata.iter().map(|m| m.data_type as u8).collect();

            while client
                .next_row_into(&mut writer)
                .await
                .map_err(|e| napi::Error::from_reason(format!("Failed to read row: {e}")))?
            {
            }

            buffers.push(Buffer::from(writer.finalize(
                &col_name_indices,
                &col_type_ids,
                0,
            )));

            has_rows = client.advance_to_rows().await.map_err(|e| {
                napi::Error::from_reason(format!("Failed to advance to next result set: {e}"))
            })?;
        }

        Ok(buffers)
    }

    /// Fetch rows from the current result set into a binary buffer, stopping
    /// when the accumulated row data reaches `byte_budget` bytes or the result
    /// set is exhausted.
    ///
    /// Returns `None` when there is no active result set.
    #[napi]
    pub async fn fetch_chunk(&self, byte_budget: u32) -> napi::Result<Option<ChunkResult>> {
        let mut client = self.tds_client.lock().await;

        if !client.on_rows() {
            return Ok(None);
        }

        let metadata = client.get_metadata().clone();
        let col_count = metadata.len() as u16;

        let mut writer = BinaryRowWriter::new(col_count);

        let col_names: Vec<String> = metadata.iter().map(|m| m.column_name.clone()).collect();
        let col_name_indices = writer.intern_column_names(&col_names);
        let col_type_ids: Vec<u8> = metadata.iter().map(|m| m.data_type as u8).collect();

        let budget = byte_budget as usize;
        let mut has_more = false;

        loop {
            let had_row = client
                .next_row_into(&mut writer)
                .await
                .map_err(|e| napi::Error::from_reason(format!("Failed to read row: {e}")))?;

            if !had_row {
                break;
            }

            if writer.row_data_len() >= budget {
                has_more = true;
                break;
            }
        }

        Ok(Some(ChunkResult {
            data: Buffer::from(writer.finalize(&col_name_indices, &col_type_ids, 0)),
            has_more,
        }))
    }

    #[napi]
    #[instrument]
    pub async fn next_result_set(&self) -> napi::Result<bool> {
        let mut client = self.tds_client.lock().await;
        let result = client.advance_to_rows().await;
        match result {
            Ok(has_next) => Ok(has_next),
            Err(e) => Err(napi::Error::from_reason(format!(
                "Failed to get next result set: {e}"
            ))),
        }
    }

    #[napi]
    #[instrument]
    pub async fn get_return_values(&self) -> napi::Result<Option<Vec<OutputParams>>> {
        let client = self.tds_client.lock().await;
        let return_values = client.get_return_values();
        let output_params: Result<Vec<OutputParams>, _> = return_values
            .into_iter()
            .map(OutputParams::try_from)
            .collect();
        let output_params = output_params.map_err(|e| {
            napi::Error::from_reason(format!("Failed to convert return values: {e}"))
        })?;
        Ok(Some(output_params))
    }

    #[napi]
    pub async fn close_query(&self) -> napi::Result<()> {
        let mut client = self.tds_client.lock().await;
        let result = client.close_query().await;
        match result {
            Ok(_) => Ok(()),
            Err(e) => Err(napi::Error::from_reason(format!(
                "Failed to close query: {e}"
            ))),
        }
    }

    #[napi]
    pub async fn close(&self) -> napi::Result<()> {
        let mut client = self.tds_client.lock().await;
        let result = client.close_connection().await;
        match result {
            Ok(_) => Ok(()),
            Err(e) => Err(napi::Error::from_reason(format!(
                "Failed to close connection: {e}"
            ))),
        }
    }

    #[napi]
    pub async fn begin_transaction(
        &self,
        isolation_level: NapiIsolationLevel,
        savepoint_name: Option<String>,
    ) -> napi::Result<()> {
        let mut client = self.tds_client.lock().await;
        let result = client
            .begin_transaction(isolation_level.into(), savepoint_name)
            .await;
        match result {
            Ok(_) => Ok(()),
            Err(e) => Err(napi::Error::from_reason(format!(
                "Failed to begin transaction: {e}"
            ))),
        }
    }

    #[napi]
    pub async fn commit_transaction(&self) -> napi::Result<()> {
        let mut client = self.tds_client.lock().await;
        let result = client.commit_transaction(None, None).await;
        match result {
            Ok(_) => Ok(()),
            Err(e) => Err(napi::Error::from_reason(format!(
                "Failed to commit transaction: {e}"
            ))),
        }
    }

    #[napi]
    pub async fn rollback_transaction(&self, name: Option<String>) -> napi::Result<()> {
        let mut client = self.tds_client.lock().await;
        let result = client.rollback_transaction(name, None).await;
        match result {
            Ok(_) => Ok(()),
            Err(e) => Err(napi::Error::from_reason(format!(
                "Failed to rollback transaction: {e}"
            ))),
        }
    }

    #[napi]
    pub async fn save_transaction(&self, savepoint_name: String) -> napi::Result<()> {
        let mut client = self.tds_client.lock().await;
        let result = client.save_transaction(savepoint_name).await;
        match result {
            Ok(_) => Ok(()),
            Err(e) => Err(napi::Error::from_reason(format!(
                "Failed to save transaction: {e}"
            ))),
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
#[napi]
pub enum NapiIsolationLevel {
    NoChange = 0x00,
    ReadUncommitted = 0x01,
    ReadCommitted = 0x02,
    RepeatableRead = 0x03,
    Serializable = 0x04,
    Snapshot = 0x05,
}

impl From<NapiIsolationLevel> for TransactionIsolationLevel {
    fn from(level: NapiIsolationLevel) -> Self {
        match level {
            NapiIsolationLevel::NoChange => TransactionIsolationLevel::NoChange,
            NapiIsolationLevel::ReadUncommitted => TransactionIsolationLevel::ReadUncommitted,
            NapiIsolationLevel::ReadCommitted => TransactionIsolationLevel::ReadCommitted,
            NapiIsolationLevel::RepeatableRead => TransactionIsolationLevel::RepeatableRead,
            NapiIsolationLevel::Serializable => TransactionIsolationLevel::Serializable,
            NapiIsolationLevel::Snapshot => TransactionIsolationLevel::Snapshot,
        }
    }
}
