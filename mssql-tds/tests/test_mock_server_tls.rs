// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#![cfg(feature = "native-tls-backend")]

//! Tests for Mock TDS Server TLS capabilities
//!
//! These tests validate the TLS/SSL functionality of the mock TDS server,
//! including both TDS 7.4-style (wrapped TLS handshake) and TDS 8.0-style
//! (strict/direct TLS) modes.
//!
//! **Prerequisites:** Before running these tests, generate the test certificates:
//! ```bash
//! cd mssql-tds/tests/test_certificates && ./generate_certs.sh
//! ```

#[cfg(test)]
mod mock_server_tls_tests {
    use mssql_mock_tds::MockTdsServer;
    #[cfg(not(windows))]
    use mssql_mock_tds::create_test_identity;
    use mssql_tds::connection::client_context::ClientContext;
    use mssql_tds::connection::tds_client::{ResultSet, ResultSetClient};
    use mssql_tds::connection_provider::tds_connection_provider::TdsConnectionProvider;
    use mssql_tds::core::{EncryptionOptions, EncryptionSetting};
    #[cfg(not(windows))]
    use std::fs;
    use std::path::Path;
    use tokio::sync::oneshot;
    use tracing_subscriber::FmtSubscriber;

    fn init_tracing() {
        let subscriber = FmtSubscriber::builder()
            .with_max_level(tracing::Level::DEBUG)
            .with_test_writer()
            .try_init();
        let _ = subscriber; // Ignore if already initialized
    }

    /// Helper function to load test certificates.
    /// Returns an error with instructions if certificates don't exist.
    fn load_test_identity() -> Result<native_tls::Identity, Box<dyn std::error::Error>> {
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
    /// Test that mock server can be created with TLS enabled
    #[tokio::test]
    async fn test_mock_server_tls_creation() -> Result<(), Box<dyn std::error::Error>> {
        init_tracing();

        let identity = load_test_identity()?;

        // Create TLS-enabled mock server
        let server = MockTdsServer::new_with_tls("127.0.0.1:0", Some(identity)).await?;
        let server_addr = server.local_addr();

        println!("✓ Mock TDS Server with TLS created on {}", server_addr);

        Ok(())
    }

    /// Test that mock server can be created in strict TLS mode (TDS 8.0)
    #[tokio::test]
    async fn test_mock_server_strict_tls_creation() -> Result<(), Box<dyn std::error::Error>> {
        init_tracing();

        let identity = load_test_identity()?;

        // Create strict TLS mock server (TDS 8.0 mode)
        let server = MockTdsServer::new_with_strict_tls("127.0.0.1:0", identity).await?;
        let server_addr = server.local_addr();

        println!(
            "✓ Mock TDS Server with Strict TLS (TDS 8.0) created on {}",
            server_addr
        );

        Ok(())
    }

    /// Test connecting to TLS-enabled mock server with TrustServerCertificate=true
    ///
    /// This test validates that:
    /// 1. The mock server correctly performs TDS 7.4-style TLS handshake
    /// 2. The client can connect when trusting the server certificate
    /// 3. The TdsTlsWrapper correctly handles TDS packet wrapping/unwrapping
    // Disabled on Windows: intermittent 15s timeout in TDS 7.4 mock server TLS (Bug #42534)
    #[cfg(not(target_os = "windows"))]
    #[tokio::test]
    async fn test_connect_with_trust_server_certificate() -> Result<(), Box<dyn std::error::Error>>
    {
        init_tracing();

        let identity = load_test_identity()?;

        // Start TLS-enabled mock server
        let server = MockTdsServer::new_with_tls("127.0.0.1:0", Some(identity)).await?;
        let server_addr = server.local_addr();

        let (shutdown_tx, shutdown_rx) = oneshot::channel();
        let server_handle =
            tokio::spawn(async move { server.run_with_shutdown(shutdown_rx).await });

        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

        // Connect with encryption required and trust_server_certificate=true
        let datasource = format!("tcp:{},{}", server_addr.ip(), server_addr.port());
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

        println!(
            "✓ Successfully connected to TLS-enabled mock server with TrustServerCertificate=true"
        );

        drop(client);
        let _ = shutdown_tx.send(());
        let _ = tokio::time::timeout(tokio::time::Duration::from_secs(2), server_handle).await;

        Ok(())
    }

    /// Test executing a query over TLS connection with TrustServerCertificate=true
    ///
    /// This test validates that:
    /// 1. TLS connection is properly established
    /// 2. SQL queries can be executed over the encrypted connection
    /// 3. Results are correctly returned through the encrypted channel
    // Disabled on Windows: intermittent 15s timeout in TDS 7.4 mock server TLS (Bug #42534)
    #[cfg(not(target_os = "windows"))]
    #[tokio::test]
    async fn test_execute_query_over_tls_with_trust_certificate()
    -> Result<(), Box<dyn std::error::Error>> {
        init_tracing();

        let identity = load_test_identity()?;

        // Start TLS-enabled mock server
        let server = MockTdsServer::new_with_tls("127.0.0.1:0", Some(identity)).await?;
        let server_addr = server.local_addr();

        let (shutdown_tx, shutdown_rx) = oneshot::channel();
        let server_handle =
            tokio::spawn(async move { server.run_with_shutdown(shutdown_rx).await });

        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

        // Connect with TLS
        let datasource = format!("tcp:{},{}", server_addr.ip(), server_addr.port());
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
        let mut client = provider.create_client(context, &datasource, None).await?;

        // Execute SELECT 1 over encrypted connection
        client.execute("SELECT 1".to_string(), None, None).await?;

        // Read result
        let mut row_count = 0;
        if let Some(resultset) = client.get_current_resultset() {
            while let Some(row) = resultset.next_row().await? {
                row_count += 1;
                println!("Row over TLS: {:?}", row);
            }
        }

        assert_eq!(row_count, 1, "Expected 1 row from SELECT 1");
        println!("✓ Successfully executed query over TLS connection");

        client.close_query().await?;
        client.close_connection().await?;

        let _ = shutdown_tx.send(());
        let _ = tokio::time::timeout(tokio::time::Duration::from_secs(2), server_handle).await;

        Ok(())
    }

    /// Test that TrustServerCertificate=true is ignored in strict TLS (TDS 8.0) mode
    ///
    /// In strict mode, TrustServerCertificate is ignored and certificate validation
    /// is always enforced. This test verifies the connection fails with a self-signed cert.
    #[tokio::test]
    async fn test_connect_strict_tls_with_trust_certificate()
    -> Result<(), Box<dyn std::error::Error>> {
        init_tracing();

        let identity = load_test_identity()?;

        // Start mock server in STRICT TLS mode (TDS 8.0)
        let server = MockTdsServer::new_with_strict_tls("127.0.0.1:0", identity).await?;
        let server_addr = server.local_addr();

        let (shutdown_tx, shutdown_rx) = oneshot::channel();
        let server_handle =
            tokio::spawn(async move { server.run_with_shutdown(shutdown_rx).await });

        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

        // Connect with strict encryption and trust_server_certificate=true
        // This should fail because TrustServerCertificate is ignored in Strict mode
        let datasource = format!("tcp:{},{}", server_addr.ip(), server_addr.port());
        let mut context = ClientContext::default();
        context.user_name = "sa".to_string();
        context.password = generate_test_password();
        context.database = "master".to_string();
        context.encryption_options = EncryptionOptions {
            mode: EncryptionSetting::Strict,
            trust_server_certificate: true, // Ignored in Strict mode
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
            "Correctly rejected connection with Strict TLS when TrustServerCertificate=true (ignored)"
        );

        let _ = shutdown_tx.send(());
        let _ = tokio::time::timeout(tokio::time::Duration::from_secs(2), server_handle).await;

        Ok(())
    }

    /// Test executing a query over strict TLS (TDS 8.0) connection
    #[tokio::test]
    async fn test_execute_query_over_strict_tls() -> Result<(), Box<dyn std::error::Error>> {
        init_tracing();

        let identity = load_test_identity()?;

        // Start strict TLS mock server
        let server = MockTdsServer::new_with_strict_tls("127.0.0.1:0", identity).await?;
        let server_addr = server.local_addr();

        let (shutdown_tx, shutdown_rx) = oneshot::channel();
        let server_handle =
            tokio::spawn(async move { server.run_with_shutdown(shutdown_rx).await });

        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

        // Connect with strict TLS using ServerCertificate for certificate pinning
        let datasource = format!("tcp:{},{}", server_addr.ip(), server_addr.port());
        let mut context = ClientContext::default();
        context.user_name = "sa".to_string();
        context.password = generate_test_password();
        context.database = "master".to_string();
        context.encryption_options = EncryptionOptions {
            mode: EncryptionSetting::Strict,
            trust_server_certificate: false,
            host_name_in_cert: None,
            server_certificate: Some("tests/test_certificates/valid_cert.pem".into()),
        };

        let provider = TdsConnectionProvider {};
        let mut client = provider.create_client(context, &datasource, None).await?;

        // Execute query
        client.execute("SELECT 1".to_string(), None, None).await?;

        let mut row_count = 0;
        if let Some(resultset) = client.get_current_resultset() {
            while let Some(row) = resultset.next_row().await? {
                row_count += 1;
                println!("Row over Strict TLS: {:?}", row);
            }
        }

        assert_eq!(row_count, 1, "Expected 1 row from SELECT 1");
        println!("✓ Successfully executed query over Strict TLS (TDS 8.0) connection");

        client.close_query().await?;
        client.close_connection().await?;

        let _ = shutdown_tx.send(());
        let _ = tokio::time::timeout(tokio::time::Duration::from_secs(2), server_handle).await;

        Ok(())
    }

    /// Test multiple queries over a single TLS connection
    ///
    /// This validates that the TLS session remains stable across multiple queries.
    // Disabled on Windows: intermittent 15s timeout in TDS 7.4 mock server TLS (Bug #42534)
    #[cfg(not(target_os = "windows"))]
    #[tokio::test]
    async fn test_multiple_queries_over_tls() -> Result<(), Box<dyn std::error::Error>> {
        init_tracing();

        let identity = load_test_identity()?;

        let server = MockTdsServer::new_with_tls("127.0.0.1:0", Some(identity)).await?;
        let server_addr = server.local_addr();

        let (shutdown_tx, shutdown_rx) = oneshot::channel();
        let server_handle =
            tokio::spawn(async move { server.run_with_shutdown(shutdown_rx).await });

        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

        let datasource = format!("tcp:{},{}", server_addr.ip(), server_addr.port());
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
        let mut client = provider.create_client(context, &datasource, None).await?;

        // Execute multiple queries
        for i in 1..=3 {
            client.execute("SELECT 1".to_string(), None, None).await?;

            let mut row_count = 0;
            if let Some(resultset) = client.get_current_resultset() {
                while let Some(_row) = resultset.next_row().await? {
                    row_count += 1;
                }
            }

            assert_eq!(row_count, 1, "Query {} should return 1 row", i);
            client.close_query().await?;

            println!("✓ Query {} executed successfully over TLS", i);
        }

        client.close_connection().await?;

        let _ = shutdown_tx.send(());
        let _ = tokio::time::timeout(tokio::time::Duration::from_secs(2), server_handle).await;

        Ok(())
    }

    /// Test multiple TLS connections to the same server
    ///
    /// This validates that the mock server can handle multiple concurrent TLS clients.
    // Disabled on Windows: intermittent 15s timeout in TDS 7.4 mock server TLS (Bug #42534)
    #[cfg(not(target_os = "windows"))]
    #[tokio::test]
    async fn test_multiple_tls_connections() -> Result<(), Box<dyn std::error::Error>> {
        init_tracing();

        let identity = load_test_identity()?;

        let server = MockTdsServer::new_with_tls("127.0.0.1:0", Some(identity)).await?;
        let server_addr = server.local_addr();

        let (shutdown_tx, shutdown_rx) = oneshot::channel();
        let server_handle =
            tokio::spawn(async move { server.run_with_shutdown(shutdown_rx).await });

        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

        let datasource = format!("tcp:{},{}", server_addr.ip(), server_addr.port());
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

        // Create multiple connections sequentially
        for i in 1..=3 {
            let mut client = provider
                .create_client(context.clone(), &datasource, None)
                .await?;

            client.execute("SELECT 1".to_string(), None, None).await?;
            client.close_query().await?;
            client.close_connection().await?;

            println!("✓ Connection {} completed successfully over TLS", i);
        }

        let _ = shutdown_tx.send(());
        let _ = tokio::time::timeout(tokio::time::Duration::from_secs(2), server_handle).await;

        Ok(())
    }

    /// Test TLS connection with custom query responses
    ///
    /// This validates that custom query responses work correctly over TLS.
    // Disabled on Windows: intermittent 15s timeout in TDS 7.4 mock server TLS (Bug #42534)
    #[cfg(not(target_os = "windows"))]
    #[tokio::test]
    async fn test_custom_query_response_over_tls() -> Result<(), Box<dyn std::error::Error>> {
        use mssql_mock_tds::{ColumnDefinition, ColumnValue, QueryResponse, Row, SqlDataType};

        init_tracing();

        let identity = load_test_identity()?;

        let server = MockTdsServer::new_with_tls("127.0.0.1:0", Some(identity)).await?;
        let server_addr = server.local_addr();

        // Register a custom query response with integer types (currently supported)
        let registry = server.query_registry();
        {
            let mut reg = registry.lock().await;
            reg.register(
                "SELECT id, age FROM users",
                QueryResponse::new(
                    vec![
                        ColumnDefinition::new("id", SqlDataType::Int),
                        ColumnDefinition::new("age", SqlDataType::Int),
                    ],
                    vec![
                        Row::new(vec![ColumnValue::Int(1), ColumnValue::Int(30)]),
                        Row::new(vec![ColumnValue::Int(2), ColumnValue::Int(25)]),
                    ],
                ),
            );
        }

        let (shutdown_tx, shutdown_rx) = oneshot::channel();
        let server_handle =
            tokio::spawn(async move { server.run_with_shutdown(shutdown_rx).await });

        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

        let datasource = format!("tcp:{},{}", server_addr.ip(), server_addr.port());
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
        let mut client = provider.create_client(context, &datasource, None).await?;

        // Execute the custom query
        client
            .execute("SELECT id, age FROM users".to_string(), None, None)
            .await?;

        let mut row_count = 0;
        if let Some(resultset) = client.get_current_resultset() {
            while let Some(row) = resultset.next_row().await? {
                row_count += 1;
                println!("Custom query row over TLS: {:?}", row);
            }
        }

        assert_eq!(row_count, 2, "Expected 2 rows from custom query");
        println!("✓ Custom query response works correctly over TLS");

        client.close_query().await?;
        client.close_connection().await?;

        let _ = shutdown_tx.send(());
        let _ = tokio::time::timeout(tokio::time::Duration::from_secs(2), server_handle).await;

        Ok(())
    }

    /// Test that TdsTlsWrapper module is properly exported from mssql_mock_tds
    #[test]
    fn test_tds_tls_wrapper_exported() {
        // This test verifies that TdsTlsWrapper is properly exported
        // The fact that this compiles is the test
        use mssql_mock_tds::TdsTlsWrapper;
        let _ = std::any::TypeId::of::<TdsTlsWrapper>();
        println!("✓ TdsTlsWrapper is properly exported from mssql_mock_tds");
    }

    /// Test that tls_helper functions are properly exported
    #[test]
    fn test_tls_helper_exports() {
        // Verify that tls_helper functions are exported
        use mssql_mock_tds::{create_test_identity, load_identity_from_file};

        // These are function pointers, just verify they exist
        let _create_fn = create_test_identity;
        let _load_fn = load_identity_from_file;

        println!("✓ TLS helper functions are properly exported from mssql_mock_tds");
    }
}
