// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use std::env;
use std::sync::Once;

use dotenv::dotenv;
use mssql_tds::connection::client_context::ClientContext;
use mssql_tds::connection::tds_client::TdsClient;
use mssql_tds::connection_provider::tds_connection_provider::TdsConnectionProvider;
use mssql_tds::core::{EncryptionOptions, EncryptionSetting, TdsResult};
use tracing::Level;
use tracing_subscriber::FmtSubscriber;

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
            let subscriber = FmtSubscriber::builder()
                .with_max_level(Level::TRACE)
                .finish();
            tracing::subscriber::set_global_default(subscriber)
                .expect("Setting default subscriber failed");
        });
    }
}

#[allow(dead_code)]
pub fn build_tcp_datasource() -> String {
    dotenv().ok();
    let host = env::var("DB_HOST").expect("DB_HOST environment variable not set");
    let port = env::var("DB_PORT")
        .ok()
        .map(|v| v.parse::<u16>().expect("DB_PORT must be a valid u16"))
        .unwrap_or(1433);
    format!("tcp:{},{}", host, port)
}

#[allow(dead_code)]
fn trust_server_certificate() -> bool {
    env::var("TRUST_SERVER_CERTIFICATE")
        .unwrap_or_else(|_| "false".to_string())
        .parse::<bool>()
        .unwrap_or(false)
}

#[allow(dead_code)]
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

#[allow(dead_code)]
pub async fn create_client(datasource: &str) -> TdsResult<TdsClient> {
    let context = create_context();
    let provider = TdsConnectionProvider {};
    provider.create_client(context, datasource, None).await
}

#[allow(dead_code)]
pub async fn begin_connection(datasource: &str) -> TdsClient {
    create_client(datasource).await.unwrap()
}
