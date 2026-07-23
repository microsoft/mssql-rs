// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! TLS handshake integration tests.
//!
//! Verifies that TLS connections to SQL Server succeed across encryption
//! settings (On, Required, Optional). Requires a live SQL Server instance.
//!
//! Run:
//!   ENABLE_TRACE=true cargo test -p mssql-tds --test test_tls_handshake -- --nocapture

mod common;

use common::{build_tcp_datasource, create_context, init_tracing};
use mssql_tds::{
    connection::client_context::ClientContext,
    connection::tds_client::ResultSet,
    connection_provider::tds_connection_provider::TdsConnectionProvider,
    core::{EncryptionOptions, EncryptionSetting},
};
use std::env;

fn sql_auth_context(trust_cert: bool, encryption: EncryptionSetting) -> ClientContext {
    dotenv::dotenv().ok();
    init_tracing();
    let mut ctx = ClientContext::default();
    ctx.user_name = env::var("DB_USERNAME").unwrap_or("sa".into());
    ctx.password = env::var("SQL_PASSWORD")
        .or_else(|_| {
            std::fs::read_to_string("/tmp/password")
                .map(|s| s.trim().to_string())
                .map_err(|_| env::VarError::NotPresent)
        })
        .expect("SQL_PASSWORD not set");
    ctx.database = "master".into();
    ctx.encryption_options = EncryptionOptions {
        mode: encryption,
        trust_server_certificate: trust_cert,
        host_name_in_cert: None,
        server_certificate: None,
    };
    ctx
}

/// Baseline: connect with TrustServerCertificate=true + Encrypt=On.
/// This is what the pipeline does. Fails on macOS with error -9806.
#[tokio::test]
async fn trust_cert_encrypt_on() {
    let ctx = sql_auth_context(true, EncryptionSetting::On);
    let datasource = build_tcp_datasource();
    let provider = TdsConnectionProvider {};
    let result = provider.create_client(ctx, &datasource, None).await;
    match &result {
        Ok(_) => println!("trust_cert_encrypt_on: CONNECTED OK"),
        Err(e) => println!("trust_cert_encrypt_on: FAILED: {e}"),
    }
    assert!(
        result.is_ok(),
        "Expected connection to succeed: {:?}",
        result.err()
    );
}

/// Same but with EncryptionSetting::Required (equivalent to Mandatory).
#[tokio::test]
async fn trust_cert_encrypt_required() {
    let ctx = sql_auth_context(true, EncryptionSetting::Required);
    let datasource = build_tcp_datasource();
    let provider = TdsConnectionProvider {};
    let result = provider.create_client(ctx, &datasource, None).await;
    match &result {
        Ok(_) => println!("trust_cert_encrypt_required: CONNECTED OK"),
        Err(e) => println!("trust_cert_encrypt_required: FAILED: {e}"),
    }
    assert!(
        result.is_ok(),
        "Expected connection to succeed: {:?}",
        result.err()
    );
}

/// TrustServerCertificate=true + PreferOff (Encrypt=Optional).
/// SQL Server may still require TLS for login — tests whether the
/// prelogin negotiation + LoginOnly TLS path works.
#[tokio::test]
async fn trust_cert_encrypt_optional() {
    let ctx = sql_auth_context(true, EncryptionSetting::PreferOff);
    let datasource = build_tcp_datasource();
    let provider = TdsConnectionProvider {};
    let result = provider.create_client(ctx, &datasource, None).await;
    match &result {
        Ok(_) => println!("trust_cert_encrypt_optional: CONNECTED OK"),
        Err(e) => println!("trust_cert_encrypt_optional: FAILED: {e}"),
    }
    assert!(
        result.is_ok(),
        "Expected connection to succeed: {:?}",
        result.err()
    );
}

/// TrustServerCertificate=false + PreferOff (Encrypt=Optional).
/// When the server doesn't force encryption, the negotiated mode is LoginOnly.
/// ODBC skips cert validation for LoginOnly regardless of TrustServerCertificate.
/// This test verifies the Rust driver matches that behavior: LoginOnly connections
/// should succeed even with TrustServerCertificate=false against self-signed certs.
#[tokio::test]
async fn no_trust_cert_encrypt_optional() {
    let ctx = sql_auth_context(false, EncryptionSetting::PreferOff);
    let datasource = build_tcp_datasource();
    let provider = TdsConnectionProvider {};
    let result = provider.create_client(ctx, &datasource, None).await;
    match &result {
        Ok(_) => println!("no_trust_cert_encrypt_optional: CONNECTED OK"),
        Err(e) => println!("no_trust_cert_encrypt_optional: FAILED: {e}"),
    }
    assert!(
        result.is_ok(),
        "Expected connection to succeed: {:?}",
        result.err()
    );
}

/// Use the standard create_context() helper (reads TRUST_SERVER_CERTIFICATE env).
/// This mirrors the common test path and should work with TRUST_SERVER_CERTIFICATE=true.
#[tokio::test]
async fn standard_create_context_connect() {
    init_tracing();
    let ctx = create_context();
    let datasource = build_tcp_datasource();
    let provider = TdsConnectionProvider {};
    let result = provider.create_client(ctx, &datasource, None).await;
    match &result {
        Ok(_) => println!("standard_create_context_connect: CONNECTED OK"),
        Err(e) => println!("standard_create_context_connect: FAILED: {e}"),
    }
    assert!(
        result.is_ok(),
        "Expected connection to succeed: {:?}",
        result.err()
    );
}

/// After connecting, run SELECT 1 to verify the connection is actually usable.
#[tokio::test]
async fn connect_and_query() {
    let ctx = sql_auth_context(true, EncryptionSetting::On);
    let datasource = build_tcp_datasource();
    let provider = TdsConnectionProvider {};
    let mut client = provider
        .create_client(ctx, &datasource, None)
        .await
        .expect("Connection failed");

    client
        .execute("SELECT 1 AS val".to_string(), ())
        .await
        .expect("Query failed");

    if let Some(rs) = client.get_current_resultset() {
        let row = rs.next_row().await.unwrap();
        assert!(row.is_some(), "Expected a result row");
        println!("connect_and_query: SELECT 1 returned a row — connection is usable");
    }
    client.close_query().await.unwrap();
}
