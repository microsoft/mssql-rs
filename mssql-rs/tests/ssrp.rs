// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.
// Portions derived from saurabh500/mssql-tiberius-bridge (MIT).

//! SSRP / SQL Browser instance lookup integration test (issue #14).
//!
//! Skipped unless `BRIDGE_SSRP_HOST` and `BRIDGE_SSRP_INSTANCE` are set,
//! because the standard MSSQL Docker images don't run the SQL Browser
//! service (UDP 1434).
//!
//! Set `SQL_PASSWORD` before running:
//!     BRIDGE_SSRP_HOST=mssql.lab \
//!     BRIDGE_SSRP_INSTANCE=SQLEXPRESS \
//!     DB_USERNAME=sa \
//!     cargo nextest run -p mssql-rs --test ssrp

pub mod common;

use mssql_rs::{AuthMethod, Client, Config};

#[tokio::test]
async fn ssrp_named_instance_connect() {
    let Ok(host) = std::env::var("BRIDGE_SSRP_HOST") else {
        eprintln!("BRIDGE_SSRP_HOST not set, skipping");
        return;
    };
    let Ok(instance) = std::env::var("BRIDGE_SSRP_INSTANCE") else {
        eprintln!("BRIDGE_SSRP_INSTANCE not set, skipping");
        return;
    };
    let password = common::password().expect("SQL_PASSWORD required");

    let mut cfg = Config::new();
    cfg.host(host)
        .instance_name(instance)
        .database(common::database())
        .authentication(AuthMethod::sql_server(common::user_name(), password));
    common::apply_certificate_settings(&mut cfg);

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
