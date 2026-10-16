// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Client-side DNS cache negotiated by LOGIN7 feature extension `0x0B`
//! (AZURESQLDNSCACHING, [MS-TDS] 2.2.6.4 and 2.2.7.11).
//!
//! Follows msodbcsql's SNI cache (`dns_cache.cpp`, `GetCachedAddrInfo`): every
//! connect resolves the host normally, and cached addresses are used only when
//! that resolution fails, so connections keep working through a DNS outage.
//!
//! The server decides whether its host may be cached. Each FEATUREEXTACK
//! settles it, as `SNI_QUERY_CONN_DNSCACHE` does in SNI: an acknowledgement
//! with bit 0 set publishes the host's latest resolution, and anything else
//! removes the host. Unlike SNI, addresses are never used before a server has
//! allowed them, and a resolution waiting for that verdict is held in a
//! bounded set, so hosts whose logins never get that far cannot grow the cache.

use std::collections::HashMap;
use std::future::Future;
use std::io;
use std::net::{IpAddr, SocketAddr};
use std::sync::{Mutex, MutexGuard, OnceLock, PoisonError};

use tracing::{debug, warn};

/// Most hosts whose latest resolution can wait for a verdict at once.
const MAX_PENDING_HOSTS: usize = 256;

/// Keys are lowercased host names, because DNS names are case-insensitive.
/// Addresses keep their IPv6 scope; serving one only replaces the port.
#[derive(Debug, Default)]
struct Entries {
    /// Hosts a server allowed caching for: the fallback addresses.
    allowed: HashMap<String, Vec<SocketAddr>>,
    /// Each host's latest resolution, or the cached addresses a login fell
    /// back to, for an allowing verdict to publish. Verdicts leave it in
    /// place, so the last verdict decides however logins interleave.
    pending: HashMap<String, Vec<SocketAddr>>,
}

impl Entries {
    fn hold(&mut self, key: String, addresses: Vec<SocketAddr>) {
        if self.pending.len() >= MAX_PENDING_HOSTS && !self.pending.contains_key(&key) {
            // Any victim will do: losing one only defers caching its host to
            // that host's next login.
            if let Some(victim) = self.pending.keys().next().cloned() {
                self.pending.remove(&victim);
            }
        }
        self.pending.insert(key, addresses);
    }
}

/// Process-lifetime host name to address cache.
#[derive(Debug, Default)]
pub(crate) struct DnsCache {
    entries: Mutex<Entries>,
}

static GLOBAL_CACHE: OnceLock<DnsCache> = OnceLock::new();

impl DnsCache {
    /// Returns the global process-lifetime cache.
    pub(crate) fn global() -> &'static DnsCache {
        GLOBAL_CACHE.get_or_init(DnsCache::default)
    }

    fn entries(&self) -> MutexGuard<'_, Entries> {
        // Every critical section leaves the maps consistent, so a poisoned lock
        // is safe to keep using; the cache is only a fallback.
        self.entries.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Resolves `host` with `lookup`. A successful resolution becomes the
    /// host's pending entry; a failed one falls back to the addresses a server
    /// allowed.
    pub(crate) async fn resolve<F, Fut>(
        &self,
        host: &str,
        port: u16,
        lookup: F,
    ) -> io::Result<Vec<SocketAddr>>
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = io::Result<Vec<SocketAddr>>>,
    {
        match lookup().await {
            Ok(addresses) => {
                self.record(host, &addresses);
                Ok(addresses)
            }
            Err(error) => match self.cached(host, port) {
                Some(addresses) => {
                    warn!(
                        host,
                        %error,
                        ?addresses,
                        "DNS resolution failed; connecting with cached addresses"
                    );
                    Ok(addresses)
                }
                None => Err(error),
            },
        }
    }

    fn record(&self, host: &str, addresses: &[SocketAddr]) {
        // An IP literal never needs a resolver to fall back from.
        if addresses.is_empty() || host.parse::<IpAddr>().is_ok() {
            return;
        }
        self.entries()
            .hold(host.to_ascii_lowercase(), addresses.to_vec());
    }

    fn cached(&self, host: &str, port: u16) -> Option<Vec<SocketAddr>> {
        let key = host.to_ascii_lowercase();
        let mut entries = self.entries();
        let addresses = entries.allowed.get(&key)?.clone();
        // This login's verdict must still be able to publish what it connected
        // with if another login's verdict removes the entry first.
        if !entries.pending.contains_key(&key) {
            entries.hold(key, addresses.clone());
        }
        Some(
            addresses
                .into_iter()
                .map(|mut address| {
                    address.set_port(port);
                    address
                })
                .collect(),
        )
    }

    /// Applies a server's DNS caching acknowledgement for `host`.
    pub(crate) fn apply_verdict(&self, host: &str, allowed: bool) {
        let key = host.to_ascii_lowercase();
        let mut entries = self.entries();
        if allowed {
            if let Some(addresses) = entries.pending.get(&key).cloned() {
                entries.allowed.insert(key, addresses);
            }
        } else {
            entries.allowed.remove(&key);
        }
        debug!(host, allowed, "Applied DNS caching acknowledgement");
    }
}

/// Resolves `host` for a TCP connect through the process-wide DNS cache.
pub(crate) async fn lookup_host(host: &str, port: u16) -> io::Result<Vec<SocketAddr>> {
    DnsCache::global()
        .resolve(host, port, || system_lookup(host, port))
        .await
}

async fn system_lookup(host: &str, port: u16) -> io::Result<Vec<SocketAddr>> {
    // Names under `.invalid` never resolve (RFC 6761). Unit tests use them to
    // reach the fallback through a real connect without depending on the
    // machine's resolver.
    #[cfg(test)]
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
    ) -> io::Result<Vec<SocketAddr>> {
        cache
            .resolve(host, port, || async move { Ok(addresses) })
            .await
    }

    async fn fails_to_resolve(
        cache: &DnsCache,
        host: &str,
        port: u16,
    ) -> io::Result<Vec<SocketAddr>> {
        cache
            .resolve(host, port, || async {
                Err(io::Error::new(io::ErrorKind::NotFound, "no such host"))
            })
            .await
    }

    #[tokio::test]
    async fn successful_resolution_is_returned_as_is() {
        let cache = DnsCache::default();
        let addresses = vec![address("10.0.0.1", 1433), address("10.0.0.2", 1433)];
        let resolved = resolves_to(&cache, "db.example.com", 1433, addresses.clone())
            .await
            .unwrap();
        assert_eq!(resolved, addresses);
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
        .await
        .unwrap();
        let error = fails_to_resolve(&cache, "db.example.com", 1433)
            .await
            .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::NotFound);
    }

    #[tokio::test]
    async fn allowed_entry_is_used_when_resolution_fails() {
        let cache = DnsCache::default();
        resolves_to(
            &cache,
            "db.example.com",
            1433,
            vec![address("10.0.0.1", 1433)],
        )
        .await
        .unwrap();
        cache.apply_verdict("db.example.com", true);

        let resolved = fails_to_resolve(&cache, "DB.Example.com", 11000)
            .await
            .unwrap();
        assert_eq!(resolved, [address("10.0.0.1", 11000)]);
    }

    #[tokio::test]
    async fn a_new_resolution_replaces_the_allowed_one_at_the_next_verdict() {
        let cache = DnsCache::default();
        resolves_to(
            &cache,
            "db.example.com",
            1433,
            vec![address("10.0.0.1", 1433)],
        )
        .await
        .unwrap();
        cache.apply_verdict("db.example.com", true);
        resolves_to(
            &cache,
            "db.example.com",
            1433,
            vec![address("10.0.0.9", 1433)],
        )
        .await
        .unwrap();

        let resolved = fails_to_resolve(&cache, "db.example.com", 1433)
            .await
            .unwrap();
        assert_eq!(
            resolved,
            [address("10.0.0.1", 1433)],
            "a resolution no server has allowed yet is not a fallback"
        );

        cache.apply_verdict("db.example.com", true);
        let resolved = fails_to_resolve(&cache, "db.example.com", 1433)
            .await
            .unwrap();
        assert_eq!(resolved, [address("10.0.0.9", 1433)]);
    }

    #[tokio::test]
    async fn disallowing_removes_the_entry() {
        let cache = DnsCache::default();
        resolves_to(
            &cache,
            "db.example.com",
            1433,
            vec![address("10.0.0.1", 1433)],
        )
        .await
        .unwrap();
        cache.apply_verdict("db.example.com", true);
        cache.apply_verdict("db.example.com", false);

        assert!(
            fails_to_resolve(&cache, "db.example.com", 1433)
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn the_last_of_interleaved_verdicts_decides() {
        let cache = DnsCache::default();
        // Three logins resolve the host before their verdicts arrive.
        for _ in 0..3 {
            resolves_to(
                &cache,
                "db.example.com",
                1433,
                vec![address("10.0.0.1", 1433)],
            )
            .await
            .unwrap();
        }
        cache.apply_verdict("db.example.com", true);
        cache.apply_verdict("db.example.com", false);
        cache.apply_verdict("db.example.com", true);

        let resolved = fails_to_resolve(&cache, "db.example.com", 1433)
            .await
            .unwrap();
        assert_eq!(resolved, [address("10.0.0.1", 1433)]);
    }

    #[tokio::test]
    async fn logins_through_the_fallback_can_restore_it_after_a_denial() {
        let cache = DnsCache::default();
        resolves_to(
            &cache,
            "db.example.com",
            1433,
            vec![address("10.0.0.1", 1433)],
        )
        .await
        .unwrap();
        cache.apply_verdict("db.example.com", true);
        // As if evicted, so only the fallback can tell the coming verdicts what
        // their logins connected with.
        cache.entries().pending.clear();

        // During a DNS outage two logins connect through the cache, and their
        // verdicts arrive deny, then allow.
        for _ in 0..2 {
            fails_to_resolve(&cache, "db.example.com", 1433)
                .await
                .unwrap();
        }
        cache.apply_verdict("db.example.com", false);
        cache.apply_verdict("db.example.com", true);

        let resolved = fails_to_resolve(&cache, "db.example.com", 1433)
            .await
            .unwrap();
        assert_eq!(resolved, [address("10.0.0.1", 1433)]);
    }

    #[tokio::test]
    async fn ip_literals_and_empty_results_are_not_cached() {
        let cache = DnsCache::default();
        resolves_to(&cache, "10.0.0.1", 1433, vec![address("10.0.0.1", 1433)])
            .await
            .unwrap();
        resolves_to(&cache, "empty.example.com", 1433, Vec::new())
            .await
            .unwrap();
        cache.apply_verdict("10.0.0.1", true);
        cache.apply_verdict("empty.example.com", true);

        let entries = cache.entries();
        assert!(entries.allowed.is_empty());
        assert!(entries.pending.is_empty());
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
        resolves_to(&cache, "db.example.com", 1433, vec![scoped(1433)])
            .await
            .unwrap();
        cache.apply_verdict("db.example.com", true);

        let resolved = fails_to_resolve(&cache, "db.example.com", 11000)
            .await
            .unwrap();
        assert_eq!(resolved, [scoped(11000)]);
    }

    #[tokio::test]
    async fn a_disallowing_verdict_does_not_cancel_a_concurrent_allowing_one() {
        let cache = DnsCache::default();
        // Two logins resolve the host before either gets its verdict.
        for _ in 0..2 {
            resolves_to(
                &cache,
                "db.example.com",
                1433,
                vec![address("10.0.0.1", 1433)],
            )
            .await
            .unwrap();
        }
        cache.apply_verdict("db.example.com", false);
        cache.apply_verdict("db.example.com", true);

        let resolved = fails_to_resolve(&cache, "db.example.com", 1433)
            .await
            .unwrap();
        assert_eq!(resolved, [address("10.0.0.1", 1433)]);
    }

    #[tokio::test]
    async fn resolutions_waiting_for_a_verdict_are_bounded() {
        let cache = DnsCache::default();
        let host = |i: usize| format!("host{i}.example.com");
        for i in 0..MAX_PENDING_HOSTS + 10 {
            resolves_to(&cache, &host(i), 1433, vec![address("10.0.0.1", 1433)])
                .await
                .unwrap();
        }

        let entries = cache.entries();
        assert_eq!(entries.pending.len(), MAX_PENDING_HOSTS);
        assert!(entries.pending.contains_key(&host(MAX_PENDING_HOSTS + 9)));
        assert!(entries.allowed.is_empty());
    }
}
