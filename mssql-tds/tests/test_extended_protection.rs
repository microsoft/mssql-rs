// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Integration tests for SQL Server Extended Protection for Authentication
//! (EPA) channel binding (`tls-unique`, RFC 5929 §3) on Windows.
//!
//! These tests require a SQL Server instance configured with **Extended
//! Protection = Required** and **Force Encryption = Yes**, reachable via
//! integrated auth (NTLM/Kerberos). They are gated behind `EPA_TEST=1` so they
//! are skipped during the normal test run (where the instance does NOT enforce
//! EPA, so the negative assertion would not hold).
//!
//! The validation pipeline configures EPA on the local instance via
//! `.pipeline/scripts/Configure-ExtendedProtection.ps1`, runs these tests with
//! `EPA_TEST=1`, then reverts EPA -- see the "Extended Protection" step in
//! `.pipeline/templates/validation-stages.yml`.
//!
//! Test isolation note: nextest runs each test in its own process, so the
//! per-process environment manipulation below does not leak across tests.

#![cfg(windows)]

use std::env;

mod common;

use mssql_tds::connection::client_context::{ClientContext, TdsAuthenticationMethod};
use mssql_tds::connection::tds_client::{ResultSet, ResultSetClient};
use mssql_tds::connection_provider::tds_connection_provider::TdsConnectionProvider;
use mssql_tds::core::{EncryptionOptions, EncryptionSetting, TdsResult};

/// Diagnostic env var honored by the Schannel-direct engine to suppress the
/// channel binding token (mirrors native SNI `m_fIgnoreChannelBindings`).
const SUPPRESS_ENV: &str = "MSSQL_TDS_IGNORE_CHANNEL_BINDINGS";

fn epa_enabled() -> bool {
    env::var("EPA_TEST").is_ok()
}

/// Integrated-auth context over a Mandatory-encrypted connection (the path that
/// triggers `tls-unique` extraction on the Windows Schannel-direct engine).
fn integrated_encrypted_context() -> ClientContext {
    let mut context = ClientContext::default();
    context.database = "master".to_string();
    context.tds_authentication_method = TdsAuthenticationMethod::SSPI;
    context.encryption_options = EncryptionOptions {
        mode: EncryptionSetting::On,
        trust_server_certificate: true,
        host_name_in_cert: None,
        server_certificate: None,
    };
    context
}

/// Positive: with EPA = Required on the server, an integrated-auth encrypted
/// connection that carries a valid `tls-unique` channel binding token logs in
/// successfully (the binding the server expects is the one we sent).
#[tokio::test]
async fn epa_required_accepts_valid_channel_binding() -> TdsResult<()> {
    if !epa_enabled() {
        println!("Skipping EPA test - set EPA_TEST=1 (requires server EPA=Required)");
        return Ok(());
    }
    common::init_tracing();

    // Ensure the token is NOT suppressed so a real CBT is sent.
    // SAFETY: nextest runs this test in its own single-threaded-by-default
    // process; no other thread reads the environment concurrently.
    unsafe {
        env::remove_var(SUPPRESS_ENV);
    }

    let provider = TdsConnectionProvider {};
    let mut connection = provider
        .create_client(integrated_encrypted_context(), "tcp:localhost,1433", None)
        .await
        .expect("EPA=Required should ACCEPT a login that sends a valid channel binding token");

    // Confirm we authenticated over an encrypted Windows-auth connection.
    let query = "SELECT auth_scheme, CAST(encrypt_option AS varchar(10)) \
                 FROM sys.dm_exec_connections WHERE session_id = @@SPID";
    connection.execute(query.to_string(), None, None).await?;
    if let Some(rs) = connection.get_current_resultset()
        && let Some(row) = rs.next_row().await?
    {
        let scheme = format!("{:?}", row.first());
        assert!(
            scheme.contains("NTLM") || scheme.contains("Kerberos"),
            "expected a Windows auth scheme (NTLM/Kerberos), got {scheme}"
        );
    }
    connection.close_query().await?;
    Ok(())
}

/// Negative (enforcement proof): with EPA = Required, suppressing the channel
/// binding token must cause the server to REJECT the login. This is the test
/// that proves we send a real, server-validated binding -- not just that login
/// happens to work.
#[tokio::test]
async fn epa_required_rejects_suppressed_channel_binding() {
    if !epa_enabled() {
        println!("Skipping EPA test - set EPA_TEST=1 (requires server EPA=Required)");
        return;
    }
    common::init_tracing();

    // Suppress the tls-unique token for this process only, so the SSPI exchange
    // proceeds WITHOUT a channel binding.
    // SAFETY: nextest runs this test in its own process; no other thread reads
    // the environment concurrently.
    unsafe {
        env::set_var(SUPPRESS_ENV, "1");
    }

    let provider = TdsConnectionProvider {};
    let result = provider
        .create_client(integrated_encrypted_context(), "tcp:localhost,1433", None)
        .await;

    // Restore the environment regardless of outcome.
    // SAFETY: see above.
    unsafe {
        env::remove_var(SUPPRESS_ENV);
    }

    assert!(
        result.is_err(),
        "EPA=Required must REJECT a login whose channel binding was suppressed, \
         but the connection succeeded -- the server is not enforcing channel binding, \
         or the client failed to omit the token"
    );
}
