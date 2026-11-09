// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.
// Portions derived from saurabh500/mssql-tiberius-bridge (MIT).

//! AAD/Entra ID federated auth integration test (issue #17).
//!
//! Gated on `ACCESS_TOKEN` (a JWT for `https://database.windows.net/.default`,
//! matching the `mssql-py-core` access-token tests) plus the shared live-test
//! `DB_HOST`, `DB_PORT`, and `DB_DATABASE` pointing at an AAD-enabled SQL
//! Server (Azure SQL DB or Managed Instance).

pub mod common;

use mssql_rs::{AuthMethod, Client, Config, EncryptionLevel};

#[tokio::test]
async fn aad_token_login() {
    let Ok(token) = std::env::var("ACCESS_TOKEN") else {
        eprintln!("ACCESS_TOKEN not set, skipping");
        return;
    };
    let host = std::env::var("DB_HOST").unwrap_or_else(|_| "localhost".into());
    let port: u16 = std::env::var("DB_PORT")
        .map(|port| port.parse().expect("DB_PORT must be a valid port"))
        .unwrap_or(1433);

    let mut cfg = Config::new();
    cfg.host(host)
        .port(port)
        .database(common::database())
        .encryption(EncryptionLevel::Required)
        .authentication(AuthMethod::aad_token(token));

    let mut client = Client::connect(&cfg).await.expect("AAD login failed");
    let row = client
        .query("SELECT SUSER_SNAME()", &[])
        .await
        .expect("query succeeds: SELECT SUSER_SNAME()")
        .into_first_result();
    let sname = row
        .first()
        .expect("expected row at index 0")
        .get::<&str, _>(0usize);
    assert!(sname.is_some(), "SUSER_SNAME() returned NULL");
    eprintln!("AAD login as: {sname:?}");
}
