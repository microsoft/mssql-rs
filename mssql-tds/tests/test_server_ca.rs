// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Tests for custom trust roots (`TrustRoots::Custom` / `PlatformAndCustom`).
//!
//! The mock server presents a leaf certificate issued by a private test CA.
//! The client trusts the CA itself, so chain building, host name verification
//! and expiry checks all stay enabled.
//!
//! **Prerequisites:** generate the test certificates first:
//! ```bash
//! ./scripts/generate_mock_tds_server_certs.sh
//! ```
//! On Windows, use `.\scripts\generate_mock_tds_server_certs.ps1`.

use mssql_mock_tds::MockTdsServer;
#[cfg(not(windows))]
use mssql_mock_tds::create_test_identity;
use mssql_tds::connection::client_context::ClientContext;
use mssql_tds::connection_provider::tds_connection_provider::TdsConnectionProvider;
use mssql_tds::core::{CertificateSource, EncryptionOptions, ServerTrust, TrustRoots};
#[cfg(not(windows))]
use std::fs;
use tokio::sync::oneshot;

const CA_CERT: &str = "tests/test_certificates/ca_cert.pem";
const UNRELATED_CA_CERT: &str = "tests/test_certificates/unrelated_ca_cert.pem";
struct IdentityFiles {
    #[cfg(not(windows))]
    cert: &'static str,
    #[cfg(not(windows))]
    key: &'static str,
    #[cfg(windows)]
    pfx: &'static str,
}

const CA_SIGNED_IDENTITY: IdentityFiles = IdentityFiles {
    #[cfg(not(windows))]
    cert: "tests/test_certificates/ca_signed_cert.pem",
    #[cfg(not(windows))]
    key: "tests/test_certificates/ca_signed_key.pem",
    #[cfg(windows)]
    pfx: "tests/test_certificates/ca_signed_identity.pfx",
};

const SELF_SIGNED_IDENTITY: IdentityFiles = IdentityFiles {
    #[cfg(not(windows))]
    cert: "tests/test_certificates/valid_cert.pem",
    #[cfg(not(windows))]
    key: "tests/test_certificates/key.pem",
    #[cfg(windows)]
    pfx: "tests/test_certificates/identity.pfx",
};

fn load_identity(
    files: &IdentityFiles,
) -> Result<native_tls::Identity, Box<dyn std::error::Error>> {
    #[cfg(windows)]
    {
        mssql_mock_tds::load_identity_from_file(files.pfx, "")
    }
    #[cfg(not(windows))]
    {
        let cert_pem = fs::read(files.cert)?;
        let key_pem = fs::read(files.key)?;
        create_test_identity(&cert_pem, &key_pem)
    }
}

fn trusting(roots: TrustRoots) -> EncryptionOptions {
    EncryptionOptions::new().with_server_trust(ServerTrust::verify(roots))
}

fn ca_file(path: &str) -> CertificateSource {
    CertificateSource::File(path.into())
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
    for roots in [
        TrustRoots::PlatformAndCustom(ca_file(CA_CERT)),
        TrustRoots::Custom(ca_file(CA_CERT)),
        TrustRoots::Custom(CertificateSource::Pem(std::fs::read(CA_CERT)?)),
    ] {
        let identity = load_identity(&CA_SIGNED_IDENTITY)?;
        assert!(
            connect_to_mock_server(identity, trusting(roots.clone())).await?,
            "connection should succeed when the issuing CA is trusted via {roots:?}"
        );
    }
    Ok(())
}

#[tokio::test]
async fn ca_signed_certificate_is_rejected_for_unrelated_ca()
-> Result<(), Box<dyn std::error::Error>> {
    for roots in [
        TrustRoots::PlatformAndCustom(ca_file(UNRELATED_CA_CERT)),
        TrustRoots::Custom(ca_file(UNRELATED_CA_CERT)),
    ] {
        let identity = load_identity(&CA_SIGNED_IDENTITY)?;
        assert!(
            !connect_to_mock_server(identity, trusting(roots.clone())).await?,
            "connection should fail when {roots:?} did not issue the server certificate"
        );
    }
    Ok(())
}

#[tokio::test]
async fn trusted_ca_still_enforces_host_name_validation() -> Result<(), Box<dyn std::error::Error>>
{
    let identity = load_identity(&CA_SIGNED_IDENTITY)?;
    let options = EncryptionOptions::new().with_server_trust(ServerTrust::Verify {
        roots: TrustRoots::Custom(ca_file(CA_CERT)),
        host_name: Some("wrong.hostname.example.com".to_string()),
    });

    assert!(
        !connect_to_mock_server(identity, options).await?,
        "connection should fail when the certificate does not match the expected host name"
    );
    Ok(())
}

#[tokio::test]
async fn trusted_ca_does_not_trust_unrelated_self_signed_certificate()
-> Result<(), Box<dyn std::error::Error>> {
    let identity = load_identity(&SELF_SIGNED_IDENTITY)?;

    assert!(
        !connect_to_mock_server(
            identity,
            trusting(TrustRoots::PlatformAndCustom(ca_file(CA_CERT)))
        )
        .await?,
        "a custom CA must not make unrelated certificates acceptable"
    );
    Ok(())
}

#[tokio::test]
async fn custom_trust_root_does_not_leak_to_other_connections()
-> Result<(), Box<dyn std::error::Error>> {
    let identity = load_identity(&CA_SIGNED_IDENTITY)?;
    assert!(
        connect_to_mock_server(
            identity,
            trusting(TrustRoots::PlatformAndCustom(ca_file(CA_CERT)))
        )
        .await?,
        "connection with a custom CA should succeed first"
    );

    let identity = load_identity(&CA_SIGNED_IDENTITY)?;
    assert!(
        !connect_to_mock_server(identity, EncryptionOptions::new()).await?,
        "a connection with platform roots must not inherit the custom trust root"
    );
    Ok(())
}
