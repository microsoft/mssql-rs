// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.
// Portions derived from saurabh500/mssql-tiberius-bridge (MIT).

//! TDS 8.0 Strict encryption integration tests (issue #62).
//!
//! Strict mode performs the TLS handshake **before** the TDS pre-login packet,
//! using ALPN with the `tds/8.0` protocol id. The cipher then wraps the entire
//! TDS conversation, including pre-login.
//!
//! These tests use the shared live-test environment (`tests/common/mod.rs`),
//! including `TRUST_SERVER_CERTIFICATE` and `CERT_HOST_NAME`, matching
//! `mssql-tds`'s Strict tests. Tests are skipped when no password is available
//! or the environment relies on `TRUST_SERVER_CERTIFICATE=true`.

pub mod common;

#[path = "common/dotenv_test.rs"]
mod dotenv_test;

use mssql_rs::{Client, Config, EncryptionLevel};

fn strict_config() -> Option<Config> {
    let Some(mut cfg) = common::live_config() else {
        eprintln!("skipping Strict tests: SQL_PASSWORD not set");
        return None;
    };
    if common::trust_server_certificate() {
        eprintln!("skipping Strict tests: TRUST_SERVER_CERTIFICATE is ignored in Strict mode");
        return None;
    }
    cfg.encryption(EncryptionLevel::Strict);
    Some(cfg)
}

#[test]
fn strict_config_loads_dotenv_before_trust_check() {
    if dotenv_test::in_child() {
        assert!(strict_config().is_none());
        assert!(common::trust_server_certificate());
        assert!(std::env::var_os("SQL_PASSWORD").is_some());
    } else {
        let password = uuid::Uuid::new_v4();
        dotenv_test::run(
            "strict_config_loads_dotenv_before_trust_check",
            &format!("SQL_PASSWORD={password}\nTRUST_SERVER_CERTIFICATE=true\n"),
            &[],
        );
    }
}

#[tokio::test]
async fn strict_select_one() {
    let Some(cfg) = strict_config() else {
        return;
    };

    let mut client = Client::connect(&cfg).await.expect("Strict connect failed");
    let rows = client
        .simple_query("SELECT 1 AS value")
        .await
        .expect("query failed")
        .into_first_result();

    assert_eq!(rows.len(), 1);
    assert_eq!(
        rows.first()
            .expect("expected row at index 0")
            .get::<i32, _>("value"),
        Some(1)
    );
}

#[tokio::test]
async fn strict_reports_encrypted_session() {
    // Asks the server whether the current session is encrypted. Under Strict
    // mode the value must be 'TRUE'.
    let Some(cfg) = strict_config() else {
        return;
    };

    let mut client = Client::connect(&cfg).await.expect("Strict connect failed");
    let rows = client
        .simple_query(
            "SELECT CAST(encrypt_option AS varchar(10)) AS encrypted \
             FROM sys.dm_exec_connections \
             WHERE session_id = @@SPID",
        )
        .await
        .expect("query failed")
        .into_first_result();

    let encrypted: String = rows
        .first()
        .expect("expected row at index 0")
        .get("encrypted")
        .expect("encrypt_option column missing");
    assert_eq!(
        encrypted.to_uppercase(),
        "TRUE",
        "Strict mode must yield an encrypted session, got {encrypted:?}"
    );
}

#[tokio::test]
async fn strict_ignores_trust_cert_flag() {
    // Even when the user calls `trust_cert()`, mssql-tds enforces real cert
    // validation under Strict. Against a properly-configured Strict endpoint
    // this should still succeed (because the real cert is valid).
    let Some(mut cfg) = strict_config() else {
        return;
    };
    cfg.trust_cert();

    let mut client = Client::connect(&cfg)
        .await
        .expect("Strict connect with trust_cert() should still succeed against a valid cert");
    let rows = client
        .simple_query("SELECT 1 AS value")
        .await
        .expect("query failed")
        .into_first_result();
    assert_eq!(
        rows.first()
            .expect("expected row at index 0")
            .get::<i32, _>("value"),
        Some(1)
    );
}
