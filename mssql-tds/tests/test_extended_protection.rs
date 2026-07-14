// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Integration test for SQL Server Extended Protection for Authentication
//! (EPA) channel binding (`tls-unique`, RFC 5929 §3) on Windows.
//!
//! The test connects with integrated auth over a Mandatory-encrypted
//! connection (the path that triggers `tls-unique` extraction on the Windows
//! Schannel-direct engine) and asserts the login **succeeds**. It is gated
//! behind `EPA_TEST=1` so it is skipped during the normal test run (which does
//! not guarantee a reachable integrated-auth SQL instance).
//!
//! The validation pipeline runs this test against the local instance under two
//! server configurations and expects success in both:
//!   1. Extended Protection = **Off** (not required) -- the baseline path.
//!   2. Extended Protection = **Required** -- the server rejects any login that
//!      does not present a valid channel binding, so a successful login here is
//!      proof that we send a real, server-validated `tls-unique` token.
//!
//! See the "Extended Protection" step in
//! `.pipeline/templates/validation-stages.yml`, which drives both configurations
//! via `.pipeline/scripts/Configure-ExtendedProtection.ps1`.

#![cfg(windows)]

use std::env;

mod common;

use mssql_tds::connection::client_context::{ClientContext, TdsAuthenticationMethod};
use mssql_tds::connection::tds_client::{ResultSet, ResultSetClient};
use mssql_tds::connection_provider::tds_connection_provider::TdsConnectionProvider;
use mssql_tds::core::{EncryptionOptions, EncryptionSetting, TdsResult};
use mssql_tds::datatypes::column_values::ColumnValues;

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

/// An integrated-auth, encrypted login succeeds regardless of the server's
/// Extended Protection level. The pipeline runs this test with EPA = Off and
/// again with EPA = Required, expecting success both times:
///
/// * EPA = Off -- the connection works when channel binding is not enforced.
/// * EPA = Required -- the server rejects any login lacking a valid channel
///   binding, so a successful login proves the `tls-unique` token we send is
///   real and server-validated.
#[tokio::test]
async fn epa_channel_binding_login_succeeds() -> TdsResult<()> {
    if !epa_enabled() {
        println!("Skipping EPA test - set EPA_TEST=1 (requires an integrated-auth SQL instance)");
        return Ok(());
    }
    common::init_tracing();

    let provider = TdsConnectionProvider {};
    let mut connection = provider
        .create_client(integrated_encrypted_context(), "tcp:localhost,1433", None)
        .await
        .expect(
            "integrated-auth encrypted login should succeed; under EPA=Required this also \
             proves a valid tls-unique channel binding token was sent",
        );

    // Confirm we authenticated over an encrypted Windows-auth connection.
    let query = "SELECT auth_scheme, CAST(encrypt_option AS varchar(10)) \
                 FROM sys.dm_exec_connections WHERE session_id = @@SPID";
    connection.execute(query.to_string(), None, None).await?;
    let rs = connection
        .get_current_resultset()
        .expect("connection-properties query should produce a result set");
    let row = rs
        .next_row()
        .await?
        .expect("sys.dm_exec_connections should return a row for the current session");

    // Extract a column as UTF-8 text regardless of (n)varchar wire encoding.
    // (`ColumnValues`' `Debug` renders a `varchar` as a raw byte array, so a
    // string match on the debug output is unreliable -- decode the value.)
    let column_text = |idx: usize| match row.get(idx) {
        Some(ColumnValues::String(s)) => s.to_utf8_string(),
        other => panic!("expected a string column at index {idx}, got {other:?}"),
    };

    let scheme = column_text(0).to_ascii_uppercase();
    assert!(
        scheme.contains("NTLM") || scheme.contains("KERBEROS"),
        "expected a Windows auth scheme (NTLM/Kerberos), got {scheme:?}"
    );

    // EPA channel binding requires an encrypted login; confirm the server sees
    // this connection as encrypted.
    let encrypt = column_text(1);
    assert_eq!(
        encrypt, "TRUE",
        "expected an encrypted login (encrypt_option = TRUE)"
    );
    connection.close_query().await?;
    Ok(())
}
