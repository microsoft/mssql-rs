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
//! The implementation uses a Tokio `JoinSet` to race multiple connection futures
//! and returns as soon as one succeeds. Failed connections are logged but don't
//! prevent other connections from succeeding.

use std::future::Future;
use std::net::SocketAddr;
use std::time::{Duration, Instant};

use tokio::net::{self, TcpStream};
use tokio::time::timeout;
use tracing::{debug, info, trace, warn};

use crate::connection::dns_cache::{self, Resolution};
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

/// Why connecting to one address failed.
#[derive(Debug, thiserror::Error)]
pub(crate) enum AttemptError {
    /// The address could not be reached, so another address may still connect.
    #[error(transparent)]
    Unreachable(#[from] std::io::Error),
    /// The server was reached but the connection still failed, such as on a
    /// certificate it could not prove, which no other address would fix.
    #[error(transparent)]
    #[cfg_attr(
        not(all(target_os = "macos", feature = "tls-network-framework")),
        allow(dead_code)
    )]
    Rejected(Error),
}

impl From<AttemptError> for Error {
    fn from(error: AttemptError) -> Self {
        match error {
            AttemptError::Unreachable(error) => Error::from(error),
            AttemptError::Rejected(error) => error,
        }
    }
}

/// Result of a parallel connection attempt.
#[derive(Debug)]
pub struct ParallelConnectResult<S = TcpStream> {
    /// The successfully connected stream.
    pub stream: S,
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
/// - More than 64 addresses were resolved
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
    let (keep_alive_in_ms, keep_alive_interval_in_ms) =
        (config.keep_alive_in_ms, config.keep_alive_interval_in_ms);
    parallel_connect_resolving_with(
        host,
        port,
        config.timeout_ms,
        |_| {},
        || {},
        |_| {},
        move |addr| connect_with_keepalive(addr, keep_alive_in_ms, keep_alive_interval_in_ms),
    )
    .await
    .map(|(result, _)| result)
}

/// [`parallel_connect`] with the per-address connect supplied by the caller,
/// also returning what `host` resolved to, which the connection's login DNS
/// caching verdict applies to. It hands `dns_fell_back` the resolver's error
/// when the DNS cache supplied the addresses instead, and calls `resolved`
/// once the host has resolved, before the race, so a caller can record where
/// the addresses came from even when no address then accepts. An address that
/// rejects the connection ends the race, and is handed to `rejected`.
pub(crate) async fn parallel_connect_resolving_with<S, C, F, E>(
    host: &str,
    port: u16,
    timeout_ms: u64,
    dns_fell_back: impl FnOnce(&std::io::Error),
    resolved: impl FnOnce(),
    rejected: impl FnOnce(SocketAddr),
    connect: C,
) -> TdsResult<(ParallelConnectResult<S>, Resolution)>
where
    S: Send + 'static,
    C: Fn(SocketAddr) -> F,
    F: Future<Output = Result<S, E>> + Send + 'static,
    E: Into<AttemptError>,
{
    info!(
        "Starting parallel connection to {}:{} with timeout {}ms",
        host, port, timeout_ms
    );

    // `timeout_ms` is documented as the overall budget for this call, so it
    // must cover DNS resolution too, not just the connection race below.
    let deadline = Instant::now() + Duration::from_millis(timeout_ms);

    // Resolve DNS to get all IP addresses. `lookup_host` awaits the resolution
    // instead of blocking the calling thread (unlike `std::net::ToSocketAddrs`),
    // so a slow or stuck resolver stays subject to `deadline` instead of
    // silently escaping it. A failed resolution falls back to the DNS cache.
    let resolution = match timeout(
        deadline.saturating_duration_since(Instant::now()),
        dns_cache::lookup_host(host, port, dns_fell_back),
    )
    .await
    {
        Ok(Ok(resolution)) => resolution,
        Ok(Err(e)) => return Err(Error::from(e)),
        Err(_elapsed) => {
            warn!(
                "DNS resolution for {}:{} timed out after {}ms",
                host, port, timeout_ms
            );
            return Err(Error::TimeoutError(crate::error::TimeoutErrorType::String(
                format!("Connection timeout: DNS resolution for {host}:{port} timed out"),
            )));
        }
    };
    resolved();
    validate_address_count(resolution.addresses().len())?;
    let addresses = resolution.addresses();

    let total_addresses = addresses.len();
    info!(
        "Resolved {} addresses for {}:{}: {:?}",
        total_addresses, host, port, addresses
    );

    // Create connection futures for all addresses
    let connect_futures: Vec<_> = addresses
        .iter()
        .enumerate()
        .map(|(idx, addr)| {
            let addr = *addr;
            let attempt = connect(addr);
            async move {
                trace!("Attempting connection {} to {}", idx, addr);
                let result: Result<S, AttemptError> = attempt.await.map_err(Into::into);
                match result {
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
            Ok((
                ParallelConnectResult {
                    stream,
                    connected_address: addr,
                    total_addresses,
                    failed_attempts,
                },
                resolution,
            ))
        }
        Ok(Err(RaceLost::Rejected(error, address))) => {
            rejected(address);
            Err(error)
        }
        Ok(Err(RaceLost::Unreachable(last_error))) => {
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
            warn!("Parallel connection timeout after {}ms", timeout_ms);
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
    let (keep_alive_in_ms, keep_alive_interval_in_ms) =
        (config.keep_alive_in_ms, config.keep_alive_interval_in_ms);
    parallel_connect_with(addresses, config.timeout_ms, move |addr| {
        connect_with_keepalive(addr, keep_alive_in_ms, keep_alive_interval_in_ms)
    })
    .await
}

/// [`parallel_connect_to_addresses`] with the per-address connect supplied by
/// the caller, so tests can stand in for an address that never answers.
pub(crate) async fn parallel_connect_with<S, C, F, E>(
    addresses: Vec<SocketAddr>,
    timeout_ms: u64,
    connect: C,
) -> TdsResult<ParallelConnectResult<S>>
where
    S: Send + 'static,
    C: Fn(SocketAddr) -> F,
    F: Future<Output = Result<S, E>> + Send + 'static,
    E: Into<AttemptError>,
{
    validate_address_count(addresses.len())?;

    let total_addresses = addresses.len();
    info!(
        "Starting parallel connection to {} addresses: {:?}",
        total_addresses, addresses
    );

    // Create connection futures for all addresses
    let connect_futures: Vec<_> = addresses
        .iter()
        .enumerate()
        .map(|(idx, addr)| {
            let addr = *addr;
            let attempt = connect(addr);
            async move {
                trace!("Attempting connection {} to {}", idx, addr);
                let result: Result<S, AttemptError> = attempt.await.map_err(Into::into);
                match result {
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
        Duration::from_millis(timeout_ms),
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
        Ok(Err(RaceLost::Rejected(error, _))) => Err(error),
        Ok(Err(RaceLost::Unreachable(last_error))) => {
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
            warn!("Parallel connection timeout after {}ms", timeout_ms);
            Err(Error::TimeoutError(crate::error::TimeoutErrorType::String(
                "Connection timeout: Connection attempt timed out".to_string(),
            )))
        }
    }
}

fn validate_address_count(count: usize) -> TdsResult<()> {
    if count == 0 {
        warn!("No addresses provided for parallel connection");
        return Err(Error::ConnectionError(
            "No addresses provided for parallel connection".to_string(),
        ));
    }
    if count > MAX_PARALLEL_IPS {
        warn!(
            count,
            maximum = MAX_PARALLEL_IPS,
            "Too many addresses for MultiSubnetFailover"
        );
        return Err(Error::UsageError(format!(
            "MultiSubnetFailover supports at most {MAX_PARALLEL_IPS} addresses; the host resolved to {count}. Disable MultiSubnetFailover to connect sequentially."
        )));
    }
    Ok(())
}

/// Connects to a single address with TCP keep-alive settings.
pub(crate) async fn connect_with_keepalive(
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

/// How a race of connection attempts ended without a connection.
enum RaceLost {
    /// The address rejected the connection, which ends the race.
    Rejected(Error, SocketAddr),
    /// No address could be reached; the last error.
    Unreachable(std::io::Error),
}

/// Races multiple connection futures and returns the first successful one.
///
/// Each attempt runs as a task in a `JoinSet`, which aborts every attempt still
/// pending when the race returns or is dropped. A win, a rejection, or the login
/// deadline cancelling the race therefore closes the other connections instead
/// of leaving them to the OS connect timeout. If all connections fail, returns
/// the last error.
async fn race_connections<S: Send + 'static>(
    connect_futures: Vec<
        impl std::future::Future<
            Output = Result<(S, SocketAddr, usize), (AttemptError, SocketAddr, usize)>,
        > + Send
        + 'static,
    >,
) -> Result<(S, SocketAddr, usize, usize), RaceLost> {
    let mut attempts = tokio::task::JoinSet::new();
    for fut in connect_futures {
        attempts.spawn(crate::trace_context::propagate_future(fut));
    }

    let mut failed_attempts = 0;
    let mut last_error = std::io::Error::new(
        std::io::ErrorKind::NotConnected,
        "No connection attempts made",
    );

    while let Some(attempt) = attempts.join_next().await {
        match attempt {
            Ok(Ok((stream, addr, idx))) => return Ok((stream, addr, idx, failed_attempts)),
            Ok(Err((AttemptError::Unreachable(e), _addr, _idx))) => {
                failed_attempts += 1;
                last_error = e;
            }
            Ok(Err((AttemptError::Rejected(error), addr, _idx))) => {
                return Err(RaceLost::Rejected(error, addr));
            }
            Err(join_error) => {
                failed_attempts += 1;
                last_error = std::io::Error::other(join_error);
            }
        }
    }

    Err(RaceLost::Unreachable(last_error))
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::FutureExt;
    use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[tokio::test]
    async fn parallel_address_limit_is_checked_before_any_attempt() {
        for count in [0, 1, 2, 64, 65] {
            let addresses: Vec<_> = (0..count)
                .map(|i| SocketAddr::from(([192, 0, 2, 1], 1000 + u16::try_from(i).unwrap())))
                .collect();
            let attempted = std::sync::Arc::new(AtomicUsize::new(0));
            let recorded = attempted.clone();
            let result = parallel_connect_with(addresses, 1_000, move |_| {
                recorded.fetch_add(1, Ordering::SeqCst);
                async {
                    Err::<TcpStream, std::io::Error>(std::io::ErrorKind::ConnectionRefused.into())
                }
            })
            .await;
            assert!(result.is_err());
            assert_eq!(
                attempted.load(Ordering::SeqCst),
                if (1..=64).contains(&count) { count } else { 0 }
            );
            if count == 65 {
                let error = result.unwrap_err();
                assert!(matches!(error, Error::UsageError(_)));
                assert!(!error.is_transient_connect_error());
            }
        }
    }

    #[tokio::test]
    async fn parallel_connect_does_not_wait_for_a_silent_first_address() {
        let listener = net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let live = listener.local_addr().unwrap();
        let silent = SocketAddr::from(([192, 0, 2, 1], 1433));
        let result = timeout(
            Duration::from_secs(1),
            parallel_connect_with(vec![silent, live], 5_000, move |address| async move {
                if address == silent {
                    std::future::pending().await
                } else {
                    TcpStream::connect(address).await
                }
            }),
        )
        .await
        .expect("a silent first address must not delay the live one")
        .unwrap();
        assert_eq!(result.connected_address, live);
        assert_eq!(result.total_addresses, 2);
    }

    #[test]
    fn parallel_attempt_tasks_inherit_context_without_leaking_it() {
        use crate::trace_context::{self, ConnectionTrace, Context, Snapshot, snapshot};

        trace_context::enable();
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .unwrap();
        let connection = Arc::new(ConnectionTrace::default());
        let _scope = Context::connection(connection).pending_connection().enter();
        let expected = snapshot();
        assert!(expected.dbc.is_some());
        assert_eq!(expected.cid, None, "TCP attempts precede PRELOGIN");
        let attempts = (0..2)
            .map(|index| async move {
                assert_eq!(snapshot(), expected);
                tokio::task::yield_now().await;
                assert_eq!(snapshot(), expected);
                Err((
                    AttemptError::from(std::io::Error::from(std::io::ErrorKind::ConnectionRefused)),
                    SocketAddr::from(([127, 0, 0, 1], 0)),
                    index,
                ))
            })
            .collect();
        let result = runtime.block_on(race_connections::<TcpStream>(attempts));
        assert!(matches!(
            result,
            Err(RaceLost::Unreachable(error))
                if error.kind() == std::io::ErrorKind::ConnectionRefused
        ));
        assert_eq!(snapshot(), expected);
        let worker_context = runtime
            .block_on(runtime.spawn(async { snapshot() }))
            .unwrap();
        assert_eq!(worker_context, Snapshot::default());
    }

    /// The login deadline cancels a race by dropping it, so every attempt still
    /// connecting must go with it rather than keep its socket open.
    #[tokio::test]
    async fn a_dropped_race_aborts_every_pending_attempt() {
        let pending = Arc::new(AtomicUsize::new(3));
        let attempts = (0..3)
            .map(|_| {
                let guard = Pending(Arc::clone(&pending));
                async move {
                    let _guard = guard;
                    std::future::pending::<
                        Result<(TcpStream, SocketAddr, usize), (AttemptError, SocketAddr, usize)>,
                    >()
                    .await
                }
            })
            .collect();

        let raced = timeout(Duration::from_millis(50), race_connections(attempts)).await;
        assert!(raced.is_err(), "no attempt ever finishes");
        all_dropped(&pending).await;
    }

    /// A server that was reached but rejected the connection ends the race at
    /// once, rather than waiting on addresses that may never answer.
    #[tokio::test]
    async fn a_rejected_attempt_ends_the_race() {
        let pending = Arc::new(AtomicUsize::new(1));
        let silent = {
            let guard = Pending(Arc::clone(&pending));
            async move {
                let _guard = guard;
                std::future::pending::<
                    Result<(TcpStream, SocketAddr, usize), (AttemptError, SocketAddr, usize)>,
                >()
                .await
            }
            .boxed()
        };
        let rejected = async {
            Err((
                AttemptError::Rejected(Error::CertificateMismatch),
                SocketAddr::from(([127, 0, 0, 1], 1)),
                1,
            ))
        }
        .boxed();

        let raced = timeout(
            Duration::from_secs(5),
            race_connections(vec![silent, rejected]),
        )
        .await
        .expect("a rejection does not wait for the silent attempt");
        assert!(matches!(
            raced,
            Err(RaceLost::Rejected(Error::CertificateMismatch, address))
                if address == SocketAddr::from(([127, 0, 0, 1], 1))
        ));
        all_dropped(&pending).await;
    }

    #[tokio::test]
    async fn parallel_connect_reports_a_rejection_as_it_is() {
        let mut rejected_at = None;
        let result = parallel_connect_resolving_with(
            "127.0.0.1",
            1,
            5_000,
            |_| {},
            || {},
            |address| rejected_at = Some(address),
            |_| async { Err::<TcpStream, _>(AttemptError::Rejected(Error::CertificateMismatch)) },
        )
        .await;

        assert!(matches!(result, Err(Error::CertificateMismatch)));
        assert_eq!(rejected_at, Some(SocketAddr::from(([127, 0, 0, 1], 1))));
    }

    struct Pending(Arc<AtomicUsize>);

    impl Drop for Pending {
        fn drop(&mut self) {
            self.0.fetch_sub(1, Ordering::SeqCst);
        }
    }

    async fn all_dropped(pending: &AtomicUsize) {
        timeout(Duration::from_secs(5), async {
            while pending.load(Ordering::SeqCst) > 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("ending the race aborts every attempt");
    }

    /// Regression coverage for AB#47704: `parallel_connect` used to resolve
    /// DNS via blocking `to_socket_addrs()`, which never yields to the
    /// executor. Timing can't prove the fix — resolving even a nonexistent
    /// host completes in ms either way — so this asserts the structural
    /// property instead: a concurrently spawned heartbeat must get scheduled
    /// while resolution is in flight. `lookup_host` bridges to
    /// `spawn_blocking` via a channel, so the awaiting task is guaranteed to
    /// yield at least once; a blocking call never yields, so the heartbeat
    /// gets zero chances to run. Uses a host that fails to resolve so the
    /// call returns from resolution alone, without reaching
    /// `race_connections` — which spawns tasks and awaits their results
    /// regardless of DNS behavior, and would mask the signal. Confirmed by
    /// mutation testing (reverting to `to_socket_addrs()` makes this fail).
    ///
    /// Must stay on the default `current_thread` runtime: a `multi_thread`
    /// flavor would let the heartbeat run on another worker even if
    /// resolution blocked, so the assertion would pass without proving
    /// anything.
    #[tokio::test(flavor = "current_thread")]
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
