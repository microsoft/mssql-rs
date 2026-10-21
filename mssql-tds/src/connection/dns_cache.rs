// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Client-side DNS cache negotiated by LOGIN7 feature extension `0x0B`
//! (AZURESQLDNSCACHING, [MS-TDS] 2.2.6.4 and 2.2.7.11).
//!
//! Follows msodbcsql's SNI cache (`dns_cache.cpp`, `GetCachedAddrInfo`): every
//! connect resolves the host normally, and cached addresses are used only when
//! that resolution fails, so connections keep working through a DNS outage.
//!
//! The last FEATUREEXTACK in a completed, accepted login decides whether its
//! host may be cached: bit 0 set publishes that connection's addresses; a
//! missing or negative DNS acknowledgement removes the host. No FEATUREEXTACK
//! means no new verdict, including on a gateway redirect. Rejected, incomplete,
//! and authentication-continuation responses leave the fallback unchanged.
//! Unlike SNI, resolutions are never shared before an accepted login allows
//! them, and a malformed FEATUREEXTACK cannot retain a partial DNS allow.
//!
//! Each connection carries its own [`Resolution`] to its login, as SqlClient
//! does with `pendingSQLDNSObject`, so the cache only ever holds allowed hosts
//! and concurrent logins cannot lose each other's addresses.

use std::collections::HashMap;
use std::future::Future;
use std::io;
use std::net::{IpAddr, SocketAddr};
use std::sync::{Mutex, MutexGuard, OnceLock, PoisonError};

use tracing::{debug, warn};

/// The addresses one connection resolved its host to, or fell back to. Only
/// that connection's login verdict can publish them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Resolution {
    /// Lowercased, because DNS names are case-insensitive.
    host: String,
    addresses: Vec<SocketAddr>,
}

impl Resolution {
    pub(crate) fn new(host: &str, addresses: Vec<SocketAddr>) -> Self {
        Self {
            host: host.to_ascii_lowercase(),
            addresses,
        }
    }

    pub(crate) fn addresses(&self) -> &[SocketAddr] {
        &self.addresses
    }
}

/// Process-lifetime map from each host a server allowed caching for to the
/// addresses to fall back to. Addresses keep their IPv6 scope; serving one only
/// replaces the port.
#[derive(Debug, Default)]
pub(crate) struct DnsCache {
    allowed: Mutex<HashMap<String, Vec<SocketAddr>>>,
}

static GLOBAL_CACHE: OnceLock<DnsCache> = OnceLock::new();

impl DnsCache {
    /// Returns the global process-lifetime cache.
    pub(crate) fn global() -> &'static DnsCache {
        GLOBAL_CACHE.get_or_init(DnsCache::default)
    }

    fn allowed(&self) -> MutexGuard<'_, HashMap<String, Vec<SocketAddr>>> {
        // Every critical section leaves the map consistent, so a poisoned lock
        // is safe to keep using; the cache is only a fallback.
        self.allowed.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Resolves `host` with `lookup`, falling back to the addresses a server
    /// allowed when that fails.
    pub(crate) async fn resolve<F, Fut>(
        &self,
        host: &str,
        port: u16,
        lookup: F,
    ) -> io::Result<Resolution>
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = io::Result<Vec<SocketAddr>>>,
    {
        let addresses = match lookup().await {
            Ok(addresses) => addresses,
            Err(error) => {
                let Some(addresses) = self.cached(host, port) else {
                    return Err(error);
                };
                warn!(
                    host,
                    %error,
                    ?addresses,
                    "DNS resolution failed; connecting with cached addresses"
                );
                addresses
            }
        };
        Ok(Resolution::new(host, addresses))
    }

    fn cached(&self, host: &str, port: u16) -> Option<Vec<SocketAddr>> {
        let allowed = self.allowed();
        let addresses = allowed.get(&host.to_ascii_lowercase())?;
        Some(
            addresses
                .iter()
                .copied()
                .map(|mut address| {
                    address.set_port(port);
                    address
                })
                .collect(),
        )
    }

    /// Applies a server's DNS caching verdict to the host `resolution` came
    /// from: allowing publishes its addresses, anything else removes the host.
    pub(crate) fn apply_verdict(&self, resolution: &Resolution, allowed: bool) {
        let Resolution { host, addresses } = resolution;
        let mut entries = self.allowed();
        if !allowed {
            entries.remove(host);
        } else if !addresses.is_empty() && host.parse::<IpAddr>().is_err() {
            // An IP literal never needs a resolver to fall back from.
            entries.insert(host.clone(), addresses.clone());
        }
        debug!(
            host = host.as_str(),
            allowed, "Applied DNS caching acknowledgement"
        );
    }
}

/// Resolves `host` for a TCP connect through the process-wide DNS cache.
pub(crate) async fn lookup_host(host: &str, port: u16) -> io::Result<Resolution> {
    DnsCache::global()
        .resolve(host, port, || system_lookup(host, port))
        .await
}

async fn system_lookup(host: &str, port: u16) -> io::Result<Vec<SocketAddr>> {
    // Names under `.invalid` never resolve (RFC 6761). Tests use them to
    // reach the fallback through a real connect without depending on the
    // machine's resolver.
    #[cfg(any(test, feature = "test-util"))]
    if host.ends_with(".invalid") {
        return Err(io::Error::new(io::ErrorKind::NotFound, "no such host"));
    }
    Ok(tokio::net::lookup_host((host, port)).await?.collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn address(ip: &str, port: u16) -> SocketAddr {
        SocketAddr::new(ip.parse().unwrap(), port)
    }

    async fn resolves_to(
        cache: &DnsCache,
        host: &str,
        port: u16,
        addresses: Vec<SocketAddr>,
    ) -> Resolution {
        cache
            .resolve(host, port, || async move { Ok(addresses) })
            .await
            .unwrap()
    }

    async fn fails_to_resolve(cache: &DnsCache, host: &str, port: u16) -> io::Result<Resolution> {
        cache
            .resolve(host, port, || async {
                Err(io::Error::new(io::ErrorKind::NotFound, "no such host"))
            })
            .await
    }

    /// The addresses a connect to `host` falls back to when resolution fails.
    async fn fallback(cache: &DnsCache, host: &str, port: u16) -> Option<Vec<SocketAddr>> {
        fails_to_resolve(cache, host, port)
            .await
            .ok()
            .map(|resolution| resolution.addresses)
    }

    #[tokio::test]
    async fn successful_resolution_is_returned_as_is() {
        let cache = DnsCache::default();
        let addresses = vec![address("10.0.0.1", 1433), address("10.0.0.2", 1433)];
        let resolution = resolves_to(&cache, "DB.Example.com", 1433, addresses.clone()).await;
        assert_eq!(resolution, Resolution::new("db.example.com", addresses));
    }

    #[tokio::test]
    async fn resolution_is_not_a_fallback_until_a_server_allows_it() {
        let cache = DnsCache::default();
        resolves_to(
            &cache,
            "db.example.com",
            1433,
            vec![address("10.0.0.1", 1433)],
        )
        .await;
        let error = fails_to_resolve(&cache, "db.example.com", 1433)
            .await
            .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::NotFound);
        assert!(
            cache.allowed().is_empty(),
            "nothing is held while a verdict is pending"
        );
    }

    #[tokio::test]
    async fn allowed_resolution_is_used_when_resolution_fails() {
        let cache = DnsCache::default();
        let resolution = resolves_to(
            &cache,
            "db.example.com",
            1433,
            vec![address("10.0.0.1", 1433)],
        )
        .await;
        cache.apply_verdict(&resolution, true);

        let resolved = fails_to_resolve(&cache, "DB.Example.com", 11000)
            .await
            .unwrap();
        assert_eq!(
            resolved,
            Resolution::new("db.example.com", vec![address("10.0.0.1", 11000)])
        );
    }

    #[tokio::test]
    async fn a_new_resolution_replaces_the_allowed_one_only_once_allowed() {
        let cache = DnsCache::default();
        let first = resolves_to(
            &cache,
            "db.example.com",
            1433,
            vec![address("10.0.0.1", 1433)],
        )
        .await;
        cache.apply_verdict(&first, true);
        let second = resolves_to(
            &cache,
            "db.example.com",
            1433,
            vec![address("10.0.0.9", 1433)],
        )
        .await;
        assert_eq!(
            fallback(&cache, "db.example.com", 1433).await,
            Some(vec![address("10.0.0.1", 1433)]),
            "a resolution no server has allowed yet is not a fallback"
        );

        cache.apply_verdict(&second, true);
        assert_eq!(
            fallback(&cache, "db.example.com", 1433).await,
            Some(vec![address("10.0.0.9", 1433)])
        );
    }

    #[tokio::test]
    async fn disallowing_removes_the_entry() {
        let cache = DnsCache::default();
        let resolution = resolves_to(
            &cache,
            "db.example.com",
            1433,
            vec![address("10.0.0.1", 1433)],
        )
        .await;
        cache.apply_verdict(&resolution, true);
        cache.apply_verdict(&resolution, false);

        assert_eq!(fallback(&cache, "db.example.com", 1433).await, None);
    }

    #[tokio::test]
    async fn the_last_verdict_publishes_what_its_own_login_resolved() {
        let cache = DnsCache::default();
        // Three logins resolve the host differently, and their verdicts arrive
        // in another order.
        let mut logins = Vec::new();
        for ip in ["10.0.0.1", "10.0.0.2", "10.0.0.3"] {
            logins.push(resolves_to(&cache, "db.example.com", 1433, vec![address(ip, 1433)]).await);
        }
        cache.apply_verdict(&logins[2], true);
        cache.apply_verdict(&logins[0], false);
        cache.apply_verdict(&logins[1], true);

        assert_eq!(
            fallback(&cache, "db.example.com", 1433).await,
            Some(vec![address("10.0.0.2", 1433)])
        );
    }

    #[tokio::test]
    async fn a_disallowing_verdict_does_not_cancel_a_concurrent_allowing_one() {
        let cache = DnsCache::default();
        // Two logins resolve the host before either gets its verdict.
        let first = resolves_to(
            &cache,
            "db.example.com",
            1433,
            vec![address("10.0.0.1", 1433)],
        )
        .await;
        let second = resolves_to(
            &cache,
            "db.example.com",
            1433,
            vec![address("10.0.0.1", 1433)],
        )
        .await;
        cache.apply_verdict(&first, false);
        cache.apply_verdict(&second, true);

        assert_eq!(
            fallback(&cache, "db.example.com", 1433).await,
            Some(vec![address("10.0.0.1", 1433)])
        );
    }

    #[tokio::test]
    async fn logins_through_the_fallback_can_restore_it_after_a_denial() {
        let cache = DnsCache::default();
        let resolution = resolves_to(
            &cache,
            "db.example.com",
            1433,
            vec![address("10.0.0.1", 1433)],
        )
        .await;
        cache.apply_verdict(&resolution, true);

        // During a DNS outage two logins connect through the cache, and their
        // verdicts arrive deny, then allow.
        let first = fails_to_resolve(&cache, "db.example.com", 1433)
            .await
            .unwrap();
        let second = fails_to_resolve(&cache, "db.example.com", 1433)
            .await
            .unwrap();
        cache.apply_verdict(&first, false);
        cache.apply_verdict(&second, true);

        assert_eq!(
            fallback(&cache, "db.example.com", 1433).await,
            Some(vec![address("10.0.0.1", 1433)])
        );
    }

    #[tokio::test]
    async fn every_allowed_host_is_published() {
        let cache = DnsCache::default();
        let host = |i: usize| format!("host{i}.example.com");
        let mut logins = Vec::new();
        for i in 0..=256 {
            logins.push(resolves_to(&cache, &host(i), 1433, vec![address("10.0.0.1", 1433)]).await);
        }
        for login in &logins {
            cache.apply_verdict(login, true);
        }

        for i in 0..=256 {
            assert_eq!(
                fallback(&cache, &host(i), 1433).await,
                Some(vec![address("10.0.0.1", 1433)]),
                "{}",
                host(i)
            );
        }
    }

    #[tokio::test]
    async fn ip_literals_and_empty_results_are_not_cached() {
        let cache = DnsCache::default();
        let literal = resolves_to(&cache, "10.0.0.1", 1433, vec![address("10.0.0.1", 1433)]).await;
        let empty = resolves_to(&cache, "empty.example.com", 1433, Vec::new()).await;
        cache.apply_verdict(&literal, true);
        cache.apply_verdict(&empty, true);

        assert!(cache.allowed().is_empty());
    }

    #[tokio::test]
    async fn a_cached_ipv6_address_keeps_its_scope() {
        let scoped = |port| {
            SocketAddr::V6(std::net::SocketAddrV6::new(
                "fe80::1".parse().unwrap(),
                port,
                0,
                7,
            ))
        };
        let cache = DnsCache::default();
        let resolution = resolves_to(&cache, "db.example.com", 1433, vec![scoped(1433)]).await;
        cache.apply_verdict(&resolution, true);

        assert_eq!(
            fallback(&cache, "db.example.com", 11000).await,
            Some(vec![scoped(11000)])
        );
    }

    #[tokio::test]
    async fn a_resolver_timeout_error_uses_the_allowed_fallback() {
        let cache = DnsCache::default();
        let resolution = Resolution::new("db.example.com", vec![address("10.0.0.1", 1433)]);
        cache.apply_verdict(&resolution, true);
        let resolved = cache
            .resolve("db.example.com", 1433, || async {
                Err(io::Error::new(io::ErrorKind::TimedOut, "DNS timed out"))
            })
            .await
            .unwrap();
        assert_eq!(resolved, resolution);
    }

    #[tokio::test(start_paused = true)]
    async fn a_hanging_lookup_remains_bounded_by_the_callers_deadline() {
        use std::time::Duration;
        use tokio::time::{Instant, timeout};

        let cache = DnsCache::default();
        let resolution = Resolution::new("db.example.com", vec![address("10.0.0.1", 1433)]);
        cache.apply_verdict(&resolution, true);
        let budget = Duration::from_secs(1);
        let started = Instant::now();
        let result = timeout(
            budget,
            cache.resolve("db.example.com", 1433, std::future::pending),
        )
        .await;
        assert!(result.is_err());
        assert_eq!(started.elapsed(), budget);
        assert_eq!(
            fallback(&cache, "db.example.com", 1433).await,
            Some(resolution.addresses)
        );
    }
}
