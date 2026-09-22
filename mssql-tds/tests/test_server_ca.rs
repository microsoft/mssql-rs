// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Tests for the ServerCA option (custom trust roots).
//!
//! The mock server presents a leaf certificate issued by a private test CA.
//! The client trusts the CA itself, so chain building, host name verification
//! and expiry checks all stay enabled.
//!
//! **Prerequisites:** generate the test certificates first:
//! ```bash
//! ./scripts/generate_mock_tds_server_certs.sh
//! ```

// The mock server identity is built from PEM files, which is only wired up for
// non-Windows targets (see tests/test_mock_server_tls.rs).
#![cfg(not(windows))]

use mssql_mock_tds::{MockTdsServer, create_test_identity};
use mssql_tds::connection::client_context::ClientContext;
use mssql_tds::connection_provider::tds_connection_provider::TdsConnectionProvider;
use mssql_tds::core::{EncryptionOptions, EncryptionSetting};
use std::fs;
use std::path::Path;
use tokio::sync::oneshot;

const CA_CERT: &str = "tests/test_certificates/ca_cert.pem";
const UNRELATED_CA_CERT: &str = "tests/test_certificates/unrelated_ca_cert.pem";
const CA_SIGNED_CERT: &str = "tests/test_certificates/ca_signed_cert.pem";
const CA_SIGNED_KEY: &str = "tests/test_certificates/ca_signed_key.pem";
const SELF_SIGNED_CERT: &str = "tests/test_certificates/valid_cert.pem";
const SELF_SIGNED_KEY: &str = "tests/test_certificates/key.pem";

/// Returns `None` when the certificates have not been generated yet, so the
/// test can skip instead of failing on a developer machine.
fn load_identity(cert: &str, key: &str) -> Option<native_tls::Identity> {
    if !Path::new(cert).exists() || !Path::new(key).exists() {
        eprintln!(
            "Skipping: {cert} / {key} not found. Run ./scripts/generate_mock_tds_server_certs.sh"
        );
        return None;
    }
    let cert_pem = fs::read(cert).ok()?;
    let key_pem = fs::read(key).ok()?;
    create_test_identity(&cert_pem, &key_pem).ok()
}

fn encryption_options(
    server_ca: Option<&str>,
    host_name_in_cert: Option<&str>,
) -> EncryptionOptions {
    EncryptionOptions {
        mode: EncryptionSetting::Strict,
        trust_server_certificate: false,
        host_name_in_cert: host_name_in_cert.map(str::to_string),
        server_certificate: None,
        server_ca: server_ca.map(Into::into),
    }
}

/// Starts a strict-TLS mock server with `identity` and tries to connect with
/// `options`. Returns whether the connection succeeded.
async fn connect_to_mock_server(
    identity: native_tls::Identity,
    options: EncryptionOptions,
) -> Result<bool, Box<dyn std::error::Error>> {
    let server = MockTdsServer::new_with_strict_tls("127.0.0.1:0", identity).await?;
    let server_addr = server.local_addr();

    let (shutdown_tx, shutdown_rx) = oneshot::channel();
    let server_handle = tokio::spawn(async move { server.run_with_shutdown(shutdown_rx).await });

    tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

    let datasource = format!("tcp:{},{}", server_addr.ip(), server_addr.port());
    let mut context = ClientContext::default();
    context.user_name = "sa".to_string();
    context.database = "master".to_string();
    context.encryption_options = options;

    let provider = TdsConnectionProvider {};
    let result = provider.create_client(context, &datasource, None).await;
    let connected = result.is_ok();
    drop(result);

    let _ = shutdown_tx.send(());
    let _ = tokio::time::timeout(tokio::time::Duration::from_secs(2), server_handle).await;

    Ok(connected)
}

#[tokio::test]
async fn ca_signed_certificate_is_accepted_when_ca_is_trusted()
-> Result<(), Box<dyn std::error::Error>> {
    let Some(identity) = load_identity(CA_SIGNED_CERT, CA_SIGNED_KEY) else {
        return Ok(());
    };

    assert!(
        connect_to_mock_server(identity, encryption_options(Some(CA_CERT), None)).await?,
        "connection should succeed when the issuing CA is supplied through ServerCA"
    );
    Ok(())
}

#[tokio::test]
async fn ca_signed_certificate_is_rejected_for_unrelated_ca()
-> Result<(), Box<dyn std::error::Error>> {
    let Some(identity) = load_identity(CA_SIGNED_CERT, CA_SIGNED_KEY) else {
        return Ok(());
    };

    assert!(
        !connect_to_mock_server(identity, encryption_options(Some(UNRELATED_CA_CERT), None))
            .await?,
        "connection should fail when the supplied CA did not issue the server certificate"
    );
    Ok(())
}

#[tokio::test]
async fn trusted_ca_still_enforces_host_name_validation() -> Result<(), Box<dyn std::error::Error>>
{
    let Some(identity) = load_identity(CA_SIGNED_CERT, CA_SIGNED_KEY) else {
        return Ok(());
    };

    assert!(
        !connect_to_mock_server(
            identity,
            encryption_options(Some(CA_CERT), Some("wrong.hostname.example.com"))
        )
        .await?,
        "connection should fail when the certificate does not match the expected host name"
    );
    Ok(())
}

#[tokio::test]
async fn trusted_ca_does_not_trust_unrelated_self_signed_certificate()
-> Result<(), Box<dyn std::error::Error>> {
    let Some(identity) = load_identity(SELF_SIGNED_CERT, SELF_SIGNED_KEY) else {
        return Ok(());
    };

    assert!(
        !connect_to_mock_server(identity, encryption_options(Some(CA_CERT), None)).await?,
        "ServerCA must not make unrelated certificates acceptable"
    );
    Ok(())
}

#[tokio::test]
async fn custom_trust_root_does_not_leak_to_other_connections()
-> Result<(), Box<dyn std::error::Error>> {
    let Some(identity) = load_identity(CA_SIGNED_CERT, CA_SIGNED_KEY) else {
        return Ok(());
    };
    assert!(
        connect_to_mock_server(identity, encryption_options(Some(CA_CERT), None)).await?,
        "connection with ServerCA should succeed first"
    );

    let identity = load_identity(CA_SIGNED_CERT, CA_SIGNED_KEY).expect("identity should reload");
    assert!(
        !connect_to_mock_server(identity, encryption_options(None, None)).await?,
        "a connection without ServerCA must not inherit the custom trust root"
    );
    Ok(())
}
