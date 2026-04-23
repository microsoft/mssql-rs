// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Integration tests for no-protocol connection resolution
//!
//! These tests verify ODBC-compatible behavior when no explicit protocol is specified
//! in the connection string. The driver should automatically try protocols in order:
//! 1. Shared Memory (local only, Windows only)
//! 2. TCP
//! 3. Named Pipes (Windows only)

#[cfg(test)]
mod common;

#[cfg(test)]
mod no_protocol_resolution {
    use dotenv::dotenv;
    use mssql_tds::connection::client_context::ClientContext;
    use mssql_tds::connection::tds_client::{ResultSet, ResultSetClient, TdsClient};
    use mssql_tds::connection_provider::tds_connection_provider::TdsConnectionProvider;
    use mssql_tds::core::{EncryptionOptions, EncryptionSetting, TdsResult};
    use std::env;

    use crate::common::init_tracing;

    /// Helper function to get database credentials from environment
    fn get_db_credentials() -> (String, String) {
        dotenv().ok();
        let username = env::var("DB_USERNAME").expect("DB_USERNAME environment variable not set");
        let password = env::var("SQL_PASSWORD")
            .or_else(|_| {
                std::fs::read_to_string("/tmp/password")
                    .map(|s| s.trim().to_string())
                    .map_err(|_| std::env::VarError::NotPresent)
            })
            .expect(
                "SQL_PASSWORD environment variable not set and /tmp/password could not be read",
            );
        (username, password)
    }

    /// Helper function to get trust server certificate setting
    fn trust_server_certificate() -> bool {
        dotenv().ok();
        env::var("TRUST_SERVER_CERTIFICATE")
            .unwrap_or_else(|_| "false".to_string())
            .parse::<bool>()
            .unwrap()
    }

    /// Helper function to get certificate hostname
    fn get_cert_hostname() -> Option<String> {
        dotenv().ok();
        env::var("CERT_HOST_NAME").ok()
    }

    /// Create a client using parse_datasource with encryption mode
    async fn create_client_from_datasource_with_encryption(
        datasource: &str,
        encryption_mode: EncryptionSetting,
    ) -> TdsResult<TdsClient> {
        let (username, password) = get_db_credentials();

        let mut client_context = ClientContext::default();
        client_context.user_name = username;
        client_context.password = password;
        client_context.database = "master".to_string();
        client_context.encryption_options = EncryptionOptions {
            mode: encryption_mode,
            trust_server_certificate: trust_server_certificate(),
            host_name_in_cert: get_cert_hostname(),
            server_certificate: None,
        };

        let provider = TdsConnectionProvider {};
        provider
            .create_client(client_context, datasource, None)
            .await
    }

    /// Create a client with explicit trust_server_certificate setting
    /// Useful for tests that need to bypass certificate validation
    async fn create_client_with_trust_cert(
        datasource: &str,
        encryption_mode: EncryptionSetting,
        trust_cert: bool,
    ) -> TdsResult<TdsClient> {
        let (username, password) = get_db_credentials();

        let mut client_context = ClientContext::default();
        client_context.user_name = username;
        client_context.password = password;
        client_context.database = "master".to_string();
        client_context.encryption_options = EncryptionOptions {
            mode: encryption_mode,
            trust_server_certificate: trust_cert,
            host_name_in_cert: get_cert_hostname(),
            server_certificate: None,
        };

        let provider = TdsConnectionProvider {};
        provider
            .create_client(client_context, datasource, None)
            .await
    }

    /// Create a client using parse_datasource with default encryption
    /// Uses Encryption=On when TRUST_SERVER_CERTIFICATE=true, otherwise Strict
    async fn create_client_from_datasource(datasource: &str) -> TdsResult<TdsClient> {
        let encryption_mode = if trust_server_certificate() {
            EncryptionSetting::On
        } else {
            EncryptionSetting::Strict
        };
        create_client_from_datasource_with_encryption(datasource, encryption_mode).await
    }

    /// Execute a simple query and verify we get results
    async fn test_simple_query(client: &mut TdsClient) -> TdsResult<()> {
        let query = "SELECT @@VERSION AS version, @@SERVERNAME AS servername";
        client.execute(query.to_string(), None, None).await?;

        let mut has_results = false;
        let mut row_count = 0;

        loop {
            if let Some(resultset) = client.get_current_resultset() {
                has_results = true;
                while let Some(_row) = resultset.next_row().await? {
                    row_count += 1;
                }
            }

            if !client.move_to_next().await? {
                break;
            }
        }

        client.close_query().await?;
        assert!(has_results, "Query should return results");
        assert_eq!(row_count, 1, "Query should return exactly one row");
        Ok(())
    }

    // =========================================================================
    // Explicit Protocol Tests (Baseline)
    // =========================================================================

    #[tokio::test]
    async fn test_explicit_tcp_with_port() -> TdsResult<()> {
        init_tracing();
        dotenv().ok();

        // Use DB_HOST and DB_PORT from environment (works in CI with sql1)
        let host = env::var("DB_HOST").unwrap_or_else(|_| "localhost".to_string());
        let port = env::var("DB_PORT").unwrap_or_else(|_| "1433".to_string());

        // Test explicit TCP protocol with port
        let datasource = format!("tcp:{},{}", host, port);
        let mut client = create_client_from_datasource(&datasource).await?;
        test_simple_query(&mut client).await?;

        Ok(())
    }

    #[tokio::test]
    #[cfg(windows)]
    async fn test_explicit_named_pipe() -> TdsResult<()> {
        init_tracing();
        dotenv().ok();

        // Test explicit Named Pipe protocol with full pipe path
        let datasource = r"np:\\.\pipe\sql\query";
        let mut client = create_client_from_datasource(datasource).await?;
        test_simple_query(&mut client).await?;

        Ok(())
    }

    #[tokio::test]
    #[cfg(windows)]
    async fn test_explicit_shared_memory() -> TdsResult<()> {
        init_tracing();
        dotenv().ok();

        // Test explicit Shared Memory protocol (lpc: - Local Procedure Call)
        // Only works for local connections
        let datasource = "lpc:.";
        let mut client = create_client_from_datasource(datasource).await?;
        test_simple_query(&mut client).await?;

        Ok(())
    }

    #[tokio::test]
    #[cfg(windows)]
    async fn test_explicit_named_pipe_with_server() -> TdsResult<()> {
        init_tracing();
        dotenv().ok();

        // Test explicit Named Pipe protocol with server name
        // Parser should construct the appropriate pipe path
        let datasource = "np:localhost";
        let mut client = create_client_from_datasource(datasource).await?;
        test_simple_query(&mut client).await?;

        Ok(())
    }

    // =========================================================================
    // No Protocol - Default Port Tests
    // =========================================================================

    #[tokio::test]
    #[cfg_attr(
        not(windows),
        ignore = "Requires local SQL Server on localhost - run on Windows"
    )]
    async fn test_no_protocol_localhost_default_port() -> TdsResult<()> {
        init_tracing();
        dotenv().ok();

        // Test no protocol, just "localhost" - should try protocol list
        let datasource = "localhost";
        let mut client = create_client_from_datasource(datasource).await?;
        test_simple_query(&mut client).await?;

        Ok(())
    }

    #[tokio::test]
    #[cfg_attr(
        not(windows),
        ignore = "Requires local SQL Server on localhost - run on Windows"
    )]
    async fn test_no_protocol_dot_local() -> TdsResult<()> {
        init_tracing();
        dotenv().ok();

        // Test no protocol with "." (local shorthand)
        let datasource = ".";
        let mut client = create_client_from_datasource(datasource).await?;
        test_simple_query(&mut client).await?;

        Ok(())
    }

    #[tokio::test]
    #[cfg_attr(
        not(windows),
        ignore = "Requires local SQL Server on 127.0.0.1 - run on Windows"
    )]
    async fn test_no_protocol_127_0_0_1() -> TdsResult<()> {
        init_tracing();
        dotenv().ok();

        // Test no protocol with loopback IP
        let datasource = "127.0.0.1";
        let mut client = create_client_from_datasource(datasource).await?;
        test_simple_query(&mut client).await?;

        Ok(())
    }

    #[tokio::test]
    async fn test_no_protocol_with_explicit_port() -> TdsResult<()> {
        init_tracing();
        dotenv().ok();

        // Use DB_HOST and DB_PORT from environment (works in CI with sql1)
        let host = env::var("DB_HOST").unwrap_or_else(|_| "localhost".to_string());
        let port = env::var("DB_PORT").unwrap_or_else(|_| "1433".to_string());

        // Test no protocol prefix but with port (should auto-default to TCP)
        let datasource = format!("{},{}", host, port);
        let mut client = create_client_from_datasource(&datasource).await?;
        test_simple_query(&mut client).await?;

        Ok(())
    }

    // =========================================================================
    // Protocol Fallback Tests (Windows Only - tests SM, TCP, NP order)
    // =========================================================================

    #[tokio::test]
    #[cfg(windows)]
    async fn test_protocol_fallback_order_with_encryption_on() -> TdsResult<()> {
        init_tracing();
        dotenv().ok();

        // Test no protocol with Encryption=On to allow all protocols to work
        // This tests that the driver tries protocols in correct order:
        // 1. Shared Memory (local only)
        // 2. TCP
        // 3. Named Pipes
        let datasource = "localhost";
        let mut client =
            create_client_from_datasource_with_encryption(datasource, EncryptionSetting::On)
                .await?;
        test_simple_query(&mut client).await?;

        Ok(())
    }

    #[tokio::test]
    #[cfg(windows)]
    async fn test_named_pipe_auto_format() -> TdsResult<()> {
        init_tracing();
        dotenv().ok();

        // Test Named Pipe auto-detection (UNC path without "np:" prefix)
        // Format: \\server\pipe\path
        let datasource = r"\\.\pipe\sql\query";
        let mut client = create_client_from_datasource(datasource).await?;
        test_simple_query(&mut client).await?;

        Ok(())
    }

    // =========================================================================
    // Error Cases - SSRP (SQL Browser Unreachable)
    // =========================================================================

    #[tokio::test]
    async fn test_instance_name_without_port_returns_error() -> TdsResult<()> {
        init_tracing();
        dotenv().ok();

        // Use DB_HOST from environment (works in CI with sql1)
        let host = env::var("DB_HOST").unwrap_or_else(|_| "localhost".to_string());

        // Instance name without port triggers SSRP. With no SQL Browser running
        // on the default port, the connection should fail with a descriptive error.
        let datasource = format!("{}\\SQLEXPRESS", host);
        let result = create_client_from_datasource(&datasource).await;

        // Should fail — either SQL Browser timeout or connection refused
        assert!(
            result.is_err(),
            "Should fail when SQL Browser is unreachable"
        );

        let err = result.unwrap_err();
        let err_msg = err.to_string().to_lowercase();
        assert!(
            err_msg.contains("browser")
                || err_msg.contains("instance")
                || err_msg.contains("locating"),
            "Error should mention Browser/Instance/Locating: {}",
            err
        );

        Ok(())
    }

    #[tokio::test]
    #[cfg(windows)]
    async fn test_named_instance_with_named_pipe_format_returns_error() -> TdsResult<()> {
        init_tracing();
        dotenv().ok();

        // Test named instance with named pipe format (requires SSRP to resolve)
        let datasource = r"tcp:localhost\SQLEXPRESS";
        let result = create_client_from_datasource(datasource).await;

        // Should fail because no SQL Browser is running
        assert!(
            result.is_err(),
            "Should fail for named instance without SQL Browser"
        );

        Ok(())
    }

    // =========================================================================
    // Port Priority Tests
    // =========================================================================

    #[tokio::test]
    async fn test_port_takes_priority_over_instance() -> TdsResult<()> {
        init_tracing();
        dotenv().ok();

        // Use DB_HOST and DB_PORT from environment (works in CI with sql1)
        let host = env::var("DB_HOST").unwrap_or_else(|_| "localhost".to_string());
        let port = env::var("DB_PORT").unwrap_or_else(|_| "1433".to_string());

        // ODBC behavior: when both port and instance are specified, port takes priority
        // Instance name should be ignored
        // Format: server\instance,port
        let datasource = format!("{}\\IGNORED,{}", host, port);
        let mut client = create_client_from_datasource(&datasource).await?;
        test_simple_query(&mut client).await?;

        Ok(())
    }

    // =========================================================================
    // Local Server Detection Tests
    // =========================================================================

    #[tokio::test]
    #[cfg_attr(
        not(windows),
        ignore = "Requires local SQL Server on localhost/127.0.0.1 - run on Windows"
    )]
    async fn test_various_localhost_formats() -> TdsResult<()> {
        init_tracing();
        dotenv().ok();

        // Test various formats that should be detected as localhost
        // Note: Using Encryption=On (not Strict) because IP addresses like 127.0.0.1
        // will fail TLS certificate validation since certs use hostnames, not IPs.
        // This test validates connection routing, not TLS certificate matching.
        let local_formats = vec![
            "localhost",
            ".",
            "127.0.0.1",
            "(local)",
            // IPv6 loopback
            // "::1",  // May need special handling
        ];

        for format in local_formats {
            let datasource = format!("{},1433", format);
            // Use Encryption=On with trust_server_certificate=true to bypass TLS hostname validation
            // since IP addresses like 127.0.0.1 won't match certificate CN/SAN
            let result =
                create_client_with_trust_cert(&datasource, EncryptionSetting::On, true).await;

            match result {
                Ok(mut client) => {
                    test_simple_query(&mut client).await?;
                }
                Err(e) => {
                    return Err(e);
                }
            }
        }

        Ok(())
    }

    // =========================================================================
    // Protocol List Building Tests
    // =========================================================================

    #[tokio::test]
    #[cfg_attr(
        not(windows),
        ignore = "Requires local SQL Server on localhost:1433 - run on Windows"
    )]
    async fn test_no_protocol_uses_default_1433() -> TdsResult<()> {
        init_tracing();
        dotenv().ok();

        // When no protocol and no port, should try default port 1433
        let datasource = "localhost";
        let mut client = create_client_from_datasource(datasource).await?;
        test_simple_query(&mut client).await?;

        Ok(())
    }

    // =========================================================================
    // Edge Cases
    // =========================================================================

    #[tokio::test]
    async fn test_whitespace_handling() -> TdsResult<()> {
        init_tracing();
        dotenv().ok();

        // Use DB_HOST and DB_PORT from environment (works in CI with sql1)
        let host = env::var("DB_HOST").unwrap_or_else(|_| "localhost".to_string());
        let port = env::var("DB_PORT").unwrap_or_else(|_| "1433".to_string());

        // Test that whitespace is properly trimmed (ODBC behavior)
        let datasource = format!("  {}  ,  {}  ", host, port);
        let mut client = create_client_from_datasource(&datasource).await?;
        test_simple_query(&mut client).await?;

        Ok(())
    }

    #[tokio::test]
    async fn test_case_insensitive_protocol() -> TdsResult<()> {
        init_tracing();
        dotenv().ok();

        // Use DB_HOST and DB_PORT from environment (works in CI with sql1)
        let host = env::var("DB_HOST").unwrap_or_else(|_| "localhost".to_string());
        let port = env::var("DB_PORT").unwrap_or_else(|_| "1433".to_string());

        // Test that protocol names are case-insensitive (ODBC behavior)
        let datasources = vec![
            format!("tcp:{},{}", host, port),
            format!("TCP:{},{}", host, port),
            format!("Tcp:{},{}", host, port),
        ];

        for datasource in &datasources {
            let mut client = create_client_from_datasource(datasource).await?;
            test_simple_query(&mut client).await?;
        }

        Ok(())
    }

    // =========================================================================
    // Connection String Parsing Tests
    // =========================================================================

    #[tokio::test]
    async fn test_empty_instance_name_ignored() -> TdsResult<()> {
        init_tracing();
        dotenv().ok();

        // Use DB_HOST and DB_PORT from environment (works in CI with sql1)
        let host = env::var("DB_HOST").unwrap_or_else(|_| "localhost".to_string());
        let port = env::var("DB_PORT").unwrap_or_else(|_| "1433".to_string());

        // Test that trailing backslash without instance name is handled
        // Format: server\ (empty instance name)
        let datasource = format!("{}\\,{}", host, port);
        let mut client = create_client_from_datasource(&datasource).await?;
        test_simple_query(&mut client).await?;

        Ok(())
    }
}
