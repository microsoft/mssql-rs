// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Integration tests for connection redirection.
//!
//! These tests verify that the Rust client properly handles routing tokens
//! from the server and sends the correct ServerName in Login7 after redirection.

#[cfg(test)]
mod redirection_tests {
    use mssql_tds::connection::client_context::ClientContext;
    use mssql_tds::connection_provider::tds_connection_provider::TdsConnectionProvider;
    use mssql_tds::core::{EncryptionOptions, EncryptionSetting};

    /// Create a client context for testing without encryption
    fn create_test_context() -> ClientContext {
        let mut context = ClientContext::default();
        context.user_name = "sa".to_string();
        context.password = "TestPassword123!".to_string();
        context.database = "master".to_string();
        context.encryption_options = EncryptionOptions {
            mode: EncryptionSetting::PreferOff,
            trust_server_certificate: true,
            host_name_in_cert: None,
            server_certificate: None,
        };
        context.connect_timeout = 30;
        context
    }

    /// Unit test to verify get_login_server_name() returns correct format
    #[test]
    fn test_transport_context_login_server_name() {
        use mssql_tds::connection::client_context::TransportContext;

        // Test TCP with simple hostname
        let tcp = TransportContext::Tcp {
            host: "localhost".to_string(),
            port: 1433,
            instance_name: None,
        };
        assert_eq!(tcp.get_login_server_name(), "localhost,1433");

        // Test TCP with instance name (from routing token)
        let tcp_with_instance = TransportContext::Tcp {
            host: "localhost".to_string(), // Network hostname
            port: 1434,
            instance_name: Some("testinstance".to_string()), // Just the instance name
        };
        assert_eq!(
            tcp_with_instance.get_login_server_name(),
            "localhost\\testinstance,1434"
        );

        // Test that get_server_name() still returns just the host
        assert_eq!(tcp.get_server_name(), "localhost");
        assert_eq!(tcp_with_instance.get_server_name(), "localhost");
    }

    /// Test that exceeding the maximum redirect count (10) results in an error.
    ///
    /// This test creates 12 mock servers in a chain, where each server redirects
    /// to the next one. The client should fail after 10 redirects with a
    /// "Received more redirection tokens than expected" error.
    #[tokio::test]
    async fn test_max_redirects_exceeded() {
        use mssql_mock_tds::MockTdsServer;
        use tokio::sync::oneshot;

        // Create 12 mock servers in a chain (need > 10 to exceed the limit)
        // Server 0 redirects to Server 1
        // Server 1 redirects to Server 2
        // ...
        // Server 10 redirects to Server 11
        // Server 11 would be the final destination (but we never get there)

        const NUM_SERVERS: usize = 12;
        let mut servers = Vec::with_capacity(NUM_SERVERS);
        let mut ports = Vec::with_capacity(NUM_SERVERS);

        // First, create all servers on port 0 (OS assigns ports) to get their addresses
        for _ in 0..NUM_SERVERS {
            let server = MockTdsServer::new("127.0.0.1:0").await.unwrap();
            let port = server.local_addr().port();
            ports.push(port);
            servers.push(server);
        }

        // Now create servers with redirection configs
        // We need to recreate them because redirection config is set at creation time
        drop(servers);
        servers = Vec::with_capacity(NUM_SERVERS);

        for i in 0..NUM_SERVERS {
            if i < NUM_SERVERS - 1 {
                // This server redirects to the next one
                let next_port = ports[i + 1];
                let server = MockTdsServer::new_with_redirection(
                    &format!("127.0.0.1:{}", ports[i]),
                    "127.0.0.1",
                    next_port,
                )
                .await
                .unwrap();
                servers.push(server);
            } else {
                // Last server doesn't redirect (but client should never reach it)
                let server = MockTdsServer::new(&format!("127.0.0.1:{}", ports[i]))
                    .await
                    .unwrap();
                servers.push(server);
            }
        }

        // Start all servers
        let mut shutdown_senders = Vec::with_capacity(NUM_SERVERS);
        let mut server_handles = Vec::with_capacity(NUM_SERVERS);

        for server in servers {
            let (shutdown_tx, shutdown_rx) = oneshot::channel();
            shutdown_senders.push(shutdown_tx);

            let handle = tokio::spawn(async move {
                let _ = server.run_with_shutdown(shutdown_rx).await;
            });
            server_handles.push(handle);
        }

        // Give servers time to start
        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

        // Try to connect - should fail with max redirects exceeded
        let context = create_test_context();
        let provider = TdsConnectionProvider {};
        let datasource = format!("127.0.0.1,{}", ports[0]);

        let result = provider.create_client(context, &datasource, None).await;

        // Verify we get the expected error
        match result {
            Err(e) => {
                let error_msg = e.to_string();
                assert!(
                    error_msg.contains("more redirection tokens than expected")
                        || error_msg.contains("redirect"),
                    "Expected redirection limit error, got: {}",
                    error_msg
                );
            }
            Ok(_) => {
                panic!("Expected connection to fail due to too many redirects, but it succeeded");
            }
        }

        // Shutdown all servers
        for tx in shutdown_senders {
            let _ = tx.send(());
        }

        // Wait for all server tasks to complete
        for handle in server_handles {
            let _ = handle.await;
        }
    }

    /// Test that a single redirect works correctly.
    ///
    /// This test creates 2 mock servers:
    /// - Gateway server that redirects to the destination
    /// - Destination server that accepts the connection
    #[tokio::test]
    async fn test_single_redirect_success() {
        use mssql_mock_tds::MockTdsServer;
        use tokio::sync::oneshot;

        // Create destination server first (no redirection)
        let destination_server = MockTdsServer::new("127.0.0.1:0").await.unwrap();
        let destination_port = destination_server.local_addr().port();

        // Create gateway server that redirects to destination
        let gateway_server =
            MockTdsServer::new_with_redirection("127.0.0.1:0", "127.0.0.1", destination_port)
                .await
                .unwrap();
        let gateway_port = gateway_server.local_addr().port();

        // Start both servers
        let (dest_shutdown_tx, dest_shutdown_rx) = oneshot::channel();
        let (gw_shutdown_tx, gw_shutdown_rx) = oneshot::channel();

        let dest_handle = tokio::spawn(async move {
            let _ = destination_server.run_with_shutdown(dest_shutdown_rx).await;
        });

        let gw_handle = tokio::spawn(async move {
            let _ = gateway_server.run_with_shutdown(gw_shutdown_rx).await;
        });

        // Give servers time to start
        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

        // Connect through gateway - should redirect to destination and succeed
        let context = create_test_context();
        let provider = TdsConnectionProvider {};
        let datasource = format!("127.0.0.1,{}", gateway_port);

        let result = provider.create_client(context, &datasource, None).await;

        // Connection should succeed
        assert!(
            result.is_ok(),
            "Expected connection to succeed after redirect, got: {:?}",
            result.err()
        );

        // Shutdown servers
        let _ = dest_shutdown_tx.send(());
        let _ = gw_shutdown_tx.send(());
        let _ = dest_handle.await;
        let _ = gw_handle.await;
    }

    /// Test redirection chain with exactly 10 redirects (at the limit).
    ///
    /// This should succeed because the limit is > 10, not >= 10.
    #[tokio::test]
    async fn test_exactly_ten_redirects_succeeds() {
        use mssql_mock_tds::MockTdsServer;
        use tokio::sync::oneshot;

        // Create 11 servers: 10 redirectors + 1 destination = 10 redirects total
        const NUM_SERVERS: usize = 11;
        let mut ports = Vec::with_capacity(NUM_SERVERS);

        // First pass: get all ports
        for _ in 0..NUM_SERVERS {
            let server = MockTdsServer::new("127.0.0.1:0").await.unwrap();
            ports.push(server.local_addr().port());
            drop(server);
        }

        // Second pass: create servers with redirection config
        let mut servers = Vec::with_capacity(NUM_SERVERS);
        for i in 0..NUM_SERVERS {
            if i < NUM_SERVERS - 1 {
                let next_port = ports[i + 1];
                let server = MockTdsServer::new_with_redirection(
                    &format!("127.0.0.1:{}", ports[i]),
                    "127.0.0.1",
                    next_port,
                )
                .await
                .unwrap();
                servers.push(server);
            } else {
                let server = MockTdsServer::new(&format!("127.0.0.1:{}", ports[i]))
                    .await
                    .unwrap();
                servers.push(server);
            }
        }

        // Start all servers
        let mut shutdown_senders = Vec::with_capacity(NUM_SERVERS);
        let mut server_handles = Vec::with_capacity(NUM_SERVERS);

        for server in servers {
            let (shutdown_tx, shutdown_rx) = oneshot::channel();
            shutdown_senders.push(shutdown_tx);

            let handle = tokio::spawn(async move {
                let _ = server.run_with_shutdown(shutdown_rx).await;
            });
            server_handles.push(handle);
        }

        // Give servers time to start
        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

        // Connect - should succeed with exactly 10 redirects
        let context = create_test_context();
        let provider = TdsConnectionProvider {};
        let datasource = format!("127.0.0.1,{}", ports[0]);

        let result = provider.create_client(context, &datasource, None).await;

        // Should succeed
        assert!(
            result.is_ok(),
            "Expected connection to succeed with 10 redirects, got: {:?}",
            result.err()
        );

        // Shutdown all servers
        for tx in shutdown_senders {
            let _ = tx.send(());
        }
        for handle in server_handles {
            let _ = handle.await;
        }
    }

    /// Test that routing tokens with host\instance format preserve the instance name
    /// in the redirected LOGIN7 server_name field.
    ///
    /// This reproduces the Fabric DW redirect scenario where the gateway sends a
    /// routing token with server="host\INSTANCE-dw" and port=1433. The client must
    /// send "host\INSTANCE-dw,1433" as the LOGIN7 server_name to the destination,
    /// not just "host,1433".
    #[tokio::test]
    async fn test_redirect_preserves_instance_name() {
        use mssql_mock_tds::MockTdsServer;
        use tokio::sync::oneshot;

        // Create destination server (no redirection)
        let destination_server = MockTdsServer::new("127.0.0.1:0").await.unwrap();
        let destination_port = destination_server.local_addr().port();
        let dest_store = destination_server.connection_store();

        // Gateway redirects with host\instance format (simulating Fabric DW)
        let redirect_host = "127.0.0.1\\INSTANCE-dw";
        let gateway_server =
            MockTdsServer::new_with_redirection("127.0.0.1:0", redirect_host, destination_port)
                .await
                .unwrap();
        let gateway_port = gateway_server.local_addr().port();

        // Start both servers
        let (dest_shutdown_tx, dest_shutdown_rx) = oneshot::channel();
        let (gw_shutdown_tx, gw_shutdown_rx) = oneshot::channel();

        let dest_handle = tokio::spawn(async move {
            let _ = destination_server.run_with_shutdown(dest_shutdown_rx).await;
        });
        let gw_handle = tokio::spawn(async move {
            let _ = gateway_server.run_with_shutdown(gw_shutdown_rx).await;
        });

        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

        // Connect through gateway
        let context = create_test_context();
        let provider = TdsConnectionProvider {};
        let datasource = format!("127.0.0.1,{}", gateway_port);

        let result = provider.create_client(context, &datasource, None).await;
        assert!(
            result.is_ok(),
            "Expected redirect with instance to succeed, got: {:?}",
            result.err()
        );

        // Drop the client so the destination server's connection handler
        // finishes and stores the ConnectionInfo.
        drop(result);
        tokio::time::sleep(tokio::time::Duration::from_millis(200)).await;

        // Verify the LOGIN7 server_name sent to the destination includes the instance
        let store = dest_store.lock().await;
        let connections: Vec<_> = store.all().values().collect();
        assert_eq!(
            connections.len(),
            1,
            "Expected exactly one connection to destination"
        );

        let expected_server_name = format!("127.0.0.1\\INSTANCE-dw,{}", destination_port);
        assert_eq!(
            connections[0].received_server_name.as_deref(),
            Some(expected_server_name.as_str()),
            "LOGIN7 server_name should include instance from routing token"
        );

        let _ = dest_shutdown_tx.send(());
        let _ = gw_shutdown_tx.send(());
        let _ = dest_handle.await;
        let _ = gw_handle.await;
    }

    type ServerHandle = (
        tokio::sync::oneshot::Sender<()>,
        tokio::task::JoinHandle<()>,
    );

    type Hop = Box<dyn FnOnce(u16) -> mssql_mock_tds::RedirectionConfig>;

    fn start(server: mssql_mock_tds::MockTdsServer) -> ServerHandle {
        let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel();
        let handle = tokio::spawn(async move {
            let _ = server.run_with_shutdown(shutdown_rx).await;
        });
        (shutdown_tx, handle)
    }

    /// Outcome of logging in through a chain of redirecting gateways.
    struct RoutedLogin {
        result: Result<(), String>,
        /// LOGIN7s received by each gateway, then by the destination.
        logins: Vec<Vec<mssql_mock_tds::ConnectionInfo>>,
    }

    /// Logs in to the first of `hops` gateways. Each gateway redirects to the
    /// next one (the last to a destination that accepts the login) with the
    /// configuration its builder returns for the target port.
    async fn login_through(hops: Vec<Hop>) -> RoutedLogin {
        login_through_with(hops, |port| format!("127.0.0.1,{port}")).await
    }

    /// [`login_through`], connecting to the data source that `datasource`
    /// returns for the first gateway's port.
    async fn login_through_with(
        hops: Vec<Hop>,
        datasource: impl FnOnce(u16) -> String,
    ) -> RoutedLogin {
        use mssql_mock_tds::MockTdsServer;

        let destination = MockTdsServer::new("127.0.0.1:0").await.unwrap();
        let mut target_port = destination.local_addr().port();
        let mut stores = vec![destination.connection_store()];
        let mut servers = vec![start(destination)];
        for hop in hops.into_iter().rev() {
            let gateway =
                MockTdsServer::new_with_redirection_config("127.0.0.1:0", hop(target_port))
                    .await
                    .unwrap();
            target_port = gateway.local_addr().port();
            stores.insert(0, gateway.connection_store());
            servers.push(start(gateway));
        }

        let result = TdsConnectionProvider {}
            .create_client(create_test_context(), &datasource(target_port), None)
            .await
            .map(drop)
            .map_err(|e| e.to_string());
        // Let the servers record connections the client has just closed.
        tokio::time::sleep(tokio::time::Duration::from_millis(200)).await;

        let mut logins = Vec::with_capacity(stores.len());
        for store in &stores {
            logins.push(store.lock().await.all().values().cloned().collect());
        }
        for (shutdown_tx, handle) in servers {
            let _ = shutdown_tx.send(());
            let _ = handle.await;
        }
        RoutedLogin { result, logins }
    }

    fn enhanced(database: &'static str) -> Hop {
        Box::new(move |port| {
            mssql_mock_tds::RedirectionConfig::enhanced("127.0.0.1", port, database)
        })
    }

    fn plain() -> Hop {
        Box::new(|port| mssql_mock_tds::RedirectionConfig::new("127.0.0.1", port))
    }

    fn only_database(logins: &[mssql_mock_tds::ConnectionInfo]) -> Option<&str> {
        assert_eq!(
            logins.len(),
            1,
            "expected exactly one LOGIN7, got {logins:?}"
        );
        logins[0].received_database.as_deref()
    }

    /// Enhanced Routing (ENVCHANGE 21) redirects the login to the routed
    /// database; the client advertises feature 0x0F so the route is offered.
    #[tokio::test]
    async fn test_enhanced_routing_logs_in_to_the_routed_database() {
        let routed = login_through(vec![enhanced("routed_db")]).await;
        assert_eq!(routed.result, Ok(()));

        let gateway = &routed.logins[0];
        assert_eq!(only_database(gateway), Some("master"));
        assert!(
            gateway[0]
                .requested_feature_ids
                .contains(&mssql_mock_tds::protocol::FEATURE_EXT_ENHANCED_ROUTING),
            "LOGIN7 must advertise Enhanced Routing, got {:?}",
            gateway[0].requested_feature_ids
        );
        assert_eq!(only_database(&routed.logins[1]), Some("routed_db"));
    }

    /// Servers may acknowledge the feature before or after the routing token.
    #[tokio::test]
    async fn test_enhanced_routing_acknowledged_before_the_routing_token() {
        let routed = login_through(vec![Box::new(|port| {
            let mut config =
                mssql_mock_tds::RedirectionConfig::enhanced("127.0.0.1", port, "routed_db");
            config.ack_before_routing = true;
            config
        })])
        .await;
        assert_eq!(routed.result, Ok(()));
        assert_eq!(only_database(&routed.logins[1]), Some("routed_db"));
    }

    /// Enhanced Routing without an acknowledgement of 1 fails the login, as in
    /// msodbcsql, instead of following or ignoring the route.
    #[tokio::test]
    async fn test_enhanced_routing_without_acknowledgement_fails_the_login() {
        for acknowledgement in [None, Some(0)] {
            let routed = login_through(vec![Box::new(move |port| {
                let mut config =
                    mssql_mock_tds::RedirectionConfig::enhanced("127.0.0.1", port, "routed_db");
                config.enhanced_routing_ack = acknowledgement;
                config
            })])
            .await;
            assert!(
                routed.result.is_err(),
                "acknowledgement {acknowledgement:?} should fail the login"
            );
            assert!(
                routed.logins[1].is_empty(),
                "the client must not follow the route"
            );
        }
    }

    /// Plain routing (ENVCHANGE 20) keeps the database the application asked for.
    #[tokio::test]
    async fn test_plain_routing_keeps_the_login_database() {
        let routed = login_through(vec![plain()]).await;
        assert_eq!(routed.result, Ok(()));
        assert_eq!(only_database(&routed.logins[1]), Some("master"));
    }

    /// The routed database applies to its own hop only: a later plain route
    /// logs in to the original database again, as msodbcsql does.
    #[tokio::test]
    async fn test_plain_routing_after_enhanced_routing_restores_the_login_database() {
        let routed = login_through(vec![enhanced("routed_db"), plain()]).await;
        assert_eq!(routed.result, Ok(()));
        assert_eq!(only_database(&routed.logins[1]), Some("routed_db"));
        assert_eq!(only_database(&routed.logins[2]), Some("master"));
    }

    /// [MS-TDS] 2.2.7.9: a route follows LOGINACK, and a login response carries
    /// at most one. Like msodbcsql, the client fails the login on either
    /// violation instead of following whichever route came last.
    #[tokio::test]
    async fn test_malformed_routing_fails_the_login_without_following_a_route() {
        use mssql_mock_tds::RedirectionConfig;

        let cases: Vec<(&str, &str, Hop)> = vec![
            (
                "Enhanced Routing without LOGINACK",
                "before LOGINACK",
                Box::new(|port| RedirectionConfig {
                    login_ack: false,
                    ack_before_routing: true,
                    ..RedirectionConfig::enhanced("127.0.0.1", port, "routed_db")
                }),
            ),
            (
                "plain routing without LOGINACK",
                "before LOGINACK",
                Box::new(|port| RedirectionConfig {
                    login_ack: false,
                    ..RedirectionConfig::new("127.0.0.1", port)
                }),
            ),
            (
                "two Enhanced Routing ENVCHANGEs",
                "more than one routing ENVCHANGE",
                Box::new(|port| RedirectionConfig {
                    extra_routes: vec![Some("other_db".to_string())],
                    ..RedirectionConfig::enhanced("127.0.0.1", port, "routed_db")
                }),
            ),
            (
                "Enhanced Routing, then plain routing",
                "more than one routing ENVCHANGE",
                Box::new(|port| RedirectionConfig {
                    extra_routes: vec![None],
                    ..RedirectionConfig::enhanced("127.0.0.1", port, "routed_db")
                }),
            ),
            (
                "plain routing, then Enhanced Routing",
                "more than one routing ENVCHANGE",
                Box::new(|port| RedirectionConfig {
                    enhanced_routing_ack: Some(1),
                    extra_routes: vec![Some("routed_db".to_string())],
                    ..RedirectionConfig::new("127.0.0.1", port)
                }),
            ),
        ];
        for (case, violation, hop) in cases {
            let routed = login_through(vec![hop]).await;
            let error = routed.result.expect_err(case);
            assert!(
                error.starts_with("Protocol Error") && error.contains(violation),
                "{case}: expected a protocol error naming the violation, got {error}"
            );
            assert!(
                routed.logins[1].is_empty(),
                "{case}: the client must not follow a route"
            );
        }
    }

    /// A route received over shared memory is followed over TCP. The gateway
    /// is served on the `\\.\pipe\SQLLocal\<instance>` pipe that a local named
    /// instance's shared-memory shortcut opens before SSRP.
    #[cfg(windows)]
    #[tokio::test]
    async fn test_shared_memory_login_follows_the_route_over_tcp() {
        let instance = format!("ROUTE{}", std::process::id());
        let routed = login_through_with(vec![enhanced("routed_db")], |port| {
            relay_shared_memory(&instance, port);
            format!(r".\{instance}")
        })
        .await;
        assert_eq!(routed.result, Ok(()));
        assert_eq!(only_database(&routed.logins[0]), Some("master"));
        assert_eq!(only_database(&routed.logins[1]), Some("routed_db"));
    }

    /// Relays the shared-memory pipe of local instance `instance` to the TDS
    /// server on `port`.
    #[cfg(windows)]
    fn relay_shared_memory(instance: &str, port: u16) {
        use tokio::net::windows::named_pipe::{PipeMode, ServerOptions};

        let mut pipe = ServerOptions::new()
            .first_pipe_instance(true)
            .pipe_mode(PipeMode::Message)
            .create(format!(r"\\.\pipe\SQLLocal\{instance}"))
            .expect("create the shared-memory pipe");
        tokio::spawn(async move {
            pipe.connect()
                .await
                .expect("accept the shared-memory client");
            let mut server = tokio::net::TcpStream::connect(("127.0.0.1", port))
                .await
                .expect("connect to the gateway");
            let _ = tokio::io::copy_bidirectional(&mut pipe, &mut server).await;
        });
    }
}
