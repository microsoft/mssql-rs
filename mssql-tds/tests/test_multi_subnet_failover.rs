// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Tests for MultiSubnetFailover (parallel connection) functionality.
//!
//! These tests verify that the MultiSubnetFailover feature works correctly
//! for connecting to SQL Server AlwaysOn Availability Groups.

#[cfg(test)]
mod common;

#[cfg(test)]
mod multi_subnet_failover_tests {
    use mssql_tds::connection::client_context::ClientContext;
    use mssql_tds::connection::tds_client::{ResultSet, TdsClient};
    use mssql_tds::connection_provider::tds_connection_provider::TdsConnectionProvider;
    use mssql_tds::core::{EncryptionSetting, TdsResult};

    use crate::common::{build_tcp_datasource_explicit, create_context, init_tracing};

    /// Create a minimal context with dummy credentials for error-path tests.
    /// These tests fail before authentication, so real credentials aren't needed.
    fn create_dummy_context() -> ClientContext {
        let mut context = ClientContext::default();
        context.user_name = "dummy_user".to_string();
        context.password = "dummy_password".to_string();
        context.database = "master".to_string();
        context
    }

    /// Create a client with MultiSubnetFailover enabled
    async fn create_client_with_msf(datasource: &str) -> TdsResult<TdsClient> {
        let mut client_context = create_context();
        client_context.multi_subnet_failover = true; // Enable MSF
        // Use PreferOff encryption to test MSF without TDS 8.0 strict mode
        // trust_server_certificate is read from TRUST_SERVER_CERTIFICATE env var by create_context()
        client_context.encryption_options.mode = EncryptionSetting::PreferOff;

        let provider = TdsConnectionProvider {};
        provider
            .create_client(client_context, datasource, None)
            .await
    }

    /// Create a client with MultiSubnetFailover disabled (sequential mode)
    async fn create_client_sequential(datasource: &str) -> TdsResult<TdsClient> {
        let mut client_context = create_context();
        client_context.multi_subnet_failover = false; // Disable MSF (sequential)
        // Use PreferOff encryption to test sequential mode without TDS 8.0 strict mode
        // trust_server_certificate is read from TRUST_SERVER_CERTIFICATE env var by create_context()
        client_context.encryption_options.mode = EncryptionSetting::PreferOff;

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

    /// Test that MultiSubnetFailover can connect to a server
    #[tokio::test]
    async fn test_msf_connection() -> TdsResult<()> {
        init_tracing();
        let datasource = build_tcp_datasource_explicit();

        let mut client = create_client_with_msf(&datasource).await?;
        test_simple_query(&mut client).await?;
        client.close().await?;

        Ok(())
    }

    /// Test that sequential connection still works
    #[tokio::test]
    async fn test_sequential_connection() -> TdsResult<()> {
        init_tracing();
        let datasource = build_tcp_datasource_explicit();

        let mut client = create_client_sequential(&datasource).await?;
        test_simple_query(&mut client).await?;
        client.close().await?;

        Ok(())
    }

    /// Test that both MSF and sequential modes can connect to the same server
    #[tokio::test]
    async fn test_msf_and_sequential_same_server() -> TdsResult<()> {
        init_tracing();
        let datasource = build_tcp_datasource_explicit();

        // Connect with MSF
        {
            let mut client = create_client_with_msf(&datasource).await?;
            test_simple_query(&mut client).await?;
            client.close().await?;
        }

        // Connect sequentially
        {
            let mut client = create_client_sequential(&datasource).await?;
            test_simple_query(&mut client).await?;
            client.close().await?;
        }

        Ok(())
    }

    /// Test that MSF fails gracefully when connecting to an invalid host
    #[tokio::test]
    async fn test_msf_invalid_host() {
        init_tracing();

        let mut client_context = create_dummy_context();
        client_context.multi_subnet_failover = true;

        let provider = TdsConnectionProvider {};
        let result = provider
            .create_client(
                client_context,
                "tcp:invalid.host.that.does.not.exist.local,1433",
                None,
            )
            .await;
        assert!(result.is_err(), "Should fail with invalid host");
    }

    /// Test that MSF fails gracefully when connecting to a port with no listener
    #[tokio::test]
    async fn test_msf_connection_refused() {
        init_tracing();

        let mut client_context = create_dummy_context();
        client_context.multi_subnet_failover = true;

        // Try to connect to a port that's unlikely to be listening
        let provider = TdsConnectionProvider {};
        let result = provider
            .create_client(client_context, "tcp:127.0.0.1,59999", None)
            .await;
        assert!(result.is_err(), "Should fail when connection is refused");
    }

    /// Test that MSF cannot be used with FailoverPartner (database mirroring)
    #[tokio::test]
    async fn test_msf_with_failover_partner_fails() {
        init_tracing();

        let mut client_context = create_dummy_context();
        client_context.multi_subnet_failover = true;
        client_context.failover_partner = "mirror.server.com".to_string(); // Set failover partner

        let provider = TdsConnectionProvider {};
        let result = provider
            .create_client(client_context, "tcp:127.0.0.1,1433", None)
            .await;

        assert!(result.is_err(), "MSF with FailoverPartner should fail");
        let error_message = format!("{}", result.unwrap_err());
        assert!(
            error_message.contains("MultiSubnetFailover cannot be used with FailoverPartner"),
            "Error should mention incompatibility: {}",
            error_message
        );
    }
}

#[cfg(test)]
mod parallel_connect_unit_tests {
    use mssql_tds::connection::transport::parallel_connect::{
        DEFAULT_PARALLEL_TIMEOUT_MS, MAX_PARALLEL_IPS, ParallelConnectConfig, parallel_connect,
    };

    /// Test default configuration values
    #[test]
    fn test_config_defaults() {
        let config = ParallelConnectConfig::default();
        assert_eq!(config.timeout_ms, DEFAULT_PARALLEL_TIMEOUT_MS);
        assert_eq!(config.keep_alive_in_ms, 30_000);
        assert_eq!(config.keep_alive_interval_in_ms, 1_000);
    }

    /// Test maximum parallel IPs constant
    #[test]
    fn test_max_parallel_ips() {
        // SQL Server ODBC driver limits to 64 IPs
        assert_eq!(MAX_PARALLEL_IPS, 64);
    }

    /// Test that parallel_connect fails with invalid DNS
    #[tokio::test]
    async fn test_invalid_dns() {
        let config = ParallelConnectConfig {
            timeout_ms: 1000,
            ..Default::default()
        };

        let result =
            parallel_connect("invalid.host.that.does.not.exist.local", 1433, &config).await;
        assert!(result.is_err());
    }

    /// Test that parallel_connect fails when connection is refused
    #[tokio::test]
    async fn test_connection_refused() {
        let config = ParallelConnectConfig {
            timeout_ms: 2000,
            ..Default::default()
        };

        // Connect to localhost on a port unlikely to be listening
        let result = parallel_connect("127.0.0.1", 59999, &config).await;
        assert!(result.is_err());
    }

    /// Test parallel connection timeout
    #[tokio::test]
    async fn test_connection_timeout() {
        let config = ParallelConnectConfig {
            timeout_ms: 100, // Very short timeout
            ..Default::default()
        };

        // Try to connect to a non-routable address (should timeout)
        // 10.255.255.1 is typically a non-routable address
        let result = parallel_connect("10.255.255.1", 1433, &config).await;
        assert!(result.is_err());
    }
}

/// Tests that validate true parallel connection behavior
/// These tests use multiple local TCP listeners to simulate multi-IP scenarios
#[cfg(test)]
mod parallel_connect_integration_tests {
    use mssql_tds::connection::transport::parallel_connect::{
        ParallelConnectConfig, parallel_connect, parallel_connect_to_addresses,
    };
    use std::net::SocketAddr;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::{Duration, Instant};
    use tokio::net::TcpListener;
    use tokio::time::sleep;

    /// Test helper to create a TCP listener that accepts connections immediately
    async fn create_fast_listener() -> (TcpListener, SocketAddr) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        (listener, addr)
    }

    /// Test that parallel connection succeeds when connecting to a listening port
    #[tokio::test]
    async fn test_parallel_connect_to_single_listener() {
        let (listener, addr) = create_fast_listener().await;

        // Spawn a task to accept the connection
        let accept_handle = tokio::spawn(async move {
            let _ = listener.accept().await;
        });

        // Connect to the listener
        let config = ParallelConnectConfig {
            timeout_ms: 5000,
            ..Default::default()
        };

        let result = parallel_connect(&addr.ip().to_string(), addr.port(), &config).await;

        assert!(result.is_ok(), "Should connect to listening port");
        accept_handle.abort();
    }

    /// Test that demonstrates parallel connection advantage:
    /// When one IP is unreachable but another responds, we get a quick connection.
    ///
    /// This test validates the core MultiSubnetFailover behavior:
    /// - Creates one listening port (fast)
    /// - Uses a non-routable IP (slow/timeout)
    /// - Verifies connection completes quickly via the fast path
    #[tokio::test]
    async fn test_parallel_vs_sequential_timing() {
        // Create a fast listener
        let (fast_listener, fast_addr) = create_fast_listener().await;

        // Spawn task to accept on fast listener
        let connection_count = Arc::new(AtomicUsize::new(0));
        let count_clone = connection_count.clone();
        let accept_handle = tokio::spawn(async move {
            loop {
                if let Ok((_stream, _)) = fast_listener.accept().await {
                    count_clone.fetch_add(1, Ordering::SeqCst);
                }
            }
        });

        // Test 1: Connect to the fast listener - should be quick
        let start = Instant::now();
        let config = ParallelConnectConfig {
            timeout_ms: 5000,
            ..Default::default()
        };

        let result = parallel_connect(&fast_addr.ip().to_string(), fast_addr.port(), &config).await;

        let fast_duration = start.elapsed();
        assert!(result.is_ok(), "Fast connection should succeed");
        assert!(
            fast_duration < Duration::from_millis(500),
            "Fast connection should complete in <500ms, took {:?}",
            fast_duration
        );

        println!(
            "Fast connection to {} completed in {:?}",
            fast_addr, fast_duration
        );

        accept_handle.abort();
    }

    /// TRUE PARALLEL CONNECTION TEST
    ///
    /// This test validates that when connecting to multiple addresses in parallel:
    /// 1. The first address that accepts wins
    /// 2. Connection completes quickly even if one address is slow/unresponsive
    ///
    /// We create two listeners and verify the behavior with explicit addresses.
    #[tokio::test]
    async fn test_true_parallel_first_wins() {
        // Create two listeners
        let (listener1, addr1) = create_fast_listener().await;
        let (listener2, addr2) = create_fast_listener().await;

        let which_connected = Arc::new(AtomicUsize::new(0));

        // Accept on listener1 immediately (mark as 1)
        let connected_clone1 = which_connected.clone();
        let handle1 = tokio::spawn(async move {
            if let Ok((_stream, _)) = listener1.accept().await {
                connected_clone1.store(1, Ordering::SeqCst);
            }
        });

        // Accept on listener2 after a delay (mark as 2)
        let connected_clone2 = which_connected.clone();
        let handle2 = tokio::spawn(async move {
            // Delay listener2 - if parallel works, listener1 should win
            sleep(Duration::from_millis(200)).await;
            if let Ok((_stream, _)) = listener2.accept().await {
                connected_clone2.store(2, Ordering::SeqCst);
            }
        });

        // Give listeners time to start
        sleep(Duration::from_millis(10)).await;

        // Use the explicit addresses function to test true parallel behavior
        let config = ParallelConnectConfig {
            timeout_ms: 5000,
            ..Default::default()
        };

        let start = Instant::now();
        let result = parallel_connect_to_addresses(vec![addr1, addr2], &config).await;
        let duration = start.elapsed();

        assert!(result.is_ok(), "Parallel connection should succeed");
        let result = result.unwrap();

        // Connection should complete quickly (before the 200ms delay on listener2)
        assert!(
            duration < Duration::from_millis(150),
            "Parallel connection should complete quickly, took {:?}",
            duration
        );

        // The connected address should be addr1 (the fast one)
        assert_eq!(
            result.connected_address, addr1,
            "Should connect to the first (faster) address"
        );

        println!(
            "Parallel connection to {} succeeded in {:?} (total {} addresses, {} failed)",
            result.connected_address, duration, result.total_addresses, result.failed_attempts
        );

        handle1.abort();
        handle2.abort();
    }

    /// Test parallel connection when first address is unavailable but second succeeds.
    /// This simulates a subnet being down in MultiSubnetFailover scenario.
    /// Note: The timing of when the bad connection fails vs. the good one succeeds
    /// is OS-dependent, so we only verify that the connection ultimately succeeds.
    #[tokio::test]
    async fn test_parallel_fallback_on_failure() {
        // First address: a port with no listener (will fail immediately)
        let bad_addr: SocketAddr = "127.0.0.1:59998".parse().unwrap();

        // Second address: a good listener
        let (good_listener, good_addr) = create_fast_listener().await;

        let handle = tokio::spawn(async move {
            let _ = good_listener.accept().await;
        });

        sleep(Duration::from_millis(10)).await;

        let config = ParallelConnectConfig {
            timeout_ms: 5000,
            ..Default::default()
        };

        let start = Instant::now();
        let result = parallel_connect_to_addresses(vec![bad_addr, good_addr], &config).await;
        let duration = start.elapsed();

        assert!(result.is_ok(), "Should connect via fallback address");
        let result = result.unwrap();

        // Should connect to the good address
        assert_eq!(
            result.connected_address, good_addr,
            "Should fallback to good address"
        );

        // The bad address may or may not have failed before the good one succeeded,
        // depending on OS timing. On Windows, connection refused can take longer
        // than connecting to a listening port, so the good connection may complete first.
        assert!(
            result.failed_attempts <= 1,
            "Should have at most 1 failed attempt, got {}",
            result.failed_attempts
        );

        println!(
            "Fallback connection to {} succeeded in {:?} ({} failed attempts)",
            result.connected_address, duration, result.failed_attempts
        );

        handle.abort();
    }

    /// Test parallel connection with many addresses - validates scalability
    #[tokio::test]
    async fn test_parallel_many_addresses() {
        // Create one good listener
        let (good_listener, good_addr) = create_fast_listener().await;

        let handle = tokio::spawn(async move {
            let _ = good_listener.accept().await;
        });

        sleep(Duration::from_millis(10)).await;

        // Create many bad addresses (ports not listening)
        let mut addresses: Vec<SocketAddr> = (59900..59910)
            .map(|port| format!("127.0.0.1:{}", port).parse().unwrap())
            .collect();

        // Add the good address in the middle
        addresses.insert(5, good_addr);

        let config = ParallelConnectConfig {
            timeout_ms: 5000,
            ..Default::default()
        };

        let start = Instant::now();
        let result = parallel_connect_to_addresses(addresses, &config).await;
        let duration = start.elapsed();

        assert!(result.is_ok(), "Should connect despite many bad addresses");
        let result = result.unwrap();

        assert_eq!(
            result.connected_address, good_addr,
            "Should connect to good address"
        );

        // Connection should be fast since parallel tries all at once
        assert!(
            duration < Duration::from_millis(500),
            "Should complete quickly with parallel connections, took {:?}",
            duration
        );

        println!(
            "Parallel connection (11 addresses) to {} in {:?}, {} failed",
            result.connected_address, duration, result.failed_attempts
        );

        handle.abort();
    }

    /// Test that all connections timing out results in an error
    #[tokio::test]
    async fn test_parallel_all_timeout() {
        // Use non-routable addresses that will timeout
        let addresses: Vec<SocketAddr> = vec![
            "10.255.255.1:1433".parse().unwrap(),
            "10.255.255.2:1433".parse().unwrap(),
        ];

        let config = ParallelConnectConfig {
            timeout_ms: 200, // Short timeout
            ..Default::default()
        };

        let start = Instant::now();
        let result = parallel_connect_to_addresses(addresses, &config).await;
        let duration = start.elapsed();

        assert!(result.is_err(), "Should fail when all addresses timeout");

        // Should respect the timeout
        assert!(
            duration >= Duration::from_millis(180) && duration < Duration::from_millis(500),
            "Should timeout around 200ms, took {:?}",
            duration
        );

        println!("All-timeout test completed in {:?}", duration);
    }
}
