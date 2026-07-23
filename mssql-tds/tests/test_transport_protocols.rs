// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#[cfg(test)]
mod common;

#[cfg(test)]
mod transport_protocols {
    use dotenv::dotenv;
    use mssql_tds::connection::client_context::ClientContext;
    #[cfg(windows)]
    use mssql_tds::connection::client_context::TdsAuthenticationMethod;
    use mssql_tds::connection::tds_client::{ResultSet, TdsClient};
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

    /// Create a client with the specified datasource string
    async fn create_client_with_datasource(datasource: &str) -> TdsResult<TdsClient> {
        create_client_with_datasource_and_encryption(datasource, EncryptionSetting::Strict).await
    }

    /// Create a client with the specified datasource string and encryption mode
    async fn create_client_with_datasource_and_encryption(
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

    /// Create a LocalDB client with integrated authentication (SSPI)
    #[cfg(windows)]
    async fn create_localdb_client_with_integrated_auth(
        datasource: &str,
        encryption_mode: EncryptionSetting,
    ) -> TdsResult<TdsClient> {
        let mut client_context = ClientContext::default();
        client_context.database = "master".to_string();
        client_context.tds_authentication_method = TdsAuthenticationMethod::SSPI;
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

    /// Execute a simple query and verify we get results
    async fn test_simple_query(client: &mut TdsClient) -> TdsResult<()> {
        let query = "SELECT @@VERSION AS version";
        client.execute(query.to_string(), ()).await?;

        let mut has_results = false;
        loop {
            if let Some(resultset) = client.get_current_resultset() {
                has_results = true;
                while let Some(_row) = resultset.next_row().await? {
                    // We got at least one row, which is what we expect
                }
            }

            if !client.advance_to_rows().await? {
                break;
            }
        }

        client.close_query().await?;
        assert!(has_results, "Query should return results");
        Ok(())
    }

    // =========================================================================
    // Named Pipe Tests
    // =========================================================================

    #[tokio::test]
    #[cfg(windows)]
    async fn test_named_pipe_default_instance() -> TdsResult<()> {
        init_tracing();
        dotenv().ok();

        // Skip this test if TRUST_SERVER_CERTIFICATE=true because Strict encryption
        // enforces certificate validation even with trust_server_certificate=true
        if trust_server_certificate() {
            println!(
                "Skipping test_named_pipe_default_instance: TRUST_SERVER_CERTIFICATE=true is incompatible with Strict encryption mode"
            );
            return Ok(());
        }

        let host = env::var("DB_HOST").expect("DB_HOST environment variable not set");

        // Test connecting to default instance using Named Pipe
        // Format: np:\\server\pipe\sql\query
        let datasource = format!(r"np:\\{}\pipe\sql\query", host);

        let mut client = create_client_with_datasource(&datasource).await?;
        test_simple_query(&mut client).await?;

        Ok(())
    }

    #[tokio::test]
    #[cfg(windows)]
    async fn test_named_pipe_local_default_instance() -> TdsResult<()> {
        init_tracing();
        dotenv().ok();

        // Skip this test if TRUST_SERVER_CERTIFICATE=true because Strict encryption
        // enforces certificate validation even with trust_server_certificate=true
        if trust_server_certificate() {
            println!(
                "Skipping test_named_pipe_local_default_instance: TRUST_SERVER_CERTIFICATE=true is incompatible with Strict encryption mode"
            );
            return Ok(());
        }

        // Test connecting to local default instance using Named Pipe
        // Format: np:\\.\pipe\sql\query (local machine shorthand)
        let datasource = r"np:\\.\pipe\sql\query";

        let mut client = create_client_with_datasource(datasource).await?;
        test_simple_query(&mut client).await?;

        Ok(())
    }

    #[tokio::test]
    #[cfg(windows)]
    async fn test_named_pipe_with_encryption_on() -> TdsResult<()> {
        init_tracing();
        dotenv().ok();

        // Test Named Pipe with Encryption=On (TDS 7.4, negotiated encryption)
        // This now works! The key was ensuring atomic writes to the Named Pipe during TLS handshake.
        // Named Pipes in Message mode treat each write as a complete message. Vectored I/O was
        // writing the TDS packet header (8 bytes) and TLS payload separately, causing SQL Server
        // to see an invalid 8-byte message and close the pipe. The fix: flatten multiple buffers
        // into a single buffer before writing during TLS handshake.
        let datasource = r"np:\\.\pipe\sql\query";

        let mut client =
            create_client_with_datasource_and_encryption(datasource, EncryptionSetting::On).await?;
        test_simple_query(&mut client).await?;

        Ok(())
    }

    // =========================================================================
    // Shared Memory Tests
    // =========================================================================

    #[tokio::test]
    #[cfg(windows)]
    async fn test_shared_memory_default_instance() -> TdsResult<()> {
        init_tracing();
        dotenv().ok();

        // Skip this test if TRUST_SERVER_CERTIFICATE=true because Strict encryption
        // enforces certificate validation even with trust_server_certificate=true
        if trust_server_certificate() {
            println!(
                "Skipping test_shared_memory_default_instance: TRUST_SERVER_CERTIFICATE=true is incompatible with Strict encryption mode"
            );
            return Ok(());
        }

        // Test connecting to default instance using Shared Memory (lpc:)
        // This uses the default instance name (empty string)
        let datasource = "lpc:.";

        let mut client = create_client_with_datasource(datasource).await?;
        test_simple_query(&mut client).await?;

        Ok(())
    }

    #[tokio::test]
    #[cfg(windows)]
    async fn test_shared_memory_mssqlserver_instance() -> TdsResult<()> {
        init_tracing();
        dotenv().ok();

        // Skip this test if TRUST_SERVER_CERTIFICATE=true because Strict encryption
        // enforces certificate validation even with trust_server_certificate=true
        if trust_server_certificate() {
            println!(
                "Skipping test_shared_memory_mssqlserver_instance: TRUST_SERVER_CERTIFICATE=true is incompatible with Strict encryption mode"
            );
            return Ok(());
        }

        // Test connecting using explicit local server syntax
        // lpc:. connects to the default instance via shared memory
        // (MSSQLSERVER is reserved and maps to default instance automatically)
        let datasource = "lpc:.";

        let mut client = create_client_with_datasource(datasource).await?;
        test_simple_query(&mut client).await?;

        Ok(())
    }

    #[tokio::test]
    #[cfg(windows)]
    async fn test_shared_memory_with_encryption_on() -> TdsResult<()> {
        init_tracing();
        dotenv().ok();

        // Test Shared Memory with Encryption=On (TDS 7.4, negotiated encryption)
        // This now works! The same atomic write fix that enabled Named Pipes encryption
        // also works for Shared Memory, since both transports have message-boundary semantics
        // that require complete messages to be written atomically.
        // lpc:. connects to default instance via shared memory
        let datasource = "lpc:.";

        let mut client =
            create_client_with_datasource_and_encryption(datasource, EncryptionSetting::On).await?;
        test_simple_query(&mut client).await?;

        Ok(())
    }

    // =========================================================================
    // TCP Tests (for comparison and to ensure TCP still works)
    // =========================================================================

    #[tokio::test]
    async fn test_tcp_connection() -> TdsResult<()> {
        init_tracing();
        dotenv().ok();

        // Skip this test if TRUST_SERVER_CERTIFICATE=true because Strict encryption
        // enforces certificate validation even with trust_server_certificate=true
        if trust_server_certificate() {
            println!(
                "Skipping test_tcp_connection: TRUST_SERVER_CERTIFICATE=true is incompatible with Strict encryption mode"
            );
            return Ok(());
        }

        let host = env::var("DB_HOST").expect("DB_HOST environment variable not set");
        let port = env::var("DB_PORT")
            .ok()
            .map(|v| v.parse::<u16>().expect("DB_PORT must be a valid u16"))
            .unwrap_or(1433);

        let datasource = format!("tcp:{},{}", host, port);

        let mut client = create_client_with_datasource(&datasource).await?;
        test_simple_query(&mut client).await?;

        Ok(())
    }

    #[tokio::test]
    async fn test_tcp_with_encryption_on() -> TdsResult<()> {
        init_tracing();
        dotenv().ok();

        // Test TCP with Encryption=On (TDS 7.4, negotiated encryption)
        // This should use TLS wrapping within TDS packets
        // If this test fails the same way as Named Pipes, it proves the issue
        // is in the TDS 7.4 SSL handling, not transport-specific
        let host = env::var("DB_HOST").expect("DB_HOST environment variable not set");
        let port = env::var("DB_PORT")
            .ok()
            .map(|v| v.parse::<u16>().expect("DB_PORT must be a valid u16"))
            .unwrap_or(1433);

        let datasource = format!("tcp:{},{}", host, port);

        let mut client =
            create_client_with_datasource_and_encryption(&datasource, EncryptionSetting::On)
                .await?;
        test_simple_query(&mut client).await?;

        Ok(())
    }

    // =========================================================================
    // Concurrent Connection Tests
    // =========================================================================

    #[tokio::test]
    #[cfg(windows)]
    async fn test_concurrent_named_pipe_connections() -> TdsResult<()> {
        init_tracing();
        dotenv().ok();

        // Skip this test if TRUST_SERVER_CERTIFICATE=true because Strict encryption
        // enforces certificate validation even with trust_server_certificate=true
        if trust_server_certificate() {
            println!(
                "Skipping test_concurrent_named_pipe_connections: TRUST_SERVER_CERTIFICATE=true is incompatible with Strict encryption mode"
            );
            return Ok(());
        }

        // Test opening 10 Named Pipe connections simultaneously
        // This verifies that our retry mechanism handles concurrent access correctly
        let datasource = r"np:\\.\pipe\sql\query";

        let mut handles = Vec::new();

        for i in 0..10 {
            let datasource_clone = datasource;
            let handle = tokio::spawn(async move {
                let mut client = create_client_with_datasource(datasource_clone)
                    .await
                    .unwrap_or_else(|_| panic!("Failed to create client {i}"));

                test_simple_query(&mut client)
                    .await
                    .unwrap_or_else(|_| panic!("Failed to execute query on client {i}"));

                println!("Client {i} completed successfully");
            });
            handles.push(handle);
        }

        // Wait for all connections to complete
        for handle in handles {
            handle.await.expect("Task panicked");
        }

        Ok(())
    }

    #[tokio::test]
    #[cfg(windows)]
    async fn test_concurrent_shared_memory_connections() -> TdsResult<()> {
        init_tracing();
        dotenv().ok();

        // Skip this test if TRUST_SERVER_CERTIFICATE=true because Strict encryption
        // enforces certificate validation even with trust_server_certificate=true
        if trust_server_certificate() {
            println!(
                "Skipping test_concurrent_shared_memory_connections: TRUST_SERVER_CERTIFICATE=true is incompatible with Strict encryption mode"
            );
            return Ok(());
        }

        // Test opening 10 Shared Memory connections simultaneously
        // This verifies that our retry mechanism handles concurrent access correctly
        // and that SQLLocal pipes don't conflict

        let mut handles = Vec::new();

        for i in 0..10 {
            let handle = tokio::spawn(async move {
                // lpc:. connects to default instance via shared memory
                let datasource = "lpc:.";

                let mut client = create_client_with_datasource(datasource)
                    .await
                    .unwrap_or_else(|_| panic!("Failed to create client {i}"));

                test_simple_query(&mut client)
                    .await
                    .unwrap_or_else(|_| panic!("Failed to execute query on client {i}"));

                println!("Client {i} completed successfully");
            });
            handles.push(handle);
        }

        // Wait for all connections to complete
        for handle in handles {
            handle.await.expect("Task panicked");
        }

        Ok(())
    }

    #[tokio::test]
    #[cfg(windows)]
    async fn test_localdb_connection() -> TdsResult<()> {
        init_tracing();
        dotenv().ok();

        println!("Testing LocalDB connection with integrated authentication...");

        // Connect to LocalDB using integrated authentication - test will fail if connection fails
        // Use Strict encryption to verify that the connection provider automatically
        // overrides it to PreferOff for LocalDB connections (matching ODBC behavior)
        let datasource = "(localdb)\\MSSQLLocalDB";
        let mut client =
            create_localdb_client_with_integrated_auth(datasource, EncryptionSetting::Strict)
                .await?;

        println!("Connected to LocalDB successfully with integrated authentication!");

        // Execute a simple query
        test_simple_query(&mut client).await?;

        println!("Query executed successfully on LocalDB");
        Ok(())
    }

    #[tokio::test]
    #[cfg(windows)]
    async fn test_localdb_query_execution() -> TdsResult<()> {
        init_tracing();
        dotenv().ok();

        println!("Testing LocalDB query execution with integrated authentication...");

        // Connect to LocalDB using integrated authentication - test will fail if connection fails
        // Use Required encryption to verify that the connection provider automatically
        // overrides it to PreferOff for LocalDB connections (matching ODBC behavior)
        let datasource = "(localdb)\\MSSQLLocalDB";
        let mut client =
            create_localdb_client_with_integrated_auth(datasource, EncryptionSetting::Required)
                .await?;

        // Execute multiple queries to test stability
        let queries = vec![
            "SELECT @@VERSION",
            "SELECT DB_NAME()",
            "SELECT GETDATE()",
            "SELECT 1 AS test_value",
        ];

        for query in queries {
            println!("Executing: {query}");
            client.execute(query.to_string(), ()).await?;

            while let Some(resultset) = client.get_current_resultset() {
                while let Some(_row) = resultset.next_row().await? {}
            }

            if client.advance_to_rows().await? {
                // Process any additional result sets
            }

            client.close_query().await?;
            println!("  ✓ Success");
        }

        println!("All queries executed successfully on LocalDB with integrated authentication");
        Ok(())
    }

    // LocalDB parsing is now handled by datasource parser
    // These tests have been moved to datasource parser tests

    // LocalDB connection properties are now tested via datasource parser tests
}
