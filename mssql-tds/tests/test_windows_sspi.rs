// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Integration test for Windows Integrated Authentication (SSPI).
//!
//! This test requires:
//! - A local SQL Server instance listening on port 1433
//! - Windows Integrated Authentication enabled on SQL Server
//! - The current Windows user must have access to SQL Server
//!
//! Set SSPI_TEST=1 to enable this test.

#![cfg(windows)]

use std::env;

mod common;

use mssql_tds::connection::client_context::{ClientContext, TdsAuthenticationMethod};
use mssql_tds::connection::tds_client::ResultSet;
use mssql_tds::connection_provider::tds_connection_provider::TdsConnectionProvider;
use mssql_tds::core::{EncryptionOptions, EncryptionSetting, TdsResult};
use mssql_tds::datatypes::column_values::ColumnValues;

/// Test Windows SSPI token generation
#[test]
fn test_windows_sspi_context_creation() {
    use mssql_tds::security::{IntegratedAuthConfig, create_security_context};

    let config = IntegratedAuthConfig::new();
    let result = create_security_context(&config, "localhost", 1433);

    match result {
        Ok(ctx) => {
            assert_eq!(ctx.spn(), "MSSQLSvc/localhost:1433");
            assert!(!ctx.is_complete());
            println!(
                "✓ SSPI context created successfully with SPN: {}",
                ctx.spn()
            );
        }
        Err(e) => {
            panic!("Failed to create SSPI context: {:?}", e);
        }
    }
}

/// Test Windows SSPI token generation with Negotiate package
#[test]
fn test_windows_sspi_generate_initial_token() {
    use mssql_tds::security::{IntegratedAuthConfig, create_security_context};

    let config = IntegratedAuthConfig::new();
    let mut ctx =
        create_security_context(&config, "localhost", 1433).expect("Failed to create SSPI context");

    // Generate initial token
    let token = ctx.generate_token(None);

    match token {
        Ok(t) => {
            assert!(!t.data.is_empty(), "Token should not be empty");
            println!("✓ Generated SSPI token of {} bytes", t.data.len());
            println!("  First byte: 0x{:02x}", t.data[0]);
            println!("  Is complete: {}", t.is_complete);
        }
        Err(e) => {
            panic!("Failed to generate initial token: {:?}", e);
        }
    }
}

/// Full integration test: Connect to local SQL Server with Windows Integrated Auth
///
/// Set SSPI_TEST=1 to enable this test.
#[tokio::test]
async fn test_windows_integrated_auth_connection() -> TdsResult<()> {
    if env::var("SSPI_TEST").is_err() {
        println!("Skipping SSPI integration test - set SSPI_TEST=1 to enable");
        return Ok(());
    }

    common::init_tracing();

    // Create client context for integrated authentication
    let mut context = ClientContext::default();
    context.database = "master".to_string();
    context.tds_authentication_method = TdsAuthenticationMethod::SSPI;
    context.encryption_options = EncryptionOptions {
        mode: EncryptionSetting::On,
        trust_server_certificate: true,
        host_name_in_cert: None,
        server_certificate: None,
    };

    let provider = TdsConnectionProvider {};
    let datasource = "tcp:localhost,1433";

    println!(
        "Connecting to {} with Windows Integrated Auth...",
        datasource
    );

    let mut connection = provider.create_client(context, datasource, None).await?;
    println!("✓ Successfully connected using Windows Integrated Auth!");

    // Verify the authentication scheme
    let query = "SELECT auth_scheme FROM sys.dm_exec_connections WHERE session_id = @@SPID";
    connection.execute(query.to_string(), ()).await?;

    if let Some(resultset) = connection.get_current_resultset()
        && let Some(row) = resultset.next_row().await?
    {
        let auth_scheme = format!("{:?}", row.first());
        println!("✓ Authentication scheme: {}", auth_scheme);
        // Should be NTLM or Kerberos
        assert!(
            auth_scheme.contains("NTLM") || auth_scheme.contains("Kerberos"),
            "Expected NTLM or Kerberos authentication scheme"
        );
    }

    connection.close_query().await?;
    println!("✓ Test completed successfully!");

    Ok(())
}

/// Full integration test: Connect to LocalDB with Windows Integrated Auth
///
/// LocalDB uses named pipes and only supports Windows Integrated Authentication.
/// Note: LocalDB typically does not support encryption, but we specify Strict here
/// to verify that the connection provider automatically overrides it to PreferOff
/// (matching ODBC behavior).
/// Set SSPI_TEST=1 to enable this test.
#[tokio::test]
async fn test_localdb_integrated_auth_connection() -> TdsResult<()> {
    if env::var("SSPI_TEST").is_err() {
        println!("Skipping LocalDB integration test - set SSPI_TEST=1 to enable");
        return Ok(());
    }

    common::init_tracing();

    // Create client context for integrated authentication
    // Specify Strict encryption to verify automatic override to PreferOff for LocalDB
    let mut context = ClientContext::default();
    context.database = "master".to_string();
    context.tds_authentication_method = TdsAuthenticationMethod::SSPI;
    context.encryption_options = EncryptionOptions {
        mode: EncryptionSetting::Strict,
        trust_server_certificate: true,
        host_name_in_cert: None,
        server_certificate: None,
    };

    let provider = TdsConnectionProvider {};

    // LocalDB connection string using the special (localdb) syntax
    let datasource = "(localdb)\\MSSQLLocalDB";

    println!(
        "Connecting to {} with Windows Integrated Auth...",
        datasource
    );

    let mut connection = provider.create_client(context, datasource, None).await?;
    println!("✓ Successfully connected to LocalDB using Windows Integrated Auth!");

    // Verify the authentication scheme
    let query = "SELECT auth_scheme FROM sys.dm_exec_connections WHERE session_id = @@SPID";
    connection.execute(query.to_string(), ()).await?;

    if let Some(resultset) = connection.get_current_resultset()
        && let Some(row) = resultset.next_row().await?
    {
        let auth_scheme = format!("{:?}", row.first());
        println!("✓ Authentication scheme: {}", auth_scheme);
        // LocalDB only supports Windows auth, should be NTLM
        assert!(
            auth_scheme.contains("NTLM") || auth_scheme.contains("Kerberos"),
            "Expected NTLM or Kerberos authentication scheme"
        );
    }
    connection.close_query().await?;

    // Also verify we're connected to LocalDB by checking the server name
    let query = "SELECT @@SERVERNAME, @@VERSION";
    connection.execute(query.to_string(), ()).await?;

    if let Some(resultset) = connection.get_current_resultset()
        && let Some(row) = resultset.next_row().await?
    {
        let server_name = format!("{:?}", row.first());
        let version = format!("{:?}", row.get(1));
        println!("✓ Server name: {}", server_name);
        println!("✓ Version: {}", &version[..100.min(version.len())]);
    }

    connection.close_query().await?;
    println!("✓ LocalDB test completed successfully!");

    Ok(())
}

/// Connect to a named instance via SSRP where only Named Pipes is enabled.
///
/// Exercises the SSRP fallback to Named Pipe transport when SQL Browser returns
/// no TCP endpoint. Set SSPI_TEST=1 and SSPI_NAMED_INSTANCE to enable.
#[tokio::test]
async fn test_ssrp_named_pipe_integrated_auth() -> TdsResult<()> {
    if env::var("SSPI_TEST").is_err() {
        return Ok(());
    }

    common::init_tracing();

    let datasource = env::var("DB_INSTANCE").unwrap_or_else(|_| r"localhost\SQLDEV".to_string());

    let mut context = ClientContext::default();
    context.database = "master".to_string();
    context.tds_authentication_method = TdsAuthenticationMethod::SSPI;
    context.encryption_options = EncryptionOptions {
        mode: EncryptionSetting::On,
        trust_server_certificate: true,
        host_name_in_cert: None,
        server_certificate: None,
    };

    let provider = TdsConnectionProvider {};
    let mut client = provider.create_client(context, &datasource, None).await?;

    client
        .execute(
            "SELECT net_transport FROM sys.dm_exec_connections WHERE session_id = @@SPID"
                .to_string(),
            (),
        )
        .await?;

    let mut transport = String::new();
    if let Some(rs) = client.get_current_resultset()
        && let Some(row) = rs.next_row().await?
        && let ColumnValues::String(s) = &row[0]
    {
        transport = s.to_string();
    }
    client.close_query().await?;

    assert!(
        !transport.is_empty(),
        "Expected net_transport from dm_exec_connections"
    );
    println!("✓ SSRP named pipe test passed for {datasource} (transport: {transport})");

    Ok(())
}

/// Helper: connect with SSPI and return the net_transport reported by SQL Server.
async fn connect_and_get_transport(datasource: &str) -> TdsResult<String> {
    let mut context = ClientContext::default();
    context.database = "master".to_string();
    context.tds_authentication_method = TdsAuthenticationMethod::SSPI;
    context.encryption_options = EncryptionOptions {
        mode: EncryptionSetting::On,
        trust_server_certificate: true,
        host_name_in_cert: None,
        server_certificate: None,
    };

    let provider = TdsConnectionProvider {};
    let mut client = provider.create_client(context, datasource, None).await?;

    client
        .execute(
            "SELECT net_transport FROM sys.dm_exec_connections WHERE session_id = @@SPID"
                .to_string(),
            (),
        )
        .await?;

    let mut transport = String::new();
    if let Some(rs) = client.get_current_resultset()
        && let Some(row) = rs.next_row().await?
        && let ColumnValues::String(s) = &row[0]
    {
        transport = s.to_string();
    }
    client.close_query().await?;

    assert!(
        !transport.is_empty(),
        "Expected net_transport from dm_exec_connections for {datasource}"
    );
    Ok(transport)
}

/// Verify that a tcp:-prefixed named instance connects over TCP.
#[tokio::test]
async fn test_tcp_prefix_uses_tcp_transport() -> TdsResult<()> {
    if env::var("SSPI_TEST").is_err() {
        return Ok(());
    }
    common::init_tracing();

    let instance = env::var("DB_INSTANCE").unwrap_or_else(|_| r"localhost\SQLDEV".to_string());
    let datasource = format!("tcp:{instance}");

    let transport = Box::pin(connect_and_get_transport(&datasource)).await?;
    assert_eq!(
        transport, "TCP",
        "tcp: prefix should use TCP transport, got {transport}"
    );
    println!("✓ tcp: prefix test passed for {datasource} (transport: {transport})");
    Ok(())
}

/// Verify that an np:-prefixed named instance connects over Named Pipes.
#[tokio::test]
async fn test_np_prefix_uses_named_pipe_transport() -> TdsResult<()> {
    if env::var("SSPI_TEST").is_err() {
        return Ok(());
    }
    common::init_tracing();

    let instance = env::var("DB_INSTANCE").unwrap_or_else(|_| r"localhost\SQLDEV".to_string());
    let datasource = format!("np:{instance}");

    let transport = Box::pin(connect_and_get_transport(&datasource)).await?;
    assert_eq!(
        transport, "Named pipe",
        "np: prefix should use Named pipe transport, got {transport}"
    );
    println!("✓ np: prefix test passed for {datasource} (transport: {transport})");
    Ok(())
}

/// Connect to localhost with SSPI, run SELECT 1, and validate the result.
///
/// Set SSPI_TEST=1 to enable this test.
#[tokio::test]
async fn test_sspi_localhost_select_one() -> TdsResult<()> {
    if env::var("SSPI_TEST").is_err() {
        return Ok(());
    }

    common::init_tracing();

    let mut context = ClientContext::default();
    context.database = "master".to_string();
    context.tds_authentication_method = TdsAuthenticationMethod::SSPI;
    context.encryption_options = EncryptionOptions {
        mode: EncryptionSetting::PreferOff,
        trust_server_certificate: true,
        host_name_in_cert: None,
        server_certificate: None,
    };

    let provider = TdsConnectionProvider {};
    let mut client = provider
        .create_client(context, "tcp:localhost,1433", None)
        .await?;

    client.execute("SELECT 1 AS val".to_string(), ()).await?;

    let mut got_result = false;
    if let Some(rs) = client.get_current_resultset()
        && let Some(row) = rs.next_row().await?
    {
        assert_eq!(
            row[0],
            ColumnValues::Int(1),
            "Expected SELECT 1 to return 1"
        );
        got_result = true;
    }
    client.close_query().await?;

    assert!(got_result, "Expected exactly one row from SELECT 1");

    Ok(())
}

/// Connect to a named instance via SSRP with SSPI and run SELECT 1.
///
/// Uses `localhost\<instance>` (no `tcp:` prefix) which exercises the SSRP path:
/// datasource parser → SQL Browser resolution → SSPI with loopback detection.
///
/// Set SSPI_TEST=1 to enable. Optionally set SSPI_NAMED_INSTANCE (default: sqldev).
#[tokio::test]
async fn test_sspi_named_instance_select_one() -> TdsResult<()> {
    if env::var("SSPI_TEST").is_err() {
        return Ok(());
    }

    common::init_tracing();

    let instance = env::var("SSPI_NAMED_INSTANCE").unwrap_or_else(|_| "sqldev".to_string());
    let datasource = format!(r"localhost\{instance}");

    let mut context = ClientContext::default();
    context.database = "master".to_string();
    context.tds_authentication_method = TdsAuthenticationMethod::SSPI;
    context.encryption_options = EncryptionOptions {
        mode: EncryptionSetting::PreferOff,
        trust_server_certificate: true,
        host_name_in_cert: None,
        server_certificate: None,
    };

    let provider = TdsConnectionProvider {};
    let mut client = provider.create_client(context, &datasource, None).await?;

    client
        .execute("SELECT @@SERVICENAME AS svc".to_string(), ())
        .await?;

    if let Some(rs) = client.get_current_resultset()
        && let Some(row) = rs.next_row().await?
    {
        let svc = format!("{:?}", row.first());
        assert!(
            svc.to_uppercase().contains(&instance.to_uppercase()),
            "Expected instance {instance}, got: {svc}"
        );
    }
    client.close_query().await?;

    client.execute("SELECT 1 AS val".to_string(), ()).await?;

    let mut got_result = false;
    if let Some(rs) = client.get_current_resultset()
        && let Some(row) = rs.next_row().await?
    {
        assert_eq!(
            row[0],
            ColumnValues::Int(1),
            "Expected SELECT 1 to return 1"
        );
        got_result = true;
    }
    client.close_query().await?;

    assert!(got_result, "Expected exactly one row from SELECT 1");

    Ok(())
}
