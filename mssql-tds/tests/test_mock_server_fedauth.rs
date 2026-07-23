// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Integration tests for FedAuth (Access Token) authentication using the mock TDS server

#[cfg(test)]
mod mock_server_fedauth_tests {
    use mssql_mock_tds::MockTdsServer;
    use mssql_tds::connection::client_context::{
        ClientContext, EntraIdTokenFactory, TdsAuthenticationMethod,
    };
    use mssql_tds::connection_provider::tds_connection_provider::TdsConnectionProvider;
    use mssql_tds::core::{EncryptionOptions, EncryptionSetting, TdsResult};
    use std::sync::{Arc, Mutex};
    use tokio::sync::oneshot;
    use tracing_subscriber::FmtSubscriber;

    fn init_tracing() {
        let subscriber = FmtSubscriber::builder()
            .with_max_level(tracing::Level::DEBUG)
            .with_test_writer()
            .try_init();
        let _ = subscriber; // Ignore if already initialized
    }

    /// Generate a mock access token for testing.
    /// In a real scenario, this would be an actual Azure AD / Entra ID token.
    fn generate_mock_access_token() -> String {
        // The access token should be a valid JWT-like structure, but for mock testing
        // we just need a non-empty string that the mock server will accept
        "mock_access_token_for_testing_12345".to_string()
    }

    #[derive(Clone)]
    struct MockEntraFactory {
        token: String,
        calls: Arc<Mutex<Vec<(String, String, TdsAuthenticationMethod)>>>,
    }

    #[async_trait::async_trait]
    impl EntraIdTokenFactory for MockEntraFactory {
        async fn create_token(
            &self,
            spn: String,
            sts_url: String,
            auth_method: TdsAuthenticationMethod,
        ) -> TdsResult<Vec<u8>> {
            self.calls
                .lock()
                .expect("factory call capture mutex poisoned")
                .push((spn, sts_url, auth_method));
            Ok(self
                .token
                .encode_utf16()
                .flat_map(|unit| unit.to_le_bytes())
                .collect())
        }
    }

    /// Test basic connectivity to mock server using access token authentication
    /// and verify the exact token received by the server matches what was sent.
    #[tokio::test]
    async fn test_connect_with_access_token() -> Result<(), Box<dyn std::error::Error>> {
        init_tracing();

        // Start mock server with FedAuth support on a random port
        let server = MockTdsServer::new("127.0.0.1:0").await?;
        let server_addr = server.local_addr();

        // Get a reference to the connection store BEFORE running the server
        // This allows us to verify the token after connection
        let connection_store = server.connection_store();

        // Run server in background
        let (shutdown_tx, shutdown_rx) = oneshot::channel();
        let server_handle =
            tokio::spawn(async move { server.run_with_shutdown(shutdown_rx).await });

        // Give server time to start
        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

        // The token we're sending
        let access_token = generate_mock_access_token();

        // Create client context with access token authentication
        let datasource = format!("tcp:{},{}", server_addr.ip(), server_addr.port());
        let mut context = ClientContext::default();
        context.access_token = Some(access_token.clone());
        context.tds_authentication_method = TdsAuthenticationMethod::AccessToken;
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

        println!(
            "Successfully connected to mock server at {} using access token",
            server_addr
        );

        // Close the client to trigger connection completion and storage
        drop(client);

        // Give the server time to store connection info
        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

        // IMPORTANT: Verify the token received by the server matches what we sent
        {
            let store = connection_store.lock().await;
            assert_eq!(
                store.count(),
                1,
                "Server should have stored exactly one connection"
            );

            // Get the first (and only) connection info
            let conn_info = store
                .all()
                .values()
                .next()
                .expect("Should have at least one connection");

            let received_token = conn_info
                .received_token_as_string()
                .expect("Should have received a token as string");

            assert_eq!(
                received_token, access_token,
                "The token received by the server should match the token sent by the client"
            );
            println!("✓ Token verification passed: server received the correct access token");
        }

        // Cleanup
        let _ = shutdown_tx.send(());
        let _ = tokio::time::timeout(tokio::time::Duration::from_secs(2), server_handle).await;

        Ok(())
    }

    /// Test executing a query after authenticating with access token
    /// and verify the token was correctly received.
    #[tokio::test]
    async fn test_execute_query_with_access_token() -> Result<(), Box<dyn std::error::Error>> {
        init_tracing();

        // Start mock server with FedAuth support
        let server = MockTdsServer::new("127.0.0.1:0").await?;
        let server_addr = server.local_addr();

        // Get reference to connection store for verification
        let connection_store = server.connection_store();

        let (shutdown_tx, shutdown_rx) = oneshot::channel();
        let server_handle =
            tokio::spawn(async move { server.run_with_shutdown(shutdown_rx).await });

        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

        // The token we're sending
        let access_token = generate_mock_access_token();

        // Create client with access token
        let datasource = format!("tcp:{},{}", server_addr.ip(), server_addr.port());
        let mut context = ClientContext::default();
        context.access_token = Some(access_token.clone());
        context.tds_authentication_method = TdsAuthenticationMethod::AccessToken;
        context.database = "master".to_string();
        context.encryption_options = EncryptionOptions {
            mode: EncryptionSetting::PreferOff,
            trust_server_certificate: true,
            host_name_in_cert: None,
            server_certificate: None,
        };

        // Connect and execute query
        let provider = TdsConnectionProvider {};
        let mut client = provider.create_client(context, &datasource, None).await?;

        // Execute a simple SELECT 1 query
        client.execute("SELECT 1".to_string(), ()).await?;

        println!("Successfully executed query with access token authentication");

        // Close the client to trigger connection completion and storage
        drop(client);

        // Give the server time to store connection info
        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

        // Verify the token
        {
            let store = connection_store.lock().await;
            assert_eq!(
                store.count(),
                1,
                "Server should have stored exactly one connection"
            );

            let conn_info = store
                .all()
                .values()
                .next()
                .expect("Should have at least one connection");

            let received_token = conn_info
                .received_token_as_string()
                .expect("Should have received a token as string");

            assert_eq!(
                received_token, access_token,
                "The token received by the server should match the token sent by the client"
            );
            println!("✓ Token verification passed after query execution");
        }

        // Cleanup
        let _ = shutdown_tx.send(());
        let _ = tokio::time::timeout(tokio::time::Duration::from_secs(2), server_handle).await;

        Ok(())
    }

    /// Test that FedAuth mode properly advertises support in PreLogin
    /// and verify the complete token round-trip.
    #[tokio::test]
    async fn test_fedauth_prelogin_negotiation() -> Result<(), Box<dyn std::error::Error>> {
        init_tracing();

        // Start mock server with FedAuth support
        let server = MockTdsServer::new("127.0.0.1:0").await?;
        let server_addr = server.local_addr();

        // Get reference to connection store for verification
        let connection_store = server.connection_store();

        let (shutdown_tx, shutdown_rx) = oneshot::channel();
        let server_handle =
            tokio::spawn(async move { server.run_with_shutdown(shutdown_rx).await });

        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

        // The token we're sending
        let access_token = generate_mock_access_token();

        // Create client with access token - the PreLogin should successfully negotiate FedAuth
        let datasource = format!("tcp:{},{}", server_addr.ip(), server_addr.port());
        let mut context = ClientContext::default();
        context.access_token = Some(access_token.clone());
        context.tds_authentication_method = TdsAuthenticationMethod::AccessToken;
        context.database = "master".to_string();
        context.encryption_options = EncryptionOptions {
            mode: EncryptionSetting::PreferOff,
            trust_server_certificate: true,
            host_name_in_cert: None,
            server_certificate: None,
        };

        // This should succeed if FedAuth negotiation works
        let provider = TdsConnectionProvider {};
        let client = provider.create_client(context, &datasource, None).await?;

        println!(
            "Successfully negotiated FedAuth in PreLogin and connected to {}",
            server_addr
        );

        // Close the client to trigger connection completion and storage
        drop(client);

        // Give the server time to store connection info
        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

        // Verify complete round-trip: sent token == received token
        {
            let store = connection_store.lock().await;
            assert!(
                store.count() > 0,
                "Server should have stored at least one connection"
            );

            let conn_info = store
                .all()
                .values()
                .next()
                .expect("Should have at least one connection");

            let received_token = conn_info
                .received_token_as_string()
                .expect("Should have received a token as string");

            assert_eq!(
                received_token, access_token,
                "FedAuth token round-trip verification: sent != received"
            );
            println!("✓ FedAuth token round-trip verification passed");
        }

        // Cleanup
        let _ = shutdown_tx.send(());
        let _ = tokio::time::timeout(tokio::time::Duration::from_secs(2), server_handle).await;

        Ok(())
    }

    /// Test with a unique token to ensure we're not just accepting any token
    #[tokio::test]
    async fn test_unique_token_verification() -> Result<(), Box<dyn std::error::Error>> {
        init_tracing();

        let server = MockTdsServer::new("127.0.0.1:0").await?;
        let server_addr = server.local_addr();
        let connection_store = server.connection_store();

        let (shutdown_tx, shutdown_rx) = oneshot::channel();
        let server_handle =
            tokio::spawn(async move { server.run_with_shutdown(shutdown_rx).await });

        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

        // Use a unique token with timestamp to ensure uniqueness
        let unique_token = format!(
            "unique_test_token_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );

        let datasource = format!("tcp:{},{}", server_addr.ip(), server_addr.port());
        let mut context = ClientContext::default();
        context.access_token = Some(unique_token.clone());
        context.tds_authentication_method = TdsAuthenticationMethod::AccessToken;
        context.database = "master".to_string();
        context.encryption_options = EncryptionOptions {
            mode: EncryptionSetting::PreferOff,
            trust_server_certificate: true,
            host_name_in_cert: None,
            server_certificate: None,
        };

        let provider = TdsConnectionProvider {};
        let client = provider.create_client(context, &datasource, None).await?;

        // Close the client to trigger connection completion and storage
        drop(client);

        // Give the server time to store connection info
        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

        // Verify the unique token was received correctly
        {
            let store = connection_store.lock().await;
            let conn_info = store
                .all()
                .values()
                .next()
                .expect("Should have at least one connection");

            let received = conn_info
                .received_token_as_string()
                .expect("Should have received the unique token");

            assert_eq!(
                received, unique_token,
                "Unique token verification failed: expected '{}' but got '{}'",
                unique_token, received
            );
            println!(
                "✓ Unique token '{}' verified successfully",
                &unique_token[..50.min(unique_token.len())]
            );
        }

        let _ = shutdown_tx.send(());
        let _ = tokio::time::timeout(tokio::time::Duration::from_secs(2), server_handle).await;

        Ok(())
    }

    #[tokio::test]
    async fn test_connect_with_fedauth_challenge_and_token_factory()
    -> Result<(), Box<dyn std::error::Error>> {
        init_tracing();

        let server = MockTdsServer::new("127.0.0.1:0").await?;
        let server_addr = server.local_addr();
        let connection_store = server.connection_store();

        let (shutdown_tx, shutdown_rx) = oneshot::channel();
        let server_handle =
            tokio::spawn(async move { server.run_with_shutdown(shutdown_rx).await });

        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

        let token = "mock_service_principal_token".to_string();
        let factory_calls = Arc::new(Mutex::new(Vec::new()));

        let datasource = format!("tcp:{},{}", server_addr.ip(), server_addr.port());
        let mut context = ClientContext::default();
        context.tds_authentication_method =
            TdsAuthenticationMethod::ActiveDirectoryServicePrincipal;
        context.database = "master".to_string();
        context.encryption_options = EncryptionOptions {
            mode: EncryptionSetting::PreferOff,
            trust_server_certificate: true,
            host_name_in_cert: None,
            server_certificate: None,
        };
        context.auth_method_map.insert(
            TdsAuthenticationMethod::ActiveDirectoryServicePrincipal,
            Box::new(MockEntraFactory {
                token: token.clone(),
                calls: Arc::clone(&factory_calls),
            }),
        );

        let provider = TdsConnectionProvider {};
        let mut client = provider.create_client(context, &datasource, None).await?;
        client.execute("SELECT 1".to_string(), ()).await?;
        drop(client);

        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

        {
            let calls = factory_calls
                .lock()
                .expect("factory call capture mutex poisoned");
            assert_eq!(calls.len(), 1, "token factory should be called once");
            assert_eq!(
                calls[0],
                (
                    "https://database.windows.net/".to_string(),
                    "https://login.microsoftonline.com/test-tenant/".to_string(),
                    TdsAuthenticationMethod::ActiveDirectoryServicePrincipal
                )
            );
        }

        {
            let store = connection_store.lock().await;
            assert_eq!(
                store.count(),
                1,
                "Server should have stored exactly one connection"
            );

            let conn_info = store
                .all()
                .values()
                .next()
                .expect("Should have at least one connection");
            let received_token = conn_info
                .received_token_as_string()
                .expect("Should have received challenged token as string");

            assert_eq!(received_token, token);
        }

        let _ = shutdown_tx.send(());
        let _ = tokio::time::timeout(tokio::time::Duration::from_secs(2), server_handle).await;

        Ok(())
    }

    /// Regression guard for eager token recording: the token must be visible in the
    /// shared store WHILE the client connection is still open, i.e. without relying on
    /// connection teardown. This mirrors the downstream ODBC pooled-connection scenario
    /// where close() returns the socket to a pool without sending EOF, so a teardown-only
    /// record would never fire.
    #[tokio::test]
    async fn test_token_recorded_before_connection_close() -> Result<(), Box<dyn std::error::Error>>
    {
        init_tracing();

        let server = MockTdsServer::new("127.0.0.1:0").await?;
        let server_addr = server.local_addr();
        let connection_store = server.connection_store();

        let (shutdown_tx, shutdown_rx) = oneshot::channel();
        let server_handle =
            tokio::spawn(async move { server.run_with_shutdown(shutdown_rx).await });

        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

        let access_token = "eager_record_guard_token_67890".to_string();

        let datasource = format!("tcp:{},{}", server_addr.ip(), server_addr.port());
        let mut context = ClientContext::default();
        context.access_token = Some(access_token.clone());
        context.tds_authentication_method = TdsAuthenticationMethod::AccessToken;
        context.database = "master".to_string();
        context.encryption_options = EncryptionOptions {
            mode: EncryptionSetting::PreferOff,
            trust_server_certificate: true,
            host_name_in_cert: None,
            server_certificate: None,
        };

        let provider = TdsConnectionProvider {};
        let client = provider.create_client(context, &datasource, None).await?;

        // Assert the token is recorded WITHOUT closing/dropping the connection first.
        let recorded = {
            let store = connection_store.lock().await;
            store
                .all()
                .values()
                .any(|c| c.received_token_as_string().as_deref() == Some(&access_token))
        };
        assert!(
            recorded,
            "FedAuth token must be recorded before the client connection is closed (eager-record guard)"
        );

        // Keep the connection alive until after the assertion.
        drop(client);

        let _ = shutdown_tx.send(());
        let _ = tokio::time::timeout(tokio::time::Duration::from_secs(2), server_handle).await;

        Ok(())
    }
}
