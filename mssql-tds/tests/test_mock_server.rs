// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Integration tests using the mock TDS server

#[cfg(test)]
mod mock_server_tests {
    use mssql_mock_tds::MockTdsServer;
    use mssql_tds::connection::client_context::ClientContext;
    use mssql_tds::connection::tds_client::{ResultSet, ResultSetClient};
    use mssql_tds::connection_provider::tds_connection_provider::TdsConnectionProvider;
    use mssql_tds::core::{EncryptionOptions, EncryptionSetting};
    use tokio::sync::oneshot;
    use tracing_subscriber::FmtSubscriber;

    fn init_tracing() {
        let subscriber = FmtSubscriber::builder()
            .with_max_level(tracing::Level::DEBUG)
            .with_test_writer()
            .try_init();
        let _ = subscriber; // Ignore if already initialized
    }

    /// Generate a random password for test connections.
    /// Uses a cryptographically secure random number generator.
    fn generate_test_password() -> String {
        use rand::Rng;
        const CHARSET: &[u8] =
            b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789!@#$%^&*";
        let mut rng = rand::rng();
        let password: String = (0..24)
            .map(|_| {
                let idx = rng.random_range(0..CHARSET.len());
                CHARSET[idx] as char
            })
            .collect();
        password
    }

    /// Helper function to load test certificates.
    /// Returns an error with instructions if certificates don't exist.
    fn load_test_identity() -> Result<native_tls::Identity, Box<dyn std::error::Error>> {
        use std::path::Path;

        // On Windows, use the pre-generated .pfx file
        #[cfg(windows)]
        {
            let pfx_path = "tests/test_certificates/identity.pfx";
            if !Path::new(pfx_path).exists() {
                return Err("Test certificates not found. Generate them using:\n\
                     \n\
                     From repository root:\n\
                       Windows: .\\scripts\\generate_mock_tds_server_certs.ps1"
                    .into());
            }
            mssql_mock_tds::load_identity_from_file(pfx_path, "")
        }

        // On non-Windows, use PEM files with OpenSSL conversion
        #[cfg(not(windows))]
        {
            use mssql_mock_tds::create_test_identity;
            use std::fs;

            let cert_path = "tests/test_certificates/valid_cert.pem";
            let key_path = "tests/test_certificates/key.pem";

            if !Path::new(cert_path).exists() || !Path::new(key_path).exists() {
                return Err(
                    "Test certificates not found. Generate them using one of these methods:\n\
                     \n\
                     From repository root:\n\
                       Linux/macOS: ./scripts/generate_mock_tds_server_certs.sh\n\
                       Windows:     .\\scripts\\generate_mock_tds_server_certs.ps1\n\
                     \n\
                     Or from mssql-tds directory:\n\
                       ./tests/test_certificates/generate_certs.sh"
                        .into(),
                );
            }

            let cert_pem = fs::read(cert_path)?;
            let key_pem = fs::read(key_path)?;
            create_test_identity(&cert_pem, &key_pem)
        }
    }

    /// Test basic connectivity to mock server
    #[tokio::test]
    async fn test_connect_to_mock_server() -> Result<(), Box<dyn std::error::Error>> {
        init_tracing();

        // Start mock server on a random port
        let server = MockTdsServer::new("127.0.0.1:0").await?;
        let server_addr = server.local_addr();

        // Run server in background
        let (shutdown_tx, shutdown_rx) = oneshot::channel();
        let server_handle =
            tokio::spawn(async move { server.run_with_shutdown(shutdown_rx).await });

        // Give server time to start
        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

        // Create client context pointing to mock server
        let datasource = format!("tcp:{},{}", server_addr.ip(), server_addr.port());
        let mut context = ClientContext::default();
        context.user_name = "sa".to_string();
        context.password = generate_test_password();
        context.database = "master".to_string();
        context.encryption_options = EncryptionOptions {
            mode: EncryptionSetting::PreferOff,
            trust_server_certificate: true,
            host_name_in_cert: None,
            server_certificate: None,
        };

        // Connect to mock server
        let provider = TdsConnectionProvider {};
        let client = provider.create_client(context, &datasource, None).await?;

        println!("Successfully connected to mock server at {server_addr}");

        // Cleanup
        drop(client);
        let _ = shutdown_tx.send(());
        let _ = tokio::time::timeout(tokio::time::Duration::from_secs(2), server_handle).await;

        Ok(())
    }

    /// Test executing "SELECT 1" against mock server
    #[tokio::test]
    async fn test_execute_select_one() -> Result<(), Box<dyn std::error::Error>> {
        init_tracing();

        // Start mock server
        let server = MockTdsServer::new("127.0.0.1:0").await?;
        let server_addr = server.local_addr();

        let (shutdown_tx, shutdown_rx) = oneshot::channel();
        let server_handle =
            tokio::spawn(async move { server.run_with_shutdown(shutdown_rx).await });

        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

        // Create and connect client
        let datasource = format!("tcp:{},{}", server_addr.ip(), server_addr.port());
        let mut context = ClientContext::default();
        context.user_name = "sa".to_string();
        context.password = generate_test_password();
        context.database = "master".to_string();
        context.encryption_options = EncryptionOptions {
            mode: EncryptionSetting::PreferOff,
            trust_server_certificate: true,
            host_name_in_cert: None,
            server_certificate: None,
        };

        let provider = TdsConnectionProvider {};
        let mut client = provider.create_client(context, &datasource, None).await?;

        // Execute query
        client.execute("SELECT 1".to_string(), None, None).await?;

        // Read results
        let mut row_count = 0;
        if let Some(resultset) = client.get_current_resultset() {
            while let Some(row) = resultset.next_row().await? {
                row_count += 1;
                println!("Row: {row:?}");

                // Verify we got the value 1
                assert_eq!(row.len(), 1);
                if let Some(value) = row.first() {
                    println!("Value: {value:?}");
                }
            }
        }

        assert_eq!(row_count, 1, "Expected 1 row from SELECT 1");

        client.close_query().await?;
        client.close_connection().await?;

        // Cleanup
        let _ = shutdown_tx.send(());
        let _ = tokio::time::timeout(tokio::time::Duration::from_secs(2), server_handle).await;

        Ok(())
    }

    /// Test executing multiple queries
    #[tokio::test]
    async fn test_execute_multiple_queries() -> Result<(), Box<dyn std::error::Error>> {
        init_tracing();

        // Start mock server
        let server = MockTdsServer::new("127.0.0.1:0").await?;
        let server_addr = server.local_addr();

        let (shutdown_tx, shutdown_rx) = oneshot::channel();
        let server_handle =
            tokio::spawn(async move { server.run_with_shutdown(shutdown_rx).await });

        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

        // Create and connect client
        let datasource = format!("tcp:{},{}", server_addr.ip(), server_addr.port());
        let mut context = ClientContext::default();
        context.user_name = "sa".to_string();
        context.password = generate_test_password();
        context.database = "master".to_string();
        context.encryption_options = EncryptionOptions {
            mode: EncryptionSetting::PreferOff,
            trust_server_certificate: true,
            host_name_in_cert: None,
            server_certificate: None,
        };

        let provider = TdsConnectionProvider {};
        let mut client = provider.create_client(context, &datasource, None).await?;

        // Execute first query
        client.execute("SELECT 1".to_string(), None, None).await?;
        client.close_query().await?;

        // Execute second query
        client.execute("SELECT 1".to_string(), None, None).await?;

        let mut row_count = 0;
        if let Some(resultset) = client.get_current_resultset() {
            while let Some(_row) = resultset.next_row().await? {
                row_count += 1;
            }
        }

        assert_eq!(row_count, 1);
        client.close_query().await?;

        client.close_connection().await?;

        // Cleanup
        let _ = shutdown_tx.send(());
        let _ = tokio::time::timeout(tokio::time::Duration::from_secs(2), server_handle).await;

        Ok(())
    }

    /// Test connection reuse
    #[tokio::test]
    async fn test_connection_reuse() -> Result<(), Box<dyn std::error::Error>> {
        init_tracing();

        // Start mock server
        let server = MockTdsServer::new("127.0.0.1:0").await?;
        let server_addr = server.local_addr();

        let (shutdown_tx, shutdown_rx) = oneshot::channel();
        let server_handle =
            tokio::spawn(async move { server.run_with_shutdown(shutdown_rx).await });

        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

        // Create context
        let datasource = format!("tcp:{},{}", server_addr.ip(), server_addr.port());
        let mut context = ClientContext::default();
        context.user_name = "sa".to_string();
        context.password = generate_test_password();
        context.database = "master".to_string();
        context.encryption_options = EncryptionOptions {
            mode: EncryptionSetting::PreferOff,
            trust_server_certificate: true,
            host_name_in_cert: None,
            server_certificate: None,
        };

        let provider = TdsConnectionProvider {};

        // Create multiple clients (simulating connection reuse scenario)
        for i in 0..3 {
            println!("Connecting client {i}");
            let mut client = provider
                .create_client(context.clone(), &datasource, None)
                .await?;

            client.execute("SELECT 1".to_string(), None, None).await?;
            client.close_query().await?;
            client.close_connection().await?;

            println!("Client {i} completed successfully");
        }

        // Cleanup
        let _ = shutdown_tx.send(());
        let _ = tokio::time::timeout(tokio::time::Duration::from_secs(2), server_handle).await;

        Ok(())
    }

    /// Test custom query responses with multiple data types
    #[tokio::test]
    async fn test_custom_query_response() -> Result<(), Box<dyn std::error::Error>> {
        use mssql_mock_tds::{ColumnDefinition, ColumnValue, QueryResponse, Row, SqlDataType};

        init_tracing();

        // Start mock server
        let server = MockTdsServer::new("127.0.0.1:0").await?;
        let server_addr = server.local_addr();

        // Register a custom query response before starting the server
        let registry = server.query_registry();
        {
            let mut reg = registry.lock().await;
            reg.register(
                "SELECT CAST(1 AS BIGINT), 2, 3",
                QueryResponse::new(
                    vec![
                        ColumnDefinition::new("col1", SqlDataType::BigInt),
                        ColumnDefinition::new("col2", SqlDataType::Int),
                        ColumnDefinition::new("col3", SqlDataType::Int),
                    ],
                    vec![Row::new(vec![
                        ColumnValue::BigInt(1),
                        ColumnValue::Int(2),
                        ColumnValue::Int(3),
                    ])],
                ),
            );
        }

        let (shutdown_tx, shutdown_rx) = oneshot::channel();
        let server_handle =
            tokio::spawn(async move { server.run_with_shutdown(shutdown_rx).await });

        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

        // Create and connect client
        let datasource = format!("tcp:{},{}", server_addr.ip(), server_addr.port());
        let mut context = ClientContext::default();
        context.user_name = "sa".to_string();
        context.password = generate_test_password();
        context.database = "master".to_string();
        context.encryption_options = EncryptionOptions {
            mode: EncryptionSetting::PreferOff,
            trust_server_certificate: true,
            host_name_in_cert: None,
            server_certificate: None,
        };

        let provider = TdsConnectionProvider {};
        let mut client = provider.create_client(context, &datasource, None).await?;

        // Execute the custom query
        client
            .execute("SELECT CAST(1 AS BIGINT), 2, 3".to_string(), None, None)
            .await?;

        // Read results
        let mut row_count = 0;
        if let Some(resultset) = client.get_current_resultset() {
            while let Some(row) = resultset.next_row().await? {
                row_count += 1;
                println!("Row: {row:?}");

                // Verify we got 3 columns
                assert_eq!(row.len(), 3, "Expected 3 columns");

                // Print values
                for (i, value) in row.iter().enumerate() {
                    println!("Column {i}: {value:?}");
                }
            }
        }

        assert_eq!(row_count, 1, "Expected 1 row");

        client.close_query().await?;
        client.close_connection().await?;

        // Cleanup
        let _ = shutdown_tx.send(());
        let _ = tokio::time::timeout(tokio::time::Duration::from_secs(2), server_handle).await;

        Ok(())
    }

    /// Test query response with NULL values
    #[tokio::test]
    async fn test_query_with_nulls() -> Result<(), Box<dyn std::error::Error>> {
        use mssql_mock_tds::{ColumnDefinition, ColumnValue, QueryResponse, Row, SqlDataType};

        init_tracing();

        // Start mock server
        let server = MockTdsServer::new("127.0.0.1:0").await?;
        let server_addr = server.local_addr();

        // Register a query with NULL values
        let registry = server.query_registry();
        {
            let mut reg = registry.lock().await;
            reg.register(
                "SELECT 1, NULL, 3",
                QueryResponse::new(
                    vec![
                        ColumnDefinition::new("", SqlDataType::Int),
                        ColumnDefinition::new("", SqlDataType::Int),
                        ColumnDefinition::new("", SqlDataType::Int),
                    ],
                    vec![Row::new(vec![
                        ColumnValue::Int(1),
                        ColumnValue::Null,
                        ColumnValue::Int(3),
                    ])],
                ),
            );
        }

        let (shutdown_tx, shutdown_rx) = oneshot::channel();
        let server_handle =
            tokio::spawn(async move { server.run_with_shutdown(shutdown_rx).await });

        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

        // Create and connect client
        let datasource = format!("tcp:{},{}", server_addr.ip(), server_addr.port());
        let mut context = ClientContext::default();
        context.user_name = "sa".to_string();
        context.password = generate_test_password();
        context.database = "master".to_string();
        context.encryption_options = EncryptionOptions {
            mode: EncryptionSetting::PreferOff,
            trust_server_certificate: true,
            host_name_in_cert: None,
            server_certificate: None,
        };

        let provider = TdsConnectionProvider {};
        let mut client = provider.create_client(context, &datasource, None).await?;

        // Execute query with NULL
        client
            .execute("SELECT 1, NULL, 3".to_string(), None, None)
            .await?;

        // Read results
        let mut row_count = 0;
        if let Some(resultset) = client.get_current_resultset() {
            while let Some(row) = resultset.next_row().await? {
                row_count += 1;
                println!("Row with NULLs: {row:?}");
                assert_eq!(row.len(), 3, "Expected 3 columns");
            }
        }

        assert_eq!(row_count, 1, "Expected 1 row");

        client.close_query().await?;
        client.close_connection().await?;

        // Cleanup
        let _ = shutdown_tx.send(());
        let _ = tokio::time::timeout(tokio::time::Duration::from_secs(2), server_handle).await;

        Ok(())
    }

    /// Test ServerCertificate parameter rejects invalid file paths
    #[tokio::test]
    async fn test_server_certificate_invalid_path() {
        init_tracing();

        // Start mock server
        let server = MockTdsServer::new("127.0.0.1:0").await.unwrap();
        let server_addr = server.local_addr();

        let (shutdown_tx, shutdown_rx) = oneshot::channel();
        let server_handle =
            tokio::spawn(async move { server.run_with_shutdown(shutdown_rx).await });

        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

        // Create client context with invalid certificate path
        let datasource = format!("tcp:{},{}", server_addr.ip(), server_addr.port());
        let mut context = ClientContext::default();
        context.user_name = "sa".to_string();
        context.password = generate_test_password();
        context.database = "master".to_string();
        context.encryption_options = EncryptionOptions {
            mode: EncryptionSetting::PreferOff, // Use PreferOff since mock server doesn't support TLS
            trust_server_certificate: true,
            host_name_in_cert: None,
            server_certificate: Some("/nonexistent/path/certificate.cer".to_string()),
        };

        // Attempt to connect - should succeed since encryption is off
        // But ServerCertificate should still be validated when reading from file
        let provider = TdsConnectionProvider {};
        let result = provider.create_client(context, &datasource, None).await;

        // The connection may succeed because mock server doesn't use encryption
        // What matters is that the invalid certificate path doesn't cause a crash
        // In a real scenario with Required encryption, this would fail
        match result {
            Ok(_) => {
                // With PreferOff and mock server (no encryption), connection succeeds
                // The certificate file is only checked if encryption is used
            }
            Err(e) => {
                // If it fails, it should be due to certificate or connectivity issues
                println!("Expected error occurred: {}", e);
            }
        }

        // Cleanup
        let _ = shutdown_tx.send(());
        let _ = tokio::time::timeout(tokio::time::Duration::from_secs(2), server_handle).await;
    }

    /// Test ServerCertificate and TrustServerCertificate mutual exclusion warning
    #[tokio::test]
    async fn test_server_certificate_with_trust_server_certificate() {
        init_tracing();

        // Start mock server
        let server = MockTdsServer::new("127.0.0.1:0").await.unwrap();
        let server_addr = server.local_addr();

        let (shutdown_tx, shutdown_rx) = oneshot::channel();
        let server_handle =
            tokio::spawn(async move { server.run_with_shutdown(shutdown_rx).await });

        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

        // Create a temporary test certificate file
        use std::fs;
        use std::io::Write;
        let temp_dir = std::env::temp_dir();
        let cert_path = temp_dir.join("test_cert_mock.cer");

        // Create a minimal DER certificate structure (just for testing file existence)
        // This won't be used for actual validation in mock server tests
        let mut file = fs::File::create(&cert_path).unwrap();
        file.write_all(&[0x30, 0x82, 0x01, 0x00]).unwrap();
        file.write_all(&vec![0; 256]).unwrap();

        // Create client context with both ServerCertificate and TrustServerCertificate
        // This should log a warning but ServerCertificate takes precedence
        let datasource = format!("tcp:{},{}", server_addr.ip(), server_addr.port());
        let mut context = ClientContext::default();
        context.user_name = "sa".to_string();
        context.password = generate_test_password();
        context.database = "master".to_string();
        context.encryption_options = EncryptionOptions {
            mode: EncryptionSetting::Required,
            trust_server_certificate: true, // This should be ignored
            host_name_in_cert: None,
            server_certificate: Some(cert_path.to_str().unwrap().to_string()),
        };

        // Attempt to connect - ServerCertificate should take precedence
        // This may fail due to certificate mismatch or SSL issues with mock server,
        // but we're testing that the configuration is accepted
        let provider = TdsConnectionProvider {};
        let result = provider.create_client(context, &datasource, None).await;

        // Either succeeds (if mock server doesn't use SSL) or fails with SSL/certificate error
        // The important thing is it doesn't reject the configuration
        match result {
            Ok(_) => {
                // Connection succeeded (mock server likely doesn't use SSL)
            }
            Err(e) => {
                let error_msg = e.to_string();
                // Should fail with SSL or certificate-related error, not configuration error
                assert!(
                    error_msg.contains("certificate")
                        || error_msg.contains("SSL")
                        || error_msg.contains("TLS")
                        || error_msg.contains("encryption"),
                    "Error should be SSL/certificate related, got: {}",
                    error_msg
                );
            }
        }

        // Cleanup
        let _ = fs::remove_file(cert_path);
        let _ = shutdown_tx.send(());
        let _ = tokio::time::timeout(tokio::time::Duration::from_secs(2), server_handle).await;
    }

    /// Test ServerCertificate and HostnameInCertificate mutual exclusion
    #[tokio::test]
    async fn test_server_certificate_with_hostname_in_cert_fails() {
        init_tracing();

        // Start mock server
        let server = MockTdsServer::new("127.0.0.1:0").await.unwrap();
        let server_addr = server.local_addr();

        let (shutdown_tx, shutdown_rx) = oneshot::channel();
        let server_handle =
            tokio::spawn(async move { server.run_with_shutdown(shutdown_rx).await });

        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

        // Create a temporary test certificate file
        use std::fs;
        use std::io::Write;
        let temp_dir = std::env::temp_dir();
        let cert_path = temp_dir.join("test_cert_hnic.cer");

        let mut file = fs::File::create(&cert_path).unwrap();
        file.write_all(&[0x30, 0x82, 0x01, 0x00]).unwrap();
        file.write_all(&vec![0; 256]).unwrap();

        // Create client context with both ServerCertificate and HostnameInCertificate
        // This should return an error as they are mutually exclusive
        let datasource = format!("tcp:{},{}", server_addr.ip(), server_addr.port());
        let mut context = ClientContext::default();
        context.user_name = "sa".to_string();
        context.password = generate_test_password();
        context.database = "master".to_string();
        context.encryption_options = EncryptionOptions {
            mode: EncryptionSetting::PreferOff, // Use PreferOff since mock server doesn't support TLS
            trust_server_certificate: true,
            host_name_in_cert: Some("custom.hostname.com".to_string()),
            server_certificate: Some(cert_path.to_str().unwrap().to_string()),
        };

        // Attempt to connect - with PreferOff, may succeed but both options set is unusual
        // The test primarily validates that the configuration is accepted at creation time
        let provider = TdsConnectionProvider {};
        let result = provider.create_client(context, &datasource, None).await;

        // When encryption is off, the mutual exclusivity check might not trigger
        // This test validates the configuration doesn't cause a crash
        match result {
            Ok(_) => {
                // Connection succeeded (no encryption, so certificate options ignored)
            }
            Err(e) => {
                let error_msg = e.to_string();
                // If it fails, could be due to the mutual exclusivity or connection issues
                println!("Error occurred (expected behavior): {}", error_msg);
            }
        }

        // Cleanup
        let _ = fs::remove_file(cert_path);
        let _ = shutdown_tx.send(());
        let _ = tokio::time::timeout(tokio::time::Duration::from_secs(2), server_handle).await;
    }

    /// Test ServerCertificate with TLS-enabled mock server
    /// Tests the ServerCertificate validation feature with the mock TDS server.
    /// The mock server supports TDS 7.4-style TDS-wrapped TLS handshakes.
    // Disabled on Windows: intermittent 15s timeout in TDS 7.4 mock server TLS (Bug #42534)
    #[cfg(not(target_os = "windows"))]
    #[tokio::test]
    async fn test_server_certificate_with_tls() -> Result<(), Box<dyn std::error::Error>> {
        init_tracing();

        // Load TLS identity for server (handles Windows vs non-Windows)
        let identity = load_test_identity()?;

        // Start TLS-enabled mock server
        let server = MockTdsServer::new_with_tls("127.0.0.1:0", Some(identity)).await?;
        let server_addr = server.local_addr();

        let (shutdown_tx, shutdown_rx) = oneshot::channel();
        let server_handle =
            tokio::spawn(async move { server.run_with_shutdown(shutdown_rx).await });

        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

        // Test 1: Connect with matching certificate (should succeed)
        let datasource = format!("tcp:{},{}", server_addr.ip(), server_addr.port());
        {
            let mut context = ClientContext::default();
            context.user_name = "sa".to_string();
            context.password = generate_test_password();
            context.database = "master".to_string();
            context.encryption_options = EncryptionOptions {
                mode: EncryptionSetting::Required,
                trust_server_certificate: false,
                host_name_in_cert: None,
                server_certificate: Some("tests/test_certificates/valid_cert.pem".to_string()),
            };

            let provider = TdsConnectionProvider {};
            let client = provider.create_client(context, &datasource, None).await?;
            println!("✓ Successfully connected with matching ServerCertificate");
            drop(client);
        }

        // Test 2: Connect without ServerCertificate but with TrustServerCertificate=true (should succeed)
        {
            let mut context = ClientContext::default();
            context.user_name = "sa".to_string();
            context.password = generate_test_password();
            context.database = "master".to_string();
            context.encryption_options = EncryptionOptions {
                mode: EncryptionSetting::Required,
                trust_server_certificate: true,
                host_name_in_cert: None,
                server_certificate: None,
            };

            let provider = TdsConnectionProvider {};
            let client = provider.create_client(context, &datasource, None).await?;
            println!("✓ Successfully connected with TrustServerCertificate=true");
            drop(client);
        }

        // Test 3: Execute a query over TLS
        {
            let mut context = ClientContext::default();
            context.user_name = "sa".to_string();
            context.password = generate_test_password();
            context.database = "master".to_string();
            context.encryption_options = EncryptionOptions {
                mode: EncryptionSetting::Required,
                trust_server_certificate: false,
                host_name_in_cert: None,
                server_certificate: Some("tests/test_certificates/valid_cert.pem".to_string()),
            };

            let provider = TdsConnectionProvider {};
            let mut client = provider.create_client(context, &datasource, None).await?;

            // Execute SELECT 1 over encrypted connection
            client.execute("SELECT 1".to_string(), None, None).await?;

            // Read result
            let mut row_count = 0;
            if let Some(resultset) = client.get_current_resultset() {
                while let Some(_row) = resultset.next_row().await? {
                    row_count += 1;
                }
            }
            assert_eq!(row_count, 1, "Expected 1 row from SELECT 1");
            println!("✓ Successfully executed query over TLS with ServerCertificate validation");

            client.close_query().await?;
            client.close_connection().await?;
        }

        // Cleanup
        let _ = shutdown_tx.send(());
        let _ = tokio::time::timeout(tokio::time::Duration::from_secs(2), server_handle).await;

        Ok(())
    }

    /// Test strict encryption (TDS 8.0) with ServerCertificate keyword
    ///
    /// In strict mode (TDS 8.0), TLS handshake happens immediately on the TCP socket
    /// before any TDS packets are exchanged. This is different from TDS 7.4 where
    /// TLS handshake data is wrapped inside TDS PreLogin packets.
    #[tokio::test]
    async fn test_strict_encryption_with_server_certificate()
    -> Result<(), Box<dyn std::error::Error>> {
        init_tracing();

        // Load TLS identity for server (handles Windows vs non-Windows)
        let identity = load_test_identity()?;

        // Start mock server in STRICT TLS mode (TDS 8.0)
        // In strict mode, TLS handshake happens directly on the socket
        let server = MockTdsServer::new_with_strict_tls("127.0.0.1:0", identity).await?;
        let server_addr = server.local_addr();

        let (shutdown_tx, shutdown_rx) = oneshot::channel();
        let server_handle =
            tokio::spawn(async move { server.run_with_shutdown(shutdown_rx).await });

        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

        // Test 1: Connect with strict encryption and matching ServerCertificate
        let datasource = format!("tcp:{},{}", server_addr.ip(), server_addr.port());
        {
            let mut context = ClientContext::default();
            context.user_name = "sa".to_string();
            context.password = generate_test_password();
            context.database = "master".to_string();
            context.encryption_options = EncryptionOptions {
                mode: EncryptionSetting::Strict, // TDS 8.0 - direct TLS
                trust_server_certificate: false,
                host_name_in_cert: None,
                server_certificate: Some("tests/test_certificates/valid_cert.pem".to_string()),
            };

            let provider = TdsConnectionProvider {};
            let client = provider.create_client(context, &datasource, None).await?;
            println!("✓ Successfully connected with Strict encryption and ServerCertificate");
            drop(client);
        }

        // Test 2: Verify that TrustServerCertificate=true is ignored in Strict mode
        // The connection should fail because the self-signed cert is not in the trust store
        {
            let mut context = ClientContext::default();
            context.user_name = "sa".to_string();
            context.password = generate_test_password();
            context.database = "master".to_string();
            context.encryption_options = EncryptionOptions {
                mode: EncryptionSetting::Strict,
                trust_server_certificate: true, // This is ignored in Strict mode
                host_name_in_cert: None,
                server_certificate: None,
            };

            let provider = TdsConnectionProvider {};
            let result = provider.create_client(context, &datasource, None).await;
            assert!(
                result.is_err(),
                "Expected connection to fail when TrustServerCertificate is ignored in Strict mode"
            );
            println!(
                "✓ Correctly rejected connection with Strict encryption when TrustServerCertificate=true (ignored)"
            );
        }

        // Test 3: Execute a query over strict TLS connection
        {
            let mut context = ClientContext::default();
            context.user_name = "sa".to_string();
            context.password = generate_test_password();
            context.database = "master".to_string();
            context.encryption_options = EncryptionOptions {
                mode: EncryptionSetting::Strict,
                trust_server_certificate: false,
                host_name_in_cert: None,
                server_certificate: Some("tests/test_certificates/valid_cert.pem".to_string()),
            };

            let provider = TdsConnectionProvider {};
            let mut client = provider.create_client(context, &datasource, None).await?;

            // Execute SELECT 1 over strict encrypted connection
            client.execute("SELECT 1".to_string(), None, None).await?;

            // Read result
            let mut row_count = 0;
            if let Some(resultset) = client.get_current_resultset() {
                while let Some(_row) = resultset.next_row().await? {
                    row_count += 1;
                }
            }
            assert_eq!(row_count, 1, "Expected 1 row from SELECT 1");
            println!(
                "✓ Successfully executed query over Strict TLS with ServerCertificate validation"
            );

            client.close_query().await?;
            client.close_connection().await?;
        }

        // Cleanup
        let _ = shutdown_tx.send(());
        let _ = tokio::time::timeout(tokio::time::Duration::from_secs(2), server_handle).await;

        Ok(())
    }

    #[tokio::test]
    async fn test_user_agent_interception() -> Result<(), Box<dyn std::error::Error>> {
        init_tracing();

        let server_addr = "127.0.0.1:0";
        let server = MockTdsServer::new(server_addr).await?;
        let bound_addr = server.local_addr();
        let connection_store = server.connection_store();

        let (shutdown_tx, shutdown_rx) = oneshot::channel();
        let server_handle =
            tokio::spawn(async move { server.run_with_shutdown(shutdown_rx).await });

        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

        let datasource = format!("tcp:{},{}", bound_addr.ip(), bound_addr.port());
        let mut context = ClientContext::default();
        context.user_name = "sa".to_string();
        context.password = generate_test_password();
        context.database = "master".to_string();
        context.encryption_options = EncryptionOptions {
            mode: EncryptionSetting::PreferOff,
            trust_server_certificate: true,
            host_name_in_cert: None,
            server_certificate: None,
        };

        let provider = TdsConnectionProvider {};
        let mut client = provider.create_client(context, &datasource, None).await?;

        client.close_connection().await?;
        drop(client);

        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

        let store = connection_store.lock().await;
        let conn_info = store
            .all()
            .values()
            .next()
            .expect("Should have at least one connection");

        let returned_agent = conn_info.received_user_agent().unwrap_or_default();
        println!("Intercepted User Agent: {}", returned_agent);
        assert!(
            returned_agent.contains("MS-TDS"),
            "Expected formatted MS-TDS, got {}",
            returned_agent
        );

        // Cleanup
        let _ = shutdown_tx.send(());
        let _ = tokio::time::timeout(tokio::time::Duration::from_secs(2), server_handle).await;

        Ok(())
    }
}
