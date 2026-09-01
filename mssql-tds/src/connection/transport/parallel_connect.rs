// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Parallel TCP connection module for MultiSubnetFailover support.
//!
//! This module implements parallel connection logic that allows connecting to multiple
//! IP addresses simultaneously, which is essential for fast failover in SQL Server
//! AlwaysOn Availability Groups with MultiSubnetFailover enabled.
//!
//! ## Overview
//!
//! When MultiSubnetFailover is enabled, the client attempts to connect to all resolved
//! IP addresses in parallel rather than sequentially. The first successful connection
//! is used, and all other pending connections are cancelled.
//!
//! ## Key Features
//!
//! - Parallel connection attempts to all resolved IP addresses
//! - Maximum 64 IP addresses supported (SQL Server limit)
//! - First successful connection wins
//! - Automatic cleanup of pending connections
//! - TCP-only (Named Pipes and Shared Memory not supported)
//!
//! ## Implementation Details
//!
//! The implementation uses Tokio's `select!` macro to race multiple connection futures
//! and returns as soon as one succeeds. Failed connections are logged but don't
//! prevent other connections from succeeding.

use std::net::SocketAddr;
use std::time::{Duration, Instant};

use tokio::net::{self, TcpStream};
use tokio::time::timeout;
use tracing::{debug, info, trace, warn};

use crate::core::TdsResult;
use crate::error::Error;

/// Maximum number of IP addresses to connect to in parallel.
/// This matches SQL Server ODBC driver behavior.
pub const MAX_PARALLEL_IPS: usize = 64;

/// Default timeout for parallel connection attempts in milliseconds.
/// Individual connections may have their own timeouts within this window.
pub const DEFAULT_PARALLEL_TIMEOUT_MS: u64 = 15000;

/// Configuration for parallel connection attempts.
#[derive(Clone, Debug)]
pub struct ParallelConnectConfig {
    /// Overall timeout for the parallel connection attempt.
    pub timeout_ms: u64,
    /// TCP keep-alive idle time in milliseconds.
    pub keep_alive_in_ms: u32,
    /// TCP keep-alive interval in milliseconds.
    pub keep_alive_interval_in_ms: u32,
}

impl Default for ParallelConnectConfig {
    fn default() -> Self {
        Self {
            timeout_ms: DEFAULT_PARALLEL_TIMEOUT_MS,
            keep_alive_in_ms: 30_000,
            keep_alive_interval_in_ms: 1_000,
        }
    }
}

/// Result of a parallel connection attempt.
#[derive(Debug)]
pub struct ParallelConnectResult {
    /// The successfully connected TCP stream.
    pub stream: TcpStream,
    /// The address that was successfully connected to.
    pub connected_address: SocketAddr,
    /// Number of addresses that were attempted.
    pub total_addresses: usize,
    /// Number of failed connection attempts before success.
    pub failed_attempts: usize,
}

/// Attempts to connect to multiple IP addresses in parallel.
///
/// This function resolves the host to multiple IP addresses and attempts
/// to connect to all of them simultaneously. The first successful connection
/// is returned, and all other pending connections are dropped.
///
/// # Arguments
///
/// * `host` - The hostname to resolve
/// * `port` - The port to connect to
/// * `config` - Configuration for the parallel connection attempt
///
/// # Returns
///
/// A `TdsResult` containing the `ParallelConnectResult` with the successful
/// connection, or an error if all connections failed.
///
/// # Errors
///
/// Returns an error if:
/// - DNS resolution fails
/// - No addresses were resolved
/// - All connection attempts failed
/// - The overall timeout was exceeded
///
/// # Example
///
/// ```ignore
/// use mssql_tds::connection::transport::parallel_connect::{
///     parallel_connect, ParallelConnectConfig
/// };
///
/// let config = ParallelConnectConfig::default();
/// let result = parallel_connect("myserver.example.com", 1433, &config).await?;
/// println!("Connected to: {}", result.connected_address);
/// ```
pub async fn parallel_connect(
    host: &str,
    port: u16,
    config: &ParallelConnectConfig,
) -> TdsResult<ParallelConnectResult> {
    info!(
        "Starting parallel connection to {}:{} with timeout {}ms",
        host, port, config.timeout_ms
    );

    // `timeout_ms` is documented as the overall budget for this call, so it
    // must cover DNS resolution too, not just the connection race below.
    let deadline = Instant::now() + Duration::from_millis(config.timeout_ms);

    // Resolve DNS to get all IP addresses. `lookup_host` awaits the resolution
    // instead of blocking the calling thread (unlike `std::net::ToSocketAddrs`),
    // so a slow or stuck resolver stays subject to `deadline` instead of
    // silently escaping it.
    let addresses: Vec<SocketAddr> = match timeout(
        deadline.saturating_duration_since(Instant::now()),
        tokio::net::lookup_host((host, port)),
    )
    .await
    {
        Ok(Ok(resolved)) => resolved.take(MAX_PARALLEL_IPS).collect(),
        Ok(Err(e)) => return Err(Error::from(e)),
        Err(_elapsed) => {
            warn!(
                "DNS resolution for {}:{} timed out after {}ms",
                host, port, config.timeout_ms
            );
            return Err(Error::TimeoutError(crate::error::TimeoutErrorType::String(
                format!("Connection timeout: DNS resolution for {host}:{port} timed out"),
            )));
        }
    };

    if addresses.is_empty() {
        return Err(Error::ConnectionError(format!(
            "DNS resolution returned no addresses for {}:{}",
            host, port
        )));
    }

    let total_addresses = addresses.len();
    info!(
        "Resolved {} addresses for {}:{}: {:?}",
        total_addresses, host, port, addresses
    );

    if total_addresses > MAX_PARALLEL_IPS {
        warn!(
            "Resolved {} addresses, but only {} will be used",
            total_addresses, MAX_PARALLEL_IPS
        );
    }

    // Create connection futures for all addresses
    let connect_futures: Vec<_> = addresses
        .iter()
        .enumerate()
        .map(|(idx, addr)| {
            let addr = *addr;
            let keep_alive_in_ms = config.keep_alive_in_ms;
            let keep_alive_interval_in_ms = config.keep_alive_interval_in_ms;
            async move {
                trace!("Attempting connection {} to {}", idx, addr);
                match connect_with_keepalive(addr, keep_alive_in_ms, keep_alive_interval_in_ms)
                    .await
                {
                    Ok(stream) => {
                        info!("Connection {} to {} succeeded", idx, addr);
                        Ok((stream, addr, idx))
                    }
                    Err(e) => {
                        debug!("Connection {} to {} failed: {}", idx, addr, e);
                        Err((e, addr, idx))
                    }
                }
            }
        })
        .collect();

    // Race all connections with whatever remains of the overall timeout —
    // DNS resolution above may have already spent part of it.
    let result = timeout(
        deadline.saturating_duration_since(Instant::now()),
        race_connections(connect_futures),
    )
    .await;

    match result {
        Ok(Ok((stream, addr, _idx, failed_attempts))) => {
            info!(
                "Parallel connection succeeded to {} after {} failed attempts",
                addr, failed_attempts
            );
            Ok(ParallelConnectResult {
                stream,
                connected_address: addr,
                total_addresses,
                failed_attempts,
            })
        }
        Ok(Err(last_error)) => {
            warn!(
                "All {} parallel connections failed. Last error: {}",
                total_addresses, last_error
            );
            Err(Error::ConnectionError(format!(
                "All parallel connection attempts failed to {}:{}. Last error: {}",
                host, port, last_error
            )))
        }
        Err(_) => {
            warn!("Parallel connection timeout after {}ms", config.timeout_ms);
            Err(Error::TimeoutError(crate::error::TimeoutErrorType::String(
                "Connection timeout: Connection attempt timed out".to_string(),
            )))
        }
    }
}

/// Attempts to connect to a list of explicit socket addresses in parallel.
///
/// This is the lower-level function that allows direct control over which addresses
/// to connect to. Useful for testing and scenarios where DNS resolution is already done.
///
/// # Arguments
///
/// * `addresses` - List of socket addresses to connect to in parallel
/// * `config` - Configuration for the parallel connection attempt
///
/// # Returns
///
/// A `TdsResult` containing the `ParallelConnectResult` with the successful
/// connection, or an error if all connections failed.
pub async fn parallel_connect_to_addresses(
    addresses: Vec<SocketAddr>,
    config: &ParallelConnectConfig,
) -> TdsResult<ParallelConnectResult> {
    if addresses.is_empty() {
        return Err(Error::ConnectionError(
            "No addresses provided for parallel connection".to_string(),
        ));
    }

    let total_addresses = addresses.len();
    info!(
        "Starting parallel connection to {} addresses: {:?}",
        total_addresses, addresses
    );

    if total_addresses > MAX_PARALLEL_IPS {
        warn!(
            "Provided {} addresses, but only {} will be used",
            total_addresses, MAX_PARALLEL_IPS
        );
    }

    // Take only up to MAX_PARALLEL_IPS addresses
    let addresses: Vec<SocketAddr> = addresses.into_iter().take(MAX_PARALLEL_IPS).collect();
    let total_addresses = addresses.len();

    // Create connection futures for all addresses
    let connect_futures: Vec<_> = addresses
        .iter()
        .enumerate()
        .map(|(idx, addr)| {
            let addr = *addr;
            let keep_alive_in_ms = config.keep_alive_in_ms;
            let keep_alive_interval_in_ms = config.keep_alive_interval_in_ms;
            async move {
                trace!("Attempting connection {} to {}", idx, addr);
                match connect_with_keepalive(addr, keep_alive_in_ms, keep_alive_interval_in_ms)
                    .await
                {
                    Ok(stream) => {
                        info!("Connection {} to {} succeeded", idx, addr);
                        Ok((stream, addr, idx))
                    }
                    Err(e) => {
                        debug!("Connection {} to {} failed: {}", idx, addr, e);
                        Err((e, addr, idx))
                    }
                }
            }
        })
        .collect();

    // Race all connections with an overall timeout
    let result = timeout(
        Duration::from_millis(config.timeout_ms),
        race_connections(connect_futures),
    )
    .await;

    match result {
        Ok(Ok((stream, addr, _idx, failed_attempts))) => {
            info!(
                "Parallel connection succeeded to {} after {} failed attempts",
                addr, failed_attempts
            );
            Ok(ParallelConnectResult {
                stream,
                connected_address: addr,
                total_addresses,
                failed_attempts,
            })
        }
        Ok(Err(last_error)) => {
            warn!(
                "All {} parallel connections failed. Last error: {}",
                total_addresses, last_error
            );
            Err(Error::ConnectionError(format!(
                "All parallel connection attempts failed. Last error: {}",
                last_error
            )))
        }
        Err(_) => {
            warn!("Parallel connection timeout after {}ms", config.timeout_ms);
            Err(Error::TimeoutError(crate::error::TimeoutErrorType::String(
                "Connection timeout: Connection attempt timed out".to_string(),
            )))
        }
    }
}

/// Connects to a single address with TCP keep-alive settings.
///
/// On Windows, this function ensures a minimum wait time (MIN_PARALLEL_WAIT_TIME_MS)
/// to increase the likelihood of getting useful error messages like WSAECONNREFUSED
/// rather than generic timeout errors.
async fn connect_with_keepalive(
    addr: SocketAddr,
    keep_alive_in_ms: u32,
    keep_alive_interval_in_ms: u32,
) -> Result<TcpStream, std::io::Error> {
    let socket = if addr.is_ipv6() {
        net::TcpSocket::new_v6()?
    } else {
        net::TcpSocket::new_v4()?
    };

    // Configure keep-alive settings
    let keep_alive_settings = socket2::TcpKeepalive::new()
        .with_time(Duration::from_millis(keep_alive_in_ms as u64))
        .with_interval(Duration::from_millis(keep_alive_interval_in_ms as u64));

    let socket2_socket = socket2::SockRef::from(&socket);
    socket2_socket.set_tcp_keepalive(&keep_alive_settings)?;
    socket2_socket.set_nodelay(true)?;

    socket.connect(addr).await
}

/// Races multiple connection futures and returns the first successful one.
///
/// This function spawns all connection attempts as tasks and waits for the first
/// successful connection. If all connections fail, returns the last error.
async fn race_connections(
    connect_futures: Vec<
        impl std::future::Future<
            Output = Result<(TcpStream, SocketAddr, usize), (std::io::Error, SocketAddr, usize)>,
        > + Send
        + 'static,
    >,
) -> Result<(TcpStream, SocketAddr, usize, usize), std::io::Error> {
    use tokio::sync::mpsc;

    let (tx, mut rx) = mpsc::channel::<Result<(TcpStream, SocketAddr, usize), std::io::Error>>(1);
    let total = connect_futures.len();

    // Spawn all connection attempts
    let handles: Vec<_> = connect_futures
        .into_iter()
        .map(|fut| {
            let tx = tx.clone();
            tokio::spawn(async move {
                let result = fut.await;
                match result {
                    Ok((stream, addr, idx)) => {
                        // Try to send success, ignore if receiver dropped
                        let _ = tx.send(Ok((stream, addr, idx))).await;
                    }
                    Err((e, _addr, _idx)) => {
                        // Try to send error, ignore if receiver dropped
                        let _ = tx.send(Err(e)).await;
                    }
                }
            })
        })
        .collect();

    // Drop our sender so channel closes when all spawned tasks complete
    drop(tx);

    let mut failed_attempts = 0;
    let mut last_error = std::io::Error::new(
        std::io::ErrorKind::NotConnected,
        "No connection attempts made",
    );

    // Wait for first success or all failures
    while let Some(result) = rx.recv().await {
        match result {
            Ok((stream, addr, idx)) => {
                // Success! Abort all other tasks
                for handle in handles {
                    handle.abort();
                }
                return Ok((stream, addr, idx, failed_attempts));
            }
            Err(e) => {
                failed_attempts += 1;
                last_error = e;
                if failed_attempts >= total {
                    // All connections failed
                    break;
                }
            }
        }
    }

    Err(last_error)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// Regression coverage for AB#47704: `parallel_connect` used to resolve
    /// DNS via the blocking `std::net::ToSocketAddrs`, which never yields to
    /// the executor. Timing alone can't prove the fix — resolving even a
    /// nonexistent host completes in milliseconds whether or not the call
    /// blocks, so a `timeout()`-based assertion would pass either way. This
    /// proves the structural property instead: a concurrently spawned task
    /// must get *some* chance to run while resolution is in flight.
    /// `tokio::net::lookup_host` bridges to `spawn_blocking` via a channel,
    /// so the awaiting task is guaranteed to yield at least once no matter
    /// how fast the lookup itself finishes; a synchronous `to_socket_addrs()`
    /// call never yields at all. Uses a host that fails to resolve so the
    /// call returns from DNS resolution alone, without reaching
    /// `race_connections` — which spawns tasks and awaits a channel
    /// regardless of DNS behavior, and would otherwise mask the signal this
    /// test is after. Confirmed by mutation testing: reverting
    /// `parallel_connect` to `to_socket_addrs()` makes this test fail (the
    /// heartbeat counter stays at 0).
    #[tokio::test]
    async fn parallel_connect_resolution_yields_to_the_executor() {
        let heartbeats = std::sync::Arc::new(AtomicUsize::new(0));
        let heartbeats_task = heartbeats.clone();
        // A short sleep (rather than `yield_now()`) still catches the same
        // scheduling gap — the first increment can't happen until the main
        // task yields either way — without busy-spinning a core for however
        // long this host takes to NXDOMAIN.
        let heartbeat = tokio::spawn(async move {
            loop {
                heartbeats_task.fetch_add(1, Ordering::SeqCst);
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        });

        let config = ParallelConnectConfig {
            timeout_ms: 5000,
            ..Default::default()
        };
        let result =
            parallel_connect("invalid.host.that.does.not.exist.local", 1433, &config).await;

        heartbeat.abort();
        assert!(
            result.is_err(),
            "resolution must fail for a nonexistent host"
        );
        assert!(
            heartbeats.load(Ordering::SeqCst) > 0,
            "the heartbeat task never ran while resolution was in flight — \
             resolution is blocking the executor instead of awaiting it"
        );
    }

    #[test]
    fn test_parallel_connect_config_default() {
        let config = ParallelConnectConfig::default();
        assert_eq!(config.timeout_ms, DEFAULT_PARALLEL_TIMEOUT_MS);
        assert_eq!(config.keep_alive_in_ms, 30_000);
        assert_eq!(config.keep_alive_interval_in_ms, 1_000);
    }

    #[test]
    fn test_max_parallel_ips() {
        assert_eq!(MAX_PARALLEL_IPS, 64);
    }

    #[tokio::test]
    async fn test_parallel_connect_invalid_host() {
        let config = ParallelConnectConfig {
            timeout_ms: 1000,
            ..Default::default()
        };

        let result =
            parallel_connect("invalid.host.that.does.not.exist.local", 1433, &config).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_parallel_connect_connection_refused() {
        // Try to connect to localhost on a port that's likely not listening
        let config = ParallelConnectConfig {
            timeout_ms: 1000,
            ..Default::default()
        };

        let result = parallel_connect("127.0.0.1", 59999, &config).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_connect_with_keepalive_v4() {
        // Just verify it creates a socket without crashing
        let addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), 59999);
        let result = connect_with_keepalive(addr, 30_000, 1_000).await;
        // Expected to fail since nothing is listening
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_connect_with_keepalive_v6() {
        // Test IPv6 socket creation path
        let addr = SocketAddr::new(IpAddr::V6(Ipv6Addr::LOCALHOST), 59999);
        let result = connect_with_keepalive(addr, 30_000, 1_000).await;
        // Expected to fail since nothing is listening
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_parallel_connect_to_addresses_empty() {
        let config = ParallelConnectConfig {
            timeout_ms: 1000,
            ..Default::default()
        };

        let result = parallel_connect_to_addresses(vec![], &config).await;
        assert!(result.is_err());
        match result {
            Err(Error::ConnectionError(msg)) => {
                assert!(msg.contains("No addresses provided"));
            }
            _ => panic!("Expected ConnectionError for empty addresses"),
        }
    }

    #[tokio::test]
    async fn test_parallel_connect_to_addresses_single_failure() {
        let config = ParallelConnectConfig {
            timeout_ms: 1000,
            ..Default::default()
        };

        let addresses = vec![SocketAddr::new(
            IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)),
            59999,
        )];

        let result = parallel_connect_to_addresses(addresses, &config).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_parallel_connect_to_addresses_multiple_failures() {
        let config = ParallelConnectConfig {
            timeout_ms: 5000, // Use longer timeout to ensure we get connection errors, not timeout
            ..Default::default()
        };

        // Try multiple addresses that should all fail with connection refused
        let addresses = vec![
            SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), 59997),
            SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), 59998),
            SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), 59999),
        ];

        let result = parallel_connect_to_addresses(addresses, &config).await;
        assert!(result.is_err());
        // The error could be either a ConnectionError or TimeoutError depending on timing
        match result {
            Err(Error::ConnectionError(_)) | Err(Error::TimeoutError(_)) => {
                // Both are acceptable outcomes
            }
            Err(e) => panic!("Unexpected error type: {:?}", e),
            Ok(_) => panic!("Expected error but got success"),
        }
    }

    #[tokio::test]
    async fn test_parallel_connect_timeout() {
        // Use a very short timeout to trigger timeout path
        let config = ParallelConnectConfig {
            timeout_ms: 1, // 1ms timeout - should trigger timeout
            ..Default::default()
        };

        // Try connecting to a non-routable address that will hang
        // 10.255.255.1 is commonly used for testing timeouts
        let result = parallel_connect("10.255.255.1", 1433, &config).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_parallel_connect_to_addresses_timeout() {
        // Use a very short timeout to trigger timeout path
        let config = ParallelConnectConfig {
            timeout_ms: 1, // 1ms timeout
            ..Default::default()
        };

        // Non-routable address that will hang
        let addresses = vec![SocketAddr::new(
            IpAddr::V4(Ipv4Addr::new(10, 255, 255, 1)),
            1433,
        )];

        let result = parallel_connect_to_addresses(addresses, &config).await;
        assert!(result.is_err());
    }

    #[test]
    fn test_parallel_connect_result_fields() {
        // Test that ParallelConnectResult can be constructed and has expected fields
        // We can't actually create a TcpStream in a unit test easily, so this just
        // tests the struct's Debug implementation
        let config = ParallelConnectConfig {
            timeout_ms: 5000,
            keep_alive_in_ms: 10_000,
            keep_alive_interval_in_ms: 2_000,
        };
        assert_eq!(config.timeout_ms, 5000);
        assert_eq!(config.keep_alive_in_ms, 10_000);
        assert_eq!(config.keep_alive_interval_in_ms, 2_000);

        // Test Debug implementation
        let debug_str = format!("{:?}", config);
        assert!(debug_str.contains("ParallelConnectConfig"));
        assert!(debug_str.contains("5000"));
    }
}
