// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Integration tests for Kerberos/GSSAPI authentication.
//!
//! These tests require a Kerberos environment to be set up.
//! Run from the kerberos-test directory with:
//!   ./setup.sh  # Set up the test environment
//!   cargo test --features gssapi test_kerberos
//!
//! The test environment provides:
//! - Samba AD DC at dc.example.local (KDC)
//! - SQL Server at sql.example.local with Kerberos enabled
//! - Test user: testuser@EXAMPLE.LOCAL / TestUser123!

#![cfg(unix)]

use mssql_tds::security::unix::{GssapiContext, has_valid_credentials, is_gssapi_available};
use mssql_tds::security::{IntegratedAuthConfig, SecurityContext, SecurityPackage};
use std::env;

/// Test that GSSAPI library availability check works correctly.
///
/// The library is loaded via dlopen at runtime, so availability depends on
/// whether libgssapi_krb5.so is installed on the system. This matches ODBC's
/// approach of having no compile-time krb5 dependency.
#[test]
fn test_gssapi_available() {
    let available = is_gssapi_available();

    // On Alpine (musl) without krb5-dev, GSSAPI won't be available - this is expected.
    // On macOS, GSSAPI may not be available in CI environments since Homebrew's krb5
    // is keg-only and requires special library path configuration.
    // On standard Linux systems with krb5 installed, it should be available.
    #[cfg(target_env = "musl")]
    {
        println!(
            "GSSAPI library availability on musl: {}",
            if available {
                "available"
            } else {
                "not available (expected if krb5-dev not installed)"
            }
        );
        // On musl, availability depends on whether krb5 package is installed
    }

    #[cfg(all(target_os = "macos", not(target_env = "musl")))]
    {
        println!(
            "GSSAPI library availability on macOS: {}",
            if available {
                "available"
            } else {
                "not available (expected in CI)"
            }
        );
        // On macOS, we just verify the function doesn't panic - availability is optional
    }

    #[cfg(all(target_os = "linux", not(target_env = "musl")))]
    {
        assert!(
            available,
            "GSSAPI library (libgssapi_krb5) should be available on glibc Linux"
        );
    }
}

/// Test GssapiContext creation with valid SPN
///
/// With the dlopen-based approach, this test works on all platforms including musl.
/// If libgssapi_krb5 is not installed, GssapiContext::new() returns an error early.
#[test]
fn test_gssapi_context_creation() {
    let config = IntegratedAuthConfig::new();
    let result = GssapiContext::new(&config, "sql.example.local", 1433);

    // This will fail if GSSAPI is not available or no credentials exist
    match result {
        Ok(ctx) => {
            assert_eq!(ctx.spn(), "MSSQLSvc/sql.example.local:1433");
            assert!(!ctx.is_complete());
        }
        Err(e) => {
            // Expected if GSSAPI library not available or no Kerberos ticket
            println!(
                "GssapiContext creation failed (expected without krb5/ticket): {:?}",
                e
            );
        }
    }
}

/// Test GssapiContext with explicit SPN
#[test]
fn test_gssapi_context_with_explicit_spn() {
    let config = IntegratedAuthConfig::with_spn("MSSQLSvc/custom.host:5000".to_string());
    let result = GssapiContext::new(&config, "server", 1433);

    match result {
        Ok(ctx) => {
            assert_eq!(ctx.spn(), "MSSQLSvc/custom.host:5000");
        }
        Err(_) => {
            // Expected if GSSAPI not available or no Kerberos ticket
        }
    }
}

/// Integration test for full Kerberos authentication flow (token generation only).
///
/// This test requires:
/// 1. A running Kerberos environment (./kerberos-test/setup.sh)
/// 2. A valid Kerberos ticket (kinit testuser@EXAMPLE.LOCAL)
///
/// Set KERBEROS_TEST=1 to enable this test.
#[test]
#[ignore = "Requires Kerberos environment - set KERBEROS_TEST=1 to run"]
fn test_kerberos_token_generation() {
    if env::var("KERBEROS_TEST").is_err() {
        println!("Skipping Kerberos test - set KERBEROS_TEST=1 to enable");
        return;
    }

    // Check for valid credentials
    if !has_valid_credentials() {
        panic!("No valid Kerberos credentials. Run 'kinit testuser@EXAMPLE.LOCAL' first.");
    }

    // Create context
    let config = IntegratedAuthConfig::new().with_package(SecurityPackage::Kerberos);
    let mut ctx = GssapiContext::new(&config, "sql.example.local", 1433)
        .expect("Failed to create GSSAPI context");

    assert_eq!(ctx.spn(), "MSSQLSvc/sql.example.local:1433");
    assert_eq!(ctx.package_name(), "Kerberos");
    assert!(!ctx.is_complete());

    // Generate initial token
    let token = ctx
        .generate_token(None)
        .expect("Failed to generate initial token");

    assert!(!token.data.is_empty(), "Token should not be empty");
    println!("Generated token of {} bytes", token.data.len());

    // The token should be a valid GSSAPI/Kerberos token
    // It typically starts with 0x60 (ASN.1 APPLICATION tag)
    if !token.data.is_empty() {
        println!("Token first byte: 0x{:02x}", token.data[0]);
        // Kerberos tokens wrapped in GSSAPI start with 0x60
        assert!(
            token.data[0] == 0x60,
            "Expected GSSAPI wrapped token starting with 0x60"
        );
    }

    // Check completion status
    println!(
        "Authentication complete: {}, Token complete flag: {}",
        ctx.is_complete(),
        token.is_complete
    );
}

/// Test credential checking
#[test]
fn test_has_valid_credentials() {
    // This just tests that the function doesn't panic.
    // With dlopen, this returns false if GSSAPI library is not available.
    let has_creds = has_valid_credentials();
    println!("Has valid Kerberos credentials: {}", has_creds);
}

// =============================================================================
// Full End-to-End TDS Client Tests with Kerberos Authentication
// =============================================================================

mod e2e {
    use super::*;
    use mssql_tds::connection::client_context::{ClientContext, TdsAuthenticationMethod};
    use mssql_tds::connection::tds_client::{ResultSet, TdsClient};
    use mssql_tds::connection_provider::tds_connection_provider::TdsConnectionProvider;
    use mssql_tds::core::{EncryptionOptions, EncryptionSetting};

    /// Creates a ClientContext configured for Kerberos/SSPI authentication.
    fn create_kerberos_context() -> ClientContext {
        let mut context = ClientContext::default();

        // Use SSPI authentication (Kerberos on Linux via GSSAPI)
        context.tds_authentication_method = TdsAuthenticationMethod::SSPI;

        // No username/password needed for integrated auth
        context.user_name = String::new();
        context.password = String::new();

        // Database
        context.database = "master".to_string();

        // Increase timeout for Kerberos (GSSAPI can be slower)
        context.connect_timeout = 60;

        // Encryption settings - trust the test server certificate
        context.encryption_options = EncryptionOptions {
            mode: EncryptionSetting::On,
            trust_server_certificate: true,
            host_name_in_cert: None,
            server_certificate: None,
        };

        context
    }

    /// Full end-to-end test: Connect to SQL Server using Kerberos and run a query.
    ///
    /// This test:
    /// 1. Creates a TDS client with SSPI/Kerberos authentication
    /// 2. Connects to sql.example.local:1433
    /// 3. Executes a simple query
    /// 4. Verifies the result
    ///
    /// Requirements:
    /// - Kerberos environment running (./kerberos-test/setup.sh)
    /// - Valid Kerberos ticket (kinit testuser@EXAMPLE.LOCAL)
    /// - Set KERBEROS_TEST=1 to run
    #[tokio::test]
    #[ignore = "Requires Kerberos environment - set KERBEROS_TEST=1 to run"]
    async fn test_kerberos_full_connection() {
        if env::var("KERBEROS_TEST").is_err() {
            println!("Skipping Kerberos test - set KERBEROS_TEST=1 to enable");
            return;
        }

        // Check for valid credentials first
        if !has_valid_credentials() {
            panic!("No valid Kerberos credentials. Run 'kinit testuser@EXAMPLE.LOCAL' first.");
        }

        println!("=== Starting Kerberos E2E Connection Test ===");

        // Create context for Kerberos auth
        let context = create_kerberos_context();

        // Connect to SQL Server
        let provider = TdsConnectionProvider {};
        let datasource = "sql.example.local,1433";

        println!(
            "Connecting to {} with Kerberos authentication...",
            datasource
        );

        let mut client: TdsClient = provider
            .create_client(context, datasource, None)
            .await
            .expect("Failed to connect to SQL Server with Kerberos");

        println!("✓ Connected successfully!");

        // Execute a simple query to verify the connection works
        let query = "SELECT SUSER_NAME() AS CurrentUser, DB_NAME() AS CurrentDatabase";
        println!("Executing query: {}", query);

        client
            .execute(query.to_string(), ())
            .await
            .expect("Failed to execute query");

        // Get the result
        if let Some(resultset) = client.get_current_resultset()
            && let Some(row) = resultset.next_row().await.expect("Failed to get row")
        {
            println!("Query returned a result set");
            let current_user = &row[0];
            let current_db = &row[1];

            println!("✓ Current User: {:?}", current_user);
            println!("✓ Current Database: {:?}", current_db);

            // Verify we're authenticated as the expected user
            // The user should be something like EXAMPLE\testuser
            let user_str = format!("{:?}", current_user);
            assert!(
                user_str.contains("testuser") || user_str.contains("TESTUSER"),
                "Expected to be authenticated as testuser, got: {}",
                user_str
            );
        } else {
            panic!("No result set or rows returned from query");
        }

        // Move to completion
        while client
            .advance_to_rows()
            .await
            .expect("Failed to move to next")
        {}

        println!("=== Kerberos E2E Connection Test PASSED ===");
    }

    /// Test that we can execute multiple queries on a Kerberos-authenticated connection.
    #[tokio::test]
    #[ignore = "Requires Kerberos environment - set KERBEROS_TEST=1 to run"]
    async fn test_kerberos_multiple_queries() {
        if env::var("KERBEROS_TEST").is_err() {
            return;
        }

        if !has_valid_credentials() {
            panic!("No valid Kerberos credentials.");
        }

        let context = create_kerberos_context();
        let provider = TdsConnectionProvider {};

        let mut client: TdsClient = provider
            .create_client(context, "sql.example.local,1433", None)
            .await
            .expect("Failed to connect");

        // Query 1: Get server version
        client
            .execute("SELECT @@VERSION".to_string(), ())
            .await
            .expect("Query 1 failed");
        if let Some(rs) = client.get_current_resultset()
            && let Some(row) = rs.next_row().await.expect("Failed to read row")
        {
            println!("SQL Server Version: {:?}", row[0]);
        }
        while client.advance_to_rows().await.expect("Failed") {}

        // Query 2: Get current time
        client
            .execute("SELECT GETDATE() AS ServerTime".to_string(), ())
            .await
            .expect("Query 2 failed");
        if let Some(rs) = client.get_current_resultset()
            && let Some(row) = rs.next_row().await.expect("Failed to read row")
        {
            println!("Server Time: {:?}", row[0]);
        }
        while client.advance_to_rows().await.expect("Failed") {}

        // Query 3: Get authentication info
        client
            .execute(
                "SELECT auth_scheme FROM sys.dm_exec_connections WHERE session_id = @@SPID"
                    .to_string(),
                (),
            )
            .await
            .expect("Query 3 failed");
        if let Some(rs) = client.get_current_resultset()
            && let Some(row) = rs.next_row().await.expect("Failed to read row")
        {
            let auth_scheme = format!("{:?}", row[0]);
            println!("Authentication Scheme: {}", auth_scheme);
            // Should be KERBEROS for integrated auth
            assert!(
                auth_scheme.contains("KERBEROS") || auth_scheme.contains("NTLM"),
                "Expected KERBEROS or NTLM auth scheme, got: {}",
                auth_scheme
            );
        }
        while client.advance_to_rows().await.expect("Failed") {}

        println!("✓ Multiple queries on Kerberos connection succeeded!");
    }

    /// Test connection with explicit SPN.
    /// Note: On Linux/GSSAPI, the SPN must be in GSSAPI format (service@host)
    /// rather than Windows format (service/host:port).
    #[tokio::test]
    #[ignore = "Requires Kerberos environment - set KERBEROS_TEST=1 to run"]
    async fn test_kerberos_with_explicit_spn() {
        if env::var("KERBEROS_TEST").is_err() {
            return;
        }

        if !has_valid_credentials() {
            panic!("No valid Kerberos credentials.");
        }

        let mut context = create_kerberos_context();
        // Set explicit SPN in GSSAPI format (service@host, not service/host:port)
        // This is required because user-provided SPNs are passed directly to GSSAPI
        context.server_spn = Some("MSSQLSvc@sql.example.local".to_string());

        let provider = TdsConnectionProvider {};

        let mut client: TdsClient = provider
            .create_client(context, "sql.example.local,1433", None)
            .await
            .expect("Failed to connect with explicit SPN");

        // Verify connection works
        client
            .execute("SELECT 1 AS Test".to_string(), ())
            .await
            .expect("Query failed");
        if let Some(rs) = client.get_current_resultset()
            && let Some(row) = rs.next_row().await.expect("Failed")
        {
            println!("Explicit SPN test result: {:?}", row[0]);
        }
        while client.advance_to_rows().await.expect("Failed") {}

        println!("✓ Connection with explicit SPN succeeded!");
    }

    /// Test connection via IP address, which requires reverse DNS lookup for SPN.
    ///
    /// This test verifies that when connecting to a SQL Server by IP address:
    /// 1. The reverse DNS lookup resolves the IP to an FQDN
    /// 2. The SPN is constructed using the resolved FQDN
    /// 3. Kerberos authentication succeeds with the correct SPN
    ///
    /// Requirements:
    /// - Kerberos environment running (./kerberos-test/setup.sh)
    /// - Valid Kerberos ticket (kinit testuser@EXAMPLE.LOCAL)
    /// - Reverse DNS configured: 172.20.0.20 -> sql.example.local
    /// - Set KERBEROS_TEST=1 to run
    #[tokio::test]
    #[ignore = "Requires Kerberos environment - set KERBEROS_TEST=1 to run"]
    async fn test_kerberos_connection_via_ip_address() {
        if env::var("KERBEROS_TEST").is_err() {
            println!("Skipping Kerberos test - set KERBEROS_TEST=1 to enable");
            return;
        }

        if !has_valid_credentials() {
            panic!("No valid Kerberos credentials. Run 'kinit testuser@EXAMPLE.LOCAL' first.");
        }

        println!("=== Starting Kerberos IP Address Connection Test ===");

        // Create context for Kerberos auth
        let context = create_kerberos_context();

        // Connect using IP address instead of hostname
        // The reverse DNS lookup should resolve 172.20.0.20 -> sql.example.local
        // and construct SPN: MSSQLSvc/sql.example.local:1433
        let provider = TdsConnectionProvider {};
        let datasource = "172.20.0.20,1433"; // IP address, not hostname!

        println!(
            "Connecting to {} (IP address) with Kerberos authentication...",
            datasource
        );
        println!("  Expected: Reverse DNS lookup resolves IP to sql.example.local");
        println!("  Expected: SPN constructed as MSSQLSvc/sql.example.local:1433");

        let mut client: TdsClient = provider
            .create_client(context, datasource, None)
            .await
            .expect("Failed to connect to SQL Server via IP address with Kerberos");

        println!("✓ Connected successfully via IP address!");

        // Execute a query to verify the connection and authentication
        let query = "SELECT SUSER_NAME() AS CurrentUser, auth_scheme FROM sys.dm_exec_connections WHERE session_id = @@SPID";
        println!("Executing query: {}", query);

        client
            .execute(query.to_string(), ())
            .await
            .expect("Failed to execute query");

        if let Some(resultset) = client.get_current_resultset()
            && let Some(row) = resultset.next_row().await.expect("Failed to get row")
        {
            let current_user = format!("{:?}", row[0]);
            let auth_scheme = format!("{:?}", row[1]);

            println!("✓ Current User: {}", current_user);
            println!("✓ Auth Scheme: {}", auth_scheme);

            // Verify we're authenticated as the expected user
            assert!(
                current_user.contains("testuser") || current_user.contains("TESTUSER"),
                "Expected to be authenticated as testuser, got: {}",
                current_user
            );

            // Verify Kerberos was used
            assert!(
                auth_scheme.contains("KERBEROS"),
                "Expected KERBEROS auth scheme when connecting via IP, got: {}",
                auth_scheme
            );
        } else {
            panic!("No result set or rows returned from query");
        }

        while client
            .advance_to_rows()
            .await
            .expect("Failed to move to next")
        {}

        println!("=== Kerberos IP Address Connection Test PASSED ===");
        println!("✓ Reverse DNS lookup correctly resolved IP to FQDN for SPN construction!");
    }
}
