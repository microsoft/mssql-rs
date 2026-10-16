// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Client-side DNS cache negotiated by LOGIN7 feature extension `0x0B`
//! (AZURESQLDNSCACHING, [MS-TDS] 2.2.6.4 and 2.2.7.11).
//!
//! Behaves like msodbcsql's SNI cache (`dns_cache.cpp`, `GetCachedAddrInfo`):
//! every connect resolves the host normally and a successful resolution
//! replaces the host's addresses. The cached addresses are used only when a
//! later resolution fails, so connections keep working through a DNS outage.
//!
//! The server decides whether its host may be cached. Each FEATUREEXTACK
//! settles it, as `SNI_QUERY_CONN_DNSCACHE` does in SNI: an acknowledgement
//! with bit 0 set makes the entry usable, and anything else removes it. Unlike
//! SNI, an entry is never used before a server has allowed it.

use std::collections::HashMap;
use std::future::Future;
use std::io;
use std::net::{IpAddr, SocketAddr};
use std::sync::{Mutex, MutexGuard, OnceLock, PoisonError};

use tracing::{debug, warn};

#[derive(Debug, Default)]
struct Entry {
    addresses: Vec<IpAddr>,
    /// Whether a server reached through this host acknowledged DNS caching.
    allowed: bool,
}

/// Process-lifetime host name to address cache. Keys are lowercased because
/// DNS names are case-insensitive.
#[derive(Debug, Default)]
pub(crate) struct DnsCache {
    entries: Mutex<HashMap<String, Entry>>,
}

static GLOBAL_CACHE: OnceLock<DnsCache> = OnceLock::new();

impl DnsCache {
    /// Returns the global process-lifetime cache.
    pub(crate) fn global() -> &'static DnsCache {
        GLOBAL_CACHE.get_or_init(DnsCache::default)
    }

    fn entries(&self) -> MutexGuard<'_, HashMap<String, Entry>> {
        // Every critical section leaves the map consistent, so a poisoned lock
        // is safe to keep using; the cache is only a fallback.
        self.entries.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Resolves `host` with `lookup`. A successful resolution refreshes the
    /// host's entry; a failed one falls back to the entry if a server allowed it.
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
        let mut entries = self.entries();
        let entry = entries.entry(host.to_ascii_lowercase()).or_default();
        entry.addresses.clear();
        entry.addresses.extend(addresses.iter().map(SocketAddr::ip));
    }

    fn cached(&self, host: &str, port: u16) -> Option<Vec<SocketAddr>> {
        let entries = self.entries();
        let entry = entries
            .get(&host.to_ascii_lowercase())
            .filter(|entry| entry.allowed)?;
        Some(
            entry
                .addresses
                .iter()
                .map(|ip| SocketAddr::new(*ip, port))
                .collect(),
        )
    }

    /// Applies a server's DNS caching acknowledgement for `host`.
    pub(crate) fn apply_verdict(&self, host: &str, allowed: bool) {
        let key = host.to_ascii_lowercase();
        let mut entries = self.entries();
        if allowed {
            if let Some(entry) = entries.get_mut(&key) {
                entry.allowed = true;
            }
        } else {
            entries.remove(&key);
        }
        debug!(host, allowed, "Applied DNS caching acknowledgement");
    }
}

/// Resolves `host` for a TCP connect through the process-wide DNS cache.
pub(crate) async fn lookup_host(host: &str, port: u16) -> io::Result<Vec<SocketAddr>> {
    DnsCache::global()
        .resolve(host, port, || async {
            Ok(tokio::net::lookup_host((host, port)).await?.collect())
        })
        .await
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
    async fn successful_resolution_refreshes_an_allowed_entry() {
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
        cache.apply_verdict("db.example.com", true);
        assert!(
            fails_to_resolve(&cache, "db.example.com", 1433)
                .await
                .is_err(),
            "allowing a removed host must not resurrect its addresses"
        );
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

        assert!(cache.entries().is_empty());
    }
}
