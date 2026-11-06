// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.
// Portions derived from saurabh500/mssql-tiberius-bridge (MIT).

//! Integration tests for `Config::trust_cert_ca` (issue #18).
//!
//! Mirrors `tiberius/tests/custom-cert.rs`. The positive test
//! (`connect_with_trusted_ca`) requires a PEM/DER file matching the SQL
//! Server's TLS certificate and is run only when `BRIDGE_CUSTOM_CA_PATH`
//! is set.

pub mod common;

use mssql_rs::{Client, Config, EncryptionLevel};

fn base_config() -> Config {
    common::base_config().unwrap_or_else(|| {
        eprintln!("SQL_PASSWORD not set, skipping");
        std::process::exit(0);
    })
}

/// Negative case: encryption required, no trust override, and a certificate
/// host name the server cannot match ⇒ connect fails. Uses a name mismatch
/// rather than an untrusted CA because CI installs the test CA system-wide.
#[tokio::test]
async fn connect_with_unvalidated_certificate_fails() {
    let mut cfg = base_config();
    cfg.encryption(EncryptionLevel::On)
        .host_name_in_certificate("mssql-rs-cert-mismatch.invalid");
    let result = Client::connect(&cfg).await;
    result
        .map(|_| ())
        .expect_err("expected TLS validation to fail for a mismatched certificate host name");
}

/// Positive case: pin the CA cert and connect successfully.
/// Set `BRIDGE_CUSTOM_CA_PATH` to a PEM/DER file that matches the
/// server's TLS certificate. Mirrors tiberius `connect_to_custom_cert_instance_*`.
#[tokio::test]
async fn connect_with_trusted_ca() {
    let Ok(ca_path) = std::env::var("BRIDGE_CUSTOM_CA_PATH") else {
        eprintln!("BRIDGE_CUSTOM_CA_PATH not set, skipping");
        return;
    };
    let mut cfg = base_config();
    cfg.encryption(EncryptionLevel::On).trust_cert_ca(&ca_path);

    let mut client = Client::connect(&cfg)
        .await
        .expect("connect with pinned CA failed");

    let row = client
        .query("SELECT @P1", &[&-4i32])
        .await
        .expect("query failed")
        .into_first_result();
    assert_eq!(
        row.first()
            .expect("expected row at index 0")
            .get::<i32, _>(0usize),
        Some(-4)
    );
}
