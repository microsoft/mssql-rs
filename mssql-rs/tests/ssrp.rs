// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.
// Portions derived from saurabh500/mssql-tiberius-bridge (MIT).

//! SSRP / SQL Browser instance lookup integration test (issue #14).
//!
//! Windows-only and gated on `SSPI_TEST`, matching
//! `mssql-tds/tests/test_ssrp_local.rs`. Requires SQL Browser and a local
//! named instance reachable with integrated authentication. The instance name
//! comes from `SSPI_NAMED_INSTANCE` (default: `sqldev`), which CI provisions in
//! `.pipeline/templates/sql-setup-template.yml`.

#![cfg(windows)]

use mssql_rs::{AuthMethod, Client, Config};

#[tokio::test]
async fn ssrp_named_instance_connect() {
    if std::env::var("SSPI_TEST").is_err() {
        eprintln!("SSPI_TEST not set, skipping");
        return;
    }
    let instance = std::env::var("SSPI_NAMED_INSTANCE").unwrap_or_else(|_| "sqldev".into());

    let mut cfg = Config::new();
    cfg.host("localhost")
        .instance_name(instance)
        .database("master")
        .authentication(AuthMethod::integrated())
        .trust_cert();

    let mut client = Client::connect(&cfg)
        .await
        .expect("SSRP-resolved connect failed");
    let row = client
        .query("SELECT @@SERVERNAME", &[])
        .await
        .expect("query succeeds: SELECT @@SERVERNAME")
        .into_first_result();
    let server_name = row
        .first()
        .expect("expected row at index 0")
        .get::<&str, _>(0usize);
    assert!(server_name.is_some());
    eprintln!("SSRP-resolved @@SERVERNAME: {server_name:?}");
}
