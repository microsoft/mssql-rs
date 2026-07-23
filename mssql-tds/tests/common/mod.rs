// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#[cfg(test)]
use std::default::Default;
use std::env;
use std::sync::Once;

use dotenv::dotenv;
use mssql_tds::connection::tds_client::{ResultSet, TdsClient};
use mssql_tds::core::{EncryptionOptions, TdsResult};
use mssql_tds::datatypes::column_values::ColumnValues;
use mssql_tds::query::metadata::ColumnMetadata;
use mssql_tds::{
    connection::client_context::ClientContext,
    connection_provider::tds_connection_provider::TdsConnectionProvider, core::EncryptionSetting,
};
use tracing::Level;
use tracing_subscriber::FmtSubscriber;
// This module will have a lot of allow(dead_code). This is because some of the capabilities are being used only in
// some tests. Each test leads to a new binary being created. So if a test is not using parts of Common module, the functions in common
// will end up being analyzed as dead code. So we will allow dead code in this module.

#[allow(dead_code)]
static INIT: Once = Once::new();

#[allow(dead_code)]
pub fn init_tracing() {
    dotenv().ok();
    let enable_trace = env::var("ENABLE_TEST_TRACE")
        .unwrap_or_else(|_| "false".to_string())
        .parse::<bool>()
        .unwrap();
    if enable_trace {
        INIT.call_once(|| {
            // Initialize the global tracing subscriber for standalone Rust tests
            let subscriber = FmtSubscriber::builder()
                .with_max_level(Level::TRACE)
                .finish();
            tracing::subscriber::set_global_default(subscriber)
                .expect("Setting default subscriber failed");
        });
    }
}

#[allow(dead_code)]
pub(crate) enum ExpectedQueryResultType {
    Update(u64),
    Result(u64),
}

pub fn create_context() -> ClientContext {
    dotenv().ok();
    let mut context = ClientContext::default();
    context.user_name = env::var("DB_USERNAME").expect("DB_USERNAME environment variable not set");
    context.password = env::var("SQL_PASSWORD")
        .or_else(|_| {
            std::fs::read_to_string("/tmp/password")
                .map(|s| s.trim().to_string())
                .map_err(|_| std::env::VarError::NotPresent)
        })
        .expect("SQL_PASSWORD environment variable not set and /tmp/password could not be read");
    context.database = "master".to_string();
    context.encryption_options = EncryptionOptions {
        mode: EncryptionSetting::On,
        trust_server_certificate: trust_server_certificate(),
        host_name_in_cert: env::var("CERT_HOST_NAME").ok(),
        server_certificate: None,
    };
    context
}

/// Build datasource string for TCP connection from environment
#[allow(dead_code)]
pub fn build_tcp_datasource() -> String {
    dotenv().ok();
    let host = env::var("DB_HOST").expect("DB_HOST environment variable not set");
    let port = env::var("DB_PORT")
        .ok()
        .map(|v| v.parse::<u16>().expect("DB_PORT must be a valid u16"))
        .unwrap_or(1433);

    // Always use explicit TCP protocol prefix to avoid Named Pipes fallback
    format!("tcp:{},{}", host, port)
}

/// Build datasource string for TCP connection with explicit protocol prefix
#[allow(dead_code)]
pub fn build_tcp_datasource_explicit() -> String {
    dotenv().ok();
    let host = env::var("DB_HOST").expect("DB_HOST environment variable not set");
    let port = env::var("DB_PORT")
        .ok()
        .map(|v| v.parse::<u16>().expect("DB_PORT must be a valid u16"))
        .unwrap_or(1433);

    format!("tcp:{},{}", host, port)
}

/// Build datasource string for Named Pipe connection
#[allow(dead_code)]
#[cfg(windows)]
pub fn build_named_pipe_datasource() -> String {
    dotenv().ok();
    let host = env::var("DB_HOST").expect("DB_HOST environment variable not set");
    let instance = env::var("DB_INSTANCE").ok();

    if let Some(inst) = instance {
        if inst.is_empty() || inst.eq_ignore_ascii_case("MSSQLSERVER") {
            format!(r"np:\\{}\pipe\sql\query", host)
        } else {
            format!(r"np:\\{}\pipe\MSSQL${}\sql\query", host, inst)
        }
    } else {
        format!(r"np:\\{}\pipe\sql\query", host)
    }
}

/// Build datasource string for Shared Memory connection
#[allow(dead_code)]
#[cfg(windows)]
pub fn build_shared_memory_datasource() -> String {
    dotenv().ok();
    let instance = env::var("DB_INSTANCE").unwrap_or_else(|_| String::new());

    // Normalize MSSQLSERVER to empty string (default instance)
    if instance.is_empty() || instance.eq_ignore_ascii_case("MSSQLSERVER") {
        "lpc:.".to_string()
    } else {
        format!("lpc:{}", instance)
    }
}

#[allow(dead_code)]
pub async fn create_client(datasource: &str) -> TdsResult<TdsClient> {
    let context = create_context();
    let provider = TdsConnectionProvider {};
    let client = provider.create_client(context, datasource, None).await?;
    Ok(client)
}

#[allow(dead_code)]
pub async fn begin_connection(datasource: &str) -> TdsClient {
    create_client(datasource).await.unwrap()
}

pub async fn validate_results(
    client: &mut TdsClient,
    expected_results: &[ExpectedQueryResultType],
) -> TdsResult<()> {
    let mut expected_index = 0;
    println!("Before looping.");

    let mut has_rows = client.on_rows();
    if !has_rows {
        has_rows = client.advance_to_rows().await?;
    }
    while has_rows {
        println!("Current index {expected_index:?}");
        assert!(expected_index < expected_results.len());

        let expected = &expected_results[expected_index];
        match expected {
            ExpectedQueryResultType::Result(expected_row_count) => {
                let mut actual_rows: u64 = 0;
                println!("Columns: {:?}", client.get_metadata());

                while let Some(row) = client.next_row().await? {
                    print!("Row {actual_rows:?}: ");
                    for cell in row {
                        print!("{cell:?},");
                    }
                    println!();
                    actual_rows += 1;
                }
                assert_eq!(actual_rows, *expected_row_count);
            }
            ExpectedQueryResultType::Update(_expected_row_count) => {
                // For DML statements, we just drain any rows if present
                while client.next_row().await?.is_some() {}
            }
        }
        expected_index += 1;

        has_rows = client.advance_to_rows().await?;
    }

    client.close_query().await?;
    Ok(())
}

pub async fn run_query_and_check_results(
    client: &mut TdsClient,
    query: String,
    expected_results: &[ExpectedQueryResultType],
) {
    client.execute(query, ()).await.unwrap();
    validate_results(client, expected_results).await.unwrap();
}

#[allow(dead_code)]
pub async fn connect_query_and_validate(
    query: String,
    expected_results: &[ExpectedQueryResultType],
) {
    let mut client = create_client(&build_tcp_datasource()).await.unwrap();
    run_query_and_check_results(&mut client, query, expected_results).await;
}

// Returns the first column of the first row of the result set, and drains the resultset.
#[allow(dead_code)]
pub async fn get_scalar_value(client: &mut TdsClient) -> TdsResult<Option<ColumnValues>> {
    let mut result = None;

    let mut has_rows = client.on_rows();
    if !has_rows {
        has_rows = client.advance_to_rows().await?;
    }
    while has_rows {
        if let Some(row) = client.next_row().await?
            && !row.is_empty()
        {
            result = Some(row[0].clone());
            break;
        }

        has_rows = client.advance_to_rows().await?;
    }

    client.close_query().await?;
    Ok(result)
}

// Returns the first row of the result set, and drains the resultset.
#[allow(dead_code)]
pub async fn get_first_row(
    client: &mut TdsClient,
) -> TdsResult<(Vec<ColumnMetadata>, Vec<ColumnValues>)> {
    let mut result: Vec<ColumnValues> = Vec::new();
    let mut metadata: Vec<ColumnMetadata> = Vec::new();

    let mut has_rows = client.on_rows();
    if !has_rows {
        has_rows = client.advance_to_rows().await?;
    }
    while has_rows {
        if metadata.is_empty() {
            metadata = client.get_metadata().clone();
        }

        if let Some(row) = client.next_row().await? {
            result = row;
            break;
        }

        has_rows = client.advance_to_rows().await?;
    }

    client.close_query().await?;
    Ok((metadata, result))
}

pub fn trust_server_certificate() -> bool {
    env::var("TRUST_SERVER_CERTIFICATE")
        .map(|v| v.parse().unwrap_or(false))
        .unwrap_or(false)
}

// Helper functions for creating different transport contexts

/// Create context and datasource for Named Pipe connection
#[allow(dead_code)]
#[cfg(windows)]
pub fn create_named_pipe_context_and_datasource() -> (ClientContext, String) {
    let context = create_context();
    let datasource = build_named_pipe_datasource();
    (context, datasource)
}

/// Create context and datasource for Shared Memory connection
#[allow(dead_code)]
#[cfg(windows)]
pub fn create_shared_memory_context_and_datasource() -> (ClientContext, String) {
    let context = create_context();
    let datasource = build_shared_memory_datasource();
    (context, datasource)
}
