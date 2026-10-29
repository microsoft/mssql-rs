// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use std::sync::Arc;
mod tracing_init;
use tracing_init::init_tracing;

use mssql_tds::{
    connection::client_context::ClientContext,
    connection_provider::tds_connection_provider::TdsConnectionProvider,
};
use tokio::sync::Mutex;

use crate::{connection::Connection, context::JsClientContext, ffidatatypes::CollationMetadata};

#[macro_use]
extern crate napi_derive;

pub mod binary_row_writer;
pub mod connection;
pub mod context;
pub mod datatypes;
pub mod ffidatatypes;

#[napi]
pub fn set_default_multi_subnet_failover(enabled: bool) -> napi::Result<()> {
    mssql_tds::connection::multi_subnet_failover::set_default_multi_subnet_failover(enabled)
        .map_err(|e| napi::Error::from_reason(e.to_string()))
}

#[napi]
pub async fn connect(context: JsClientContext) -> napi::Result<Connection> {
    // Initialize tracing if enabled
    init_tracing();
    let client_context: ClientContext = context.clone().into();
    let provider = TdsConnectionProvider {};
    // Use comma separator for port, not colon (SQL Server convention)
    let datasource = format!("{},{}", context.server_name, context.port);
    let tds_client = provider
        .create_client(client_context.clone(), &datasource, None)
        .await;

    if tds_client.is_err() {
        return Err(napi::Error::from_reason(format!(
            "Failed to Connect to SQL Server: {}",
            tds_client.err().unwrap()
        )));
    }

    match tds_client {
        Ok(client) => {
            let collation = CollationMetadata::from(client.get_collation());
            let connection = Connection {
                tds_client: Arc::new(Mutex::new(client)),
                collation: Some(collation),
            };
            Ok(connection)
        }
        Err(e) => {
            // Handle the error
            Err(napi::Error::from_reason(format!(
                "Failed to create TdsClient: {e}"
            )))
        }
    }
}
